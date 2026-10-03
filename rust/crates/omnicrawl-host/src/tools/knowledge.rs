//! 知识库工具执行体：Markdown + YAML frontmatter + 自动索引。
//!
//! 语义基准是 `omnicrawl/knowledge/__init__.py`（笔记读写、检索与 `INDEX.md` 维护）与
//! `omnicrawl/agent/toolkit/knowledge_tools.py`（工具参数与结果适配）。知识库默认位于用户
//! 主目录的 `.OmniCrawl/knowledge`，与工作区无关；所有读写路径都必须落在根目录内，`..` 拒绝。

use std::fs;
use std::path::{Path, PathBuf};

use omnicrawl_controllers::json::{python_dumps, python_number_text};
use omnicrawl_controllers::tool_args::{read_limited_int, read_optional_string_list};
use serde_json::{json, Map, Value};

use super::decision_search::{
    apply_order, reranked_order, RerankOptions, RERANK_CANDIDATE_LIMIT,
};
use super::error::{ToolError, ToolOutcome};
use super::paths::resolve_lenient;

pub const DEFAULT_KNOWLEDGE_DIRNAME: &str = "knowledge";
pub const INDEX_FILENAME: &str = "INDEX.md";
pub const README_FILENAME: &str = "README.md";
pub const MAX_SEARCH_RESULTS: i64 = 50;
pub const MAX_LIST_RESULTS: i64 = 200;
pub const READ_CHARS_DEFAULT: i64 = 50000;
pub const READ_CHARS_MAXIMUM: i64 = 200000;
pub const READ_TRUNCATED_SUFFIX: &str = "\n…（已截断，可提高 max_chars 读取更多内容）";

const FRONTMATTER_FIELD_ORDER: [&str; 7] = [
    "title", "created", "updated", "project", "tags", "type", "status",
];
const DEFAULT_TYPE: &str = "note";
const DEFAULT_STATUS: &str = "draft";
const VALID_TYPES: [&str; 6] = [
    "note",
    "meeting",
    "decision",
    "log",
    "research",
    "reference",
];
const VALID_STATUSES: [&str; 3] = ["draft", "done", "archived"];
const SNIPPET_RADIUS: usize = 80;

/// 笔记元数据（frontmatter + 相对路径）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteMeta {
    pub rel_path: String,
    pub title: String,
    pub created: String,
    pub updated: String,
    pub project: String,
    pub note_type: String,
    pub status: String,
    pub tags: Vec<String>,
}

impl NoteMeta {
    pub fn to_value(&self) -> Value {
        json!({
            "path": self.rel_path,
            "title": self.title,
            "created": self.created,
            "updated": self.updated,
            "project": self.project,
            "type": self.note_type,
            "status": self.status,
            "tags": self.tags,
        })
    }
}

/// 一次搜索命中的笔记摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub rel_path: String,
    pub title: String,
    pub project: String,
    pub note_type: String,
    pub status: String,
    pub tags: Vec<String>,
    pub snippet: String,
    pub score: i64,
}

impl SearchHit {
    pub fn to_value(&self) -> Value {
        json!({
            "path": self.rel_path,
            "title": self.title,
            "project": self.project,
            "type": self.note_type,
            "status": self.status,
            "tags": self.tags,
            "snippet": self.snippet,
            "score": self.score,
        })
    }
}

/// 一次写入的可选 frontmatter 字段。
#[derive(Debug, Clone, Default)]
pub struct WriteRequest<'a> {
    pub title: Option<&'a str>,
    pub project: Option<&'a str>,
    pub tags: Option<&'a [String]>,
    pub note_type: Option<&'a str>,
    pub status: Option<&'a str>,
    pub mode: &'a str,
}

/// 默认知识库根：`~/.OmniCrawl/knowledge`（Windows 上 `USERPROFILE` 优先，与 `Path.home()` 同序）。
pub fn default_root() -> PathBuf {
    for name in ["USERPROFILE", "HOME"] {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                return PathBuf::from(value)
                    .join(".OmniCrawl")
                    .join(DEFAULT_KNOWLEDGE_DIRNAME);
            }
        }
    }
    PathBuf::from(".")
        .join(".OmniCrawl")
        .join(DEFAULT_KNOWLEDGE_DIRNAME)
}

pub struct KnowledgeBase {
    root: PathBuf,
}

