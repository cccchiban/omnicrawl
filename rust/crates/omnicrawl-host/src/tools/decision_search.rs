//! 检索重排：把记忆搜索与知识库检索的候选交给结构化决策模型按相关度排序。
//!
//! 可选项、默认关闭（`decision_models.toml` 的 `[features]` 里两个工具各一个开关）。开启后本地
//! 检索先取一份更宽的候选池，再向决策服务提一个 choice 问题，按各候选项的置信度
//! （`answers.<id>.probabilities`）降序排列，最后按调用方要求的 `max_results` 截断——排序更准，
//! 返回条数仍由调用方决定。
//!
//! **失败一律 fail-open**：开关没开、没有可用决策渠道、缺凭据、请求失败、响应不可解析都退回
//! 本地排序结果，检索工具不会因为决策服务不可用而失败（与审查通道的 fail-closed 语义相反，
//! 是本功能刻意选的：检索少几条比检索直接失败代价小）。
//!
//! 出网脱敏复用审查通道那套 `[desensitization]` 旁路（[`crate::review::masking_from_config`]）：
//! 候选内容同样屏蔽后再外发；脱敏构造失败按「重排不可用」处理，绝不外发原文。

use std::sync::Arc;
use std::time::Duration;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::decision_model::{
    load_decision_model_configuration, load_decision_switches, DecisionChannelConfig,
};
use serde_json::{json, Map, Value};

use crate::review::{masking_from_config, ReviewMasking};

/// 送给决策模型排序的候选上限：候选越多越准，请求体、延迟与费用也越大。
pub const RERANK_CANDIDATE_LIMIT: usize = 20;
/// 单个候选项进请求前截断到的字符数（只留判断相关度所需的部分）。
const CANDIDATE_MAX_CHARS: usize = 400;
/// 一次重排请求的超时（决策服务本身是 70–500ms 量级，余量留给网络）。
const RERANK_TIMEOUT_SECONDS: u64 = 20;
const USER_AGENT: &str = "omnicrawl-search-rerank/0.0.1";
/// 提问 ID；候选项键就是它在 `criteria` 里的下标（`c0`、`c1`…），便于把回答映射回来。
const QUESTION_ID: &str = "most_relevant";
const CANDIDATE_KEY_PREFIX: &str = "c";

/// 重排用的决策渠道：与审查通道同源（`decision_models.toml` 的默认决策渠道）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RerankChannel {
    /// 请求方式（`jev` / `chat_completions`）。
    pub mode: String,
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub api_key_env: String,
}

impl RerankChannel {
    fn from_config(channel: &DecisionChannelConfig) -> Self {
        Self {
            mode: channel.mode.clone(),
            model: channel.model.clone(),
            base_url: channel.base_url.clone(),
            api_key: channel.api_key.clone(),
            api_key_env: channel.api_key_env.clone(),
        }
    }

    /// 决策接口地址（对应 `DecisionChannelConfig::decide_url`）。
    pub fn decide_url(&self) -> String {
        crate::decision_wire::decide_url(&self.mode, &self.base_url)
    }

    /// 生效的内联凭据：只认配置里的明文密钥。
    pub fn resolve_api_key(&self) -> String {
        self.api_key.trim().to_string()
    }
}

/// 一次排序请求的全部输入。
pub struct RerankRequest<'a> {
    pub channel: &'a RerankChannel,
    /// 出网脱敏旁路；`None` 表示脱敏未启用。
    pub masking: Option<&'a ReviewMasking>,
    /// 交给决策模型的上下文（查询、过滤条件等）。
    pub state: &'a Value,
    pub instructions: &'a str,
    /// 候选描述；下标即返回顺序里的候选下标。
    pub candidates: &'a [String],
}

/// 排序执行器：真实实现走决策服务；测试注入桩以便不起网络。
pub trait RerankClient: Send + Sync {
    /// 按相关度降序返回候选下标；`Err` 表示本次重排不可用（调用方 fail-open）。
    fn rank(&self, request: &RerankRequest<'_>) -> Result<Vec<usize>, String>;
}

/// 检索重排的运行期：一个工具一份（记忆搜索、知识库检索各一个开关）。
#[derive(Clone)]
pub struct RerankOptions {
    /// 该工具的开关状态。
    pub enabled: bool,
    /// 默认决策渠道；`None` 表示没有可用渠道。
    pub channel: Option<RerankChannel>,
    pub masking: Option<Arc<ReviewMasking>>,
    /// 排序执行器（测试注入桩）。
    pub client: Arc<dyn RerankClient>,
}

impl Default for RerankOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            channel: None,
            masking: None,
            client: Arc::new(DecisionRerankClient),
        }
    }
}

