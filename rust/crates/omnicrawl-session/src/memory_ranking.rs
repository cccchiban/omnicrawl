//! 记忆的分类、摘要与检索排序：纯逻辑，不碰磁盘。
//!
//! 语义基准是 Python `omnicrawl/state/memory_ranking.py`。`MemoryStore` 负责 Markdown 与
//! index 的持久化，并在需要时调用这里的选择策略；本模块不反向依赖存储层。
//!
//! 未搬：`text_similarity`（Python 用 `difflib.SequenceMatcher.ratio()` 的
//! Ratcliff-Obershelp 匹配）。它是「近似重复记忆合并」的判定依据，必须逐位一致，
//! 留作单独一片照搬算法并对照验证，这里不提供近似替代。

use std::collections::BTreeSet;

use chrono::{DateTime, FixedOffset, Local};

use crate::memory::MemoryIndexEntry;

/// 推荐存储目录（供提示词段落列举）。
pub const DEFAULT_STORAGE_DIRECTORIES: [&str; 6] = [
    "user-preferences/general",
    "project-context/general",
    "task-history/general",
    "code-knowledge/general",
    "error-lessons/general",
    "external-context/general",
];

const FALLBACK_STORAGE_DIRECTORY: &str = "task-history/general";

/// 分类规则：按顺序取第一条命中的规则，最后一条同时充当兜底。
const CLASSIFICATION_RULES: [(&str, &[&str]); 6] = [
    (
        "user-preferences/communication-style",
        &[
            "偏好",
            "喜欢",
            "不喜欢",
            "习惯",
            "沟通",
            "回答风格",
            "输出",
            "称呼",
        ],
    ),
    (
        "project-context/general",
        &[
            "项目",
            "仓库",
            "架构",
            "约束",
            "配置",
            "入口",
            "workspace",
            "repository",
        ],
    ),
    (
        "code-knowledge/general",
        &[
            "代码", "函数", "类", "模块", "接口", "实现", "源码", "class", "function",
        ],
    ),
    (
        "error-lessons/general",
        &[
            "错误",
            "失败",
            "异常",
            "修复",
            "调试",
            "踩坑",
            "bug",
            "error",
            "exception",
        ],
    ),
    (
        "external-context/general",
        &[
            "api",
            "外部服务",
            "环境变量",
            "域名",
            "权限",
            "token",
            "模型",
            "网关",
        ],
    ),
    (
        "task-history/general",
        &["任务", "完成", "决策", "待办", "跟进", "历史", "计划"],
    ),
];

/// 按关键词启发式为记忆选择默认存储目录。
pub fn classify_storage_directory(content: &str) -> String {
    let text = content.to_lowercase();
    for (directory, keywords) in CLASSIFICATION_RULES {
        if keywords.iter().any(|keyword| text.contains(keyword)) {
            return directory.to_string();
        }
    }
    FALLBACK_STORAGE_DIRECTORY.to_string()
}

/// 从正文生成低成本摘要，供搜索结果和索引使用。
pub fn make_summary(content: &str, max_chars: usize) -> String {
    let without_fences = strip_code_fences(content);
    let without_prefixes = strip_line_prefixes(&without_fences);
    let collapsed = collapse_whitespace(&without_prefixes);
    let text = collapsed.trim();
    if text.is_empty() {
        return String::new();
    }
    let text = match first_sentence_end(text) {
        Some(index) if index <= max_chars => &text[..byte_index(text, index)],
        _ => text,
    };
    let length = text.chars().count();
    if length <= max_chars {
        return text.to_string();
    }
    let head: String = text
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>()
        .trim_end()
        .to_string();
    format!("{head}…")
}

/// 合并近似重复记忆的正文，避免无意义的重复落盘。
pub fn merge_memory_content(old_content: &str, new_content: &str) -> String {
    let old = old_content.trim();
    let new = new_content.trim();
    if old.is_empty() {
        return new.to_string();
    }
    if normalize_for_compare(old).contains(&normalize_for_compare(new)) {
        return old.to_string();
    }
    format!("{old}\n\n补充：{new}")
}

