//! 客户端 API 工厂与 401 拦截辅助。
//!
//! - `make_client()` 根据编译目标返回合适的 `client_api::Client`。
//! - `is_unauth(err)` 判定一个 `ClientError` 是否表示 token 失效。
//! - `handle_unauth(err, auth, nav, log_bus)` 检测到 401 时执行 logout + 跳转 `/auth`
//!   并写入 `LogKind::Important` 日志，与 `TokenExpiryGuard::fire_expiry` 行为对齐。
//! - `humanize_error(err, ctx, lang)` 将 API 错误翻译为当前语言提示。

mod client;

pub use client::make_client;

use client_api::ClientError;
use dioxus::prelude::dioxus_router::Navigator;
use i18n::Language;

use crate::Route;
use crate::auth::AuthState;
use crate::components::{HttpMethod, LogBus, LogKind};

/// 错误翻译上下文 —— 不同视图对同一 HTTP 状态码可能有不同文案。
pub enum ErrorContext {
    /// 登录 / 注册页面：401 → "邮箱或密码错误"
    Auth,
    /// 用户管理页面：401 → "未登录或会话已过期"，额外支持 403 / 404
    UserManagement,
    /// 邮件验证页面：统一 400 文案（与服务端 anti-enumeration 对齐——
    /// 不区分"用户不存在"、"码错误"、"码过期"、"超过尝试上限"）
    EmailVerification,
    /// 密码重置页面：与 verify-email 类似，服务端对所有失败分支统一
    /// 400 + 通用文案以防 enumeration / 凭证探测。
    PasswordReset,
    /// 租户管理页面
    TenantManagement,
    /// 渠道管理页面
    ChannelManagement,
    /// 用量统计页面
    Metering,
    /// 套餐管理页面（含个人中心购买场景的 no_active_plan / insufficient_balance）
    PlanManagement,
    /// 支付订单页面
    OrderManagement,
    /// 个人中心（自助接口）：403 的唯一来源是租户被禁用
    /// （require_actor_tenant_active），而非权限不足 —— 不可复用
    /// PlanManagement 的“需 admin”文案；404 覆盖购买 disabled/跨租户
    /// 套餐时的 NotFound。
    PersonalCenter,
}

/// 购买链路专属错误码 → 文案（PlanManagement 与 PersonalCenter 两语境
/// 共享，防止双处维护导致文案静默漂移）。
///
/// 仅覆盖三个购买专属错误码；调用方需先排除 401（会话失效语义
/// 优先）。validation_error 不在此处理 —— 两语境均约定 403/404
/// 状态码优先于 validation_error 错误码。
fn purchase_code_message(code: &str, lang: Language) -> Option<String> {
    let msg = match (lang, code) {
        (Language::En, "no_active_plan") => "No active plan with remaining calls",
        (Language::En, "insufficient_balance") => "Insufficient balance to purchase this plan",
        (Language::En, "purchase_in_progress") => {
            "A purchase is already in progress, please try again shortly"
        }
        (Language::Zh, "no_active_plan") => "没有可用套餐或套餐次数已用尽",
        (Language::Zh, "insufficient_balance") => "余额不足，无法购买该套餐",
        (Language::Zh, "purchase_in_progress") => "已有一笔购买正在处理中，请稍后再试",
        _ => return None,
    };
    Some(msg.to_string())
}

