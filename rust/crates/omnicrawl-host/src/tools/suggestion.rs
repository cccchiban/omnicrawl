//! 提问建议：用户发来新提问时，用**本地 BM25** 从候选里挑出最贴合的几个名字，把结果拼进
//! user 尾部。
//!
//! 三类建议各有一个开关（`config.toml` 的 `[suggestion]`），都默认关闭：
//!
//! * 工具建议（`[suggestion] tools`）：候选是**本宿主工具表里全部已启用的工具**
//!   （工具名 + 描述首句截断到 [`CANDIDATE_SUMMARY_MAX_CHARS`] 字）。
//! * 子代理建议（`[suggestion] subagents`）：候选是**本宿主可调度的全部子代理角色**
//!   （角色名 + 定义里的一句话描述），与 `subagent_run` 工具的参数枚举同源。
//! * Skill 建议（`[suggestion] skills`）：候选是**本机已索引的 Skill**（名称 + 作用域 + 描述
//!   + 正文），检索通路与 `skill_search` 工具完全一致。
//!
//! 匹配全部在本地完成（[`omnicrawl_extensions::bm25`]，毫秒级、不出网、不读任何渠道配置）。
//! 这三项原先走决策模型判定：一次往返 70–500ms，而建议发生在**每一轮提问之前**，这个开销
//! 压在最敏感的路径上；现在按同一份 BM25 打分排序，三类候选一次性并发算完，总耗时由最慢
//! 的那一类决定。
//!
//! 判定面是一条**相对分数线**：[`score_floor_ratio`]。BM25 的绝对分值随查询与候选集变化，
//! 用固定阈值会把「一问命中很多」和「只勉强命中一个」判成同一档；这里改取该次查询的最高分
//! 乘比例作为门槛，低于它的一律排除——分数是相对的，门槛也该相对。
//!
//! Skill 这一类不走这条分数线：它的候选来自 `SkillManager::search_meta`，名次本身就是按提问
//! 对「名称 + 描述 + 正文」打出来的，候选项里只剩截断后的描述。再拿它重算一次 BM25 会丢掉
//! 正文的贡献，靠正文命中的 Skill 与提问没有交集，整条建议会被清零。
//!
//! 名单非空时在 user 尾部追加一行（工具 `当前请求建议调用tool：a、b。`，子代理
//! `当前请求建议使用子代理：explore、plan。`，Skill `建议使用skills：pdf、docx。`），几类同时
//! 命中就各占一行。
//!
//! **失败一律 fail-open**：开关没开、没有候选、一个词都没命中都按「本次无建议」处理，user
//! 原文照发。建议只是提示，值不当为此挡住提问。

use std::sync::Arc;

use omnicrawl_core::diagnostics;
use omnicrawl_extensions::bm25;
use omnicrawl_extensions::skill::SkillSearchHit;

use crate::review::masking_from_config;

/// 送审的候选项说明：取描述首句，截断到这么多字符（超出加省略号）。
pub const CANDIDATE_SUMMARY_MAX_CHARS: usize = 24;

/// 建议名单的条数上限：只提示「最该看哪几个」，再多就成噪声。
pub const SUGGESTION_LIMIT: usize = 3;

/// 相对分数线：候选得分须达到该次查询最高分的这个比例才进名单。
///
/// 取 0.4：既排除「只碰到一个高频词」的边角命中，又不会因为第一名特别高而把同样贴题的
/// 第二、三名挤掉。
pub const SCORE_FLOOR_RATIO: f64 = 0.4;

/// Skill 建议行的固定前缀（插入文本与展示层拆行共用）。
pub const SKILL_SUGGESTION_PREFIX: &str = "建议使用skills：";

/// 旧的 Skill 送审池上限名（保留给调用方，语义已变成「检索取前几条」）。
pub const SKILL_SUGGESTION_POOL: usize = 8;

/// 旧的 Skill 注入上限名（保留给调用方，与 [`SUGGESTION_LIMIT`] 同值）。
pub const SKILL_SUGGESTION_LIMIT: usize = SUGGESTION_LIMIT;

/// 建议种类：三类建议形状相同（都是「从候选里按 BM25 挑几个」），开关、候选来源与文案不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestKind {
    /// 工具建议（`[suggestion] tools`）：候选是本宿主工具表里全部已启用的工具。
    Tools,
    /// 子代理建议（`[suggestion] subagents`）：候选是本宿主可调度的子代理角色。
    Subagents,
    /// Skill 建议（`[suggestion] skills`）：候选是本机已索引的 Skill。
    Skills,
}

