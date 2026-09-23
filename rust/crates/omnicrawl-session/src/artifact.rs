//! 会话工具输出 artifact、脱敏与路径安全：对齐 Python `omnicrawl/state/session_artifacts.py`。
//!
//! 超长工具输出不直接进模型上下文：完整内容写进会话 artifacts 目录，上下文只留头尾预览与
//! 相对路径；落盘的每一份文本都先过脱敏，公开消费者永远接触不到原始文本。

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::error::SessionStoreError;
use crate::naming::clean_title;
use crate::redaction::{redact_sensitive_text, redact_sensitive_values};

pub const TOOL_RESULT_INLINE_OUTPUT_CHARS: usize = 8 * 1024;
pub const TOOL_RESULT_LARGE_OUTPUT_CHARS: usize = 128 * 1024;
pub const TOOL_RESULT_PREVIEW_CHARS: usize = 1200;
pub const SUBAGENT_RESULT_LARGE_OUTPUT_CHARS: usize = 128 * 1024;
const ARTIFACTS_DIR_NAME: &str = "artifacts";

pub struct SessionArtifactStore {
    root: PathBuf,
    artifacts_dir: PathBuf,
}

impl SessionArtifactStore {
    pub fn new(root: impl Into<PathBuf>, artifacts_dir: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            artifacts_dir: artifacts_dir.into(),
        }
    }

    /// 事件载荷的安全投影入口：先处理 UI artifact，再脱敏，最后决定内联还是落盘。
    pub fn prepare_event_payload(
        &self,
        session_id: &str,
        event_type: &str,
        payload: Option<&Value>,
    ) -> Result<Value, SessionStoreError> {
        let empty = Value::Object(Map::new());
        let raw = payload.unwrap_or(&empty);
        let prepared = if event_type == "tool_result" {
            self.prepare_tool_ui_artifact_payload(session_id, raw)?
        } else {
            raw.clone()
        };
        let safe = redact_sensitive_values(&prepared);
        if event_type == "tool_result" {
            return self.prepare_tool_result_payload(session_id, &safe);
        }
        Ok(safe)
    }

    /// 公开入口：把完整工具输出写入 artifact 文件，返回相对路径。
    pub fn write_tool_result_artifact(
        &self,
        session_id: &str,
        output: &str,
    ) -> Result<String, SessionStoreError> {
        let hash = sha256_hex(output);
        self.write_tool_artifact(session_id, output, &hash, false)
    }

    /// 生成可安全回传的子任务摘要，并在结果较大时写入任务级 JSON artifact。
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_subagent_result(
        &self,
        session_id: &str,
        task_id: &str,
        agent_type: &str,
        description: &str,
        result_text: &str,
        summary_chars: usize,
    ) -> Result<Value, SessionStoreError> {
        if !is_subagent_task_id(task_id) {
            return Err(SessionStoreError::new(format!(
                "SubAgent task_id 格式无效：{task_id}"
            )));
        }
        if summary_chars == 0 {
            return Err(SessionStoreError::new(
                "SubAgent summary_chars 必须是正整数。",
            ));
        }

        let safe_result = redact_sensitive_text(result_text.trim());
        let summary = if char_len(&safe_result) > summary_chars {
            format!(
                "{}\n... 子任务结果已截断，完整结果见 artifact。",
                take_chars(&safe_result, summary_chars)
            )
        } else {
            safe_result.clone()
        };
        if char_len(&safe_result) <= summary_chars {
            return Ok(serde_json::json!({"summary": summary, "artifacts": []}));
        }

        let output_hash = sha256_hex(&safe_result);
        let mut document = Map::new();
        document.insert("version".to_string(), serde_json::json!(1));
        document.insert("task_id".to_string(), serde_json::json!(task_id));
        document.insert(
            "agent_type".to_string(),
            serde_json::json!(redact_sensitive_text(agent_type)),
        );
        document.insert(
            "description".to_string(),
            serde_json::json!(redact_sensitive_text(description)),
        );
        document.insert("result".to_string(), serde_json::json!(safe_result));
        document.insert(
            "result_size_chars".to_string(),
            serde_json::json!(char_len(&safe_result)),
        );
        document.insert("result_sha256".to_string(), serde_json::json!(output_hash));
        document.insert("truncated".to_string(), serde_json::json!(false));

        let session_artifacts_dir = self.ensure_session_artifacts_dir(session_id)?;
        let subagents_dir = session_artifacts_dir.join("subagents");
        if !is_relative_to(&subagents_dir, &self.root) {
            return Err(SessionStoreError::new(format!(
                "SubAgent artifact 目录越界：{task_id}"
            )));
        }
        let path = subagents_dir.join(format!("{task_id}.json"));
        if !is_relative_to(&path, &self.root) {
            return Err(SessionStoreError::new(format!(
                "SubAgent artifact 路径越界：{task_id}"
            )));
        }
        std::fs::create_dir_all(&subagents_dir).map_err(|error| {
            SessionStoreError::new(format!(
                "创建 artifact 目录失败：{}，{error}",
                subagents_dir.display()
            ))
        })?;
        let text = format!(
            "{}\n",
            serde_json::to_string_pretty(&Value::Object(document))
                .expect("子任务 artifact 可序列化")
        );
        crate::locking::atomic_write_text(&path, &text, true).map_err(|error| {
            SessionStoreError::new(format!("写入 artifact 失败：{}，{error}", path.display()))
        })?;
        let artifact_path = relative_posix(&path, &self.root)?;
        Ok(serde_json::json!({
            "summary": summary,
            "artifacts": [{
                "type": "subagent_result",
                "artifact_path": artifact_path,
                "size_chars": char_len(&safe_result),
                "sha256": output_hash,
                "truncated": false,
            }],
        }))
    }

    /// 读取 artifact 文本；只允许读取当前会话目录下的安全相对路径。
    pub fn read_text(
        &self,
        session_id: &str,
        artifact_path: &str,
    ) -> Result<String, SessionStoreError> {
        let normalized =
            normalize_relative_artifact_path(&Value::String(artifact_path.to_string()))?;
        let parts: Vec<&str> = normalized.split('/').collect();
        if parts.len() < 2 || parts[1] != session_id {
            return Err(SessionStoreError::new(format!(
                "artifact 路径必须位于当前会话目录：{artifact_path}"
            )));
        }
        let path = self.root.join(&normalized);
        if !is_relative_to(&path, &self.root) {
            return Err(SessionStoreError::new(format!(
                "artifact 路径越界：{artifact_path}"
            )));
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(
                SessionStoreError::new(format!("artifact 不存在：{artifact_path}")),
            ),
            Err(error) => Err(SessionStoreError::new(format!(
                "读取 artifact 失败：{artifact_path}，{error}"
            ))),
        }
    }

    /// 工具结果载荷：算出哈希与长度，短则内联（脱敏后），长则落盘并留预览。
    fn prepare_tool_result_payload(
        &self,
        session_id: &str,
        payload: &Value,
    ) -> Result<Value, SessionStoreError> {
        let Some(output) = payload.get("output").and_then(Value::as_str) else {
            return Ok(payload.clone());
        };
        let mut result = as_object(payload);
        let output_hash = sha256_hex(output);
        result.insert("output_sha256".to_string(), serde_json::json!(output_hash));
        result.insert(
            "output_size_chars".to_string(),
            serde_json::json!(char_len(output)),
        );
        if char_len(output) <= TOOL_RESULT_INLINE_OUTPUT_CHARS {
            result.insert(
                "output".to_string(),
                serde_json::json!(redact_sensitive_text(output)),
            );
            result.insert("storage".to_string(), serde_json::json!("inline"));
            return Ok(Value::Object(result));
        }

        result.insert(
            "output_preview".to_string(),
            serde_json::json!(redact_sensitive_text(&preview_text(
                output,
                TOOL_RESULT_PREVIEW_CHARS
            ))),
        );
        result.insert(
            "output".to_string(),
            serde_json::json!(tool_output_summary(output)),
        );
        result.insert("storage".to_string(), serde_json::json!("artifact"));
        // artifact 已取消内容上限；保留字段供旧会话读取，并明确表示当前持久化文本是完整的。
        result.insert("artifact_truncated".to_string(), serde_json::json!(false));
        let artifact_path = self.write_tool_artifact(session_id, output, &output_hash, false)?;
        result.insert(
            "artifact_path".to_string(),
            serde_json::json!(artifact_path),
        );
        Ok(Value::Object(result))
    }

    /// HTML 预览 artifact：同样先脱敏再落盘，哈希描述的是实际可读取的内容。
    fn prepare_tool_ui_artifact_payload(
        &self,
        session_id: &str,
        payload: &Value,
    ) -> Result<Value, SessionStoreError> {
        let Some(artifact) = payload.get("ui_artifact").and_then(Value::as_object) else {
            return Ok(payload.clone());
        };
        if artifact.get("type").and_then(Value::as_str) != Some("html") {
            return Ok(payload.clone());
        }
        // Python 是 `str(artifact.get("title") or "HTML 预览")`：空串算假、回退默认标题。
        let title = match artifact.get("title") {
            Some(Value::String(text)) if !text.is_empty() => clean_title(text),
            Some(Value::Number(number)) => clean_title(&number.to_string()),
            Some(Value::Bool(true)) => clean_title("true"),
            // 数组/对象标题没有实际调用方；Python 会写 repr，这里退回默认标题。
            _ => clean_title("HTML 预览"),
        };
        let source_path = artifact
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut result = as_object(payload);

        let html = artifact
            .get("html")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if html.trim().is_empty() {
            result.insert(
                "ui_artifact".to_string(),
                serde_json::json!({"type": "html", "title": title, "path": source_path}),
            );
            return Ok(Value::Object(result));
        }

        let persisted_html = redact_sensitive_html(html);
        let html_hash = sha256_hex(&persisted_html);
        let artifact_path = self.write_html_artifact(session_id, &persisted_html, &html_hash)?;
        result.insert(
            "ui_artifact".to_string(),
            serde_json::json!({
                "type": "html",
                "title": title,
                "path": source_path,
                "artifact_path": artifact_path,
                "html_size_chars": char_len(&persisted_html),
                "html_sha256": html_hash,
                "redacted": true,
            }),
        );
        Ok(Value::Object(result))
    }

    fn write_html_artifact(
        &self,
        session_id: &str,
        html: &str,
        output_hash: &str,
    ) -> Result<String, SessionStoreError> {
        let directory = self.ensure_session_artifacts_dir(session_id)?;
        let filename = format!("html_preview_{}.html", take_chars(output_hash, 16));
        let path = directory.join(&filename);
        if !is_relative_to(&path, &self.root) {
            return Err(SessionStoreError::new(format!(
                "HTML artifact 路径越界：{filename}"
            )));
        }
        std::fs::write(&path, html).map_err(|error| {
            SessionStoreError::new(format!(
                "写入 HTML artifact 失败：{}，{error}",
                path.display()
            ))
        })?;
        relative_posix(&path, &self.root)
    }

    fn write_tool_artifact(
        &self,
        session_id: &str,
        output: &str,
        output_hash: &str,
        truncated: bool,
    ) -> Result<String, SessionStoreError> {
        let directory = self.ensure_session_artifacts_dir(session_id)?;
        let filename = format!("tool_result_{}.txt", take_chars(output_hash, 16));
        let path = directory.join(&filename);
        if !is_relative_to(&path, &self.root) {
            return Err(SessionStoreError::new(format!(
                "artifact 路径越界：{filename}"
            )));
        }
        let mut artifact_text = redact_sensitive_text(output);
        if truncated {
            artifact_text.push_str(&format!(
                "\n... artifact 已按安全上限截断，原始输出字符数：{}，sha256：{output_hash}。",
                char_len(output)
            ));
        }
        std::fs::write(&path, artifact_text).map_err(|error| {
            SessionStoreError::new(format!(
                "写入工具输出 artifact 失败：{}，{error}",
                path.display()
            ))
        })?;
        relative_posix(&path, &self.root)
    }

    /// artifact 根目录（`artifacts/`）：undo 预检按「根 + 会话 id」解析补丁路径。
    pub(crate) fn artifacts_root(&self) -> PathBuf {
        self.artifacts_dir.clone()
    }

    pub(crate) fn ensure_session_artifacts_dir(
        &self,
        session_id: &str,
    ) -> Result<PathBuf, SessionStoreError> {
        let directory = self.artifacts_dir.join(session_id);
        if !is_relative_to(&directory, &self.root) {
            return Err(SessionStoreError::new(format!(
                "artifact 目录越界：{session_id}"
            )));
        }
        std::fs::create_dir_all(&directory).map_err(|error| {
            SessionStoreError::new(format!(
                "创建 artifact 目录失败：{}，{error}",
                directory.display()
            ))
        })?;
        Ok(directory)
    }
}

