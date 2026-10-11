//! 工具建议：用户发来新提问时，先问一次决策模型「这次要用哪些工具」，把结果拼进 user 尾部。
//!
//! 可选项、默认关闭（`decision_models.toml` 的 `[features] tool_suggestion`）。开启后，宿主在
//! 把提问交给内核之前拦截一次：只拿**当前这条提问的正文**（不带历史）与**本宿主工具表里全部
//! 已启用的工具**（工具名 + 描述首句截断到 [`TOOL_SUMMARY_MAX_CHARS`] 字），向决策服务提一组
//! noul 提问——每个工具一个「本次是否需要它」，外加一个兜底提问（本次是否属于无需任何工具的
//! 普通对话）。
//!
//! 候选不做人工取舍：`disabled_tools` 关掉的、当前没有运行期的工具（语音合成、顾问、Windows
//! 组）都已被工具表挡在外面，进表的即送审，工具表增删后清单自动跟着变。该用专用工具还是
//! 顺手用泛化工具，交给决策模型按 [`SUGGEST_REQUIREMENTS`] 判断——专用工具（子代理、生图、
//! 语音、桌面操作等）贴合任务特殊性，泛化工具（读写文件、搜索代码、执行命令、待办、提问）
//! 几乎每轮都可能顺手用到，除非本次任务围绕它展开，否则都该判为用不到。
//!
//! 判定面只有一条阈值：[`SUGGEST_THRESHOLD`]（0.5）。概率 ≥ 阈值进建议名单，< 阈值排除；
//! 兜底提问 ≥ 阈值时整份建议作废（普通对话、寒暄或无从判断的异常任务不该被塞进工具提示）。
//! 名单非空时在 user 尾部追加一行 `当前请求建议调用tool：a、b。`
//!
//! **失败一律 fail-open**：开关没开、没有可用决策渠道、缺凭据、请求失败、响应不可解析都按
//! 「本次无建议」处理，user 原文照发（与检索重排、提问托管、工具淘汰同一语义）：建议只是提示，
//! 值不当为此挡住提问。
//!
//! 出网脱敏复用审查通道那套 `[desensitization]` 旁路（[`crate::review::masking_from_config`]）：
//! 提问正文同样屏蔽后再外发；脱敏构造失败按「建议不可用」处理，绝不外发原文。

use std::sync::Arc;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::decision_model::{
    load_decision_model_configuration, load_decision_switches, DecisionChannelConfig,
};
use omnicrawl_core::diagnostics;
use serde_json::{json, Map, Value};

use crate::review::{masking_from_config, ReviewMasking};

/// 送审的工具说明：取描述首句，截断到这么多字符（超出加省略号）。
///
/// 够决策模型分辨「这个工具是干什么的」，又不至于把请求撑大——工具表里的描述动辄上百字，
/// 全量送审会让请求体随工具数线性膨胀。
pub const TOOL_SUMMARY_MAX_CHARS: usize = 24;

/// 置信度分界线：≥ 该值的工具进建议名单，< 该值的排除。
pub const SUGGEST_THRESHOLD: f64 = 0.5;

/// 一次建议请求的超时（决策服务本身是 70–500ms 量级，余量留给网络）。
const SUGGEST_TIMEOUT_SECONDS: u64 = 20;
/// 提问正文进请求前截断到的字符数。
const USER_PROMPT_MAX_CHARS: usize = 1_200;
/// 兜底提问 ID：本次是否属于无需任何工具的普通对话。
const NO_TOOL_QUESTION_ID: &str = "no_tool";
/// 工具提问 ID 的前缀（`t0`、`t1`…），下标与送审顺序一致。
const TOOL_QUESTION_PREFIX: &str = "t";

