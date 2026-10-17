//! 结构化决策请求的线格式：把 `state` + `questions` 组装成请求体，并从响应里取回 `answers`。
//!
//! 三种请求方式（`decision_models.toml` 的 `mode`）共享同一套 state / questions / answers
//! 语义，差异只在这一层：
//! * `jev`：请求体是 `{model, state, questions}`；响应顶层的 `answers` 就是答案。
//! * `onejev`：自部署的 OneJev 服务，请求体与响应形状与 `jev` 相同，**传输走本机管道**
//!   （命名管道 / Unix socket，见 [`crate::decision_pipe`]），不经 HTTP；只有指向非回环
//!   地址时（云端网关也提供同一协议形状）才按 `POST {base_url}/v1/systemone` 走网络。
//! * `chat_completions`：请求体走 OpenAI 兼容的 `messages`——state + questions 作为一条 user
//!   消息的 JSON 文本，并带 `response_format` 要求 JSON 输出；答案从
//!   `choices[0].message.content` 里解析出同一形状的 `answers`。该方式的 `base_url` 已含
//!   `/v1`（与 `[image_gen]` 的 OpenAI 兼容口径一致），因此只追加
//!   `/chat/completions`。
//!
//! 因此三个调用点（工具调用审查、检索重排、提问托管）只管按 mode 组装请求体与读回
//! `answers`，其余解析逻辑两种方式共用。

use omnicrawl_config::features::decision_model::{
    is_loopback_url, DECISION_MODE_CHAT_COMPLETIONS, DECISION_MODE_ONEJEV,
};
use serde_json::{json, Map, Value};

/// `chat_completions` 方式的输出约束：只要一个 JSON 对象，形状与 questions 对齐。
const CHAT_SYSTEM_PROMPT: &str = "你是结构化决策服务：按 user 消息里 JSON 的 questions 逐项判定，\
只输出一个 JSON 对象 {\"answers\": {...}}，键与 questions 的键一致，不要输出任何其他文字。";

/// 决策接口地址：按请求方式选路径（基地址末尾斜杠在此收口）。
///
/// `chat_completions` 的基地址按 OpenAI 兼容口径填到 `/v1`（同 `[image_gen]`），
/// 因此只追加资源路径；`jev` 与 `onejev` 的基地址是站点根下的 API 前缀，各自补 `/v1/...`。
pub fn decide_url(mode: &str, base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if mode == DECISION_MODE_CHAT_COMPLETIONS {
        format!("{base}/chat/completions")
    } else if mode == DECISION_MODE_ONEJEV {
        // 自部署的 OneJev 服务：TypeSafe 兼容接口（与云端 jev 同形状、不同路径）。
        format!("{base}/v1/systemone")
    } else {
        format!("{base}/v1/decide")
    }
}

/// 该请求方式是否需要凭据：自部署的本地服务（`onejev`）不鉴权，其余一律要。
pub fn requires_api_key(mode: &str) -> bool {
    mode != DECISION_MODE_ONEJEV
}

/// 该渠道是否走本机直连（自部署的 OneJev 服务，地址落在回环上）。
///
/// `onejev` 是「本机判定服务」的协议形状，但云端网关（new-api 一类）也提供同一形状的
/// 接口，因此只有回环地址才切成管道；非回环仍走 HTTP。
pub fn uses_local_endpoint(mode: &str, base_url: &str) -> bool {
    mode == DECISION_MODE_ONEJEV && is_loopback_url(base_url)
}

/// 发一次决策请求（唯一入口）：本机自部署走管道直连，其余走 HTTP。
///
/// 上层调用点因此不必区分两种传输——请求体与响应形状完全一致，只有「怎么送到」不同。
pub fn post_decision(
    mode: &str,
    base_url: &str,
    api_key: &str,
    body_text: &str,
    timeout_seconds: u64,
    user_agent: &str,
    retry_count: u64,
) -> Result<String, String> {
    if uses_local_endpoint(mode, base_url) {
        // 本机端点：连接失败说明服务没跑，重试没有意义（等待窗口在连接里）。
        let environment = omnicrawl_config::core::runtime::ConfigEnvironment::from_process();
        let endpoint = omnicrawl_config::features::decision_model::onejev_endpoint(&environment);
        return crate::decision_pipe::post_body(&endpoint, body_text, timeout_seconds);
    }
    post_body(
        &decide_url(mode, base_url),
        api_key,
        body_text,
        timeout_seconds,
        user_agent,
        retry_count,
    )
}