impl KnowledgeBase {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: resolve_lenient(&root.into()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 重建 `INDEX.md`（自动维护，不保留手工编辑内容）。
    pub fn refresh_index(&self) -> Result<(), ToolError> {
        let root = resolve_lenient(&self.root);
        fs::create_dir_all(&root).map_err(|error| {
            ToolError::new(format!("创建知识库目录失败：{}，{error}", root.display()))
        })?;
        let notes = self.list_notes();
        let mut lines = vec![
            "# Knowledge Base Index".to_string(),
            String::new(),
            "本文件由知识库自动维护，请勿手工编辑。".to_string(),
            String::new(),
            format!("共 {} 篇笔记。", notes.len()),
            String::new(),
        ];
        for note in &notes {
            let tags = if note.tags.is_empty() {
                "-".to_string()
            } else {
                note.tags.join(", ")
            };
            let project = if note.project.is_empty() {
                "-"
            } else {
                note.project.as_str()
            };
            lines.push(format!(
                "- [{}]({}) — `{}` | 项目：{} | 状态：{} | 标签：{} | 更新：{}",
                note.title, note.rel_path, note.note_type, project, note.status, tags, note.updated
            ));
        }
        lines.push(String::new());
        let tmp = root.join(".INDEX.md.tmp");
        fs::write(&tmp, lines.join("\n")).map_err(|error| {
            ToolError::new(format!("写入知识库索引失败：{}，{error}", tmp.display()))
        })?;
        fs::rename(&tmp, root.join(INDEX_FILENAME)).map_err(|error| {
            ToolError::new(format!("更新知识库索引失败：{}，{error}", tmp.display()))
        })?;
        Ok(())
    }

    /// 把工具传入的相对路径解析到知识库内；`..` 与越界路径一律拒绝。
    pub fn safe_join(&self, rel_path: &str, default_suffix: &str) -> Result<PathBuf, ToolError> {
        let raw = rel_path.trim().replace('\\', "/");
        let raw = raw.trim_start_matches('/');
        let mut parts: Vec<String> = raw
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .map(|part| part.to_string())
            .collect();
        if parts.iter().any(|part| part == "..") {
            return Err(ToolError::new("知识库路径不允许包含 ..。"));
        }
        let root = resolve_lenient(&self.root);
        if parts.is_empty() {
            return Ok(root);
        }
        let last = parts.pop().expect("已确认非空");
        parts.push(if !default_suffix.is_empty() && !last.contains('.') {
            format!("{last}{default_suffix}")
        } else {
            last
        });
        let candidate = parts
            .iter()
            .fold(root.clone(), |path, part| path.join(part));
        let resolved = resolve_lenient(&candidate);
        if resolved != root && !resolved.starts_with(&root) {
            return Err(ToolError::new("知识库路径超出根目录。"));
        }
        Ok(resolved)
    }

    /// 根目录相对的知识库路径；越界时回落到文件名（与 Python `_display_path` 一致）。
    pub fn display_path(&self, target: &Path) -> String {
        let root = resolve_lenient(&self.root);
        match resolve_lenient(target).strip_prefix(&root) {
            Ok(relative) => to_posix(relative),
            Err(_) => target
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default(),
        }
    }

    /// 扫描知识库全部笔记（跳过 `INDEX.md` / `README.md` 与隐藏项），按相对路径小写排序。
    pub fn list_notes(&self) -> Vec<NoteMeta> {
        let mut notes: Vec<NoteMeta> = self
            .scan_notes()
            .into_iter()
            .map(|(meta, _body)| meta)
            .collect();
        notes.sort_by_key(|note| note.rel_path.to_lowercase());
        notes
    }

    fn scan_notes(&self) -> Vec<(NoteMeta, String)> {
        let root = resolve_lenient(&self.root);
        if !root.is_dir() {
            return Vec::new();
        }
        let mut documents: Vec<(NoteMeta, String)> = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(current) = stack.pop() {
            let Ok(entries) = fs::read_dir(&current) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                // 目录符号链接不跟随：`file_type` 对链接返回 symlink，走文件分支。
                if file_type.is_dir() {
                    stack.push(entry.path());
                    continue;
                }
                if !name.ends_with(".md") {
                    continue;
                }
                if name == INDEX_FILENAME || name == README_FILENAME {
                    continue;
                }
                let path = entry.path();
                let is_link = file_type.is_symlink();
                let resolved = if is_link {
                    resolve_lenient(&path)
                } else {
                    path.clone()
                };
                if is_link {
                    let linked = resolved
                        .file_name()
                        .map(|value| value.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if linked == INDEX_FILENAME || linked == README_FILENAME {
                        continue;
                    }
                }
                let Ok(relative) = resolved.strip_prefix(&root) else {
                    continue;
                };
                let Ok(bytes) = fs::read(&resolved) else {
                    continue;
                };
                let Ok(text) = String::from_utf8(bytes) else {
                    continue;
                };
                let (meta, body, _extra) = parse_frontmatter(&text);
                documents.push((meta_from_dict(&to_posix(relative), &meta), body));
            }
        }
        documents
    }

    pub fn read(&self, rel_path: &str) -> Result<String, ToolError> {
        let target = self.safe_join(rel_path, ".md")?;
        if !target.is_file() {
            return Err(ToolError::new(format!(
                "知识库笔记不存在：{}。",
                self.display_path(&target)
            )));
        }
        let bytes = fs::read(&target).map_err(|error| {
            ToolError::new(format!("读取知识库笔记失败：{}。", python_os_error(&error)))
        })?;
        String::from_utf8(bytes).map_err(|_| ToolError::new("知识库笔记必须是 UTF-8 文本。"))
    }

    /// 新建、覆盖或追加一篇笔记，并刷新索引。
    pub fn write(
        &self,
        rel_path: &str,
        content: &str,
        request: &WriteRequest<'_>,
    ) -> Result<NoteMeta, ToolError> {
        let raw_mode = request.mode.trim().to_lowercase();
        let mode = if raw_mode.is_empty() {
            "overwrite".to_string()
        } else {
            raw_mode
        };
        if mode != "create" && mode != "overwrite" && mode != "append" {
            return Err(ToolError::new("mode 必须是 create、overwrite 或 append。"));
        }

        let target = self.safe_join(rel_path, ".md")?;
        let existed = target.is_file();
        if mode == "create" && existed {
            return Err(ToolError::new(format!(
                "笔记已存在：{}。",
                self.display_path(&target)
            )));
        }

        let mut old_meta = Map::new();
        let mut old_body = String::new();
        let mut extra: Vec<(String, Value)> = Vec::new();
        if existed {
            let bytes = fs::read(&target).map_err(|error| {
                ToolError::new(format!("读取知识库笔记失败：{}。", python_os_error(&error)))
            })?;
            let text = String::from_utf8(bytes)
                .map_err(|_| ToolError::new("知识库笔记必须是 UTF-8 文本。"))?;
            let (meta, body, extras) = parse_frontmatter(&text);
            old_meta = meta;
            old_body = body;
            extra = extras;
        }

        let today = today_text();
        let mut merged = Map::new();
        merged.insert(
            "title".to_string(),
            match request.title {
                Some(text) if !text.trim().is_empty() => Value::String(text.trim().to_string()),
                _ => match old_meta.get("title") {
                    Some(value) if truthy(value) => value.clone(),
                    _ => Value::String(file_stem(rel_path)),
                },
            },
        );
        merged.insert(
            "created".to_string(),
            match old_meta.get("created") {
                Some(value) if truthy(value) => Value::String(scalar_text(value)),
                _ => Value::String(today.clone()),
            },
        );
        merged.insert("updated".to_string(), Value::String(today));
        merged.insert(
            "project".to_string(),
            match request.project {
                Some(text) => Value::String(text.trim().to_string()),
                None => match old_meta.get("project") {
                    Some(value) if truthy(value) => value.clone(),
                    _ => Value::String(String::new()),
                },
            },
        );
        merged.insert(
            "tags".to_string(),
            match request.tags {
                Some(tags) => Value::Array(
                    tags.iter()
                        .map(|tag| tag.trim())
                        .filter(|tag| !tag.is_empty())
                        .map(|tag| Value::String(tag.to_string()))
                        .collect(),
                ),
                None => match old_meta.get("tags") {
                    Some(value) if truthy(value) => value.clone(),
                    _ => Value::Array(Vec::new()),
                },
            },
        );
        merged.insert(
            "type".to_string(),
            match request.note_type {
                Some(text) if !text.trim().is_empty() => Value::String(text.trim().to_lowercase()),
                _ => match old_meta.get("type") {
                    Some(value) if truthy(value) => value.clone(),
                    _ => Value::String(DEFAULT_TYPE.to_string()),
                },
            },
        );
        merged.insert(
            "status".to_string(),
            match request.status {
                Some(text) if !text.trim().is_empty() => Value::String(text.trim().to_lowercase()),
                _ => match old_meta.get("status") {
                    Some(value) if truthy(value) => value.clone(),
                    _ => Value::String(DEFAULT_STATUS.to_string()),
                },
            },
        );

        let merged_type = text_field(&merged, "type");
        if !VALID_TYPES.contains(&merged_type.as_str()) {
            let mut valid: Vec<&str> = VALID_TYPES.to_vec();
            valid.sort_unstable();
            return Err(ToolError::new(format!(
                "type 必须是 {} 之一。",
                valid.join(", ")
            )));
        }
        let merged_status = text_field(&merged, "status");
        if !VALID_STATUSES.contains(&merged_status.as_str()) {
            let mut valid: Vec<&str> = VALID_STATUSES.to_vec();
            valid.sort_unstable();
            return Err(ToolError::new(format!(
                "status 必须是 {} 之一。",
                valid.join(", ")
            )));
        }

        let stripped_content = content.trim();
        let body = if mode == "append" {
            if old_body.trim().is_empty() {
                stripped_content.to_string()
            } else {
                format!("{}\n\n{stripped_content}", old_body.trim_end())
            }
        } else {
            stripped_content.to_string()
        };

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                ToolError::new(format!("创建知识库目录失败：{}，{error}", parent.display()))
            })?;
        }
        let mut parts = vec![format_frontmatter(&merged, &extra)];
        if !body.is_empty() {
            parts.push(body);
        }
        let text = format!("{}\n", parts.join("\n\n"));
        let file_name = target
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let tmp = target.with_file_name(format!("{file_name}.tmp"));
        fs::write(&tmp, text).map_err(|error| {
            ToolError::new(format!("写入知识库笔记失败：{}，{error}", tmp.display()))
        })?;
        fs::rename(&tmp, &target).map_err(|error| {
            ToolError::new(format!("保存知识库笔记失败：{}，{error}", target.display()))
        })?;
        self.refresh_index()?;
        self.meta_for(&target)
    }

    /// 向已有笔记追加正文（只更新 `updated`）。
    pub fn append(&self, rel_path: &str, content: &str) -> Result<NoteMeta, ToolError> {
        self.write(
            rel_path,
            content,
            &WriteRequest {
                mode: "append",
                ..WriteRequest::default()
            },
        )
    }

    fn meta_for(&self, target: &Path) -> Result<NoteMeta, ToolError> {
        let bytes = fs::read(target).map_err(|error| {
            ToolError::new(format!("读取知识库笔记失败：{}。", python_os_error(&error)))
        })?;
        let text = String::from_utf8(bytes)
            .map_err(|_| ToolError::new("知识库笔记必须是 UTF-8 文本。"))?;
        let (meta, _body, _extra) = parse_frontmatter(&text);
        let root = resolve_lenient(&self.root);
        let resolved = resolve_lenient(target);
        let rel_path = match resolved.strip_prefix(&root) {
            Ok(relative) => to_posix(relative),
            Err(_) => to_posix(target),
        };
        Ok(meta_from_dict(&rel_path, &meta))
    }

    /// 按关键词与结构化字段搜索笔记（先摘要、后全文）。
    pub fn search(
        &self,
        query: &str,
        project: Option<&str>,
        tags: Option<&[String]>,
        note_type: Option<&str>,
        status: Option<&str>,
        max_results: i64,
    ) -> Result<Vec<SearchHit>, ToolError> {
        let query = query.trim();
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|term| term.to_lowercase())
            .collect();
        if query.is_empty() || terms.is_empty() {
            return Err(ToolError::new("query 不能为空。"));
        }

        let note_type = note_type.filter(|value| !value.trim().is_empty());
        let status = status.filter(|value| !value.trim().is_empty());
        let project = project.filter(|value| !value.trim().is_empty());
        let mut results: Vec<SearchHit> = Vec::new();
        for (note, body) in self.scan_notes() {
            if !matches_filters(&note, project, note_type, status, tags) {
                continue;
            }
            let title_blob = [
                note.title.as_str(),
                note.project.as_str(),
                note.note_type.as_str(),
                note.status.as_str(),
                &note.tags.join(" "),
            ]
            .join(" ")
            .to_lowercase();
            let haystack = format!("{title_blob} {}", body.to_lowercase());
            if !terms.iter().all(|term| haystack.contains(term)) {
                continue;
            }
            let score = if terms.iter().all(|term| title_blob.contains(term)) {
                3
            } else {
                1
            };
            results.push(SearchHit {
                rel_path: note.rel_path,
                title: note.title,
                project: note.project,
                note_type: note.note_type,
                status: note.status,
                tags: note.tags,
                snippet: make_snippet(&body, &terms),
                score,
            });
        }
        results.sort_by(|left, right| {
            right.score.cmp(&left.score).then_with(|| {
                left.rel_path
                    .to_lowercase()
                    .cmp(&right.rel_path.to_lowercase())
            })
        });
        results.truncate(limit(max_results, MAX_SEARCH_RESULTS));
        Ok(results)
    }

    /// 浏览知识库目录，或按字段筛选笔记。
    pub fn list_entries(
        &self,
        rel_path: Option<&str>,
        project: Option<&str>,
        tags: Option<&[String]>,
        note_type: Option<&str>,
        status: Option<&str>,
        max_results: i64,
    ) -> Result<Vec<NoteMeta>, ToolError> {
        let mut notes = self.list_notes();
        if let Some(raw_path) = rel_path.map(str::trim).filter(|value| !value.is_empty()) {
            let target = self.safe_join(raw_path, "")?;
            let root = resolve_lenient(&self.root);
            let resolved = resolve_lenient(&target);
            let Ok(relative) = resolved.strip_prefix(&root) else {
                return Err(ToolError::new(format!("知识库路径不存在：{raw_path}。")));
            };
            let target_rel = to_posix(relative);
            if target.is_dir() {
                let prefix = if target_rel.is_empty() {
                    String::new()
                } else {
                    format!("{target_rel}/")
                };
                notes.retain(|note| note.rel_path.starts_with(&prefix));
            } else if target.is_file() {
                notes.retain(|note| note.rel_path == target_rel);
            } else {
                return Err(ToolError::new(format!("知识库路径不存在：{raw_path}。")));
            }
        }

        let note_type = note_type.filter(|value| !value.trim().is_empty());
        let status = status.filter(|value| !value.trim().is_empty());
        let project = project.filter(|value| !value.trim().is_empty());
        notes.retain(|note| matches_filters(note, project, note_type, status, tags));
        notes.truncate(limit(max_results, MAX_LIST_RESULTS));
        Ok(notes)
    }
}