impl std::fmt::Debug for RerankOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 脱敏旁路与执行器都是 trait 对象，只报「有没有」。
        formatter
            .debug_struct("RerankOptions")
            .field("enabled", &self.enabled)
            .field("channel", &self.channel)
            .field("masking", &self.masking.is_some())
            .finish()
    }
}

impl RerankOptions {
    /// 是否真正可用（开关打开且已选中决策渠道）。
    pub fn active(&self) -> bool {
        self.enabled && self.channel.is_some()
    }
}

/// 按配置装配某个工具的重排运行期（TUI 与 API 共用同一口径）。
pub fn rerank_options_from_config(
    environment: &ConfigEnvironment,
    switch_key: &str,
) -> RerankOptions {
    if !load_decision_switches(environment, None)
        .get(switch_key)
        .copied()
        .unwrap_or(false)
    {
        // 关掉时连配置都不读：重排是个纯加速项，不该在启动路径上多花一次磁盘往返。
        return RerankOptions::default();
    }
    let channel = load_decision_model_configuration(environment, None)
        .ok()
        .and_then(|configuration| configuration.active_channel().cloned())
        .map(|channel| RerankChannel::from_config(&channel));
    RerankOptions {
        enabled: true,
        channel,
        masking: masking_from_config(environment).map(Arc::new),
        ..RerankOptions::default()
    }
}

/// 按决策模型给出的相关度重排候选；`None` 表示本次重排不可用，调用方保留本地顺序（fail-open）。
pub fn reranked_order(
    options: &RerankOptions,
    state: Value,
    instructions: &str,
    candidates: &[String],
) -> Option<Vec<usize>> {
    if !options.active() || candidates.len() < 2 {
        return None;
    }
    let channel = options.channel.as_ref()?;
    let request = RerankRequest {
        channel,
        masking: options.masking.as_deref(),
        state: &state,
        instructions,
        candidates,
    };
    match options.client.rank(&request) {
        Ok(order) => Some(order),
        Err(error) => {
            eprintln!("[host] 检索重排不可用，改用本地排序：{error}");
            None
        }
    }
}

/// 按重排给出的下标重排条目；`order` 之外的条目按原顺序接在后面。
pub fn apply_order<T>(items: Vec<T>, order: &[usize]) -> Vec<T> {
    let mut slots: Vec<Option<T>> = items.into_iter().map(Some).collect();
    let mut reordered: Vec<T> = Vec::with_capacity(slots.len());
    for index in order {
        if let Some(slot) = slots.get_mut(*index) {
            if let Some(item) = slot.take() {
                reordered.push(item);
            }
        }
    }
    for slot in slots.into_iter().flatten() {
        reordered.push(slot);
    }
    reordered
}

/// 真实实现：`POST {base_url}/v1/decide`，一个 choice 问题，读回各候选项的置信度。
struct DecisionRerankClient;