/// 选择要求（写进每个提问，决策模型据此判定）。
///
/// 候选不做人工取舍（进工具表的都送审），因此要求必须说清专用工具与泛化工具的分量差别，
/// 否则泛化工具几乎每轮都会被判为「会用到」，把真正贴合任务的那一个挤下去。
const SUGGEST_REQUIREMENTS: &str = "本次只会用到下列工具中的一部分，请逐个判断：判定概率 ≥0.5 表示本次任务会用到该工具，\
 <0.5 表示用不到。只依据 state.user_prompt 这一条提问的正文判断，不要因为工具存在就判定为需要。\
 优先识别贴合本次任务特殊性的专用工具（例如要拆并行子任务用子代理、要出图用生图、要桌面操作\
 找窗口工具）；读文件、写文件、搜代码、执行命令、记待办、向用户提问这类通用工具几乎每轮都能\
 顺手用一下，除非本次任务明确围绕它展开，否则判为用不到。";

/// 建议用的决策渠道：与审查、检索重排、提问托管、工具淘汰同源（默认决策渠道）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestChannel {
    /// 请求方式（`jev` / `chat_completions` / `onejev`）。
    pub mode: String,
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub api_key_env: String,
    /// 请求失败后的重试次数（不含首次请求）。
    pub retry_count: u64,
}

impl SuggestChannel {
    fn from_config(channel: &DecisionChannelConfig) -> Self {
        Self {
            mode: channel.mode.clone(),
            model: channel.model.clone(),
            base_url: channel.base_url.clone(),
            api_key: channel.api_key.clone(),
            api_key_env: channel.api_key_env.clone(),
            retry_count: channel.retry_count.max(0) as u64,
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

/// 一次建议请求的全部输入。
pub struct SuggestRequest<'a> {
    pub channel: &'a SuggestChannel,
    /// 出网脱敏旁路；`None` 表示脱敏未启用。
    pub masking: Option<&'a ReviewMasking>,
    /// 用户本次的提问正文（截断后进 `state`）。
    pub user_prompt: &'a str,
    /// 送审的候选工具（`(名称, 一句话说明)`，来自本宿主工具表），下标即提问下标。
    pub candidates: &'a [(String, String)],
}

/// 建议执行器：真实实现走决策服务；测试注入桩以便不起网络。
pub trait SuggestClient: Send + Sync {
    /// 返回建议使用的工具名（按送审顺序）；`Err` 表示本次建议不可用（调用方按无建议处理）。
    fn suggest(&self, request: &SuggestRequest<'_>) -> Result<Vec<String>, String>;
}

/// 工具建议的运行期。
#[derive(Clone)]
pub struct SuggestOptions {
    /// 开关状态。
    pub enabled: bool,
    /// 默认决策渠道；`None` 表示没有可用渠道。
    pub channel: Option<SuggestChannel>,
    pub masking: Option<Arc<ReviewMasking>>,
    /// 建议执行器（测试注入桩）。
    pub client: Arc<dyn SuggestClient>,
}

impl Default for SuggestOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            channel: None,
            masking: None,
            client: Arc::new(DecisionSuggestClient),
        }
    }
}

impl std::fmt::Debug for SuggestOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 脱敏旁路与执行器都是 trait 对象，只报「有没有」。
        formatter
            .debug_struct("SuggestOptions")
            .field("enabled", &self.enabled)
            .field("channel", &self.channel)
            .field("masking", &self.masking.is_some())
            .finish()
    }
}

impl SuggestOptions {
    /// 是否真正可用（开关打开且已选中决策渠道）。
    pub fn active(&self) -> bool {
        self.enabled && self.channel.is_some()
    }
}

/// 按配置装配工具建议运行期（TUI 与 API 共用同一口径）。
pub fn suggest_options_from_config(environment: &ConfigEnvironment) -> SuggestOptions {
    if !load_decision_switches(environment, None)
        .get(omnicrawl_config::features::decision_model::DECISION_SWITCH_TOOL_SUGGEST)
        .copied()
        .unwrap_or(false)
    {
        // 关掉时连配置都不读：建议是个纯提示项，不该在启动路径上多花一次磁盘往返。
        return SuggestOptions::default();
    }
    let channel = load_decision_model_configuration(environment, None)
        .ok()
        .and_then(|configuration| configuration.active_channel().cloned())
        .map(|channel| SuggestChannel::from_config(&channel));
    SuggestOptions {
        enabled: true,
        channel,
        masking: masking_from_config(environment).map(Arc::new),
        ..SuggestOptions::default()
    }
}

