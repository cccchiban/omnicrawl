//! `omnicrawl/agent/subagents/definitions.py` 的移植：Markdown Agent 定义解析、
//! 来源发现、优先级覆盖与诊断。
//!
//! 扫描顺序「离项目越近优先级越高」，首次出现的名称获胜：项目 `.omnicrawl`、项目兼容
//! `.agents`、用户、包内内置、已批准插件；插件只能提供最低优先级定义。
//!
//! frontmatter 只解析 Agent 定义真正用到的 YAML 子集：顶层映射、块列表、折叠/字面标量、
//! 行内 `[]`/`{}`、引号与注释；语义按 PyYAML `safe_load` 对齐。

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::error::AgentError;
use crate::undo::{is_relative_to, resolve_path};

/// 定义文件的字节上限。
pub const MAX_DEFINITION_FILE_BYTES: usize = 256 * 1024;
/// 定义正文（system prompt）的字符上限。
pub const MAX_SYSTEM_PROMPT_CHARS: usize = 64_000;
/// `tools` / `disallowedTools` 等列表的项数上限。
pub const MAX_LIST_ITEMS: usize = 64;
/// 列表单项的字符上限。
pub const MAX_LIST_ITEM_CHARS: usize = 128;

const ALLOWED_FIELDS: [&str; 13] = [
    "name",
    "description",
    "tools",
    "disallowedTools",
    "model",
    "maxTurns",
    "maxToolCalls",
    "permissionMode",
    "background",
    "isolation",
    "skills",
    "mcpServers",
    "gitMode",
];
const ALLOWED_PERMISSION_MODES: [&str; 3] = [
    "delegated-read-only",
    "explicit-command-allowlist",
    "standard",
];
const ALLOWED_GIT_MODES: [&str; 2] = ["readonly", "full"];
const ALLOWED_ISOLATIONS: [&str; 2] = ["shared", "worktree"];

/// 创建子执行时冻结的角色定义快照。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    pub tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub model: String,
    pub permission_mode: String,
    pub background: bool,
    pub isolation: String,
    pub skills: Vec<String>,
    pub mcp_servers: Vec<String>,
    pub git_mode: String,
    pub source_path: Option<PathBuf>,
    pub source: String,
}

/// 发现阶段的无效文件或同名覆盖诊断。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentDefinitionDiagnostic {
    pub kind: String,
    pub message: String,
    pub path: String,
    pub winner_path: String,
    pub loser_path: String,
}

impl AgentDefinitionDiagnostic {
    fn invalid(message: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            kind: "invalid".to_string(),
            message: message.into(),
            path: path.into(),
            winner_path: String::new(),
            loser_path: String::new(),
        }
    }

    fn collision(
        message: impl Into<String>,
        path: impl Into<String>,
        winner_path: impl Into<String>,
        loser_path: impl Into<String>,
    ) -> Self {
        Self {
            kind: "collision".to_string(),
            message: message.into(),
            path: path.into(),
            winner_path: winner_path.into(),
            loser_path: loser_path.into(),
        }
    }
}

/// 按「离项目越近优先级越高」规则构建不可变定义索引。
#[derive(Debug, Clone)]
pub struct AgentDefinitionRegistry {
    builtin_directory: PathBuf,
    home_directory: PathBuf,
    index: BTreeMap<String, AgentDefinition>,
    diagnostics: Vec<AgentDefinitionDiagnostic>,
    real_paths: HashSet<String>,
}

impl AgentDefinitionRegistry {
    /// `builtin_directory` 对应 Python 侧的包内 `templates/subagents`；
    /// `home_directory` 省略时取 `USERPROFILE` / `HOME`。
    pub fn new(builtin_directory: impl Into<PathBuf>, home_directory: Option<PathBuf>) -> Self {
        Self {
            builtin_directory: resolve_path(&builtin_directory.into()),
            home_directory: resolve_path(&home_directory.unwrap_or_else(default_home_directory)),
            index: BTreeMap::new(),
            diagnostics: Vec::new(),
            real_paths: HashSet::new(),
        }
    }