impl SuggestKind {
    /// 建议行的固定前缀：插入文本与展示层拆行共用同一份口径。
    pub fn line_prefix(self) -> &'static str {
        match self {
            SuggestKind::Tools => "当前请求建议调用tool：",
            SuggestKind::Subagents => "当前请求建议使用子代理：",
            SuggestKind::Skills => SKILL_SUGGESTION_PREFIX,
        }
    }
}

/// 一次建议计算的输入。
pub struct SuggestRequest<'a> {
    /// 建议种类（只影响诊断文案与建议行前缀）。
    pub kind: SuggestKind,
    /// 用户本次的提问正文。
    pub user_prompt: &'a str,
    /// 候选（`(名称, 说明)`）：工具来自本宿主工具表，子代理来自角色定义，Skill 来自索引。
    pub candidates: &'a [(String, String)],
}

/// 建议执行器：默认实现走本地 BM25；测试注入桩以便钉住名单。
pub trait SuggestClient: Send + Sync {
    /// 返回建议使用的候选名（按得分降序）；`Err` 表示本次建议不可用（调用方按无建议处理）。
    fn suggest(&self, request: &SuggestRequest<'_>) -> Result<Vec<String>, String>;
}

/// 一类建议的运行期：只有一个开关与一个执行器，不再需要任何渠道配置。
#[derive(Clone)]
pub struct SuggestOptions {
    /// 开关状态。
    pub enabled: bool,
    /// 建议执行器（测试注入桩）。
    pub client: Arc<dyn SuggestClient>,
}

impl Default for SuggestOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            client: Arc::new(Bm25SuggestClient),
        }
    }
}

impl std::fmt::Debug for SuggestOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SuggestOptions")
            .field("enabled", &self.enabled)
            .finish()
    }
}

impl SuggestOptions {
    /// 是否真正可用：只看开关，本地匹配不需要任何外部条件。
    pub fn active(&self) -> bool {
        self.enabled
    }
}

/// 按配置装配建议运行期（TUI 与 API 共用同一口径）。
pub fn suggest_options_from_config(
    environment: &omnicrawl_config::core::runtime::ConfigEnvironment,
) -> [SuggestOptions; 3] {
    let config = omnicrawl_config::features::suggestion::load_suggestion_config(environment, None);
    [
        SuggestOptions {
            enabled: config.tools,
            ..SuggestOptions::default()
        },
        SuggestOptions {
            enabled: config.subagents,
            ..SuggestOptions::default()
        },
        SuggestOptions {
            enabled: config.skills,
            ..SuggestOptions::default()
        },
    ]
}

/// 脱敏旁路：本地匹配不出网，这里只保留「是否装配成功」的语义以便调用方沿用同一装配路径。
///
/// 纯本地匹配不需要脱敏，保留这个函数是为了让 `masking_from_config` 的构造失败不会中断
/// 建议装配（它原先与服务地址一起装配）。
pub fn suggest_masking(
    environment: &omnicrawl_config::core::runtime::ConfigEnvironment,
) -> Option<Arc<crate::review::ReviewMasking>> {
    masking_from_config(environment).map(Arc::new)
}

/// 工具候选：`(名称, 一句话说明)`，说明取描述首句截断。
pub fn suggest_candidates(inventory: &[(String, String)]) -> Vec<(String, String)> {
    inventory
        .iter()
        .map(|(name, description)| (name.clone(), tool_summary(description)))
        .collect()
}

/// 子代理候选：与 [`suggest_candidates`] 同口径（角色描述取首句）。
pub fn subagent_candidates(roles: &[(String, String)]) -> Vec<(String, String)> {
    roles
        .iter()
        .map(|(name, description)| (name.clone(), tool_summary(description)))
        .collect()
}

/// 描述 → 送审用的一句话：剥掉「是什么：」前缀，取首句，压成单空格后截断。
pub fn tool_summary(description: &str) -> String {
    let body = description.trim();
    let body = body.strip_prefix("是什么：").unwrap_or(body);
    let first_sentence = body.split(['。', '；', '！']).next().unwrap_or(body);
    let collapsed = first_sentence
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let characters: Vec<char> = collapsed.chars().collect();
    if characters.len() <= CANDIDATE_SUMMARY_MAX_CHARS {
        return collapsed;
    }
    let head: String = characters[..CANDIDATE_SUMMARY_MAX_CHARS].iter().collect();
    format!("{head}…")
}