impl RerankClient for DecisionRerankClient {
    fn rank(&self, request: &RerankRequest<'_>) -> Result<Vec<usize>, String> {
        let channel = request.channel;
        let api_key = channel.resolve_api_key();
        if api_key.is_empty() && crate::decision_wire::requires_api_key(&channel.mode) {
            return Err(format!(
                "缺少 API Key：请设置环境变量 {}。",
                channel.api_key_env
            ));
        }
        let criteria: Map<String, Value> = request
            .candidates
            .iter()
            .enumerate()
            .map(|(index, text)| {
                (
                    format!("{CANDIDATE_KEY_PREFIX}{index}"),
                    Value::String(candidate_text(text)),
                )
            })
            .collect();
        let body = crate::decision_wire::request_body(
            &channel.mode,
            &channel.model,
            request.state,
            &json!({
                QUESTION_ID: {
                    "type": "choice",
                    "instructions": request.instructions,
                    "criteria": criteria,
                }
            }),
        );
        let body_text = serde_json::to_string(&body)
            .map_err(|error| format!("构造重排请求失败：{error}"))?;
        // 脱敏旁路：屏蔽失败即放弃本次重排（不外发原文），与「失败一律 fail-open」一致。
        let body_text = match request.masking {
            Some(masking) => {
                let mut masker =
                    (masking.factory)().map_err(|error| format!("脱敏不可用：{error}"))?;
                let masked = masker.mask(&body_text);
                masker.close();
                masked
            }
            None => body_text,
        };

        let timeout = Duration::from_secs(RERANK_TIMEOUT_SECONDS);
        let agent: ureq::Agent = ureq::Agent::config_builder()
            // 状态码不转错误：上游 4xx/5xx 的正文要留给诊断。
            .http_status_as_error(false)
            .build()
            .into();
        let response = agent
            .post(channel.decide_url())
            .config()
            .timeout_connect(Some(timeout))
            .timeout_recv_response(Some(timeout))
            .timeout_recv_body(Some(timeout))
            .build()
            .header("Content-Type", "application/json")
            .header("Authorization", &format!("Bearer {api_key}"))
            .header("User-Agent", USER_AGENT)
            .send(body_text.as_str())
            .map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        let text = response.into_body().read_to_string().map_err(|error| error.to_string())?;
        if status >= 400 {
            return Err(format!(
                "决策服务返回 HTTP {status}：{}",
                truncate(text.trim(), 200)
            ));
        }
        parse_ranking(&text, &channel.mode, request.candidates.len())
    }
}

