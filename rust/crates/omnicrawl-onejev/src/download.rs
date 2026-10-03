//! 从 Hugging Face 拉取某个尺寸的 OneJev 权重，以及就绪判断与删除。
//!
//! 只依赖 HTTP 客户端与标准库：下载器与「能不能跑起来」（虚拟环境、GPU）解耦，
//! 因此没有 CUDA 的机器也能先把权重取下来。
//!
//! 仓库文件按白名单过滤（`*.json` / `*.jinja` / `*.txt` / `*.safetensors`），落到
//! `<root>/models/<仓库名>/`；下载完成即就地可用，服务启动时用本地目录而不是仓库 id，
//! 于是启动不需要联网。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use serde_json::Value;

use crate::paths;
use crate::sizes::OneJevSize;

const HF_API_BASE: &str = "https://huggingface.co/api/models";
const HF_RESOLVE_BASE: &str = "https://huggingface.co";
const USER_AGENT: &str = "omnicrawl-onejev";

/// 下载重试次数（网络抖动重试）。
const DOWNLOAD_RETRIES: usize = 3;
/// 单个文件的下载超时（秒）；权重文件按 MB 级带宽留足余量。
const READ_TIMEOUT_SECONDS: u64 = 1800;
/// 单文件进度回调的写入块大小。
const CHUNK_BYTES: usize = 1 << 20;

/// 仓库白名单：权重与推理需要的文本文件。
const ALLOW_PATTERNS: [&str; 4] = ["*.safetensors", "*.json", "*.jinja", "*.txt"];

/// 以 `.part` 结尾的临时文件后缀（未下载完成前不落地成正式文件名）。
const PART_SUFFIX: &str = ".part";

/// 下载进度回调：`(已下载字节, 总字节)`；总字节为 0 表示大小未知。
pub type ProgressCallback<'a> = Option<&'a mut dyn FnMut(u64, u64)>;

/// 一次下载的结果：权重目录 + 该尺寸仓库的总字节数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadOutcome {
    pub dir: PathBuf,
    pub total_bytes: u64,
}

/// 该尺寸的权重是否已就绪（配置文件、分词器与至少一个权重分片都在）。
pub fn model_ready(root: &Path, size: &OneJevSize) -> bool {
    let dir = paths::model_dir(root, size.repo_id);
    if !dir.join("config.json").is_file() || !dir.join("tokenizer.json").is_file() {
        return false;
    }
    has_safetensors(&dir)
}

/// 目录里是否有权重分片（27B 是两片，其余单片）。
fn has_safetensors(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name().to_string_lossy().to_string();
        name.ends_with(".safetensors") && !name.ends_with(PART_SUFFIX)
    })
}

/// 把某个尺寸下载到 `<root>/models/<仓库名>/`（设置页「下载」按钮走这里）。
///
/// 已就绪时直接返回（不重复下载）；缺文件时补齐——HF 侧我们只按仓库清单逐个取，
/// 已存在且大小一致的文件跳过，因此中断后重跑不会从头再来。
pub fn download_model(
    env: &ConfigEnvironment,
    root: &Path,
    size: &OneJevSize,
    mut progress: ProgressCallback<'_>,
) -> Result<DownloadOutcome, String> {
    let _ = env;
    let dir = paths::model_dir(root, size.repo_id);
    if model_ready(root, size) {
        return Ok(DownloadOutcome {
            dir,
            total_bytes: size.total_bytes,
        });
    }
    std::fs::create_dir_all(&dir).map_err(|error| format!("创建 {} 失败：{error}", dir.display()))?;

    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let files = list_repo_files(&agent, size.repo_id)?;
    let matched: Vec<RepoFile> = files
        .into_iter()
        .filter(|entry| matches_allow_patterns(&entry.path, &ALLOW_PATTERNS))
        .collect();
    if matched.is_empty() {
        return Err(format!("仓库 {} 里没有可下载的权重文件。", size.repo_id));
    }
    let total: u64 = matched
        .iter()
        .map(|entry| entry.size)
        .sum::<u64>()
        .max(size.total_bytes);
    let mut done_base = 0u64;
    for entry in &matched {
        let destination = dir.join(&entry.path);
        if file_complete(&destination, entry.size) {
            done_base += entry.size;
            if let Some(callback) = progress.as_deref_mut() {
                callback(done_base, total);
            }
            continue;
        }
        let url = format!(
            "{HF_RESOLVE_BASE}/{}/resolve/main/{}",
            size.repo_id,
            entry.path.replace(' ', "%20")
        );
        {
            let mut file_progress = |done_in_file: u64, _file_total: u64| {
                if let Some(callback) = progress.as_deref_mut() {
                    callback(done_base + done_in_file, total);
                }
            };
            let mut sink: Option<&mut dyn FnMut(u64, u64)> = Some(&mut file_progress);
            download_file(&agent, &url, &destination, entry.size, &mut sink)
                .map_err(|error| format!("{} 下载失败：{error}", size.repo_id))?;
        }
        done_base += entry.size;
    }
    // 分片权重会带一个 index 文件，其余布局就是仓库原样：直接把目录当本地模型目录用。
    if !model_ready(root, size) {
        return Err(format!(
            "{} 下载完成但文件不齐（缺 config.json / tokenizer.json / 权重分片）。",
            size.repo_id
        ));
    }
    Ok(DownloadOutcome {
        dir,
        total_bytes: total,
    })
}