    pub fn diagnostics(&self) -> Vec<AgentDefinitionDiagnostic> {
        self.diagnostics.clone()
    }

    /// 重新发现当前工作区定义；已启动任务应继续使用旧定义快照。
    pub fn discover(&mut self, workspace_root: &Path, plugin_definitions: &[(String, PathBuf)]) {
        let workspace = resolve_path(workspace_root);
        self.index.clear();
        self.diagnostics.clear();
        self.real_paths.clear();

        let directories = [
            ("project", workspace.join(".omnicrawl").join("agents")),
            ("project-compat", workspace.join(".agents").join("agents")),
            (
                "user",
                self.home_directory.join(".OmniCrawl").join("agents"),
            ),
            ("builtin", self.builtin_directory.clone()),
        ];
        for (source, directory) in directories {
            self.scan_directory(&directory, source);
        }
        for (plugin_name, path) in plugin_definitions {
            self.load_path(path, &format!("plugin:{plugin_name}"), None);
        }
    }

    pub fn get(&self, name: &str) -> Option<&AgentDefinition> {
        self.index.get(&name.trim().to_lowercase())
    }

    pub fn list_all(&self) -> Vec<&AgentDefinition> {
        self.index.values().collect()
    }

    fn scan_directory(&mut self, directory: &Path, source: &str) {
        if !directory.is_dir() {
            return;
        }
        let mut paths: Vec<PathBuf> = Vec::new();
        match std::fs::read_dir(directory) {
            Ok(entries) => {
                for entry in entries {
                    let Ok(entry) = entry else {
                        self.diagnostics.push(AgentDefinitionDiagnostic::invalid(
                            "扫描 Agent 定义目录失败：读取目录项失败",
                            display_path(directory),
                        ));
                        return;
                    };
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !name.to_lowercase().ends_with(".md") {
                        continue;
                    }
                    let path = entry.path();
                    if path.is_file() {
                        paths.push(path);
                    }
                }
            }
            Err(error) => {
                self.diagnostics.push(AgentDefinitionDiagnostic::invalid(
                    format!("扫描 Agent 定义目录失败：{error}"),
                    display_path(directory),
                ));
                return;
            }
        }
        paths.sort_by_key(|path| file_name_key(path));

        let allowed_root = match resolve_strict(directory) {
            Ok(path) => path,
            Err(error) => {
                self.diagnostics.push(AgentDefinitionDiagnostic::invalid(
                    format!("解析 Agent 定义来源目录失败：{error}"),
                    display_path(directory),
                ));
                return;
            }
        };
        for path in paths {
            self.load_path(&path, source, Some(&allowed_root));
        }
    }

    fn load_path(&mut self, path: &Path, source: &str, allowed_root: Option<&Path>) {
        let resolved = match resolve_strict(&expand_user(path)) {
            Ok(path) => path,
            Err(error) => {
                self.diagnostics.push(AgentDefinitionDiagnostic::invalid(
                    format!("Agent 定义路径不存在或无法解析：{error}"),
                    display_path(path),
                ));
                return;
            }
        };
        let real_key = display_path(&resolved);
        if let Some(root) = allowed_root {
            if !is_relative_to(&resolved, root) {
                self.diagnostics.push(AgentDefinitionDiagnostic::invalid(
                    "Agent 定义解析后超出声明的来源目录，已拒绝。",
                    display_path(path),
                ));
                return;
            }
        }
        if self.real_paths.contains(&real_key) {
            return;
        }
        self.real_paths.insert(real_key.clone());

        let definition = match parse_agent_definition(&resolved, source) {
            Ok(definition) => definition,
            Err(error) => {
                self.diagnostics.push(AgentDefinitionDiagnostic::invalid(
                    error.message(),
                    real_key,
                ));
                return;
            }
        };

        if let Some(existing) = self.index.get(&definition.name) {
            let winner_path = match &existing.source_path {
                Some(path) => display_path(path),
                None => existing.source.clone(),
            };
            self.diagnostics.push(AgentDefinitionDiagnostic::collision(
                format!(
                    "Agent 定义名称 \"{}\" 冲突，保留高优先级来源。",
                    definition.name
                ),
                real_key.clone(),
                winner_path,
                real_key,
            ));
            return;
        }
        self.index.insert(definition.name.clone(), definition);
    }
}