fn limit(max_results: i64, maximum: i64) -> usize {
    std::cmp::max(1, std::cmp::min(max_results, maximum)) as usize
}

fn matches_filters(
    note: &NoteMeta,
    project: Option<&str>,
    note_type: Option<&str>,
    status: Option<&str>,
    tags: Option<&[String]>,
) -> bool {
    if let Some(expected) = project {
        if note.project.to_lowercase() != expected.trim().to_lowercase() {
            return false;
        }
    }
    if let Some(expected) = note_type {
        if note.note_type.to_lowercase() != expected.trim().to_lowercase() {
            return false;
        }
    }
    if let Some(expected) = status {
        if note.status.to_lowercase() != expected.trim().to_lowercase() {
            return false;
        }
    }
    if let Some(filters) = tags {
        let wanted: Vec<String> = filters
            .iter()
            .map(|tag| tag.trim().to_lowercase())
            .filter(|tag| !tag.is_empty())
            .collect();
        if !wanted.is_empty() {
            let note_tags: Vec<String> = note.tags.iter().map(|tag| tag.to_lowercase()).collect();
            if !wanted.iter().any(|tag| note_tags.contains(tag)) {
                return false;
            }
        }
    }
    true
}

fn text_field(meta: &Map<String, Value>, key: &str) -> String {
    match meta.get(key) {
        Some(Value::String(text)) => text.clone(),
        Some(other) => scalar_text(other),
        None => String::new(),
    }
}