/// 解析答案里的候选项排序：先按置信度降序，取不到的候选按本地顺序补在后面。
fn parse_ranking(text: &str, mode: &str, count: usize) -> Result<Vec<usize>, String> {
    let answers = crate::decision_wire::extract_answers(mode, text)?;
    let answer = answers.get(QUESTION_ID);
    let mut ranked: Vec<(f64, usize)> = Vec::new();
    if let Some(probabilities) = answer
        .and_then(|value| value.get("probabilities"))
        .and_then(Value::as_object)
    {
        for (key, value) in probabilities {
            let Some(index) = candidate_index(key) else {
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
        // 没有逐项置信度时退化用胜出项：它排第一，其余保持本地顺序。
        let winner = answer
            .and_then(|value| value.get("choice"))
            .and_then(Value::as_str)
            .and_then(candidate_index)
            .filter(|index| *index < count)
            .ok_or_else(|| {
                format!("决策模型未返回可用的候选项排序：{}", truncate(text.trim(), 200))
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

fn candidate_index(key: &str) -> Option<usize> {
    key.strip_prefix(CANDIDATE_KEY_PREFIX)?.parse::<usize>().ok()
}

/// 候选项进请求前的收敛：空白压成单空格，超长截断（相关度判断不需要整段正文）。
fn candidate_text(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&collapsed, CANDIDATE_MAX_CHARS)
}

fn truncate(text: &str, limit: usize) -> String {
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
    use omnicrawl_config::features::decision_model::{
        DECISION_MODE_CHAT_COMPLETIONS, DECISION_MODE_JEV,
    };

    const JEV: &str = DECISION_MODE_JEV;
    const CHAT: &str = DECISION_MODE_CHAT_COMPLETIONS;

    fn channel(base_url: &str) -> RerankChannel {
        RerankChannel {
            mode: JEV.to_string(),
            model: "jev-latest".to_string(),
            base_url: base_url.to_string(),
            api_key: "jv_test".to_string(),
            api_key_env: "JEV_API_KEY".to_string(),
        }
    }

    #[test]
    fn inactive_options_skip_the_request() {
        let candidates = vec!["甲".to_string(), "乙".to_string()];
        assert!(reranked_order(
            &RerankOptions::default(),
            json!({}),
            "指令",
            &candidates
        )
        .is_none());
        // 开关开着但没有可用渠道同样跳过。
        let enabled_without_channel = RerankOptions {
            enabled: true,
            ..RerankOptions::default()
        };
        assert!(reranked_order(&enabled_without_channel, json!({}), "指令", &candidates).is_none());
    }

    #[test]
    fn ranking_follows_probabilities_then_local_order() {
        let text = json!({
            "model": "jev-1.13.0",
            "answers": {
                QUESTION_ID: {
                    "type": "choice",
                    "choice": "c1",
                    "confidence": 0.91,
                    "probabilities": {"c0": 0.2, "c1": 0.7},
                }
            }
        })
        .to_string();
        assert_eq!(parse_ranking(&text, JEV, 3).expect("可解析"), vec![1, 0, 2]);

        // 置信度并列时保持本地顺序。
        let tied = json!({
            "answers": {QUESTION_ID: {"type": "choice", "choice": "c2",
                "probabilities": {"c1": 0.5, "c2": 0.5, "c0": 0.5}}}
        })
        .to_string();
        assert_eq!(parse_ranking(&tied, JEV, 3).expect("可解析"), vec![0, 1, 2]);
    }

    #[test]
    fn ranking_falls_back_to_the_winning_choice() {
        let text = json!({
            "answers": {QUESTION_ID: {"type": "choice", "choice": "c2", "confidence": 0.8}}
        })
        .to_string();
        assert_eq!(parse_ranking(&text, JEV, 3).expect("可解析"), vec![2, 0, 1]);
    }

    #[test]
    fn unusable_responses_are_rejected() {
        assert!(parse_ranking("not json", JEV, 2).is_err());
        let empty = json!({"answers": {QUESTION_ID: {"type": "choice"}}}).to_string();
        assert!(parse_ranking(&empty, JEV, 2).is_err());
        // 越界或未知键一律忽略；全都取不到时按不可用处理。
        let out_of_range = json!({
            "answers": {QUESTION_ID: {"probabilities": {"c9": 0.9, "other": 0.1}}}
        })
        .to_string();
        assert!(parse_ranking(&out_of_range, JEV, 2).is_err());
    }

    /// 对话补全方式：答案包在 `choices[0].message.content` 里，排序解析共用同一段代码。
    #[test]
    fn chat_completions_ranking_reads_answers_from_message_content() {
        let answers = json!({
            "answers": {QUESTION_ID: {"type": "choice", "choice": "c2",
                "probabilities": {"c0": 0.05, "c1": 0.15, "c2": 0.8}}}
        })
        .to_string();
        let text = json!({"choices": [{"message": {"content": answers}}]}).to_string();
        assert_eq!(
            parse_ranking(&text, CHAT, 3).expect("可解析"),
            vec![2, 1, 0]
        );
    }

    #[test]
    fn candidates_are_collapsed_and_truncated() {
        let long = "x".repeat(CANDIDATE_MAX_CHARS + 10);
        let collapsed = candidate_text(&format!("  多   行  {long}  "));
        assert!(collapsed.starts_with("多 行 x"));
        assert_eq!(collapsed.chars().count(), CANDIDATE_MAX_CHARS + 1);
        assert!(collapsed.ends_with('…'));
    }

    #[test]
    fn request_shape_and_ranking_round_trip_over_the_wire() {
        // 本地回环：一次 `/v1/decide` 往返，验证请求形状与按置信度排序。
        let cassette = RerankCassette::serve();
        let options = RerankOptions {
            enabled: true,
            channel: Some(channel(&cassette.base_url())),
            ..RerankOptions::default()
        };
        let candidates = vec![
            "第一条候选".to_string(),
            "第二条候选".to_string(),
            "第三条候选".to_string(),
        ];
        let order = reranked_order(
            &options,
            json!({"query": "查询文本"}),
            "哪个候选项与查询最相关？",
            &candidates,
        )
        .expect("回环应当返回排序");
        assert_eq!(order, vec![2, 1, 0], "按置信度降序");

        let captured = cassette.captured();
        assert_eq!(captured.path, "/v1/decide");
        assert_eq!(captured.authorization, "Bearer jv_test");
        assert_eq!(captured.model, "jev-latest");
        assert!(captured.state.contains("查询文本"), "{}", captured.state);
        assert_eq!(captured.criteria_keys, vec!["c0", "c1", "c2"]);
        assert_eq!(captured.question_type, "choice");
    }

    #[test]
    fn chat_completions_round_trip_keeps_the_same_semantics() {
        // 同一份候选与判定标准，换成对话补全方式：路径换成 /v1/chat/completions，
        // state 与 questions 走 user 消息，排序结果不变。
        let cassette = RerankCassette::serve();
        let options = RerankOptions {
            enabled: true,
            channel: Some(RerankChannel {
                mode: CHAT.to_string(),
                ..channel(&cassette.base_url())
            }),
            ..RerankOptions::default()
        };
        let candidates = vec!["甲".to_string(), "乙".to_string(), "丙".to_string()];
        let order = reranked_order(
            &options,
            json!({"query": "查询文本"}),
            "哪个候选项与查询最相关？",
            &candidates,
        )
        .expect("回环应当返回排序");
        assert_eq!(order, vec![2, 1, 0], "按置信度降序");

        let captured = cassette.captured();
        assert_eq!(captured.path, "/v1/chat/completions");
        assert!(captured.state.contains("查询文本"), "{}", captured.state);
        assert_eq!(captured.criteria_keys, vec!["c0", "c1", "c2"]);
        assert_eq!(captured.question_type, "choice");
    }

    #[test]
    fn missing_credentials_skip_the_rerank() {
        let options = RerankOptions {
            enabled: true,
            channel: Some(RerankChannel {
                api_key: String::new(),
                ..channel("http://127.0.0.1:1")
            }),
            ..RerankOptions::default()
        };
        assert!(reranked_order(
            &options,
            json!({}),
            "指令",
            &["甲".to_string(), "乙".to_string()]
        )
        .is_none());
    }

    /// 决策服务的本地回环：固定回答一个排序，并记录收到的请求。
    struct RerankCassette {
        base_url: String,
        captured: Arc<std::sync::Mutex<CapturedRerankRequest>>,
    }

    #[derive(Default, Clone)]
    struct CapturedRerankRequest {
        path: String,
        authorization: String,
        model: String,
        state: String,
        question_type: String,
        criteria_keys: Vec<String>,
    }

    impl RerankCassette {
        fn serve() -> Self {
            use std::io::{BufRead, BufReader, Read, Write};
            use std::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
            let port = listener.local_addr().expect("本地地址").port();
            let captured = Arc::new(std::sync::Mutex::new(CapturedRerankRequest::default()));
            let seen = Arc::clone(&captured);
            std::thread::spawn(move || {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(stream.try_clone().expect("克隆流"));
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    return;
                }
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
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
                // 两种请求方式：原生把 state/questions 放顶层，对话补全把它们塞进 user 消息。
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
                let question = inner.get("questions").and_then(|value| value.get(QUESTION_ID));
                {
                    let mut record = seen.lock().expect("记录未被毒化");
                    record.path = path;
                    record.authorization = authorization;
                    record.model = request
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    record.state = inner
                        .get("state")
                        .cloned()
                        .unwrap_or(Value::Null)
                        .to_string();
                    record.question_type = question
                        .and_then(|value| value.get("type"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    record.criteria_keys = question
                        .and_then(|value| value.get("criteria"))
                        .and_then(Value::as_object)
                        .map(|entries| entries.keys().cloned().collect())
                        .unwrap_or_default();
                }
                let answers = json!({
                    QUESTION_ID: {
                        "type": "choice",
                        "choice": "c2",
                        "confidence": 0.9,
                        "probabilities": {"c0": 0.05, "c1": 0.15, "c2": 0.8},
                    }
                });
                // 两种请求方式各自认自己的响应形状。
                let payload = if native {
                    json!({"model": "jev-1.13.0", "answers": answers}).to_string()
                } else {
                    json!({
                        "model": "jev-1.13.0",
                        "choices": [{
                            "message": {
                                "role": "assistant",
                                "content": json!({"answers": answers}).to_string(),
                            }
                        }],
                    })
                    .to_string()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let mut writer = stream;
                let _ = writer.write_all(response.as_bytes());
                let _ = writer.flush();
            });
            Self {
                base_url: format!("http://127.0.0.1:{port}"),
                captured,
            }
        }

        fn base_url(&self) -> String {
            self.base_url.clone()
        }

        fn captured(&self) -> CapturedRerankRequest {
            self.captured.lock().expect("记录未被毒化").clone()
        }
    }
}