/// 解析 YAML frontmatter + Markdown body。
pub fn parse_agent_definition(path: &Path, source: &str) -> Result<AgentDefinition, AgentError> {
    let file_path = path.to_path_buf();
    let metadata = std::fs::metadata(&file_path).map_err(|error| {
        AgentError::new(format!(
            "读取 Agent 定义元数据失败：{}，{error}",
            display_path(&file_path)
        ))
    })?;
    if metadata.len() > MAX_DEFINITION_FILE_BYTES as u64 {
        return Err(AgentError::new(format!(
            "Agent 定义文件大小超过 {MAX_DEFINITION_FILE_BYTES} 字节：{}",
            display_path(&file_path)
        )));
    }
    let bytes = std::fs::read(&file_path).map_err(|error| {
        AgentError::new(format!(
            "读取 Agent 定义失败：{}，{error}",
            display_path(&file_path)
        ))
    })?;
    let text = match String::from_utf8(strip_bom(bytes)) {
        Ok(text) => text,
        Err(error) => {
            return Err(AgentError::new(format!(
                "读取 Agent 定义失败：{}，{error}",
                display_path(&file_path)
            )))
        }
    };

    let (frontmatter_text, body) = split_frontmatter(&text, &file_path)?;
    if body.chars().count() > MAX_SYSTEM_PROMPT_CHARS {
        return Err(AgentError::new(format!(
            "Agent 定义正文超过 {MAX_SYSTEM_PROMPT_CHARS} 字符：{}",
            display_path(&file_path)
        )));
    }

    let raw = match parse_frontmatter_mapping(&frontmatter_text) {
        Ok(Some(map)) => map,
        Ok(None) => {
            return Err(AgentError::new(format!(
                "Agent 定义 frontmatter 必须是对象：{}",
                display_path(&file_path)
            )))
        }
        Err(error) => {
            return Err(AgentError::new(format!(
                "Agent 定义 YAML 解析失败：{}，{error}",
                display_path(&file_path)
            )))
        }
    };

    let mut unknown: Vec<String> = raw
        .keys()
        .filter(|key| !ALLOWED_FIELDS.contains(&key.as_str()))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        unknown.sort();
        return Err(AgentError::new(format!(
            "Agent 定义包含未知字段：{}（{}）",
            unknown.join(", "),
            display_path(&file_path)
        )));
    }

    let name = required_string(&raw, "name", &file_path, 64)?.to_lowercase();
    if !matches_name_pattern(&name) {
        return Err(AgentError::new(format!(
            "Agent 定义 name 只能使用小写字母、数字和单连字符：{name}（{}）",
            display_path(&file_path)
        )));
    }
    let description = required_string(&raw, "description", &file_path, 300)?;
    let tools = string_tuple(&raw, "tools", &file_path)?;
    let disallowed_tools = string_tuple(&raw, "disallowedTools", &file_path)?;
    let model = optional_string(&raw, "model", "inherit", &file_path, 200)?;
    let permission_mode = optional_string(
        &raw,
        "permissionMode",
        "delegated-read-only",
        &file_path,
        64,
    )?;
    if !ALLOWED_PERMISSION_MODES.contains(&permission_mode.as_str()) {
        return Err(AgentError::new(format!(
            "Agent 定义 permissionMode 不受支持：{permission_mode}（{}）",
            display_path(&file_path)
        )));
    }
    let background = bool_value(&raw, "background", false, &file_path)?;
    let isolation = optional_string(&raw, "isolation", "shared", &file_path, 32)?;
    if !ALLOWED_ISOLATIONS.contains(&isolation.as_str()) {
        return Err(AgentError::new(format!(
            "Agent 定义 isolation 不受支持：{isolation}（{}）",
            display_path(&file_path)
        )));
    }
    let git_mode = optional_string(&raw, "gitMode", "readonly", &file_path, 32)?;
    if !ALLOWED_GIT_MODES.contains(&git_mode.as_str()) {
        return Err(AgentError::new(format!(
            "Agent 定义 gitMode 不受支持：{git_mode}（{}）",
            display_path(&file_path)
        )));
    }

    Ok(AgentDefinition {
        name,
        description,
        system_prompt: body.trim().to_string(),
        tools,
        disallowed_tools,
        model,
        permission_mode,
        background,
        isolation,
        skills: string_tuple(&raw, "skills", &file_path)?,
        mcp_servers: string_tuple(&raw, "mcpServers", &file_path)?,
        git_mode,
        source_path: Some(resolve_strict(path).unwrap_or_else(|_| resolve_path(path))),
        source: source.to_string(),
    })
}

