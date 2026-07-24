//! WeChat callback API handlers.
//!
//! Provides WeChat callback handlers for server verification (echostr
//! handshake) and incoming message processing (captcha generation on a
//! trigger keyword or a custom-menu CLICK event, a configurable welcome
//! reply on subscribe, AI-reply transfer for other user chat messages, and a
//! bare `success` ack for non-chat events), plus a small endpoint reporting
//! whether the WeChat captcha-login feature is enabled.

use std::collections::HashMap;

use serde::Serialize;
use wechat_api::callback::CallbackQuery;

use crate::AppState;
use crate::handlers::helpers::extract_state;
use ains_runtime::{HttpError, RequestContext, Response};

// ── WeChat configuration status endpoint ───────────────────────────────────

#[derive(Serialize)]
pub struct WechatEnabledResponse {
    pub enabled: bool,
}

/// GET /api/public/auth/wechat-enabled
///
/// Returns whether the WeChat captcha-login feature is enabled.
/// The frontend uses this to conditionally show the captcha login tab.
pub async fn wechat_enabled(req: crate::ServerRequest) -> Result<Response, HttpError> {
    let state: AppState = extract_state(&req)?;
    let enabled = state.wechat.is_some();
    Response::json(&WechatEnabledResponse { enabled })
}

// ── WeChat callback handlers ──────────────────────────────────────────────

/// GET /api/public/wechat/callback
///
/// WeChat server verification (echostr handshake).
/// The account_id is determined from config rather than URL path.
pub async fn wechat_callback_get(req: crate::ServerRequest) -> Result<Response, HttpError> {
    let state: AppState = extract_state(&req)?;

    let raw_query: HashMap<String, String> = req
        .parse_query()
        .map_err(|_| HttpError::bad_request("Missing WeChat callback query parameters"))?;
    let query = CallbackQuery::from_params(raw_query.iter().map(|(k, v)| (k.as_str(), v.as_str())));

    let wechat = state
        .wechat
        .as_ref()
        .ok_or_else(|| HttpError::not_found("WeChat components not initialized"))?;

    // Verify signature and return echostr.
    match wechat_api::callback::handle_verification(&wechat.config, &query) {
        Ok(echostr) => {
            let mut resp = Response::new();
            resp.set_text_body(echostr);
            resp.set_content_type("text/plain");
            Ok(resp)
        }
        Err(_) => Err(HttpError::bad_request("Signature verification failed")),
    }
}

/// POST /api/public/wechat/callback
///
/// WeChat message callback — processes incoming text messages and triggers
/// captcha generation when a trigger keyword is received. The captcha code
/// is returned in the reply message so the user sees it in their WeChat chat.
///
/// IMPORTANT: This handler MUST always return 200 OK. WeChat retries messages
/// on non-200 responses, so all errors are logged and absorbed.
pub async fn wechat_callback_post(mut req: crate::ServerRequest) -> Result<Response, HttpError> {
    let state: AppState = extract_state(&req)?;

    let reply_xml = match wechat_process_callback(&state, &mut req).await {
        Ok(xml) => xml,
        Err(e) => {
            tracing::warn!("WeChat callback error (returning 200 OK to suppress retry): {e}");
            "success".to_string()
        }
    };

    let mut response = Response::new();
    response.set_text_body(&reply_xml);
    response.set_content_type("application/xml");
    Ok(response)
}

/// Inner processing for WeChat callbacks. All errors are captured as `String`
/// so the outer handler can always return 200 OK.
async fn wechat_process_callback(
    state: &AppState,
    req: &mut crate::ServerRequest,
) -> Result<String, String> {
    let raw_query: HashMap<String, String> = req
        .parse_query()
        .map_err(|e| format!("Missing/invalid WeChat callback query parameters: {e}"))?;
    let query = CallbackQuery::from_params(raw_query.iter().map(|(k, v)| (k.as_str(), v.as_str())));

    let body_bytes = req
        .read_body_bytes()
        .await
        .map_err(|e| format!("Failed to read WeChat callback body: {e}"))?;
    let body = String::from_utf8_lossy(&body_bytes).to_string();

    let wechat = state
        .wechat
        .as_ref()
        .ok_or_else(|| "WeChat components not initialized".to_string())?;

    // Parse callback from WeChat.
    let parsed = wechat_api::parse_callback(&wechat.config, &query, &body)
        .map_err(|e| format!("Failed to parse WeChat message: {e}"))?;

    let account_id = &wechat.config.account_id;
    let msg = &parsed.message;

    // A captcha is issued when either:
    // - a text message contains a trigger keyword (e.g. 「验证码」), or
    // - the user taps the custom-menu CLICK button whose key is `GET_AINS_CAPTCHA`.
    let should_send_captcha = if msg.msg_type == "text" {
        wechat
            .captcha_service
            .matches_trigger(msg.content.as_deref().unwrap_or(""))
    } else {
        msg.is_event()
            && msg.event.as_deref() == Some("CLICK")
            && msg.event_key.as_deref() == Some("GET_AINS_CAPTCHA")
    };

    let reply_xml = if should_send_captcha {
        // Generate a captcha, store it, and reply with the code.
        match wechat
            .captcha_service
            .generate(account_id, &msg.from_user_name)
            .await
        {
            Ok(code) => {
                let reply_text = format!(
                    "你的验证码：{}，有效期{}分钟。请在登录页输入该验证码完成登录。",
                    code,
                    wechat.captcha_service.captcha_ttl() / 60,
                );
                wechat_api::build_text_reply(&msg.from_user_name, &msg.to_user_name, &reply_text)
            }
            Err(wechat_api::WechatError::CooldownActive) => {
                let reply_text = "最近已发送过验证码，请查看聊天记录，或稍后再重新获取。";
                wechat_api::build_text_reply(&msg.from_user_name, &msg.to_user_name, reply_text)
            }
            Err(e) => {
                tracing::error!("Failed to generate captcha: {e}");
                let reply_text = "系统繁忙，请稍后再试。";
                wechat_api::build_text_reply(&msg.from_user_name, &msg.to_user_name, reply_text)
            }
        }
    } else if msg.is_subscribe() && !wechat.subscribe_reply.is_empty() {
        // A user just followed the account. The backend "subscribe auto-reply"
        // is bypassed once a callback URL + Token is configured, so we emit the
        // configured welcome text ourselves. When `subscribe_reply` is empty
        // this branch is skipped and the event falls through to the AI reply.
        wechat_api::build_text_reply(
            &msg.from_user_name,
            &msg.to_user_name,
            &wechat.subscribe_reply,
        )
    } else if !msg.is_event() || msg.is_subscribe() {
        // A user-sent chat message that isn't a captcha trigger (regular text,
        // image, voice, video, shared location, link), OR a `subscribe` event
        // when no welcome is configured: hand the conversation over to WeChat's
        // official AI reply service via a `transfer_biz_ai_ivr` passive reply.
        // Returning our own text (or "success"/empty) here would suppress the
        // account's configured AI reply, so we explicitly transfer instead.
        wechat_api::build_ai_transfer_reply(&msg.from_user_name, &msg.to_user_name)
    } else {
        // Any other event message (unsubscribe, automatic LOCATION/SCAN
        // reports, menu VIEW clicks, non-captcha CLICK keys, template-send
        // callbacks, …). These are not user chat messages, so replying — even
        // a transfer — would push an unwanted message to the user or produce
        // an error reply. Acknowledge with a bare "success" so WeChat does not
        // retry, without sending anything to the user.
        "success".to_string()
    };

    Ok(reply_xml)
}