fn parse_frontmatter(text: &str) -> (Map<String, Value>, String, Vec<(String, Value)>) {
    if !text.starts_with("---\n") {
        return (Map::new(), text.to_string(), Vec::new());
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut end_index = None;
    for (index, line) in lines.iter().enumerate().skip(1) {
        if line.trim() == "---" {
            end_index = Some(index);
            break;
        }
    }
    let Some(end_index) = end_index else {
        return (Map::new(), text.to_string(), Vec::new());
    };
    let mut meta = Map::new();
    let mut extra: Vec<(String, Value)> = Vec::new();
    for line in &lines[1..end_index] {
        let stripped = line.trim();
        if stripped.is_empty() || stripped.starts_with('#') || !line.contains(':') {
            continue;
        }
        let (key, value) = line.split_once(':').expect("已确认存在冒号");
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let parsed = parse_scalar(value);
        if FRONTMATTER_FIELD_ORDER.contains(&key) {
            meta.insert(key.to_string(), parsed);
        } else {
            extra.push((key.to_string(), parsed));
        }
    }
    let body = lines[end_index + 1..].join("\n");
    (meta, body, extra)
}

fn format_frontmatter(meta: &Map<String, Value>, extra: &[(String, Value)]) -> String {
    let mut lines = vec!["---".to_string()];
    for key in FRONTMATTER_FIELD_ORDER {
        let value = meta
            .get(key)
            .cloned()
            .unwrap_or_else(|| Value::String(String::new()));
        lines.push(format!("{key}: {}", format_scalar(&value)));
    }
    for (key, value) in extra {
        lines.push(format!("{key}: {}", format_scalar(value)));
    }
    lines.push("---".to_string());
    lines.join("\n")
}

fn unquote_yaml(value: &str) -> String {
    let value = value.trim();
    let chars: Vec<char> = value.chars().collect();
    if chars.len() >= 2
        && chars[0] == chars[chars.len() - 1]
        && (chars[0] == '\'' || chars[0] == '"')
    {
        let inner: String = chars[1..chars.len() - 1].iter().collect();
        if chars[0] == '\'' {
            return inner.replace("''", "'");
        }
        return inner.replace("\\\"", "\"");
    }
    value.to_string()
}

fn parse_scalar(value: &str) -> Value {
    let value = value.trim();
    if value.starts_with('[') && value.ends_with(']') && value.chars().count() >= 2 {
        let inner = value[1..value.len() - 1].trim();
        if inner.is_empty() {
            return Value::Array(Vec::new());
        }
        return Value::Array(
            inner
                .split(',')
                .filter(|item| !item.trim().is_empty())
                .map(|item| Value::String(unquote_yaml(item)))
                .collect(),
        );
    }
    Value::String(unquote_yaml(value))
}

fn yaml_quote(value: &str) -> String {
    if value.is_empty() {
        return "\"\"".to_string();
    }
    let reserved = ["true", "false", "null", "~", "yes", "no", "on", "off"];
    let needs_quote = value != value.trim()
        || value
            .chars()
            .any(|character| ":,[]{}#&*!|>'\"%@`".contains(character))
        || reserved.contains(&value)
        || value.starts_with('-')
        || value.starts_with('?')
        || value.starts_with(' ');
    if needs_quote {
        return format!("'{}'", value.replace('\'', "''"));
    }
    value.to_string()
}

fn format_scalar(value: &Value) -> String {
    match value {
        Value::Array(items) => {
            let rendered: Vec<String> = items
                .iter()
                .map(|item| yaml_quote(&scalar_text(item)))
                .collect();
            format!("[{}]", rendered.join(", "))
        }
        other => yaml_quote(&scalar_text(other)),
    }
}

/// Python `str(value)` 的可用子集（frontmatter 值只会是字符串或字符串列表）。
fn scalar_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Bool(flag) => if *flag { "True" } else { "False" }.to_string(),
        Value::Number(_) => python_number_text(value),
        Value::Array(items) => {
            let rendered: Vec<String> = items
                .iter()
                .map(|item| match item {
                    Value::String(text) => {
                        format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
                    }
                    other => scalar_text(other),
                })
                .collect();
            format!("[{}]", rendered.join(", "))
        }
        Value::Object(_) => String::new(),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

fn meta_from_dict(rel_path: &str, meta: &Map<String, Value>) -> NoteMeta {
    let field = |key: &str| match meta.get(key) {
        Some(value) if truthy(value) => scalar_text(value),
        _ => String::new(),
    };
    let note_type = match meta.get("type") {
        Some(value) if truthy(value) => scalar_text(value),
        _ => DEFAULT_TYPE.to_string(),
    };
    let status = match meta.get("status") {
        Some(value) if truthy(value) => scalar_text(value),
        _ => DEFAULT_STATUS.to_string(),
    };
    let title = match meta.get("title") {
        Some(value) if truthy(value) => scalar_text(value),
        _ => file_stem(rel_path),
    };
    NoteMeta {
        rel_path: rel_path.to_string(),
        title,
        created: field("created"),
        updated: field("updated"),
        project: field("project"),
        note_type,
        status,
        tags: meta_tags(meta.get("tags")),
    }
}

fn meta_tags(value: Option<&Value>) -> Vec<String> {
    let items: Vec<String> = match value {
        Some(Value::Array(entries)) => entries.iter().map(scalar_text).collect(),
        Some(Value::String(text)) => text.chars().map(|item| item.to_string()).collect(),
        _ => Vec::new(),
    };
    items
        .into_iter()
        .filter(|tag| !tag.trim().is_empty())
        .collect()
}

fn make_snippet(body: &str, terms: &[String]) -> String {
    if body.trim().is_empty() {
        return String::new();
    }
    let body_chars: Vec<char> = body.chars().collect();
    let lowered: Vec<char> = body.to_lowercase().chars().collect();
    let mut first_pos: Option<usize> = None;
    for term in terms {
        let needle: Vec<char> = term.chars().collect();
        if needle.is_empty() {
            continue;
        }
        if let Some(position) = find_chars(&lowered, &needle) {
            if first_pos.map(|current| position < current).unwrap_or(true) {
                first_pos = Some(position);
            }
        }
    }
    match first_pos {
        None => body_chars
            .iter()
            .take(SNIPPET_RADIUS * 2)
            .collect::<String>()
            .trim()
            .to_string(),
        Some(position) => {
            let start = position.saturating_sub(SNIPPET_RADIUS);
            let end = std::cmp::min(body_chars.len(), position + SNIPPET_RADIUS * 2);
            let prefix = if start > 0 { "…" } else { "" };
            let suffix = if end < body_chars.len() { "…" } else { "" };
            let slice: String = body_chars[start..end].iter().collect();
            format!("{prefix}{}{suffix}", slice.trim())
        }
    }
}

fn find_chars(haystack: &[char], needle: &[char]) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len())
        .find(|start| &haystack[*start..*start + needle.len()] == needle)
}

