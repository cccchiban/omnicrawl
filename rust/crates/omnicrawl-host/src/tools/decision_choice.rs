//! 提问托管：把 `ask_user` 的选项交给结构化决策模型，自动选置信度最高的那个。
//!
//! 可选项、默认关闭（`decision_models.toml` 的 `[features] ask_user_custody`）。开启后
//! **有选项**的提问不再弹给用户：宿主把问题、用户本回合的请求与已有的顾问答复当 `state`，
//! 向决策服务提一个 choice 问题（候选项就是模型的选项），读回 `answers.<id>.choice` 作为答案。
//! 没有选项的提问（自由问答）照旧交给用户——决策模型只能从给定选项里选，写不出自由文本。
//!
//! **失败一律 fail-open**：开关没开、没有可用决策渠道、缺凭据、请求失败、响应不可解析都退回
//! 人工提问（与检索重排同一语义，和审查通道的 fail-closed 相反）：托管的目的是省一次人工往返，
//! 不值当为此让回合失败或替用户瞎猜。
//!
//! 出网脱敏复用审查通道那套 `[desensitization]` 旁路（[`crate::review::masking_from_config`]）：
//! 问题与上下文同样屏蔽后再外发；脱敏构造失败按「托管不可用」处理，绝不外发原文。

use std::sync::Arc;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::decision_model::{
    load_decision_model_configuration, load_decision_switches, DecisionChannelConfig,
};
use serde_json::{json, Map, Value};

use crate::review::{masking_from_config, ReviewMasking};

/// 进请求前单个字段截断到的字符数（判断选哪一项不需要整段正文）。
const CONTEXT_MAX_CHARS: usize = 1_200;
/// 一次托管请求的超时（决策服务本身是 70–500ms 量级，余量留给网络）。
const CUSTODY_TIMEOUT_SECONDS: u64 = 20;
const USER_AGENT: &str = "omnicrawl-ask-user-custody/0.0.1";
/// 提问 ID；候选项键就是它在 `criteria` 里的下标（`o0`、`o1`…），便于把回答映射回选项。
const QUESTION_ID: &str = "best_option";

/// 托管用的决策渠道：与审查、检索重排同源（`decision_models.toml` 的默认决策渠道）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceChannel {
    /// 请求方式（`jev` / `chat_completions`）。
    pub mode: String,
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub api_key_env: String,
}

impl ChoiceChannel {
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

/// 一次托管请求的全部输入。
pub struct ChoiceRequest<'a> {
    pub channel: &'a ChoiceChannel,
    /// 出网脱敏旁路；`None` 表示脱敏未启用。
    pub masking: Option<&'a ReviewMasking>,
    /// 交给决策模型的上下文（问题、用户请求、顾问答复等）。
    pub state: &'a Value,
    pub instructions: &'a str,
    /// 候选选项的判定标准；下标即选项下标。
    pub criteria: &'a [(String, String)],
}

/// 托管执行器：真实实现走决策服务；测试注入桩以便不起网络。
pub trait ChoiceClient: Send + Sync {
    /// 返回胜出选项的下标；`Err` 表示本次托管不可用（调用方 fail-open）。
    fn choose(&self, request: &ChoiceRequest<'_>) -> Result<usize, String>;
}

/// 提问托管的运行期。
#[derive(Clone)]
pub struct CustodyOptions {
    /// 开关状态。
    pub enabled: bool,
    /// 默认决策渠道；`None` 表示没有可用渠道。
    pub channel: Option<ChoiceChannel>,
    pub masking: Option<Arc<ReviewMasking>>,
    /// 托管执行器（测试注入桩）。
    pub client: Arc<dyn ChoiceClient>,
}

impl Default for CustodyOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            channel: None,
            masking: None,
            client: Arc::new(DecisionChoiceClient),
        }
    }
}

impl std::fmt::Debug for CustodyOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 脱敏旁路与执行器都是 trait 对象，只报「有没有」。
        formatter
            .debug_struct("CustodyOptions")
            .field("enabled", &self.enabled)
            .field("channel", &self.channel)
            .field("masking", &self.masking.is_some())
            .finish()
    }
}

