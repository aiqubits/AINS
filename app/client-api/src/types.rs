use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ──────────────────────────────────────────────
//  Auth types
// ──────────────────────────────────────────────

/// Login request body
#[derive(Debug, Serialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub remember: bool,
    /// WeChat captcha code (only required when wechat captcha-login is enabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captcha_code: Option<String>,
}

/// Login response
#[derive(Debug, Deserialize)]
pub struct LoginResponse {
    pub token: String,
    pub token_type: String,
    pub expires_in: u64,
    pub user_id: String,
    pub role: String,
    /// Non-zero when the server issued a refresh token ("remember me" login).
    /// Zero / absent when the login was non-persistent.
    #[serde(default)]
    pub refresh_expires_in: Option<u64>,
}

/// Register request body
#[derive(Debug, Serialize)]
pub struct RegisterRequest {
    pub email: String,
    pub password: String,
    pub name: String,
    /// Password confirmation (must match password).
    pub password_confirm: String,
    #[serde(default)]
    pub remember: bool,
}

/// Register response
#[derive(Debug, Deserialize)]
pub struct RegisterResponse {
    pub message: String,
    pub user_id: String,
    /// Whether the email is already verified.
    ///
    /// `false` means the server has issued a 6-digit verification code and
    /// the user must complete email verification before being able to log in.
    /// `true` means the server has either auto-verified the user (no SMTP
    /// configured, or send failed) or the user was already verified.
    ///
    /// `#[serde(default)]` preserves compatibility with older server responses
    /// or fixtures that do not include the field — it will deserialize as
    /// `false`, which is the safer default (forces the frontend to show the
    /// verification UI rather than silently auto-login).
    #[serde(default)]
    pub email_verified: bool,
}

/// Verify email request body
#[derive(Debug, Serialize)]
pub struct VerifyEmailRequest {
    pub email: String,
    pub code: String,
}

/// Verify email response
#[derive(Debug, Deserialize)]
pub struct VerifyEmailResponse {
    pub message: String,
}

/// Resend verification code request body
#[derive(Debug, Serialize)]
pub struct ResendCodeRequest {
    pub email: String,
}

/// Resend verification code response
#[derive(Debug, Deserialize)]
pub struct ResendCodeResponse {
    pub message: String,
}

// ──────────────────────────────────────────────
//  Password reset types
// ──────────────────────────────────────────────

/// Forgot password request body
///
/// 仅含 `email` —— 服务端对未知邮箱走 dummy hash 恒定分支，
/// 永远返回 200 + 通用文案（防 enumeration），因此请求体不会泄露用户存在性。
#[derive(Debug, Serialize)]
pub struct ForgotPasswordRequest {
    pub email: String,
}

/// Forgot password response
#[derive(Debug, Deserialize)]
pub struct ForgotPasswordResponse {
    pub message: String,
}

/// Reset password request body
///
/// `code` 必须是 6 位数字验证码（来自密码重置邮件），
/// `new_password` 须满足服务端复杂度校验（≥8 字符 + 复杂度）。
#[derive(Debug, Serialize)]
pub struct ResetPasswordRequest {
    pub email: String,
    pub code: String,
    pub new_password: String,
}

/// Reset password response
///
/// 与 `LoginResponse` 同形（多一个 `message`），因为服务端在事务内
/// 原子地 `token_version += 1` 后直接签发新 JWT —— 验证码校验通过
/// 即等于登录。
#[derive(Debug, Deserialize)]
pub struct ResetPasswordResponse {
    pub message: String,
    pub token: String,
    pub token_type: String,
    pub expires_in: u64,
    pub user_id: String,
    pub role: String,
}

/// Refresh token response
#[derive(Debug, Deserialize)]
pub struct RefreshResponse {
    pub token: String,
    pub token_type: String,
    pub expires_in: u64,
    pub user_id: String,
    pub role: String,
    pub refresh_expires_in: u64,
}

/// Logout response
///
/// The body is informational only — the actual session termination happens
/// via the `Set-Cookie` headers (which are invisible to JS) and the
/// server-side deletion of the refresh-token row. Clients should still
/// clear their own in-memory JWT state and the readable `ains_exp`
/// cookie after calling this endpoint.
#[derive(Debug, Deserialize)]
pub struct LogoutResponse {
    pub message: String,
}

