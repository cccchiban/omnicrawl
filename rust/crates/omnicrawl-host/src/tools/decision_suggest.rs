//! 建议：用户发来新提问时，先问一次决策模型「这次要用哪些工具／哪些子代理」，把结果拼进 user 尾部。
//!
//! 两类建议各有一个开关、各是一条独立的出站请求，都默认可选、默认关闭：
//!
//! * 工具建议（`decision_models.toml` 的 `[features] tool_suggestion`）：候选是**本宿主工具表里
//!   全部已启用的工具**（工具名 + 描述首句截断到 [`CANDIDATE_SUMMARY_MAX_CHARS`] 字）。
//! * 子代理建议（`[features] subagent_suggestion`）：候选是**本宿主可调度的全部子代理角色**
//!   （角色名 + 定义里的一句话描述），与 `subagent` 工具的参数枚举同源。
//!
//! 两者共用同一套机制：只拿**当前这条提问的正文**（不带历史）与候选清单，向决策服务提一组
//! noul 提问——每个候选一个「本次是否需要它」，外加一个兜底提问（本次是否属于无需任何候选的
//! 普通对话）。两类同时开启时由 [`suggestion_lines`] 并发发起，总耗时取两者的较大者而不是
//! 相加。
//!
//! 候选不做人工取舍：`disabled_tools` 关掉的、当前没有运行期的工具（语音合成、顾问、Windows
//! 组）都已被工具表挡在外面，进表的即送审，工具表增删后清单自动跟着变；子代理角色同理，只列
//! 内核真能调度的角色。该用专用工具还是顺手用泛化工具，交给决策模型按
//! [`SuggestKind::requirements`] 判断——专用工具（子代理、生图、语音、桌面操作等）贴合任务
//! 特殊性，泛化工具（读写文件、搜索代码、执行命令、待办、提问）几乎每轮都可能顺手用到，
//! 除非本次任务围绕它展开，否则都该判为用不到。
//!
//! 判定面只有一条阈值：[`SUGGEST_THRESHOLD`]（0.5）。概率 ≥ 阈值进建议名单，< 阈值排除；
//! 兜底提问 ≥ 阈值时整份建议作废（普通对话、寒暄或无从判断的异常任务不该被塞进提示）。
//! 名单非空时在 user 尾部追加一行（工具 `当前请求建议调用tool：a、b。`，子代理
//! `当前请求建议使用子代理：explore、plan。`），两类同时命中就各占一行。
//!
//! **失败一律 fail-open**：开关没开、没有可用决策渠道、缺凭据、请求失败、响应不可解析都按
//! 「本次无建议」处理，user 原文照发（与检索重排、提问托管、工具淘汰同一语义）：建议只是提示，
//! 值不当为此挡住提问。
//!
//! 出网脱敏复用审查通道那套 `[desensitization]` 旁路（[`crate::review::masking_from_config`]）：
//! 提问正文同样屏蔽后再外发；脱敏构造失败按「建议不可用」处理，绝不外发原文。

use std::sync::Arc;
use std::thread;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::decision_model::{
    load_decision_model_configuration, load_decision_switches, DecisionChannelConfig,
};
use omnicrawl_core::diagnostics;
use serde_json::{json, Map, Value};

use crate::review::{masking_from_config, ReviewMasking};

/// 送审的候选项说明：取描述首句，截断到这么多字符（超出加省略号）。
///
/// 够决策模型分辨「这个工具或子代理是干什么的」，又不至于把请求撑大——工具表里的描述动辄
/// 上百字，全量送审会让请求体随候选数线性膨胀。
pub const CANDIDATE_SUMMARY_MAX_CHARS: usize = 24;

/// 置信度分界线：≥ 该值的候选项进建议名单，< 该值的排除。
pub const SUGGEST_THRESHOLD: f64 = 0.5;

/// 一次建议请求的超时（决策服务本身是 70–500ms 量级，余量留给网络）。
const SUGGEST_TIMEOUT_SECONDS: u64 = 20;
/// 提问正文进请求前截断到的字符数。
const USER_PROMPT_MAX_CHARS: usize = 1_200;
/// 候选项提问 ID 的前缀（`t0`、`t1`…），下标与送审顺序一致。
const CANDIDATE_QUESTION_PREFIX: &str = "t";

/// 与阈值一起写进提问的通用要求：只依据本条提问正文判定，不因候选存在就判为需要。
const COMMON_REQUIREMENTS: &str =
    "只依据 state.user_prompt 这一条提问的正文判断，不要因为候选存在就判定为需要。";

/// 工具建议的选择要求。
///
/// 候选不做人工取舍（进工具表的都送审），因此要求必须说清专用工具与泛化工具的分量差别，
/// 否则泛化工具几乎每轮都会被判为「会用到」，把真正贴合任务的那一个挤下去。
const TOOL_REQUIREMENTS: &str = "本次只会用到下列工具中的一部分，请逐个判断：判定概率 ≥0.5 表示本次任务会用到该工具，\
  <0.5 表示用不到。优先识别贴合本次任务特殊性的专用工具（例如要拆并行子任务用子代理、要出图用生图、要桌面操作\
  找窗口工具）；读文件、写文件、搜代码、执行命令、记待办、向用户提问这类通用工具几乎每轮都能\
  顺手用一下，除非本次任务明确围绕它展开，否则判为用不到。";

