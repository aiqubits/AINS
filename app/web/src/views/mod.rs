mod auth;
mod channels;
mod dashboard;
mod forgot_password;
mod login_landing;
mod metering;
mod not_found;
mod personal_center;
mod reset_password;
mod settings;
mod tenants;
mod users;
mod verify_email;

pub use auth::Auth;
pub use channels::Channels;
pub use dashboard::Dashboard;
pub use forgot_password::ForgotPassword;
pub use login_landing::LoginLanding;
pub use metering::Metering;
pub use not_found::NotFound;
pub use personal_center::PersonalCenter;
pub use reset_password::ResetPassword;
pub use settings::Settings;
pub use tenants::Tenants;
pub use users::Users;
pub use verify_email::VerifyEmail;

/// Returns true when the scroll container identified by `id` is scrolled to
/// (or near) its bottom edge. Used by the infinite-scroll tenant dropdowns to
/// decide when to fetch the next page. Returns false when the element or the
/// browser globals are unavailable (e.g. during SSR/hydration or host tests).
#[cfg(target_arch = "wasm32")]
pub(crate) fn element_near_bottom(id: &str) -> bool {
    const THRESHOLD_PX: f64 = 48.0;
    let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
    else {
        return false;
    };
    let scroll_top = el.scroll_top() as f64;
    let client_height = el.client_height() as f64;
    let scroll_height = el.scroll_height() as f64;
    scroll_top + client_height >= scroll_height - THRESHOLD_PX
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn element_near_bottom(_id: &str) -> bool {
    false
}