/// 组装一次决策请求的请求体。
pub fn request_body(mode: &str, model: &str, state: &Value, questions: &Value) -> Value {
    if mode == DECISION_MODE_CHAT_COMPLETIONS {
        let payload = json!({"state": state, "questions": questions}).to_string();
        return json!({
            "model": model,
            "messages": [
                {"role": "system", "content": CHAT_SYSTEM_PROMPT},
                {"role": "user", "content": payload},
            ],
            "response_format": {"type": "json_object"},
        });
    }
    json!({"model": model, "state": state, "questions": questions})
}

/// 决策请求失败后的默认重试次数（不含首次请求）。
pub const DEFAULT_RETRY_COUNT: u64 = 3;
/// 单次重试的退避基数（毫秒）：第 n 次重试前等 `BASE << (n-1)`，上限 [`MAX_RETRY_BACKOFF_MILLIS`]。
const BASE_RETRY_BACKOFF_MILLIS: u64 = 200;
/// 退避上限（毫秒）：决策在回合路径上同步等待，重试不能让用户等太久。
const MAX_RETRY_BACKOFF_MILLIS: u64 = 2_000;

/// 发一次决策出站请求（失败按 `retry_count` 重试）：返回响应正文；连接失败与 4xx/5xx 都回
/// `Err`（正文带进错误便于诊断）。
///
/// 这是决策出站的唯一传输落点——工具调用审查、检索重排、提问托管与本地 REST 决策接口
/// 都经它发出，因此超时、鉴权头、`User-Agent` 与「状态码不转错误、正文留给诊断」的口径
/// 只有一份实现。请求体由调用方给（脱敏发生在构造请求体之后、发送之前）。
///
/// 失败按次数重试（`retry_count` 不含首次请求）：网关抖动、限流、瞬时被拒都不该让一次决策
/// 白等。**4xx 不重试**——那是请求本身的问题，原样再发一次还是同一个错。
pub fn post_body(
    decide_url: &str,
    api_key: &str,
    body_text: &str,
    timeout_seconds: u64,
    user_agent: &str,
    retry_count: u64,
) -> Result<String, String> {
    let mut attempt = 0u64;
    loop {
        match post_body_once(decide_url, api_key, body_text, timeout_seconds, user_agent) {
            Ok(text) => return Ok(text),
            Err(error) => {
                if attempt >= retry_count || !retryable_failure(&error) {
                    return Err(error);
                }
                attempt += 1;
                let backoff = (BASE_RETRY_BACKOFF_MILLIS << (attempt - 1).min(4))
                    .min(MAX_RETRY_BACKOFF_MILLIS);
                std::thread::sleep(std::time::Duration::from_millis(backoff));
            }
        }
    }
}

/// 该失败是否值得重试：连接/超时一类传输失败与上游 5xx 重试；4xx 与响应不可解析不重试。
fn retryable_failure(error: &str) -> bool {
    match http_status_of(error) {
        Some(status) => status >= 500,
        None => true,
    }
}

/// 从 `post_body_once` 的错误文案里取出 HTTP 状态码；不是 HTTP 错误时返回 `None`。
fn http_status_of(error: &str) -> Option<u16> {
    error
        .split_once("HTTP ")
        .and_then(|(_, rest)| rest.split(|character: char| !character.is_ascii_digit()).next())
        .and_then(|digits| digits.parse().ok())
}

/// 发一次决策请求（不含重试）。
fn post_body_once(
    decide_url: &str,
    api_key: &str,
    body_text: &str,
    timeout_seconds: u64,
    user_agent: &str,
) -> Result<String, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        // 状态码不转错误：上游 4xx/5xx 的正文要留给诊断。
        .http_status_as_error(false)
        .build()
        .into();
    let timeout = std::time::Duration::from_secs(timeout_seconds.max(1));
    let response = agent
        .post(decide_url)
        .config()
        .timeout_connect(Some(timeout))
        .timeout_recv_response(Some(timeout))
        .timeout_recv_body(Some(timeout))
        .build()
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {api_key}"))
        .header("User-Agent", user_agent)
        .send(body_text)
        .map_err(|error| error.to_string())?;
    let status = response.status().as_u16();
    let text = response
        .into_body()
        .read_to_string()
        .map_err(|error| error.to_string())?;
    if status >= 400 {
        return Err(format!(
            "决策服务返回 HTTP {status}：{}",
            truncate(text.trim())
        ));
    }
    Ok(text)
}