/// 删除某尺寸的权重目录；目录不存在时返回 `false`。
pub fn delete_model(root: &Path, size: &OneJevSize) -> Result<bool, String> {
    let dir = paths::model_dir(root, size.repo_id);
    if !dir.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&dir).map_err(|error| format!("删除 {} 失败：{error}", dir.display()))?;
    Ok(true)
}

/// 仓库内一个文件。
#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoFile {
    path: String,
    size: u64,
}

fn list_repo_files(agent: &ureq::Agent, repo_id: &str) -> Result<Vec<RepoFile>, String> {
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
        // 大文件走 LFS，`size` 在顶层缺失时在 `lfs.size` 里。
        let size = row
            .get("size")
            .and_then(Value::as_u64)
            .or_else(|| row.get("lfs").and_then(|lfs| lfs.get("size")).and_then(Value::as_u64))
            .unwrap_or(0);
        files.push(RepoFile {
            path: path.to_string(),
            size,
        });
    }
    Ok(files)
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

/// 文件名是否命中通配模式（语义对齐 Python `fnmatch` 的 `*` / `?`）。
fn matches_allow_patterns(path: &str, allow_patterns: &[&str]) -> bool {
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

/// 已存在且大小一致（大小未知时只判断存在）。
fn file_complete(path: &Path, expected_size: u64) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    expected_size == 0 || metadata.len() == expected_size
}

fn download_file(
    agent: &ureq::Agent,
    url: &str,
    destination: &Path,
    expected_size: u64,
    progress: &mut Option<&mut dyn FnMut(u64, u64)>,
) -> Result<(), String> {
    let mut last_error = String::new();
    for _attempt in 1..=DOWNLOAD_RETRIES {
        match download_file_once(agent, url, destination, expected_size, progress) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = error,
        }
    }
    Err(format!("{url}（{last_error}）"))
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
    let part = destination.with_file_name(format!(
        "{}{PART_SUFFIX}",
        destination
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default()
    ));
    let mut reader = get_reader(agent, url, READ_TIMEOUT_SECONDS)?;
    let mut output = std::fs::File::create(&part)
        .map_err(|error| format!("创建 {} 失败：{error}", part.display()))?;
    let mut buffer = vec![0u8; CHUNK_BYTES];
    let mut received = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("读取响应失败：{error}"))?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .map_err(|error| format!("写入 {} 失败：{error}", part.display()))?;
        received += read as u64;
        if let Some(callback) = progress.as_deref_mut() {
            callback(received, expected_size);
        }
    }
    output.flush().map_err(|error| format!("{error}"))?;
    drop(output);
    if expected_size > 0 && received != expected_size {
        let _ = std::fs::remove_file(&part);
        return Err(format!(
            "下载字节数不符（得到 {received}，期望 {expected_size}）"
        ));
    }
    std::fs::rename(&part, destination).map_err(|error| format!("{error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sizes::ONEJEV_SIZES;

    #[test]
    fn patterns_take_weights_and_text_files_only() {
        assert!(matches_allow_patterns(
            "model.safetensors",
            &ALLOW_PATTERNS
        ));
        assert!(matches_allow_patterns(
            "model-00001-of-00002.safetensors",
            &ALLOW_PATTERNS
        ));
        assert!(matches_allow_patterns("config.json", &ALLOW_PATTERNS));
        assert!(matches_allow_patterns("chat_template.jinja", &ALLOW_PATTERNS));
        assert!(!matches_allow_patterns("assets/banner.svg", &ALLOW_PATTERNS));
        assert!(!matches_allow_patterns("README.md", &ALLOW_PATTERNS));
    }

    #[test]
    fn readiness_needs_config_tokenizer_and_shards() {
        let root = std::env::temp_dir().join(format!("oc-onejev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let size = ONEJEV_SIZES[0];
        let dir = paths::model_dir(&root, size.repo_id);
        std::fs::create_dir_all(&dir).expect("建目录");
        assert!(!model_ready(&root, &size), "空目录不就绪");
        std::fs::write(dir.join("config.json"), "{}").expect("写配置");
        std::fs::write(dir.join("tokenizer.json"), "{}").expect("写分词器");
        assert!(!model_ready(&root, &size), "缺权重分片不就绪");
        std::fs::write(dir.join("model.safetensors"), "x").expect("写权重");
        assert!(model_ready(&root, &size));
        // `.part` 不算权重分片。
        std::fs::remove_file(dir.join("model.safetensors")).expect("删权重");
        std::fs::write(dir.join("model.safetensors.part"), "x").expect("写半成品");
        assert!(!model_ready(&root, &size));
        assert!(delete_model(&root, &size).expect("删除"));
        assert!(!delete_model(&root, &size).expect("再删返回未删除"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn complete_files_are_skipped_by_size() {
        let dir = std::env::temp_dir().join(format!("oc-onejev-f-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录");
        let file = dir.join("config.json");
        std::fs::write(&file, "{}").expect("写文件");
        assert!(file_complete(&file, 2));
        assert!(!file_complete(&file, 99), "大小不符要重下");
        assert!(file_complete(&file, 0), "大小未知时只判断存在");
        assert!(!file_complete(&dir.join("缺文件"), 1));
        let _ = std::fs::remove_dir_all(&dir);
    }
}