/// artifact 路径必须是位于 artifacts 目录下的安全相对路径。
pub fn normalize_relative_artifact_path(raw_path: &Value) -> Result<String, SessionStoreError> {
    let Some(text) = raw_path.as_str() else {
        return Err(SessionStoreError::new("artifact 路径必须是非空字符串。"));
    };
    if text.trim().is_empty() {
        return Err(SessionStoreError::new("artifact 路径必须是非空字符串。"));
    }
    let normalized = text.trim().replace('\\', "/");
    if normalized.starts_with('/') || has_parent_component(&normalized) {
        return Err(SessionStoreError::new(format!(
            "artifact 路径必须是安全相对路径：{text}"
        )));
    }
    let first = normalized.split('/').next().unwrap_or_default();
    if !first.eq_ignore_ascii_case(ARTIFACTS_DIR_NAME) {
        return Err(SessionStoreError::new(format!(
            "artifact 路径必须位于 artifacts 目录：{text}"
        )));
    }
    Ok(normalized)
}

/// 清理会被直接落盘和回放的 HTML 中常见凭据表示（不改结构，只替换凭据值本身）。
pub fn redact_sensitive_html(html: &str) -> String {
    let redacted = replace_html_credentials(html);
    redact_sensitive_text(&redacted)
}

/// 预览文本：超长时保留头尾，中间用省略提示代替。
pub fn preview_text(text: &str, max_chars: usize) -> String {
    if char_len(text) <= max_chars {
        return text.to_string();
    }
    let head_chars = max_chars / 2;
    let tail_chars = max_chars - head_chars;
    format!(
        "{}\n... 中间内容已省略 ...\n{}",
        take_chars(text, head_chars),
        take_last_chars(text, tail_chars)
    )
}

