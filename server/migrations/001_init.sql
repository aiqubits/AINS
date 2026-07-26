-- 系统尚未上线，无需兼容旧数据，所有字段直接定义在 CREATE TABLE 中。
-- 如需增加新字段，直接在此文件修改即可，无需编写单独的 ALTER TABLE 迁移脚本。
-- Tenant bootstrap must precede users/channels, both of which have a strict
-- foreign key to the default tenant.
CREATE TABLE IF NOT EXISTS tenants (
    id VARCHAR(255) PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    status VARCHAR(32) NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'disabled')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
INSERT INTO tenants (id, name, status) VALUES ('default', 'Default Tenant', 'active')
ON CONFLICT (id) DO NOTHING;

-- Create users table if not exists
CREATE TABLE IF NOT EXISTS users (
    id BIGINT PRIMARY KEY,
    email VARCHAR(255) UNIQUE NOT NULL,
    password_hash TEXT NOT NULL,
    name VARCHAR(255) NOT NULL,
    role VARCHAR(50) NOT NULL DEFAULT 'user'
        CHECK (role IN ('user', 'admin', 'system')),
    token_version INTEGER NOT NULL DEFAULT 1,
    email_verified BOOLEAN NOT NULL DEFAULT FALSE,
    verification_code_hash VARCHAR(255),
    verification_code_expires_at TIMESTAMPTZ,
    verification_code_sent_at TIMESTAMPTZ,
    verification_failed_attempts INTEGER NOT NULL DEFAULT 0,
    password_reset_token_hash VARCHAR(255),
    password_reset_expires_at TIMESTAMPTZ,
    password_reset_sent_at TIMESTAMPTZ,
    password_reset_failed_attempts INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    balance BIGINT NOT NULL DEFAULT 0,
    wx_openid VARCHAR(255) UNIQUE,
    tenant_id VARCHAR(255) NOT NULL DEFAULT 'default' REFERENCES tenants(id) ON DELETE RESTRICT
);

-- Index on tenant_id for multi-tenant queries
CREATE INDEX IF NOT EXISTS idx_users_tenant_id ON users(tenant_id);

-- Create index on email for faster lookups
CREATE INDEX IF NOT EXISTS idx_users_email ON users(email);

-- Create index on created_at for pagination
CREATE INDEX IF NOT EXISTS idx_users_created_at ON users(created_at DESC);