fn file_stem(rel_path: &str) -> String {
    let name = rel_path.rsplit(['/', '\\']).next().unwrap_or(rel_path);
    match name.rfind('.') {
        Some(index) if index > 0 => name[..index].to_string(),
        _ => name.to_string(),
    }
}

fn to_posix(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn today_text() -> String {
    chrono::Local::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string()
}

/// Python `OSError` 的文案尾巴：这里只取系统消息，避免平台前缀差异。
fn python_os_error(error: &std::io::Error) -> String {
    match error.raw_os_error() {
        Some(code) => format!("[Errno {code}] {error}"),
        None => error.to_string(),
    }
}

fn argument_text(arguments: &Map<String, Value>, key: &str) -> String {
    match arguments.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.trim().to_string(),
        Some(other) => scalar_text(other).trim().to_string(),
    }
}

fn optional_text(arguments: &Map<String, Value>, key: &str) -> Option<String> {
    let value = arguments.get(key)?;
    let Value::String(text) = value else {
        return None;
    };
    let stripped = text.trim();
    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_string())
    }
}

/// 送进重排的候选描述：标题、路径与摘要即可判断相关度，不必带整段正文。
fn rerank_candidate_text(hit: &SearchHit) -> String {
    let mut parts = vec![
        format!("标题：{}", hit.title),
        format!("路径：{}", hit.rel_path),
    ];
    if !hit.snippet.trim().is_empty() {
        parts.push(format!("摘要：{}", hit.snippet));
    }
    parts.join("\n")
}

