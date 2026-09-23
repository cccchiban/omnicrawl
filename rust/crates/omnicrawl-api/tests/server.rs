//! 服务端装配的端到端测试：真实回环 HTTP 上跑一遍鉴权、信封、CORS 与框架级错误。
//!
//! 断言取自 Python 侧同名行为（`tests/test_api.py`）：`/health` 公共；
//! `/api/v1` 下已匹配路由需要 `Bearer`；未配置白名单时不装 CORS，预检落到 405。

use std::io::Read;
use std::net::SocketAddr;

use axum::routing::get;
use axum::Router;
use omnicrawl_api::{build_app, build_router, merge_guarded, ApiConfig, ApiState};
use serde_json::Value;

const TOKEN: &str = "parity-token";
const ORIGIN: &str = "http://localhost:5173";

fn config(origins: &[&str]) -> ApiConfig {
    ApiConfig::new(
        TOKEN,
        "127.0.0.1",
        8765,
        origins.iter().map(|origin| origin.to_string()).collect(),
        300.0,
        1,
    )
    .expect("合法配置")
}

/// 起一个真实回环服务；返回监听地址。
fn spawn(app: Router) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定回环端口");
    listener.set_nonblocking(true).expect("设为非阻塞");
    let address = listener.local_addr().expect("读取监听地址");
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("构建 tokio 运行时");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).expect("接管监听套接字");
            let _ = axum::serve(listener, app).await;
        });
    });
    address
}

struct Reply {
    status: u16,
    body: String,
    headers: Vec<(String, String)>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).expect("响应体应当是 JSON")
    }
}

fn client() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into()
}

fn collect(response: ureq::http::Response<ureq::Body>) -> Reply {
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_ascii_lowercase(),
                value.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let mut reader = response.into_body().into_reader();
    let mut body = String::new();
    let _ = reader.read_to_string(&mut body);
    Reply {
        status,
        body,
        headers,
    }
}

fn request(method: &str, address: SocketAddr, path: &str, headers: &[(&str, &str)]) -> Reply {
    let url = format!("http://{address}{path}");
    let agent = client();
    if method == "POST" {
        let mut builder = agent.post(&url);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        return collect(builder.send_empty().expect("请求应当到达服务端"));
    }
    let mut builder = match method {
        "GET" => agent.get(&url),
        "OPTIONS" => agent.options(&url),
        other => panic!("未覆盖的方法：{other}"),
    };
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    collect(builder.call().expect("请求应当到达服务端"))
}

fn auth() -> [(&'static str, &'static str); 1] {
    [("Authorization", "Bearer parity-token")]
}

/// 生产路由集合为空，探针模块用来验证装配层（鉴权、CORS、框架错误）本身。
fn probe_app(config: ApiConfig) -> Router {
    async fn probe() -> &'static str {
        "ok"
    }
    let state = ApiState::new(config);
    build_app(
        state.clone(),
        merge_guarded(state, vec![Router::new().route("/probe", get(probe))]),
    )
}

#[test]
fn health_is_public_and_uses_data_envelope() {
    let address = spawn(build_router(config(&[])));
    let reply = request("GET", address, "/health", &[]);

    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.json(),
        serde_json::json!({"data": {"status": "ok", "service": "omnicrawl"}})
    );
}

#[test]
fn matched_api_routes_require_bearer_token() {
    let address = spawn(probe_app(config(&[])));

    let missing = request("GET", address, "/api/v1/probe", &[]);
    assert_eq!(missing.status, 401);
    assert_eq!(
        missing.json(),
        serde_json::json!({
            "error": {
                "code": "UNAUTHORIZED",
                "message": "缺少或无效的 Bearer Token。",
                "details": Value::Null,
            }
        })
    );

    let wrong_token = request(
        "GET",
        address,
        "/api/v1/probe",
        &[("Authorization", "Bearer other-token")],
    );
    assert_eq!(wrong_token.status, 401);

    let without_scheme = request(
        "GET",
        address,
        "/api/v1/probe",
        &[("Authorization", "parity-token")],
    );
    assert_eq!(without_scheme.status, 401);

    let accepted = request("GET", address, "/api/v1/probe", &auth());
    assert_eq!(accepted.status, 200);
    assert_eq!(accepted.body, "ok");
}

#[test]
fn unknown_paths_and_methods_use_framework_detail() {
    let address = spawn(probe_app(config(&[])));

    let unknown = request("GET", address, "/api/v1/nope", &[]);
    assert_eq!(unknown.status, 404);
    assert_eq!(unknown.json(), serde_json::json!({"detail": "Not Found"}));

    let root = request("GET", address, "/nope", &[]);
    assert_eq!(root.status, 404);

    let wrong_method = request("POST", address, "/api/v1/probe", &auth());
    assert_eq!(wrong_method.status, 405);
    assert_eq!(
        wrong_method.json(),
        serde_json::json!({"detail": "Method Not Allowed"})
    );
}

#[test]
fn cors_allows_configured_origin_only() {
    let address = spawn(probe_app(config(&[ORIGIN])));

    let allowed = request(
        "GET",
        address,
        "/api/v1/probe",
        &[("Origin", ORIGIN), ("Authorization", "Bearer parity-token")],
    );
    assert_eq!(allowed.status, 200);
    assert_eq!(allowed.header("access-control-allow-origin"), Some(ORIGIN));

    let rejected = request(
        "GET",
        address,
        "/api/v1/probe",
        &[("Origin", "http://evil.example")],
    );
    assert_eq!(rejected.header("access-control-allow-origin"), None);
}

#[test]
fn cors_preflight_is_answered_with_methods_and_headers() {
    let address = spawn(probe_app(config(&[ORIGIN])));

    let allowed = request(
        "OPTIONS",
        address,
        "/api/v1/probe",
        &[("Origin", ORIGIN), ("Access-Control-Request-Method", "GET")],
    );
    assert_eq!(allowed.status, 200);
    assert_eq!(allowed.header("access-control-allow-origin"), Some(ORIGIN));
    assert_eq!(
        allowed.header("access-control-allow-methods"),
        Some("GET, POST, PUT, PATCH, DELETE, OPTIONS")
    );
    assert_eq!(
        allowed.header("access-control-allow-headers"),
        Some("Authorization, Content-Type, Last-Event-ID")
    );

    let rejected = request(
        "OPTIONS",
        address,
        "/api/v1/probe",
        &[
            ("Origin", "http://evil.example"),
            ("Access-Control-Request-Method", "GET"),
        ],
    );
    assert_eq!(rejected.status, 400);
    assert_eq!(rejected.header("access-control-allow-origin"), None);
}

#[test]
fn cors_middleware_is_absent_without_allowlist() {
    let address = spawn(probe_app(config(&[])));

    let preflight = request(
        "OPTIONS",
        address,
        "/api/v1/probe",
        &[("Origin", ORIGIN), ("Access-Control-Request-Method", "GET")],
    );
    assert_eq!(preflight.status, 405);
    assert_eq!(preflight.header("access-control-allow-origin"), None);
}