/// 相对分数线：本次查询最高分 × [`SCORE_FLOOR_RATIO`]。
///
/// `top` 为 0（没有任何命中）时返回 `0.0`，调用方据此直接给出空名单。
pub fn score_floor_ratio(top: f64) -> f64 {
    if top <= 0.0 {
        0.0
    } else {
        top * SCORE_FLOOR_RATIO
    }
}

/// 按 BM25 给候选打分并挑出名单：得分 ≥ 相对分数线者按分数降序，最多 [`SUGGESTION_LIMIT`] 个。
///
/// 候选文档是「名称 + 说明」的分词结果——名称本身常常就是最有力的信号（`web_search` 之于
/// 「联网搜索」），只拿说明去比会把名字里写着答案的候选排到后面。
pub fn ranked_names(user_prompt: &str, candidates: &[(String, String)]) -> Vec<String> {
    if user_prompt.trim().is_empty() || candidates.is_empty() {
        return Vec::new();
    }
    let query = bm25::tokenize(user_prompt);
    if query.is_empty() {
        return Vec::new();
    }
    let documents: Vec<Vec<String>> = candidates
        .iter()
        .map(|(name, summary)| bm25::tokenize(&format!("{} {}", name.replace('-', " "), summary)))
        .collect();
    let hits = bm25::rank(&query, &documents);
    let Some(top) = hits.first().map(|hit| hit.score) else {
        return Vec::new();
    };
    let floor = score_floor_ratio(top);
    hits.into_iter()
        .filter(|hit| hit.score >= floor)
        .filter_map(|hit| candidates.get(hit.index).map(|(name, _)| name.clone()))
        .take(SUGGESTION_LIMIT)
        .collect()
}

/// 求建议名单；开关没开、没有候选或一个词都没命中时返回 `None`（不插入任何内容）。
pub fn suggested_names(
    kind: SuggestKind,
    options: &SuggestOptions,
    candidates: &[(String, String)],
    user_prompt: &str,
) -> Option<Vec<String>> {
    if !options.active() || user_prompt.trim().is_empty() || candidates.is_empty() {
        return None;
    }
    // Skill 的候选来自 `search_meta`：名次已是按提问对「名称 + 描述 + 正文」打出来的，
    // 这里直接用，不能再拿「名称 + 截断后的描述」重算一遍 BM25——正文命中项在二次打分里
    // 与提问没有交集，整条建议会被清零（池子非空却不注入任何名字）。
    if kind == SuggestKind::Skills {
        let names: Vec<String> = candidates
            .iter()
            .take(SUGGESTION_LIMIT)
            .map(|(name, _)| name.clone())
            .collect();
        return Some(names);
    }
    let request = SuggestRequest {
        kind,
        user_prompt,
        candidates,
    };
    match options.client.suggest(&request) {
        Ok(names) if names.is_empty() => None,
        Ok(names) => Some(names),
        Err(error) => {
            diagnostics::warn(format!("[host] 提问建议不可用，按无建议处理：{error}"));
            None
        }
    }
}

/// 工具建议：从工具表候选里按 BM25 挑。
pub fn suggested_tools(
    options: &SuggestOptions,
    inventory: &[(String, String)],
    user_prompt: &str,
) -> Option<Vec<String>> {
    let candidates = suggest_candidates(inventory);
    suggested_names(SuggestKind::Tools, options, &candidates, user_prompt)
}

/// 子代理建议：从角色候选里按 BM25 挑。
pub fn suggested_subagents(
    options: &SuggestOptions,
    roles: &[(String, String)],
    user_prompt: &str,
) -> Option<Vec<String>> {
    let candidates = subagent_candidates(roles);
    suggested_names(SuggestKind::Subagents, options, &candidates, user_prompt)
}

/// Skill 建议：候选来自 `SkillManager::search_meta` 的 BM25 命中项。
pub fn suggested_skills(
    options: &SuggestOptions,
    candidates: &[(String, String)],
    user_prompt: &str,
) -> Option<Vec<String>> {
    suggested_names(SuggestKind::Skills, options, candidates, user_prompt)
}

