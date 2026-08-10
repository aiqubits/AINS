# 架构概览

## 系统架构

```
┌─────────────────────────────────────────────────────────────────┐
│  Frontend (Dioxus Multi-platform)                                │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  app/web (WASM)   app/desktop   app/mobile              │    │
│  │  ┌─────────────────────────────────────────────────┐    │    │
│  │  │  app/ui (共享组件库: navbar, sidebar, table...)  │    │    │
│  │  └─────────────────────────────────────────────────┘    │    │
│  │  ┌─────────────────────────────────────────────────┐    │    │
│  │  │  app/client-api (认证SDK, 类型定义, 测试)        │    │    │
│  │  └─────────────────────────────────────────────────┘    │    │
│  └─────────────────────────────────────────────────────────┘    │
├─────────────────────────────────────────────────────────────────┤
│  Nginx 反向代理 (限流 / 安全头 / 静态资源 / TLS 终止)             │
├─────────────────────────────────────────────────────────────────┤
│  Backend — 运行时抽象层 (Runtime trait)                           │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  [feature=ains-axum]   [feature=ains-salvo]     │    │
│  │  ┌──────────────────────┐ ┌──────────────────────────┐  │    │
│  │  │  ains-axum crate  │ │  ains-salvo crate    │  │    │
│  │  │  AxumRuntime<S>       │ │  SalvoRuntime<S>         │  │    │
│  │  │  实现 Runtime trait   │ │  实现 Runtime trait      │  │    │
│  │  └──────────────────────┘ └──────────────────────────┘  │    │
│  │  ┌──────────────────────────────────────────────────┐   │    │
│  │  │  server/ (业务代码, 框架无关)                      │   │    │
│  │  │  routes / handlers / services / middlewares       │   │    │
│  │  │  统一签名: async fn(ServerRequest)->Result<..>  │   │    │
│  │  └──────────────────────────────────────────────────┘   │    │
│  └─────────────────────────────────────────────────────────┘    │
├─────────────────────────────────────────────────────────────────┤
│  Service Layer                                                   │
│  ├─ AuthService         — 注册/登录/登出/令牌刷新                │
│  ├─ UserService         — 用户 CRUD / 角色管理                   │
│  ├─ CacheService        — Redis 缓存 (bb8 池, 优雅降级)          │
│  ├─ LockService         — 分布式锁 (Lua 原子释放, Drop 自动解锁) │
│  ├─ PasswordResetService                                        │
│  ├─ VerificationService — 邮箱验证                               │
│  └─ WechatService       — 微信验证码登录                         │
├─────────────────────────────────────────────────────────────────┤
│  Data Access Layer — AutoRouter (读写分离)                       │
│  ├─ SeaORM Entity / Repository                                  │
│  ├─ ConnectionTrait — query_one/query_all → 读库 (负载均衡)      │
│  ├─ ConnectionTrait — execute → 写库                             │
│  ├─ 熔断器 + 健康检查 + 降级回写                                 │
│  └─ SELECT FOR UPDATE → 强制写库                                 │
├─────────────────────────────────────────────────────────────────┤
│  Persistent Layer                                                │
│  ├─ PostgreSQL — 主库 (Docker 单实例 / K8s CNP 1主2从集群)       │
│  └─ Redis — 缓存 + 限流 + 分布式锁                               │
└─────────────────────────────────────────────────────────────────┘
```

---

## Runtime 抽象 — Web 框架运行时切换

### 设计目标

在 Axum 和 Salvo 之间切换时，**业务代码（routes / handlers / services）无需任何修改**。

### `Runtime` trait

文件: [crates/ains-runtime/src/runtime.rs](../crates/ains-runtime/src/runtime.rs)