/// 从本宿主的工具表挑出送审候选：进表的（已启用）即送审，每项配一句描述。
///
/// `inventory` 是工具表给出的 `(名称, 描述)`（`ToolRegistry::tool_inventory`）——`disabled_tools`
/// 关掉的、当前没有运行期的（语音合成、顾问、Windows 组）都已经不在表里，因此这里不需要
/// 维护任何人工清单，工具表增删后候选自动跟着变。
pub fn suggest_candidates(inventory: &[(String, String)]) -> Vec<(String, String)> {
    inventory
        .iter()
        .map(|(name, description)| (name.clone(), tool_summary(description)))
        .collect()
}

/// 描述 → 送审用的一句话：剥掉「是什么：」前缀，取首句，压成单空格后截断到
/// [`TOOL_SUMMARY_MAX_CHARS`] 字。
pub fn tool_summary(description: &str) -> String {
    let body = description.trim();
    let body = body.strip_prefix("是什么：").unwrap_or(body);
    let first_sentence = body.split(['。', '；', '！']).next().unwrap_or(body);
    let collapsed = first_sentence.split_whitespace().collect::<Vec<_>>().join(" ");
    let characters: Vec<char> = collapsed.chars().collect();
    if characters.len() <= TOOL_SUMMARY_MAX_CHARS {
        return collapsed;
    }
    let head: String = characters[..TOOL_SUMMARY_MAX_CHARS].iter().collect();
    format!("{head}…")
}

/// 问一次决策模型「本次该用哪些工具」，返回建议名单（按送审顺序）。
///
/// `None` 表示本次没有建议：开关没开、没有可用渠道、工具表里一个工具都没有、请求失败、
/// 响应不可解析，或判定结果里一个工具都没过阈值（含兜底提问判定为「无需任何工具」）。
/// 调用方按「user 原文照发」处理。
pub fn suggested_tools(
    options: &SuggestOptions,
    inventory: &[(String, String)],
    user_prompt: &str,
) -> Option<Vec<String>> {
    if !options.active() || user_prompt.trim().is_empty() {
        return None;
    }
    let channel = options.channel.as_ref()?;
    let candidates = suggest_candidates(inventory);
    if candidates.is_empty() {
        return None;
    }
    let request = SuggestRequest {
        channel,
        masking: options.masking.as_deref(),
        user_prompt,
        candidates: &candidates,
    };
    match options.client.suggest(&request) {
        Ok(names) if names.is_empty() => None,
        Ok(names) => Some(names),
        Err(error) => {
            diagnostics::warn(format!("[host] 工具建议不可用，按无建议处理：{error}"));
            None
        }
    }
}

/// 建议行的固定前缀：插入文本与展示层拆行共用同一份口径。
const SUGGESTION_PREFIX: &str = "当前请求建议调用tool：";

/// 建议名单拼成的插入行；名单为空时返回 `None`（不插入任何内容）。
pub fn suggestion_line(names: &[String]) -> Option<String> {
    let names: Vec<&str> = names
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .collect();
    if names.is_empty() {
        return None;
    }
    Some(format!("{SUGGESTION_PREFIX}{}。", names.join("、")))
}

/// 把建议插入 user 尾部（另起一行）；没有建议时原样返回。
pub fn insert_suggestion(text: &str, names: &[String]) -> String {
    match suggestion_line(names) {
        Some(line) => format!("{text}\n\n{line}"),
        None => text.to_string(),
    }
}

/// 从 user 文本里拆出 [`insert_suggestion`] 拼进去的那一行，返回（提问正文，建议行）。
///
/// 插入行是**唱给模型看的提示**，不是用户说的内容，但落进会话流的仍是拼接后的整段文本
/// （内核与转录都按它走）。展示层在这里把它拆回两半：建议行挂到 `user：` 标签上，正文照旧
/// 逐行铺开——重放读到的历史文本同样含这一行，因此实时提交与回放走同一条拆解口径。
pub fn split_suggestion(text: &str) -> (String, Option<String>) {
    let trimmed = text.trim_end_matches(['\n', '\r']);
    let Some((body, last)) = trimmed.rsplit_once('\n') else {
        return (text.to_string(), None);
    };
    let body = body.trim_end_matches(['\n', '\r']);
    let last = last.trim_start();
    if !last.starts_with(SUGGESTION_PREFIX) {
        return (text.to_string(), None);
    }
    (body.to_string(), Some(last.to_string()))
}