/// 重排指令：只要求按相关度排序，不做其他判断。
const RERANK_INSTRUCTIONS: &str = "给定 state 里的检索查询与过滤条件，判断每个候选项与查询的相关程度；\
state 是事实来源，不要根据候选项自身措辞推测查询。";

pub fn kb_search(
    base: &KnowledgeBase,
    rerank: &RerankOptions,
    arguments: &Map<String, Value>,
) -> ToolOutcome {
    let query = argument_text(arguments, "query");
    if query.is_empty() {
        return Err(ToolError::new("query 不能为空。"));
    }
    let project = optional_text(arguments, "project");
    let tags = read_optional_string_list(arguments, "tags");
    let note_type = optional_text(arguments, "type");
    let status = optional_text(arguments, "status");
    let max_results = read_limited_int(arguments, "max_results", 10, MAX_SEARCH_RESULTS);
    // 重排开启时先取更宽的一池候选：本地排序只用于挑池子，最终顺序与条数由决策模型决定。
    let pool = if rerank.active() {
        max_results.max(RERANK_CANDIDATE_LIMIT as i64)
    } else {
        max_results
    };
    let mut hits = base.search(
        &query,
        project.as_deref(),
        tags.as_deref(),
        note_type.as_deref(),
        status.as_deref(),
        pool,
    )?;
    if rerank.active() {
        let state = json!({
            "query": query,
            "filters": {
                "project": project,
                "tags": tags,
                "type": note_type,
                "status": status,
            },
        });
        let texts: Vec<String> = hits.iter().map(rerank_candidate_text).collect();
        if let Some(order) = reranked_order(rerank, state, RERANK_INSTRUCTIONS, &texts) {
            hits = apply_order(hits, &order);
        }
    }
    hits.truncate(max_results as usize);
    Ok(python_dumps(
        &Value::Array(hits.iter().map(SearchHit::to_value).collect()),
        2,
    ))
}