fn split_frontmatter(text: &str, path: &Path) -> Result<(String, String), AgentError> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() || lines[0].trim() != "---" {
        return Err(AgentError::new(format!(
            "Agent 定义缺少 YAML frontmatter：{}",
            display_path(path)
        )));
    }
    let mut closing = None;
    for (index, line) in lines.iter().enumerate().skip(1) {
        if line.trim() == "---" {
            closing = Some(index);
            break;
        }
    }
    let Some(closing) = closing else {
        return Err(AgentError::new(format!(
            "Agent 定义 frontmatter 未闭合：{}",
            display_path(path)
        )));
    };
    Ok((
        lines[1..closing].join("\n"),
        lines[closing + 1..].join("\n"),
    ))
}

fn required_string(
    raw: &Map<String, Value>,
    name: &str,
    path: &Path,
    max_length: usize,
) -> Result<String, AgentError> {
    match raw.get(name) {
        Some(Value::String(text)) if !text.trim().is_empty() => {
            let result = text.trim().to_string();
            if result.chars().count() > max_length {
                return Err(AgentError::new(format!(
                    "Agent 定义 {name} 最长 {max_length} 字符，当前 {}：{}",
                    result.chars().count(),
                    display_path(path)
                )));
            }
            Ok(result)
        }
        _ => Err(AgentError::new(format!(
            "Agent 定义 {name} 必须是非空字符串：{}",
            display_path(path)
        ))),
    }
}

fn optional_string(
    raw: &Map<String, Value>,
    name: &str,
    default: &str,
    path: &Path,
    max_length: usize,
) -> Result<String, AgentError> {
    if !raw.contains_key(name) {
        return Ok(default.to_string());
    }
    required_string(raw, name, path, max_length)
}

fn string_tuple(
    raw: &Map<String, Value>,
    name: &str,
    path: &Path,
) -> Result<Vec<String>, AgentError> {
    let Some(value) = raw.get(name) else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let Some(items) = value.as_array() else {
        return Err(AgentError::new(format!(
            "Agent 定义 {name} 必须是字符串数组：{}",
            display_path(path)
        )));
    };
    if items.len() > MAX_LIST_ITEMS {
        return Err(AgentError::new(format!(
            "Agent 定义 {name} 最多 {MAX_LIST_ITEMS} 项：{}",
            display_path(path)
        )));
    }
    let mut stripped: Vec<String> = Vec::new();
    for item in items {
        let Some(text) = item.as_str() else {
            return Err(AgentError::new(format!(
                "Agent 定义 {name} 必须是字符串数组：{}",
                display_path(path)
            )));
        };
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            stripped.push(trimmed.to_string());
        }
    }
    if stripped
        .iter()
        .any(|item| item.chars().count() > MAX_LIST_ITEM_CHARS)
    {
        return Err(AgentError::new(format!(
            "Agent 定义 {name} 的单项最长 {MAX_LIST_ITEM_CHARS} 字符：{}",
            display_path(path)
        )));
    }
    let mut normalized: Vec<String> = Vec::new();
    for item in &stripped {
        if !normalized.contains(item) {
            normalized.push(item.clone());
        }
    }
    if normalized.len() != stripped.len() {
        return Err(AgentError::new(format!(
            "Agent 定义 {name} 不允许重复项：{}",
            display_path(path)
        )));
    }
    Ok(normalized)
}