/// 子代理建议的选择要求。
///
/// 子代理天生「贵」：每个角色都是一次带独立上下文的子回合，因此只有能拆成独立子任务、
/// 或需要隔离视角核查与隔离改动的活才该判为用得到；一次问答、小改动、串行依赖的活都判为用不到。
const SUBAGENT_REQUIREMENTS: &str = "本次只会用到下列子代理中的一个或几个，请逐个判断：判定概率 ≥0.5 表示本次任务\
  适合交给该子代理，<0.5 表示不需要。每个子代理都是一次开销明显高于你自己动手的独立子回合，\
  因此只有能拆成相互独立、可并行推进的子任务，或需要隔离视角核查、需要隔离改动时才算用得到；\
  单步问答、小改动、串行依赖的活，以及你自己直接做更省事的任务，一律判为用不到。";

/// 建议种类：两类建议形状相同（一次请求带一组 noul 提问），开关、候选来源与文案不同，
/// 因此共用同一套请求、解析与插入代码，只在候选来源、措辞与建议行前缀上分叉。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestKind {
    /// 工具建议（`[features] tool_suggestion`）：候选是本宿主工具表里全部已启用的工具。
    Tools,
    /// 子代理建议（`[features] subagent_suggestion`）：候选是本宿主可调度的子代理角色。
    Subagents,
}

impl SuggestKind {
    /// 兜底提问 ID：本次是否属于无需任何候选的普通对话。
    pub fn fallback_question_id(self) -> &'static str {
        match self {
            SuggestKind::Tools => "no_tool",
            SuggestKind::Subagents => "no_subagent",
        }
    }

    /// `state` 里候选清单的字段名。
    fn state_field(self) -> &'static str {
        match self {
            SuggestKind::Tools => "tools",
            SuggestKind::Subagents => "subagents",
        }
    }

    /// 单个候选项的提问正文。
    fn question(self, name: &str, summary: &str) -> String {
        match self {
            SuggestKind::Tools => format!("本次任务是否需要使用工具 {name}（{summary}）？"),
            SuggestKind::Subagents => {
                format!("本次任务是否需要调度子代理 {name}（{summary}）？")
            }
        }
    }

    /// 兜底提问正文（本次是否属于无需任何候选的普通对话）。
    fn fallback_question(self) -> &'static str {
        match self {
            SuggestKind::Tools => {
                "本次任务是否属于普通对话、寒暄或无从判断的异常任务，因而不需要使用 \
                 state.tools 里的任何工具？概率 ≥0.5 表示确实无需任何工具，<0.5 表示需要用到其中某个工具。"
            }
            SuggestKind::Subagents => {
                "本次任务是否属于普通对话、寒暄或无从判断的异常任务，因而不需要调度 \
                 state.subagents 里的任何子代理？概率 ≥0.5 表示确实无需任何子代理，<0.5 表示需要调度其中某个子代理。"
            }
        }
    }

    /// 选择要求（写进每个提问，决策模型据此判定）。
    fn requirements(self) -> &'static str {
        match self {
            SuggestKind::Tools => TOOL_REQUIREMENTS,
            SuggestKind::Subagents => SUBAGENT_REQUIREMENTS,
        }
    }

    /// 建议行的固定前缀：插入文本与展示层拆行共用同一份口径。
    fn line_prefix(self) -> &'static str {
        match self {
            SuggestKind::Tools => "当前请求建议调用tool：",
            SuggestKind::Subagents => "当前请求建议使用子代理：",
        }
    }

    /// 建议不可用时的诊断前缀。
    fn unavailable_note(self) -> &'static str {
        match self {
            SuggestKind::Tools => "工具建议不可用，按无建议处理",
            SuggestKind::Subagents => "子代理建议不可用，按无建议处理",
        }
    }
}

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

    /// 生效的内联凭据：只认配置里的明文密钥。
    pub fn resolve_api_key(&self) -> String {
        self.api_key.trim().to_string()
    }
}

/// 一次建议请求的全部输入。
pub struct SuggestRequest<'a> {
    /// 建议种类（决定措辞、兜底提问与候选清单字段名）。
    pub kind: SuggestKind,
    pub channel: &'a SuggestChannel,
    /// 出网脱敏旁路；`None` 表示脱敏未启用。
    pub masking: Option<&'a ReviewMasking>,
    /// 用户本次的提问正文（截断后进 `state`）。
    pub user_prompt: &'a str,
    /// 送审的候选（`(名称, 一句话说明)`：工具来自本宿主工具表，子代理来自角色定义），
    /// 下标即提问下标。
    pub candidates: &'a [(String, String)],
}

/// 建议执行器：真实实现走决策服务；测试注入桩以便不起网络。
pub trait SuggestClient: Send + Sync {
    /// 返回建议使用的候选名（按送审顺序）；`Err` 表示本次建议不可用（调用方按无建议处理）。
    fn suggest(&self, request: &SuggestRequest<'_>) -> Result<Vec<String>, String>;
}

/// 一类建议的运行期（工具建议与子代理建议各一份）。
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
    suggest_options_with_switch(
        environment,
        omnicrawl_config::features::decision_model::DECISION_SWITCH_TOOL_SUGGEST,
    )
}

