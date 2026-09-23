//! HTTP 应用装配：路由、Bearer 鉴权中间件、CORS 与框架级错误响应。
//!
//! 对应 `omnicrawl/api/app.py` 的 `create_app`：公共路由有 `/health`、`/docs` 与
//! `/openapi.json`，资源模块的路由统一经 [`api_routes`] 挂在鉴权层之下；
//! 未配置来源白名单时不装 CORS。

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::config::{ApiConfig, API_PREFIX};
use crate::error::{data, framework_detail, ApiError};
use crate::service::AgentService;

/// CORS 允许的方法（对应 Python 侧 `allow_methods`）。
const CORS_METHODS: &str = "GET, POST, PUT, PATCH, DELETE, OPTIONS";
/// CORS 允许的请求头（对应 Python 侧 `allow_headers`）。
const CORS_HEADERS: &str = "Authorization, Content-Type, Last-Event-ID";

/// 服务共享状态：配置 + 已就绪的 Agent 服务。
///
/// 服务可能尚未就绪（宿主还在起内核），此时 `/api/v1` 下的资源路由按契约回
/// `503 SERVICE_UNAVAILABLE`，与 Python 侧 `deps.service` 同义。
#[derive(Clone)]
pub struct ApiState {
    pub config: Arc<ApiConfig>,
    service: Option<Arc<AgentService>>,
}

impl ApiState {
    pub fn new(config: ApiConfig) -> Self {
        Self {
            config: Arc::new(config),
            service: None,
        }
    }

    /// 挂上已就绪的 Agent 服务。
    pub fn with_service(mut self, service: Arc<AgentService>) -> Self {
        self.service = Some(service);
        self
    }

    pub fn service(&self) -> Result<&Arc<AgentService>, ApiError> {
        self.service
            .as_ref()
            .ok_or_else(ApiError::service_unavailable)
    }
}

/// 生产装配：`/health` 公共，`/api/v1` 下的资源路由在鉴权之后。
pub fn build_router(config: ApiConfig) -> Router {
    build_router_with_state(ApiState::new(config))
}

/// 带服务状态的装配；二进制与嵌入方用这个入口。
pub fn build_router_with_state(state: ApiState) -> Router {
    build_app(state.clone(), api_routes(state))
}

/// 资源路由集合；迁移初期还没有资源模块，返回空集合（此时也只有框架级 404 可达）。
pub fn api_routes(state: ApiState) -> Router<ApiState> {
    merge_guarded(state, module_routers())
}

/// 各资源模块的路由，随迁移推进逐个加入：
/// sessions → projects/configuration → settings → monitors/subagents/support。
fn module_routers() -> Vec<Router<ApiState>> {
    vec![
        crate::routes::system::router(),
        crate::routes::runs::router(),
        crate::routes::support::router(),
        crate::routes::projects::router(),
        crate::routes::sessions::router(),
        crate::routes::settings::router(),
        crate::routes::monitors::router(),
        crate::routes::configuration::router(),
        crate::routes::subagents::router(),
    ]
}

/// 合并资源模块并统一挂上 Bearer 鉴权。
///
/// 鉴权按模块挂而不是合并后挂：axum 的 `route_layer` 不接受没有任何路由的
/// `Router`，空集合只能原样返回。挂层后未匹配的路径仍走外层 fallback，
/// 因此未知路径是 `404` 而不是 `401`——与 Python 侧「路由器级依赖」同义。
pub fn merge_guarded(state: ApiState, modules: Vec<Router<ApiState>>) -> Router<ApiState> {
    let mut merged: Option<Router<ApiState>> = None;
    for module in modules {
        merged = Some(match merged {
            Some(current) => current.merge(module),
            None => module,
        });
    }
    match merged {
        None => Router::new(),
        Some(routes) => routes.route_layer(middleware::from_fn_with_state(state, authorize)),
    }
}

/// 按给定资源路由装配完整应用；宿主与测试用它嵌入自己的路由。
pub fn build_app(state: ApiState, api: Router<ApiState>) -> Router {
    let cors_enabled = !state.config.allowed_origins.is_empty();
    let app = Router::new()
        .route("/health", get(health))
        // 文档端点在鉴权层之外（对应 Python FastAPI 的 `/docs` 与 `/openapi.json`）。
        .route("/docs", get(docs))
        .route("/openapi.json", get(openapi_json))
        .nest(API_PREFIX, api)
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed);
    let app = if cors_enabled {
        app.layer(middleware::from_fn_with_state(state.clone(), cors))
    } else {
        app
    };
    app.with_state(state)
}

/// 绑定 `state.config.host`:`state.config.port` 并开始服务，直到收到 Ctrl-C。
///
/// `api.workers > 1` 时用 SO_REUSEPORT 绑定，让内核在多个 worker 进程间分摊连接
/// （与 Python 侧 uvicorn 多 worker 的监听同义）。
pub async fn serve(state: ApiState) -> Result<(), String> {
    let address = format!("{}:{}", state.config.host, state.config.port);
    let listener = bind_listener(&state.config)?;
    if state.config.workers > 1 {
        println!("[api] worker 进程就绪（SO_REUSEPORT）");
    }
    println!("[api] OmniCrawl 本地 API 监听 http://{address}");
    let router = build_router_with_state(state);
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|error| format!("API 服务异常退出：{error}"))
}

/// 本平台是否支持 SO_REUSEPORT（与 tokio `TcpSocket::set_reuseport` 的可用范围一致）。
///
/// 多进程监听依赖它：监督进程按 `api.workers` 拉起子进程，子进程各自绑定同一端口。
pub fn reuse_port_supported() -> bool {
    cfg!(all(
        unix,
        not(any(target_os = "solaris", target_os = "illumos"))
    ))
}