/// 从响应体里取出 `answers` 对象；取不到时返回可直接展示的原因。
pub fn extract_answers(mode: &str, text: &str) -> Result<Value, String> {
    let payload = parse_json(text)?;
    if mode == DECISION_MODE_CHAT_COMPLETIONS {
        let content = payload
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!(
                    "对话补全响应缺少 choices[0].message.content：{}",
                    truncate(text)
                )
            })?;
        let body = parse_json(strip_code_fence(content))?;
        // 允许模型少包一层：顶层就是 answers 时直接当答案用。
        return Ok(match body.get("answers") {
            Some(answers) => answers.clone(),
            None => body,
        });
    }
    payload
        .get("answers")
        .cloned()
        .ok_or_else(|| format!("决策服务响应缺少 answers：{}", truncate(text)))
}

fn parse_json(text: &str) -> Result<Value, String> {
    serde_json::from_str(text).map_err(|error| format!("决策服务响应不是 JSON：{error}"))
}

/// 候选项在 `criteria` 里的默认键前缀；键即「候选下标」的稳定写法。
///
/// 决策模型只认键，因此调用方用「前缀 + 下标」构造候选项，读回时按同一规则反解——
/// 审查、检索重排、提问托管与决策 REST 服务共用这一套键。
pub const OPTION_KEY_PREFIX: &str = "o";

/// 按候选项下标构造请求里的键（`o0`、`o1`…）。
pub fn option_key(index: usize) -> String {
    format!("{OPTION_KEY_PREFIX}{index}")
}

/// 按给定前缀构造候选项键（重排用 `c0`、`c1`…，与托管区分开便于日志辨识）。
pub fn option_key_with(prefix: &str, index: usize) -> String {
    format!("{prefix}{index}")
}

/// 把 `criteria` 的键按前缀反解成候选下标；未知键返回 `None`。
pub fn option_index_with(prefix: &str, key: &str) -> Option<usize> {
    key.strip_prefix(prefix)?.parse::<usize>().ok()
}

/// 从 `answers.<question_id>` 读回胜出候选的下标（读 `choice`，缺失时退化成置信度最高项）。
///
/// 是所有 choice 类提问的共用读法：答案被约束在给定候选项里，因此调用方只需要一个下标，
/// 不必解析自由文本。越界或未知键一律忽略。
pub fn parse_choice_index(
    mode: &str,
    text: &str,
    question_id: &str,
    count: usize,
    prefix: &str,
) -> Result<usize, String> {
    let answers = extract_answers(mode, text)?;
    let answer = answers.get(question_id);
    if let Some(index) = answer
        .and_then(|value| value.get("choice"))
        .and_then(Value::as_str)
        .and_then(|key| option_index_with(prefix, key))
        .filter(|index| *index < count)
    {
        return Ok(index);
    }
    let best = answer
        .and_then(|value| value.get("probabilities"))
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| Some((option_index_with(prefix, key)?, value.as_f64()?)))
        .filter(|(index, _)| *index < count)
        .max_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(index, _)| index);
    best.ok_or_else(|| format!("决策模型未返回可用选项：{}", truncate(text.trim())))
}

/// 从 `answers.<question_id>` 读回候选项的完整排序（按置信度降序；没有逐项置信度时用胜出项排首位）。
///
/// 返回的是输入候选的下标序列，长度恒为 `count`：读不到的候选按原顺序接在后面，
/// 因此调用方可以直接按它重排。
pub fn parse_ranking_order(
    mode: &str,
    text: &str,
    question_id: &str,
    count: usize,
    prefix: &str,
) -> Result<Vec<usize>, String> {
    let answers = extract_answers(mode, text)?;
    let answer = answers.get(question_id);
    let mut ranked: Vec<(f64, usize)> = Vec::new();
    if let Some(probabilities) = answer
        .and_then(|value| value.get("probabilities"))
        .and_then(Value::as_object)
    {
        for (key, value) in probabilities {
            let Some(index) = option_index_with(prefix, key) else {
                continue;
            };
            if index >= count {
                continue;
            }
            let Some(probability) = value.as_f64() else {
                continue;
            };
            ranked.push((probability, index));
        }
    }
    if ranked.is_empty() {
        // 没有逐项置信度时退化用胜出项：它排第一，其余保持原顺序。
        let winner = answer
            .and_then(|value| value.get("choice"))
            .and_then(Value::as_str)
            .and_then(|key| option_index_with(prefix, key))
            .filter(|index| *index < count)
            .ok_or_else(|| {
                format!(
                    "决策模型未返回可用的候选项排序：{}",
                    truncate(text.trim())
                )
            })?;
        ranked.push((1.0, winner));
    }
    ranked.sort_by(|left, right| right.0.total_cmp(&left.0).then(left.1.cmp(&right.1)));

    let mut order: Vec<usize> = Vec::with_capacity(count);
    for (_, index) in ranked {
        if !order.contains(&index) {
            order.push(index);
        }
    }
    for index in 0..count {
        if !order.contains(&index) {
            order.push(index);
        }
    }
    Ok(order)
}