```rust
pub trait Runtime: Clone + Send + Sync + Sized + 'static {
    type Router: Clone + Send + Sync;
    type MethodRouter: Clone + Send + Sync;
    type State: Clone + Send + Sync;

    fn new_router() -> Self::Router;
    fn nest(router: Self::Router, path: &str, sub: Self::Router) -> Self::Router;
    fn merge(router: Self::Router, other: Self::Router) -> Self::Router;
    fn with_route(router: Self::Router, path: &str, method: Self::MethodRouter) -> Self::Router;
    fn with_state(router: Self::Router, state: Self::State) -> Self::Router;
    fn serve(router: Self::Router, state: Self::State, addr: &str)
        -> impl Future<Output = Result<()>> + Send;
}
```

### 适配器

- **`ains-axum`**: `AxumRuntime<S>` 实现 `Runtime`，内部封装 `axum::Router<S>`
- **`ains-salvo`**: `SalvoRuntime<S>` 实现 `Runtime`，内部封装 `salvo::Router`

### 类型别名（server/lib.rs）

```rust
#[cfg(feature = "ains-salvo")]
pub type AppRuntime = SalvoRuntime<AppState>;

#[cfg(not(feature = "ains-salvo"))]
pub type AppRuntime = AxumRuntime<AppState>;
```

### 统一 Handler 签名

```rust
// 框架无关：ServerRequest 是 ains_axum::UnifiedRequest / ains_salvo::UnifiedRequest
// 的类型别名（由 feature 决定），二者均实现 ains_runtime::RequestContext trait
use ains_runtime::{HttpError, Response};
use crate::ServerRequest;

async fn my_handler(mut req: ServerRequest) -> Result<Response, HttpError> {
    let payload: MyPayload = req.parse_json().await.map_err(HttpError::bad_request)?;
    let state: AppState = extract_state(&req)?; // 等价于 req.get_data::<AppState>()
    Ok(Response::json(&result)?)
}
```

### 中间件抽象

`MiddlewareState` trait（[crates/ains-runtime/src/middleware.rs](../crates/ains-runtime/src/middleware.rs)）为认证中间件提供统一接口：

```rust
pub trait MiddlewareState: Clone + Send + Sync + 'static {
    fn jwt_secret(&self) -> &str;
    fn cookie_secure(&self) -> bool;
    async fn check_token_version(&self, user_id: i64, token_version: i32) -> Result<(), String>;
}
```

---

## AutoRouter — 主从读写分离

文件: [server/src/utils/db_router.rs](../server/src/utils/db_router.rs)

### 设计

`AutoRouter` 是一个实现了 SeaORM `ConnectionTrait` + `TransactionTrait` 的结构体，对业务代码**完全透明**。

### 路由规则

| 操作 | 路由目标 | 说明 |
|------|----------|------|
| `execute()` / `execute_unprepared()` | 写库 | INSERT / UPDATE / DELETE |
| `query_one()` / `query_all()` | 读库 | SELECT 查询 |
| `SELECT ... FOR UPDATE / FOR SHARE` | 写库 | 行级锁必须在主库 |
| `INSERT/UPDATE ... RETURNING` | 写库 | SeaORM 通过 `query_one` 执行，SQL 模板检测 |
| CTE (`WITH ...`) 包含写操作 | 写库 | 深度遍历检测主操作关键字 |
| `begin()` / `transaction()` | 写库 | 事务全部在主库执行 |

### 负载均衡策略

```rust
pub enum ReadStrategy {
    RoundRobin,  // 轮询（默认）
    Random,      // 随机
    Weighted,    // 加权随机
}
```

### 熔断器

- 读库连接失败 → 标记 `down_until`（默认 30s）
- 后台健康检查（默认 15s 间隔）→ 探测恢复后自动移除熔断
- 所有读库均不可用 → `fallback_to_write`（默认 true）降级回写库

### 重试机制

```
Phase 1: 每个读副本尝试一次（排除已尝试的）
Phase 2: retry_attempts 次额外重试（不限熔断器，给每个副本第二次机会）
Phase 3: 若 fallback_to_write=true → 回退写库
```

### 配置

```toml
database_read_urls = ["postgres://user:pass@replica1:5432/ains"]
[database_routing]
strategy = "round_robin"
retry_attempts = 2
circuit_break_ms = 30000
fallback_to_write = true
health_check_interval_secs = 15
```