impl CustodyOptions {
    /// 是否真正可用（开关打开且已选中决策渠道）。
    pub fn active(&self) -> bool {
        self.enabled && self.channel.is_some()
    }
}

/// 按配置装配提问托管运行期（TUI 与 API 共用同一口径）。
pub fn custody_options_from_config(environment: &ConfigEnvironment) -> CustodyOptions {
    let switch = omnicrawl_config::features::decision_model::DECISION_SWITCH_ASK_USER_CUSTODY;
    if !load_decision_switches(environment, None)
        .get(switch)
        .copied()
        .unwrap_or(false)
    {
        // 关掉时连配置都不读：托管是个纯加速项，不该在启动路径上多花一次磁盘往返。
        return CustodyOptions::default();
    }
    let channel = load_decision_model_configuration(environment, None)
        .ok()
        .and_then(|configuration| configuration.active_channel().cloned())
        .map(|channel| ChoiceChannel::from_config(&channel));
    CustodyOptions {
        enabled: true,
        channel,
        masking: masking_from_config(environment).map(Arc::new),
        ..CustodyOptions::default()
    }
}

/// 托管的上下文：问题、用户本回合的请求、已有的顾问答复。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CustodyContext {
    /// 待决提问正文。
    pub question: String,
    /// 用户本回合提交的原文（截断）。
    pub user_prompt: String,
    /// 本回合已有的顾问模型答复（按发生顺序，截断）；没有时为空。
    pub advisor_replies: Vec<String>,
}

/// 让决策模型选一个选项；`None` 表示本次托管不可用，调用方保留人工提问（fail-open）。
///
/// 只在**有选项**时生效：没有选项的提问是自由问答，决策模型只能从给定候选项里选，
/// 写不出自由文本，因此照旧交给用户。
pub fn chosen_option(
    options: &CustodyOptions,
    context: &CustodyContext,
    choices: &[String],
) -> Option<usize> {
    if !options.active() || choices.is_empty() {
        return None;
    }
    let channel = options.channel.as_ref()?;
    let state = custody_state(context);
    let criteria: Vec<(String, String)> = choices
        .iter()
        .enumerate()
        .map(|(index, text)| {
            (
                crate::decision_wire::option_key(index),
                format!("选择「{}」：{}", option_text(text), CUSTODY_CHOICE_CRITERIA),
            )
        })
        .collect();
    let request = ChoiceRequest {
        channel,
        masking: options.masking.as_deref(),
        state: &state,
        instructions: CUSTODY_INSTRUCTIONS,
        criteria: &criteria,
    };
    match options.client.choose(&request) {
        Ok(index) if index < choices.len() => Some(index),
        Ok(index) => {
            eprintln!("[host] 提问托管返回越界选项（{index}），改为人工提问。");
            None
        }
        Err(error) => {
            eprintln!("[host] 提问托管不可用，改为人工提问：{error}");
            None
        }
    }
}

/// 托管请求的 `state`：问题 + 用户本回合请求 + 顾问答复（都截断，缺项不写空字段）。
pub fn custody_state(context: &CustodyContext) -> Value {
    let mut state = Map::new();
    state.insert(
        "question".to_string(),
        Value::String(collapse(&context.question, CONTEXT_MAX_CHARS)),
    );
    if !context.user_prompt.trim().is_empty() {
        state.insert(
            "user_prompt".to_string(),
            Value::String(collapse(&context.user_prompt, CONTEXT_MAX_CHARS)),
        );
    }
    let replies: Vec<Value> = context
        .advisor_replies
        .iter()
        .filter(|reply| !reply.trim().is_empty())
        .map(|reply| Value::String(collapse(reply, CONTEXT_MAX_CHARS)))
        .collect();
    if !replies.is_empty() {
        state.insert("advisor_replies".to_string(), Value::Array(replies));
    }
    Value::Object(state)
}