fn bool_value(
    raw: &Map<String, Value>,
    name: &str,
    default: bool,
    path: &Path,
) -> Result<bool, AgentError> {
    match raw.get(name) {
        None => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(AgentError::new(format!(
            "Agent 定义 {name} 必须是布尔值：{}",
            display_path(path)
        ))),
    }
}

/// 对应 `^[a-z0-9]+(?:-[a-z0-9]+)*$`。
fn matches_name_pattern(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut previous_dash = true;
    for character in name.chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            previous_dash = false;
        } else if character == '-' {
            if previous_dash {
                return false;
            }
            previous_dash = true;
        } else {
            return false;
        }
    }
    !previous_dash
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}

fn file_name_key(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn strip_bom(bytes: Vec<u8>) -> Vec<u8> {
    match bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(rest) => rest.to_vec(),
        None => bytes,
    }
}

fn default_home_directory() -> PathBuf {
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(profile);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home);
    }
    PathBuf::from(".")
}

fn expand_user(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return default_home_directory();
    }
    if let Some(rest) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
        return default_home_directory().join(rest);
    }
    path.to_path_buf()
}

fn resolve_strict(path: &Path) -> std::io::Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)?;
    Ok(strip_verbatim_prefix(canonical))
}

/// Windows 的 `canonicalize` 会给出 `\\?\` 前缀，Python 的 `Path.resolve()` 不带。
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy().to_string();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

// ------------------------------------------------------------------------ YAML

/// frontmatter 的受限 YAML 解析；`Ok(None)` 表示文档不是映射（对齐 PyYAML 的 None/序列）。
fn parse_frontmatter_mapping(text: &str) -> Result<Option<Map<String, Value>>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut map = Map::new();
    let mut seen_entry = false;
    let mut index = 0usize;
    while index < lines.len() {
        let raw = lines[index];
        let stripped = raw.trim();
        if stripped.is_empty() || stripped.starts_with('#') {
            index += 1;
            continue;
        }
        if leading_spaces(raw) != 0 {
            return Ok(None);
        }
        let Some((key_text, rest)) = split_mapping_entry(stripped) else {
            return Ok(None);
        };
        let key = parse_key(key_text)?;
        let value_text = rest.trim();
        seen_entry = true;
        if let Some(style) = block_scalar_style(value_text) {
            let (value, next) = read_block_scalar(&lines, index + 1, style)?;
            map.insert(key, value);
            index = next;
            continue;
        }
        if value_text.is_empty() {
            let (value, next) = read_indented_value(&lines, index + 1)?;
            map.insert(key, value);
            index = next;
            continue;
        }
        if value_text.starts_with('[') || value_text.starts_with('{') {
            map.insert(key, parse_flow(value_text)?);
            index += 1;
            continue;
        }
        map.insert(key, parse_plain_scalar(value_text));
        index += 1;
    }
    if !seen_entry {
        return Ok(None);
    }
    Ok(Some(map))
}

fn read_indented_value(lines: &[&str], start: usize) -> Result<(Value, usize), String> {
    let mut index = start;
    while index < lines.len() {
        let raw = lines[index];
        let stripped = raw.trim();
        if stripped.is_empty() || stripped.starts_with('#') {
            index += 1;
            continue;
        }
        let indent = leading_spaces(raw);
        if indent == 0 {
            break;
        }
        if stripped == "-" || stripped.starts_with("- ") {
            let (items, next) = read_block_list(lines, index, indent)?;
            return Ok((Value::Array(items), next));
        }
        // Agent 定义不使用嵌套映射。
        return Ok((Value::Null, index));
    }
    Ok((Value::Null, index))
}