/// 按配置装配子代理建议运行期（与工具建议同一口径，开关不同）。
pub fn subagent_suggest_options_from_config(environment: &ConfigEnvironment) -> SuggestOptions {
    suggest_options_with_switch(
        environment,
        omnicrawl_config::features::decision_model::DECISION_SWITCH_SUBAGENT_SUGGEST,
    )
}

/// 按开关键装配一类建议运行期。
fn suggest_options_with_switch(environment: &ConfigEnvironment, switch: &str) -> SuggestOptions {
    if !load_decision_switches(environment, None)
        .get(switch)
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

/// 从子代理角色清单挑出送审候选：`(角色名, 角色描述)` 直接进审，说明同样取首句截断。
///
/// 角色清单来自与 `subagent` 工具参数枚举同源的发现结果（本宿主真能调度的角色），因此
/// 未启用子代理、定义被删、名字冲突被覆盖等情况都不必在这里另行判断。
pub fn subagent_candidates(roles: &[(String, String)]) -> Vec<(String, String)> {
    roles
        .iter()
        .map(|(name, description)| (name.clone(), tool_summary(description)))
        .collect()
}

/// 描述 → 送审用的一句话：剥掉「是什么：」前缀，取首句，压成单空格后截断到
/// [`CANDIDATE_SUMMARY_MAX_CHARS`] 字。
pub fn tool_summary(description: &str) -> String {
    let body = description.trim();
    let body = body.strip_prefix("是什么：").unwrap_or(body);
    let first_sentence = body.split(['。', '；', '！']).next().unwrap_or(body);
    let collapsed = first_sentence.split_whitespace().collect::<Vec<_>>().join(" ");
    let characters: Vec<char> = collapsed.chars().collect();
    if characters.len() <= CANDIDATE_SUMMARY_MAX_CHARS {
        return collapsed;
    }
    let head: String = characters[..CANDIDATE_SUMMARY_MAX_CHARS].iter().collect();
    format!("{head}…")
}

/// 问一次决策模型「本次该用哪些候选」，返回建议名单（按送审顺序）。
///
/// `None` 表示本次没有建议：开关没开、没有可用渠道、候选清单为空、请求失败、响应不可解析，
/// 或判定结果里一个候选都没过阈值（含兜底提问判定为「无需任何候选」）。
/// 调用方按「user 原文照发」处理。
pub fn suggested_names(
    kind: SuggestKind,
    options: &SuggestOptions,
    candidates: &[(String, String)],
    user_prompt: &str,
) -> Option<Vec<String>> {
    if !options.active() || user_prompt.trim().is_empty() {
        return None;
    }
    let channel = options.channel.as_ref()?;
    if candidates.is_empty() {
        return None;
    }
    let request = SuggestRequest {
        kind,
        channel,
        masking: options.masking.as_deref(),
        user_prompt,
        candidates,
    };
    match options.client.suggest(&request) {
        Ok(names) if names.is_empty() => None,
        Ok(names) => Some(names),
        Err(error) => {
            diagnostics::warn(format!("[host] {}：{error}", kind.unavailable_note()));
            None
        }
    }
}

/// 工具建议：问一次决策模型「本次该用哪些工具」，候选直接取自工具表。
pub fn suggested_tools(
    options: &SuggestOptions,
    inventory: &[(String, String)],
    user_prompt: &str,
) -> Option<Vec<String>> {
    let candidates = suggest_candidates(inventory);
    suggested_names(SuggestKind::Tools, options, &candidates, user_prompt)
}

/// 子代理建议：问一次决策模型「本次该调度哪些子代理」，候选为可调度的角色。
pub fn suggested_subagents(
    options: &SuggestOptions,
    roles: &[(String, String)],
    user_prompt: &str,
) -> Option<Vec<String>> {
    let candidates = subagent_candidates(roles);
    suggested_names(SuggestKind::Subagents, options, &candidates, user_prompt)
}

/// 建议名单拼成的插入行；名单为空时返回 `None`（不插入任何内容）。
pub fn suggestion_line(kind: SuggestKind, names: &[String]) -> Option<String> {
    let names: Vec<&str> = names
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .collect();
    if names.is_empty() {
        return None;
    }
    Some(format!("{}{}。", kind.line_prefix(), names.join("、")))
}

/// 把建议插入 user 尾部（每个建议各占一行）；没有建议时原样返回。
pub fn insert_suggestions(text: &str, lines: &[String]) -> String {
    let lines: Vec<&str> = lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        return text.to_string();
    }
    format!("{text}\n\n{}", lines.join("\n"))
}