---

## Redis 三件套：缓存 + 限流 + 分布式锁

三者共享同一个 `redis::Client` 实例，通过 `CacheService.redis_client()` 暴露给其他服务。

### CacheService — 统一缓存层

文件: [server/src/services/cache.rs](../server/src/services/cache.rs)

| 方法 | 说明 |
|------|------|
| `get<T>(key)` | 读取并反序列化 |
| `set<T>(key, val, ttl)` | 带 TTL 写入 |
| `get_or_insert<T>(key, ttl, f)` | 缓存命中返回，miss 则计算并回填 |
| `get_or_insert_with_lock(...)` | 缓存击穿保护：分布式锁防 stampede |
| `set_null(key, ttl)` | 负缓存标记（防缓存穿透） |
| `invalidate(key)` | 删除 key + 负缓存标记 |

**优雅降级**：Redis 不可用时，所有操作静默 no-op，服务不崩溃。

**缓存击穿保护**（`get_or_insert_with_lock`）：
- 适用于 K8s 多副本下热点 key 过期场景
- 仅一个 pod 回源计算，其余等待后读缓存
- 使用分布式锁 + 重试轮询

### distributed-ratelimit — 分布式限流

文件: [crates/distributed-ratelimit/](../crates/distributed-ratelimit/)

固定窗口算法，Redis `SET NX EX` + `INCR` 实现。

每个认证端点的独立配额（[server/src/routes/auth.rs](../server/src/routes/auth.rs)）：

| 端点 | IP 级别 | 邮箱级别 |
|------|---------|----------|
| `/login` | 20/10min | 5/10min |
| `/register` | 10/10min | - |
| `/forgot-password` | 5/10min | - |
| `/verify-email` | 20/10min | - |
| `/refresh` | 30/10min | - |

### LockGuard — 分布式锁

文件: [server/src/services/lock.rs](../server/src/services/lock.rs)

- **安全释放**：Lua 脚本原子检查锁值匹配（防止误释放）
- **自动释放**：`Drop` trait 自动解锁，进程崩溃时 TTL 兜底
- **双策略**：
  - `LockGuard::acquire` — fail-open（Redis 不可用返回 None）
  - `acquire_lock` — fail-close（Redis 不可用返回 Err）
- **共享连接**：复用 CacheService 的 `redis::Client`

---

## Snowflake ID 生成器

文件: [server/src/utils/snowflake.rs](../server/src/utils/snowflake.rs)

### 算法

- 1 bit 符号位 + 41 bits 毫秒时间戳（自定义 epoch） + 10 bits worker ID + 12 bits 序列号
- 无锁原子实现（`AtomicI64` + CAS），无需 Mutex

### Worker 协调

- 启动时通过数据库 `snowflake_worker` 表注册 worker_id（0-1023）
- 每 10 秒心跳保活，30 秒无心跳视为过期
- 退出时自动注销（`WorkerHandle` Drop 触发）
- 多节点自动分配不重复 ID

### JSON 序列化

`SnowflakeId` 包装类型：JSON 序列化为字符串（`"1234567890"`），防止 WASM 前端 JS Number 精度丢失。

---

## 中间件执行顺序

### Axum 模式（从外到内）

```
RequestBodyLimitLayer (10MB)        ← 最外层（防止 DoS）
  → CompressionLayer (Gzip/Brotli)
    → CorsLayer
      → TraceLayer (请求/响应日志)
        → Panic 中间件 (捕获 panic 返回 500)
          → 路由匹配
            → AuthMiddleware (/api 路径)
              → RateLimit 中间件 (/api/public/auth 路径)
```

### Salvo 模式（从外到内）

```
max_body_size (10MB)                ← 最外层
  → compression
    → cors
      → logger
        → catch_panic
          → 路由匹配
            → AuthMiddleware (/api 路径)
              → RateLimit 中间件 (/api/public/auth 路径)
```

---

## 速率限制体系

### 应用层 (Redis 分布式限流)