/// 托管请求的判定标准：选最符合用户意图、能推进当前任务的一项。
const CUSTODY_INSTRUCTIONS: &str = "模型在任务中途向用户提问，需要替用户从候选项里挑一个答案。\
 依据 state 里的 question（提问正文）、user_prompt（用户本回合的请求）与 advisor_replies\
 （顾问模型给出的答复，可能为空）判断：只选最符合用户意图、最能推进当前任务的一项。";

const CUSTODY_CHOICE_CRITERIA: &str = "该选项最符合用户意图且最能推进当前任务。";

/// 真实实现：`POST {base_url}/v1/decide`，一个 choice 问题，读回胜出选项。
///
/// 公开是因为本地 REST 决策接口（`omnicrawl-decision`）直接复用这一条出站路径：
/// 请求形状、脱敏旁路与选项解析都只有这一份实现。
pub struct DecisionChoiceClient;

impl ChoiceClient for DecisionChoiceClient {
    fn choose(&self, request: &ChoiceRequest<'_>) -> Result<usize, String> {
        let channel = request.channel;
        let api_key = channel.resolve_api_key();
        if api_key.is_empty() && crate::decision_wire::requires_api_key(&channel.mode) {
            return Err(format!(
                "缺少 API Key：请设置环境变量 {}。",
                channel.api_key_env
            ));
        }
        let criteria: Map<String, Value> = request
            .criteria
            .iter()
            .map(|(key, text)| (key.clone(), Value::String(text.clone())))
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
            .map_err(|error| format!("构造托管请求失败：{error}"))?;
        // 脱敏旁路：屏蔽失败即放弃本次托管（不外发原文），与「失败一律 fail-open」一致。
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

        let timeout = CUSTODY_TIMEOUT_SECONDS;
        let text = crate::decision_wire::post_body(
            &channel.decide_url(),
            &api_key,
            &body_text,
            timeout,
            USER_AGENT,
        )?;
        parse_choice(&text, &channel.mode, request.criteria.len())
    }
}

/// 解析答案里的胜出选项：`choice` 是候选项键；缺失时退化成概率最高的一项。
fn parse_choice(text: &str, mode: &str, count: usize) -> Result<usize, String> {
    crate::decision_wire::parse_choice_index(
        mode,
        text,
        QUESTION_ID,
        count,
        crate::decision_wire::OPTION_KEY_PREFIX,
    )
}

/// 选项进请求前的收敛：空白压成单空格，超长截断。
fn option_text(text: &str) -> String {
    collapse(text, CONTEXT_MAX_CHARS)
}