// ──────────────────────────────────────────────
//  User types
// ──────────────────────────────────────────────

/// User response (mirrors server's `UserResponse`)
#[derive(Debug, Clone, Deserialize)]
pub struct UserResponse {
    pub id: String,
    pub email: String,
    pub name: String,
    pub role: String,
    #[serde(default)]
    pub tenant_id: String,
    /// Tenant display name resolved server-side (best-effort; may be absent).
    #[serde(default)]
    pub tenant_name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// User balance (stored as big value, 1 display unit = 10^10 stored units)
    #[serde(default)]
    pub balance: i64,
}

/// Create user request body (admin)
#[derive(Debug, Serialize)]
pub struct CreateUserRequest {
    pub email: String,
    pub password: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Only effective when the actor is `system`; tenant admins are scoped to
    /// their own tenant server-side regardless of this field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
}

/// Update user request body (admin — all fields optional)
#[derive(Debug, Serialize)]
pub struct UpdateUserRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Only effective when the actor is `system`; tenant admins are scoped to
    /// their own tenant server-side regardless of this field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
}

/// Paginated list of users
#[derive(Debug, Deserialize)]
pub struct PaginatedUsersResponse {
    pub items: Vec<UserResponse>,
    pub total: u64,
    pub page: u64,
    pub per_page: u64,
    pub total_pages: u64,
}

// ──────────────────────────────────────────────
//  Health types
// ──────────────────────────────────────────────

/// Health check response
#[derive(Debug, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}

/// Generic delete response
#[derive(Debug, Deserialize)]
pub struct DeleteResponse {
    pub message: String,
}

// ──────────────────────────────────────────────
//  Self-service types
// ──────────────────────────────────────────────

/// Change password request body
#[derive(Debug, Serialize)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

/// Change password response
///
/// `new_token` 必须被调用方消费 —— 服务端在改密时原子地
/// `token_version += 1`，原 JWT 永久失效；调用方必须用 `new_token` 替换
/// 旧 token，否则下次 API 调用会 401。
#[derive(Debug, Deserialize)]
pub struct ChangePasswordResponse {
    pub message: String,
    pub new_token: String,
}

// ──────────────────────────────────────────────
//  WeChat captcha-login types
// ──────────────────────────────────────────────

/// WeChat enabled check response
#[derive(Debug, Deserialize)]
pub struct WechatEnabledResponse {
    pub enabled: bool,
}

// ──────────────────────────────────────────────
//  Balance types
// ──────────────────────────────────────────────

/// Set balance request body (admin/system only)
#[derive(Debug, Serialize)]
pub struct SetBalanceRequest {
    /// Balance in stored units (1 display unit = 10^10 stored units)
    pub balance: i64,
}

/// Set balance response
#[derive(Debug, Deserialize)]
pub struct SetBalanceResponse {
    pub balance: i64,
    pub display_balance: f64,
    pub message: String,
}

/// Adjust balance request body (delta amount, positive = increase, negative = decrease)
#[derive(Debug, Serialize)]
pub struct AdjustBalanceRequest {
    /// Amount in stored units (positive = increase, negative = decrease)
    pub amount: i64,
}

/// Adjust balance response
#[derive(Debug, Deserialize)]
pub struct AdjustBalanceResponse {
    pub balance: i64,
    pub display_balance: f64,
    pub message: String,
}

// ──────────────────────────────────────────────
//  Tenant types
// ──────────────────────────────────────────────

/// Tenant response (mirrors server's `TenantResponse`)
#[derive(Debug, Clone, Deserialize)]
pub struct TenantResponse {
    pub id: String,
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub user_count: u64,
    #[serde(default)]
    pub channel_count: u64,
    pub created_at: DateTime<Utc>,
}

/// Tenant list response (paginated)
#[derive(Debug, Deserialize)]
pub struct TenantListResponse {
    pub items: Vec<TenantResponse>,
    pub total: u64,
    pub page: u64,
    pub per_page: u64,
    pub total_pages: u64,
}

