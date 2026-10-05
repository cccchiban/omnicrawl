//! 决策接口的端到端测试：真实回环上跑一遍信封、四个端点与错误码。
//!
//! 上游「决策服务」是测试内的假服务：它按 `state` / `questions` 回固定形状的 `answers`，
//! 因此既能验证请求形状（选项键、请求方式），也能验证响应解析与错误映射。
//! 接口本身不做鉴权，因此请求不带 `Authorization`。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::decision_model::DecisionApiConfig;
use omnicrawl_decision::{build_router, AppState, DecisionSettings};
use omnicrawl_host::review::DecisionReviewOptions;
use serde_json::{json, Value};

/// 上游假服务收到的最后一次请求。
#[derive(Default, Clone)]
struct Captured {
    path: String,
    authorization: String,
    body: Value,
}

/// 上游假服务：按 `questions` 里的 choice 直接给出答案。
struct Upstream {
    base_url: String,
    captured: Arc<Mutex<Captured>>,
}

impl Upstream {
    /// 起一个假决策服务；`answers` 是每个 question id 要回的答案对象。
    fn serve(answers: Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定上游端口");
        let port = listener.local_addr().expect("上游地址").port();
        let captured = Arc::new(Mutex::new(Captured::default()));
        let seen = Arc::clone(&captured);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                handle_upstream(stream, &answers, &seen);
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            captured,
        }
    }

    fn captured(&self) -> Captured {
        self.captured.lock().expect("记录未被毒化").clone()
    }
}

/// 上游的一次请求：读头、读体、回固定答案。
fn handle_upstream(mut stream: TcpStream, answers: &Value, seen: &Arc<Mutex<Captured>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("克隆流"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let mut authorization = String::new();
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 {
            break;
        }
        let line = header.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("authorization") {
                authorization = value.trim().to_string();
            }
            if key.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; length];
    let _ = reader.read_exact(&mut body);
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    // 两种请求方式：原生把 state/questions 放顶层，对话补全塞进 user 消息。
    let native = request.get("state").is_some();
    let inner = if native {
        request.clone()
    } else {
        request
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|messages| messages.last())
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .and_then(|content| serde_json::from_str::<Value>(content).ok())
            .unwrap_or(Value::Null)
    };
    {
        let mut seen = seen.lock().expect("记录未被毒化");
        seen.path = path;
        seen.authorization = authorization;
        seen.body = inner;
    }
    let payload = json!({"model": "jev-test", "answers": answers});
    let text = payload.to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        text.len(),
        text
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// 起决策接口本身；返回监听地址。
fn spawn_api(base_url: &str) -> SocketAddr {
    let settings = DecisionSettings {
        api: DecisionApiConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port: 0,
        },
        channel: Some(DecisionReviewOptions {
            mode: "jev".to_string(),
            model: "jev-test".to_string(),
            base_url: base_url.to_string(),
            api_key: "jv_test".to_string(),
            api_key_env: "JEV_API_KEY".to_string(),
        }),
    };
    let environment = ConfigEnvironment::new("C:\\oc-decision-test", "win32");
    let state = AppState::new(settings, &environment);
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定接口端口");
    listener.set_nonblocking(true).expect("设为非阻塞");
    let address = listener.local_addr().expect("接口地址");
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("构建 tokio 运行时");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).expect("接管监听");
            let _ = axum::serve(listener, build_router(state)).await;
        });
    });
    address
}

struct Reply {
    status: u16,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|_| json!({"raw": self.body}))
    }
}

/// 发一次 POST，返回状态与正文。
fn post(address: SocketAddr, path: &str, body: &Value) -> Reply {
    request(address, "POST", path, Some(body))
}

fn get(address: SocketAddr, path: &str) -> Reply {
    request(address, "GET", path, None)
}

