use std::sync::{Arc, RwLock};

use reqwest::Method;
use serde::{Serialize, de::DeserializeOwned};

use crate::config::ClientConfig;
use crate::error::ClientError;
use crate::types::*;

/// 指数退避延迟的最大位移量（shift 范围 0..=2），延迟序列：500ms, 1s, 2s
const MAX_BACKOFF_SHIFT: u32 = 2;

#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;

/// 类型化 HTTP 客户端，封装所有 ains 后端 API 请求。
///
/// # Examples
///
/// ```rust,no_run
/// # use client_api::{Client, ClientConfig};
/// # async fn _doctest() -> Result<(), Box<dyn std::error::Error>> {
/// # // ⚠ 本示例需要真实后端，仅作 API 参考。
/// let client = Client::new(ClientConfig::new("http://127.0.0.1:8080"))?;
///
/// // 登录
/// let login = client.login("admin@example.com", "password123", false, None::<String>).await?;
/// client.set_token(login.token);
///
/// // 列出用户
/// let users = client.list_users(1, 20).await?;
/// for u in &users.items {
///     println!("{} <{}>", u.name, u.email);
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

#[derive(Debug)]
struct ClientInner {
    client: reqwest::Client,
    config: ClientConfig,
    auth_token: RwLock<Option<String>>,
}

impl Client {
    /// 创建新的 API 客户端。
    pub fn new(config: ClientConfig) -> Result<Self, ClientError> {
        config.validate()?;

        let client = {
            #[cfg(not(target_arch = "wasm32"))]
            {
                // 检查是否应禁用系统 HTTP 代理：
                // 1. 配置中显式设置 no_proxy = true
                // 2. 环境变量 AINS_SYS_NO_PROXY = true（全局覆盖）
                let no_proxy = config.no_proxy
                    || std::env::var("AINS_SYS_NO_PROXY")
                        .ok()
                        .map(|v| v.eq_ignore_ascii_case("true"))
                        .unwrap_or(false);

                let mut builder = reqwest::Client::builder()
                    .timeout(Duration::from_secs(config.timeout_secs))
                    .cookie_store(true);
                if no_proxy {
                    builder = builder.no_proxy();
                }
                builder.build().map_err(|e| {
                    ClientError::Config(format!("Failed to create HTTP client: {}", e))
                })?
            }
            #[cfg(target_arch = "wasm32")]
            {
                // WASM 下 reqwest 不支持设置超时和代理（浏览器 fetch API 控制）
                let _ = config.timeout_secs;
                reqwest::Client::builder().build().map_err(|e| {
                    ClientError::Config(format!("Failed to create HTTP client: {}", e))
                })?
            }
        };

        Ok(Self {
            inner: Arc::new(ClientInner {
                client,
                config,
                auth_token: RwLock::new(None),
            }),
        })
    }

    // ──────────────────────────────────────────
    //  Token management（线程安全）
    // ──────────────────────────────────────────

    /// 设置认证 Token（线程安全，可在任意 `&self` 上下文中调用）。
    pub fn set_token(&self, token: impl Into<String>) {
        let mut guard = self.inner.auth_token.write().expect("RwLock poisoned");
        *guard = Some(token.into());
    }

    /// 清除认证 Token。
    pub fn clear_token(&self) {
        let mut guard = self.inner.auth_token.write().expect("RwLock poisoned");
        *guard = None;
    }

    /// 获取当前 Token 的副本。
    pub fn token(&self) -> Option<String> {
        let guard = self.inner.auth_token.read().expect("RwLock poisoned");
        guard.clone()
    }

    /// 检查是否已设置 Token。
    pub fn is_authenticated(&self) -> bool {
        let guard = self.inner.auth_token.read().expect("RwLock poisoned");
        guard.is_some()
    }

    /// 获取当前配置。
    pub fn config(&self) -> &ClientConfig {
        &self.inner.config
    }

    // ──────────────────────────────────────────
    //  Auth endpoints
    // ──────────────────────────────────────────