/// 检索本机 Skill 索引，取出 Skill 建议的候选。
///
/// 复用 `skill_search` 工具那条检索通路：`SkillManager::discover` 扫一次索引（覆盖本会话内
/// 新装的 Skill），`search_meta` 按 BM25 打分，检索单元是名称 + 描述 + 正文。作用域固定
/// `all`：建议是给模型看的提示，不该因为 Skill 装在用户级还是项目级而漏掉。
///
/// 这里已经用 BM25 排过序、也截到 [`SKILL_SUGGESTION_POOL`] 条，因此返回的清单本身即名单
/// 顺序；调用方再走一遍 [`ranked_names`] 只会拿「名称 + 截断后的描述」重算一次，反而丢了
/// 正文的贡献——所以 Skill 这一类直接把检索结果当名单用。
pub fn discover_skill_candidates(
    workspace: &std::path::Path,
    environment: &omnicrawl_config::core::runtime::ConfigEnvironment,
    user_prompt: &str,
) -> Vec<(String, String)> {
    if user_prompt.trim().is_empty() {
        return Vec::new();
    }
    let mut manager = omnicrawl_extensions::skill::SkillManager::new();
    let extra_paths = omnicrawl_config::features::skills::load_skills_config(environment, None)
        .map(|config| config.extra_paths)
        .unwrap_or_default();
    manager.discover(Some(workspace), &extra_paths);
    let hits = manager.search_meta(user_prompt, "all", SKILL_SUGGESTION_POOL);
    skill_candidates(&hits)
}

/// Skill 命中项 → 候选清单（名称 + 截断后的描述）。
pub fn skill_candidates(hits: &[SkillSearchHit]) -> Vec<(String, String)> {
    hits.iter()
        .map(|hit| (hit.meta.name.clone(), skill_summary(&hit.meta.description)))
        .collect()
}

/// Skill 命中项 → 名单：BM25 名次即优先级，最多 [`SUGGESTION_LIMIT`] 个。
pub fn skill_names(hits: &[SkillSearchHit]) -> Vec<String> {
    hits.iter()
        .take(SUGGESTION_LIMIT)
        .map(|hit| hit.meta.name.clone())
        .collect()
}

/// Skill 描述 → 送审用的一句话：压成单空格后截断到 [`CANDIDATE_SUMMARY_MAX_CHARS`] 字。
///
/// 与 [`tool_summary`] 的区别在于不剥「是什么：」前缀、也不按句号截半句——Skill 描述常是一整句
/// 适用场景的说明，截掉后半句会丢掉「什么时候该用它」这个判定依据。
fn skill_summary(description: &str) -> String {
    let collapsed = description
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let characters: Vec<char> = collapsed.chars().collect();
    if characters.len() <= CANDIDATE_SUMMARY_MAX_CHARS {
        return collapsed;
    }
    let head: String = characters[..CANDIDATE_SUMMARY_MAX_CHARS].iter().collect();
    format!("{head}…")
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
    [
        SuggestKind::Tools,
        SuggestKind::Subagents,
        SuggestKind::Skills,
    ]
    .iter()
    .any(|kind| line.starts_with(kind.line_prefix()))
}

/// 三类建议的输入。
pub struct SuggestionInputs<'a> {
    /// 工具建议运行期与候选（工具表清单）。
    pub tools: (&'a SuggestOptions, &'a [(String, String)]),
    /// 子代理建议运行期与候选（角色清单）。
    pub subagents: (&'a SuggestOptions, &'a [(String, String)]),
    /// Skill 建议运行期与候选（Skill 索引的 BM25 命中）。
    pub skills: (&'a SuggestOptions, &'a [(String, String)]),
    /// 用户本次的提问正文。
    pub user_prompt: &'a str,
}

/// 一类建议的待办：种类、运行期与候选清单。
type SuggestJob = (SuggestKind, SuggestOptions, Vec<(String, String)>);