/// 一次建议请求的 `state`：只有本次提问正文与候选工具清单，不带任何历史。
fn suggest_state(user_prompt: &str, candidates: &[(String, String)]) -> Value {
    let tools: Vec<Value> = candidates
        .iter()
        .map(|(name, summary)| json!({"name": name, "summary": summary}))
        .collect();
    json!({
        "user_prompt": crate::decision_wire::collapse_text(user_prompt, USER_PROMPT_MAX_CHARS),
        "tools": tools,
    })
}

/// 一次建议请求的 `questions`：每个候选工具一个 noul，外加一个兜底提问。
fn suggest_questions(candidates: &[(String, String)]) -> Value {
    let mut questions = Map::new();
    for (index, (name, summary)) in candidates.iter().enumerate() {
        questions.insert(
            tool_question_id(index),
            json!({
                "type": "noul",
                "instructions": format!("本次任务是否需要使用工具 {name}（{summary}）？{SUGGEST_REQUIREMENTS}"),
            }),
        );
    }
    questions.insert(
        NO_TOOL_QUESTION_ID.to_string(),
        json!({
            "type": "noul",
            "instructions": "本次任务是否属于普通对话、寒暄或无从判断的异常任务，\
             因而不需要使用 state.tools 里的任何工具？概率 ≥0.5 表示确实无需任何工具，\
             <0.5 表示需要用到其中某个工具。",
        }),
    );
    Value::Object(questions)
}

/// 候选工具的提问 ID：下标与送审顺序一致（`t0`、`t1`…）。
fn tool_question_id(index: usize) -> String {
    format!("{TOOL_QUESTION_PREFIX}{index}")
}

/// 真实实现：一次请求带 N 个 noul 提问，读回 ≥ 阈值的工具名。
///
/// 公开是因为本地 REST 决策接口（`omnicrawl-decision`）可以复用这一条出站路径。
pub struct DecisionSuggestClient;

impl SuggestClient for DecisionSuggestClient {
    fn suggest(&self, request: &SuggestRequest<'_>) -> Result<Vec<String>, String> {
        let channel = request.channel;
        let api_key = channel.resolve_api_key();
        if api_key.is_empty() && crate::decision_wire::requires_api_key(&channel.mode) {
            return Err(format!(
                "缺少 API Key：请设置环境变量 {}。",
                channel.api_key_env
            ));
        }
        let state = suggest_state(request.user_prompt, request.candidates);
        let questions = suggest_questions(request.candidates);
        let body =
            crate::decision_wire::request_body(&channel.mode, &channel.model, &state, &questions);
        let body_text =
            serde_json::to_string(&body).map_err(|error| format!("构造建议请求失败：{error}"))?;
        let text = crate::review::decision_post_masked(
            &crate::review::DecisionReviewOptions {
                mode: channel.mode.clone(),
                model: channel.model.clone(),
                base_url: channel.base_url.clone(),
                api_key: channel.api_key.clone(),
                api_key_env: channel.api_key_env.clone(),
                retry_count: channel.retry_count,
            },
            request.masking,
            // 脱敏失败即放弃本次建议（不外发原文），与「失败一律按无建议处理」一致。
            false,
            SUGGEST_TIMEOUT_SECONDS,
            &body_text,
        )?;
        parse_suggestions(&text, &channel.mode, request.candidates)
    }
}

