//! Hugging Face 模型下载与模型目录管理：`omnicrawl/tts/download.py` 的等价实现。
//!
//! 枚举仓库文件 + 逐个流式下载到磁盘，并承担模型目录的发现/就绪判断/下载
//! （[`ensure_model_dir`]）。只依赖 HTTP 客户端与标准库，**不依赖 ONNX 运行时与
//! 分词器**——模型下载与推理引擎解耦，缺失引擎时设置面板仍可下载模型。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::config::resolve_model_dir;
use crate::paths::resolve_lenient;

const HF_API_BASE: &str = "https://huggingface.co/api/models";
const HF_RESOLVE_BASE: &str = "https://huggingface.co";

/// 官方 ONNX 模型仓库。
pub const TTS_REPO_ID: &str = "OpenMOSS-Team/MOSS-TTS-Nano-100M-ONNX";
pub const CODEC_REPO_ID: &str = "OpenMOSS-Team/MOSS-Audio-Tokenizer-Nano-ONNX";

/// manifest 候选相对路径（下载布局可能是仓库根，或多套一层仓库名）。
pub const MANIFEST_CANDIDATE_RELATIVE_PATHS: [&str; 3] = [
    "browser_poc_manifest.json",
    "MOSS-TTS-Nano-100M-ONNX/browser_poc_manifest.json",
    "MOSS-TTS-Nano-ONNX-CPU/browser_poc_manifest.json",
];

const TTS_LAYOUT_REQUIRED_NAMES: [&str; 3] = [
    "browser_poc_manifest.json",
    "tts_browser_onnx_meta.json",
    "tokenizer.model",
];
const CODEC_LAYOUT_REQUIRED_NAMES: [&str; 1] = ["codec_browser_onnx_meta.json"];

const TTS_ALLOW_PATTERNS: [&str; 4] = ["*.onnx", "*.data", "*.json", "tokenizer.model"];
const CODEC_ALLOW_PATTERNS: [&str; 3] = ["*.onnx", "*.data", "*.json"];

/// 下载重试次数（网络抖动重试）。
const DOWNLOAD_RETRIES: usize = 3;
/// 单文件下载进度日志间隔（MB）。
const PROGRESS_LOG_INTERVAL_MB: f64 = 50.0;
const USER_AGENT: &str = "omnicrawl-tts";

/// 仓库内一个待下载文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoFile {
    pub path: String,
    pub size: u64,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into()
}

fn get_reader(
    agent: &ureq::Agent,
    url: &str,
    timeout_seconds: u64,
) -> Result<Box<dyn Read>, String> {
    let timeout = Duration::from_secs(timeout_seconds);
    let response = agent
        .get(url)
        .config()
        .timeout_connect(Some(timeout))
        .timeout_recv_response(Some(timeout))
        .timeout_recv_body(Some(timeout))
        .build()
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|error| format!("请求失败：{error}"))?;
    let status = response.status().as_u16();
    if status >= 400 {
        return Err(format!("HTTP {status}：{url}"));
    }
    Ok(Box::new(response.into_body().into_reader()))
}

/// 列出仓库全部文件（含大小），通过 HF API。
pub fn list_repo_files(agent: &ureq::Agent, repo_id: &str) -> Result<Vec<RepoFile>, String> {
    let url = format!("{HF_API_BASE}/{repo_id}/tree/main?recursive=true&expand=true");
    let mut text = String::new();
    get_reader(agent, &url, 60)?
        .read_to_string(&mut text)
        .map_err(|error| format!("读取仓库文件列表失败：{error}"))?;
    let entries: Value = serde_json::from_str(&text)
        .map_err(|error| format!("仓库文件列表不是合法 JSON：{error}"))?;
    let Some(rows) = entries.as_array() else {
        return Err("仓库文件列表不是数组。".to_string());
    };
    let mut files = Vec::new();
    for row in rows {
        if row.get("type").and_then(Value::as_str) == Some("directory") {
            continue;
        }
        let path = row.get("path").and_then(Value::as_str).unwrap_or_default();
        if path.is_empty() || path.ends_with('/') {
            continue;
        }
        files.push(RepoFile {
            path: path.to_string(),
            size: row.get("size").and_then(Value::as_u64).unwrap_or(0),
        });
    }
    Ok(files)
}

