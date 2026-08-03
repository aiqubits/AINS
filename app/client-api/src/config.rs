use crate::error::ClientError;

/// 客户端配置
///
/// 配置 HTTP 客户端的基本参数：基础 URL、超时时间、重试策略等。
///
/// # Examples
///
/// ```rust,no_run
/// use client_api::ClientConfig;
///
/// // 原生平台：指定后端地址
/// let config = ClientConfig::new("http://127.0.0.1:8080");
///
/// // 空 base_url 仅在 WASM（浏览器）下有效
/// // let config = ClientConfig::new("");
///
/// // Builder 模式自定义
/// let config = ClientConfig::new("http://127.0.0.1:8080")
///     .with_timeout(60)
///     .with_max_retries(5);
/// ```
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// API 基础 URL。空字符串表示使用相对路径（适用于 Nginx 反向代理场景）。
    pub base_url: String,
    /// 请求超时时间（秒）
    pub timeout_secs: u64,
    /// 最大重试次数。设置为 3 表示失败后最多额外重试 3 次，
    /// 即总共最多发起 4 次请求（1 次初始 + 3 次重试）。
    /// 设置为 0 表示禁用重试。
    pub max_retries: u32,
    /// 是否禁用系统 HTTP 代理。
    ///
    /// - `true`：调用 `.no_proxy()` 绕过系统代理（适用于测试/容器环境）
    /// - `false`（默认）：使用系统 HTTP_PROXY / HTTPS_PROXY 环境变量
    ///
    /// 此外，环境变量 `AINS_SYS_NO_PROXY=true` 也会令此标志生效，
    /// 方便在不修改代码的情况下全局禁用代理。
    pub no_proxy: bool,
    /// 是否允许明文 HTTP 连接到非本地主机（Phase 7.5 传输加密加固）。
    ///
    /// - `false`（默认）：非本地主机必须使用 `https://`，明文 `http://`
    ///   将在 `validate()` 被拒绝（默认要求 https）。
    /// - `true`：允许明文 http 到远端，但每次校验会发出安全告警
    ///   （`tracing::warn!`）。仅用于受信任内网 / 调试等明确知情场景。
    ///
    /// 本地回环地址（localhost / 127.0.0.0/8 / ::1）恒允许明文 http，不受此标志影响。
    pub allow_insecure_http: bool,
}

