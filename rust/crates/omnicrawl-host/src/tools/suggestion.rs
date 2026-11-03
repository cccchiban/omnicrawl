//! 提问建议：用户发来新提问时，先由主模型识别「技能词」，再用**本地 BM25** 检索本机 Skill
//! 索引，把结果拼进 user 尾部。
//!
//! 只有一个开关（`config.toml` 的 `[suggestion] skills`），默认关闭。候选是**本机已索引的
//! Skill**（名称 + 作用域 + 描述 + 正文），检索通路与 `skill_search` 工具完全一致。
//!
//! 关键词由主模型按提问识别一次（[`KeywordClient`]，提示词见 `suggestion_keywords_system.md`），
//! 不是把整条提问直接丢给 BM25：整句「帮我修改这份文档」里的「这份」「帮我」对候选表毫无
//! 区分度，而「文档」恰好就是 Skill 描述与正文里的词。
//!
//! 匹配全部在本地完成（[`omnicrawl_extensions::bm25`]，毫秒级）：只有关键词识别这一步出网，
//! 且它用的是主模型渠道——那条渠道本来就要收到这条提问，不额外扩大暴露面。识别请求固定按
//! [`KEYWORDS_REASONING_EFFORT`]（`low`）发：它只做一次词性归纳，不该在提交前花掉几秒。
//!
//! 检索名次即最终名次，不再过第二道分数线：检索单元是「名称 + 描述 + 正文」，靠正文命中的
//! Skill 与检索词没有交集，再拿截断过的描述重算一次 BM25 会把整条建议清零。
//!
//! 名单非空时在 user 尾部追加一行（`建议使用skills：pdf、docx。`）。
//!
//! **失败一律 fail-open**：开关没开、没有候选、一个词都没命中都按「本次无建议」处理，user
//! 原文照发。关键词识别不可用时退化为「整条提问当检索词」（[`SuggestKeywords::Raw`]），
//! 建议照旧能出，只是不如识别精确。建议只是提示，值不当为此挡住提问。

use std::path::PathBuf;
use std::sync::Arc;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_core::diagnostics;
use omnicrawl_extensions::skill::SkillSearchHit;
use omnicrawl_llm::{
    build_runtime, ChatRequestInput, DiscardSink, ModelDescriptor, ProviderProfile,
};
use omnicrawl_protocol::{conversation_from_openai_messages, GenerationOptions, ToolSpec};
use serde_json::{json, Value};

/// 建议名单的条数上限：只提示「最该看哪几个」，再多就成噪声。
pub const SUGGESTION_LIMIT: usize = 3;

/// Skill 建议行的固定前缀（插入文本与展示层拆行共用）。
pub const SKILL_SUGGESTION_PREFIX: &str = "建议使用skills：";

/// Skill 送审池上限：检索多取几条再截到 [`SUGGESTION_LIMIT`]，避免同分项被截断误伤。
pub const SKILL_SUGGESTION_POOL: usize = 8;

/// 关键词识别用的系统提示（只输出一个 JSON 对象）。
const KEYWORDS_SYSTEM_PROMPT: &str =
    include_str!("../../../../../rust/assets/templates/suggestion_keywords_system.md");

/// 关键词识别的超时（秒）：它挡在每次提交之前，只输出几十个 token，因此按「宁可退化成
/// 整句检索也不让提问干等」定一个短超时。
pub const KEYWORDS_TIMEOUT_SECONDS: f64 = 10.0;

/// 关键词识别的输出上限：几个词，几十个 token 就够。
const KEYWORDS_MAX_TOKENS: u32 = 512;

/// 关键词识别的推理强度：固定最低档——只做一次词性归纳，别在这一步花掉几秒。
const KEYWORDS_REASONING_EFFORT: &str = "low";

/// 检索词条数上限：词越多，BM25 查询越容易被稀释。
pub const KEYWORDS_LIMIT: usize = 6;

const USER_AGENT: &str = omnicrawl_version::USER_AGENT;

/// 一次检索用的关键词。
///
/// 主模型识别成功时是几个技能词；识别不可用时退化为整条提问。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuggestKeywords {
    /// 主模型识别出的技能词（word、ppt、文档……）。
    Words(Vec<String>),
    /// 识别不可用：整条提问直接当检索词（退化路径）。
    Raw(String),
}