/// 文件名是否命中通配模式（语义对齐 Python `fnmatch` 的 `*` / `?`）。
pub fn matches_allow_patterns(path: &str, allow_patterns: &[&str]) -> bool {
    let filename = path.rsplit('/').next().unwrap_or(path);
    allow_patterns
        .iter()
        .any(|pattern| fnmatch(filename, pattern))
}

fn fnmatch(name: &str, pattern: &str) -> bool {
    let name: Vec<char> = name.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let (mut name_index, mut pattern_index) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while name_index < name.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == '?' || pattern[pattern_index] == name[name_index])
        {
            name_index += 1;
            pattern_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == '*' {
            star = Some((pattern_index, name_index));
            pattern_index += 1;
        } else if let Some((star_pattern, star_name)) = star {
            pattern_index = star_pattern + 1;
            name_index = star_name + 1;
            star = Some((star_pattern, star_name + 1));
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == '*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

/// 流式下载单个文件到磁盘，带重试、进度日志与可选进度回调。
///
/// `progress(done_bytes, total_bytes)` 每写一个 1MB 块触发一次；`total_bytes`
/// 未知时为 0，调用方据此决定是否显示百分比。
fn download_file(
    agent: &ureq::Agent,
    url: &str,
    destination: &Path,
    expected_size: u64,
    progress: &mut Option<&mut dyn FnMut(u64, u64)>,
) -> Result<(), String> {
    let mut last_error = String::new();
    for attempt in 1..=DOWNLOAD_RETRIES {
        match download_file_once(agent, url, destination, expected_size, progress) {
            Ok(()) => return Ok(()),
            Err(error) => {
                last_error = error;
                eprintln!("下载失败（第 {attempt}/{DOWNLOAD_RETRIES} 次）：{last_error}");
            }
        }
    }
    Err(format!("下载失败：{url}（{last_error}）"))
}

fn download_file_once(
    agent: &ureq::Agent,
    url: &str,
    destination: &Path,
    expected_size: u64,
    progress: &mut Option<&mut dyn FnMut(u64, u64)>,
) -> Result<(), String> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("{error}"))?;
    }
    let partial = destination.with_file_name(format!(
        "{}.part",
        destination
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default()
    ));

    let mut reader = get_reader(agent, url, 300)?;
    let mut output = std::fs::File::create(&partial)
        .map_err(|error| format!("创建 {} 失败：{error}", partial.display()))?;
    let mut buffer = vec![0u8; 1 << 20];
    let mut received = 0u64;
    let mut last_log_mb = 0.0f64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("读取响应失败：{error}"))?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .map_err(|error| format!("写入 {} 失败：{error}", partial.display()))?;
        received += read as u64;
        if let Some(callback) = progress.as_deref_mut() {
            callback(received, expected_size);
        }
        let received_mb = received as f64 / 1e6;
        if received_mb - last_log_mb >= PROGRESS_LOG_INTERVAL_MB {
            last_log_mb = received_mb;
            let total = if expected_size > 0 {
                format!("/{:.0} MB", expected_size as f64 / 1e6)
            } else {
                String::new()
            };
            eprintln!(
                "  {} 已下载 {:.0} MB{total}",
                destination
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default(),
                received_mb
            );
        }
    }
    output.flush().map_err(|error| format!("{error}"))?;
    drop(output);

    let written = std::fs::metadata(&partial)
        .map_err(|error| format!("{error}"))?
        .len();
    if written != received {
        return Err("下载字节数与写入不一致".to_string());
    }
    std::fs::rename(&partial, destination).map_err(|error| format!("{error}"))?;
    eprintln!(
        "  {} 完成（{:.1} MB）",
        destination
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default(),
        received as f64 / 1e6
    );
    Ok(())
}