/// 解析每个 noul 提问的判定：读回 ≥ 阈值的工具名。
///
/// 兜底提问（`no_tool`）过阈值时整份建议作废——返回空名单，调用方不插入任何内容。
/// 一条可识别的概率都没有时按不可用处理（`Err`），调用方同样不插入内容。
fn parse_suggestions(
    text: &str,
    mode: &str,
    candidates: &[(String, String)],
) -> Result<Vec<String>, String> {
    let answers = crate::decision_wire::extract_answers(mode, text)?;
    let probability = |key: &str| -> Option<f64> {
        answers
            .get(key)
            .and_then(|value| value.get("noul"))
            .and_then(probability_value)
    };

    // 兜底：判定为「无需任何工具」时不给出建议。
    if probability(NO_TOOL_QUESTION_ID).is_some_and(|value| value >= SUGGEST_THRESHOLD) {
        return Ok(Vec::new());
    }

    let mut answered = probability(NO_TOOL_QUESTION_ID).is_some();
    let mut suggested: Vec<String> = Vec::new();
    for (index, (name, _)) in candidates.iter().enumerate() {
        let Some(value) = probability(&tool_question_id(index)) else {
            continue;
        };
        answered = true;
        if value >= SUGGEST_THRESHOLD {
            suggested.push(name.clone());
        }
    }
    if !answered {
        return Err(format!(
            "决策模型未返回可用的工具判定：{}",
            crate::decision_wire::collapse_text(text, 200)
        ));
    }
    Ok(suggested)
}

