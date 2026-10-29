//! `Edit_file` 工具：按字面替换文本，以单文件事务保护读取、匹配与写回。
//!
//! 语义基准是 `omnicrawl/workspace/tools.py` 的 `edit_file`：
//! 省略 `count` 时要求 `old_text` 恰好匹配 1 处；显式 `count=0` 替换全部、正数替换
//! 至多指定数量；编辑内部统一为 LF，写回时恢复原文行尾风格；写前再次校验版本指纹，
//! 避免覆盖锁外部进程的修改。锁与原子写复用 `omnicrawl-session` 的实现。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use omnicrawl_session::locking::{atomic_write_text, try_lock_file_now, ProcessFileLock};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::arguments::{limited_int, optional_text, raw_text};
use super::error::{ToolError, ToolOutcome};
use super::paths::WorkspacePaths;
use super::read::READ_MAX_LINE_LENGTH;

pub const EDIT_CONTEXT_LINES: i64 = 2;
const LOCK_TIMEOUT_SECONDS: f64 = 30.0;
const LOCK_POLL_SECONDS: f64 = 0.05;
const LOCK_FILE_SUFFIX: &str = ".omnicrawl.edit.lock";
/// 锁目录清扫阈值：超过这个年龄且没人持有才回收。
const LOCK_SWEEP_MAX_AGE_SECONDS: u64 = 7 * 24 * 3600;
/// 单次清扫扫描的文件上限：锁目录再大也不该拖慢第一次编辑。
const LOCK_SWEEP_MAX_ENTRIES: usize = 4096;

/// 文件版本指纹：大小、纳秒 mtime 与内容摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileVersion {
    size: usize,
    mtime_ns: u128,
    digest: String,
}

pub fn edit_file(paths: &WorkspacePaths, arguments: &Map<String, Value>) -> ToolOutcome {
    let path = paths.safe_path(&optional_text(arguments, "path"))?;
    let old_text = raw_text(arguments, "old_text");
    let new_text = raw_text(arguments, "new_text");
    let count_provided = arguments.contains_key("count");
    let count = limited_int(arguments, "count", 1, 0, 10_000);
    let display = paths.relative(&path);
    if !path.is_file() {
        return Err(ToolError::new(format!("不是文件：{display}")));
    }
    if old_text.is_empty() {
        return Err(ToolError::new("old_text 不能为空。"));
    }

    // 旧版把锁写在目标旁边；现在不写了，顺手清掉能清的那些（拿得到锁才删）。
    remove_legacy_lock(&path);
    let lock = lock_for(paths, &path);
    let _guard = lock
        .acquire(LOCK_TIMEOUT_SECONDS, LOCK_POLL_SECONDS)
        .map_err(|error| {
            ToolError::coded(
                format!("获取文件编辑锁失败：{display}，{error}"),
                "FS_LOCK_TIMEOUT",
                true,
            )
        })?;

    let original_bytes = read_utf8_bytes(&path, &display)?;
    let version = file_version(&path, &original_bytes)?;
    let original = String::from_utf8(original_bytes)
        .map_err(|_| ToolError::coded(invalid_text(&display), "FS_INVALID_TEXT", false))?;
    let original_line_endings = detect_line_endings(&original);
    let normalized_original = normalize_line_endings(&original);
    let normalized_old = normalize_line_endings(&old_text);
    let normalized_new = normalize_line_endings(&new_text);

    let occurrences = normalized_original.matches(&normalized_old).count();
    if occurrences == 0 {
        return Err(ToolError::coded(
            "未找到 old_text（匹配到 0 处），文件未修改；请重新读取文件并补充准确上下文。",
            "FS_EDIT_NOT_FOUND",
            false,
        ));
    }
    if !count_provided && occurrences != 1 {
        return Err(ToolError::coded(
            format!(
                "匹配到 {occurrences} 处 old_text，文件未修改；请提供 count 或补充上下文使其唯一。"
            ),
            "FS_EDIT_AMBIGUOUS",
            false,
        ));
    }

    let replace_count = if count == 0 {
        occurrences
    } else {
        count.min(occurrences as i64) as usize
    };
    let edited = replace_first_n(
        &normalized_original,
        &normalized_old,
        &normalized_new,
        replace_count,
    );
    let output = restore_line_endings(&edited, &original_line_endings);

    let latest_bytes = read_utf8_bytes(&path, &display)?;
    if file_version(&path, &latest_bytes)? != version {
        return Err(ToolError::coded(
            "文件在替换期间发生变化，未写入；请重新读取后重试。",
            "FS_STALE_VERSION",
            true,
        ));
    }
    atomic_write_text(&path, &output, true).map_err(|error| {
        ToolError::coded(
            format!("原子写入失败：{display}，{error}"),
            "FS_ATOMIC_WRITE_FAILED",
            true,
        )
    })?;

    let first_match_offset = normalized_original
        .find(&normalized_old)
        .unwrap_or_default();
    let first_match_line = normalized_original[..first_match_offset]
        .matches('\n')
        .count() as i64
        + 1;
    let replacement_prefix = format!(
        "{}{}",
        &normalized_original[..first_match_offset],
        normalized_new
    );
    let first_match_end_line = replacement_prefix.matches('\n').count() as i64 + 1;
    let (context_start, context_end, context) =
        format_edit_context(&output, first_match_line, first_match_end_line);
    Ok(format!(
        "已修改 {display}，替换 {replace_count} 处。\n\
         首个替换位置上下文（第 {context_start}-{context_end} 行，前后各 {EDIT_CONTEXT_LINES} 行）：\n{context}"
    ))
}