/// 从 user 文本里拆出 [`insert_suggestions`] 拼进去的建议行，返回（提问正文，建议行）。
///
/// 建议行是**唱给模型看的提示**，不是用户说的内容，但落进会话流的仍是拼接后的整段文本
/// （内核与转录都按它走）。展示层在这里把它们拆回两半：建议行挂到 `user：` 标签上，正文照旧
/// 逐行铺开——重放读到的历史文本同样含这些行，因此实时提交与回放走同一条拆解口径。
///
/// 两类建议同时命中时尾部有连续多行，因此这里从末尾**连续**剥离带前缀的行，遇到第一行
/// 正文就停手（正文里偶尔提到前缀但不以之开头，不会被误拆）。
pub fn split_suggestion(text: &str) -> (String, Option<String>) {
    let mut body = text.trim_end_matches(['\n', '\r']).to_string();
    let mut lines: Vec<String> = Vec::new();
    loop {
        let Some((head, last)) = body.rsplit_once('\n') else {
            break;
        };
        let line = last.trim();
        if !is_suggestion_line(line) {
            break;
        }
        lines.push(line.to_string());
        body = head.trim_end_matches(['\n', '\r']).to_string();
    }
    if lines.is_empty() {
        return (text.to_string(), None);
    }
    lines.reverse();
    (body, Some(lines.join(" ")))
}

/// 这一行是不是我们拼上去的建议行。
fn is_suggestion_line(line: &str) -> bool {
    [SuggestKind::Tools, SuggestKind::Subagents]
        .iter()
        .any(|kind| line.starts_with(kind.line_prefix()))
}

/// 一次建议请求的 `state`：只有本次提问正文与候选清单，不带任何历史。
fn suggest_state(kind: SuggestKind, user_prompt: &str, candidates: &[(String, String)]) -> Value {
    let items: Vec<Value> = candidates
        .iter()
        .map(|(name, summary)| json!({"name": name, "summary": summary}))
        .collect();
    let mut state = Map::new();
    state.insert(
        "user_prompt".to_string(),
        json!(crate::decision_wire::collapse_text(
            user_prompt,
            USER_PROMPT_MAX_CHARS
        )),
    );
    state.insert(kind.state_field().to_string(), Value::Array(items));
    Value::Object(state)
}

/// 一次建议请求的 `questions`：每个候选一个 noul，外加一个兜底提问。
fn suggest_questions(kind: SuggestKind, candidates: &[(String, String)]) -> Value {
    let mut questions = Map::new();
    for (index, (name, summary)) in candidates.iter().enumerate() {
        questions.insert(
            candidate_question_id(index),
            json!({
                "type": "noul",
                "instructions": format!(
                    "{}判定概率 ≥0.5 表示会用到，<0.5 表示用不到。{}{}",
                    kind.question(name, summary),
                    kind.requirements(),
                    COMMON_REQUIREMENTS,
                ),
            }),
        );
    }
    questions.insert(
        kind.fallback_question_id().to_string(),
        json!({"type": "noul", "instructions": kind.fallback_question()}),
    );
    Value::Object(questions)
}

/// 候选项的提问 ID：下标与送审顺序一致（`t0`、`t1`…）。
fn candidate_question_id(index: usize) -> String {
    format!("{CANDIDATE_QUESTION_PREFIX}{index}")
}

/// 真实实现：一次请求带 N 个 noul 提问，读回 ≥ 阈值的候选名。
///
/// 公开是因为本地 REST 决策接口（`omnicrawl-decision`）可以复用这一条出站路径。
pub struct DecisionSuggestClient;

impl SuggestClient for DecisionSuggestClient {
    fn suggest(&self, request: &SuggestRequest<'_>) -> Result<Vec<String>, String> {
        let kind = request.kind;
        let channel = request.channel;
        let api_key = channel.resolve_api_key();
        if api_key.is_empty() && crate::decision_wire::requires_api_key(&channel.mode) {
            return Err(format!(
                "缺少 API Key：请设置环境变量 {}。",
                channel.api_key_env
            ));
        }
        let state = suggest_state(kind, request.user_prompt, request.candidates);
        let questions = suggest_questions(kind, request.candidates);
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
        parse_suggestions(kind, &text, &channel.mode, request.candidates)
    }
}

/// 解析每个 noul 提问的判定：读回 ≥ 阈值的候选名。
///
/// 兜底提问（`no_tool` / `no_subagent`）过阈值时整份建议作废——返回空名单，调用方不插入
/// 任何内容。一条可识别的概率都没有时按不可用处理（`Err`），调用方同样不插入内容。
fn parse_suggestions(
    kind: SuggestKind,
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

    // 兜底：判定为「无需任何候选」时不给出建议。
    let fallback = kind.fallback_question_id();
    if probability(fallback).is_some_and(|value| value >= SUGGEST_THRESHOLD) {
        return Ok(Vec::new());
    }

    let mut answered = probability(fallback).is_some();
    let mut suggested: Vec<String> = Vec::new();
    for (index, (name, _)) in candidates.iter().enumerate() {
        let Some(value) = probability(&candidate_question_id(index)) else {
            continue;
        };
        answered = true;
        if value >= SUGGEST_THRESHOLD {
            suggested.push(name.clone());
        }
    }
    if !answered {
        return Err(format!(
            "决策模型未返回可用的判定：{}",
            crate::decision_wire::collapse_text(text, 200)
        ));
    }
    Ok(suggested)
}