/// 超长工具输出在上下文里的替代摘要。
pub fn tool_output_summary(output: &str) -> String {
    format!(
        "工具输出较大，已写入 artifact；字符数：{}，sha256：{}。",
        char_len(output),
        sha256_hex(output)
    )
}

fn replace_html_credentials(html: &str) -> String {
    let attributed = replace_credential_values(html, CredentialStyle::Attribute);
    replace_credential_values(&attributed, CredentialStyle::JsonProperty)
}

#[derive(Clone, Copy)]
enum CredentialStyle {
    /// `key="value"`
    Attribute,
    /// `"key": "value"`
    JsonProperty,
}

impl CredentialStyle {
    fn separator(self) -> char {
        match self {
            CredentialStyle::Attribute => '=',
            CredentialStyle::JsonProperty => ':',
        }
    }
}

/// 按 `(\bkey\s*=\s*["'])(.*?)(["'])`（或 JSON 变体）把凭据值换成 `***`，其余原样保留。
fn replace_credential_values(html: &str, style: CredentialStyle) -> String {
    let mut result = String::with_capacity(html.len());
    let mut index = 0;
    while index < html.len() {
        if !html.is_char_boundary(index) {
            index += 1;
            continue;
        }
        match match_credential(html, index, style) {
            Some((end, replacement)) if end > index => {
                result.push_str(&replacement);
                index = end;
            }
            _ => {
                let character = html[index..].chars().next().unwrap_or_default();
                result.push(character);
                index += character.len_utf8();
            }
        }
    }
    result
}