impl ClientConfig {
    /// 创建新的配置
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            timeout_secs: 30,
            max_retries: 3,
            no_proxy: false,
            allow_insecure_http: false,
        }
    }

    /// 设置超时时间（秒）
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    /// 设置最大重试次数（0 表示禁用重试）
    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    /// 设置是否禁用系统 HTTP 代理
    pub fn with_no_proxy(mut self, no_proxy: bool) -> Self {
        self.no_proxy = no_proxy;
        self
    }

    /// 允许明文 HTTP 连接到非本地主机（默认禁止；开启后仅告警不拒绝）。
    ///
    /// 传输加密加固（Phase 7.5）：默认要求非本地主机走 https。此开关供
    /// 受信任内网 / 调试等知情场景显式放行明文传输，放行时仍记录告警。
    pub fn with_allow_insecure_http(mut self, allow: bool) -> Self {
        self.allow_insecure_http = allow;
        self
    }

    /// 验证配置是否有效
    pub fn validate(&self) -> Result<(), ClientError> {
        // 原生平台需要绝对 URL（相对路径仅在 WASM 下通过 window.location 推导）
        #[cfg(not(target_arch = "wasm32"))]
        if self.base_url.is_empty() {
            return Err(ClientError::Config(
                "Base URL is required on native platform; use an absolute URL ".to_string(),
            ));
        }

        if !self.base_url.is_empty() {
            let url = reqwest::Url::parse(&self.base_url)
                .map_err(|error| ClientError::Config(format!("Base URL is invalid: {error}")))?;
            validate_transport_url(&url, self.allow_insecure_http)?;
            validate_base_url(&url)?;
        }

        if self.timeout_secs == 0 {
            return Err(ClientError::Config(
                "Timeout must be greater than 0".to_string(),
            ));
        }

        // WASM 下空 base_url 需要浏览器 window 上下文来推导 origin
        #[cfg(target_arch = "wasm32")]
        {
            if self.base_url.is_empty() && web_sys::window().is_none() {
                return Err(ClientError::Config(
                    "Empty base_url requires window context in WASM (not available in Web Workers)"
                        .to_string(),
                ));
            }
            if self.base_url.is_empty()
                && let Some(window) = web_sys::window()
                && let Ok(origin) = window.location().origin()
            {
                let url = reqwest::Url::parse(&origin).map_err(|error| {
                    ClientError::Config(format!("Browser origin is invalid: {error}"))
                })?;
                // 同源部署（wasm 空 base_url）：页面自身的传输协议由部署者
                // 选择，且浏览器同源策略限定暴露面；wasm 无 opt-in 通道
                // （环境变量不可用），此处明文 http 仅告警而非拒绝——拒绝会
                // 静默破坏合法的 http 部署（review 修复）。显式配置的
                // base_url 仍严格走 validate_transport_url（fail-closed）。
                if let Err(error) = validate_transport_url(&url, self.allow_insecure_http) {
                    if url.scheme() == "http" && !host_is_local(url.host_str().unwrap_or("")) {
                        tracing::warn!(
                            origin,
                            "serving over plaintext HTTP; use HTTPS in production ({error})"
                        );
                    } else {
                        return Err(error);
                    }
                }
            }
        }

        Ok(())
    }

    /// 构建完整 URL
    ///
    /// 空字符串 `base_url` 表示使用相对路径（适用于 Nginx 反向代理场景）。
    /// 在 WASM 环境下，空 `base_url` 会自动从浏览器 `window.location.origin()` 推导绝对 URL。
    pub fn build_url(&self, path: &str) -> String {
        let path = path.trim_start_matches('/');

        if self.base_url.is_empty() {
            // 相对路径模式：Nginx 反向代理 / WASM 同源请求
            #[cfg(target_arch = "wasm32")]
            {
                // WASM 下 reqwest 需要绝对 URL,从浏览器 location 推导
                if let Some(window) = web_sys::window()
                    && let Ok(origin) = window.location().origin()
                {
                    return format!("{}/{}", origin.trim_end_matches('/'), path);
                }
            }
            // 回退：理论上不可达——validate() 在 WASM 上已确保 window 存在，
            // 且 origin() 在标准浏览器 API 中从不失败。保留此路径仅作为防御性编程。
            format!("/{}", path)
        } else {
            let base = self.base_url.trim_end_matches('/');
            format!("{}/{}", base, path)
        }
    }
}

impl Default for ClientConfig {
    /// 默认配置使用 `http://127.0.0.1:8080` 作为 base_url，
    /// 适用于本地开发环境。生产环境请通过 `ClientConfig::new()` 显式指定。
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8080".to_string(),
            timeout_secs: 30,
            max_retries: 3,
            no_proxy: false,
            allow_insecure_http: false,
        }
    }
}

/// 验证一个已经由同一 URL 解析器解析的传输目标。
pub(crate) fn validate_transport_url(
    url: &reqwest::Url,
    allow_insecure_http: bool,
) -> Result<(), ClientError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ClientError::Config(format!(
            "URL scheme '{}' is not supported; use http:// or https://",
            url.scheme()
        )));
    }
    let Some(host) = url.host_str() else {
        return Err(ClientError::Config("URL must contain a host".to_string()));
    };
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ClientError::Config(
            "URL userinfo is not allowed; provide credentials through the client API".to_string(),
        ));
    }
    if url.scheme() == "http" && !host_is_local(host) && !allow_insecure_http {
        return Err(ClientError::Config(format!(
            "plaintext HTTP to non-local host '{host}' is insecure; use https:// (or opt in via ClientConfig::with_allow_insecure_http(true))"
        )));
    }
    if url.scheme() == "http" && !host_is_local(host) && allow_insecure_http {
        tracing::warn!(
            host,
            "insecure plaintext HTTP transport to a non-local host; data may be intercepted — prefer https://"
        );
    }
    Ok(())
}