/// 下载仓库中匹配 `allow_patterns` 的全部文件到 `local_dir`。
///
/// `progress(done_bytes, total_bytes)` 按仓库累计进度触发：done 为已下载字节数
/// （含先前文件），total 为该仓库匹配文件总字节数。
pub fn download_repo(
    agent: &ureq::Agent,
    repo_id: &str,
    local_dir: &Path,
    allow_patterns: &[&str],
    mut progress: Option<&mut dyn FnMut(u64, u64)>,
) -> Result<(), String> {
    let files = list_repo_files(agent, repo_id)?;
    let matched: Vec<RepoFile> = files
        .into_iter()
        .filter(|entry| matches_allow_patterns(&entry.path, allow_patterns))
        .collect();
    if matched.is_empty() {
        return Err(format!(
            "仓库 {repo_id} 中没有匹配 {allow_patterns:?} 的文件。"
        ));
    }
    let total_bytes: u64 = matched.iter().map(|entry| entry.size).sum();
    eprintln!(
        "开始下载 {repo_id}：{} 个文件，共 {:.0} MB",
        matched.len(),
        total_bytes as f64 / 1e6
    );

    let mut base_done = 0u64;
    for entry in matched {
        let url = format!(
            "{HF_RESOLVE_BASE}/{repo_id}/resolve/main/{}",
            entry.path.replace(' ', "%20")
        );
        let destination = local_dir.join(&entry.path);
        {
            let mut file_progress = |done_in_file: u64, _total: u64| {
                if let Some(callback) = progress.as_deref_mut() {
                    callback(base_done + done_in_file, total_bytes);
                }
            };
            let mut sink: Option<&mut dyn FnMut(u64, u64)> = Some(&mut file_progress);
            download_file(agent, &url, &destination, entry.size, &mut sink)?;
        }
        base_done += entry.size;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 模型目录发现 / 布局归一化 / 就绪判断 / 下载
// ---------------------------------------------------------------------------

fn directory_contains_all(parent: &Path, required_names: &[&str]) -> bool {
    required_names.iter().all(|name| parent.join(name).exists())
}

fn find_directory_with_required_names(root_dir: &Path, required_names: &[&str]) -> Option<PathBuf> {
    if !root_dir.exists() {
        return None;
    }
    if directory_contains_all(root_dir, required_names) {
        return Some(root_dir.to_path_buf());
    }
    let sentinel_name = required_names[0];
    let mut stack = vec![root_dir.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = std::fs::read_dir(&directory).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if directory_contains_all(&path, required_names) {
                    return Some(path);
                }
                stack.push(path);
            } else if entry.file_name().to_string_lossy() == sentinel_name {
                if let Some(parent) = path.parent() {
                    if directory_contains_all(parent, required_names) {
                        return Some(parent.to_path_buf());
                    }
                }
            }
        }
    }
    None
}

/// 把子目录内容提升到目标目录（HF 快照下载可能多套一层目录）。
fn promote_directory_contents(source_dir: &Path, target_dir: &Path) -> Result<(), String> {
    if resolve_lenient(source_dir) == resolve_lenient(target_dir) {
        return Ok(());
    }
    std::fs::create_dir_all(target_dir).map_err(|error| format!("{error}"))?;
    for entry in std::fs::read_dir(source_dir)
        .map_err(|error| format!("{error}"))?
        .flatten()
    {
        let destination = target_dir.join(entry.file_name());
        if destination.exists() {
            continue;
        }
        let source = entry.path();
        if std::fs::rename(&source, &destination).is_ok() {
            continue;
        }
        if source.is_dir() {
            copy_directory(&source, &destination)?;
        } else {
            std::fs::copy(&source, &destination).map_err(|error| format!("{error}"))?;
        }
        let _ = if source.is_dir() {
            std::fs::remove_dir_all(&source)
        } else {
            std::fs::remove_file(&source)
        };
    }
    Ok(())
}

