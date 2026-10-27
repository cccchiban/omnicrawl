//! 出站 User-Agent：AI 请求网关看到的身份必须是 `omnicrawl/<产品版本>`。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

use omnicrawl_llm::{ChatEndpoint, DiscardSink, OpenAiChatRuntime};
use omnicrawl_protocol::{
    ConversationMessage, GenerationOptions, MessageBlock, Role, TextBlock,
};

const STREAM: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"content\":\"好\"}}]}\n\n",
    "data: [DONE]\n\n",
);

/// 起一个只服务一次请求的回环服务端，把收到的请求头原样交回给断言。
fn spawn_probe() -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("无法监听回环端口");
    let addr = listener.local_addr().expect("无法取本地地址");
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        }
        let head = String::from_utf8_lossy(&buffer).to_string();
        let _ = sender.send(head);

        let length = STREAM.len();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
             Content-Length: {length}\r\nConnection: close\r\n\r\n{STREAM}"
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    });
    (format!("http://{addr}/v1"), receiver)
}

fn run_once(user_agent: &str) -> String {
    let (base_url, receiver) = spawn_probe();
    let runtime = OpenAiChatRuntime::new(ChatEndpoint {
        base_url,
        api_key: "test-key".to_string(),
        user_agent: user_agent.to_string(),
    });
    let messages = vec![ConversationMessage {
        role: Role::User,
        blocks: vec![MessageBlock::Text(TextBlock::new("你好"))],
        reasoning: String::new(),
        tools: Vec::new(),
    }];
    let tools = Vec::new();
    let options = GenerationOptions::default();
    let identity = std::collections::BTreeMap::new();
    let input = omnicrawl_llm::ChatRequestInput {
        model: "test-model",
        system_prompt: "",
        messages: &messages,
        tools: &tools,
        options: &options,
        profile_request_timeout_seconds: 30.0,
        prompt_cache_capable: false,
        prompt_cache_identity: &identity,
    };
    let _ = runtime.run_turn(&input, &mut DiscardSink);
    receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("服务端应当收到请求头")
}

fn user_agent_of(head: &str) -> Option<String> {
    head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("user-agent")
            .then(|| value.trim().to_string())
    })
}

/// 未配置渠道 UA 时报产品身份，版本号取自 `packages/cli/package.json`。
#[test]
fn unconfigured_endpoint_reports_the_product_user_agent() {
    let head = run_once("");
    assert_eq!(
        user_agent_of(&head).as_deref(),
        Some(omnicrawl_version::USER_AGENT),
        "未配置时必须报 omnicrawl/<版本>：{head}"
    );
    assert!(
        omnicrawl_version::USER_AGENT.starts_with("omnicrawl/"),
        "UA 形状是 omnicrawl/<版本>：{}",
        omnicrawl_version::USER_AGENT
    );
}

/// 渠道显式配置了 `user_agent` 时以配置为准（聚合网关常要求自己的标识）。
#[test]
fn configured_user_agent_still_wins() {
    let head = run_once("custom-agent/9.9");
    assert_eq!(user_agent_of(&head).as_deref(), Some("custom-agent/9.9"));
}