/// 组一个 choice 提问：`instructions` 是判定说明，`criteria` 是「候选项键 → 判定标准」。
pub fn choice_question(instructions: &str, criteria: Map<String, Value>) -> Value {
    json!({
        "type": "choice",
        "instructions": instructions,
        "criteria": criteria,
    })
}

/// 进请求前的文本收敛：空白压成单空格，超长截断。
///
/// 决策只是判个大小，不需要整段正文；调用方各自的上限不同（候选项 400、上下文 1200），
/// 因此上限由调用方给，收敛规则收在这里。
pub fn collapse_text(text: &str, limit: usize) -> String {
    truncate_to(&text.split_whitespace().collect::<Vec<_>>().join(" "), limit)
}

/// 剥掉模型偶尔带上的 Markdown 代码围栏（```json ... ```）。
fn strip_code_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    // 去掉语言标记那一行（若是 ```json 形式）。
    let rest = match rest.split_once('\n') {
        Some((_language, body)) => body,
        None => rest,
    };
    rest.trim_end().trim_end_matches("```").trim()
}

fn truncate(text: &str) -> String {
    truncate_to(text.trim(), 200)
}

/// 按字符截断到 `limit`，超出时以省略号收尾。
fn truncate_to(text: &str, limit: usize) -> String {
    let characters: Vec<char> = text.chars().collect();
    if characters.len() <= limit {
        return characters.iter().collect();
    }
    let head: String = characters[..limit].iter().collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnicrawl_config::features::decision_model::DECISION_MODE_JEV;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const JEV: &str = DECISION_MODE_JEV;

    /// 一个只回固定状态码的假上游；计数收到的请求次数。
    fn flaky_upstream(statuses: Vec<u16>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let port = listener.local_addr().expect("本地地址").port();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let index = seen.fetch_add(1, Ordering::SeqCst);
                let status = statuses.get(index).copied().unwrap_or(503);
                let mut reader = BufReader::new(stream.try_clone().expect("克隆流"));
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let mut length = 0usize;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 {
                        break;
                    }
                    let text = header.trim_end();
                    if text.is_empty() {
                        break;
                    }
                    if let Some((key, value)) = text.split_once(':') {
                        if key.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap_or(0);
                        }
                    }
                }
                let mut body = vec![0u8; length];
                let _ = reader.read_exact(&mut body);
                let payload = if status >= 400 {
                    "{\"error\":\"上游抖动\"}".to_string()
                } else {
                    "{\"answers\":{\"q\":{\"choice\":\"o0\"}}}".to_string()
                };
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        (format!("http://127.0.0.1:{port}"), calls)
    }

    #[test]
    fn transient_failures_are_retried_up_to_the_configured_count() {
        // 两次 503 + 一次 200：retry_count = 2 时应当自愈。
        let (base_url, calls) = flaky_upstream(vec![503, 503, 200]);
        let text = post_body(
            &format!("{base_url}/v1/decide"),
            "jv_test",
            "{}",
            5,
            "test-agent",
            2,
        )
        .expect("重试后可拿到响应");
        assert!(text.contains("answers"), "{text}");
        assert_eq!(calls.load(Ordering::SeqCst), 3, "两次失败 + 一次成功");
    }

    #[test]
    fn retries_are_bounded_and_the_last_error_is_reported() {
        let (base_url, calls) = flaky_upstream(vec![500, 500, 500, 500, 500]);
        let error = post_body(
            &format!("{base_url}/v1/decide"),
            "jv_test",
            "{}",
            5,
            "test-agent",
            2,
        )
        .expect_err("重试耗尽后仍要如实报错");
        assert!(error.contains("HTTP 500"), "{error}");
        assert_eq!(calls.load(Ordering::SeqCst), 3, "首次 + 两次重试");
    }

    #[test]
    fn client_errors_are_not_retried() {
        // 4xx 是请求本身的问题，原样再发一次还是同一个错。
        let (base_url, calls) = flaky_upstream(vec![400, 200]);
        let error = post_body(
            &format!("{base_url}/v1/decide"),
            "jv_test",
            "{}",
            5,
            "test-agent",
            3,
        )
        .expect_err("400 不做重试");
        assert!(error.contains("HTTP 400"), "{error}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "只发一次");
    }

    #[test]
    fn zero_retries_sends_exactly_one_request() {
        let (base_url, calls) = flaky_upstream(vec![503]);
        assert!(post_body(
            &format!("{base_url}/v1/decide"),
            "jv_test",
            "{}",
            5,
            "test-agent",
            0,
        )
        .is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn retry_predicate_splits_server_errors_from_client_errors() {
        assert!(retryable_failure("决策服务返回 HTTP 503：上游暂时不可用"));
        assert!(retryable_failure("决策服务返回 HTTP 500：内部错误"));
        assert!(!retryable_failure("决策服务返回 HTTP 400：参数不对"));
        assert!(!retryable_failure("决策服务返回 HTTP 401：未授权"));
        // 连接失败一类没有状态码，按可重试处理。
        assert!(retryable_failure("连接被拒绝：connection refused"));
        assert_eq!(http_status_of("决策服务返回 HTTP 503：上游暂时不可用"), Some(503));
        assert_eq!(http_status_of("连接被拒绝：connection refused"), None);
    }

    #[test]
    fn decide_url_follows_the_request_mode() {
        assert_eq!(
            decide_url(DECISION_MODE_JEV, "https://jevtypesafeai.com/api/"),
            "https://jevtypesafeai.com/api/v1/decide"
        );
        assert_eq!(
            decide_url(DECISION_MODE_CHAT_COMPLETIONS, "https://api.openlux.ai/v1/"),
            "https://api.openlux.ai/v1/chat/completions"
        );
        assert_eq!(
            decide_url(DECISION_MODE_ONEJEV, "http://127.0.0.1:8766/"),
            "http://127.0.0.1:8766/v1/systemone"
        );
    }

    #[test]
    fn onejev_uses_the_native_body_and_top_level_answers() {
        // 自部署与云端原生方式共用请求体形状：只有路径不同。
        let body = request_body(
            DECISION_MODE_ONEJEV,
            "OneJev-0.8B",
            &json!({"task": "支付账单"}),
            &json!({"next": {"type": "choice"}}),
        );
        assert_eq!(body["model"], "OneJev-0.8B");
        assert_eq!(body["state"]["task"], "支付账单");
        assert_eq!(body["questions"]["next"]["type"], "choice");
        assert!(body.get("messages").is_none(), "自部署不带 messages");

        let response = json!({
            "model": "OneJev-0.8B",
            "answers": {"next": {"type": "choice", "choice": "click", "confidence": 0.9}}
        })
        .to_string();
        assert_eq!(
            extract_answers(DECISION_MODE_ONEJEV, &response).expect("自部署响应可解析")["next"]
                ["choice"],
            "click"
        );
        assert!(
            extract_answers(DECISION_MODE_ONEJEV, "{}").is_err(),
            "缺 answers 一律拒绝"
        );
    }

    #[test]
    fn jev_body_keeps_the_native_shape() {
        let body = request_body(
            DECISION_MODE_JEV,
            "jev-latest",
            &json!({"ticket": "空白页"}),
            &json!({"urgency": {"type": "choice"}}),
        );
        assert_eq!(body["model"], "jev-latest");
        assert_eq!(body["state"]["ticket"], "空白页");
        assert_eq!(body["questions"]["urgency"]["type"], "choice");
        assert!(body.get("messages").is_none(), "原生方式不带 messages");
    }

    #[test]
    fn chat_completions_body_wraps_state_and_questions_into_messages() {
        let body = request_body(
            DECISION_MODE_CHAT_COMPLETIONS,
            "jev-1.13.0",
            &json!({"ticket": "空白页"}),
            &json!({"urgency": {"type": "choice"}}),
        );
        assert_eq!(body["model"], "jev-1.13.0");
        assert_eq!(body["response_format"]["type"], "json_object");
        let messages = body["messages"].as_array().expect("messages 是数组");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        // user 消息里就是 state + questions 的原样 JSON 文本。
        let payload: Value =
            serde_json::from_str(messages[1]["content"].as_str().expect("content 是文本"))
                .expect("content 是 JSON");
        assert_eq!(payload["state"]["ticket"], "空白页");
        assert_eq!(payload["questions"]["urgency"]["type"], "choice");
    }

    #[test]
    fn answers_come_from_each_mode_own_place() {
        let native = json!({"answers": {"q": {"choice": "a"}}}).to_string();
        assert_eq!(
            extract_answers(DECISION_MODE_JEV, &native).expect("原生方式可解析")["q"]["choice"],
            "a"
        );

        let chat = json!({
            "choices": [{"message": {"content": "{\"answers\": {\"q\": {\"choice\": \"b\"}}}"}}]
        })
        .to_string();
        assert_eq!(
            extract_answers(DECISION_MODE_CHAT_COMPLETIONS, &chat).expect("对话补全可解析")["q"]
                ["choice"],
            "b"
        );
    }

    #[test]
    fn chat_completions_tolerates_fences_and_missing_answers_wrapper() {
        let fenced = json!({
            "choices": [{"message": {"content": "```json\n{\"q\": {\"choice\": \"c\"}}\n```"}}]
        })
        .to_string();
        assert_eq!(
            extract_answers(DECISION_MODE_CHAT_COMPLETIONS, &fenced).expect("剥围栏后可解析")["q"]
                ["choice"],
            "c"
        );
    }

    #[test]
    fn unusable_responses_are_rejected() {
        assert!(extract_answers(DECISION_MODE_JEV, "not json").is_err());
        assert!(extract_answers(DECISION_MODE_JEV, "{}").is_err());
        assert!(extract_answers(DECISION_MODE_CHAT_COMPLETIONS, "{}").is_err());
        let bad_content = json!({"choices": [{"message": {"content": "not json"}}]}).to_string();
        assert!(extract_answers(DECISION_MODE_CHAT_COMPLETIONS, &bad_content).is_err());
    }

    #[test]
    fn choice_index_reads_the_key_as_an_index() {
        let text = json!({
            "answers": {"pick": {"type": "choice", "choice": "o2", "confidence": 0.8}}
        })
        .to_string();
        assert_eq!(parse_choice_index(JEV, &text, "pick", 3, "o").expect("可解析"), 2);

        // 前缀不匹配的键不算数（重排的 `c0` 与托管的 `o0` 各自独立）。
        assert!(parse_choice_index(JEV, &text, "pick", 3, "c").is_err());

        // 没有 choice 时取概率最高的一项。
        let probabilities = json!({
            "answers": {"pick": {
                "type": "choice",
                "probabilities": {"o0": 0.1, "o1": 0.7, "o2": 0.2}
            }}
        })
        .to_string();
        assert_eq!(
            parse_choice_index(JEV, &probabilities, "pick", 3, "o").expect("可解析"),
            1
        );
    }

    #[test]
    fn unusable_choice_responses_are_rejected() {
        assert!(parse_choice_index(JEV, "not json", "pick", 2, "o").is_err());
        let empty = json!({"answers": {"pick": {"type": "choice"}}}).to_string();
        assert!(parse_choice_index(JEV, &empty, "pick", 2, "o").is_err());
        // 越界或未知键一律忽略；全都取不到时按不可用处理。
        let out_of_range =
            json!({"answers": {"pick": {"choice": "o9", "probabilities": {"o9": 0.9}}}}).to_string();
        assert!(parse_choice_index(JEV, &out_of_range, "pick", 2, "o").is_err());
    }

    #[test]
    fn ranking_order_covers_every_candidate() {
        let text = json!({
            "answers": {"rank": {"type": "choice",
                "probabilities": {"c0": 0.1, "c2": 0.9, "c1": 0.4}}}
        })
        .to_string();
        // 置信度降序：c2（0.9）→ c1（0.4）→ c0（0.1）。
        assert_eq!(
            parse_ranking_order(JEV, &text, "rank", 3, "c").expect("可解析"),
            vec![2, 1, 0]
        );

        // 没有逐项置信度：胜出项排第一，其余按原顺序补齐。
        let winner = json!({"answers": {"rank": {"choice": "c2"}}}).to_string();
        assert_eq!(
            parse_ranking_order(JEV, &winner, "rank", 3, "c").expect("可解析"),
            vec![2, 0, 1]
        );

        // 读不到的候选也要补齐，长度恒为 count。
        let partial = json!({"answers": {"rank": {"probabilities": {"c1": 0.5}}}}).to_string();
        assert_eq!(
            parse_ranking_order(JEV, &partial, "rank", 3, "c").expect("可解析"),
            vec![1, 0, 2]
        );
        assert!(parse_ranking_order(JEV, "{}", "rank", 3, "c").is_err());
    }
}