fn copy_directory(source: &Path, destination: &Path) -> Result<(), String> {
    std::fs::create_dir_all(destination).map_err(|error| format!("{error}"))?;
    for entry in std::fs::read_dir(source)
        .map_err(|error| format!("{error}"))?
        .flatten()
    {
        let child_source = entry.path();
        let child_destination = destination.join(entry.file_name());
        if child_source.is_dir() {
            copy_directory(&child_source, &child_destination)?;
        } else {
            std::fs::copy(&child_source, &child_destination).map_err(|error| format!("{error}"))?;
        }
    }
    Ok(())
}

fn normalize_download_layout(target_dir: &Path, required_names: &[&str]) -> Result<(), String> {
    match find_directory_with_required_names(target_dir, required_names) {
        None => Ok(()),
        Some(candidate_dir) => promote_directory_contents(&candidate_dir, target_dir),
    }
}

/// 找到模型目录中的 manifest 路径。
pub fn find_manifest_path(model_dir: &Path) -> Option<PathBuf> {
    MANIFEST_CANDIDATE_RELATIVE_PATHS
        .iter()
        .find_map(|relative| {
            let candidate = resolve_lenient(&model_dir.join(relative));
            candidate.is_file().then_some(candidate)
        })
}

/// 读取模型 manifest 中的内置音色名（不加载 ONNX session，轻量）。
///
/// 模型缺失时返回空列表。
pub fn builtin_voice_names(model_dir: Option<&Path>) -> Vec<String> {
    let resolved = resolve_model_dir(model_dir.and_then(Path::to_str));
    let Some(manifest_path) = find_manifest_path(&resolved) else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&manifest_path) else {
        return Vec::new();
    };
    let Ok(manifest) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    manifest
        .get("builtin_voices")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.get("voice").and_then(Value::as_str))
                .filter(|voice| !voice.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// 模型目录是否已就绪（存在 manifest）。
pub fn models_ready(model_dir: Option<&Path>) -> bool {
    find_manifest_path(&resolve_model_dir(model_dir.and_then(Path::to_str))).is_some()
}

/// 下载进度回调：`(仓库名, 已下载字节, 总字节)`；总字节为 0 表示大小未知。
pub type ProgressCallback<'a> = Option<&'a mut dyn FnMut(&str, u64, u64)>;

/// 确保模型目录就绪：已有 manifest 直接返回；缺失时自动下载。
///
/// - `model_dir` 为 `None` 时使用默认目录（`~/.omnicrawl/tts/models`），缺失则自动
///   从 Hugging Face 下载两个 ONNX 仓库。
/// - 显式传入的 `model_dir` 缺失时报错，不做自动下载。
/// - `progress(label, done_bytes, total_bytes)` 可选：两个仓库依次下载时按仓库上报
///   进度（label 为仓库名，total 为 0 表示大小未知）。
pub fn ensure_model_dir(
    model_dir: Option<&Path>,
    progress: ProgressCallback<'_>,
) -> Result<PathBuf, String> {
    let resolved = resolve_model_dir(model_dir.and_then(Path::to_str));
    if find_manifest_path(&resolved).is_some() {
        return Ok(resolved);
    }

    if model_dir.is_some() {
        let tried: Vec<String> = MANIFEST_CANDIDATE_RELATIVE_PATHS
            .iter()
            .map(|item| resolve_lenient(&resolved.join(item)).display().to_string())
            .collect();
        return Err(format!(
            "指定的模型目录中未找到 browser_poc_manifest.json。已尝试：{}。可省略 model_dir 使用默认目录自动下载，或手动放置模型。",
            tried.join(", ")
        ));
    }

    eprintln!(
        "模型目录 {} 缺失，从 Hugging Face 自动下载。",
        resolved.display()
    );
    download_models_into(&resolved, progress)
}

/// 把两个 ONNX 仓库下载到指定目录（设置面板的「下载模型」按钮走这里）。
///
/// 与 [`ensure_model_dir`] 的区别：不做就绪探测、也不因目录缺失而报错——
/// 调用方明确要求「下载到这个目录」，所以无条件执行下载与布局归一化。
pub fn download_models_into(
    model_dir: &Path,
    mut progress: ProgressCallback<'_>,
) -> Result<PathBuf, String> {
    let resolved = model_dir.to_path_buf();
    eprintln!("TTS 仓库：{TTS_REPO_ID}");
    eprintln!("Codec 仓库：{CODEC_REPO_ID}");

    let agent = agent();
    let tts_dir = resolved.join("MOSS-TTS-Nano-100M-ONNX");
    let codec_dir = resolved.join("MOSS-Audio-Tokenizer-Nano-ONNX");
    std::fs::create_dir_all(&tts_dir).map_err(|error| format!("{error}"))?;
    std::fs::create_dir_all(&codec_dir).map_err(|error| format!("{error}"))?;

    {
        let mut repo_progress = |done: u64, total: u64| {
            if let Some(callback) = progress.as_deref_mut() {
                callback("TTS 模型（673MB）", done, total);
            }
        };
        download_repo(
            &agent,
            TTS_REPO_ID,
            &tts_dir,
            &TTS_ALLOW_PATTERNS,
            Some(&mut repo_progress),
        )?;
    }
    {
        let mut repo_progress = |done: u64, total: u64| {
            if let Some(callback) = progress.as_deref_mut() {
                callback("Codec 模型（91MB）", done, total);
            }
        };
        download_repo(
            &agent,
            CODEC_REPO_ID,
            &codec_dir,
            &CODEC_ALLOW_PATTERNS,
            Some(&mut repo_progress),
        )?;
    }

    normalize_download_layout(&tts_dir, &TTS_LAYOUT_REQUIRED_NAMES)?;
    normalize_download_layout(&codec_dir, &CODEC_LAYOUT_REQUIRED_NAMES)?;

    if find_manifest_path(&resolved).is_none() {
        return Err(format!(
            "模型已下载但未找到 browser_poc_manifest.json。下载目录：{}",
            resolved.display()
        ));
    }
    Ok(resolved)
}

/// 设置面板下载按钮用到的 manifest 候选路径（供宿主提示用）。
pub const fn manifest_relatives() -> [&'static str; 3] {
    MANIFEST_CANDIDATE_RELATIVE_PATHS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards_match_python_fnmatch_semantics() {
        assert!(matches_allow_patterns("sub/dir/model.onnx", &["*.onnx"]));
        assert!(matches_allow_patterns(
            "tokenizer.model",
            &["tokenizer.model"]
        ));
        assert!(!matches_allow_patterns("tokenizer.model", &["*.onnx"]));
        assert!(fnmatch("a1", "a?"));
        assert!(!fnmatch("a12", "a?"));
        assert!(fnmatch("anything", "*"));
        assert!(fnmatch("moss.data", "*.data"));
    }

    #[test]
    fn ready_probe_uses_manifest_candidates_only() {
        let root = std::env::temp_dir().join("omnicrawl-tts-download-probe");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时目录");
        assert!(!models_ready(Some(&root)));
        assert_eq!(find_manifest_path(&root), None);

        std::fs::write(root.join("browser_poc_manifest.json"), "{}").expect("写 manifest");
        assert!(models_ready(Some(&root)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nested_layout_is_promoted_to_the_model_root() {
        let root = std::env::temp_dir().join("omnicrawl-tts-download-layout");
        let _ = std::fs::remove_dir_all(&root);
        let nested = root.join("MOSS-TTS-Nano-100M-ONNX");
        std::fs::create_dir_all(&nested).expect("创建嵌套目录");
        for name in TTS_LAYOUT_REQUIRED_NAMES {
            std::fs::write(nested.join(name), "x").expect("写文件");
        }
        normalize_download_layout(&root, &TTS_LAYOUT_REQUIRED_NAMES).expect("布局归一化");
        assert!(find_manifest_path(&root).is_some());
        let _ = std::fs::remove_dir_all(&root);
    }
}