    /// 登录 — `POST /api/public/auth/login`
    pub async fn login(
        &self,
        email: impl Into<String>,
        password: impl Into<String>,
        remember: bool,
        captcha_code: Option<String>,
    ) -> Result<LoginResponse, ClientError> {
        let body = LoginRequest {
            email: email.into(),
            password: password.into(),
            remember,
            captcha_code,
        };
        self.post_json_no_auth("/api/public/auth/login", &body)
            .await
    }

    /// 注册 — `POST /api/public/auth/register`
    pub async fn register(
        &self,
        email: impl Into<String>,
        password: impl Into<String>,
        name: impl Into<String>,
        remember: bool,
        password_confirm: impl Into<String>,
    ) -> Result<RegisterResponse, ClientError> {
        let body = RegisterRequest {
            email: email.into(),
            password: password.into(),
            name: name.into(),
            remember,
            password_confirm: password_confirm.into(),
        };
        self.post_json_no_auth("/api/public/auth/register", &body)
            .await
    }

    /// 提交 6 位验证码 — `POST /api/public/auth/verify-email`
    ///
    /// 失败时由服务端以 `400` 返回（统一文案以防 user enumeration）。
    pub async fn verify_email(
        &self,
        email: impl Into<String>,
        code: impl Into<String>,
    ) -> Result<VerifyEmailResponse, ClientError> {
        let body = VerifyEmailRequest {
            email: email.into(),
            code: code.into(),
        };
        self.post_json_no_auth("/api/public/auth/verify-email", &body)
            .await
    }

    /// 重新发送验证码 — `POST /api/public/auth/resend-code`
    ///
    /// 服务端有 60 秒冷却，过早调用会以 `400` 拒绝。
    pub async fn resend_code(
        &self,
        email: impl Into<String>,
    ) -> Result<ResendCodeResponse, ClientError> {
        let body = ResendCodeRequest {
            email: email.into(),
        };
        self.post_json_no_auth("/api/public/auth/resend-code", &body)
            .await
    }

    // ──────────────────────────────────────────
    //  Password reset (public)
    // ──────────────────────────────────────────

    /// 申请密码重置邮件 — `POST /api/public/auth/forgot-password`
    ///
    /// 服务端对未知邮箱走 Argon2 dummy hash 恒定分支，**永远返回 200**。
    /// 客户端无需也无法区分"邮箱是否存在"。若 SMTP 未配置，
    /// 服务端会以 `503` 拒绝（消息为通用"重置不可用"）。
    pub async fn forgot_password(
        &self,
        email: impl Into<String>,
    ) -> Result<ForgotPasswordResponse, ClientError> {
        let body = ForgotPasswordRequest {
            email: email.into(),
        };
        self.post_json_no_auth("/api/public/auth/forgot-password", &body)
            .await
    }

    /// 提交 6 位验证码并重置密码 — `POST /api/public/auth/reset-password`
    ///
    /// 成功时服务端原子地 `token_version += 1` 并返回全新 JWT，
    /// 客户端应将 `resp.token` 写入 `AuthState`（等价于登录成功）。
    ///
    /// `code` 是邮件中的 6 位数字验证码。失败时统一以 `400` 返回，
    /// **不区分**"验证码错误 / 已过期 / 暴力尝试上限"——防止 enumeration。
    pub async fn reset_password(
        &self,
        email: impl Into<String>,
        code: impl Into<String>,
        new_password: impl Into<String>,
    ) -> Result<ResetPasswordResponse, ClientError> {
        let body = ResetPasswordRequest {
            email: email.into(),
            code: code.into(),
            new_password: new_password.into(),
        };
        self.post_json_no_auth("/api/public/auth/reset-password", &body)
            .await
    }