/// 两类建议并发发起，返回要拼进 user 尾部的建议行（顺序固定：工具在前，子代理在后）。
///
/// 两条请求彼此独立，串行发就要等两次完整往返。这里一次开两条线程并发发起，总耗时取两者
/// 的较大者而不是相加；任一失败或没有建议都只是少一行（fail-open），不影响另一条，
/// 也不影响提问本身。
pub struct SuggestionInputs<'a> {
    /// 工具建议运行期与送审候选（工具表清单）；`options.active()` 为假时不发请求。
    pub tools: (&'a SuggestOptions, &'a [(String, String)]),
    /// 子代理建议运行期与送审候选（角色清单）。
    pub subagents: (&'a SuggestOptions, &'a [(String, String)]),
    /// 用户本次的提问正文。
    pub user_prompt: &'a str,
}

/// 并发发起两类建议，返回拼好顺序的建议行；两者都没有建议时返回空列表。
pub fn suggestion_lines(inputs: &SuggestionInputs<'_>) -> Vec<String> {
    let prompt = inputs.user_prompt;
    if prompt.trim().is_empty() {
        return Vec::new();
    }
    let (tool_options, tool_candidates) = inputs.tools;
    let (subagent_options, subagent_candidates) = inputs.subagents;
    let tools_enabled = tool_options.active() && !tool_candidates.is_empty();
    let subagents_enabled = subagent_options.active() && !subagent_candidates.is_empty();
    if !tools_enabled && !subagents_enabled {
        return Vec::new();
    }
    // 只有一类真正要发时不必起线程：省下一次调度，也不引入无谓的等待。
    if tools_enabled && !subagents_enabled {
        return lines_for(
            SuggestKind::Tools,
            suggested_names(SuggestKind::Tools, tool_options, tool_candidates, prompt),
        );
    }
    if subagents_enabled && !tools_enabled {
        return lines_for(
            SuggestKind::Subagents,
            suggested_names(
                SuggestKind::Subagents,
                subagent_options,
                subagent_candidates,
                prompt,
            ),
        );
    }
    let tools_options = tool_options.clone();
    let subagent_options = subagent_options.clone();
    let prompt = prompt.to_string();
    let tools_candidates = tool_candidates.to_vec();
    let subagent_candidates = subagent_candidates.to_vec();
    let tools_task = thread::spawn(move || {
        suggested_names(SuggestKind::Tools, &tools_options, &tools_candidates, &prompt)
    });
    // 子代理这一条留在本线程跑：两条同时在飞，总耗时取较大者而不是相加。
    let subagents = suggested_names(
        SuggestKind::Subagents,
        &subagent_options,
        &subagent_candidates,
        inputs.user_prompt,
    );
    let tools = tools_task.join().unwrap_or(None);
    let mut lines = lines_for(SuggestKind::Tools, tools);
    lines.extend(lines_for(SuggestKind::Subagents, subagents));
    lines
}