- `distributed-ratelimit` crate，固定窗口算法
- IP 级别 + 邮箱级别双重策略
- 每个认证端点独立配额

### 反向代理层 (Nginx)

```
认证端点 (/api/public/auth/) : 5 req/min  (防暴力破解)
通用 API (/api/)            : 60 req/min (防资源耗尽)
```

---

## 项目结构

```
ains/
├── server/                          # 后端服务（框架无关业务代码）
│   ├── migrations/                  # SQL 迁移 (001_init.sql)
│   ├── src/
│   │   ├── bootstrap/               # 启动引导（config/DB/state/migration）
│   │   │   ├── mod.rs
│   │   │   ├── axum.rs              # Axum 路由构建 + CORS 配置
│   │   │   └── salvo.rs             # Salvo 路由构建
│   │   ├── handlers/                # HTTP 处理器（框架无关）
│   │   │   ├── auth.rs / api.rs / wechat.rs / helpers.rs
│   │   │   ├── gateway.rs / responses.rs        # AI 网关
│   │   │   ├── tenant.rs / plan.rs / payment_order.rs / metering.rs
│   │   ├── middlewares/             # JWT 认证 / Panic 捕获 / RateLimitGuard
│   │   ├── repositories/            # SeaORM 仓储层
│   │   │   ├── user.rs / refresh_token.rs / snowflake_worker.rs
│   │   │   ├── tenant.rs / plan.rs / user_plan.rs
│   │   │   ├── payment_order.rs / token_usage.rs / channel.rs
│   │   ├── routes/                  # 路由注册（api.rs 需认证 / auth.rs 公开限流）
│   │   ├── services/                # 业务服务
│   │   │   ├── auth.rs / user.rs / cache.rs / lock.rs / wechat.rs
│   │   │   ├── verification.rs / password_reset.rs
│   │   │   ├── gateway.rs / dispatch.rs / responses.rs   # AI 网关与分发
│   │   │   ├── tenant.rs / plan.rs / payment_order.rs
│   │   │   └── metering.rs / quota.rs
│   │   └── utils/
│   │       ├── config.rs / error.rs / jwt.rs / password.rs
│   │       ├── validator.rs / logger.rs / snowflake.rs
│   │       └── db_router.rs         # AutoRouter 读写分离
│   ├── tests/                       # axum_* / salvo_* 双框架集成测试
│   └── Cargo.toml
│
├── app/                             # 前端多端应用
│   ├── ui/                          # 共享 UI 组件库 (Dioxus)
│   │   └── src/ (chat_view / permission_dialog / skills_panel /
│   │             memory_viewer / tool_panel / agent_status / ...)
│   ├── web/                         # Web 端 (WASM，含 Agent 装配桥接)
│   │   ├── src/views/ (auth / dashboard / users / tenants / plans /
│   │   │            orders / metering / channels / agent_chat / skills / ...)
│   │   ├── src/agent/ (service / view_model / permission_bridge)
│   │   └── src/auth/ (jwt / storage / state)
│   ├── desktop/                     # 桌面端（经 #[path] 复用 web 桥接与视图）
│   ├── mobile/                      # 移动端
│   └── client-api/                  # 类型化客户端 API SDK (chat/embed/stt/tts)
│
├── crates/
│   ├── rust-agent/                  # 客户端 Agent Runtime 核心（Native + WASM）
│   ├── ains-runtime/                # Runtime trait + 统一请求/响应/错误
│   ├── ains-axum/                   # Axum 适配器
│   ├── ains-salvo/                  # Salvo 适配器
│   ├── distributed-ratelimit/       # Redis 分布式限流
│   ├── emailserver/                 # SMTP 邮件发送
│   ├── wechat-api/                  # 微信公众平台 SDK
│   └── i18n/                        # 国际化 (过程宏)
│
├── k8s/
│   ├── namespace.yml / postgres.yml / postgres-cluster.yml (CNP 1主2从)
│   ├── redis.yml / ains.yml (3 副本) / ains-web.yml
│   ├── configmap.yml / secret.yml.example / ingress.yml
│
├── docker-compose.yml               # Docker 全栈编排
├── Dockerfile.server / Dockerfile.web
├── config.toml / config.toml.example
└── .github/workflows/ains.yml       # CI: 双目标构建 + wasm 测试 + 安全审计
```