    /// 刷新 JWT — `POST /api/public/auth/refresh`
    ///
    /// 依赖浏览器自动发送 `ains_refresh` httpOnly cookie，
    /// 不需要手动设置 Authorization 头。成功时返回新 JWT + 新 refresh token。
    pub async fn refresh(&self) -> Result<RefreshResponse, ClientError> {
        self.post_json_no_auth("/api/public/auth/refresh", &serde_json::json!({}))
            .await
    }

    /// WeChat 验证码登录 — `POST /api/public/auth/wx-login`
    ///
    /// 用户从微信公众号获取验证码后，传入 code 进行登录。
    /// 如果 WeChat 账号未绑定任何用户，请先用 email/password 登录并在设置页绑定。
    pub async fn wx_login(&self, code: &str) -> Result<WxLoginResponse, ClientError> {
        let body = WxLoginRequest {
            code: code.to_string(),
        };
        self.post_json_no_auth("/api/public/auth/wx-login", &body)
            .await
    }

    /// WeChat captcha-login 功能开关 — `GET /api/public/auth/wechat-enabled`
    ///
    /// 返回 WeChat 验证码登录功能是否已启用。前端据此决定是否显示 captcha 登录标签。
    pub async fn wechat_enabled(&self) -> Result<WechatEnabledResponse, ClientError> {
        self.get_json_no_auth("/api/public/auth/wechat-enabled")
            .await
    }

    /// 单端登出 — `POST /api/public/auth/logout`
    ///
    /// 服务端读取浏览器携带的 `ains_refresh` httpOnly cookie，从数据库
    /// 删除对应 refresh token 行，并通过 `Set-Cookie` 头清除 JWT/refresh/exp
    /// 三个 cookie。本地 JWT 状态由调用方负责清理（`clear_token`）。
    ///
    /// 端点不需要 JWT 鉴权 —— 仅依赖 refresh cookie 的存在来定位要撤销的
    /// DB 行，因此即使本地 JWT 已过期，前端仍能完成登出。
    pub async fn logout(&self) -> Result<LogoutResponse, ClientError> {
        self.post_json_no_auth("/api/public/auth/logout", &serde_json::json!({}))
            .await
    }

    // ──────────────────────────────────────────
    //  Public endpoints
    // ──────────────────────────────────────────

    /// 健康检查 — `GET /api/health`
    pub async fn health_check(&self) -> Result<HealthResponse, ClientError> {
        self.get_json_no_auth("/api/health").await
    }

    // ──────────────────────────────────────────
    //  Admin: User management
    // ──────────────────────────────────────────

    /// 分页列出用户 — `GET /api/users?page=&per_page=`（需要 admin 角色）
    pub async fn list_users(
        &self,
        page: u64,
        per_page: u64,
    ) -> Result<PaginatedUsersResponse, ClientError> {
        if page == 0 || per_page == 0 {
            return Err(ClientError::Config(
                "page and per_page must be greater than 0".to_string(),
            ));
        }
        let url = self.inner.config.build_url("/api/users");
        let builder = self
            .request_with_auth(Method::GET, &url, None)?
            .query(&[("page", page), ("per_page", per_page)]);
        self.send_and_parse(builder).await
    }

    /// 获取单个用户 — `GET /api/users/{id}`（需要 admin 角色）
    pub async fn get_user(&self, id: String) -> Result<UserResponse, ClientError> {
        self.get_json(&format!("/api/users/{}", id), None).await
    }

    /// 获取当前登录用户资料 — `GET /api/users/me`（任意已认证用户）
    pub async fn get_me(&self) -> Result<UserResponse, ClientError> {
        self.get_json("/api/users/me", None).await
    }

    /// 修改当前用户密码 — `POST /api/users/me/password`（任意已认证用户）
    pub async fn change_password(
        &self,
        current_password: impl Into<String>,
        new_password: impl Into<String>,
    ) -> Result<ChangePasswordResponse, ClientError> {
        let body = ChangePasswordRequest {
            current_password: current_password.into(),
            new_password: new_password.into(),
        };
        self.post_json("/api/users/me/password", &body, None).await
    }