/// 一类建议的结果 → 建议行（无建议时为空列表）。
fn lines_for(kind: SuggestKind, names: Option<Vec<String>>) -> Vec<String> {
    names
        .and_then(|names| suggestion_line(kind, &names))
        .into_iter()
        .collect()
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
    use std::time::Duration;

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

    fn roles() -> Vec<(String, String)> {
        vec![
            ("explore".to_string(), "快速只读探索代码".to_string()),
            ("plan".to_string(), "制定只读实施计划".to_string()),
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
        let long = tool_summary(&format!(
            "是什么：{}。",
            "长".repeat(CANDIDATE_SUMMARY_MAX_CHARS + 5)
        ));
        assert_eq!(long.chars().count(), CANDIDATE_SUMMARY_MAX_CHARS + 1);
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

    /// 子代理候选与工具候选同一口径：角色名进审，说明取角色描述首句。
    #[test]
    fn subagent_candidates_follow_the_roles() {
        let listed = subagent_candidates(&roles());
        assert_eq!(
            listed.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            vec!["explore", "plan"]
        );
        assert_eq!(listed[0].1, "快速只读探索代码");
        assert!(subagent_candidates(&[]).is_empty());
    }

    /// 选择要求写进提问：阈值、专用工具的分量差别、调用方种类措辞都在。
    #[test]
    fn instructions_carry_the_threshold_and_the_selection_requirements() {
        let questions = suggest_questions(SuggestKind::Tools, &candidates());
        let instructions = questions["t0"]["instructions"].as_str().unwrap_or_default();
        assert!(instructions.contains("subagent"), "{instructions}");
        assert!(instructions.contains("0.5"), "阈值要告知决策模型：{instructions}");
        assert!(
            instructions.contains("通用工具"),
            "泛化工具的分量差别要写清：{instructions}"
        );
        assert!(TOOL_REQUIREMENTS.contains("专用工具"), "{TOOL_REQUIREMENTS}");

        let subagent = suggest_questions(SuggestKind::Subagents, &roles());
        let instructions = subagent["t0"]["instructions"]
            .as_str()
            .unwrap_or_default();
        assert!(instructions.contains("子代理 explore"), "{instructions}");
        assert!(instructions.contains("0.5"), "{instructions}");
        assert!(
            instructions.contains("独立子回合"),
            "子代理开销更高的缘由要写清：{instructions}"
        );
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
        assert!(suggested_subagents(&options, &roles(), "   ").is_none());
        // 工具表里空无一物：没有可送审的候选，不必白跑一次网络。
        assert!(suggested_tools(&options, &[], "写个小说").is_none());
        assert!(suggested_subagents(&options, &[], "写个小说").is_none());
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
            parse_suggestions(SuggestKind::Tools, &text, JEV, &candidates()).expect("可解析"),
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
            parse_suggestions(SuggestKind::Tools, &on_the_line, JEV, &candidates()).expect("可解析"),
            vec!["subagent"]
        );

        // 字符串形式的概率同样认。
        let as_text = json!({
            "answers": {"t1": {"type": "noul", "noul": "0.8"}}
        })
        .to_string();
        assert_eq!(
            parse_suggestions(SuggestKind::Tools, &as_text, JEV, &candidates()).expect("可解析"),
            vec!["fetcher"]
        );

        // 同一段判定换成子代理口径：读的是 `no_subagent`，候选取自角色清单。
        let subagents = json!({
            "answers": {
                "t0": {"type": "noul", "noul": 0.81},
                "t1": {"type": "noul", "noul": 0.2},
                "no_subagent": {"type": "noul", "noul": 0.1},
            }
        })
        .to_string();
        assert_eq!(
            parse_suggestions(SuggestKind::Subagents, &subagents, JEV, &roles()).expect("可解析"),
            vec!["explore"]
        );
    }

    /// 兜底：判定为普通对话时不给出任何建议（即使个别候选项过了阈值）。
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
        assert!(parse_suggestions(SuggestKind::Tools, &text, JEV, &candidates())
            .expect("可解析")
            .is_empty());

        // 子代理口径只认自己的兜底提问：`no_tool` 过阈值不该影响它。
        let subagent_fallback = json!({
            "answers": {
                "t0": {"type": "noul", "noul": 0.9},
                "no_subagent": {"type": "noul", "noul": 0.6},
            }
        })
        .to_string();
        assert!(
            parse_suggestions(SuggestKind::Subagents, &subagent_fallback, JEV, &roles())
                .expect("可解析")
                .is_empty()
        );
    }

    /// 一个候选项都没过阈值 = 本次无建议，不是错误。
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
        assert!(parse_suggestions(SuggestKind::Tools, &text, JEV, &candidates())
            .expect("可解析")
            .is_empty());
    }

    #[test]
    fn unusable_responses_are_rejected() {
        assert!(parse_suggestions(SuggestKind::Tools, "not json", JEV, &candidates()).is_err());
        // 缺 answers、一个 noul 都没有：整份响应不可用。
        assert!(parse_suggestions(SuggestKind::Tools, "{}", JEV, &candidates()).is_err());
        let without_probabilities = json!({
            "answers": {"t0": {"type": "noul"}, "no_tool": {"type": "noul"}}
        })
        .to_string();
        assert!(
            parse_suggestions(SuggestKind::Tools, &without_probabilities, JEV, &candidates())
                .is_err()
        );
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
            parse_suggestions(SuggestKind::Tools, &text, CHAT, &candidates()).expect("可解析"),
            vec!["subagent"]
        );
    }

    /// 插入行只在有名单时生成；两类建议各占一行，拼进 user 尾部。
    #[test]
    fn the_insertion_line_is_appended_to_the_user_text() {
        assert_eq!(
            suggestion_line(
                SuggestKind::Tools,
                &["subagent".to_string(), "fetcher".to_string()]
            )
            .expect("有名单"),
            "当前请求建议调用tool：subagent、fetcher。"
        );
        assert_eq!(
            suggestion_line(SuggestKind::Subagents, &["explore".to_string()]).expect("有名单"),
            "当前请求建议使用子代理：explore。"
        );
        assert!(suggestion_line(SuggestKind::Tools, &[]).is_none());

        let tools = suggestion_line(SuggestKind::Tools, &["subagent".to_string()]).expect("有名单");
        assert_eq!(
            insert_suggestions("帮我写个小说", &[tools.clone()]),
            "帮我写个小说\n\n当前请求建议调用tool：subagent。"
        );
        // 两类同时命中：各占一行，工具在前。
        let subagents =
            suggestion_line(SuggestKind::Subagents, &["explore".to_string()]).expect("有名单");
        assert_eq!(
            insert_suggestions("帮我写个小说", &[tools, subagents]),
            "帮我写个小说\n\n当前请求建议调用tool：subagent。\n当前请求建议使用子代理：explore。"
        );
        // 无建议时原文照发。
        assert_eq!(insert_suggestions("你好", &[]), "你好");
    }

    /// 展示层拆行：从末尾连续剥掉带固定前缀的建议行，其余文本原样返回。
    #[test]
    fn the_suggestion_line_splits_off_the_user_text_for_display() {
        assert_eq!(
            split_suggestion("帮我写个小说\n\n当前请求建议调用tool：subagent、fetcher。"),
            (
                "帮我写个小说".to_string(),
                Some("当前请求建议调用tool：subagent、fetcher。".to_string())
            )
        );
        // 两类同时命中：尾部连续两行都拆走，正文不受影响。
        assert_eq!(
            split_suggestion(
                "帮我写个小说\n\n当前请求建议调用tool：subagent。\n当前请求建议使用子代理：explore。"
            ),
            (
                "帮我写个小说".to_string(),
                Some(
                    "当前请求建议调用tool：subagent。 当前请求建议使用子代理：explore。".to_string()
                )
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
        // 正文中间提到前缀不该被剥：只有末尾连续的建议行才算。
        assert_eq!(
            split_suggestion("当前请求建议使用子代理：explore。\n后面还有正文"),
            ("当前请求建议使用子代理：explore。\n后面还有正文".to_string(), None)
        );
    }

    /// 只有当前提问正文与候选清单进 state：不带任何历史，字段名按种类。
    #[test]
    fn the_state_carries_only_the_current_prompt_and_candidates() {
        let state = suggest_state(SuggestKind::Tools, "  帮我   写个小说  ", &candidates());
        assert_eq!(state["user_prompt"], "帮我 写个小说");
        assert_eq!(state["tools"][0]["name"], "subagent");
        assert_eq!(state["tools"][0]["summary"], "调度子代理");
        assert_eq!(
            state.as_object().map(|map| map.len()),
            Some(2),
            "state 只有提问正文与候选清单"
        );

        let subagents = suggest_state(SuggestKind::Subagents, "帮我看看架构", &roles());
        assert_eq!(subagents["subagents"][0]["name"], "explore");
        assert!(subagents.get("tools").is_none(), "两类清单各用各的字段");

        let long = suggest_state(
            SuggestKind::Tools,
            &"请".repeat(USER_PROMPT_MAX_CHARS + 20),
            &candidates(),
        );
        let prompt = long["user_prompt"].as_str().unwrap_or_default();
        assert_eq!(prompt.chars().count(), USER_PROMPT_MAX_CHARS + 1);
        assert!(prompt.ends_with('…'));
    }

    /// 提问形状：每个候选一个 noul + 一个兜底提问，ID 前缀与兜底名按种类。
    #[test]
    fn questions_cover_every_candidate_plus_the_fallback() {
        let questions = suggest_questions(SuggestKind::Tools, &candidates());
        assert_eq!(questions["t0"]["type"], "noul");
        assert_eq!(questions["t1"]["type"], "noul");
        assert_eq!(
            questions[SuggestKind::Tools.fallback_question_id()]["type"],
            "noul"
        );
        assert!(questions.get(SuggestKind::Subagents.fallback_question_id()).is_none());

        let subagents = suggest_questions(SuggestKind::Subagents, &roles());
        assert_eq!(subagents["t0"]["type"], "noul");
        assert_eq!(subagents["t1"]["type"], "noul");
        assert_eq!(
            subagents[SuggestKind::Subagents.fallback_question_id()]["type"],
            "noul"
        );
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
        assert!(suggested_subagents(&options, &roles(), "写个小说").is_none());
    }

    /// 本地回环：一次 `/v1/decide` 往返，验证请求形状与建议名单的解析。
    #[test]
    fn request_shape_and_shortlist_round_trip_over_the_wire() {
        let cassette = SuggestCassette::serve(1, Duration::ZERO);
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
        let captured = captured.first().expect("应当收到一次请求");
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

    /// 子代理建议的回环：候选清单走 `state.subagents`，兜底提问是 `no_subagent`。
    #[test]
    fn subagent_round_trip_uses_its_own_candidates_and_fallback() {
        let cassette = SuggestCassette::serve(1, Duration::ZERO);
        let options = SuggestOptions {
            enabled: true,
            channel: Some(channel(&cassette.base_url())),
            ..SuggestOptions::default()
        };
        let suggested = suggested_subagents(&options, &roles(), "先探索代码再定计划")
            .expect("回环应当给出建议");
        assert_eq!(suggested, vec!["explore", "plan"]);

        let captured = cassette.captured();
        let captured = captured.first().expect("应当收到一次请求");
        assert!(captured.state.contains("\"subagents\""), "{}", captured.state);
        assert!(captured.state.contains("explore"), "{}", captured.state);
        assert!(
            !captured.state.contains("\"tools\""),
            "子代理建议不该带工具清单：{}",
            captured.state
        );
        assert!(
            captured.state.contains("快速只读探索代码"),
            "角色说明取首句：{}",
            captured.state
        );
    }

    /// 两类同时开启：并发发起，总耗时取较大者而不是相加（回环各延迟一份）。
    #[test]
    fn both_kinds_are_requested_concurrently() {
        let delay = Duration::from_millis(300);
        let cassette = SuggestCassette::serve(2, delay);
        let options = SuggestOptions {
            enabled: true,
            channel: Some(channel(&cassette.base_url())),
            ..SuggestOptions::default()
        };
        let tools = inventory(&["subagent", "fetcher", "read"]);
        let started = std::time::Instant::now();
        let lines = suggestion_lines(&SuggestionInputs {
            tools: (&options, &tools),
            subagents: (&options, &roles()),
            user_prompt: "帮我把这个任务拆成几路并行",
        });
        let elapsed = started.elapsed();

        assert_eq!(
            lines,
            vec![
                "当前请求建议调用tool：subagent、fetcher。".to_string(),
                "当前请求建议使用子代理：explore、plan。".to_string(),
            ],
            "顺序固定：工具在前，子代理在后"
        );
        assert_eq!(cassette.captured().len(), 2, "两条请求都要发出去");
        // 串行要 2×delay；并发只需一份多一点，留足抖动余量。
        assert!(
            elapsed < delay * 2,
            "两条请求应当并发：耗时 {elapsed:?} 不该接近两倍延迟"
        );
    }

    /// 只有一类开启时不起线程，也不该白等另一类。
    #[test]
    fn a_single_kind_needs_no_concurrency() {
        let cassette = SuggestCassette::serve(1, Duration::ZERO);
        let options = SuggestOptions {
            enabled: true,
            channel: Some(channel(&cassette.base_url())),
            ..SuggestOptions::default()
        };
        let lines = suggestion_lines(&SuggestionInputs {
            tools: (&options, &inventory(&["subagent", "fetcher", "read"])),
            subagents: (&SuggestOptions::default(), &roles()),
            user_prompt: "帮我把这个任务拆成几路并行",
        });
        assert_eq!(
            lines,
            vec!["当前请求建议调用tool：subagent、fetcher。".to_string()]
        );
        assert_eq!(cassette.captured().len(), 1);
    }

    /// 决策服务的本地回环：固定回答概率，并记录收到的请求。
    ///
    /// `expect_requests` 是预期收到的请求条数（两类并发时是 2），`delay` 用在每条回包之前，
    /// 用来验证并发——串行的话耗时就会翻倍。每条请求单独一条线程，两类并发时才能同时收下。
    struct SuggestCassette {
        base_url: String,
        captured: Arc<std::sync::Mutex<Vec<CapturedSuggestRequest>>>,
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
        /// 起一个只回环的决策服务：收 `expect_requests` 条请求，每条回包前等待 `delay`。
        fn serve(expect_requests: usize, delay: Duration) -> Self {
            use std::io::{BufRead, BufReader, Read, Write};
            use std::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
            let port = listener.local_addr().expect("本地地址").port();
            let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
            let seen = Arc::clone(&captured);
            std::thread::spawn(move || {
                for _ in 0..expect_requests {
                    let Ok((stream, _)) = listener.accept() else {
                        return;
                    };
                    let seen = Arc::clone(&seen);
                    std::thread::spawn(move || {
                        let Ok(mut reader) = stream.try_clone().map(BufReader::new) else {
                            return;
                        };
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
                        let state = request
                            .get("state")
                            .cloned()
                            .unwrap_or(Value::Null)
                            .to_string();
                        // 子代理建议的候选清单走 `state.subagents`，据此区分两类请求。
                        let subagent_request = state.contains("\"subagents\"");
                        let fallback = if subagent_request {
                            "no_subagent"
                        } else {
                            "no_tool"
                        };
                        // 除兜底提问外，其余提问顺序即送审顺序（`t0`、`t1`…）。
                        let candidate_questions = questions
                            .keys()
                            .filter(|key| key.as_str() != fallback)
                            .count();
                        {
                            let mut record = seen.lock().expect("记录未被毒化");
                            record.push(CapturedSuggestRequest {
                                path,
                                authorization,
                                model: request
                                    .get("model")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                state,
                                question_types: (0..candidate_questions)
                                    .map(|index| {
                                        questions
                                            .get(&candidate_question_id(index))
                                            .and_then(|value| value.get("type"))
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_string()
                                    })
                                    .chain(std::iter::once(
                                        questions
                                            .get(fallback)
                                            .and_then(|value| value.get("type"))
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_string(),
                                    ))
                                    .collect(),
                            });
                        }
                        std::thread::sleep(delay);
                        // 工具建议：过阈值的是 subagent 与 fetcher；子代理建议：explore 与 plan。
                        let answers = if subagent_request {
                            json!({
                                "t0": {"type": "noul", "noul": 0.93},
                                "t1": {"type": "noul", "noul": 0.71},
                                "no_subagent": {"type": "noul", "noul": 0.04},
                            })
                        } else {
                            json!({
                                "t0": {"type": "noul", "noul": 0.91},
                                "t1": {"type": "noul", "noul": 0.77},
                                "t2": {"type": "noul", "noul": 0.12},
                                "no_tool": {"type": "noul", "noul": 0.05},
                            })
                        };
                        let payload = json!({"model": "jev-1.13.0", "answers": answers}).to_string();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            payload.len(),
                            payload
                        );
                        let mut writer = stream;
                        let _ = writer.write_all(response.as_bytes());
                        let _ = writer.flush();
                    });
                }
            });
            Self {
                base_url: format!("http://127.0.0.1:{port}"),
                captured,
            }
        }

        fn base_url(&self) -> String {
            self.base_url.clone()
        }

        /// 已收到的请求快照（顺序不定，两条并发时不要依赖下标）。
        fn captured(&self) -> Vec<CapturedSuggestRequest> {
            self.captured.lock().expect("记录未被毒化").clone()
        }
    }
}
