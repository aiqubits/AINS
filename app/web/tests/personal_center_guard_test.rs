//! 个人中心 403 处理策略的回归守卫。
//!
//! 个人中心的自助接口把 403 作为**预期业务状态**（租户被禁用，
//! `require_actor_tenant_active`），必须经 `ErrorContext::PersonalCenter`
//! 渲染为业务文案。若误用 `handle_unauth`（401+403 拦截），403 会被
//! 当作权限异常跳转回个人中心自身后吞掉，导致区块永久 loading、
//! 购买弹窗无反馈卡死 —— 本文件静态守卫该修复不被回退。
//!
//! 与 `auth_view_redirect_test.rs` 一致：纯 std + `include_str!`，
//! 编译期静态校验，无需 VirtualDom。

const PERSONAL_CENTER_VIEW: &str = include_str!("../src/views/personal_center.rs");

/// 个人中心必须使用 401-only 拦截变体，403 放行给 humanize_error。
#[test]
fn personal_center_uses_401_only_interception() {
    assert!(
        PERSONAL_CENTER_VIEW.contains("handle_unauth_401_only("),
        "personal_center.rs 必须使用 handle_unauth_401_only 拦截错误"
    );
}

/// 禁止调用会拦截 403 的 `handle_unauth`（裸形式）：403 是本视图的
/// 预期业务状态（租户禁用），不得被跳转吞掉。
#[test]
fn personal_center_never_calls_bare_handle_unauth() {
    assert!(
        !PERSONAL_CENTER_VIEW.contains("handle_unauth("),
        "personal_center.rs 不得调用 handle_unauth（会把租户禁用的 403 \
         当作权限异常跳转吞掉）；请使用 handle_unauth_401_only"
    );
    // 封堵别名导入绕过：`use crate::api::handle_unauth as xx;` 后的
    // 别名调用不含 "handle_unauth(" 子串，上方断言拦不住。
    assert!(
        !PERSONAL_CENTER_VIEW.contains("handle_unauth as "),
        "personal_center.rs 不得通过别名导入 handle_unauth 绕过本守卫"
    );
}

/// 错误文案必须走 PersonalCenter 语境：自助接口的 403 来自租户禁用
/// 而非权限不足，不可复用管理页“需 admin”文案。
#[test]
fn personal_center_uses_dedicated_error_context() {
    assert!(
        PERSONAL_CENTER_VIEW.contains("ErrorContext::PersonalCenter"),
        "personal_center.rs 的错误翻译必须使用 ErrorContext::PersonalCenter"
    );
    assert!(
        !PERSONAL_CENTER_VIEW.contains("ErrorContext::PlanManagement"),
        "personal_center.rs 不得复用管理页语境（403 文案为“需 admin”，\
         与自助接口的租户禁用语义不符）"
    );
}