/// 绑定监听套接字：多 worker 且平台支持时走 SO_REUSEPORT，否则回退普通绑定。
fn bind_listener(config: &ApiConfig) -> Result<tokio::net::TcpListener, String> {
    let address = format!("{}:{}", config.host, config.port);
    if config.workers > 1 && reuse_port_supported() {
        return bind_reuse_port(config);
    }
    let listener = std::net::TcpListener::bind(&address)
        .map_err(|error| format!("无法绑定 {address}：{error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("设置非阻塞失败：{error}"))?;
    tokio::net::TcpListener::from_std(listener)
        .map_err(|error| format!("接管监听套接字失败：{error}"))
}

/// SO_REUSEPORT 绑定：多个 worker 进程监听同一端口，由内核按连接分摊。
#[cfg(all(unix, not(any(target_os = "solaris", target_os = "illumos"))))]
fn bind_reuse_port(config: &ApiConfig) -> Result<tokio::net::TcpListener, String> {
    use std::net::ToSocketAddrs;

    use tokio::net::TcpSocket;

    let address = format!("{}:{}", config.host, config.port);
    let target = (config.host.as_str(), config.port)
        .to_socket_addrs()
        .map_err(|error| format!("无法解析 {address}：{error}"))?
        .next()
        .ok_or_else(|| format!("无法解析 {address}：没有可用地址"))?;
    let socket = if target.is_ipv4() {
        TcpSocket::new_v4()
    } else {
        TcpSocket::new_v6()
    }
    .map_err(|error| format!("创建监听套接字失败：{error}"))?;
    socket
        .set_reuseaddr(true)
        .map_err(|error| format!("设置 SO_REUSEADDR 失败：{error}"))?;
    socket
        .set_reuseport(true)
        .map_err(|error| format!("设置 SO_REUSEPORT 失败：{error}"))?;
    socket
        .bind(target)
        .map_err(|error| format!("无法绑定 {address}：{error}"))?;
    socket
        .listen(1024)
        .map_err(|error| format!("监听 {address} 失败：{error}"))
}

/// 不支持 SO_REUSEPORT 的平台由 [`reuse_port_supported`] 拦下，正常不会走到这里。
#[cfg(not(all(unix, not(any(target_os = "solaris", target_os = "illumos")))))]
fn bind_reuse_port(config: &ApiConfig) -> Result<tokio::net::TcpListener, String> {
    let address = format!("{}:{}", config.host, config.port);
    Err(format!(
        "当前平台不支持 SO_REUSEPORT，无法以多进程方式监听 {address}。"
    ))
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// 无鉴权健康检查。
async fn health() -> Json<Value> {
    data(json!({"status": "ok", "service": "omnicrawl"}))
}

/// 无鉴权 OpenAPI 3.1 文档。
async fn openapi_json() -> Json<Value> {
    Json(crate::openapi::document())
}

/// 无鉴权 Swagger UI 页面。
async fn docs() -> Html<&'static str> {
    Html(crate::openapi::docs_html())
}

async fn not_found() -> Response {
    framework_detail("Not Found", StatusCode::NOT_FOUND)
}

async fn method_not_allowed() -> Response {
    framework_detail("Method Not Allowed", StatusCode::METHOD_NOT_ALLOWED)
}

async fn authorize(State(state): State<ApiState>, request: Request, next: Next) -> Response {
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if !token_matches(authorization, &state.config.bearer_token) {
        return ApiError::unauthorized().into_response();
    }
    next.run(request).await
}

/// `Authorization: Bearer <token>`：方案名大小写不敏感，令牌按常量时间比较。
pub fn token_matches(authorization: Option<&str>, expected: &str) -> bool {
    let Some(value) = authorization else {
        return false;
    };
    let Some((scheme, token)) = value.split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("bearer") {
        return false;
    }
    constant_time_eq(token.as_bytes(), expected.as_bytes())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

/// 精确来源白名单的 CORS 处理：命中才回写响应头，未命中时不带任何 CORS 头。
async fn cors(State(state): State<ApiState>, request: Request, next: Next) -> Response {
    let Some(origin) = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
    else {
        return next.run(request).await;
    };
    let Ok(origin_header) = HeaderValue::from_str(&origin) else {
        return next.run(request).await;
    };
    let allowed = state
        .config
        .allowed_origins
        .iter()
        .any(|item| item == &origin);
    let preflight = request.method() == Method::OPTIONS
        && request
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
    if !allowed {
        if preflight {
            return (StatusCode::BAD_REQUEST, "Disallowed CORS origin").into_response();
        }
        return next.run(request).await;
    }
    if preflight {
        let mut response = StatusCode::OK.into_response();
        let headers = response.headers_mut();
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin_header);
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static(CORS_METHODS),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static(CORS_HEADERS),
        );
        headers.insert(header::VARY, HeaderValue::from_static("Origin"));
        return response;
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin_header);
    headers.insert(header::VARY, HeaderValue::from_static("Origin"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_scheme_is_case_insensitive_but_token_is_exact() {
        assert!(token_matches(Some("Bearer token"), "token"));
        assert!(token_matches(Some("bearer token"), "token"));
        assert!(token_matches(Some("BEARER token"), "token"));
        assert!(!token_matches(Some("Bearer Token"), "token"));
        assert!(!token_matches(Some("Token token"), "token"));
        assert!(!token_matches(Some("token"), "token"));
        assert!(!token_matches(Some("Bearer "), "token"));
        assert!(!token_matches(None, "token"));
        assert!(!token_matches(Some("Bearer token"), ""));
    }
}