/// 并发计算三类建议，返回拼好顺序的建议行；都没有建议时返回空列表。
///
/// 三条候选集彼此独立，且本地打分是纯 CPU 计算；这里仍按「各开一线程」并发，
/// 是因为候选项数可达上百（工具表 + Skill 正文分词），并发能把三份分词开销摊到多核上。
pub fn suggestion_lines(inputs: &SuggestionInputs<'_>) -> Vec<String> {
    let prompt = inputs.user_prompt;
    if prompt.trim().is_empty() {
        return Vec::new();
    }
    let mut jobs: Vec<SuggestJob> = Vec::new();
    for (kind, (options, candidates)) in [
        (SuggestKind::Tools, inputs.tools),
        (SuggestKind::Subagents, inputs.subagents),
        (SuggestKind::Skills, inputs.skills),
    ] {
        if options.active() && !candidates.is_empty() {
            jobs.push((kind, options.clone(), candidates.to_vec()));
        }
    }
    match jobs.len() {
        0 => return Vec::new(),
        // 只有一类真正要算时不必起线程：省下一次调度，也不引入无谓的等待。
        1 => {
            let (kind, options, candidates) = jobs.remove(0);
            return lines_for(kind, suggested_names(kind, &options, &candidates, prompt));
        }
        _ => {}
    }
    let mut rest = jobs.split_off(1);
    let (first_kind, first_options, first_candidates) = jobs.remove(0);
    let prompt_owned = prompt.to_string();
    let handles: Vec<_> = rest
        .drain(..)
        .map(|(kind, options, candidates)| {
            let prompt = prompt_owned.clone();
            std::thread::spawn(move || (kind, suggested_names(kind, &options, &candidates, &prompt)))
        })
        .collect();
    let mut results = vec![(
        first_kind,
        suggested_names(first_kind, &first_options, &first_candidates, prompt),
    )];
    for handle in handles {
        if let Ok(result) = handle.join() {
            results.push(result);
        }
    }
    // 建议行的顺序固定为「工具、子代理、Skill」，与并发完成的先后无关。
    [
        SuggestKind::Tools,
        SuggestKind::Subagents,
        SuggestKind::Skills,
    ]
    .iter()
    .filter_map(|kind| {
        results
            .iter()
            .find(|(result_kind, _)| result_kind == kind)
            .and_then(|(_, names)| {
                names
                    .clone()
                    .and_then(|names| suggestion_line(*kind, &names))
            })
    })
    .collect()
}

/// 一类建议的结果 → 建议行（无建议时为空列表）。
fn lines_for(kind: SuggestKind, names: Option<Vec<String>>) -> Vec<String> {
    names
        .and_then(|names| suggestion_line(kind, &names))
        .into_iter()
        .collect()
}

/// 真实实现：本地 BM25 打分，不发起任何网络请求。
pub struct Bm25SuggestClient;