/// 计算记忆条目与候选目录的匹配强度（调用方应先完成目录归一化）。
pub fn directory_match_score(entry: &MemoryIndexEntry, directory: &str) -> f64 {
    if entry.storage_directory == directory {
        return 3.0;
    }
    if entry
        .storage_directory
        .starts_with(&format!("{directory}/"))
        || directory.starts_with(&format!("{}/", entry.storage_directory))
    {
        return 2.0;
    }
    if entry
        .related_directories
        .iter()
        .any(|item| item == directory)
    {
        return 1.5;
    }
    if entry.related_directories.iter().any(|item| {
        item.starts_with(&format!("{directory}/")) || directory.starts_with(&format!("{item}/"))
    }) {
        return 1.0;
    }
    0.0
}

/// 两个目录集合是否有交集或上下级关系。
pub fn directories_overlap(left: &[String], right: &[String]) -> bool {
    let left_set: BTreeSet<&String> = left.iter().collect();
    let right_set: BTreeSet<&String> = right.iter().collect();
    if left_set.intersection(&right_set).next().is_some() {
        return true;
    }
    left_set.iter().any(|item| {
        right_set.iter().any(|other| {
            item.starts_with(&format!("{other}/")) || other.starts_with(&format!("{item}/"))
        })
    })
}

/// 抽取检索用 token：ASCII 词块（≥2 字符）加中文片段本身与 2/3 元组。
pub fn extract_search_tokens(text: &str) -> BTreeSet<String> {
    let lowered = text.to_lowercase();
    let mut tokens: BTreeSet<String> = BTreeSet::new();

    let characters: Vec<char> = lowered.chars().collect();
    let mut index = 0;
    while index < characters.len() {
        if is_ascii_token_char(characters[index]) {
            let start = index;
            while index < characters.len() && is_ascii_token_char(characters[index]) {
                index += 1;
            }
            if index - start >= 2 {
                tokens.insert(characters[start..index].iter().collect());
            }
            continue;
        }
        index += 1;
    }

    index = 0;
    while index < characters.len() {
        if is_han(characters[index]) {
            let start = index;
            while index < characters.len() && is_han(characters[index]) {
                index += 1;
            }
            let chunk = &characters[start..index];
            if chunk.len() <= 8 {
                tokens.insert(chunk.iter().collect());
            }
            for offset in 0..chunk.len().saturating_sub(1) {
                tokens.insert(chunk[offset..offset + 2].iter().collect());
            }
            for offset in 0..chunk.len().saturating_sub(2) {
                tokens.insert(chunk[offset..offset + 3].iter().collect());
            }
            continue;
        }
        index += 1;
    }

    tokens
}

/// 计算搜索候选分；`now` 可注入，便于对照测试。
pub fn score_search_entry(
    entry: &MemoryIndexEntry,
    query: &str,
    candidate_directories: &[String],
    now: DateTime<FixedOffset>,
) -> f64 {
    let mut score = 0.0;
    let haystack = format!(
        "{} {} {}",
        entry.summary,
        entry.storage_directory,
        entry.related_directories.join(" ")
    )
    .to_lowercase();

    let query_tokens = extract_search_tokens(query);
    if !query_tokens.is_empty() {
        let token_hits = query_tokens
            .iter()
            .filter(|token| haystack.contains(token.as_str()))
            .count();
        score += token_hits as f64 / query_tokens.len() as f64 * 10.0;
        let lowered_query = query.to_lowercase();
        if !lowered_query.is_empty() && haystack.contains(&lowered_query) {
            score += 5.0;
        }
    }

    for directory in candidate_directories {
        score += directory_match_score(entry, directory) * 3.0;
    }

    // 轻微倾向被反复使用或较新的记忆，但不让它盖过文本相关度。
    score += (entry.touch_count.min(10) as f64) * 0.05;
    let age_days =
        ((now - entry.timestamp).num_microseconds().unwrap_or(0) as f64 / 1e6 / 86400.0).max(0.0);
    score += (1.0 - age_days.min(30.0) / 30.0).max(0.0) * 0.1;
    score
}