/// noul 的取值：正常是数字；对话补全方式下模型偶尔写成字符串，这里一并认。
fn probability_value(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnicrawl_config::features::decision_model::{
        DECISION_MODE_CHAT_COMPLETIONS, DECISION_MODE_JEV,
    };

    const JEV: &str = DECISION_MODE_JEV;
    const CHAT: &str = DECISION_MODE_CHAT_COMPLETIONS;

    fn channel(base_url: &str) -> SuggestChannel {
        SuggestChannel {
            mode: JEV.to_string(),
            model: "jev-latest".to_string(),
            base_url: base_url.to_string(),
            api_key: "jv_test".to_string(),
            api_key_env: "JEV_API_KEY".to_string(),
            retry_count: 0,
        }
    }

    fn inventory(names: &[&str]) -> Vec<(String, String)> {
        names
            .iter()
            .map(|name| {
                (
                    (*name).to_string(),
                    format!("是什么：{name} 的说明。怎么做：看情况。"),
                )
            })
            .collect()
    }

    fn candidates() -> Vec<(String, String)> {
        vec![
            ("subagent".to_string(), "调度子代理".to_string()),
            ("fetcher".to_string(), "抓取网页".to_string()),
        ]
    }

    /// 说明取描述首句并截断：够分辨用途，又不至于随工具数线性撑大请求。
    #[test]
    fn summaries_come_from_the_description_first_sentence() {
        assert_eq!(tool_summary("是什么：搜索公开网页。怎么做：要发现 URL 时用。"), "搜索公开网页");
        // 「是什么：」前缀剥掉，首句按句号/分号/叹号切。
        assert_eq!(tool_summary("是什么：读取本机图片；不读别的。"), "读取本机图片");
        // 单行空白压成一个空格。
        assert_eq!(tool_summary("是什么：抓取\n   网页\n正文。"), "抓取 网页 正文");
        // 超长截断到上限并加省略号。
        let long = tool_summary(&format!("是什么：{}。", "长".repeat(TOOL_SUMMARY_MAX_CHARS + 5)));
        assert_eq!(long.chars().count(), TOOL_SUMMARY_MAX_CHARS + 1);
        assert!(long.ends_with('…'));
        // 没有「是什么：」前缀的描述照旧取首句。
        assert_eq!(tool_summary("调度子代理。支持同步/后台任务。"), "调度子代理");
    }

    /// 候选直接照工具表生成：进表的都送审，不需要任何人工清单。
    #[test]
    fn candidates_follow_the_tool_table() {
        let listed = suggest_candidates(&inventory(&["subagent", "bash", "read"]));
        assert_eq!(
            listed.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            vec!["subagent", "bash", "read"]
        );
        assert_eq!(listed[0].1, "subagent 的说明");
        assert!(suggest_candidates(&[]).is_empty());
    }

    /// 选择要求写进提问：阈值与「专用工具优先、泛化工具除非围绕它展开」都在。
    #[test]
    fn instructions_carry_the_threshold_and_the_selection_requirements() {
        let questions = suggest_questions(&candidates());
        let instructions = questions["t0"]["instructions"].as_str().unwrap_or_default();
        assert!(instructions.contains("subagent"), "{instructions}");
        assert!(instructions.contains("0.5"), "阈值要告知决策模型：{instructions}");
        assert!(
            instructions.contains("通用工具"),
            "泛化工具的分量差别要写清：{instructions}"
        );
        assert!(SUGGEST_REQUIREMENTS.contains("专用工具"), "{SUGGEST_REQUIREMENTS}");
    }

    #[test]
    fn inactive_options_skip_the_request() {
        let tools = inventory(&["subagent"]);
        assert!(suggested_tools(&SuggestOptions::default(), &tools, "写个小说").is_none());
        // 开关开着但没有可用渠道同样跳过。
        let enabled_without_channel = SuggestOptions {
            enabled: true,
            ..SuggestOptions::default()
        };
        assert!(suggested_tools(&enabled_without_channel, &tools, "写个小说").is_none());
        // 空提问不送审。
        let options = SuggestOptions {
            enabled: true,
            channel: Some(channel("http://127.0.0.1:1")),
            ..SuggestOptions::default()
        };
        assert!(suggested_tools(&options, &tools, "   ").is_none());
        // 工具表里空无一物：没有可送审的候选，不必白跑一次网络。
        assert!(suggested_tools(&options, &[], "写个小说").is_none());
    }

    /// 阈值是唯一判定面：≥0.5 进名单，<0.5 排除；兜底过阈值时整份作废。
    #[test]
    fn the_threshold_decides_the_shortlist() {
        let text = json!({
            "answers": {
                "t0": {"type": "noul", "noul": 0.62},
                "t1": {"type": "noul", "noul": 0.49},
                "no_tool": {"type": "noul", "noul": 0.1},
            }
        })
        .to_string();
        assert_eq!(
            parse_suggestions(&text, JEV, &candidates()).expect("可解析"),
            vec!["subagent"]
        );

        // 恰好落在分界线上算「会用到的」。
        let on_the_line = json!({
            "answers": {
                "t0": {"type": "noul", "noul": 0.5},
                "no_tool": {"type": "noul", "noul": 0.4},
            }
        })
        .to_string();
        assert_eq!(
            parse_suggestions(&on_the_line, JEV, &candidates()).expect("可解析"),
            vec!["subagent"]
        );

        // 字符串形式的概率同样认。
        let as_text = json!({
            "answers": {"t1": {"type": "noul", "noul": "0.8"}}
        })
        .to_string();
        assert_eq!(
            parse_suggestions(&as_text, JEV, &candidates()).expect("可解析"),
            vec!["fetcher"]
        );
    }

    /// 兜底：判定为普通对话时不给出任何建议（即使个别工具过了阈值）。
    #[test]
    fn the_fallback_question_suppresses_every_suggestion() {
        let text = json!({
            "answers": {
                "t0": {"type": "noul", "noul": 0.9},
                "t1": {"type": "noul", "noul": 0.7},
                "no_tool": {"type": "noul", "noul": 0.6},
            }
        })
        .to_string();
        assert!(parse_suggestions(&text, JEV, &candidates())
            .expect("可解析")
            .is_empty());
    }

    /// 一个工具都没过阈值 = 本次无建议，不是错误。
    #[test]
    fn all_below_the_threshold_is_no_suggestion() {
        let text = json!({
            "answers": {
                "t0": {"type": "noul", "noul": 0.3},
                "t1": {"type": "noul", "noul": 0.2},
                "no_tool": {"type": "noul", "noul": 0.3},
            }
        })
        .to_string();
        assert!(parse_suggestions(&text, JEV, &candidates())
            .expect("可解析")
            .is_empty());
    }

    #[test]
    fn unusable_responses_are_rejected() {
        assert!(parse_suggestions("not json", JEV, &candidates()).is_err());
        // 缺 answers、一个 noul 都没有：整份响应不可用。
        assert!(parse_suggestions("{}", JEV, &candidates()).is_err());
        let without_probabilities = json!({
            "answers": {"t0": {"type": "noul"}, "no_tool": {"type": "noul"}}
        })
        .to_string();
        assert!(parse_suggestions(&without_probabilities, JEV, &candidates()).is_err());
    }

    /// 对话补全方式：答案包在 `choices[0].message.content` 里，判定共用同一段代码。
    #[test]
    fn chat_completions_reads_answers_from_message_content() {
        let answers = json!({
            "answers": {
                "t0": {"type": "noul", "noul": 0.8},
                "no_tool": {"type": "noul", "noul": 0.1},
            }
        })
        .to_string();
        let text = json!({"choices": [{"message": {"content": answers}}]}).to_string();
        assert_eq!(
            parse_suggestions(&text, CHAT, &candidates()).expect("可解析"),
            vec!["subagent"]
        );
    }

    /// 插入行只在有名单时生成；名单拼进 user 尾部（另起一行）。
    #[test]
    fn the_insertion_line_is_appended_to_the_user_text() {
        assert_eq!(
            suggestion_line(&["subagent".to_string(), "fetcher".to_string()]).expect("有名单"),
            "当前请求建议调用tool：subagent、fetcher。"
        );
        assert!(suggestion_line(&[]).is_none());

        let inserted = insert_suggestion("帮我写个小说", &["subagent".to_string()]);
        assert_eq!(inserted, "帮我写个小说\n\n当前请求建议调用tool：subagent。");
        // 无建议时原文照发。
        assert_eq!(insert_suggestion("你好", &[]), "你好");
    }

    /// 展示层拆行：只认末尾那一行带固定前缀的建议行，其余文本原样返回。
    #[test]
    fn the_suggestion_line_splits_off_the_user_text_for_display() {
        assert_eq!(
            split_suggestion("帮我写个小说\n\n当前请求建议调用tool：subagent、fetcher。"),
            (
                "帮我写个小说".to_string(),
                Some("当前请求建议调用tool：subagent、fetcher。".to_string())
            )
        );
        // 没有建议行（旧转录、开关没开）时整段照旧当正文。
        assert_eq!(
            split_suggestion("帮我写个小说"),
            ("帮我写个小说".to_string(), None)
        );
        assert_eq!(
            split_suggestion("帮我写个小说\n\n这句话里提到当前请求建议调用tool：但没有换行"),
            (
                "帮我写个小说\n\n这句话里提到当前请求建议调用tool：但没有换行".to_string(),
                None
            )
        );
    }

    /// 只有当前提问正文与工具清单进 state：不带任何历史。
    #[test]
    fn the_state_carries_only_the_current_prompt_and_tools() {
        let state = suggest_state("  帮我   写个小说  ", &candidates());
        assert_eq!(state["user_prompt"], "帮我 写个小说");
        assert_eq!(state["tools"][0]["name"], "subagent");
        assert_eq!(state["tools"][0]["summary"], "调度子代理");
        assert_eq!(
            state.as_object().map(|map| map.len()),
            Some(2),
            "state 只有提问正文与工具清单"
        );

        let long = suggest_state(&"请".repeat(USER_PROMPT_MAX_CHARS + 20), &candidates());
        let prompt = long["user_prompt"].as_str().unwrap_or_default();
        assert_eq!(prompt.chars().count(), USER_PROMPT_MAX_CHARS + 1);
        assert!(prompt.ends_with('…'));
    }

    /// 提问形状：每个候选一个 noul + 一个兜底提问，规则写进 instructions。
    #[test]
    fn questions_cover_every_candidate_plus_the_fallback() {
        let questions = suggest_questions(&candidates());
        assert_eq!(questions["t0"]["type"], "noul");
        assert_eq!(questions["t1"]["type"], "noul");
        assert_eq!(questions[NO_TOOL_QUESTION_ID]["type"], "noul");
    }

    #[test]
    fn missing_credentials_skip_the_suggestion() {
        let options = SuggestOptions {
            enabled: true,
            channel: Some(SuggestChannel {
                api_key: String::new(),
                ..channel("http://127.0.0.1:1")
            }),
            ..SuggestOptions::default()
        };
        assert!(suggested_tools(&options, &inventory(&["subagent"]), "写个小说").is_none());
    }

    /// 本地回环：一次 `/v1/decide` 往返，验证请求形状与建议名单的解析。
    #[test]
    fn request_shape_and_shortlist_round_trip_over_the_wire() {
        let cassette = SuggestCassette::serve();
        let options = SuggestOptions {
            enabled: true,
            channel: Some(channel(&cassette.base_url())),
            ..SuggestOptions::default()
        };
        let tools = inventory(&["subagent", "fetcher", "read"]);
        let suggested = suggested_tools(&options, &tools, "帮我把这个任务拆成几路并行")
            .expect("回环应当给出建议");
        assert_eq!(suggested, vec!["subagent", "fetcher"]);

        let captured = cassette.captured();
        assert_eq!(captured.path, "/v1/decide");
        assert_eq!(captured.authorization, "Bearer jv_test");
        assert_eq!(captured.model, "jev-latest");
        assert_eq!(
            captured.question_types,
            vec!["noul", "noul", "noul", "noul"],
            "三个候选 + 一个兜底提问"
        );
        // 工具表里的工具全部送审，说明取描述首句。
        assert!(captured.state.contains("\"subagent\""), "{}", captured.state);
        assert!(captured.state.contains("\"read\""), "{}", captured.state);
        assert!(captured.state.contains("subagent 的说明"), "{}", captured.state);
        assert!(
            captured.state.contains("帮我把这个任务拆成几路并行"),
            "{}",
            captured.state
        );
    }

    /// 决策服务的本地回环：固定回答概率，并记录收到的请求。
    struct SuggestCassette {
        base_url: String,
        captured: Arc<std::sync::Mutex<CapturedSuggestRequest>>,
    }

    #[derive(Default, Clone)]
    struct CapturedSuggestRequest {
        path: String,
        authorization: String,
        model: String,
        state: String,
        question_types: Vec<String>,
    }

    impl SuggestCassette {
        fn serve() -> Self {
            use std::io::{BufRead, BufReader, Read, Write};
            use std::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
            let port = listener.local_addr().expect("本地地址").port();
            let captured = Arc::new(std::sync::Mutex::new(CapturedSuggestRequest::default()));
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
                let questions = request
                    .get("questions")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                // 除兜底提问外，其余提问顺序即送审顺序（`t0`、`t1`…）。
                let tool_questions = questions
                    .keys()
                    .filter(|key| key.as_str() != NO_TOOL_QUESTION_ID)
                    .count();
                {
                    let mut record = seen.lock().expect("记录未被毒化");
                    record.path = path;
                    record.authorization = authorization;
                    record.model = request
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    record.state = request
                        .get("state")
                        .cloned()
                        .unwrap_or(Value::Null)
                        .to_string();
                    record.question_types = (0..tool_questions)
                        .map(|index| {
                            questions
                                .get(&tool_question_id(index))
                                .and_then(|value| value.get("type"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string()
                        })
                        .chain(std::iter::once(
                            questions
                                .get(NO_TOOL_QUESTION_ID)
                                .and_then(|value| value.get("type"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        ))
                        .collect();
                }
                // 三候选中：过阈值的是 subagent 与 fetcher，兜底提问低。
                let answers = json!({
                    "t0": {"type": "noul", "noul": 0.91},
                    "t1": {"type": "noul", "noul": 0.77},
                    "t2": {"type": "noul", "noul": 0.12},
                    "no_tool": {"type": "noul", "noul": 0.05},
                });
                let payload =
                    json!({"model": "jev-1.13.0", "answers": answers}).to_string();
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

        fn captured(&self) -> CapturedSuggestRequest {
            self.captured.lock().expect("记录未被毒化").clone()
        }
    }
}
