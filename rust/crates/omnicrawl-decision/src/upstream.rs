//! 决策转发的四种语义：通用 decide、选优 choice、排序 rank、审查 review。
//!
//! 四种操作都复用宿主执行层现有的出站路径与解析逻辑（`omnicrawl_host::decision_wire`
//! 与 `omnicrawl_host::review`），因此 REST 接口与宿主内部的三处调用点在语义上完全一致：
//! 请求形状、选项键规则、脱敏旁路、失败语义都只有一份实现。
//!
//! 失败语义按调用方的期待分流：
//! * `decide`：透传上游结果，上游报错就如实回错。
//! * `choice` / `rank`：读不出可用答案即报错，不猜、不编。
//! * `review`：判定结论与拒绝理由都回结构化字段，调用方自行决定 fail-open / fail-closed。
//!
//! 脱敏：接口是**出站请求的发起方**，因此与其余三处调用点同源——只要 `[desensitization]`
//! 启用，请求体一律先屏蔽再外发，响应回来再还原；脱敏构造失败即中止（绝不外发原文）。

use std::sync::Arc;

use omnicrawl_host::decision_wire::{
    choice_question, collapse_text, extract_answers, option_key, option_key_with,
    parse_choice_index, parse_ranking_order, request_body, OPTION_KEY_PREFIX,
};
use omnicrawl_host::review::{
    decision_post_masked, decision_review, DecisionReviewOptions, DecisionVerdict, ReviewMasking,
};
use serde_json::{json, Map, Value};

/// 决策请求的默认超时（秒）。
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 20;
/// 调用方可指定的超时上限（秒）。
pub const MAX_TIMEOUT_SECONDS: u64 = 300;
/// 候选项进请求前截断到的字符数（与工具内部同一口径）。
pub const CANDIDATE_MAX_CHARS: usize = 400;
/// 排序用的候选项键前缀（与选优的 `o0` 区分，便于日志辨识）。
const RANK_KEY_PREFIX: &str = "c";
/// `choice` / `rank` 的默认提问 ID：调用方没指定时用它。
///
/// 与宿主内部两处调用点同名，因此同一份 `answers` 在两个入口之间可以直接互通。
pub const DEFAULT_QUESTION_ID: &str = "best_option";

/// 出站决策失败：`code` 供接口层映射 HTTP 状态，`message` 可直接展示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamError {
    pub code: &'static str,
    pub message: String,
}

impl UpstreamError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// 缺少决策凭据（配置问题，改配置即可）。
    pub fn missing_credentials(message: impl Into<String>) -> Self {
        Self::new("DECISION_CREDENTIALS_MISSING", message)
    }

    /// 脱敏不可用：拒绝外发原文。
    pub fn masking_unavailable(message: impl Into<String>) -> Self {
        Self::new("DECISION_MASKING_UNAVAILABLE", message)
    }

    /// 上游请求失败（连接、超时、HTTP 错误码）。
    pub fn request_failed(message: impl Into<String>) -> Self {
        Self::new("DECISION_REQUEST_FAILED", message)
    }

    /// 上游响应不可解析或没有可用答案。
    pub fn unparsable(message: impl Into<String>) -> Self {
        Self::new("DECISION_UNPARSABLE", message)
    }

    /// 调用方的入参不合法。
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new("INVALID_REQUEST", message)
    }
}

/// 一次决策出站请求的公共上下文：渠道 + 脱敏旁路 + 超时。
pub struct DecisionContext {
    pub channel: DecisionReviewOptions,
    pub masking: Option<Arc<ReviewMasking>>,
    pub timeout_seconds: u64,
}

impl DecisionContext {
    /// 发一次请求并取回响应正文（脱敏的屏蔽与还原都在宿主的同一函数里完成）。
    fn send(&self, state: &Value, questions: &Value) -> Result<String, UpstreamError> {
        let body = request_body(&self.channel.mode, &self.channel.model, state, questions);
        let body_text = serde_json::to_string(&body)
            .map_err(|error| UpstreamError::request_failed(format!("构造决策请求失败：{error}")))?;
        let text = decision_post_masked(
            &self.channel,
            self.masking.as_deref(),
            false,
            self.timeout_seconds,
            &body_text,
        )
        .map_err(classify)?;
        Ok(text)
    }
}

/// 把宿主侧的失败文案分类成接口层的错误码。
///
/// 宿主的三处调用点只回一条可读文案，接口层需要区分「改配置」与「换时机重试」，
/// 因此按文案前缀归类——这是唯一需要区分的地方，其余一律按上游请求失败处理。
fn classify(message: String) -> UpstreamError {
    if message.contains("缺少 API Key") {
        return UpstreamError::missing_credentials(message);
    }
    if message.contains("脱敏") {
        return UpstreamError::masking_unavailable(message);
    }
    if message.contains("未返回可用")
        || message.contains("解析")
        || message.contains("不是 JSON")
        || message.contains("缺少 answers")
    {
        return UpstreamError::unparsable(message);
    }
    UpstreamError::request_failed(message)
}