    /// 登出所有设备 — `POST /api/users/me/logout-all`（任意已认证用户）
    ///
    /// 递增 token_version 使所有现有 JWT 失效，删除所有 refresh token，
    /// 并清除 auth cookies。
    pub async fn logout_all(&self) -> Result<serde_json::Value, ClientError> {
        self.post_json("/api/users/me/logout-all", &serde_json::json!({}), None)
            .await
    }

    /// 创建用户 — `POST /api/users`（需要 admin 角色）
    ///
    /// `role` 仅在当前用户为 system 时生效；admin 创建时强制为 "user"。
    /// `tenant_id` 同样仅对 system 生效，用于选择归属租户；admin 由服务端
    /// 强制归入自身租户，忽略该字段。
    pub async fn create_user(
        &self,
        email: impl Into<String>,
        password: impl Into<String>,
        name: impl Into<String>,
        role: Option<String>,
        tenant_id: Option<String>,
    ) -> Result<UserResponse, ClientError> {
        let body = CreateUserRequest {
            email: email.into(),
            password: password.into(),
            name: name.into(),
            role,
            tenant_id,
        };
        self.post_json("/api/users", &body, None).await
    }

    /// 更新用户 — `PUT /api/users/{id}`（需要 admin 角色）
    ///
    /// 仅传入的字段会被更新；`None` 字段保持原值不变。
    /// `tenant_id` 仅对 system 生效（可将用户重新归属到目标租户）；
    /// admin 由服务端强制限定在自身租户，忽略该字段。
    pub async fn update_user(
        &self,
        id: String,
        email: Option<String>,
        name: Option<String>,
        role: Option<String>,
        tenant_id: Option<String>,
    ) -> Result<UserResponse, ClientError> {
        let body = UpdateUserRequest {
            email,
            name,
            role,
            tenant_id,
        };
        self.put_json(&format!("/api/users/{}", id), &body, None)
            .await
    }

    /// 删除用户 — `DELETE /api/users/{id}`（需要 admin 角色）
    pub async fn delete_user(&self, id: String) -> Result<DeleteResponse, ClientError> {
        self.delete_json(&format!("/api/users/{}", id), None).await
    }

    /// 设置用户余额 — `PUT /api/users/{id}/balance`（需要 admin/system 角色）
    pub async fn set_balance(
        &self,
        id: String,
        balance: i64,
    ) -> Result<SetBalanceResponse, ClientError> {
        let body = SetBalanceRequest { balance };
        self.put_json(&format!("/api/users/{}/balance", id), &body, None)
            .await
    }

    /// 调整用户余额（增加/减少）— `POST /api/users/{id}/balance/adjust`（需要 admin/system 角色）
    ///
    /// `amount` 为正数则增加，为负数则减少。
    pub async fn adjust_balance(
        &self,
        id: String,
        amount: i64,
    ) -> Result<AdjustBalanceResponse, ClientError> {
        let body = AdjustBalanceRequest { amount };
        self.post_json(&format!("/api/users/{}/balance/adjust", id), &body, None)
            .await
    }

    // ──────────────────────────────────────────
    //  Admin: Tenant management
    // ──────────────────────────────────────────

    /// 分页列出租户 — `GET /api/tenants?page=...&per_page=...`（需要 system 角色）
    pub async fn list_tenants(
        &self,
        page: u64,
        per_page: u64,
    ) -> Result<TenantListResponse, ClientError> {
        if page == 0 || per_page == 0 {
            return Err(ClientError::Config(
                "page and per_page must be greater than 0".to_string(),
            ));
        }
        let url = self.inner.config.build_url("/api/tenants");
        let builder = self
            .request_with_auth(Method::GET, &url, None)?
            .query(&[("page", page), ("per_page", per_page)]);
        self.send_and_parse(builder).await
    }

    /// 创建租户 — `POST /api/tenants`（需要 system 角色）
    pub async fn create_tenant(
        &self,
        name: impl Into<String>,
    ) -> Result<TenantResponse, ClientError> {
        let body = CreateTenantRequest { name: name.into() };
        self.post_json("/api/tenants", &body, None).await
    }