pub fn kb_read(base: &KnowledgeBase, arguments: &Map<String, Value>) -> ToolOutcome {
    let path = argument_text(arguments, "path");
    if path.is_empty() {
        return Err(ToolError::new("path 不能为空。"));
    }
    let text = base.read(&path)?;
    let max_chars = read_limited_int(
        arguments,
        "max_chars",
        READ_CHARS_DEFAULT,
        READ_CHARS_MAXIMUM,
    ) as usize;
    let chars: Vec<char> = text.chars().collect();
    if chars.len() > max_chars {
        return Ok(format!(
            "{}{READ_TRUNCATED_SUFFIX}",
            chars[..max_chars].iter().collect::<String>()
        ));
    }
    Ok(text)
}

pub fn kb_write(base: &KnowledgeBase, arguments: &Map<String, Value>) -> ToolOutcome {
    let path = argument_text(arguments, "path");
    if path.is_empty() {
        return Err(ToolError::new("path 不能为空。"));
    }
    let Some(content) = arguments.get("content").and_then(Value::as_str) else {
        return Err(ToolError::new("content 必须是字符串。"));
    };
    let mode = match arguments.get("mode") {
        None | Some(Value::Null) => "overwrite".to_string(),
        Some(Value::String(text)) => text.trim().to_lowercase(),
        Some(other) => scalar_text(other).trim().to_lowercase(),
    };
    if mode != "create" && mode != "overwrite" && mode != "append" {
        return Err(ToolError::new("mode 必须是 create、overwrite 或 append。"));
    }
    let title = optional_text(arguments, "title");
    let project = optional_text(arguments, "project");
    let tags = read_optional_string_list(arguments, "tags");
    let note_type = optional_text(arguments, "type");
    let status = optional_text(arguments, "status");
    let meta = base.write(
        &path,
        content,
        &WriteRequest {
            title: title.as_deref(),
            project: project.as_deref(),
            tags: tags.as_deref(),
            note_type: note_type.as_deref(),
            status: status.as_deref(),
            mode: &mode,
        },
    )?;
    Ok(python_dumps(
        &json!({"note": meta.to_value(), "mode": mode}),
        2,
    ))
}

pub fn kb_append(base: &KnowledgeBase, arguments: &Map<String, Value>) -> ToolOutcome {
    let path = argument_text(arguments, "path");
    if path.is_empty() {
        return Err(ToolError::new("path 不能为空。"));
    }
    let Some(content) = arguments.get("content").and_then(Value::as_str) else {
        return Err(ToolError::new("content 必须是字符串。"));
    };
    let meta = base.append(&path, content)?;
    Ok(python_dumps(
        &json!({"note": meta.to_value(), "mode": "append"}),
        2,
    ))
}