impl SuggestClient for Bm25SuggestClient {
    fn suggest(&self, request: &SuggestRequest<'_>) -> Result<Vec<String>, String> {
        Ok(ranked_names(request.user_prompt, request.candidates))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn roles() -> Vec<(String, String)> {
        vec![
            ("explore".to_string(), "只读探索代码结构".to_string()),
            ("plan".to_string(), "先出计划再动手".to_string()),
            ("review".to_string(), "评审代码变更".to_string()),
        ]
    }

    fn enabled() -> SuggestOptions {
        SuggestOptions {
            enabled: true,
            ..SuggestOptions::default()
        }
    }

    #[test]
    fn a_disabled_suggestion_produces_nothing() {
        let tools = inventory(&["web_search"]);
        assert!(suggested_tools(&SuggestOptions::default(), &tools, "联网搜索").is_none());
        assert!(suggested_subagents(&SuggestOptions::default(), &roles(), "先探索代码").is_none());
        assert!(suggested_skills(&SuggestOptions::default(), &tools, "把 pdf 转成文本").is_none());
    }

    #[test]
    fn an_empty_prompt_or_candidate_list_is_skipped() {
        let options = enabled();
        assert!(suggested_tools(&options, &inventory(&["read"]), "   ").is_none());
        assert!(suggested_tools(&options, &[], "读文件").is_none());
        assert!(suggested_subagents(&options, &[], "先探索代码").is_none());
    }

    /// 名称本身就是最强信号：`web_search` 该被「联网搜索」选中。
    #[test]
    fn a_matching_name_wins() {
        let tools = vec![
            ("read".to_string(), "读取文件内容".to_string()),
            ("web_search".to_string(), "搜索公开资料".to_string()),
            ("fetcher".to_string(), "抓取页面正文".to_string()),
        ];
        let names = suggested_tools(&enabled(), &tools, "帮我联网搜索一下这个报错")
            .expect("应当有建议");
        assert_eq!(names.first().map(String::as_str), Some("web_search"), "{names:?}");
    }

    /// 相对分数线：只留下与最高分同一量级的候选。
    #[test]
    fn the_floor_is_relative_to_the_top_score() {
        let candidates = vec![
            ("pdf".to_string(), "处理 PDF 文件的转换".to_string()),
            ("docx".to_string(), "处理 Word 文档".to_string()),
            ("xlsx".to_string(), "电子表格计算".to_string()),
        ];
        let names = ranked_names("把 pdf 转成文本", &candidates);
        assert_eq!(names, vec!["pdf".to_string()], "只有 pdf 贴合：{names:?}");
    }

    #[test]
    fn without_any_hit_nothing_is_suggested() {
        let candidates = inventory(&["read", "write_file"]);
        assert!(ranked_names("今天天气不错", &candidates).is_empty());
    }

    #[test]
    fn the_shortlist_is_capped() {
        let candidates: Vec<(String, String)> = (0..10)
            .map(|index| (format!("tool{index}"), "检索代码".to_string()))
            .collect();
        assert_eq!(ranked_names("检索代码", &candidates).len(), SUGGESTION_LIMIT);
    }

    #[test]
    fn suggestion_lines_keep_a_fixed_order() {
        let tools = vec![("web_search".to_string(), "搜索公开资料".to_string())];
        let roles = vec![("explore".to_string(), "只读探索代码结构".to_string())];
        let lines = suggestion_lines(&SuggestionInputs {
            tools: (&enabled(), &tools),
            subagents: (&enabled(), &roles),
            skills: (&SuggestOptions::default(), &[]),
            user_prompt: "探索代码并搜索公开资料",
        });
        assert_eq!(
            lines,
            vec![
                "当前请求建议调用tool：web_search。".to_string(),
                "当前请求建议使用子代理：explore。".to_string(),
            ]
        );
    }

    #[test]
    fn skill_lines_use_their_own_prefix() {
        let line = suggestion_line(SuggestKind::Skills, &["pdf".to_string()])
            .expect("非空名单应拼出一行");
        assert_eq!(line, "建议使用skills：pdf。");
    }

    /// Skill 候选的顺序就是检索名次，不能再被「名称 + 截断描述」的二次打分清零。
    ///
    /// 候选池来自 `search_meta`：它按「名称 + 描述 + 正文」打过一次分，而候选项里只留了
    /// 截断后的描述。这里用一个描述里不含任何提问词的候选复现——旧实现下它会被整条丢弃。
    #[test]
    fn skill_candidates_keep_the_retrieval_order_without_rescoring() {
        let candidates = vec![
            (
                "agent-browser-cli".to_string(),
                "使用 agent-browser-cli 进行浏览器感知与控制".to_string(),
            ),
            ("multiplane".to_string(), "用 Multiplane 把 HTML 网页渲染成视频".to_string()),
        ];
        let names = suggested_skills(&enabled(), &candidates, "用浏览器打开这个页面并截图")
            .expect("候选池非空就该给出名单");
        assert_eq!(names, vec!["agent-browser-cli".to_string(), "multiplane".to_string()]);
    }

    /// 名单截断到 [`SUGGESTION_LIMIT`] 条，而不是把整个候选池灌进 user 尾部。
    #[test]
    fn the_skill_shortlist_is_capped() {
        let candidates: Vec<(String, String)> = (0..10)
            .map(|index| (format!("skill{index}"), "处理文档".to_string()))
            .collect();
        let names = suggested_skills(&enabled(), &candidates, "处理一下文档").expect("应当有建议");
        assert_eq!(names.len(), SUGGESTION_LIMIT, "{names:?}");
        assert_eq!(names.first().map(String::as_str), Some("skill0"), "名次即优先级");
    }

    /// 建议行必须能被展示层原样拆回去（实时提交与回放共用同一口径）。
    #[test]
    fn the_inserted_lines_split_back_off() {
        let lines = vec!["当前请求建议调用tool：read。".to_string()];
        let text = insert_suggestions("读一下这个文件", &lines);
        let (body, suggestions) = split_suggestion(&text);
        assert_eq!(body, "读一下这个文件");
        assert_eq!(suggestions, lines);
    }

    #[test]
    fn a_summary_keeps_only_the_first_sentence() {
        assert_eq!(
            tool_summary("是什么：搜索公开网页。怎么做：要发现 URL 时用。"),
            "搜索公开网页"
        );
        let long = tool_summary(&format!(
            "是什么：{}。",
            "长".repeat(CANDIDATE_SUMMARY_MAX_CHARS + 5)
        ));
        assert_eq!(long.chars().count(), CANDIDATE_SUMMARY_MAX_CHARS + 1);
    }
}