/// 一次通用决策的结果：答案对象 + 上游原始响应（便于调用方自行诊断）。
#[derive(Debug, Clone, PartialEq)]
pub struct DecideOutcome {
    pub answers: Value,
    pub raw: Value,
}

/// 通用 `decide`：把 `state` + `questions` 原样转发给决策服务，读回 `answers`。
pub fn decide(
    context: &DecisionContext,
    state: &Value,
    questions: &Value,
) -> Result<DecideOutcome, UpstreamError> {
    let text = context.send(state, questions)?;
    let answers =
        extract_answers(&context.channel.mode, &text).map_err(UpstreamError::unparsable)?;
    let raw = serde_json::from_str::<Value>(&text)
        .map_err(|error| UpstreamError::unparsable(format!("决策响应不是 JSON：{error}")))?;
    Ok(DecideOutcome { answers, raw })
}

/// 一次选优的结果：胜出候选下标 + 上游给出的答案。
#[derive(Debug, Clone, PartialEq)]
pub struct ChoiceOutcome {
    pub index: usize,
    pub answers: Value,
}

/// `choice`：从调用方给的候选项里选最合适的一个。
pub fn decide_choice(
    context: &DecisionContext,
    state: &Value,
    instructions: &str,
    options: &[String],
    question_id: &str,
) -> Result<ChoiceOutcome, UpstreamError> {
    if options.is_empty() {
        return Err(UpstreamError::invalid_request("choice 至少需要一个候选项。"));
    }
    let criteria = labeled_criteria(options, option_key, instructions);
    let questions = json!({ question_id: choice_question(instructions, criteria) });
    let text = context.send(state, &questions)?;
    let index = parse_choice_index(
        &context.channel.mode,
        &text,
        question_id,
        options.len(),
        OPTION_KEY_PREFIX,
    )
    .map_err(UpstreamError::unparsable)?;
    let answers =
        extract_answers(&context.channel.mode, &text).map_err(UpstreamError::unparsable)?;
    Ok(ChoiceOutcome { index, answers })
}

/// `rank`：按相关度给调用方给的候选项排序，返回输入下标的新顺序。
pub fn decide_rank(
    context: &DecisionContext,
    state: &Value,
    instructions: &str,
    candidates: &[String],
    question_id: &str,
) -> Result<Vec<usize>, UpstreamError> {
    if candidates.len() < 2 {
        return Err(UpstreamError::invalid_request(
            "rank 至少需要两个候选项（单个候选无需排序）。",
        ));
    }
    // 排序的候选项就是待排文本本身：只收敛文本，不再套一层「选择「…」」的说明。
    let criteria: Map<String, Value> = candidates
        .iter()
        .enumerate()
        .map(|(index, text)| {
            (
                option_key_with(RANK_KEY_PREFIX, index),
                Value::String(collapse_text(text, CANDIDATE_MAX_CHARS)),
            )
        })
        .collect();
    let questions = json!({ question_id: choice_question(instructions, criteria) });
    let text = context.send(state, &questions)?;
    parse_ranking_order(
        &context.channel.mode,
        &text,
        question_id,
        candidates.len(),
        RANK_KEY_PREFIX,
    )
    .map_err(UpstreamError::unparsable)
}

/// `review`：把一份待审查负载交给决策渠道判定，回结构化结论。
///
/// 与工具调用审查共用 `omnicrawl_host::review::decision_review`，因此提问与拒绝理由
/// 都是同一套写死的候选，不存在「REST 接口与内部审查判定不一致」。
pub fn decide_review(
    context: &DecisionContext,
    payload: &Value,
    fail_closed: bool,
) -> Result<DecisionVerdict, UpstreamError> {
    decision_review(
        &context.channel,
        context.masking.as_deref(),
        fail_closed,
        context.timeout_seconds,
        payload,
    )
    .map_err(classify)
}

/// 构造「候选项键 → 判定标准」：键由调用方给（选优用 `o`、排序用 `c`）。
fn labeled_criteria(
    options: &[String],
    key_of: fn(usize) -> String,
    instructions: &str,
) -> Map<String, Value> {
    let fallback = if instructions.trim().is_empty() {
        "该选项最符合 state 描述的情形。"
    } else {
        instructions
    };
    options
        .iter()
        .enumerate()
        .map(|(index, text)| {
            (
                key_of(index),
                Value::String(format!(
                    "选择「{}」：{}",
                    collapse_text(text, CANDIDATE_MAX_CHARS),
                    fallback
                )),
            )
        })
        .collect()
}
