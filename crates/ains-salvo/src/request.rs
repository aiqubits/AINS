use ains_runtime::RequestContext;
use bytes::Bytes;
use salvo::Depot;
use salvo::http::header::HeaderMap;
use salvo::http::uri::Uri;
use salvo::http::{Method, Request as SalvoRequest};
use salvo::routing::PathParams;
use serde::de::DeserializeOwned;

/// 统一请求上下文（Salvo 端）
///
/// # 设计：owned 快照，无 unsafe
///
/// 历史实现用裸指针持有 `handle()` 栈帧上的 `&mut SalvoRequest` / `&mut Depot`
/// 并手写 `unsafe impl Send/Sync`。由于 `UnifiedRequest` 是 `'static` 且按值
/// 交给用户 handler，安全代码即可把它 `tokio::spawn` 出 `handle()` 栈帧，
/// 裸指针悬垂——这是可被安全代码触发的 use-after-free（unsound）。
///
/// 现实现改为在构造时对请求做 **owned 快照**：
/// - `method` / `uri` / `headers` / `params`：从原生请求克隆（读多写少，成本可控）
/// - `remote_ip`：构造时立即解析为 `Option<IpAddr>`
/// - `depot`：经 `std::mem::take` **整体移入**（`get_data` 需访问任意
///   `Box<dyn Any>`，无法克隆）。前置中间件在 `call_next` 返回后不再读
///   Depot，因此移交所有权是安全的；handler 之后 Depot 留空属预期行为
/// - `cached_body`：Eager Buffered 的 owned `Bytes`
///
/// 所有字段自身即 `Send + Sync + 'static`，`Send`/`Sync` 由编译器自动派生，
/// 逃逸到其他任务也不会产生悬垂。
///
/// # 维护警告：新增 RequestContext 方法时的同步义务
///
/// 当向 `ains_runtime::RequestContext` trait 添加**新的默认方法**时，
/// 必须在此适配器中检查是否需要重写（override），具体场景包括但不限于：
///
/// - 新方法使用了 `matched_route_pattern()`（Salvo 端返回 None，需自行实现）
/// - 新方法通过 `get_data()` / `get_data_ref()` 访问扩展数据（Salvo 端在 Depot 中）
/// - 新方法涉及 body 或 cookie 解析（Salvo 端基于快照字段实现）
///
/// **参考案例**：`get_param()` 的默认实现依赖 `matched_route_pattern()`，
/// Salvo 端已重写以使用快照的 `PathParams`。若新增的默认方法
/// 也依赖 `matched_route_pattern()`，必须同步重写，否则会导致参数
/// 提取在 Salvo 模式下静默失败。
#[must_use]
pub struct UnifiedRequest {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    params: PathParams,
    remote_ip: Option<std::net::IpAddr>,
    depot: Depot,
    cached_body: Bytes,
}

impl UnifiedRequest {
    /// 创建新的 UnifiedRequest：对 `req` 做 owned 快照并取走 `depot`。
    ///
    /// 调用后 `depot` 被置空（`std::mem::take`）；调用方（`UnifiedHandler`）
    /// 是路由终点 handler，其后无人再读 Depot。
    pub fn new(req: &mut SalvoRequest, depot: &mut Depot, cached_body: Bytes) -> Self {
        let remote_ip = match req.remote_addr() {
            salvo::conn::SocketAddr::IPv4(addr) => Some(std::net::IpAddr::V4(*addr.ip())),
            salvo::conn::SocketAddr::IPv6(addr) => Some(std::net::IpAddr::V6(*addr.ip())),
            _ => None,
        };
        Self {
            method: req.method().clone(),
            uri: req.uri().clone(),
            headers: req.headers().clone(),
            params: req.params().clone(),
            remote_ip,
            depot: std::mem::take(depot),
            cached_body,
        }
    }
}

impl RequestContext for UnifiedRequest {
    fn method(&self) -> &str {
        self.method.as_str()
    }

    fn path(&self) -> &str {
        self.uri.path()
    }