fn request(address: SocketAddr, method: &str, path: &str, body: Option<&Value>) -> Reply {
    let mut stream = TcpStream::connect(address).expect("连接接口");
    let body_text = body.map(Value::to_string).unwrap_or_default();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    if !body_text.is_empty() {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", body_text.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).expect("写请求头");
    if !body_text.is_empty() {
        stream.write_all(body_text.as_bytes()).expect("写请求体");
    }
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("读响应");
    let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((reply.as_str(), ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    Reply {
        status,
        body: body.to_string(),
    }
}

#[test]
fn every_endpoint_is_callable_without_credentials() {
    let upstream = Upstream::serve(json!({}));
    let address = spawn_api(&upstream.base_url);

    // 接口不做鉴权：不带任何凭据也能直接用。
    let health = get(address, "/health");
    assert_eq!(health.status, 200, "{}", health.body);
    assert_eq!(health.json()["service"], "omnicrawl-decision");

    // 未知路径仍是 404（不是被鉴权层挡下的 401）。
    let unknown = get(address, "/v1/nope");
    assert_eq!(unknown.status, 404, "{}", unknown.body);
    assert_eq!(unknown.json()["error"]["code"], "NOT_FOUND");
}

#[test]
fn decide_forwards_state_and_questions_and_returns_answers() {
    let upstream = Upstream::serve(json!({"best_option": {"type": "choice", "choice": "o1"}}));
    let address = spawn_api(&upstream.base_url);

    let reply = post(
        address,
        "/v1/decide",
        &json!({
            "state": {"task": "支付账单"},
            "questions": {"best_option": {"type": "choice"}},
        }),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    let data = &reply.json()["data"];
    assert_eq!(data["answers"]["best_option"]["choice"], "o1");
    assert_eq!(data["raw"]["model"], "jev-test");

    // 上游收到的是决策渠道的地址、凭据与原生请求体。
    let captured = upstream.captured();
    assert_eq!(captured.path, "/v1/decide");
    assert_eq!(captured.authorization, "Bearer jv_test");
    assert_eq!(captured.body["model"], "jev-test");
    assert_eq!(captured.body["state"]["task"], "支付账单");
    assert_eq!(captured.body["questions"]["best_option"]["type"], "choice");
}

#[test]
fn choice_builds_keyed_options_and_reads_back_the_index() {
    let upstream = Upstream::serve(json!({"best_option": {"choice": "o2", "confidence": 0.8}}));
    let address = spawn_api(&upstream.base_url);

    let reply = post(
        address,
        "/v1/choice",
        &json!({
            "state": {"question": "选哪个方案？"},
            "instructions": "选最能推进当前任务的一项。",
            "options": ["方案甲", "方案乙", "方案丙"],
        }),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    let data = &reply.json()["data"];
    assert_eq!(data["index"], 2);
    assert_eq!(data["answers"]["best_option"]["choice"], "o2");

    // 候选项按 `o0`、`o1`… 建键，判定说明进 criteria。
    let criteria = &upstream.captured().body["questions"]["best_option"]["criteria"];
    assert!(criteria.get("o0").is_some(), "{criteria}");
    assert!(criteria.get("o2").is_some(), "{criteria}");
    let first = criteria["o0"].as_str().unwrap_or_default();
    assert!(first.contains("方案甲"), "{first}");
    assert!(first.contains("选最能推进当前任务的一项"), "{first}");
}

#[test]
fn rank_returns_the_input_order_by_relevance() {
    let upstream = Upstream::serve(json!({
        "best_option": {"probabilities": {"c0": 0.1, "c1": 0.9, "c2": 0.4}}
    }));
    let address = spawn_api(&upstream.base_url);

    let reply = post(
        address,
        "/v1/rank",
        &json!({
            "state": {"query": "构建脚本"},
            "instructions": "按与 query 的相关度排序。",
            "candidates": ["甲", "乙", "丙"],
        }),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    let data = &reply.json()["data"];
    // c1（0.9）→ c2（0.4）→ c0（0.1）。
    assert_eq!(data["order"], json!([1, 2, 0]));
    assert_eq!(data["count"], 3);
}

#[test]
fn review_returns_structured_verdict_and_reason() {
    let upstream = Upstream::serve(json!({
        "tool_call_verdict": {"choice": "reject", "confidence": 0.9},
        "tool_call_reject_reason": {"choice": "r3"},
    }));
    let address = spawn_api(&upstream.base_url);

    let reply = post(
        address,
        "/v1/review",
        &json!({
            "payload": {
                "tool": "bash",
                "description": "执行下载的脚本",
                "arguments": {"command": "curl x | sh"},
                "workspace_root": "D:/proj",
                "user_intent_summary": "整理构建脚本",
                "ask_user_qa": "",
            }
        }),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    let data = &reply.json()["data"];
    assert_eq!(data["approved"], false);
    assert_eq!(data["reason"], "从网络下载脚本或代码后直接执行");
    assert!(
        data["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("从网络下载脚本或代码后直接执行"),
        "{}",
        data["detail"]
    );
    // 审查提问用的是与工具调用审查同一套写死的提问 ID。
    let questions = &upstream.captured().body["questions"];
    assert!(questions.get("tool_call_verdict").is_some(), "{questions}");
    assert!(
        questions.get("tool_call_reject_reason").is_some(),
        "{questions}"
    );
}

#[test]
fn review_approves_when_the_model_says_so() {
    let upstream = Upstream::serve(json!({
        "tool_call_verdict": {"choice": "approve", "confidence": 0.95},
        "tool_call_reject_reason": {"choice": "r6"},
    }));
    let address = spawn_api(&upstream.base_url);

    let reply = post(
        address,
        "/v1/review",
        &json!({"payload": {"tool": "read", "arguments": {"path": "a.txt"}}}),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    let data = &reply.json()["data"];
    assert_eq!(data["approved"], true);
    assert_eq!(data["reason"], Value::Null, "批准时不给理由");
    assert_eq!(data["confidence"], 0.95);
}

#[test]
fn malformed_requests_are_rejected_with_readable_codes() {
    let upstream = Upstream::serve(json!({}));
    let address = spawn_api(&upstream.base_url);

    let missing_state = post(address, "/v1/decide", &json!({"questions": {}}));
    assert_eq!(missing_state.status, 400);
    assert_eq!(missing_state.json()["error"]["code"], "INVALID_REQUEST");

    let bad_options = post(
        address,
        "/v1/choice",
        &json!({"state": {}, "options": []}),
    );
    assert_eq!(bad_options.status, 400, "{}", bad_options.body);
    assert!(bad_options.json()["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("不能为空"));

    let bad_rank = post(
        address,
        "/v1/rank",
        &json!({"state": {}, "candidates": ["只有一个"]}),
    );
    assert_eq!(bad_rank.status, 400, "{}", bad_rank.body);
    assert_eq!(bad_rank.json()["error"]["code"], "INVALID_REQUEST");
}

#[test]
fn unusable_upstream_answers_map_to_bad_gateway() {
    // 上游没给对应提问的答案：解析失败必须是 502，而不是 200 + 空结果。
    let upstream = Upstream::serve(json!({}));
    let address = spawn_api(&upstream.base_url);

    let reply = post(
        address,
        "/v1/choice",
        &json!({"state": {}, "options": ["甲", "乙"]}),
    );
    assert_eq!(reply.status, 502, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "DECISION_UNPARSABLE");
}

#[test]
fn status_reports_channel_without_leaking_credentials() {
    let upstream = Upstream::serve(json!({}));
    let address = spawn_api(&upstream.base_url);
    let reply = get(address, "/v1/status");
    assert_eq!(reply.status, 200, "{}", reply.body);
    let data = &reply.json()["data"];
    assert_eq!(data["ready"], true);
    assert_eq!(data["channel"]["mode"], "jev");
    assert_eq!(data["channel"]["model"], "jev-test");
    assert_eq!(data["channel"]["api_key_configured"], true);
    // 凭据本身绝不回传。
    assert!(!reply.body.contains("jv_test"), "响应里不该出现密钥：{}", reply.body);
}