-- Refresh tokens for silent session recovery.
-- The unique token_hash index prevents duplicate token values.
-- Application code enforces "at most one active refresh token per user"
-- by deleting old tokens before inserting new ones during login.
-- Tokens are rotated on each refresh call (old deleted, new created in
-- a transaction) to limit replay windows.
-- CASCADE delete ensures orphaned tokens are cleaned when a user is removed.
CREATE TABLE IF NOT EXISTS refresh_tokens (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash VARCHAR(255) NOT NULL UNIQUE,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_refresh_tokens_user_id ON refresh_tokens(user_id);
CREATE INDEX IF NOT EXISTS idx_refresh_tokens_token_hash ON refresh_tokens(token_hash);

CREATE TABLE IF NOT EXISTS ai_gateway_channels (
    id UUID PRIMARY KEY,
    tenant_id VARCHAR(255) NOT NULL REFERENCES tenants(id) ON DELETE RESTRICT,
    name VARCHAR(255) NOT NULL,
    protocol_type VARCHAR(32) NOT NULL CHECK (protocol_type IN ('openai', 'anthropic')),
    models JSONB NOT NULL DEFAULT '[]'::jsonb,
    capabilities JSONB NOT NULL DEFAULT '[]'::jsonb,
    api_key_encrypted TEXT NOT NULL,
    base_url TEXT NOT NULL,
    is_active BOOLEAN NOT NULL DEFAULT TRUE,
    weight INTEGER NOT NULL DEFAULT 1 CHECK (weight >= 1),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_channels_tenant_capability_active
    ON ai_gateway_channels (tenant_id, is_active);
CREATE INDEX IF NOT EXISTS idx_channels_capabilities_gin
    ON ai_gateway_channels USING GIN (capabilities);

-- Snowflake worker ID registry for automatic worker coordination.
-- Each server instance registers here on startup to get a unique worker_id (0-1023).
-- Stale entries (heartbeat older than 30s) are cleaned up during registration.
CREATE TABLE IF NOT EXISTS snowflake_worker (
    worker_id SMALLINT PRIMARY KEY CHECK (worker_id >= 0 AND worker_id < 1024),
    host TEXT NOT NULL DEFAULT '',
    pid INTEGER NOT NULL DEFAULT 0,
    heartbeat TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Token usage metering table for Phase 0 P1 (Task 0.13).
-- Records AI proxy token consumption with user_id + tenant_id + channel_id
-- three-dimensional accounting for billing and quota management.
CREATE TABLE IF NOT EXISTS token_usage (
    id BIGINT PRIMARY KEY,
    user_id BIGINT NOT NULL,
    tenant_id VARCHAR(255) NOT NULL,
    channel_id UUID NOT NULL,
    model VARCHAR(255) NOT NULL,
    prompt_tokens BIGINT NOT NULL DEFAULT 0,
    completion_tokens BIGINT NOT NULL DEFAULT 0,
    total_tokens BIGINT NOT NULL DEFAULT 0,
    request_type VARCHAR(32) NOT NULL DEFAULT 'chat',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Index for per-user usage queries (e.g. "how many tokens did user X use this month").
CREATE INDEX IF NOT EXISTS idx_token_usage_user
    ON token_usage(user_id, created_at DESC);

-- Index for per-tenant usage queries (e.g. billing aggregation).
CREATE INDEX IF NOT EXISTS idx_token_usage_tenant
    ON token_usage(tenant_id, created_at DESC);

-- Index for per-channel usage queries (e.g. channel cost analysis).
CREATE INDEX IF NOT EXISTS idx_token_usage_channel
    ON token_usage(channel_id, created_at DESC);

-- Index for per-model usage stats queries (e.g. model cost analysis).
-- The `get_usage_stats` function filters by model with GROUP BY model,
-- and `list_usage` filters by model with ORDER BY created_at DESC.
-- A composite index on (model, created_at DESC) covers both query patterns.
CREATE INDEX IF NOT EXISTS idx_token_usage_model
    ON token_usage(model, created_at DESC);

-- Subscription plans (tenant-scoped, mirrors ai_gateway_channels isolation).
-- price shares the balance scale: 1 display unit = 10^10 stored units.
-- total_calls is the number of /api/ai/response calls granted per instance,
-- validity_days determines expires_at at grant/purchase time.
CREATE TABLE IF NOT EXISTS plans (
    id BIGINT PRIMARY KEY,
    tenant_id VARCHAR(255) NOT NULL REFERENCES tenants(id) ON DELETE RESTRICT,
    name VARCHAR(255) NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    price BIGINT NOT NULL CHECK (price > 0),
    total_calls BIGINT NOT NULL CHECK (total_calls > 0),
    validity_days INTEGER NOT NULL CHECK (validity_days > 0),
    status VARCHAR(32) NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'disabled')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Covers tenant-scoped admin listing and the user-facing
-- "available plans in my tenant" query (tenant_id + status = 'active').
CREATE INDEX IF NOT EXISTS idx_plans_tenant_status
    ON plans(tenant_id, status);

-- Plan instances held by users (snapshot-style).
-- plan_id intentionally has no FK: deleting a plan template must not
-- invalidate instances users already purchased or were granted.
-- An instance is "active" while expires_at > NOW() AND remaining_calls > 0;
-- expiry is evaluated at query time, no background job required.
CREATE TABLE IF NOT EXISTS user_plans (
    id BIGINT PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    plan_id BIGINT,
    plan_name VARCHAR(255) NOT NULL,
    total_calls BIGINT NOT NULL CHECK (total_calls > 0),
    remaining_calls BIGINT NOT NULL CHECK (remaining_calls >= 0),
    expires_at TIMESTAMPTZ NOT NULL,
    source VARCHAR(32) NOT NULL
        CHECK (source IN ('purchase', 'admin_grant')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Covers per-user plan listing and the consume_call earliest-expiry lookup.
CREATE INDEX IF NOT EXISTS idx_user_plans_user
    ON user_plans(user_id, expires_at DESC);

-- Payment orders (audit trail of plan purchases and manual entries).
-- tenant_id and user_email are snapshots taken at order creation so that
-- admin tenant-isolation filtering and UI display remain stable even if
-- the user later moves tenant or changes email. payment_method reserves
-- 'wechat' / 'alipay' for future external payment integration.
CREATE TABLE IF NOT EXISTS payment_orders (
    id BIGINT PRIMARY KEY,
    user_id BIGINT NOT NULL,
    tenant_id VARCHAR(255) NOT NULL,
    user_email VARCHAR(255) NOT NULL,
    plan_id BIGINT,
    plan_name VARCHAR(255) NOT NULL DEFAULT '',
    amount BIGINT NOT NULL CHECK (amount >= 0),
    status VARCHAR(32) NOT NULL DEFAULT 'paid'
        CHECK (status IN ('paid', 'pending', 'refunded', 'cancelled')),
    payment_method VARCHAR(32) NOT NULL DEFAULT 'balance'
        CHECK (payment_method IN ('balance', 'wechat', 'alipay')),
    external_txn_id VARCHAR(255),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    paid_at TIMESTAMPTZ
);

-- Index for admin tenant-scoped order listing.
CREATE INDEX IF NOT EXISTS idx_payment_orders_tenant
    ON payment_orders(tenant_id, created_at DESC);

-- Index for per-user order queries (/users/me/orders).
CREATE INDEX IF NOT EXISTS idx_payment_orders_user
    ON payment_orders(user_id, created_at DESC);