/// Validate a native redirect transition in addition to the destination URL.
///
/// The loopback HTTP exception exists only so a user can explicitly configure
/// a local development endpoint.  It must not let a remote server pivot the
/// client into services listening on the user's machine.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn validate_redirect_transport_url(
    previous: &reqwest::Url,
    next: &reqwest::Url,
    allow_insecure_http: bool,
) -> Result<(), ClientError> {
    validate_transport_url(next, allow_insecure_http)?;

    let previous_host = previous.host_str().ok_or_else(|| {
        ClientError::Config("Redirect source URL must contain a host".to_string())
    })?;
    let next_host = next.host_str().ok_or_else(|| {
        ClientError::Config("Redirect destination URL must contain a host".to_string())
    })?;

    if !host_is_local(previous_host) && host_is_local(next_host) {
        return Err(ClientError::Config(
            "Redirects from a non-local host to a local service are not allowed".to_string(),
        ));
    }
    if previous.scheme() == "https" && next.scheme() == "http" && !allow_insecure_http {
        return Err(ClientError::Config(
            "Redirects from HTTPS to plaintext HTTP are not allowed".to_string(),
        ));
    }

    Ok(())
}

/// `ClientConfig::build_url()` appends API paths textually, so configuration
/// base URLs must be an origin or path prefix.  This is separate from
/// [`validate_transport_url`], because redirect targets legitimately carry
/// query strings and native redirect validation reuses that helper.
fn validate_base_url(url: &reqwest::Url) -> Result<(), ClientError> {
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ClientError::Config(
            "Base URL must not include a query string or fragment".to_string(),
        ));
    }
    Ok(())
}

/// host 是否为本地回环（localhost / 127.0.0.0/8 / ::1 / 0.0.0.0 / :: /
/// IPv6-mapped IPv4 回环 ::ffff:127.0.0.0/104）。
fn host_is_local(host: &str) -> bool {
    // `reqwest::Url::host_str()` retains brackets around IPv6 literals;
    // normalize them before applying the loopback checks.
    let host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host == "::"
        || host == "0.0.0.0"
        || is_loopback_ipv4(host)
        || is_ipv6_mapped_ipv4_loopback(host)
}

/// 检测 IPv6-mapped IPv4 回环地址（::ffff:127.0.0.0/104），提取嵌入的
/// IPv4 后委托 [`is_loopback_ipv4`] 判定。
fn is_ipv6_mapped_ipv4_loopback(host: &str) -> bool {
    // URL parsers may canonicalize dotted IPv4 tails to hexadecimal (for
    // example `::ffff:127.0.0.1` → `::ffff:7f00:1`), so parse the address
    // instead of relying only on textual prefixes.
    if let Ok(ipv6) = host.parse::<std::net::Ipv6Addr>() {
        return ipv6.to_ipv4().is_some_and(|ipv4| ipv4.is_loopback());
    }
    let v4 = match host.strip_prefix("::ffff:") {
        Some(v4) => v4,
        None => match host.strip_prefix("::FFFF:") {
            Some(v4) => v4,
            None => return false,
        },
    };
    is_loopback_ipv4(v4)
}