/// Create tenant request body
#[derive(Debug, Serialize)]
pub struct CreateTenantRequest {
    pub name: String,
}

/// Update tenant request body
#[derive(Debug, Serialize)]
pub struct UpdateTenantRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

// ──────────────────────────────────────────────
//  Channel types
// ──────────────────────────────────────────────

/// Channel response (mirrors server's `ChannelResponse`)
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelResponse {
    pub id: String,
    pub tenant_id: String,
    /// Tenant display name resolved server-side (best-effort; may be absent).
    #[serde(default)]
    pub tenant_name: Option<String>,
    pub name: String,
    pub protocol_type: String,
    pub models: serde_json::Value,
    pub capabilities: serde_json::Value,
    pub base_url: String,
    pub is_active: bool,
    pub weight: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Channel list response (paginated)
#[derive(Debug, Deserialize)]
pub struct ChannelListResponse {
    pub items: Vec<ChannelResponse>,
    pub total: u64,
    pub page: u64,
    pub per_page: u64,
    pub total_pages: u64,
}

/// Create channel request body
#[derive(Serialize)]
pub struct CreateChannelRequest {
    pub name: String,
    pub protocol_type: String,
    pub models: Vec<String>,
    pub capabilities: Vec<String>,
    pub api_key: String,
    pub base_url: String,
    pub weight: i32,
    pub is_active: bool,
    /// Only required when actor is system (to specify which tenant the channel belongs to)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
}

impl std::fmt::Debug for CreateChannelRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreateChannelRequest")
            .field("name", &self.name)
            .field("protocol_type", &self.protocol_type)
            .field("models", &self.models)
            .field("capabilities", &self.capabilities)
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("weight", &self.weight)
            .field("is_active", &self.is_active)
            .field("tenant_id", &self.tenant_id)
            .finish()
    }
}

/// Update channel request body (all fields optional)
#[derive(Serialize)]
pub struct UpdateChannelRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_active: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
}

impl std::fmt::Debug for UpdateChannelRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateChannelRequest")
            .field("name", &self.name)
            .field("protocol_type", &self.protocol_type)
            .field("models", &self.models)
            .field("capabilities", &self.capabilities)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("base_url", &self.base_url)
            .field("is_active", &self.is_active)
            .field("weight", &self.weight)
            .field("tenant_id", &self.tenant_id)
            .finish()
    }
}

// ──────────────────────────────────────────────
//  Token usage / Metering types
// ──────────────────────────────────────────────

/// Token usage record response
#[derive(Debug, Clone, Deserialize)]
pub struct TokenUsageResponse {
    pub id: i64,
    pub user_id: i64,
    pub tenant_id: String,
    pub channel_id: String,
    pub model: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub request_type: String,
    pub created_at: DateTime<Utc>,
}

/// Paginated token usage response
#[derive(Debug, Deserialize)]
pub struct PaginatedUsageResponse {
    pub items: Vec<TokenUsageResponse>,
    pub total: u64,
    pub page: u64,
    pub per_page: u64,
    pub total_pages: u64,
}

/// Usage statistics response
#[derive(Debug, Clone, Deserialize)]
pub struct UsageStatsResponse {
    pub total_requests: u64,
    pub total_prompt_tokens: i64,
    pub total_completion_tokens: i64,
    pub total_tokens: i64,
    pub model_breakdown: Vec<ModelUsageSummary>,
}

/// Per-model usage summary
#[derive(Debug, Clone, Deserialize)]
pub struct ModelUsageSummary {
    pub model: String,
    pub request_count: u64,
    pub total_tokens: i64,
}

/// Query parameters for listing token usage (used as query string)
/// All filter fields are optional.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ListUsageFilter {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_type: Option<String>,
    /// ISO 8601 date string (e.g. "2026-01-01T00:00:00Z")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_to: Option<String>,
}

/// Query parameters for usage stats (alias of `ListUsageFilter` — identical fields).
pub type UsageStatsFilter = ListUsageFilter;

// ──────────────────────────────────────────────
//  Plan types
// ──────────────────────────────────────────────