fn match_credential(html: &str, index: usize, style: CredentialStyle) -> Option<(usize, String)> {
    let (key_start, quoted_key) = match style {
        CredentialStyle::Attribute => {
            if previous_is_word_char(html, index) {
                return None;
            }
            (index, false)
        }
        CredentialStyle::JsonProperty => {
            let quote = html[index..].chars().next()?;
            if quote != '"' && quote != '\'' {
                return None;
            }
            (index + 1, true)
        }
    };

    let key_len = match_sensitive_key_len(html, key_start)?;
    let mut cursor = key_start + key_len;
    if quoted_key {
        let quote = html[cursor..].chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        cursor += 1;
    }
    cursor = skip_whitespace(html, cursor);
    if !html[cursor..].starts_with(style.separator()) {
        return None;
    }
    cursor = skip_whitespace(html, cursor + 1);
    let quote = html[cursor..].chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }

    // `.*?` 不含换行且非贪婪：值停在最近的任一种引号处（可以为空）。
    let value_start = cursor + 1;
    let mut value_end = value_start;
    loop {
        let character = html[value_end..].chars().next()?;
        if character == '"' || character == '\'' {
            break;
        }
        if character == '\n' {
            return None;
        }
        value_end += character.len_utf8();
    }
    let closing = html[value_end..].chars().next()?;
    let end = value_end + closing.len_utf8();
    Some((end, format!("{}***{closing}", &html[index..value_start])))
}