/// 同一目标文件共享的跨线程/跨进程编辑锁（Python 侧 `_EDIT_FILE_LOCKS`）。
///
/// 锁文件落在 `WorkspacePaths::lock_root()`（宿主给的是用户配置目录下的 `edit-locks`），
/// 文件名是目标绝对路径的摘要——**不再写进用户的项目树**。锁的语义不变：同机跨进程靠
/// 同一个锁文件互斥，进程退出由 OS 释放。
fn lock_for(paths: &WorkspacePaths, path: &Path) -> Arc<ProcessFileLock> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<ProcessFileLock>>>> = OnceLock::new();
    let registry = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let lock_root = paths.lock_root();
    sweep_lock_root(&lock_root);
    let key = path.to_path_buf();
    let mut guard = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .entry(key.clone())
        .or_insert_with(|| {
            Arc::new(ProcessFileLock::new(
                lock_root.join(lock_file_name(&key)),
            ))
        })
        .clone()
}

/// 锁文件名：目标绝对路径的 sha256 前 16 字节，去掉目录层级，避免把路径结构搬进锁目录。
fn lock_file_name(target: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(target.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{hex}{LOCK_FILE_SUFFIX}")
}

/// 清扫一次锁目录：一个进程只扫一遍。
///
/// 只删「年龄超过阈值且现在能立刻拿到锁」的文件——拿不到说明有进程正在编辑，
/// 绝不能动。这样锁目录不会无限增长，也不会打破互斥。
fn sweep_lock_root(root: &Path) {
    static SWEPT: OnceLock<()> = OnceLock::new();
    if SWEPT.set(()).is_err() {
        return;
    }
    let _ = sweep_lock_root_once(root, LOCK_SWEEP_MAX_AGE_SECONDS, SystemTime::now());
}

fn sweep_lock_root_once(root: &Path, max_age_seconds: u64, now: SystemTime) -> std::io::Result<usize> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        // 锁目录还没建：第一次编辑自然会建，不需要在这里代劳。
        Err(_) => return Ok(0),
    };
    let mut removed = 0usize;
    for (index, entry) in entries.flatten().enumerate() {
        if index >= LOCK_SWEEP_MAX_ENTRIES {
            break;
        }
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("lock") {
            continue;
        }
        let age_seconds = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .map(|age| age.as_secs())
            .unwrap_or(0);
        if age_seconds < max_age_seconds {
            continue;
        }
        // 先取锁确认没人用，再放手删除（Windows 上持着句柄删不掉）。
        let probe = try_lock_file_now(&path).ok().flatten();
        if probe.is_some() {
            drop(probe);
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// 删掉旧版留在目标旁边的锁文件：先确认没人持有（拿得到锁才删）。
fn remove_legacy_lock(target: &Path) {
    let legacy = PathBuf::from(format!("{}{LOCK_FILE_SUFFIX}", target.display()));
    if !legacy.exists() {
        return;
    }
    if let Ok(Some(probe)) = try_lock_file_now(&legacy) {
        drop(probe);
        let _ = std::fs::remove_file(&legacy);
    }
}

fn invalid_text(display: &str) -> String {
    format!("文件不是 UTF-8 文本或包含二进制内容：{display}")
}

fn read_utf8_bytes(path: &Path, display: &str) -> Result<Vec<u8>, ToolError> {
    let data = std::fs::read(path).map_err(|error| {
        ToolError::coded(
            format!("读取文件失败：{display}，{error}"),
            "FS_READ_FAILED",
            true,
        )
    })?;
    std::str::from_utf8(&data)
        .map_err(|_| ToolError::coded(invalid_text(display), "FS_INVALID_TEXT", false))?;
    Ok(data)
}

fn file_version(path: &Path, data: &[u8]) -> Result<FileVersion, ToolError> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        ToolError::coded(
            format!("读取文件状态失败：{}，{error}", path.display()),
            "FS_STAT_FAILED",
            true,
        )
    })?;
    let mtime_ns = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    Ok(FileVersion {
        size: data.len(),
        mtime_ns,
        digest: format!("{:x}", Sha256::digest(data)),
    })
}

fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// 原文行尾风格：混合文件按首次出现的风格恢复。
fn detect_line_endings(text: &str) -> String {
    let crlf = text.find("\r\n");
    let lf = text.find('\n');
    match (crlf, lf) {
        (Some(index), Some(other)) if index < other => "\r\n".to_string(),
        (Some(_), Some(_)) => "\n".to_string(),
        (Some(_), None) => "\r\n".to_string(),
        (None, Some(_)) => "\n".to_string(),
        (None, None) => {
            // 只有裸 `\r` 时按它恢复。
            if text.contains('\r') {
                "\r".to_string()
            } else {
                "\n".to_string()
            }
        }
    }
}

fn restore_line_endings(text: &str, line_ending: &str) -> String {
    if line_ending == "\n" {
        return text.to_string();
    }
    text.replace('\n', line_ending)
}

/// 替换前 `count` 处非重叠匹配（Python `str.replace(old, new, count)`）。
fn replace_first_n(text: &str, old: &str, new: &str, count: usize) -> String {
    if count == 0 || old.is_empty() {
        return text.to_string();
    }
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0usize;
    let mut replaced = 0usize;
    while replaced < count {
        let Some(offset) = text[cursor..].find(old) else {
            break;
        };
        let start = cursor + offset;
        result.push_str(&text[cursor..start]);
        result.push_str(new);
        cursor = start + old.len();
        replaced += 1;
    }
    result.push_str(&text[cursor..]);
    result
}