impl Default for SuggestKeywords {
    fn default() -> Self {
        Self::Words(Vec::new())
    }
}

impl SuggestKeywords {
    /// 本次检索词（空格分隔）；空串表示这类建议不必起检索。
    pub fn query(&self) -> String {
        match self {
            SuggestKeywords::Raw(text) => text.trim().to_string(),
            SuggestKeywords::Words(words) => words.join(" ").trim().to_string(),
        }
    }

    /// 检索词是否为空。
    pub fn is_empty(&self) -> bool {
        self.query().is_empty()
    }
}

/// 关键词识别器：真实实现走主模型渠道，测试注入桩以便不起网络。
pub trait KeywordClient: Send + Sync {
    /// 返回本次提问的检索词；`Err` 表示识别不可用（调用方退化为整句检索）。
    fn keywords(&self, user_prompt: &str) -> Result<SuggestKeywords, String>;
}

/// 主模型渠道（关键词识别走这条）：模型、协议、地址与凭据都来自当前主模型视图。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeywordChannel {
    pub model: String,
    pub provider: String,
    pub protocol: String,
    pub base_url: String,
    pub api_key: String,
    pub timeout_seconds: f64,
}

impl KeywordChannel {
    /// 是否可用：模型与地址齐全、凭据非空。
    pub fn active(&self) -> bool {
        !self.model.trim().is_empty()
            && !self.base_url.trim().is_empty()
            && !self.api_key.trim().is_empty()
    }
}

/// 真实实现：一次单轮补全，把返回的 JSON 解成检索词。
pub struct MainModelKeywordClient {
    pub channel: KeywordChannel,
}

impl Default for MainModelKeywordClient {
    fn default() -> Self {
        Self {
            channel: KeywordChannel {
                timeout_seconds: KEYWORDS_TIMEOUT_SECONDS,
                ..KeywordChannel::default()
            },
        }
    }
}

impl KeywordClient for MainModelKeywordClient {
    fn keywords(&self, user_prompt: &str) -> Result<SuggestKeywords, String> {
        if user_prompt.trim().is_empty() {
            return Ok(SuggestKeywords::default());
        }
        if !self.channel.active() {
            return Err("主模型渠道未配置（model / base_url / api_key 缺一）".to_string());
        }
        let profile = ProviderProfile {
            id: String::new(),
            provider: self.channel.provider.clone(),
            base_url: self.channel.base_url.clone(),
            api_key: self.channel.api_key.clone(),
            user_agent: USER_AGENT.to_string(),
            default_protocol: self.channel.protocol.clone(),
        };
        let descriptor = ModelDescriptor {
            model_id: self.channel.model.clone(),
            protocol: self.channel.protocol.clone(),
            capabilities: None,
            context_window_tokens: 0,
            max_output_tokens: Some(KEYWORDS_MAX_TOKENS),
        };
        let bundle = build_runtime(&profile, &descriptor)
            .map_err(|error| format!("关键词识别运行期不可用：{}", error.message))?;

        let timeout_seconds = if self.channel.timeout_seconds > 0.0 {
            self.channel.timeout_seconds
        } else {
            KEYWORDS_TIMEOUT_SECONDS
        };
        let generation = GenerationOptions {
            max_output_tokens: Some(KEYWORDS_MAX_TOKENS),
            reasoning_effort: KEYWORDS_REASONING_EFFORT.to_string(),
            request_timeout_seconds: timeout_seconds,
            request_retry_count: 1,
            ..GenerationOptions::default()
        };
        let messages = vec![json!({
            "role": "user",
            "content": format!("用户本次提问：\n{user_prompt}"),
        })];
        let conversation = conversation_from_openai_messages(&messages);
        let tools: Vec<ToolSpec> = Vec::new();
        let identity = std::collections::BTreeMap::from([
            ("scope".to_string(), "omnicrawl-suggestion".to_string()),
            ("model".to_string(), self.channel.model.clone()),
        ]);
        let input = ChatRequestInput {
            model: self.channel.model.as_str(),
            system_prompt: KEYWORDS_SYSTEM_PROMPT,
            messages: &conversation,
            tools: &tools,
            options: &generation,
            profile_request_timeout_seconds: timeout_seconds,
            prompt_cache_capable: false,
            prompt_cache_identity: &identity,
        };
        let mut sink = DiscardSink;
        let reply = bundle
            .runtime
            .run_turn(&input, &mut sink)
            .map_err(|error| format!("关键词识别请求失败：{error}"))?;
        let text = reply.content.trim();
        if text.is_empty() {
            return Err("关键词识别返回空响应".to_string());
        }
        Ok(parse_keywords_reply(text))
    }
}