fn read_block_list(
    lines: &[&str],
    start: usize,
    item_indent: usize,
) -> Result<(Vec<Value>, usize), String> {
    let mut items = Vec::new();
    let mut index = start;
    while index < lines.len() {
        let raw = lines[index];
        let stripped = raw.trim();
        if stripped.is_empty() || stripped.starts_with('#') {
            index += 1;
            continue;
        }
        if leading_spaces(raw) < item_indent {
            break;
        }
        if !(stripped == "-" || stripped.starts_with("- ")) {
            break;
        }
        let item_text = stripped[1..].trim();
        if item_text.is_empty() {
            items.push(Value::Null);
        } else {
            items.push(parse_plain_scalar(item_text));
        }
        index += 1;
    }
    Ok((items, index))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chomp {
    Clip,
    Strip,
    Keep,
}

#[derive(Debug, Clone, Copy)]
struct BlockStyle {
    literal: bool,
    chomp: Chomp,
}

fn block_scalar_style(text: &str) -> Option<BlockStyle> {
    let mut characters = text.chars();
    let first = characters.next()?;
    if first != '>' && first != '|' {
        return None;
    }
    let rest: String = characters.collect();
    let chomp = match rest.as_str() {
        "" => Chomp::Clip,
        "-" => Chomp::Strip,
        "+" => Chomp::Keep,
        _ => return None,
    };
    Some(BlockStyle {
        literal: first == '|',
        chomp,
    })
}

fn read_block_scalar(
    lines: &[&str],
    start: usize,
    style: BlockStyle,
) -> Result<(Value, usize), String> {
    let mut content: Vec<Option<String>> = Vec::new();
    let mut index = start;
    let mut indent: Option<usize> = None;
    while index < lines.len() {
        let raw = lines[index];
        if raw.trim().is_empty() {
            content.push(None);
            index += 1;
            continue;
        }
        let current = leading_spaces(raw);
        if current == 0 {
            break;
        }
        let base = *indent.get_or_insert(current);
        if current < base {
            break;
        }
        content.push(Some(raw[base..].to_string()));
        index += 1;
    }

    let mut trailing_blank = 0usize;
    while content.last() == Some(&None) {
        content.pop();
        trailing_blank += 1;
    }

    let mut text = String::new();
    if style.literal {
        let joined: Vec<String> = content
            .iter()
            .map(|item| item.clone().unwrap_or_default())
            .collect();
        text = joined.join("\n");
    } else {
        let mut pending_breaks = 0usize;
        for item in &content {
            match item {
                None => pending_breaks += 1,
                Some(line) => {
                    if !text.is_empty() {
                        if pending_breaks > 0 {
                            for _ in 0..pending_breaks {
                                text.push('\n');
                            }
                        } else {
                            text.push(' ');
                        }
                    }
                    text.push_str(line);
                    pending_breaks = 0;
                }
            }
        }
    }
    match style.chomp {
        Chomp::Strip => {}
        Chomp::Clip => {
            if !text.is_empty() {
                text.push('\n');
            }
        }
        Chomp::Keep => {
            for _ in 0..trailing_blank {
                text.push('\n');
            }
        }
    }
    Ok((Value::String(text), index))
}

fn parse_key(text: &str) -> Result<String, String> {
    let trimmed = text.trim();
    if let Some(inner) = quoted(trimmed, '"') {
        return Ok(unquote_double(inner));
    }
    if let Some(inner) = quoted(trimmed, '\'') {
        return Ok(inner.replace("''", "'"));
    }
    if trimmed.is_empty() {
        return Err("映射键为空".to_string());
    }
    Ok(trimmed.to_string())
}

/// 找顶层映射的分隔冒号；只在引号外、且冒号后是行尾或空格时才成立。
fn split_mapping_entry(text: &str) -> Option<(&str, &str)> {
    let bytes = text.as_bytes();
    let mut quote: Option<u8> = None;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(active) => {
                if byte == active {
                    quote = None;
                }
            }
            None => {
                if byte == b'\'' || byte == b'"' {
                    quote = Some(byte);
                } else if byte == b':' {
                    let next = bytes.get(index + 1);
                    if next.is_none() || *next.unwrap() == b' ' {
                        return Some((&text[..index], &text[index + 1..]));
                    }
                }
            }
        }
        index += 1;
    }
    None
}