/// 编辑后首个替换位置附近的带行号上下文。
fn format_edit_context(
    text: &str,
    replacement_start_line: i64,
    replacement_end_line: i64,
) -> (i64, i64, String) {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return (0, 0, "文件修改后为空。".to_string());
    }
    let first_line = (replacement_start_line - EDIT_CONTEXT_LINES).max(1);
    let last_line = (replacement_end_line + EDIT_CONTEXT_LINES).min(lines.len() as i64);
    let numbered: Vec<String> = (first_line..=last_line)
        .map(|line_number| {
            let line = lines[(line_number - 1) as usize];
            let line = if line.chars().count() > READ_MAX_LINE_LENGTH {
                let head: String = line.chars().take(READ_MAX_LINE_LENGTH).collect();
                format!("{head}... (line truncated to {READ_MAX_LINE_LENGTH} chars)")
            } else {
                line.to_string()
            };
            format!("{line_number}: {line}")
        })
        .collect();
    (first_line, last_line, numbered.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn workspace(name: &str) -> (WorkspacePaths, PathBuf) {
        let root = std::env::temp_dir().join(format!("omnicrawl-tui-edit-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        // 锁目录单独放在工作区之外（生产同理：用户配置目录），这里用工作区的兄弟目录模拟。
        let lock_root = std::env::temp_dir().join(format!("omnicrawl-tui-edit-{name}-locks"));
        let _ = std::fs::remove_dir_all(&lock_root);
        (
            WorkspacePaths::new(&root).with_lock_root(&lock_root),
            root,
        )
    }

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn edit_lock_stays_out_of_the_workspace() {
        let (paths, root) = workspace("lock-location");
        std::fs::write(root.join("a.py"), "旧\n").expect("预置文件");
        edit_file(
            &paths,
            &args(json!({"path": "a.py", "old_text": "旧", "new_text": "新"})),
        )
        .expect("替换应成功");

        assert!(
            !root.join("a.py.omnicrawl.edit.lock").exists(),
            "锁文件不该出现在用户项目树里"
        );
        let locks: Vec<_> = std::fs::read_dir(paths.lock_root())
            .expect("锁目录应当存在")
            .flatten()
            .map(|entry| entry.path())
            .collect();
        assert_eq!(locks.len(), 1, "每个被编辑的文件一把锁：{locks:?}");
        assert!(locks[0]
            .to_string_lossy()
            .ends_with(".omnicrawl.edit.lock"));
        let content = std::fs::read_to_string(&locks[0]).unwrap_or_default();
        assert!(content.starts_with("pid="), "锁文件应写入持有者：{content}");
    }

    #[test]
    fn legacy_lock_beside_the_target_is_cleaned_up() {
        let (paths, root) = workspace("legacy-lock");
        std::fs::write(root.join("a.py"), "旧\n").expect("预置文件");
        let legacy = root.join("a.py.omnicrawl.edit.lock");
        std::fs::write(&legacy, "pid=1\n").expect("预置旧锁");

        edit_file(
            &paths,
            &args(json!({"path": "a.py", "old_text": "旧", "new_text": "新"})),
        )
        .expect("替换应成功");
        assert!(!legacy.exists(), "旧版留在目标旁的锁文件应当被清掉");
    }

    #[test]
    fn sweep_removes_idle_locks_and_keeps_held_ones() {
        let dir = std::env::temp_dir().join("omnicrawl-edit-sweep");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建锁目录");
        let idle = dir.join("idle.omnicrawl.edit.lock");
        let held = dir.join("held.omnicrawl.edit.lock");
        std::fs::write(&idle, "pid=1\n").expect("写锁");
        std::fs::write(&held, "pid=2\n").expect("写锁");
        let guard = try_lock_file_now(&held).expect("取锁").expect("应当拿到锁");

        let removed = sweep_lock_root_once(&dir, 0, SystemTime::now()).expect("清扫");
        assert_eq!(removed, 1);
        assert!(!idle.exists(), "没人用的旧锁应被回收");
        assert!(held.exists(), "有人持有的锁绝不能删");
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_replacement_reports_context() {
        let (paths, root) = workspace("unique");
        std::fs::write(root.join("a.py"), "第一行\n第二行\n第三行\n").expect("预置文件");
        let message = edit_file(
            &paths,
            &args(json!({"path": "a.py", "old_text": "第二行", "new_text": "改过的行"})),
        )
        .expect("替换应成功");
        assert!(message.starts_with("已修改 a.py，替换 1 处。"), "{message}");
        assert!(message.contains("第 1-3 行，前后各 2 行"), "{message}");
        assert!(message.contains("1: 第一行"), "{message}");
        assert!(message.contains("2: 改过的行"), "{message}");
        assert_eq!(
            std::fs::read_to_string(root.join("a.py")).expect("读回"),
            "第一行\n改过的行\n第三行\n"
        );
    }

    #[test]
    fn missing_and_ambiguous_matches_report_codes() {
        let (paths, root) = workspace("codes");
        std::fs::write(root.join("a.py"), "x\nx\n").expect("预置文件");

        let missing = edit_file(
            &paths,
            &args(json!({"path": "a.py", "old_text": "y", "new_text": "z"})),
        )
        .unwrap_err();
        assert_eq!(missing.code.as_deref(), Some("FS_EDIT_NOT_FOUND"));
        assert!(missing
            .formatted()
            .starts_with("错误码：FS_EDIT_NOT_FOUND；"));

        let ambiguous = edit_file(
            &paths,
            &args(json!({"path": "a.py", "old_text": "x", "new_text": "z"})),
        )
        .unwrap_err();
        assert_eq!(ambiguous.code.as_deref(), Some("FS_EDIT_AMBIGUOUS"));
        assert!(
            ambiguous.message.contains("匹配到 2 处"),
            "{}",
            ambiguous.message
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.py")).expect("读回"),
            "x\nx\n"
        );
    }

    #[test]
    fn explicit_count_replaces_limited_occurrences() {
        let (paths, root) = workspace("count");
        std::fs::write(root.join("a.txt"), "a\na\na\n").expect("预置文件");

        let first_only = edit_file(
            &paths,
            &args(json!({"path": "a.txt", "old_text": "a", "new_text": "b", "count": 1})),
        )
        .expect("count=1 应成功");
        assert!(
            first_only.starts_with("已修改 a.txt，替换 1 处。"),
            "{first_only}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).expect("读回"),
            "b\na\na\n"
        );

        let all = edit_file(
            &paths,
            &args(json!({"path": "a.txt", "old_text": "a", "new_text": "c", "count": 0})),
        )
        .expect("count=0 应替换全部");
        assert!(all.starts_with("已修改 a.txt，替换 2 处。"), "{all}");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).expect("读回"),
            "b\nc\nc\n"
        );
    }

    #[test]
    fn crlf_style_is_restored_and_matching_is_newline_agnostic() {
        let (paths, root) = workspace("crlf");
        std::fs::write(root.join("a.txt"), "one\r\ntwo\r\n").expect("预置文件");
        let message = edit_file(
            &paths,
            &args(json!({"path": "a.txt", "old_text": "one\ntwo", "new_text": "three\nfour"})),
        )
        .expect("跨行替换应成功");
        assert!(
            message.starts_with("已修改 a.txt，替换 1 处。"),
            "{message}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).expect("读回"),
            "three\r\nfour\r\n"
        );
    }

    #[test]
    fn empty_old_text_and_non_file_are_rejected() {
        let (paths, root) = workspace("invalid");
        std::fs::write(root.join("a.txt"), "x").expect("预置文件");
        let empty = edit_file(
            &paths,
            &args(json!({"path": "a.txt", "old_text": "", "new_text": "y"})),
        )
        .unwrap_err();
        assert_eq!(empty.message, "old_text 不能为空。");

        let not_a_file = edit_file(
            &paths,
            &args(json!({"path": "missing.txt", "old_text": "x", "new_text": "y"})),
        )
        .unwrap_err();
        assert_eq!(not_a_file.message, "不是文件：missing.txt");
    }

    #[test]
    fn stale_version_is_detected_before_write() {
        let (_paths, root) = workspace("stale");
        std::fs::write(root.join("a.txt"), "x\n").expect("预置文件");
        let version = file_version(&root.join("a.txt"), b"x\n").expect("版本指纹");
        std::fs::write(root.join("a.txt"), "y\n").expect("外部修改");
        let latest = file_version(&root.join("a.txt"), b"y\n").expect("新指纹");
        assert_ne!(version, latest, "外部修改后指纹必须变化");
    }

    #[test]
    fn helper_replace_first_n_matches_python_semantics() {
        assert_eq!(replace_first_n("aaa", "a", "b", 2), "bba");
        assert_eq!(replace_first_n("aaa", "a", "b", 0), "aaa");
        assert_eq!(replace_first_n("abab", "ab", "x", 5), "xx");
        assert_eq!(replace_first_n("中文中文", "中文", "字", 1), "字中文");
    }

    #[test]
    fn line_ending_detection_prefers_the_first_seen_style() {
        assert_eq!(detect_line_endings("a\r\nb\n"), "\r\n");
        assert_eq!(detect_line_endings("a\nb\r\n"), "\n");
        assert_eq!(detect_line_endings("a\rb"), "\r");
        assert_eq!(detect_line_endings("ab"), "\n");
    }
}