    /// 更新租户 — `PUT /api/tenants/{id}`（需要 system 角色，admin 仅可更新自身租户）
    pub async fn update_tenant(
        &self,
        id: &str,
        name: Option<String>,
        status: Option<String>,
    ) -> Result<TenantResponse, ClientError> {
        let body = UpdateTenantRequest { name, status };
        self.put_json(&format!("/api/tenants/{}", id), &body, None)
            .await
    }

    /// 删除租户 — `DELETE /api/tenants/{id}`（需要 system 角色）
    ///
    /// 若租户下存在用户或渠道则返回 409 Conflict。
    pub async fn delete_tenant(&self, id: &str) -> Result<DeleteResponse, ClientError> {
        self.delete_json(&format!("/api/tenants/{}", id), None)
            .await
    }

    // ──────────────────────────────────────────
    //  Admin: Channel management
    // ──────────────────────────────────────────

    /// 分页列出渠道 — `GET /api/channels?page=...&per_page=...`（需要 admin/system 角色）
    pub async fn list_channels(
        &self,
        page: u64,
        per_page: u64,
    ) -> Result<ChannelListResponse, ClientError> {
        if page == 0 || per_page == 0 {
            return Err(ClientError::Config(
                "page and per_page must be greater than 0".to_string(),
            ));
        }
        let url = self.inner.config.build_url("/api/channels");
        let builder = self
            .request_with_auth(Method::GET, &url, None)?
            .query(&[("page", page), ("per_page", per_page)]);
        self.send_and_parse(builder).await
    }

    /// 创建渠道 — `POST /api/channels`（需要 admin/system 角色）
    pub async fn create_channel(
        &self,
        input: CreateChannelRequest,
    ) -> Result<ChannelResponse, ClientError> {
        self.post_json("/api/channels", &input, None).await
    }

    /// 更新渠道 — `PUT /api/channels/{id}`（需要 admin/system 角色）
    pub async fn update_channel(
        &self,
        id: &str,
        input: UpdateChannelRequest,
    ) -> Result<ChannelResponse, ClientError> {
        self.put_json(&format!("/api/channels/{}", id), &input, None)
            .await
    }

    /// 禁用渠道 — 通过 `PUT /api/channels/{id} { is_active: false }` 实现（需要 admin/system 角色）
    ///
    /// 此方法仅将渠道标记为 `is_active = false`，不删除记录。
    /// 若要物理删除，请使用 [`Self::delete_channel`]。
    pub async fn disable_channel(&self, id: &str) -> Result<(), ClientError> {
        self.put_json::<serde_json::Value, _>(
            &format!("/api/channels/{}", id),
            &serde_json::json!({"is_active": false}),
            None,
        )
        .await?;
        Ok(())
    }

    /// 物理删除渠道 — `DELETE /api/channels/{id}`（需要 admin/system 角色）
    ///
    /// 若渠道存在关联的 token_usage 记录则会返回 409 Conflict，
    /// 必须先清理历史用量数据后才能删除。
    pub async fn delete_channel(&self, id: &str) -> Result<DeleteResponse, ClientError> {
        self.delete_json(&format!("/api/channels/{}", id), None)
            .await
    }

    // ──────────────────────────────────────────
    //  Admin: Token usage / Metering
    // ──────────────────────────────────────────

    /// 分页查询 token 用量 — `GET /api/usage`（需要 admin/system 角色）
    pub async fn list_usage(
        &self,
        page: u64,
        per_page: u64,
        filter: Option<&ListUsageFilter>,
    ) -> Result<PaginatedUsageResponse, ClientError> {
        let url = self.inner.config.build_url("/api/usage");
        let mut builder = self
            .request_with_auth(Method::GET, &url, None)?
            .query(&[("page", page), ("per_page", per_page)]);
        if let Some(f) = filter {
            builder = Self::apply_usage_filter(builder, f);
        }
        self.send_and_parse(builder).await
    }