/// 严格校验 host 是否为 127.0.0.0/8 回环 IPv4 字面量（四段、首段=127，
/// 每段均为合法 u8），避免 `starts_with("127.")` 误匹配 `127.evil.com` 域名。
fn is_loopback_ipv4(host: &str) -> bool {
    let octets: Vec<&str> = host.split('.').collect();
    if octets.len() != 4 {
        return false;
    }
    octets[0].parse::<u8>().ok() == Some(127)
        && octets.iter().skip(1).all(|s| s.parse::<u8>().is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transport_security_hardening() {
        // 默认：非本地主机明文 http 被拒绝（默认要求 https）
        assert!(
            ClientConfig::new("http://api.example.com")
                .validate()
                .is_err()
        );
        assert!(
            ClientConfig::new("http://api.example.com:8443/base")
                .validate()
                .is_err()
        );

        // 非本地主机 https 放行
        assert!(
            ClientConfig::new("https://api.example.com")
                .validate()
                .is_ok()
        );

        // 显式开启 allow_insecure_http 后，远端明文 http 放行（仅告警）
        assert!(
            ClientConfig::new("http://api.example.com")
                .with_allow_insecure_http(true)
                .validate()
                .is_ok()
        );

        // `127.*` 前缀不应匹配非回环域名（防 `127.evil.com` 绕过）
        assert!(
            ClientConfig::new("http://127.evil.com/api")
                .validate()
                .is_err(),
            "127.evil.com is not loopback"
        );

        // 本地回环恒放行明文 http
        for local in [
            "http://127.0.0.1:8080",
            "http://127.0.0.2:3000",
            "http://localhost:3000",
            "http://[::1]:8080",
            "http://[::]:9090",
            "http://0.0.0.0:8000",
        ] {
            assert!(
                ClientConfig::new(local).validate().is_ok(),
                "local host should allow plaintext http: {local}"
            );
        }

        // 仅精确的 `localhost` 可绕过 HTTPS；子域名仍会走普通 DNS，不能
        // 因后缀名称而被假定为回环。
        assert!(
            ClientConfig::new("http://dev.localhost/api")
                .validate()
                .is_err(),
            "*.localhost must not bypass the HTTPS requirement"
        );

        // IPv6 非本地地址明文 http 被拒绝
        assert!(
            ClientConfig::new("http://[2001:db8::1]:8080")
                .validate()
                .is_err()
        );

        // 无方括号 IPv6 被 URL 解析器拒绝 → fail-closed（不绕过安全校验）
        assert!(
            ClientConfig::new("http://::1:8080").validate().is_err(),
            "unbracketed IPv6 must be rejected (fail-closed)"
        );

        // IPv6-mapped IPv4 回环被识别为本地（放行）
        for mapped in [
            "http://[::ffff:127.0.0.1]:8080",
            "http://[::FFFF:127.0.0.1]:8080",
        ] {
            assert!(
                ClientConfig::new(mapped).validate().is_ok(),
                "IPv6-mapped IPv4 loopback should be local: {mapped}"
            );
        }

        // IPv6-mapped 非回环 IPv4 被拒绝
        assert!(
            ClientConfig::new("http://[::ffff:192.168.1.1]:8080")
                .validate()
                .is_err(),
            "IPv6-mapped non-loopback must be rejected"
        );
    }

    #[test]
    fn test_url_parser_security_properties() {
        let url = reqwest::Url::parse("http://127.0.0.1:8080/x").unwrap();
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert!(
            ClientConfig::new("https://user:pass@example.com:443/p")
                .validate()
                .is_err()
        );
        assert!(
            ClientConfig::new(r"http://evil.example\@localhost")
                .validate()
                .is_err()
        );
        assert!(ClientConfig::new("http://[::1]:8080").validate().is_ok());
        assert!(
            ClientConfig::new("https://api.example.com/?tenant=a")
                .validate()
                .is_err()
        );
        assert!(
            ClientConfig::new("https://api.example.com/#section")
                .validate()
                .is_err()
        );
        assert!(ClientConfig::new("").validate().is_err());
    }

    #[test]
    fn redirect_transport_validation_allows_query_and_fragment() {
        let redirect =
            reqwest::Url::parse("https://api.example.com/login?next=%2Fapp#ignored").unwrap();
        assert!(validate_transport_url(&redirect, false).is_ok());
        assert!(validate_base_url(&redirect).is_err());
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn redirect_transport_validation_rejects_remote_to_local_pivots_and_downgrades() {
        let remote = reqwest::Url::parse("https://api.example.com/v1").unwrap();
        let loopback = reqwest::Url::parse("http://127.0.0.1:3000/admin").unwrap();
        let ipv6_loopback = reqwest::Url::parse("http://[::1]:3000/admin").unwrap();
        let remote_http = reqwest::Url::parse("http://api.example.com/v1").unwrap();

        assert!(validate_redirect_transport_url(&remote, &loopback, false).is_err());
        assert!(validate_redirect_transport_url(&remote, &ipv6_loopback, false).is_err());
        // Explicitly allowing insecure remote HTTP does not authorize a
        // remote endpoint to pivot the client into loopback services.
        assert!(validate_redirect_transport_url(&remote, &loopback, true).is_err());
        assert!(validate_redirect_transport_url(&remote, &remote_http, false).is_err());
        assert!(validate_redirect_transport_url(&remote, &remote_http, true).is_ok());
    }

    #[test]
    fn test_config_validation() {
        // 有效配置：HTTP
        let config = ClientConfig::new("http://127.0.0.1:8080");
        assert!(config.validate().is_ok());

        // 有效配置：HTTPS
        let config = ClientConfig::new("https://api.example.com");
        assert!(config.validate().is_ok());

        // 原生平台：空 URL 无效（相对路径仅在 WASM 下通过 window.location 推导）
        let config = ClientConfig::new("");
        assert!(config.validate().is_err());

        // 无效配置：错误协议
        let config = ClientConfig::new("ftp://127.0.0.1");
        assert!(config.validate().is_err());

        // 无效配置：超时为 0
        let config = ClientConfig::new("http://127.0.0.1:8080").with_timeout(0);
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_build_url() {
        // 正常 URL
        let config = ClientConfig::new("http://127.0.0.1:8080");
        assert_eq!(
            config.build_url("/api/v1/users"),
            "http://127.0.0.1:8080/api/v1/users"
        );
        assert_eq!(
            config.build_url("api/v1/users"),
            "http://127.0.0.1:8080/api/v1/users"
        );

        // 带尾部斜杠的 URL
        let config = ClientConfig::new("http://127.0.0.1:8080/");
        assert_eq!(
            config.build_url("/api/v1/users"),
            "http://127.0.0.1:8080/api/v1/users"
        );

        // 空字符串（相对路径）
        let config = ClientConfig::new("");
        assert_eq!(config.build_url("/api/v1/users"), "/api/v1/users");
        assert_eq!(config.build_url("api/v1/users"), "/api/v1/users");
    }

    #[test]
    fn test_builder_pattern() {
        let config = ClientConfig::new("http://127.0.0.1:8080")
            .with_timeout(60)
            .with_max_retries(5);

        assert_eq!(config.base_url, "http://127.0.0.1:8080");
        assert_eq!(config.timeout_secs, 60);
        assert_eq!(config.max_retries, 5);
    }

    #[test]
    fn test_default_config() {
        let config = ClientConfig::default();
        assert_eq!(config.base_url, "http://127.0.0.1:8080");
        assert_eq!(config.timeout_secs, 30);
        assert_eq!(config.max_retries, 3);
    }

    #[test]
    fn test_ipv6_mapped_ipv4_loopback_detection() {
        // 回环
        assert!(is_ipv6_mapped_ipv4_loopback("::ffff:127.0.0.1"));
        assert!(is_ipv6_mapped_ipv4_loopback("::FFFF:127.0.0.2"));
        assert!(is_ipv6_mapped_ipv4_loopback("::ffff:127.255.255.255"));
        // 非回环
        assert!(!is_ipv6_mapped_ipv4_loopback("::ffff:192.168.1.1"));
        assert!(!is_ipv6_mapped_ipv4_loopback("::ffff:8.8.8.8"));
        // 非 IPv6-mapped 格式
        assert!(!is_ipv6_mapped_ipv4_loopback("::1"));
        assert!(!is_ipv6_mapped_ipv4_loopback("127.0.0.1"));
        assert!(!is_ipv6_mapped_ipv4_loopback("localhost"));
    }
}