/// 将 `ClientError` 翻译为当前语言提示，根据 `ctx` 差异化状态码文案。
pub fn humanize_error(err: &ClientError, ctx: ErrorContext, lang: Language) -> String {
    match err {
        ClientError::Network(msg) => match lang {
            Language::En => format!("Network error: {msg}"),
            Language::Zh => format!("网络异常: {msg}"),
        },
        ClientError::ServerError(status, body) => match lang {
            Language::En => format!("Server error (HTTP {status}): {body}"),
            Language::Zh => format!("服务器错误 (HTTP {status}): {body}"),
        },
        ClientError::Other(status, body) => {
            let json = serde_json::from_str::<serde_json::Value>(body).ok();
            let code = json
                .as_ref()
                .and_then(|v| v.get("error").and_then(|c| c.as_str().map(String::from)))
                .unwrap_or_default();
            let msg = json
                .as_ref()
                .and_then(|v| v.get("message").and_then(|m| m.as_str().map(String::from)))
                .unwrap_or_else(|| body.clone());
            // 购买链路共享错误码先于各语境 match（但让位 401）：
            // no_active_plan 服务端以 403 返回，必须在进入 (403, _)
            // 分支前命中专属文案，不得被“需 admin”/“租户禁用”遮蔽。
            if matches!(
                ctx,
                ErrorContext::PlanManagement | ErrorContext::PersonalCenter
            ) && *status != 401
                && let Some(shared) = purchase_code_message(code.as_str(), lang)
            {
                return shared;
            }
            match lang {
                Language::En => match ctx {
                    ErrorContext::Auth => match (status, code.as_str()) {
                        (401, _) => "Invalid email or password".to_string(),
                        (_, "validation_error") => format!("Validation error: {msg}"),
                        (_, "conflict") => "Email already registered".to_string(),
                        _ => format!("Request failed (HTTP {status}): {msg}"),
                    },
                    ErrorContext::UserManagement => match (status, code.as_str()) {
                        (401, _) => "Not logged in or session expired".to_string(),
                        (403, _) => "Insufficient permissions (admin required)".to_string(),
                        (404, _) => "User not found".to_string(),
                        (_, "insufficient_balance") => "Amount exceeds current balance".to_string(),
                        (_, "validation_error") => format!("Validation error: {msg}"),
                        (_, "conflict") => {
                            "Operation conflict (email already exists or constraint violation)"
                                .to_string()
                        }
                        _ => format!("Request failed (HTTP {status}): {msg}"),
                    },
                    ErrorContext::EmailVerification => match (status, code.as_str()) {
                        (400, _) => "Invalid or expired verification code".to_string(),
                        (503, _) => "Email service not configured".to_string(),
                        _ => format!("Request failed (HTTP {status}): {msg}"),
                    },
                    ErrorContext::PasswordReset => match (status, code.as_str()) {
                        (400, _) => "Invalid or expired reset code".to_string(),
                        (503, _) => "Password reset is currently unavailable".to_string(),
                        _ => format!("Request failed (HTTP {status}): {msg}"),
                    },
                    ErrorContext::TenantManagement => match (status, code.as_str()) {
                        (401, _) => "Not logged in or session expired".to_string(),
                        (403, _) => "Insufficient permissions (system role required)".to_string(),
                        (404, _) => "Tenant not found".to_string(),
                        (409, _) => msg,
                        (_, "validation_error") => format!("Validation error: {msg}"),
                        _ => format!("Request failed (HTTP {status}): {msg}"),
                    },
                    ErrorContext::ChannelManagement => match (status, code.as_str()) {
                        (401, _) => "Not logged in or session expired".to_string(),
                        (403, _) => "Insufficient permissions (admin required)".to_string(),
                        (404, _) => "Channel not found".to_string(),
                        (_, "validation_error") => format!("Validation error: {msg}"),
                        _ => format!("Request failed (HTTP {status}): {msg}"),
                    },
                    ErrorContext::Metering => match (status, code.as_str()) {
                        (401, _) => "Not logged in or session expired".to_string(),
                        (403, _) => "Insufficient permissions (admin required)".to_string(),
                        _ => format!("Request failed (HTTP {status}): {msg}"),
                    },
                    // 购买专属错误码已由上方 purchase_code_message 命中。
                    ErrorContext::PlanManagement => match (status, code.as_str()) {
                        (401, _) => "Not logged in or session expired".to_string(),
                        (403, _) => "Insufficient permissions (admin required)".to_string(),
                        (404, _) => "Plan or user not found".to_string(),
                        (_, "validation_error") => format!("Validation error: {msg}"),
                        _ => format!("Request failed (HTTP {status}): {msg}"),
                    },
                    ErrorContext::OrderManagement => match (status, code.as_str()) {
                        (401, _) => "Not logged in or session expired".to_string(),
                        (403, _) => "Insufficient permissions (admin required)".to_string(),
                        (404, _) => "Order not found".to_string(),
                        (_, "validation_error") => format!("Validation error: {msg}"),
                        _ => format!("Request failed (HTTP {status}): {msg}"),
                    },
                    // 购买专属错误码（含防御性的 no_active_plan 映射，理由
                    // 见 purchase_code_message）已由上方共享 helper 命中。
                    ErrorContext::PersonalCenter => match (status, code.as_str()) {
                        (401, _) => "Not logged in or session expired".to_string(),
                        (403, _) => {
                            "Your tenant is disabled; purchasing is unavailable".to_string()
                        }
                        (404, _) => "Plan not found or no longer available".to_string(),
                        (_, "validation_error") => format!("Validation error: {msg}"),
                        _ => format!("Request failed (HTTP {status}): {msg}"),
                    },
                },
                Language::Zh => match ctx {
                    ErrorContext::Auth => match (status, code.as_str()) {
                        (401, _) => "邮箱或密码错误".to_string(),
                        (_, "validation_error") => format!("参数错误: {msg}"),
                        (_, "conflict") => "该邮箱已注册".to_string(),
                        _ => format!("请求失败 (HTTP {status}): {msg}"),
                    },
                    ErrorContext::UserManagement => match (status, code.as_str()) {
                        (401, _) => "未登录或会话已过期".to_string(),
                        (403, _) => "权限不足 (需 admin)".to_string(),
                        (404, _) => "用户不存在".to_string(),
                        (_, "insufficient_balance") => "减少金额超过当前余额".to_string(),
                        (_, "validation_error") => format!("参数错误: {msg}"),
                        (_, "conflict") => "操作冲突（邮箱已存在或违反约束）".to_string(),
                        _ => format!("请求失败 (HTTP {status}): {msg}"),
                    },
                    ErrorContext::EmailVerification => match (status, code.as_str()) {
                        (400, _) => "验证码错误或已过期".to_string(),
                        (503, _) => "邮件服务未配置".to_string(),
                        _ => format!("请求失败 (HTTP {status}): {msg}"),
                    },
                    ErrorContext::PasswordReset => match (status, code.as_str()) {
                        (400, _) => "重置验证码无效或已过期".to_string(),
                        (503, _) => "密码重置功能暂不可用".to_string(),
                        _ => format!("请求失败 (HTTP {status}): {msg}"),
                    },
                    ErrorContext::TenantManagement => match (status, code.as_str()) {
                        (401, _) => "未登录或会话已过期".to_string(),
                        (403, _) => "权限不足 (需 system 角色)".to_string(),
                        (404, _) => "租户不存在".to_string(),
                        (409, _) => msg,
                        (_, "validation_error") => format!("参数错误: {msg}"),
                        _ => format!("请求失败 (HTTP {status}): {msg}"),
                    },
                    ErrorContext::ChannelManagement => match (status, code.as_str()) {
                        (401, _) => "未登录或会话已过期".to_string(),
                        (403, _) => "权限不足 (需 admin)".to_string(),
                        (404, _) => "渠道不存在".to_string(),
                        (_, "validation_error") => format!("参数错误: {msg}"),
                        _ => format!("请求失败 (HTTP {status}): {msg}"),
                    },
                    ErrorContext::Metering => match (status, code.as_str()) {
                        (401, _) => "未登录或会话已过期".to_string(),
                        (403, _) => "权限不足 (需 admin)".to_string(),
                        _ => format!("请求失败 (HTTP {status}): {msg}"),
                    },
                    // 购买专属错误码已由上方 purchase_code_message 命中。
                    ErrorContext::PlanManagement => match (status, code.as_str()) {
                        (401, _) => "未登录或会话已过期".to_string(),
                        (403, _) => "权限不足 (需 admin)".to_string(),
                        (404, _) => "套餐或用户不存在".to_string(),
                        (_, "validation_error") => format!("参数错误: {msg}"),
                        _ => format!("请求失败 (HTTP {status}): {msg}"),
                    },
                    ErrorContext::OrderManagement => match (status, code.as_str()) {
                        (401, _) => "未登录或会话已过期".to_string(),
                        (403, _) => "权限不足 (需 admin)".to_string(),
                        (404, _) => "订单不存在".to_string(),
                        (_, "validation_error") => format!("参数错误: {msg}"),
                        _ => format!("请求失败 (HTTP {status}): {msg}"),
                    },
                    // 购买专属错误码已由上方共享 helper 命中。
                    ErrorContext::PersonalCenter => match (status, code.as_str()) {
                        (401, _) => "未登录或会话已过期".to_string(),
                        (403, _) => "当前租户已禁用，暂不可购买".to_string(),
                        (404, _) => "套餐不存在或已下架".to_string(),
                        (_, "validation_error") => format!("参数错误: {msg}"),
                        _ => format!("请求失败 (HTTP {status}): {msg}"),
                    },
                },
            }
        }
        ClientError::RateLimited(_) => match lang {
            Language::En => "Too many requests, please try again later".to_string(),
            Language::Zh => "请求过于频繁，请稍后再试".to_string(),
        },
        ClientError::Deserialization(msg) => match lang {
            Language::En => format!("Response parse failed: {msg}"),
            Language::Zh => format!("响应解析失败: {msg}"),
        },
        ClientError::Config(msg) => match lang {
            Language::En => format!("Client configuration error: {msg}"),
            Language::Zh => format!("客户端配置错误: {msg}"),
        },
        _ => match lang {
            Language::En => format!("Unknown error: {err}"),
            Language::Zh => format!("未知错误: {err}"),
        },
    }
}