---

## 客户端 Agent Runtime（rust-agent）

`crates/rust-agent` 是 AINS 的客户端 Agent Runtime 核心，**同一 crate 双目标编译**（Native 与 WASM），业务逻辑只依赖 trait 抽象，平台实现（tokio / wasm-bindgen / redb / IndexedDB）收敛在 cfg 门控文件中按 target 互斥编译。

| 模块 | 职责 |
|------|------|
| `kernel/` | Agent Kernel：事件驱动 FSM 主循环、流式工具调用、协作式中断机制 |
| `memory/` | 三层记忆：KV（redb / IndexedDB）、Vector（HNSW + int8 量化）、Document，含 memdir 与 LLM 提取 |
| `tools/` | Tool Runtime：本地工具 + MCP（stdio / streamable-http），工具输出预算管理 |
| `policy/` | 三态权限引擎 + 平台沙箱（Linux bubblewrap / macOS sandbox-exec / Windows Job Object / Mobile） |
| `skills/` | Skills 系统：版本化存储、评分淘汰、自动回滚 |
| `perception/` | Vision / Voice / File 感知输入通道 |
| `context/` | 分段系统提示、会话持久化、四级上下文压缩 |
| `hooks/` | 10 触发点 Hook 系统 |
| `commands/` `plugins/` `swarm/` `tasks/` `personalization/` | Slash 命令 / 插件 / 子代理 / 后台任务 / 个性化扩展 |

安全基线遵循 **fail-closed** 原则：沙箱不可用时拒绝执行而非降级；`EncryptedKvStore` 提供 ChaCha20-Poly1305 AEAD 本地加密。

---

## 安全特性

### 密码安全

- **算法**: Argon2id (KDF)
- **盐化**: 自动生成唯一盐
- **哈希**: 不存储明文密码

### JWT 令牌

- **签名算法**: HS256
- **过期时间**: 可配置（默认 1 小时，记住我 30 天）
- **版本控制**: `token_version` 字段，密码变更后旧令牌立即失效
- **Refresh Token**: 90 天有效，轮转机制（每次刷新同时作废旧 token）
- **Cookie**: Secure 标志（生产环境），HttpOnly + SameSite

### 输入验证

- **邮箱验证**: RFC 5322 格式检查
- **密码强度**: 最少 8 字符，必含大小写字母和数字
- **长度限制**: 名字 2-50 字符

### HTTP 安全头

```
Strict-Transport-Security: max-age=31536000; includeSubDomains
X-Frame-Options: SAMEORIGIN
X-Content-Type-Options: nosniff
Referrer-Policy: strict-origin-when-cross-origin
Content-Security-Policy: default-src 'self'; script-src 'self' 'wasm-unsafe-eval'
```

### 可靠性特性

- **Panic 恢复**: 自动捕获 panic，返回 500 错误而不是崩溃
- **优雅关闭**: SIGTERM/SIGINT 信号处理
- **连接池**: PostgreSQL + Redis 双连接池管理
- **健康检查**: Liveness / Readiness 探测
- **Redis 优雅降级**: 不可用时缓存静默 no-op，服务不启动失败

---

## 分布式 ID — Snowflake

| 组件 | 说明 |
|------|------|
| 算法 | Twitter Snowflake，64 位整数 |
| Worker 协调 | DB `snowflake_worker` 表自动注册/心跳/注销 |
| 无锁生成 | `AtomicI64` + CAS，无需 Mutex |
| JS 精度安全 | JSON 序列化为字符串 |

---

## 依赖版本

### 核心依赖