/// 按配置装配关键词识别器（无头宿主与测试用：读 `[llm]` 段）。
///
/// 配置读不出来时给一份未配置的渠道：`keywords` 返回 `Err`，调用方退化为整句检索，
/// 不因为缺配置把整条建议链路禁掉。
pub fn keyword_client_from_config(environment: &ConfigEnvironment) -> Arc<dyn KeywordClient> {
    let channel = match omnicrawl_config::models::llm::load_llm_config(environment) {
        Ok(llm) => KeywordChannel {
            model: llm.model,
            provider: llm.provider,
            protocol: llm.protocol,
            base_url: llm.base_url,
            api_key: llm.api_key,
            timeout_seconds: KEYWORDS_TIMEOUT_SECONDS,
        },
        Err(_) => KeywordChannel {
            timeout_seconds: KEYWORDS_TIMEOUT_SECONDS,
            ..KeywordChannel::default()
        },
    };
    keyword_client(channel)
}

/// 按给定渠道装配关键词识别器。
///
/// TUI 用这条：它握着的模型视图比磁盘更即时（命令行 `--model`、运行期切模型都还没写盘），
/// 识别必须跟着那条视图走，否则换了模型还在用旧渠道。
pub fn keyword_client(channel: KeywordChannel) -> Arc<dyn KeywordClient> {
    let channel = KeywordChannel {
        timeout_seconds: if channel.timeout_seconds > 0.0 {
            channel.timeout_seconds
        } else {
            KEYWORDS_TIMEOUT_SECONDS
        },
        ..channel
    };
    Arc::new(MainModelKeywordClient { channel })
}

/// 求本次检索词；识别器缺失或失败时退化为整条提问（并留一条诊断）。
pub fn resolve_keywords(client: Option<&dyn KeywordClient>, user_prompt: &str) -> SuggestKeywords {
    let Some(client) = client else {
        return SuggestKeywords::Raw(user_prompt.to_string());
    };
    match client.keywords(user_prompt) {
        Ok(keywords) => keywords,
        Err(error) => {
            diagnostics::warn(format!(
                "[host] 提问建议的关键词识别不可用，退化为整句检索：{error}"
            ));
            SuggestKeywords::Raw(user_prompt.to_string())
        }
    }
}

/// 一类建议的运行期：只带一个开关，本地匹配不需要任何渠道配置。
#[derive(Debug, Clone, Default)]
pub struct SuggestOptions {
    /// 开关状态。
    pub enabled: bool,
}

impl SuggestOptions {
    /// 是否真正可用：只看开关，本地匹配不需要任何外部条件。
    pub fn active(&self) -> bool {
        self.enabled
    }
}

/// 按配置装配建议运行期（TUI 与 API 共用同一口径）。
pub fn suggest_options_from_config(environment: &ConfigEnvironment) -> SuggestOptions {
    SuggestOptions {
        enabled: omnicrawl_config::features::suggestion::load_suggestion_config(environment, None)
            .enabled(),
    }
}

/// 检索本机 Skill 索引，取出 Skill 建议的候选。
///
/// 复用 `skill_search` 工具那条检索通路：`SkillManager::discover` 扫一次索引（覆盖本会话内
/// 新装的 Skill），`search_meta` 按 BM25 打分，检索单元是名称 + 描述 + 正文。作用域固定
/// `all`：建议是给模型看的提示，不该因为 Skill 装在用户级还是项目级而漏掉。
///
/// 返回的清单本身即名单顺序（名次就是最终名次），调用方只截断不重排。
fn discover_skill_candidates(
    workspace: &std::path::Path,
    environment: &ConfigEnvironment,
    query: &str,
) -> Vec<(String, String)> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    let mut manager = omnicrawl_extensions::skill::SkillManager::new();
    let extra_paths = omnicrawl_config::features::skills::load_skills_config(environment, None)
        .map(|config| config.extra_paths)
        .unwrap_or_default();
    manager.discover(Some(workspace), &extra_paths);
    let hits = manager.search_meta(query, "all", SKILL_SUGGESTION_POOL);
    skill_candidates(&hits)
}