/// 沿关联目录展开时的候选分：越深越弱。
pub fn score_related_entry(
    entry: &MemoryIndexEntry,
    directories: &BTreeSet<String>,
    depth: usize,
) -> f64 {
    let best = directories
        .iter()
        .map(|directory| directory_match_score(entry, directory))
        .fold(0.0_f64, f64::max);
    if best <= 0.0 {
        return 0.0;
    }
    best / (depth as f64 + 1.0)
}

/// 比较用归一化：去掉所有非字母数字字符（含下划线）并小写。
pub fn normalize_for_compare(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter(|character| character.is_alphanumeric())
        .collect()
}

/// 本机当前时间（对应 Python `datetime.now().astimezone()`）。
pub fn local_now() -> DateTime<FixedOffset> {
    Local::now().fixed_offset()
}

fn is_ascii_token_char(character: char) -> bool {
    character.is_ascii_lowercase()
        || character.is_ascii_digit()
        || matches!(character, '_' | '+' | '-')
}

fn is_han(character: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&character)
}

/// 去掉成对的代码围栏：` ```...``` `（最短匹配，落单的围栏保留）。
fn strip_code_fences(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(start) = rest.find("```") else {
            result.push_str(rest);
            return result;
        };
        let Some(end) = rest[start + 3..].find("```") else {
            result.push_str(rest);
            return result;
        };
        result.push_str(&rest[..start]);
        rest = &rest[start + 3 + end + 3..];
    }
}

/// 去掉行首的列表/引用/标题标记：0-3 个空白、若干标记字符、随后空白。
fn strip_line_prefixes(text: &str) -> String {
    let characters: Vec<char> = text.chars().collect();
    let mut result = String::with_capacity(text.len());
    let mut index = 0;
    let mut at_line_start = true;
    while index < characters.len() {
        if !at_line_start {
            result.push(characters[index]);
            at_line_start = characters[index] == '\n';
            index += 1;
            continue;
        }
        // 正则允许 `\s{0,3}` 吃掉换行，这里按“先贪心、失败则回退”模拟。
        let mut matched = None;
        for whitespace in (0..=3).rev() {
            let mut cursor = index;
            let mut consumed = 0;
            while consumed < whitespace
                && cursor < characters.len()
                && characters[cursor].is_whitespace()
            {
                cursor += 1;
                consumed += 1;
            }
            if consumed != whitespace {
                continue;
            }
            let markers_start = cursor;
            while cursor < characters.len()
                && matches!(characters[cursor], '-' | '*' | '+' | '>' | '#')
            {
                cursor += 1;
            }
            if cursor == markers_start {
                continue;
            }
            let mut tail = cursor;
            while tail < characters.len() && characters[tail].is_whitespace() {
                tail += 1;
            }
            matched = Some(tail);
            break;
        }
        match matched {
            Some(next) => {
                at_line_start = next > index && characters[next - 1] == '\n';
                index = next;
            }
            None => {
                result.push(characters[index]);
                at_line_start = characters[index] == '\n';
                index += 1;
            }
        }
    }
    result
}

/// 连续的空白折成一个空格（对应 Python `re.sub(r"\s+", " ", ...)`）。
fn collapse_whitespace(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut in_run = false;
    for character in text.chars() {
        if character.is_whitespace() {
            if !in_run {
                result.push(' ');
                in_run = true;
            }
        } else {
            result.push(character);
            in_run = false;
        }
    }
    result
}

/// 第一个句末标记的位置（字符下标）。
fn first_sentence_end(text: &str) -> Option<usize> {
    text.chars()
        .position(|character| matches!(character, '。' | '！' | '？' | '!' | '?' | '；' | ';'))
        .map(|index| index + 1)
}

fn byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(index, _)| index)
        .unwrap_or(text.len())
}