| 库 | 版本 | 用途 |
|----|------|------|
| axum | 0.8.9 | Web 框架（默认引擎） |
| salvo | 0.93 | Web 框架（可选引擎） |
| tokio | 1 | 异步运行时 |
| sea-orm | 1.1.20 | 异步 ORM（PostgreSQL） |
| redis | 1.2.3 | Redis 客户端 |
| dioxus | 0.7.7 | 前端框架 |
| tower-http | 0.6 | 中间件（CORS/压缩/限流/追踪） |

### 认证和安全

| 库 | 版本 | 用途 |
|----|------|------|
| jsonwebtoken | 9 | JWT 签发/验证 |
| argon2 | 0.5 | 密码哈希 |
| validator | 0.19 | 输入验证 |

### 序列化和工具

| 库 | 版本 | 用途 |
|----|------|------|
| serde | 1 | 序列化/反序列化 |
| chrono | 0.4 | 日期时间 |
| uuid | 1 | UUID 生成 |

### 日志和错误

| 库 | 版本 | 用途 |
|----|------|------|
| tracing | 0.1 | 结构化日志 |
| thiserror | 2 | 自定义错误 |
| anyhow | 1 | 通用错误处理 |

---

## API 文档

### 基础 URL

```
http://127.0.0.1:3000/api
```

### 健康检查

```http
GET /api/health
```

响应:

```json
{
  "status": "ok",
  "version": "0.1.0"
}
```

### 认证端点

#### 注册用户

```http
POST /api/public/auth/register
Content-Type: application/json

{
  "email": "user@example.com",
  "password": "SecurePass123",
  "name": "User Name"
}
```

响应 (201 Created):

```json
{
  "message": "User registered successfully",
  "user_id": "550e8400-e29b-41d4-a716-446655440000"
}
```

**密码要求:**
- 最少 8 字符
- 至少包含 1 个小写字母
- 至少包含 1 个大写字母
- 至少包含 1 个数字

#### 登录

```http
POST /api/public/auth/login
Content-Type: application/json

{
  "email": "user@example.com",
  "password": "SecurePass123"
}
```

响应 (200 OK):

```json
{
  "token": "eyJ0eXAiOiJKV1QiLCJhbGc...",
  "token_type": "Bearer",
  "expires_in": 3600,
  "user_id": "550e8400-e29b-41d4-a716-446655440000",
  "role": "user",
  "refresh_token": "dGhpcyBpcyBhIHJlZnJlc2ggdG9rZW4..."
}
```

#### 令牌刷新

```http
POST /api/public/auth/refresh
Content-Type: application/json

{
  "refresh_token": "dGhpcyBpcyBhIHJlZnJlc2ggdG9rZW4..."
}
```

#### 登出

```http
POST /api/public/auth/logout
Authorization: Bearer <token>
Content-Type: application/json

{
  "refresh_token": "dGhpcyBpcyBhIHJlZnJlc2ggdG9rZW4..."
}
```

#### 邮箱验证

```http
POST /api/public/auth/verify-email
Content-Type: application/json

{
  "code": "123456"
}
```

#### 重发验证码

```http
POST /api/public/auth/resend-code
Content-Type: application/json

{
  "email": "user@example.com"
}
```

#### 忘记密码

```http
POST /api/public/auth/forgot-password
Content-Type: application/json

{
  "email": "user@example.com"
}
```

#### 重置密码

```http
POST /api/public/auth/reset-password
Content-Type: application/json

{
  "token": "reset-token-from-email",
  "password": "NewSecurePass123"
}
```

### 用户管理

#### 创建用户 (需要认证)

```http
POST /api/users
Authorization: Bearer <token>
Content-Type: application/json

{
  "email": "newuser@example.com",
  "password": "SecurePass123",
  "name": "New User"
}
```

#### 获取用户 (需要认证)

```http
GET /api/users/{id}
Authorization: Bearer <token>
```

#### 更新用户 (需要认证)

```http
PUT /api/users/{id}
Authorization: Bearer <token>
Content-Type: application/json

{
  "email": "updated@example.com",
  "name": "Updated Name",
  "role": "user"
}
```

#### 删除用户 (需要认证)

```http
DELETE /api/users/{id}
Authorization: Bearer <token>
```