/// 判定一个 `ClientError` 是否代表 token 失效（HTTP 401）。
pub fn is_unauth(err: &ClientError) -> bool {
    matches!(err, ClientError::Other(401, _))
}

/// 若 `err` 为 401 或 403，执行相应导航并返回 `true`：
/// - 401：调用 `auth.logout_async()` 撤销后端 refresh token，再跳 `/auth`；
/// - 403：仅跳 `/`（已认证但权限不足，如 JWT role 被篡改）。
///
/// 否则返回 `false`，让调用方继续处理业务错误。
///
/// 视图层模式：
/// ```ignore
/// if let Err(e) = client.list_users(1, 20).await {
///     if handle_unauth(&e, auth, nav, log_bus).await { return; }
///     // 处理业务错误...
/// }
/// ```
pub async fn handle_unauth(
    err: &ClientError,
    mut auth: AuthState,
    nav: Navigator,
    mut log_bus: LogBus,
) -> bool {
    if is_unauth(err) {
        log_bus.push(
            HttpMethod::Post,
            "/auth/logout (session expired via 401)".to_string(),
            "401".to_string(),
            LogKind::Important,
        );
        // 401 意味着 JWT 已被服务端拒绝 —— 通过 logout 端点同步撤销
        // refresh token，避免 refresh cookie 仍可用来换发新 JWT 的悬空会话。
        auth.logout_async().await;
        nav.replace(Route::LoginLanding {});
        true
    } else if matches!(err, ClientError::Other(403, _)) {
        log_bus.push(
            HttpMethod::Post,
            "/auth/forbidden (JWT admin scope mismatch)".to_string(),
            "403".to_string(),
            LogKind::Important,
        );
        nav.replace(Route::PersonalCenter {});
        true
    } else {
        false
    }
}