fn parse_flow(value: &str) -> Result<Value, String> {
    let text = value.trim();
    if text == "[]" {
        return Ok(Value::Array(Vec::new()));
    }
    if text == "{}" {
        return Ok(Value::Object(Map::new()));
    }
    if text.starts_with('[') {
        if !text.ends_with(']') {
            return Err("flow 序列未闭合".to_string());
        }
        let inner = &text[1..text.len() - 1];
        let mut items = Vec::new();
        for part in split_flow_items(inner) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            items.push(parse_plain_scalar(part));
        }
        return Ok(Value::Array(items));
    }
    if text.starts_with('{') {
        if !text.ends_with('}') {
            return Err("flow 映射未闭合".to_string());
        }
        let inner = &text[1..text.len() - 1];
        let mut map = Map::new();
        for part in split_flow_items(inner) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let Some((key, rest)) = split_mapping_entry(part) else {
                return Err("flow 映射项非法".to_string());
            };
            map.insert(parse_key(key)?, parse_plain_scalar(rest.trim()));
        }
        return Ok(Value::Object(map));
    }
    Err("flow 值非法".to_string())
}

fn split_flow_items(text: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for character in text.chars() {
        match quote {
            Some(active) => {
                current.push(character);
                if character == active {
                    quote = None;
                }
            }
            None => {
                if character == '\'' || character == '"' {
                    quote = Some(character);
                    current.push(character);
                } else if character == ',' {
                    items.push(std::mem::take(&mut current));
                } else {
                    current.push(character);
                }
            }
        }
    }
    items.push(current);
    items
}

fn parse_plain_scalar(text: &str) -> Value {
    let value = text.trim();
    if value.is_empty() || matches!(value, "~" | "null" | "Null" | "NULL") {
        return Value::Null;
    }
    if let Some(inner) = quoted(value, '"') {
        return Value::String(unquote_double(inner));
    }
    if let Some(inner) = quoted(value, '\'') {
        return Value::String(inner.replace("''", "'"));
    }
    if value.starts_with('[') || value.starts_with('{') {
        return parse_flow(value).unwrap_or_else(|_| Value::String(value.to_string()));
    }
    match value {
        "true" | "True" | "TRUE" | "yes" | "Yes" | "YES" | "on" | "On" | "ON" => Value::Bool(true),
        "false" | "False" | "FALSE" | "no" | "No" | "NO" | "off" | "Off" | "OFF" => {
            Value::Bool(false)
        }
        _ => parse_number(value).unwrap_or_else(|| Value::String(value.to_string())),
    }
}

fn parse_number(value: &str) -> Option<Value> {
    let cleaned = value.replace('_', "");
    if let Some(rest) = cleaned
        .strip_prefix("0x")
        .or_else(|| cleaned.strip_prefix("0X"))
    {
        return i64::from_str_radix(rest, 16).ok().map(Value::from);
    }
    if let Some(rest) = cleaned
        .strip_prefix("0o")
        .or_else(|| cleaned.strip_prefix("0O"))
    {
        return i64::from_str_radix(rest, 8).ok().map(Value::from);
    }
    if let Ok(integer) = cleaned.parse::<i64>() {
        return Some(Value::from(integer));
    }
    if cleaned.contains('.') || cleaned.contains('e') || cleaned.contains('E') {
        if let Ok(float) = cleaned.parse::<f64>() {
            return serde_json::Number::from_f64(float).map(Value::Number);
        }
    }
    None
}

fn quoted(text: &str, quote: char) -> Option<&str> {
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[0] == quote as u8 && bytes[bytes.len() - 1] == quote as u8 {
        return Some(&text[1..text.len() - 1]);
    }
    None
}

fn unquote_double(text: &str) -> String {
    let mut out = String::new();
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn leading_spaces(text: &str) -> usize {
    text.len() - text.trim_start_matches(' ').len()
}