#### 列表用户 - 分页 (需要认证)

```http
GET /api/users?page=1&per_page=10
Authorization: Bearer <token>
```

响应:

```json
{
  "items": [...],
  "total": 42,
  "page": 1,
  "per_page": 10,
  "total_pages": 5
}
```

### AI 网关（需要认证）

统一入口 `POST /api/ai/response`，按 `capability` 字段路由到不同能力（省略时服务端按输入自动检测）：

| capability | 说明 | 输入 |
|------------|------|------|
| `chat` | 对话补全（vision 图片消息经 chat 携带） | 消息数组 |
| `vision` | 视觉理解（同 chat 路由） | 消息数组（含图片） |
| `embedding` | 向量化 | 文本 / 文本数组 |
| `stt` | 语音转文字 | 音频 |
| `tts` | 文字转语音 | 文本 |
| `web_search` | 联网搜索 | 文本 |

```http
POST /api/ai/response
Authorization: Bearer <token>
Content-Type: application/json

{
  "capability": "chat",
  "model": "optional-model",
  "input": "Hello",
  "stream": true
}
```

`stream: true` 时返回 SSE 流式响应。该接口对 `user` 角色强制校验有效套餐（`admin`/`system` 豁免），未持套餐返回 `403 no_active_plan`。

### 业务管理端点（需要认证）

| 资源 | 端点 |
|------|------|
| 租户 | `GET/POST /api/tenants` · `GET/PUT/DELETE /api/tenants/{id}` |
| 套餐 | `GET/POST /api/plans` · `GET/PUT/DELETE /api/plans/{id}` · `GET /api/plans/available` · `POST /api/plans/{id}/purchase` |
| 订单 | `GET/POST /api/orders` · `GET/PUT/DELETE /api/orders/{id}` · `GET /api/users/me/orders` |
| 计量 | `GET /api/usage` · `GET /api/usage/stats` |
| 渠道 | `GET/POST /api/channels` · `GET/PUT/DELETE /api/channels/{id}` |
| 用户管理 | `GET/POST /api/users` · `GET/PUT/DELETE /api/users/{id}` · `GET/POST /api/users/{id}/plans` · `GET /api/users/{id}/balance` · `POST /api/users/{id}/balance/adjust` |
| 个人中心 | `GET /api/users/me` · `PUT /api/users/me/password` · `GET /api/users/me/plans` · `POST /api/users/me/logout-all` |

### 微信登录

```http
GET /api/public/wechat/callback    # 微信服务器 GET 验证
POST /api/public/wechat/callback   # 微信消息回调
GET /api/public/auth/wechat-enabled # 验证码登录功能开关

# 验证码作为邮箱+密码登录的第二因子（共享验证码模型，不绑定 openid）
POST /api/public/auth/login        # 启用微信后额外携带 captcha_code
{
  "email": "user@example.com",
  "password": "******",
  "captcha_code": "12345"
}
```

> **安全提示（共享验证码模型）**：验证码不与登录账号绑定，任何关注了公众号的
> 用户都能获取有效验证码，且任意有效验证码都可满足该第二因子校验。因此它证明的是
> “请求者是公众号关注者”，而非账号级的强 MFA——掌握了目标账号密码且本人（或同伙）
> 是关注者的攻击者仍可登录。若需账号级绑定登录，可改用 `wechat-api` 中保留的
> `LoginService` / `UserBindingStore`（openid 绑定的免密登录）。

### 错误处理

所有错误响应遵循统一格式:

```json
{
  "error": "error_type",
  "message": "Detailed error message"
}
```

**错误类型:**
- `bad_request` (400) — 请求参数错误
- `unauthorized` (401) — 缺少或无效的认证
- `forbidden` (403) — 权限不足
- `not_found` (404) — 资源不存在
- `conflict` (409) — 资源冲突（如邮箱重复）
- `validation_error` (400) — 输入验证失败
- `internal_error` (500) — 服务器内部错误
- `service_unavailable` (503) — 服务不可用