    /// 获取 token 用量统计 — `GET /api/usage/stats`（需要 admin/system 角色）
    pub async fn get_usage_stats(
        &self,
        filter: Option<&UsageStatsFilter>,
    ) -> Result<UsageStatsResponse, ClientError> {
        let url = self.inner.config.build_url("/api/usage/stats");
        let mut builder = self.request_with_auth(Method::GET, &url, None)?;
        if let Some(f) = filter {
            builder = Self::apply_usage_filter(builder, f);
        }
        self.send_and_parse(builder).await
    }

    /// Apply optional usage filter parameters to a request builder.
    fn apply_usage_filter(
        mut builder: reqwest::RequestBuilder,
        filter: &ListUsageFilter,
    ) -> reqwest::RequestBuilder {
        if let Some(user_id) = filter.user_id {
            builder = builder.query(&[("user_id", user_id)]);
        }
        if let Some(ref channel_id) = filter.channel_id {
            builder = builder.query(&[("channel_id", channel_id)]);
        }
        if let Some(ref model) = filter.model {
            builder = builder.query(&[("model", model)]);
        }
        if let Some(ref request_type) = filter.request_type {
            builder = builder.query(&[("request_type", request_type)]);
        }
        if let Some(ref date_from) = filter.date_from {
            builder = builder.query(&[("date_from", date_from)]);
        }
        if let Some(ref date_to) = filter.date_to {
            builder = builder.query(&[("date_to", date_to)]);
        }
        builder
    }

    // ──────────────────────────────────────────
    //  Internal HTTP helpers
    // ──────────────────────────────────────────

    /// 构建请求（不带认证头）。用于 login、register、health_check 等公共端点。
    fn request_no_auth(&self, method: Method, url: &str) -> reqwest::RequestBuilder {
        self.inner.client.request(method, url)
    }

    /// 构建带认证头的请求。
    ///
    /// 优先使用传入的 `token` 参数；如果未传入，使用内部存储的 token。
    fn request_with_auth(
        &self,
        method: Method,
        url: &str,
        token: Option<&str>,
    ) -> Result<reqwest::RequestBuilder, ClientError> {
        let mut builder = self.inner.client.request(method, url);

        if let Some(t) = token {
            builder = builder.header("Authorization", format!("Bearer {}", t));
        } else if let Some(t) = self.token() {
            builder = builder.header("Authorization", format!("Bearer {}", t));
        }

        Ok(builder)
    }