/// 敏感键名（与 Python 的 alternation 顺序一致），返回消耗的长度。
fn match_sensitive_key_len(html: &str, start: usize) -> Option<usize> {
    const KEYS: [&[&str]; 12] = [
        &["api", "key"],
        &["apikey"],
        &["access", "key"],
        &["secret", "key"],
        &["access", "token"],
        &["refresh", "token"],
        &["id", "token"],
        &["authorization"],
        &["cookie"],
        &["password"],
        &["secret"],
        &["token"],
    ];
    for parts in KEYS {
        if let Some(length) = match_key_parts(html, start, parts) {
            return Some(length);
        }
    }
    None
}

/// 逐段比较键名，段之间允许一个 `_`/`-` 分隔符。
fn match_key_parts(html: &str, start: usize, parts: &[&str]) -> Option<usize> {
    let mut cursor = start;
    for (position, part) in parts.iter().enumerate() {
        let window = html.get(cursor..)?;
        let head = window.get(..part.len())?;
        if !head.eq_ignore_ascii_case(part) {
            return None;
        }
        cursor += part.len();
        if position + 1 < parts.len() {
            if let Some(character) = html[cursor..].chars().next() {
                if matches!(character, '_' | '-') {
                    cursor += 1;
                }
            }
        }
    }
    Some(cursor - start)
}

fn previous_is_word_char(text: &str, index: usize) -> bool {
    text.get(..index)
        .and_then(|prefix| prefix.chars().next_back())
        .is_some_and(|character| character.is_alphanumeric() || character == '_')
}

fn skip_whitespace(text: &str, index: usize) -> usize {
    let mut cursor = index;
    while let Some(character) = text[cursor..].chars().next() {
        if !character.is_whitespace() {
            break;
        }
        cursor += character.len_utf8();
    }
    cursor
}

fn as_object(payload: &Value) -> Map<String, Value> {
    payload.as_object().cloned().unwrap_or_default()
}

fn is_subagent_task_id(task_id: &str) -> bool {
    let Some(rest) = task_id.strip_prefix("task-") else {
        return false;
    };
    rest.len() == 12
        && rest
            .chars()
            .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
}

fn has_parent_component(path: &str) -> bool {
    path.split('/').any(|part| part == "..")
}

fn is_relative_to(path: &Path, root: &Path) -> bool {
    let normalized_path = normalize_path(path);
    let normalized_root = normalize_path(root);
    normalized_path.starts_with(&normalized_root)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

fn relative_posix(path: &Path, root: &Path) -> Result<String, SessionStoreError> {
    path.strip_prefix(root)
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .map_err(|_| SessionStoreError::new(format!("artifact 路径越界：{}", path.display())))
}

fn sha256_hex(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

fn take_chars(text: &str, count: usize) -> String {
    text.chars().take(count).collect()
}

fn take_last_chars(text: &str, count: usize) -> String {
    let total = char_len(text);
    text.chars().skip(total.saturating_sub(count)).collect()
}