fn collapse(text: &str, limit: usize) -> String {
    crate::decision_wire::collapse_text(text, limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnicrawl_config::features::decision_model::{
        DECISION_MODE_CHAT_COMPLETIONS, DECISION_MODE_JEV,
    };

    const JEV: &str = DECISION_MODE_JEV;
    const CHAT: &str = DECISION_MODE_CHAT_COMPLETIONS;

    fn channel(base_url: &str) -> ChoiceChannel {
        ChoiceChannel {
            mode: omnicrawl_config::features::decision_model::DECISION_MODE_JEV.to_string(),
            model: "jev-latest".to_string(),
            base_url: base_url.to_string(),
            api_key: "jv_test".to_string(),
            api_key_env: "JEV_API_KEY".to_string(),
        }
    }

    #[test]
    fn inactive_options_skip_the_request() {
        let choices = vec!["甲".to_string(), "乙".to_string()];
        assert!(chosen_option(
            &CustodyOptions::default(),
            &CustodyContext::default(),
            &choices
        )
        .is_none());
        // 开关开着但没有可用渠道同样跳过。
        let enabled_without_channel = CustodyOptions {
            enabled: true,
            ..CustodyOptions::default()
        };
        assert!(chosen_option(
            &enabled_without_channel,
            &CustodyContext::default(),
            &choices
        )
        .is_none());
    }

    #[test]
    fn a_question_without_options_is_never_custodied() {
        // 自由问答交给用户：决策模型只能从给定选项里选，没有选项就没有可托管的余地。
        let options = CustodyOptions {
            enabled: true,
            channel: Some(channel("http://127.0.0.1:1")),
            ..CustodyOptions::default()
        };
        assert!(chosen_option(&options, &CustodyContext::default(), &[]).is_none());
    }

    #[test]
    fn state_carries_question_user_prompt_and_advisor_replies() {
        let context = CustodyContext {
            question: "选哪个方案？".to_string(),
            user_prompt: "把构建脚本整理一下".to_string(),
            advisor_replies: vec!["建议先拆分脚本".to_string(), "  ".to_string()],
        };
        let state = custody_state(&context);
        assert_eq!(state["question"], "选哪个方案？");
        assert_eq!(state["user_prompt"], "把构建脚本整理一下");
        assert_eq!(
            state["advisor_replies"],
            json!(["建议先拆分脚本"]),
            "空顾问答复不写进请求"
        );

        // 没有顾问答复时不写这个字段（不是写一个空数组）。
        let bare = custody_state(&CustodyContext {
            question: "选哪个方案？".to_string(),
            ..CustodyContext::default()
        });
        assert!(bare.get("advisor_replies").is_none());
        assert!(bare.get("user_prompt").is_none());
    }

    #[test]
    fn state_fields_are_collapsed_and_truncated() {
        let context = CustodyContext {
            question: format!("  多   行  {}  ", "x".repeat(CONTEXT_MAX_CHARS + 10)),
            ..CustodyContext::default()
        };
        let state = custody_state(&context);
        let question = state["question"].as_str().unwrap_or_default();
        assert!(question.starts_with("多 行 x"), "{question}");
        assert_eq!(question.chars().count(), CONTEXT_MAX_CHARS + 1);
        assert!(question.ends_with('…'));
    }

    #[test]
    fn choice_parses_winning_option_and_falls_back_to_probabilities() {
        let text = json!({
            "model": "jev-1.13.0",
            "answers": {QUESTION_ID: {"type": "choice", "choice": "o2", "confidence": 0.8}}
        })
        .to_string();
        assert_eq!(parse_choice(&text, JEV, 3).expect("可解析"), 2);

        // 没有 choice 时取概率最高的一项。
        let probabilities = json!({
            "answers": {QUESTION_ID: {"type": "choice",
                "probabilities": {"o0": 0.1, "o1": 0.7, "o2": 0.2}}}
        })
        .to_string();
        assert_eq!(parse_choice(&probabilities, JEV, 3).expect("可解析"), 1);
    }

    #[test]
    fn unusable_responses_are_rejected() {
        assert!(parse_choice("not json", JEV, 2).is_err());
        let empty = json!({"answers": {QUESTION_ID: {"type": "choice"}}}).to_string();
        assert!(parse_choice(&empty, JEV, 2).is_err());
        // 越界或未知键一律忽略；全都取不到时按不可用处理。
        let out_of_range = json!({
            "answers": {QUESTION_ID: {"choice": "o9", "probabilities": {"o9": 0.9}}}
        })
        .to_string();
        assert!(parse_choice(&out_of_range, JEV, 2).is_err());
    }

    /// 对话补全方式：答案包在 `choices[0].message.content` 里，选项解析共用同一段代码。
    #[test]
    fn chat_completions_choice_reads_answers_from_message_content() {
        let answers = json!({
            "answers": {QUESTION_ID: {"type": "choice", "choice": "o2", "confidence": 0.8}}
        })
        .to_string();
        let text = json!({"choices": [{"message": {"content": answers}}]}).to_string();
        assert_eq!(parse_choice(&text, CHAT, 3).expect("可解析"), 2);
    }

    #[test]
    fn missing_credentials_skip_the_custody() {
        let options = CustodyOptions {
            enabled: true,
            channel: Some(ChoiceChannel {
                api_key: String::new(),
                ..channel("http://127.0.0.1:1")
            }),
            ..CustodyOptions::default()
        };
        assert!(chosen_option(
            &options,
            &CustodyContext::default(),
            &["甲".to_string(), "乙".to_string()]
        )
        .is_none());
    }

    #[test]
    fn request_shape_and_choice_round_trip_over_the_wire() {
        // 本地回环：一次 `/v1/decide` 往返，验证请求形状与胜出选项的解析。
        let cassette = CustodyCassette::serve();
        let options = CustodyOptions {
            enabled: true,
            channel: Some(channel(&cassette.base_url())),
            ..CustodyOptions::default()
        };
        let context = CustodyContext {
            question: "选哪个方案？".to_string(),
            user_prompt: "把构建脚本整理一下".to_string(),
            advisor_replies: vec!["建议先拆分脚本".to_string()],
        };
        let chosen = chosen_option(
            &options,
            &context,
            &["方案甲".to_string(), "方案乙".to_string(), "方案丙".to_string()],
        )
        .expect("回环应当选出选项");
        assert_eq!(chosen, 2);

        let captured = cassette.captured();
        assert_eq!(captured.path, "/v1/decide");
        assert_eq!(captured.authorization, "Bearer jv_test");
        assert_eq!(captured.model, "jev-latest");
        assert_eq!(captured.question_type, "choice");
        assert_eq!(captured.criteria_keys, vec!["o0", "o1", "o2"]);
        // 三类上下文都进了请求：提问、用户本回合的请求、顾问答复。
        assert!(captured.state.contains("选哪个方案"), "{}", captured.state);
        assert!(
            captured.state.contains("把构建脚本整理一下"),
            "{}",
            captured.state
        );
        assert!(captured.state.contains("建议先拆分脚本"), "{}", captured.state);
    }

    #[test]
    fn chat_completions_round_trip_keeps_the_same_semantics() {
        // 同一份上下文与选项，换成对话补全方式：基地址按 OpenAI 兼容口径带上 /v1，
        // 只追加资源路径，state 与 questions 走 user 消息，选出的选项不变。
        let cassette = CustodyCassette::serve();
        let options = CustodyOptions {
            enabled: true,
            channel: Some(ChoiceChannel {
                mode: CHAT.to_string(),
                base_url: format!("{}/v1", cassette.base_url()),
                ..channel(&cassette.base_url())
            }),
            ..CustodyOptions::default()
        };
        let context = CustodyContext {
            question: "选哪个方案？".to_string(),
            user_prompt: "把构建脚本整理一下".to_string(),
            advisor_replies: Vec::new(),
        };
        let chosen = chosen_option(
            &options,
            &context,
            &["方案甲".to_string(), "方案乙".to_string(), "方案丙".to_string()],
        )
        .expect("回环应当选出选项");
        assert_eq!(chosen, 2);

        let captured = cassette.captured();
        assert_eq!(captured.path, "/v1/chat/completions");
        assert_eq!(captured.question_type, "choice");
        assert_eq!(captured.criteria_keys, vec!["o0", "o1", "o2"]);
        assert!(captured.state.contains("选哪个方案"), "{}", captured.state);
    }

    /// 决策服务的本地回环：固定回答一个选项，并记录收到的请求。
    struct CustodyCassette {
        base_url: String,
        captured: Arc<std::sync::Mutex<CapturedCustodyRequest>>,
    }

    #[derive(Default, Clone)]
    struct CapturedCustodyRequest {
        path: String,
        authorization: String,
        model: String,
        state: String,
        question_type: String,
        criteria_keys: Vec<String>,
    }

    impl CustodyCassette {
        fn serve() -> Self {
            use std::io::{BufRead, BufReader, Read, Write};
            use std::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
            let port = listener.local_addr().expect("本地地址").port();
            let captured = Arc::new(std::sync::Mutex::new(CapturedCustodyRequest::default()));
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
                        "choice": "o2",
                        "confidence": 0.9,
                        "probabilities": {"o0": 0.1, "o1": 0.2, "o2": 0.7},
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

        fn captured(&self) -> CapturedCustodyRequest {
            self.captured.lock().expect("记录未被毒化").clone()
        }
    }
}