    /// GET 请求 + JSON 反序列化（不带认证）
    async fn get_json_no_auth<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let url = self.inner.config.build_url(path);
        let builder = self.request_no_auth(Method::GET, &url);
        self.send_and_parse(builder).await
    }

    /// GET 请求 + JSON 反序列化（带认证）
    async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        token: Option<&str>,
    ) -> Result<T, ClientError> {
        let url = self.inner.config.build_url(path);
        let builder = self.request_with_auth(Method::GET, &url, token)?;
        self.send_and_parse(builder).await
    }

    /// POST 请求 + JSON 序列化/反序列化（不带认证）
    async fn post_json_no_auth<T: DeserializeOwned, B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ClientError> {
        let url = self.inner.config.build_url(path);
        let builder = self.request_no_auth(Method::POST, &url);
        self.send_and_parse(builder.json(body)).await
    }

    /// POST 请求 + JSON 序列化/反序列化（带认证）
    async fn post_json<T: DeserializeOwned, B: Serialize>(
        &self,
        path: &str,
        body: &B,
        token: Option<&str>,
    ) -> Result<T, ClientError> {
        let url = self.inner.config.build_url(path);
        let builder = self.request_with_auth(Method::POST, &url, token)?;
        self.send_and_parse(builder.json(body)).await
    }

    /// PUT 请求 + JSON 序列化/反序列化（带认证）
    async fn put_json<T: DeserializeOwned, B: Serialize>(
        &self,
        path: &str,
        body: &B,
        token: Option<&str>,
    ) -> Result<T, ClientError> {
        let url = self.inner.config.build_url(path);
        let builder = self.request_with_auth(Method::PUT, &url, token)?;
        self.send_and_parse(builder.json(body)).await
    }

    /// DELETE 请求 + JSON 反序列化（带认证）
    async fn delete_json<T: DeserializeOwned>(
        &self,
        path: &str,
        token: Option<&str>,
    ) -> Result<T, ClientError> {
        let url = self.inner.config.build_url(path);
        let builder = self.request_with_auth(Method::DELETE, &url, token)?;
        self.send_and_parse(builder).await
    }

    /// 发送请求并解析 JSON 响应（含重试逻辑）。
    ///
    /// **重试条件**：网络/连接错误、服务器 5xx、限流 429
    ///
    /// **退避策略**：指数退避，初始延迟 500ms，每次翻倍，最大 2s
    ///
    /// 延迟序列（第 i 次重试）：500ms × 2^(i-1)，即 500ms, 1s, 2s（上限）
    async fn send_and_parse<T: DeserializeOwned>(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<T, ClientError> {
        let max_retries = self.inner.config.max_retries;

        // 带 streaming body 的 builder 无法克隆，直接发送不重试
        // 注意：不能加 max_retries > 0 的额外条件——
        // 如果 max_retries = 0 且 builder 不可克隆，会跳过整个发送，变成静默错误。
        if builder.try_clone().is_none() {
            let response = builder.send().await?;
            return Self::handle_response(response).await;
        }

        let mut last_err: Option<ClientError> = None;

        for attempt in 0..=max_retries {
            // 非首次尝试则加入指数退避延迟（包括最后一次重试，
            // 为服务器留出最大恢复窗口）
            if attempt > 0 {
                let delay_ms = 500u64 * (1u64 << (attempt - 1).min(MAX_BACKOFF_SHIFT));
                Self::sleep_ms(delay_ms.min(u32::MAX as u64) as u32).await;
            }

            // 入口守卫（上方 `try_clone().is_none()` 检查）已确保 builder
            // 可克隆才进入循环；
            // try_clone 行为由 body 类型决定且不随迭代变化，此处必然成功。
            let req = builder
                .try_clone()
                .expect("builder known to be clonable (guarded at entry)");

            match req.send().await {
                Ok(response) => match Self::handle_response::<T>(response).await {
                    Ok(result) => return Ok(result),
                    Err(e) if Self::should_retry(&e) && attempt < max_retries => {
                        last_err = Some(e);
                        continue;
                    }
                    Err(e) => return Err(e),
                },
                Err(e) => {
                    let err: ClientError = e.into();
                    if Self::should_retry(&err) && attempt < max_retries {
                        last_err = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            }
        }

        // 理论上不可达：入口守卫确保仅可克隆 builder 才进入循环。
        // 保留此路径作为防御性编程，以防 reqwest 内部行为变更。
        Err(last_err.unwrap_or_else(|| {
            ClientError::Network("Request failed: all retries exhausted".to_string())
        }))
    }

    /// 判断错误是否值得重试。
    fn should_retry(err: &ClientError) -> bool {
        matches!(
            err,
            ClientError::Network(_) | ClientError::ServerError(..) | ClientError::RateLimited(_)
        )
    }

    /// 跨平台异步等待。
    ///
    /// - WASM：使用 `gloo_timers`
    /// - Native：使用 `tokio::time::sleep`
    ///
    /// 公开以便视图层在 WASM/native 双端复用同一份定时器代码
    /// （如注册流程中 60 秒重发倒计时的循环 sleep）。
    pub async fn sleep_ms(ms: u32) {
        #[cfg(target_arch = "wasm32")]
        {
            gloo_timers::future::TimeoutFuture::new(ms).await;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            tokio::time::sleep(Duration::from_millis(ms as u64)).await;
        }
    }

    /// 统一处理 HTTP 响应：成功 → 反序列化 JSON，失败 → 构造对应错误。
    async fn handle_response<T: DeserializeOwned>(
        response: reqwest::Response,
    ) -> Result<T, ClientError> {
        let status = response.status();

        if status.is_success() {
            response.json::<T>().await.map_err(ClientError::from)
        } else {
            let status_code = status.as_u16();
            let text = response.text().await.unwrap_or_default();
            let default_msg = status.canonical_reason().unwrap_or("Unknown error");

            // 传递原始响应体文本，由调用方 (如 humanize_error) 做结构化解析
            // 不在此处格式化 ErrorBody，避免前端错误码匹配死代码。
            // The raw JSON text is passed through so that callers like
            // humanize_error can parse it themselves.
            let message = if text.is_empty() {
                default_msg.to_string()
            } else {
                text
            };

            Err(ClientError::from_status(status_code, message))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_stored() {
        let config = ClientConfig::new("http://127.0.0.1:8080")
            .with_timeout(45)
            .with_max_retries(5);
        let client = Client::new(config.clone()).unwrap();

        assert_eq!(client.config().base_url, "http://127.0.0.1:8080");
        assert_eq!(client.config().timeout_secs, 45);
        assert_eq!(client.config().max_retries, 5);
    }

    #[test]
    fn test_token_management() {
        let client = Client::new(ClientConfig::new("http://127.0.0.1:8080")).unwrap();
        assert!(!client.is_authenticated());
        assert!(client.token().is_none());

        client.set_token("my-jwt-token");
        assert!(client.is_authenticated());
        assert_eq!(client.token(), Some("my-jwt-token".to_string()));

        client.clear_token();
        assert!(!client.is_authenticated());
    }

    #[test]
    fn test_token_shared_across_clones() {
        let client = Client::new(ClientConfig::new("http://127.0.0.1:8080")).unwrap();
        let cloned = client.clone();

        client.set_token("shared-token");
        assert_eq!(cloned.token(), Some("shared-token".to_string()));

        cloned.clear_token();
        assert!(client.token().is_none());
    }

    #[test]
    fn test_config_validation_http() {
        let config = ClientConfig::new("http://127.0.0.1:8080");
        assert!(Client::new(config).is_ok());
    }

    #[test]
    fn test_config_validation_https() {
        let config = ClientConfig::new("https://api.example.com");
        assert!(Client::new(config).is_ok());
    }

    // 原生平台上空 URL 不合法；WASM 下空 URL 通过 window.location 推导 origin，
    // 因此本测试仅在非 WASM 平台执行。
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn test_config_validation_empty_url() {
        let config = ClientConfig::new("");
        assert!(Client::new(config).is_err());
    }

    #[test]
    fn test_config_validation_rejects_ftp() {
        let config = ClientConfig::new("ftp://127.0.0.1");
        assert!(Client::new(config).is_err());
    }

    #[test]
    fn test_config_validation_rejects_zero_timeout() {
        let config = ClientConfig::new("http://127.0.0.1:8080").with_timeout(0);
        assert!(Client::new(config).is_err());
    }

    #[test]
    fn test_should_retry_network_error() {
        assert!(Client::should_retry(&ClientError::Network(
            "timeout".into()
        )));
    }

    #[test]
    fn test_should_retry_server_error() {
        assert!(Client::should_retry(&ClientError::ServerError(
            500,
            "Internal Server Error".into()
        )));
    }

    #[test]
    fn test_should_retry_rate_limited() {
        assert!(Client::should_retry(&ClientError::RateLimited(
            "Too many requests".into()
        )));
    }

    #[test]
    fn test_should_not_retry_client_error() {
        assert!(!Client::should_retry(&ClientError::Other(
            404,
            "Not found".into()
        )));
    }

    #[test]
    fn test_should_not_retry_config_error() {
        assert!(!Client::should_retry(&ClientError::Config("bad".into())));
    }
}