/// Skill 命中项 → 候选清单（名称 + 描述）；名次即优先级。
fn skill_candidates(hits: &[SkillSearchHit]) -> Vec<(String, String)> {
    hits.iter()
        .map(|hit| {
            (
                hit.meta.name.clone(),
                hit.meta.description.trim().to_string(),
            )
        })
        .collect()
}

/// 建议名单拼成的插入行；名单为空时返回 `None`（不插入任何内容）。
fn suggestion_line(names: &[String]) -> Option<String> {
    let names: Vec<&str> = names
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .collect();
    if names.is_empty() {
        return None;
    }
    Some(format!("{SKILL_SUGGESTION_PREFIX}{}。", names.join("、")))
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
/// （内核与转录都按它走）。展示层在这里把它们拆回两半：建议行单独占一块、正文照旧逐行铺开
/// ——重放读到的历史文本同样含这些行，因此实时提交与回放走同一条拆解口径。
pub fn split_suggestion(text: &str) -> (String, Vec<String>) {
    let mut body = text.trim_end_matches(['\n', '\r']).to_string();
    let mut suggestions: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    while let Some((head, last)) = body.rsplit_once('\n') {
        let line = last.trim();
        if is_suggestion_line(line) {
            suggestions.push(line.to_string());
        } else if is_attachment_note_line(line) {
            // 提交路径在建议之后追加图片落盘说明，它会把建议行挡在尾部之外：跳过它继续往前拆。
            notes.push(line.to_string());
        } else {
            break;
        }
        body = head.trim_end_matches(['\n', '\r']).to_string();
    }
    suggestions.reverse();
    notes.reverse();
    if !notes.is_empty() {
        body = format!("{body}\n\n{}", notes.join("\n"));
    }
    (body, suggestions)
}

/// 图片落盘说明的首行（`image_archive::archive_pasted_images` 拼在提问末尾那一段的开头）。
const ATTACHMENT_NOTE_HEADER: &str = "粘贴的图片已保存：";

/// 附件说明块里的一行：首行是表头，其余每行是 `[ #n Image ] → 路径`。
fn is_attachment_note_line(line: &str) -> bool {
    line == ATTACHMENT_NOTE_HEADER || (line.starts_with("[ #") && line.contains("] → "))
}

/// 这一行是不是我们拼上去的建议行。
fn is_suggestion_line(line: &str) -> bool {
    line.starts_with(SKILL_SUGGESTION_PREFIX)
}

/// Skill 建议的候选来源。
#[derive(Debug, Clone)]
pub enum SkillSource {
    /// 测试钉死的候选池（不扫本机索引）。
    Pinned(Vec<(String, String)>),
    /// 按技能词实时检索本机 Skill 索引。
    Live {
        workspace: PathBuf,
        environment: ConfigEnvironment,
    },
}

/// 计算 Skill 建议，返回拼好顺序的建议行；没有建议时返回空列表。
pub fn suggestion_lines(
    options: &SuggestOptions,
    keywords: Option<&dyn KeywordClient>,
    source: &SkillSource,
    user_prompt: &str,
) -> Vec<String> {
    if !options.active() || user_prompt.trim().is_empty() {
        return Vec::new();
    }
    let keywords = resolve_keywords(keywords, user_prompt);
    let query = keywords.query();
    if query.is_empty() {
        return Vec::new();
    }
    // 候选：检索单元是「名称 + 描述 + 正文」，检索词是技能词。
    let candidates = match source {
        SkillSource::Pinned(pinned) => pinned.clone(),
        SkillSource::Live {
            workspace,
            environment,
        } => discover_skill_candidates(workspace, environment, &query),
    };
    let names: Vec<String> = candidates
        .into_iter()
        .take(SUGGESTION_LIMIT)
        .map(|(name, _)| name)
        .collect();
    suggestion_line(&names).into_iter().collect()
}

/// 从模型回复里解出检索词。
///
/// 回复可能带代码围栏或前后解释，因此只截取首个 `{` 到末个 `}` 之间的 JSON；解不出来时返回
/// 空列表（等于本次无建议），不把原文当 JSON 猜。词去空白、去重、截到 [`KEYWORDS_LIMIT`] 条。
pub fn parse_keywords_reply(text: &str) -> SuggestKeywords {
    let body = text.trim();
    let Some(start) = body.find('{') else {
        return SuggestKeywords::default();
    };
    let Some(end) = body.rfind('}') else {
        return SuggestKeywords::default();
    };
    if end <= start {
        return SuggestKeywords::default();
    }
    let Ok(value) = serde_json::from_str::<Value>(&body[start..=end]) else {
        return SuggestKeywords::default();
    };
    SuggestKeywords::Words(keyword_list(&value, "skills"))
}

/// 取关键词数组，去空白、去重并截断。
fn keyword_list(value: &Value, key: &str) -> Vec<String> {
    let mut items: Vec<String> = Vec::new();
    for entry in value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(text) = entry.as_str() else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() || items.iter().any(|item| item == text) {
            continue;
        }
        items.push(text.to_string());
        if items.len() >= KEYWORDS_LIMIT {
            break;
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedKeywords(SuggestKeywords);

    impl KeywordClient for FixedKeywords {
        fn keywords(&self, _user_prompt: &str) -> Result<SuggestKeywords, String> {
            Ok(self.0.clone())
        }
    }

    struct BrokenKeywords;

    impl KeywordClient for BrokenKeywords {
        fn keywords(&self, _user_prompt: &str) -> Result<SuggestKeywords, String> {
            Err("渠道不可用".to_string())
        }
    }

    fn enabled() -> SuggestOptions {
        SuggestOptions { enabled: true }
    }

    fn words(items: &[&str]) -> SuggestKeywords {
        SuggestKeywords::Words(items.iter().map(|item| item.to_string()).collect())
    }

    #[test]
    fn a_disabled_suggestion_produces_nothing() {
        let lines = suggestion_lines(
            &SuggestOptions::default(),
            Some(&FixedKeywords(words(&["pdf"]))),
            &SkillSource::Pinned(vec![("kimi-pdf".to_string(), "处理 PDF".to_string())]),
            "把这份文档转成 pdf",
        );
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn an_empty_prompt_skips_everything() {
        let lines = suggestion_lines(
            &enabled(),
            Some(&FixedKeywords(words(&["pdf"]))),
            &SkillSource::Pinned(vec![("kimi-pdf".to_string(), "处理 PDF".to_string())]),
            "   ",
        );
        assert!(lines.is_empty(), "{lines:?}");
    }

    /// 检索词为空时不去扫 Skill 索引：那一步要读每个 SKILL.md 的正文。
    #[test]
    fn empty_keywords_skip_the_retrieval() {
        let lines = suggestion_lines(
            &enabled(),
            Some(&FixedKeywords(words(&[]))),
            &SkillSource::Pinned(vec![("kimi-pdf".to_string(), "处理 PDF".to_string())]),
            "把这段文字替换一下",
        );
        assert!(lines.is_empty(), "{lines:?}");
    }

    /// 命中钉死的候选池：名单按池子顺序截断。
    #[test]
    fn the_shortlist_keeps_the_retrieval_order() {
        let lines = suggestion_lines(
            &enabled(),
            Some(&FixedKeywords(words(&["pdf"]))),
            &SkillSource::Pinned(vec![
                ("kimi-pdf".to_string(), "处理 PDF".to_string()),
                ("kimi-docx".to_string(), "处理 Word".to_string()),
            ]),
            "把这份文档转成 pdf",
        );
        assert_eq!(
            lines,
            vec!["建议使用skills：kimi-pdf、kimi-docx。".to_string()]
        );
    }

    /// 名单截断到 [`SUGGESTION_LIMIT`] 条，而不是把整个候选池灌进 user 尾部。
    #[test]
    fn the_shortlist_is_capped() {
        let candidates: Vec<(String, String)> = (0..10)
            .map(|index| (format!("skill{index}"), "处理文档".to_string()))
            .collect();
        let names: Vec<String> = candidates
            .into_iter()
            .take(SUGGESTION_LIMIT)
            .map(|(name, _)| name)
            .collect();
        assert_eq!(
            suggestion_line(&names),
            Some("建议使用skills：skill0、skill1、skill2。".to_string())
        );
    }

    /// 识别失败退化为整句检索：建议照旧能出，只是不如识别精确。
    #[test]
    fn a_broken_keyword_client_falls_back_to_the_raw_prompt() {
        let keywords = resolve_keywords(Some(&BrokenKeywords), "把这份 pdf 转成 word");
        assert_eq!(
            keywords,
            SuggestKeywords::Raw("把这份 pdf 转成 word".to_string())
        );
    }

    /// 没有识别器时直接整句检索（旧行为），不因为缺模型渠道把建议关掉。
    #[test]
    fn without_a_keyword_client_the_whole_prompt_is_the_query() {
        let keywords = resolve_keywords(None, "把这份 pdf 转成 word");
        assert_eq!(
            keywords.query(),
            "把这份 pdf 转成 word",
            "整句就是检索词"
        );
    }

    /// 建议行必须能被展示层原样拆回去（实时提交与回放共用同一口径）。
    #[test]
    fn the_inserted_lines_split_back_off() {
        let lines = vec!["建议使用skills：kimi-pdf。".to_string()];
        let text = insert_suggestions("读一下这个文件", &lines);
        let (body, suggestions) = split_suggestion(&text);
        assert_eq!(body, "读一下这个文件");
        assert_eq!(suggestions, lines);
    }

    /// 图片落盘说明挡在尾部时，建议行仍要拆得出来。
    #[test]
    fn attachment_notes_do_not_block_the_split() {
        let text = "看这两张图\n\n建议使用skills：kimi-pdf。\n\n\
                    粘贴的图片已保存：\n[ #1 Image ] → a.png";
        let (body, suggestions) = split_suggestion(text);
        assert_eq!(suggestions, vec!["建议使用skills：kimi-pdf。".to_string()]);
        assert!(body.starts_with("看这两张图"), "{body}");
        assert!(body.ends_with("a.png"), "{body}");
    }

    #[test]
    fn a_reply_is_parsed_into_the_word_list() {
        let keywords = parse_keywords_reply(r#"{"skills": ["word", "文档"]}"#);
        assert_eq!(keywords.query(), "word 文档");
    }

    /// 回复可能带围栏与前后解释：只取首个 `{` 到末个 `}`。
    #[test]
    fn a_fenced_reply_is_parsed() {
        let keywords =
            parse_keywords_reply("结果如下：\n```json\n{\"skills\": [\"pdf\"]}\n```\n");
        assert_eq!(keywords.query(), "pdf");
    }

    #[test]
    fn unparsable_replies_give_no_words() {
        assert!(parse_keywords_reply("我觉得应该用 kimi-pdf").is_empty());
        assert!(parse_keywords_reply("{坏了}").is_empty());
    }

    #[test]
    fn the_word_list_is_deduped_and_capped() {
        let mut entries: Vec<String> = (0..10).map(|index| format!("\"词{index}\"")).collect();
        entries.push("\"词0\"".to_string());
        entries.push("\"  \"".to_string());
        let text = format!("{{\"skills\": [{}]}}", entries.join(","));
        let SuggestKeywords::Words(items) = parse_keywords_reply(&text) else {
            panic!("应当解出词表");
        };
        assert_eq!(items.len(), KEYWORDS_LIMIT, "{items:?}");
        assert!(!items.iter().any(|item| item.trim().is_empty()));
    }

    /// 主模型渠道没配齐时返回 `Err`，由调用方退化为整句检索。
    #[test]
    fn an_unconfigured_channel_reports_itself_as_unavailable() {
        let client = MainModelKeywordClient::default();
        assert!(client.keywords("帮我改文档").is_err());
        assert_eq!(
            resolve_keywords(Some(&client), "帮我改文档"),
            SuggestKeywords::Raw("帮我改文档".to_string())
        );
    }
}