/// Plan response (mirrors server's `PlanResponse`)
#[derive(Debug, Clone, Deserialize)]
pub struct PlanResponse {
    pub id: String,
    pub tenant_id: String,
    /// Tenant display name resolved server-side (best-effort; may be absent).
    #[serde(default)]
    pub tenant_name: Option<String>,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Price in stored units (1 display unit = 10^10 stored units)
    pub price: i64,
    pub total_calls: i64,
    pub validity_days: i32,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Plan list response (paginated)
#[derive(Debug, Deserialize)]
pub struct PlanListResponse {
    pub items: Vec<PlanResponse>,
    pub total: u64,
    pub page: u64,
    pub per_page: u64,
    pub total_pages: u64,
}

/// Available plans response (user-facing, active plans of own tenant)
#[derive(Debug, Deserialize)]
pub struct AvailablePlansResponse {
    pub items: Vec<PlanResponse>,
}

/// Create plan request body
#[derive(Debug, Serialize)]
pub struct CreatePlanRequest {
    /// Only required when actor is system (to pick the owning tenant)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub price: i64,
    pub total_calls: i64,
    pub validity_days: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// Update plan request body (all fields optional)
#[derive(Debug, Default, Serialize)]
pub struct UpdatePlanRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_calls: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validity_days: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// User plan instance response (mirrors server's `UserPlanResponse`)
#[derive(Debug, Clone, Deserialize)]
pub struct UserPlanResponse {
    pub id: String,
    pub user_id: String,
    #[serde(default)]
    pub plan_id: Option<String>,
    pub plan_name: String,
    pub total_calls: i64,
    pub remaining_calls: i64,
    pub expires_at: DateTime<Utc>,
    pub source: String,
    pub created_at: DateTime<Utc>,
    /// Derived state: "active" | "expired" | "exhausted".
    pub status: String,
}

/// User plan list response ({ items })
#[derive(Debug, Deserialize)]
pub struct UserPlanListResponse {
    pub items: Vec<UserPlanResponse>,
}

/// Assign plan request body (admin grants a plan to a user)
#[derive(Debug, Serialize)]
pub struct AssignPlanRequest {
    pub plan_id: String,
}

/// Purchase plan response
#[derive(Debug, Deserialize)]
pub struct PurchasePlanResponse {
    pub order: PaymentOrderResponse,
    pub user_plan: UserPlanResponse,
    /// Remaining balance after deduction (stored units).
    pub balance: i64,
    pub display_balance: f64,
    pub message: String,
}

// ──────────────────────────────────────────────
//  Payment order types
// ──────────────────────────────────────────────

/// Payment order response (mirrors server's `PaymentOrderResponse`)
#[derive(Debug, Clone, Deserialize)]
pub struct PaymentOrderResponse {
    pub id: String,
    pub user_id: String,
    pub tenant_id: String,
    /// Tenant display name resolved server-side (best-effort; may be absent).
    #[serde(default)]
    pub tenant_name: Option<String>,
    pub user_email: String,
    #[serde(default)]
    pub plan_id: Option<String>,
    #[serde(default)]
    pub plan_name: String,
    /// Amount in stored units (1 display unit = 10^10 stored units)
    pub amount: i64,
    pub status: String,
    pub payment_method: String,
    #[serde(default)]
    pub external_txn_id: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub paid_at: Option<DateTime<Utc>>,
}

/// Payment order list response (paginated)
#[derive(Debug, Deserialize)]
pub struct PaymentOrderListResponse {
    pub items: Vec<PaymentOrderResponse>,
    pub total: u64,
    pub page: u64,
    pub per_page: u64,
    pub total_pages: u64,
}

/// Create payment order request body (admin manual entry)
#[derive(Debug, Serialize)]
pub struct CreateOrderRequest {
    pub user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    /// Amount in stored units (1 display unit = 10^10 stored units)
    pub amount: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payment_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_txn_id: Option<String>,
}

/// Update payment order request body (all fields optional)
#[derive(Debug, Default, Serialize)]
pub struct UpdateOrderRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payment_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_txn_id: Option<String>,
}

/// Filters for the admin order listing (used as query string)
#[derive(Debug, Clone, Default, Serialize)]
pub struct ListOrdersFilter {
    /// system-only tenant filter; ignored for admin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    /// Snowflake user ID as string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}
