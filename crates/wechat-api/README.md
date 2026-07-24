# wechat-api

Extensible WeChat Official Account API for **ains**.

Encapsulates the WeChat captcha-login flow into a
standalone, pluggable crate designed to grow with future WeChat features.

## Features

| Feature | Default | Description |
|---------|:-------:|-------------|
| `client` | ✅ | HTTP client for WeChat Platform API calls (`access_token`, customer-service messages) |
| `memory-store` | ❌ | In-memory store implementations (tests / single-instance dev) |
| `crypto-safe-mode` | ❌ | AES-CBC decryption for WeChat "safe mode" encrypted messages |

## Module map

| Module | Responsibility |
|--------|----------------|
| `config` | Account credentials & login-flow settings |
| `error` | Unified `WechatError` / `WechatResult` |
| `store` | Pluggable traits: `CaptchaStore`, `UserBindingStore`, `AccessTokenStore` |
| `crypto` | Signature verification + optional AES decryption |
| `message` | XML (de)serialization & reply builders |
| `callback` | Framework-agnostic GET/POST callback handling |
| `client` | WeChat Platform HTTP API client (`access_token`, custom messages) |
| `captcha` | Captcha generation, storage, one-shot verification |
| `handler` | Extensible `MessageHandler` trait + `HandlerChain` + default handlers |
| `login` | `LoginService` orchestrating captcha → user lookup |

## Login flow

```text
① User sends "验证码" to the official account
   → WeChat POST callback → parse_callback → CaptchaTriggerHandler
   → CaptchaService::generate → WechatClient::send_text_message
   → User receives code in WeChat

② User enters the code on the web login page
   → POST /api/public/auth/login { email, password, captcha_code }
   → CaptchaService::verify_for_openid (one-shot consume)
   → password check → host app issues JWT for the email+password account
```

> **ains uses a shared-captcha model**: the code is a *second factor* for
> email+password login and is **not** bound to any openid — any follower's
> valid code satisfies the check. The optional `LoginService` /
> `UserBindingStore` (openid-bound passwordless login) is retained as SDK
> surface but is **not** used by ains.

## Integration with ains

1. Implement the store traits for ains's `CacheService` and DB layer.
2. Build `WechatClient` + `CaptchaService` in `bootstrap`.
3. Wire `CaptchaTriggerHandler` into the WeChat callback route.
4. Call `CaptchaService::verify_for_openid` as a second factor inside the
   existing `/api/public/auth/login` handler (before the password check).

## Extensibility

Adding new WeChat capabilities later only requires a new module (e.g.
`pay.rs`, `template.rs`, `miniprogram/`) and optionally new store traits. The
existing core (`config`, `crypto`, `message`, `callback`) is reused unchanged.