    fn client_ip(&self) -> Option<std::net::IpAddr> {
        // Check X-Forwarded-For header first (proxy / load balancer support)
        if let Some(ip) = self
            .header("x-forwarded-for")
            .and_then(|v| v.split(',').next().map(|s| s.trim()))
            .and_then(|s| s.parse::<std::net::IpAddr>().ok())
        {
            return Some(ip);
        }

        // Then X-Real-IP
        if let Some(ip) = self
            .header("x-real-ip")
            .and_then(|v| v.trim().parse::<std::net::IpAddr>().ok())
        {
            return Some(ip);
        }

        // Fall back to peer IP（构造时已从原生请求快照）
        self.remote_ip
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)?.to_str().ok()
    }

    fn matched_route_pattern(&self) -> Option<&str> {
        // ⚠️  IMPORTANT — 此方法故意返回 None。
        //
        // Salvo 通过快照的 PathParams 提取路径参数（见 get_param() 的 override），
        // 不需要 matched_route_pattern 来做路径模板匹配。这与 Axum 不同（Axum 使用
        // MatchedPath extension 来获取路径模板）。
        //
        // RequestContext trait 的默认 get_param() 实现依赖 matched_route_pattern()。
        // Salvo 端已重写 get_param() 以避免使用默认实现（见上文）。任何在 trait 上
        // 新增的、使用 matched_route_pattern() 的默认方法，都必须在 Salvo 端同步重写。
        None
    }

    fn get_param(&self, name: &str) -> Option<&str> {
        // 与 salvo `Request::param::<&str>` 同口径：PathParams 原值直取
        self.params.get(name).map(String::as_str)
    }

    fn parse_query<T: DeserializeOwned>(&self) -> Result<T, String> {
        let query_str = self.uri.query().unwrap_or("");
        serde_urlencoded::from_str(query_str).map_err(|e| e.to_string())
    }

    fn get_data<T: Clone + Send + Sync + 'static>(&self) -> Option<T> {
        self.depot.obtain::<T>().ok().cloned()
    }

    fn get_data_ref<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.depot.obtain::<T>().ok()
    }

    fn set_data<T: Clone + Send + Sync + 'static>(&mut self, value: T) -> Option<T> {
        let old = self.depot.scrape::<T>().ok();
        self.depot.inject(value);
        old
    }

    fn cookie(&self, name: &str) -> Option<String> {
        self.headers
            .get("cookie")?
            .to_str()
            .ok()?
            .split(';')
            .filter_map(|c| cookie::Cookie::parse(c.trim()).ok())
            .find(|c| c.name() == name)
            .map(|c| c.value().to_string())
    }

    async fn parse_json<T: DeserializeOwned>(&mut self) -> Result<T, String> {
        serde_json::from_slice(&self.cached_body).map_err(|e| e.to_string())
    }

    async fn read_body_bytes(&mut self) -> Result<Bytes, String> {
        Ok(self.cached_body.clone())
    }

    async fn parse_form<T: DeserializeOwned>(&mut self) -> Result<T, String> {
        serde_urlencoded::from_bytes(&self.cached_body).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_native_request() -> SalvoRequest {
        let hyper_req = salvo::hyper::Request::builder()
            .method("POST")
            .uri("http://example.com/api/users/42?page=2")
            .header("content-type", "application/json")
            .header("cookie", "session=abc; theme=dark")
            .header("x-real-ip", "203.0.113.9")
            .body(salvo::http::body::ReqBody::Once(Bytes::from(
                "{\"name\":\"neo\"}",
            )))
            .unwrap();
        let mut req = SalvoRequest::new();
        req.merge_hyper(hyper_req);
        req.params_mut().insert("id", "42".to_string());
        req
    }

    #[tokio::test]
    async fn snapshot_covers_request_surface() {
        let mut req = build_native_request();
        let mut depot = Depot::new();
        depot.inject(7usize);
        let mut unified =
            UnifiedRequest::new(&mut req, &mut depot, Bytes::from("{\"name\":\"neo\"}"));

        assert_eq!(unified.method(), "POST");
        assert_eq!(unified.path(), "/api/users/42");
        assert_eq!(unified.header("content-type"), Some("application/json"));
        assert_eq!(unified.get_param("id"), Some("42"));
        assert_eq!(unified.cookie("theme").as_deref(), Some("dark"));
        assert_eq!(
            unified.client_ip(),
            "203.0.113.9".parse::<std::net::IpAddr>().ok()
        );
        #[derive(serde::Deserialize)]
        struct Query {
            page: u32,
        }
        assert_eq!(unified.parse_query::<Query>().unwrap().page, 2);
        assert_eq!(
            unified.read_body_bytes().await.unwrap(),
            Bytes::from("{\"name\":\"neo\"}")
        );
        // Depot 所有权已移入：原 depot 置空，数据经 get_data/set_data 存取
        assert_eq!(unified.get_data::<usize>(), Some(7));
        assert!(depot.obtain::<usize>().is_err());
        assert_eq!(unified.set_data(9usize), Some(7));
        assert_eq!(unified.get_data::<usize>(), Some(9));
    }

    #[tokio::test]
    async fn unified_request_outlives_native_request_and_crosses_tasks() {
        // soundness 回归：旧实现持有 handle() 栈帧裸指针，UnifiedRequest
        // 逃逸（spawn / 存活超过原生请求）即 use-after-free。owned 快照
        // 实现下此测试必须完全安全。
        let unified = {
            let mut req = build_native_request();
            let mut depot = Depot::new();
            depot.inject("state".to_string());
            UnifiedRequest::new(&mut req, &mut depot, Bytes::from("body"))
            // req / depot 在此作用域结束时销毁
        };

        let handle = tokio::spawn(async move {
            assert_eq!(unified.method(), "POST");
            assert_eq!(unified.path(), "/api/users/42");
            assert_eq!(unified.get_param("id"), Some("42"));
            assert_eq!(unified.get_data::<String>().as_deref(), Some("state"));
            unified.header("content-type").map(str::to_owned)
        });
        assert_eq!(handle.await.unwrap().as_deref(), Some("application/json"));
    }
}