pub fn kb_list(base: &KnowledgeBase, arguments: &Map<String, Value>) -> ToolOutcome {
    let path = optional_text(arguments, "path");
    let project = optional_text(arguments, "project");
    let tags = read_optional_string_list(arguments, "tags");
    let note_type = optional_text(arguments, "type");
    let status = optional_text(arguments, "status");
    let max_results = read_limited_int(arguments, "max_results", 100, MAX_LIST_RESULTS);
    let notes = base.list_entries(
        path.as_deref(),
        project.as_deref(),
        tags.as_deref(),
        note_type.as_deref(),
        status.as_deref(),
        max_results,
    )?;
    Ok(python_dumps(
        &Value::Array(notes.iter().map(NoteMeta::to_value).collect()),
        2,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("omnicrawl-tui-knowledge-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时知识库");
        root
    }

    fn arguments(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn dot_dot_paths_are_rejected() {
        let base = KnowledgeBase::new(temp_root("escape"));
        assert_eq!(
            base.safe_join("../secret.md", "").unwrap_err().message,
            "知识库路径不允许包含 ..。"
        );
        assert_eq!(
            base.read("../secret").unwrap_err().message,
            "知识库路径不允许包含 ..。"
        );
    }

    #[test]
    fn notes_are_written_with_frontmatter_and_indexed() {
        let root = temp_root("write");
        let base = KnowledgeBase::new(&root);
        let meta = base
            .write(
                "projects/demo/2026-09-20-笔记",
                "正文",
                &WriteRequest {
                    title: Some("示例"),
                    tags: Some(&["rust".to_string(), "内核".to_string()]),
                    mode: "create",
                    ..WriteRequest::default()
                },
            )
            .expect("写入应当成功");
        assert_eq!(meta.rel_path, "projects/demo/2026-09-20-笔记.md");
        assert_eq!(meta.title, "示例");
        assert_eq!(meta.note_type, "note");
        assert_eq!(meta.status, "draft");
        assert_eq!(meta.tags, vec!["rust".to_string(), "内核".to_string()]);
        let index = std::fs::read_to_string(root.join(INDEX_FILENAME)).expect("索引应当存在");
        assert!(index.contains("共 1 篇笔记。"), "{index}");
        assert!(
            index.contains("- [示例](projects/demo/2026-09-20-笔记.md)"),
            "{index}"
        );
    }

    #[test]
    fn search_scores_title_matches_higher_and_snippets_around_the_term() {
        let root = temp_root("search");
        let base = KnowledgeBase::new(&root);
        base.write(
            "topics/rust.md",
            "Rust 内核重写进度：检索命中。",
            &WriteRequest {
                title: Some("Rust 迁移"),
                mode: "create",
                ..WriteRequest::default()
            },
        )
        .expect("写入应当成功");
        base.write(
            "topics/other.md",
            "正文里提到 rust 但与标题无关。",
            &WriteRequest {
                title: Some("其它"),
                mode: "create",
                ..WriteRequest::default()
            },
        )
        .expect("写入应当成功");
        let hits = kb_search(
            &base,
            &RerankOptions::default(),
            &arguments(json!({"query": "rust"})),
        )
        .expect("搜索应当成功");
        let parsed: Value = serde_json::from_str(&hits).expect("结果是 JSON");
        let entries = parsed.as_array().expect("结果是数组");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["path"], "topics/rust.md");
        assert_eq!(entries[0]["score"], 3);
        assert_eq!(entries[1]["score"], 1);
    }

    /// 重排桩：按给定顺序返回下标，并记录收到的候选文本（不起网络）。
    struct StubRerank {
        order: Vec<usize>,
        seen: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl super::super::decision_search::RerankClient for StubRerank {
        fn rank(
            &self,
            request: &super::super::decision_search::RerankRequest<'_>,
        ) -> Result<Vec<usize>, String> {
            *self.seen.lock().expect("记录未被毒化") = request.candidates.to_vec();
            Ok(self.order.clone())
        }
    }

    #[test]
    fn rerank_reorders_hits_and_keeps_max_results() {
        let root = temp_root("rerank");
        let base = KnowledgeBase::new(&root);
        for (path, title, body) in [
            ("topics/a.md", "甲", "rust 检索：内核工具表"),
            ("topics/b.md", "乙", "rust 检索：记忆排序"),
            ("topics/c.md", "丙", "rust 检索：知识库索引"),
        ] {
            base.write(
                path,
                body,
                &WriteRequest {
                    title: Some(title),
                    mode: "create",
                    ..WriteRequest::default()
                },
            )
            .expect("写入应当成功");
        }

        // 倒序重排：结果顺序按重排给出的下标，条数仍按 max_results。
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let options = RerankOptions {
            enabled: true,
            channel: Some(super::super::decision_search::RerankChannel {
                mode: omnicrawl_config::features::decision_model::DECISION_MODE_JEV.to_string(),
                model: "jev-latest".to_string(),
                base_url: "http://127.0.0.1:1".to_string(),
                api_key: "jv_test".to_string(),
                api_key_env: "JEV_API_KEY".to_string(),
            }),
            client: Arc::new(StubRerank {
                order: vec![2, 1, 0],
                seen: Arc::clone(&seen),
            }),
            ..RerankOptions::default()
        };
        let hits = kb_search(
            &base,
            &options,
            &arguments(json!({"query": "rust 检索", "max_results": 2})),
        )
        .expect("搜索应当成功");
        let parsed: Value = serde_json::from_str(&hits).expect("结果是 JSON");
        let entries = parsed.as_array().expect("结果是数组");
        assert_eq!(entries.len(), 2, "返回条数仍按 max_results：{hits}");
        assert_eq!(entries[0]["path"], "topics/c.md", "按重排顺序返回：{hits}");
        assert_eq!(entries[1]["path"], "topics/b.md", "{hits}");

        let recorded = seen.lock().expect("记录未被毒化").clone();
        assert_eq!(recorded.len(), 3, "候选池比 max_results 宽：{recorded:?}");
        assert!(
            recorded.iter().all(|text| text.contains("标题：")),
            "候选描述应带标题：{recorded:?}"
        );
    }

    #[test]
    fn rerank_failure_falls_back_to_local_order() {
        let root = temp_root("rerank-fail");
        let base = KnowledgeBase::new(&root);
        base.write(
            "topics/only.md",
            "rust 检索回退",
            &WriteRequest {
                title: Some("回退"),
                mode: "create",
                ..WriteRequest::default()
            },
        )
        .expect("写入应当成功");
        // 开关开着但没有可用渠道：重排不可用，检索照旧返回本地排序结果。
        let options = RerankOptions {
            enabled: true,
            channel: None,
            ..RerankOptions::default()
        };
        let hits = kb_search(
            &base,
            &options,
            &arguments(json!({"query": "检索回退"})),
        )
        .expect("检索不该因为重排不可用而失败");
        let parsed: Value = serde_json::from_str(&hits).expect("结果是 JSON");
        assert_eq!(parsed.as_array().map(Vec::len), Some(1), "{hits}");
    }
}