/// [`handle_unauth`] 的 401-only 变体：仅拦截 token 失效（登出 + 跳
/// `/auth`），403 一律放行给调用方。
///
/// 适用于把 403 作为**预期业务状态**的视图 —— 个人中心的自助接口
/// 在租户被禁用时返回 403（require_actor_tenant_active），必须落入
/// `humanize_error` 渲染为业务文案；若走 [`handle_unauth`]，403 会被
/// 当作权限异常跳转回个人中心自身后吞掉，导致区块永久 loading、
/// 购买弹窗无反馈卡死。
pub async fn handle_unauth_401_only(
    err: &ClientError,
    auth: AuthState,
    nav: Navigator,
    log_bus: LogBus,
) -> bool {
    if !is_unauth(err) {
        return false;
    }
    handle_unauth(err, auth, nav, log_bus).await
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_unauth ────────────────────────────────────────

    #[test]
    fn is_unauth_401_returns_true() {
        let err = ClientError::Other(401, "unauthorized".into());
        assert!(is_unauth(&err));
    }

    #[test]
    fn is_unauth_403_returns_false() {
        let err = ClientError::Other(403, "forbidden".into());
        assert!(!is_unauth(&err));
    }

    #[test]
    fn is_unauth_400_returns_false() {
        let err = ClientError::Other(400, "bad request".into());
        assert!(!is_unauth(&err));
    }

    #[test]
    fn is_unauth_network_returns_false() {
        let err = ClientError::Network("timeout".into());
        assert!(!is_unauth(&err));
    }

    #[test]
    fn is_unauth_server_error_401_returns_false() {
        let err = ClientError::ServerError(401, "Unauthorized".into());
        assert!(!is_unauth(&err));
    }

    // ── humanize_error: Auth context ─────────────────────

    #[test]
    fn humanize_auth_401_zh() {
        let err = ClientError::Other(401, r#"{"error":"unauthorized"}"#.into());
        let msg = humanize_error(&err, ErrorContext::Auth, Language::Zh);
        assert_eq!(msg, "邮箱或密码错误");
    }

    #[test]
    fn humanize_auth_401_en() {
        let err = ClientError::Other(401, r#"{"error":"unauthorized"}"#.into());
        let msg = humanize_error(&err, ErrorContext::Auth, Language::En);
        assert_eq!(msg, "Invalid email or password");
    }

    #[test]
    fn humanize_auth_validation_error_zh() {
        let err = ClientError::Other(
            400,
            r#"{"error":"validation_error","message":"email is invalid"}"#.into(),
        );
        let msg = humanize_error(&err, ErrorContext::Auth, Language::Zh);
        assert!(msg.contains("参数错误"));
    }

    #[test]
    fn humanize_auth_validation_error_en() {
        let err = ClientError::Other(
            400,
            r#"{"error":"validation_error","message":"email is invalid"}"#.into(),
        );
        let msg = humanize_error(&err, ErrorContext::Auth, Language::En);
        assert!(msg.contains("Validation error"));
    }

    #[test]
    fn humanize_auth_conflict_zh() {
        let err = ClientError::Other(
            409,
            r#"{"error":"conflict","message":"email already registered"}"#.into(),
        );
        let msg = humanize_error(&err, ErrorContext::Auth, Language::Zh);
        assert_eq!(msg, "该邮箱已注册");
    }

    #[test]
    fn humanize_auth_conflict_en() {
        let err = ClientError::Other(
            409,
            r#"{"error":"conflict","message":"email already registered"}"#.into(),
        );
        let msg = humanize_error(&err, ErrorContext::Auth, Language::En);
        assert_eq!(msg, "Email already registered");
    }

    #[test]
    fn humanize_auth_network_zh() {
        let err = ClientError::Network("connection refused".into());
        let msg = humanize_error(&err, ErrorContext::Auth, Language::Zh);
        assert!(msg.contains("网络异常"));
    }

    #[test]
    fn humanize_auth_network_en() {
        let err = ClientError::Network("connection refused".into());
        let msg = humanize_error(&err, ErrorContext::Auth, Language::En);
        assert!(msg.contains("Network error"));
    }

    // ── humanize_error: UserManagement context ───────────

    #[test]
    fn humanize_usermgmt_401_zh() {
        let err = ClientError::Other(401, r#"{"error":"unauthorized"}"#.into());
        let msg = humanize_error(&err, ErrorContext::UserManagement, Language::Zh);
        assert_eq!(msg, "未登录或会话已过期");
    }

    #[test]
    fn humanize_usermgmt_401_en() {
        let err = ClientError::Other(401, r#"{"error":"unauthorized"}"#.into());
        let msg = humanize_error(&err, ErrorContext::UserManagement, Language::En);
        assert_eq!(msg, "Not logged in or session expired");
    }

    #[test]
    fn humanize_usermgmt_403_zh() {
        let err = ClientError::Other(403, r#"{"error":"forbidden"}"#.into());
        let msg = humanize_error(&err, ErrorContext::UserManagement, Language::Zh);
        assert_eq!(msg, "权限不足 (需 admin)");
    }

    #[test]
    fn humanize_usermgmt_403_en() {
        let err = ClientError::Other(403, r#"{"error":"forbidden"}"#.into());
        let msg = humanize_error(&err, ErrorContext::UserManagement, Language::En);
        assert_eq!(msg, "Insufficient permissions (admin required)");
    }

    #[test]
    fn humanize_usermgmt_404_zh() {
        let err = ClientError::Other(404, r#"{"error":"not_found"}"#.into());
        let msg = humanize_error(&err, ErrorContext::UserManagement, Language::Zh);
        assert_eq!(msg, "用户不存在");
    }

    #[test]
    fn humanize_usermgmt_404_en() {
        let err = ClientError::Other(404, r#"{"error":"not_found"}"#.into());
        let msg = humanize_error(&err, ErrorContext::UserManagement, Language::En);
        assert_eq!(msg, "User not found");
    }

    #[test]
    fn humanize_usermgmt_insufficient_balance_zh() {
        let err = ClientError::Other(400, r#"{"error":"insufficient_balance"}"#.into());
        let msg = humanize_error(&err, ErrorContext::UserManagement, Language::Zh);
        assert_eq!(msg, "减少金额超过当前余额");
    }

    #[test]
    fn humanize_usermgmt_insufficient_balance_en() {
        let err = ClientError::Other(400, r#"{"error":"insufficient_balance"}"#.into());
        let msg = humanize_error(&err, ErrorContext::UserManagement, Language::En);
        assert_eq!(msg, "Amount exceeds current balance");
    }

    #[test]
    fn humanize_plan_purchase_in_progress_zh() {
        let err = ClientError::Other(409, r#"{"error":"purchase_in_progress"}"#.into());
        let msg = humanize_error(&err, ErrorContext::PlanManagement, Language::Zh);
        assert_eq!(msg, "已有一笔购买正在处理中，请稍后再试");
    }

    #[test]
    fn humanize_plan_purchase_in_progress_en() {
        let err = ClientError::Other(409, r#"{"error":"purchase_in_progress"}"#.into());
        let msg = humanize_error(&err, ErrorContext::PlanManagement, Language::En);
        assert_eq!(
            msg,
            "A purchase is already in progress, please try again shortly"
        );
    }

    #[test]
    fn humanize_personal_center_403_zh() {
        // 自助接口的 403 来自租户禁用，不得渲染为“需 admin”。
        let err = ClientError::Other(403, r#"{"error":"forbidden"}"#.into());
        let msg = humanize_error(&err, ErrorContext::PersonalCenter, Language::Zh);
        assert_eq!(msg, "当前租户已禁用，暂不可购买");
    }

    #[test]
    fn humanize_personal_center_403_en() {
        let err = ClientError::Other(403, r#"{"error":"forbidden"}"#.into());
        let msg = humanize_error(&err, ErrorContext::PersonalCenter, Language::En);
        assert_eq!(msg, "Your tenant is disabled; purchasing is unavailable");
    }

    #[test]
    fn humanize_personal_center_404_zh() {
        let err = ClientError::Other(404, r#"{"error":"not_found"}"#.into());
        let msg = humanize_error(&err, ErrorContext::PersonalCenter, Language::Zh);
        assert_eq!(msg, "套餐不存在或已下架");
    }

    #[test]
    fn humanize_personal_center_404_en() {
        let err = ClientError::Other(404, r#"{"error":"not_found"}"#.into());
        let msg = humanize_error(&err, ErrorContext::PersonalCenter, Language::En);
        assert_eq!(msg, "Plan not found or no longer available");
    }

    #[test]
    fn humanize_personal_center_purchase_in_progress_zh() {
        let err = ClientError::Other(409, r#"{"error":"purchase_in_progress"}"#.into());
        let msg = humanize_error(&err, ErrorContext::PersonalCenter, Language::Zh);
        assert_eq!(msg, "已有一笔购买正在处理中，请稍后再试");
    }

    #[test]
    fn humanize_personal_center_purchase_in_progress_en() {
        let err = ClientError::Other(409, r#"{"error":"purchase_in_progress"}"#.into());
        let msg = humanize_error(&err, ErrorContext::PersonalCenter, Language::En);
        assert_eq!(
            msg,
            "A purchase is already in progress, please try again shortly"
        );
    }

    #[test]
    fn humanize_personal_center_401_zh() {
        let err = ClientError::Other(401, r#"{"error":"unauthorized"}"#.into());
        let msg = humanize_error(&err, ErrorContext::PersonalCenter, Language::Zh);
        assert_eq!(msg, "未登录或会话已过期");
    }

    #[test]
    fn humanize_personal_center_401_en() {
        let err = ClientError::Other(401, r#"{"error":"unauthorized"}"#.into());
        let msg = humanize_error(&err, ErrorContext::PersonalCenter, Language::En);
        assert_eq!(msg, "Not logged in or session expired");
    }

    #[test]
    fn humanize_personal_center_insufficient_balance_zh() {
        // 服务端以 400 + insufficient_balance 返回（utils/error.rs）。
        let err = ClientError::Other(400, r#"{"error":"insufficient_balance"}"#.into());
        let msg = humanize_error(&err, ErrorContext::PersonalCenter, Language::Zh);
        assert_eq!(msg, "余额不足，无法购买该套餐");
    }

    #[test]
    fn humanize_personal_center_insufficient_balance_en() {
        let err = ClientError::Other(400, r#"{"error":"insufficient_balance"}"#.into());
        let msg = humanize_error(&err, ErrorContext::PersonalCenter, Language::En);
        assert_eq!(msg, "Insufficient balance to purchase this plan");
    }

    #[test]
    fn humanize_personal_center_no_active_plan_not_shadowed_by_403_zh() {
        // no_active_plan 服务端以 403 返回（handlers/plan.rs）：必须命中
        // 专属文案，不得被通用 (403, _) 的“租户禁用”分支遮蔽。
        let err = ClientError::Other(403, r#"{"error":"no_active_plan"}"#.into());
        let msg = humanize_error(&err, ErrorContext::PersonalCenter, Language::Zh);
        assert_eq!(msg, "没有可用套餐或套餐次数已用尽");
    }

    #[test]
    fn humanize_personal_center_no_active_plan_not_shadowed_by_403_en() {
        let err = ClientError::Other(403, r#"{"error":"no_active_plan"}"#.into());
        let msg = humanize_error(&err, ErrorContext::PersonalCenter, Language::En);
        assert_eq!(msg, "No active plan with remaining calls");
    }

    #[test]
    fn purchase_code_messages_identical_across_contexts() {
        // 防漂移守卫：三个购买专属错误码在 PlanManagement 与
        // PersonalCenter 两语境下必须产生完全一致的文案（共享
        // purchase_code_message），不随状态码变化（401 除外）。
        for code in ["no_active_plan", "insufficient_balance", "purchase_in_progress"] {
            for status in [400u16, 403, 409] {
                let err = ClientError::Other(status, format!(r#"{{"error":"{code}"}}"#));
                for lang in [Language::Zh, Language::En] {
                    let plan_mgmt = humanize_error(&err, ErrorContext::PlanManagement, lang);
                    let personal = humanize_error(&err, ErrorContext::PersonalCenter, lang);
                    assert_eq!(
                        plan_mgmt, personal,
                        "context drift for code={code} status={status} lang={lang:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn purchase_code_yields_to_401_session_expiry() {
        // 401 语义优先：即使响应体携带购买错误码，会话失效文案
        // 仍胜出（共享 helper 不得拦截 401）。
        let err = ClientError::Other(401, r#"{"error":"insufficient_balance"}"#.into());
        assert_eq!(
            humanize_error(&err, ErrorContext::PersonalCenter, Language::Zh),
            "未登录或会话已过期"
        );
        assert_eq!(
            humanize_error(&err, ErrorContext::PlanManagement, Language::En),
            "Not logged in or session expired"
        );
    }

    #[test]
    fn humanize_personal_center_validation_error_zh() {
        let err = ClientError::Other(
            422,
            r#"{"error":"validation_error","message":"plan_id is invalid"}"#.into(),
        );
        let msg = humanize_error(&err, ErrorContext::PersonalCenter, Language::Zh);
        assert_eq!(msg, "参数错误: plan_id is invalid");
    }

    #[test]
    fn humanize_personal_center_validation_error_en() {
        let err = ClientError::Other(
            422,
            r#"{"error":"validation_error","message":"plan_id is invalid"}"#.into(),
        );
        let msg = humanize_error(&err, ErrorContext::PersonalCenter, Language::En);
        assert_eq!(msg, "Validation error: plan_id is invalid");
    }

    #[test]
    fn humanize_personal_center_fallback_exposes_only_parsed_message() {
        // 兼具兜底臂覆盖与不泄漏验证：未知错误码落入通用文案，
        // 且仅透出服务端显式提供的 message 字段 —— body 中的其他
        // 内部字段（如 detail）不得出现在用户可见文案里。
        // 用 409 + 未知错误码构造：5xx 被 client-api 映射为
        // ClientError::ServerError（from_status），Other(5xx, _) 实际
        // 不会出现；兜底臂的真实来源是携未知错误码的 4xx。
        let err = ClientError::Other(
            409,
            r#"{"error":"conflict","message":"Plan operation failed","detail":"db timeout at pool.rs:42"}"#.into(),
        );
        let msg = humanize_error(&err, ErrorContext::PersonalCenter, Language::Zh);
        assert_eq!(msg, "请求失败 (HTTP 409): Plan operation failed");
        assert!(!msg.contains("db timeout"), "internal detail leaked: {msg}");

        let msg_en = humanize_error(&err, ErrorContext::PersonalCenter, Language::En);
        assert_eq!(msg_en, "Request failed (HTTP 409): Plan operation failed");
        assert!(
            !msg_en.contains("pool.rs"),
            "internal detail leaked: {msg_en}"
        );
    }

    #[test]
    fn humanize_personal_center_404_wins_over_validation_error_code() {
        // 文档性测试：固定 (404, _) 臂优先于 (_, "validation_error")
        // 的匹配顺序。服务端目前不会产生 404 + validation_error
        // 的组合，若未来出现，状态码语义（套餐不存在）优先展示。
        let err = ClientError::Other(
            404,
            r#"{"error":"validation_error","message":"plan_id is invalid"}"#.into(),
        );
        let msg = humanize_error(&err, ErrorContext::PersonalCenter, Language::Zh);
        assert_eq!(msg, "套餐不存在或已下架");
        let msg_en = humanize_error(&err, ErrorContext::PersonalCenter, Language::En);
        assert_eq!(msg_en, "Plan not found or no longer available");
    }

    #[test]
    fn humanize_usermgmt_server_error_zh() {
        let err = ClientError::ServerError(500, "Internal Server Error".into());
        let msg = humanize_error(&err, ErrorContext::UserManagement, Language::Zh);
        assert!(msg.contains("服务器错误"));
    }

    #[test]
    fn humanize_usermgmt_server_error_en() {
        let err = ClientError::ServerError(500, "Internal Server Error".into());
        let msg = humanize_error(&err, ErrorContext::UserManagement, Language::En);
        assert!(msg.contains("Server error"));
    }

    #[test]
    fn humanize_rate_limited_zh() {
        let err = ClientError::RateLimited("Too many requests".into());
        let msg = humanize_error(&err, ErrorContext::Auth, Language::Zh);
        assert_eq!(msg, "请求过于频繁，请稍后再试");
    }

    #[test]
    fn humanize_rate_limited_en() {
        let err = ClientError::RateLimited("Too many requests".into());
        let msg = humanize_error(&err, ErrorContext::Auth, Language::En);
        assert_eq!(msg, "Too many requests, please try again later");
    }

    #[test]
    fn humanize_deserialization_zh() {
        let err = ClientError::Deserialization("expected `{`".into());
        let msg = humanize_error(&err, ErrorContext::Auth, Language::Zh);
        assert!(msg.contains("响应解析失败"));
    }

    #[test]
    fn humanize_deserialization_en() {
        let err = ClientError::Deserialization("expected `{`".into());
        let msg = humanize_error(&err, ErrorContext::Auth, Language::En);
        assert!(msg.contains("Response parse failed"));
    }

    #[test]
    fn humanize_config_zh() {
        let err = ClientError::Config("bad base url".into());
        let msg = humanize_error(&err, ErrorContext::Auth, Language::Zh);
        assert!(msg.contains("客户端配置错误"));
    }

    #[test]
    fn humanize_config_en() {
        let err = ClientError::Config("bad base url".into());
        let msg = humanize_error(&err, ErrorContext::Auth, Language::En);
        assert!(msg.contains("Client configuration error"));
    }

    // ── humanize_error: EmailVerification context ────────

    #[test]
    fn humanize_verify_400_zh() {
        let err = ClientError::Other(400, r#"{"error":"invalid_code"}"#.into());
        let msg = humanize_error(&err, ErrorContext::EmailVerification, Language::Zh);
        assert_eq!(msg, "验证码错误或已过期");
    }

    #[test]
    fn humanize_verify_400_en() {
        let err = ClientError::Other(400, r#"{"error":"invalid_code"}"#.into());
        let msg = humanize_error(&err, ErrorContext::EmailVerification, Language::En);
        assert_eq!(msg, "Invalid or expired verification code");
    }

    #[test]
    fn humanize_verify_503_zh() {
        let err = ClientError::Other(503, r#"{"error":"mail_unconfigured"}"#.into());
        let msg = humanize_error(&err, ErrorContext::EmailVerification, Language::Zh);
        assert_eq!(msg, "邮件服务未配置");
    }

    #[test]
    fn humanize_verify_503_en() {
        let err = ClientError::Other(503, r#"{"error":"mail_unconfigured"}"#.into());
        let msg = humanize_error(&err, ErrorContext::EmailVerification, Language::En);
        assert_eq!(msg, "Email service not configured");
    }

    // ── humanize_error: PasswordReset context ────────────

    #[test]
    fn humanize_reset_400_zh() {
        let err = ClientError::Other(400, r#"{"error":"invalid_token"}"#.into());
        let msg = humanize_error(&err, ErrorContext::PasswordReset, Language::Zh);
        assert_eq!(msg, "重置验证码无效或已过期");
    }

    #[test]
    fn humanize_reset_400_en() {
        let err = ClientError::Other(400, r#"{"error":"invalid_token"}"#.into());
        let msg = humanize_error(&err, ErrorContext::PasswordReset, Language::En);
        assert_eq!(msg, "Invalid or expired reset code");
    }

    #[test]
    fn humanize_reset_503_zh() {
        let err = ClientError::Other(503, r#"{"error":"mail_unconfigured"}"#.into());
        let msg = humanize_error(&err, ErrorContext::PasswordReset, Language::Zh);
        assert_eq!(msg, "密码重置功能暂不可用");
    }

    #[test]
    fn humanize_reset_503_en() {
        let err = ClientError::Other(503, r#"{"error":"mail_unconfigured"}"#.into());
        let msg = humanize_error(&err, ErrorContext::PasswordReset, Language::En);
        assert_eq!(msg, "Password reset is currently unavailable");
    }

    // ── humanize_error: ChannelManagement context ───────

    #[test]
    fn humanize_channel_401_zh() {
        let err = ClientError::Other(401, r#"{"error":"unauthorized"}"#.into());
        let msg = humanize_error(&err, ErrorContext::ChannelManagement, Language::Zh);
        assert_eq!(msg, "未登录或会话已过期");
    }

    #[test]
    fn humanize_channel_401_en() {
        let err = ClientError::Other(401, r#"{"error":"unauthorized"}"#.into());
        let msg = humanize_error(&err, ErrorContext::ChannelManagement, Language::En);
        assert_eq!(msg, "Not logged in or session expired");
    }

    #[test]
    fn humanize_channel_403_zh() {
        let err = ClientError::Other(403, r#"{"error":"forbidden"}"#.into());
        let msg = humanize_error(&err, ErrorContext::ChannelManagement, Language::Zh);
        assert_eq!(msg, "权限不足 (需 admin)");
    }

    #[test]
    fn humanize_channel_403_en() {
        let err = ClientError::Other(403, r#"{"error":"forbidden"}"#.into());
        let msg = humanize_error(&err, ErrorContext::ChannelManagement, Language::En);
        assert_eq!(msg, "Insufficient permissions (admin required)");
    }

    #[test]
    fn humanize_channel_404_zh() {
        let err = ClientError::Other(404, r#"{"error":"not_found"}"#.into());
        let msg = humanize_error(&err, ErrorContext::ChannelManagement, Language::Zh);
        assert_eq!(msg, "渠道不存在");
    }

    #[test]
    fn humanize_channel_404_en() {
        let err = ClientError::Other(404, r#"{"error":"not_found"}"#.into());
        let msg = humanize_error(&err, ErrorContext::ChannelManagement, Language::En);
        assert_eq!(msg, "Channel not found");
    }

    // ── humanize_error: Metering context ────────────────

    #[test]
    fn humanize_metering_401_zh() {
        let err = ClientError::Other(401, r#"{"error":"unauthorized"}"#.into());
        let msg = humanize_error(&err, ErrorContext::Metering, Language::Zh);
        assert_eq!(msg, "未登录或会话已过期");
    }

    #[test]
    fn humanize_metering_401_en() {
        let err = ClientError::Other(401, r#"{"error":"unauthorized"}"#.into());
        let msg = humanize_error(&err, ErrorContext::Metering, Language::En);
        assert_eq!(msg, "Not logged in or session expired");
    }

    #[test]
    fn humanize_metering_403_zh() {
        let err = ClientError::Other(403, r#"{"error":"forbidden"}"#.into());
        let msg = humanize_error(&err, ErrorContext::Metering, Language::Zh);
        assert_eq!(msg, "权限不足 (需 admin)");
    }

    #[test]
    fn humanize_metering_403_en() {
        let err = ClientError::Other(403, r#"{"error":"forbidden"}"#.into());
        let msg = humanize_error(&err, ErrorContext::Metering, Language::En);
        assert_eq!(msg, "Insufficient permissions (admin required)");
    }
}
