//! `omnicrawl/extensions/plugin_install.py` 的 Rust 移植：
//! NPM 插件安装、更新、卸载、回滚与开发模式本地注册。
//!
//! 与 Python 的差异（都源于 Rust 没有 `__file__` 与 `sys.path`）：
//! runner 路径由调用方给出，不再靠包内相对定位；npm/node 的发现沿用 PATH 查找。

use crate::error::PluginInstallError;
use crate::models::{
    is_valid_npm_name, is_valid_semver, parse_plugin_manifest, PluginRecord, PluginVersionRef,
};
use crate::path::{expand_user, resolve_path};
use crate::protocol::{which, PluginWorkerClient, WorkerConfig};
use crate::registry::{
    load_registry_document, project_registry_path, remove_plugin_record, save_registry_document,
    upsert_plugin_record, user_registry_path, user_store_root,
};
use base64::Engine;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256, Sha512};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 安装确认回调：返回 `false` 表示用户取消。
pub type ConfirmCallback = std::sync::Arc<dyn Fn(&str) -> bool + Send + Sync>;

const INTEGRITY_MARKER: &str = ".omnicrawl-integrity";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageSpec {
    pub name: String,
    /// `None` / tag / exact
    pub version: Option<String>,
    pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallResult {
    pub name: String,
    pub version: String,
    pub scope: String,
    pub enabled: bool,
    pub store_path: String,
    pub integrity: String,
    pub diagnostics: Vec<String>,
}

/// 解析 `name` / `name@version` / `name@tag`；不支持 git/http/file。
pub fn parse_package_spec(spec: &str) -> Result<PackageSpec, PluginInstallError> {
    let text = spec.trim().to_string();
    if text.is_empty() {
        return Err(PluginInstallError::new("package-spec 不能为空。"));
    }
    if text.starts_with("git+")
        || text.contains("://")
        || text.starts_with('.')
        || text.starts_with('/')
    {
        return Err(PluginInstallError::new(
            "V1 仅支持 NPM registry 的 name / name@version / name@tag。",
        ));
    }
    let (name, version) = if let Some(after) = text.strip_prefix('@') {
        let Some((scope, rest)) = after.split_once('/') else {
            return Err(PluginInstallError::new(format!(
                "非法 package-spec：{spec}"
            )));
        };
        match rest.split_once('@') {
            Some((package, version)) => (format!("@{scope}/{package}"), Some(version.to_string())),
            None => (format!("@{scope}/{rest}"), None),
        }
    } else {
        if text.matches('@').count() > 1 {
            return Err(PluginInstallError::new(format!(
                "非法 package-spec：{spec}"
            )));
        }
        match text.split_once('@') {
            Some((package, version)) => (package.to_string(), Some(version.to_string())),
            None => (text.clone(), None),
        }
    };
    if !is_valid_npm_name(&name) {
        return Err(PluginInstallError::new(format!("非法 NPM 包名：{name}")));
    }
    Ok(PackageSpec {
        name,
        version,
        raw: text,
    })
}

/// 同时检查 node 与 npm，并要求 Node.js >= 20。
pub fn detect_node_npm() -> Result<(String, String), PluginInstallError> {
    let node = crate::protocol::resolve_node_executable()
        .map_err(|error| PluginInstallError::new(error.to_string()))?;
    let Some(npm) = which("npm.cmd").or_else(|| which("npm")) else {
        return Err(PluginInstallError::new(
            "未找到 npm，请安装 Node.js 20+ 自带的 npm。",
        ));
    };
    let output = Command::new(&node)
        .arg("--version")
        .output()
        .map_err(|error| PluginInstallError::new(format!("无法读取 node 版本：{error}")))?;
    let node_version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if node_major(&node_version) < 20 {
        return Err(PluginInstallError::new(format!(
            "需要 Node.js >= 20，当前：{node_version}"
        )));
    }
    Ok((node, npm))
}

fn node_major(version_text: &str) -> i64 {
    let text = version_text.trim().trim_start_matches('v');
    let major = text.split('.').next().unwrap_or_default();
    major.parse::<i64>().unwrap_or(0)
}

/// `urllib.parse.quote(name, safe="@")`。
pub fn quote_path_segment(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        let keep = byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-' | b'~' | b'@');
        if keep {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

pub fn registry_url_for(name: &str, version: Option<&str>) -> String {
    let encoded = quote_path_segment(name);
    let base = std::env::var("npm_config_registry")
        .or_else(|_| std::env::var("NPM_CONFIG_REGISTRY"))
        .unwrap_or_else(|_| "https://registry.npmjs.org".to_string());
    let base = base.trim_end_matches('/').to_string();
    match version {
        Some(version) => format!("{base}/{encoded}/{}", quote_path_segment(version)),
        None => format!("{base}/{encoded}"),
    }
}

fn fetch_bytes(
    url: &str,
    timeout_seconds: f64,
    accept: &str,
) -> Result<Vec<u8>, PluginInstallError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let timeout = Duration::from_secs_f64(timeout_seconds.max(0.001));
    let response = agent
        .get(url)
        .config()
        .timeout_connect(Some(timeout))
        .timeout_recv_response(Some(timeout))
        .timeout_recv_body(Some(timeout))
        .build()
        .header("Accept", accept)
        .call()
        .map_err(|error| PluginInstallError::new(format!("访问 NPM registry 失败：{error}")))?;
    let status = response.status().as_u16();
    if status >= 400 {
        return Err(PluginInstallError::new(format!(
            "NPM registry HTTP {status}：{url}"
        )));
    }
    let mut buffer: Vec<u8> = Vec::new();
    response
        .into_body()
        .into_reader()
        .read_to_end(&mut buffer)
        .map_err(|error| PluginInstallError::new(format!("访问 NPM registry 失败：{error}")))?;
    Ok(buffer)
}

pub fn fetch_json(
    url: &str,
    timeout_seconds: f64,
) -> Result<Map<String, Value>, PluginInstallError> {
    let bytes = fetch_bytes(url, timeout_seconds, "application/json")?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| PluginInstallError::new(format!("访问 NPM registry 失败：{error}")))?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(PluginInstallError::new("NPM registry 返回值必须是对象。")),
    }
}

/// 解析精确版本与 packument/version metadata。
pub fn resolve_exact_version(
    spec: &PackageSpec,
) -> Result<(String, Map<String, Value>), PluginInstallError> {
    if let Some(version) = spec.version.as_ref().filter(|item| is_valid_semver(item)) {
        let meta = fetch_json(&registry_url_for(&spec.name, Some(version)), 30.0)?;
        return Ok((version.clone(), meta));
    }

    let packument = fetch_json(&registry_url_for(&spec.name, None), 30.0)?;
    let dist_tags = packument
        .get("dist-tags")
        .and_then(Value::as_object)
        .cloned();
    let versions = packument
        .get("versions")
        .and_then(Value::as_object)
        .cloned();
    let (Some(dist_tags), Some(versions)) = (dist_tags, versions) else {
        return Err(PluginInstallError::new(format!(
            "NPM packument 结构无效：{}",
            spec.name
        )));
    };

    let tag = spec.version.clone().unwrap_or_else(|| "latest".to_string());
    let exact = match dist_tags
        .get(&tag)
        .map(|item| match item {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .filter(|item| !item.is_empty())
    {
        Some(value) => value,
        None => {
            // 允许直接把 tag 字段当成版本键。
            if versions.contains_key(&tag) {
                tag.clone()
            } else {
                return Err(PluginInstallError::new(format!(
                    "无法解析 {}@{tag} 的精确版本。",
                    spec.name
                )));
            }
        }
    };
    let version_meta = match versions.get(&exact) {
        Some(Value::Object(map)) => map.clone(),
        _ => fetch_json(&registry_url_for(&spec.name, Some(&exact)), 30.0)?,
    };
    Ok((exact, version_meta))
}

fn sha512_integrity(data: &[u8]) -> String {
    let digest = Sha512::digest(data);
    format!(
        "sha512-{}",
        base64::engine::general_purpose::STANDARD.encode(digest)
    )
}

fn sha256_file(path: &Path) -> Result<String, PluginInstallError> {
    let mut hasher = Sha256::new();
    let mut handle =
        std::fs::File::open(path).map_err(|error| PluginInstallError::new(error.to_string()))?;
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = handle
            .read(&mut buffer)
            .map_err(|error| PluginInstallError::new(error.to_string()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("sha256-{}", crate::models::hex(&hasher.finalize())))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TreeNodeKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// 递归收集条目，等价于 `sorted(root.rglob("*"), key=as_posix)`。
fn collect_tree(root: &Path, prefix: &str, entries: &mut Vec<(String, PathBuf, TreeNodeKind)>) {
    let Ok(read_dir) = std::fs::read_dir(root) else {
        return;
    };
    for entry in read_dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let relative = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let path = entry.path();
        let kind = match entry.file_type() {
            Ok(file_type) if file_type.is_symlink() => TreeNodeKind::Symlink,
            Ok(file_type) if file_type.is_dir() => TreeNodeKind::Dir,
            Ok(file_type) if file_type.is_file() => TreeNodeKind::File,
            _ => TreeNodeKind::Other,
        };
        entries.push((relative.clone(), path.clone(), kind));
        if kind == TreeNodeKind::Dir {
            collect_tree(&path, &relative, entries);
        }
    }
}

/// 计算插件文件树的确定性哈希，排除 Host 自己写入的完整性标记。
fn content_tree_hash(root: &Path) -> Result<String, PluginInstallError> {
    let mut entries: Vec<(String, PathBuf, TreeNodeKind)> = Vec::new();
    collect_tree(root, "", &mut entries);
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    let mut hasher = Sha256::new();
    for (relative, path, kind) in entries {
        if path
            .file_name()
            .map(|item| item == INTEGRITY_MARKER)
            .unwrap_or(false)
        {
            continue;
        }
        if kind == TreeNodeKind::Symlink {
            return Err(PluginInstallError::new(format!(
                "插件 store 不允许符号链接：{relative}"
            )));
        }
        if kind == TreeNodeKind::Dir {
            continue;
        }
        if kind != TreeNodeKind::File {
            return Err(PluginInstallError::new(format!(
                "插件 store 包含特殊文件：{relative}"
            )));
        }
        hasher.update(relative.as_bytes());
        hasher.update([0u8]);
        let mut handle = std::fs::File::open(&path)
            .map_err(|error| PluginInstallError::new(error.to_string()))?;
        let mut buffer = vec![0u8; 1024 * 1024];
        loop {
            let read = handle
                .read(&mut buffer)
                .map_err(|error| PluginInstallError::new(error.to_string()))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        hasher.update([0u8]);
    }
    Ok(format!("sha256-{}", crate::models::hex(&hasher.finalize())))
}

fn escape_package_dir(name: &str) -> String {
    name.replace('/', "__").replace('@', "")
}

pub fn download_tarball(url: &str, dest: &Path) -> Result<Vec<u8>, PluginInstallError> {
    let data = fetch_bytes(url, 60.0, "application/octet-stream")
        .map_err(|error| PluginInstallError::new(format!("下载 tarball 失败：{error}")))?;
    std::fs::write(dest, &data)
        .map_err(|error| PluginInstallError::new(format!("下载 tarball 失败：{error}")))?;
    Ok(data)
}

/// 解压 `.tgz`：先整包校验成员类型与路径，再落盘（V1 插件包只允许普通文件与目录）。
pub fn extract_tarball(tarball: &Path, dest_dir: &Path) -> Result<PathBuf, PluginInstallError> {
    let resolved_dest = resolve_path(dest_dir);
    std::fs::create_dir_all(dest_dir)
        .map_err(|error| PluginInstallError::new(error.to_string()))?;

    let file =
        std::fs::File::open(tarball).map_err(|error| PluginInstallError::new(error.to_string()))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let entries = archive
        .entries()
        .map_err(|error| PluginInstallError::new(error.to_string()))?;
    for entry in entries {
        let entry = entry.map_err(|error| PluginInstallError::new(error.to_string()))?;
        let path = entry
            .path()
            .map_err(|error| PluginInstallError::new(error.to_string()))?
            .to_path_buf();
        let display = path.to_string_lossy().to_string();
        let normalized = display.replace('\\', "/");
        let member_path = Path::new(&normalized);
        let target = resolve_path(&resolved_dest.join(member_path));
        let escapes = member_path.is_absolute()
            || member_path
                .components()
                .any(|item| item.as_os_str() == "..")
            || !target.starts_with(&resolved_dest);
        if escapes {
            return Err(PluginInstallError::new(format!(
                "tarball 包含不安全路径：{display}"
            )));
        }
        let entry_type = entry.header().entry_type();
        // V1 插件包只需要普通文件和目录，拒绝链接、设备、FIFO 等特殊 member。
        if !(entry_type.is_dir() || entry_type.is_file()) {
            return Err(PluginInstallError::new(format!(
                "tarball 包含不安全特殊文件：{display}"
            )));
        }
    }

    let file =
        std::fs::File::open(tarball).map_err(|error| PluginInstallError::new(error.to_string()))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let entries = archive
        .entries()
        .map_err(|error| PluginInstallError::new(error.to_string()))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| PluginInstallError::new(error.to_string()))?;
        let path = entry
            .path()
            .map_err(|error| PluginInstallError::new(error.to_string()))?
            .to_path_buf();
        let target = dest_dir.join(&path);
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|error| PluginInstallError::new(error.to_string()))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| PluginInstallError::new(error.to_string()))?;
        }
        let mut out = std::fs::File::create(&target)
            .map_err(|error| PluginInstallError::new(error.to_string()))?;
        std::io::copy(&mut entry, &mut out)
            .map_err(|error| PluginInstallError::new(error.to_string()))?;
    }

    // npm pack 通常顶层是 package/。
    let package_dir = dest_dir.join("package");
    if package_dir.is_dir() {
        return Ok(package_dir);
    }
    let mut children: Vec<PathBuf> = std::fs::read_dir(dest_dir)
        .map_err(|error| PluginInstallError::new(error.to_string()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    children.sort();
    if children.len() == 1 {
        return Ok(children.remove(0));
    }
    Err(PluginInstallError::new(
        "无法定位 tarball 解压后的包根目录。",
    ))
}

/// 判断是否需要 `npm install`（无 dependencies 时可离线跳过）。
fn package_has_production_deps(package_dir: &Path) -> bool {
    let package_json = package_dir.join("package.json");
    if !package_json.is_file() {
        return false;
    }
    let Ok(bytes) = std::fs::read(&package_json) else {
        return true;
    };
    let Ok(value) = serde_json::from_str::<Value>(&crate::models::decode_utf8_sig(&bytes)) else {
        return true;
    };
    value
        .get("dependencies")
        .and_then(Value::as_object)
        .map(|map| !map.is_empty())
        .unwrap_or(false)
}

/// 在包目录安装 production 依赖，强制 `--ignore-scripts`。
pub fn npm_install_production(
    package_dir: &Path,
    npm: &str,
    allow_offline_skip: bool,
) -> Result<PathBuf, PluginInstallError> {
    let lock_path = package_dir.join("package-lock.json");
    if allow_offline_skip && !package_has_production_deps(package_dir) {
        if !lock_path.is_file() {
            let mut packages = Map::new();
            packages.insert(String::new(), Value::Object(Map::new()));
            let mut payload = Map::new();
            payload.insert("name".to_string(), Value::from("omnicrawl-plugin"));
            payload.insert("lockfileVersion".to_string(), Value::from(3));
            payload.insert("requires".to_string(), Value::from(true));
            payload.insert("packages".to_string(), Value::Object(packages));
            let text = format!(
                "{}\n",
                serde_json::to_string_pretty(&Value::Object(payload)).unwrap_or_default()
            );
            std::fs::write(&lock_path, text)
                .map_err(|error| PluginInstallError::new(error.to_string()))?;
        }
        return Ok(lock_path);
    }

    let output = Command::new(npm)
        .args([
            "install",
            "--ignore-scripts",
            "--omit=dev",
            "--no-audit",
            "--no-fund",
            "--package-lock-only=false",
        ])
        .current_dir(package_dir)
        .output()
        .map_err(|error| PluginInstallError::new(format!("npm install 失败：{error}")))?;
    if !output.status.success() {
        let code = output.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let text = if stderr.is_empty() { stdout } else { stderr };
        let clipped: String = text.chars().take(1000).collect();
        return Err(PluginInstallError::new(format!(
            "npm install 退出码 {code}：{clipped}"
        )));
    }
    if !lock_path.is_file() {
        // npm 可能未生成 lock；写一个最小占位并基于 node_modules 扫描不在 V1 强依赖。
        std::fs::write(&lock_path, "{}\n")
            .map_err(|error| PluginInstallError::new(error.to_string()))?;
    }
    Ok(lock_path)
}

/// 从 `package-lock.json` 提取依赖 integrity 摘要。
pub fn collect_lockfile_integrity_report(lock_path: &Path) -> Map<String, Value> {
    let mut report = Map::new();
    let lockfile_hash = if lock_path.is_file() {
        sha256_file(lock_path).unwrap_or_default()
    } else {
        String::new()
    };
    let mut missing: Vec<String> = Vec::new();
    let mut counted: i64 = 0;

    if lock_path.is_file() {
        match std::fs::read(lock_path).ok().and_then(|bytes| {
            serde_json::from_str::<Value>(&crate::models::decode_utf8_sig(&bytes)).ok()
        }) {
            Some(Value::Object(data)) => {
                if let Some(Value::Object(packages)) = data.get("packages") {
                    for (pkg_path, meta) in packages {
                        if pkg_path.is_empty() {
                            continue;
                        }
                        let Some(meta) = meta.as_object() else {
                            continue;
                        };
                        let has_resolved = meta
                            .get("resolved")
                            .map(|item| !item.is_null())
                            .unwrap_or(false);
                        let has_version = meta
                            .get("version")
                            .map(|item| !item.is_null())
                            .unwrap_or(false);
                        if !has_resolved && !has_version {
                            continue;
                        }
                        counted += 1;
                        let integrity = meta
                            .get("integrity")
                            .map(|item| match item {
                                Value::String(text) => text.trim().to_string(),
                                other => other.to_string(),
                            })
                            .unwrap_or_default();
                        if integrity.is_empty() && missing.len() < 20 {
                            missing.push(pkg_path.clone());
                        }
                    }
                } else {
                    // lockfileVersion 1 风格 dependencies 树。
                    walk_dependencies(&data, "", &mut counted, &mut missing);
                }
            }
            Some(_) => {
                missing.push("<unreadable-lockfile>".to_string());
            }
            None => {
                missing.push("<unreadable-lockfile>".to_string());
            }
        }
    }

    report.insert("packages".to_string(), Value::from(counted));
    report.insert(
        "missingIntegrity".to_string(),
        Value::Array(missing.into_iter().map(Value::from).collect()),
    );
    report.insert("lockfileHash".to_string(), Value::from(lockfile_hash));
    report
}

fn walk_dependencies(
    node: &Map<String, Value>,
    prefix: &str,
    counted: &mut i64,
    missing: &mut Vec<String>,
) {
    let Some(Value::Object(deps)) = node.get("dependencies") else {
        return;
    };
    for (name, meta) in deps {
        let Some(meta) = meta.as_object() else {
            continue;
        };
        *counted += 1;
        let path = format!("{prefix}{name}");
        let integrity = meta
            .get("integrity")
            .map(|item| match item {
                Value::String(text) => text.trim().to_string(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        if integrity.is_empty() && missing.len() < 20 {
            missing.push(path.clone());
        }
        walk_dependencies(meta, &format!("{path}>"), counted, missing);
    }
}

/// 校验 store 内的 package.json 与完整性标记。
pub fn verify_store_integrity(
    store_path: &Path,
    expected_integrity: &str,
) -> Result<(), PluginInstallError> {
    if !store_path.is_dir() {
        return Err(PluginInstallError::new(format!(
            "store 路径不存在：{}",
            store_path.display()
        )));
    }
    if !store_path.join("package.json").is_file() {
        return Err(PluginInstallError::new(format!(
            "store 缺少 package.json：{}",
            store_path.display()
        )));
    }
    let marker = store_path.join(INTEGRITY_MARKER);
    if !expected_integrity.is_empty() && marker.is_file() {
        let actual = std::fs::read_to_string(&marker)
            .unwrap_or_default()
            .trim()
            .to_string();
        if !actual.is_empty() && actual != expected_integrity {
            return Err(PluginInstallError::new(format!(
                "store integrity 标记不匹配：期望 {}... 实际 {}...",
                clip(expected_integrity, 32),
                clip(&actual, 32)
            )));
        }
    }
    Ok(())
}

pub fn verify_content_tree_hash(
    store_path: &Path,
    expected_content_hash: &str,
) -> Result<(), PluginInstallError> {
    if expected_content_hash.is_empty() {
        return Ok(());
    }
    let actual = content_tree_hash(store_path)?;
    if actual != expected_content_hash {
        return Err(PluginInstallError::new(format!(
            "插件内容哈希不匹配：期望 {}... 实际 {}...",
            clip(expected_content_hash, 20),
            clip(&actual, 20)
        )));
    }
    Ok(())
}

/// 校验 store 内 `package-lock.json` 与注册表记录的 lockfileHash 一致。
pub fn verify_lockfile_hash(
    store_path: &Path,
    expected_lockfile_hash: &str,
) -> Result<(), PluginInstallError> {
    if expected_lockfile_hash.is_empty() {
        return Ok(());
    }
    let lock_path = store_path.join("package-lock.json");
    if !lock_path.is_file() {
        return Err(PluginInstallError::new(format!(
            "store 缺少 package-lock.json：{}",
            store_path.display()
        )));
    }
    let actual = sha256_file(&lock_path)?;
    if actual != expected_lockfile_hash {
        return Err(PluginInstallError::new(format!(
            "lockfileHash 不匹配：期望 {}... 实际 {}...",
            clip(expected_lockfile_hash, 16),
            clip(&actual, 16)
        )));
    }
    Ok(())
}

fn clip(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// 临时目录：创建即用，Drop 时尽力清理。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn create(prefix: &str) -> Result<Self, PluginInstallError> {
        let path = std::env::temp_dir().join(format!("{prefix}{}", random_hex(8)));
        std::fs::create_dir_all(&path)
            .map_err(|error| PluginInstallError::new(error.to_string()))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// `secrets.token_hex(nbytes)` 的同形写法。
fn random_hex(bytes: usize) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut seed = Vec::new();
    if let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) {
        seed.extend_from_slice(&now.as_nanos().to_le_bytes());
    }
    seed.extend_from_slice(&std::process::id().to_le_bytes());
    seed.extend_from_slice(&COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    let digest = Sha256::digest(&seed);
    crate::models::hex(&digest[..bytes])
}

fn copy_tree(src: &Path, dest: &Path) -> Result<(), PluginInstallError> {
    std::fs::create_dir_all(dest).map_err(|error| PluginInstallError::new(error.to_string()))?;
    for entry in std::fs::read_dir(src)
        .map_err(|error| PluginInstallError::new(error.to_string()))?
        .flatten()
    {
        let target = dest.join(entry.file_name());
        let path = entry.path();
        if path.is_dir() {
            copy_tree(&path, &target)?;
        } else {
            std::fs::copy(&path, &target)
                .map_err(|error| PluginInstallError::new(error.to_string()))?;
        }
    }
    Ok(())
}

/// Worker 握手冒烟测试。
pub fn smoke_test_worker(
    plugin_root: &Path,
    plugin_name: &str,
    runner_path: &Path,
) -> Result<(), PluginInstallError> {
    let client = PluginWorkerClient::new(WorkerConfig {
        plugin_root: plugin_root.to_path_buf(),
        plugin_name: plugin_name.to_string(),
        timeout_ms: Some(5000),
        max_message_bytes: None,
        node_executable: None,
        runner_path: Some(runner_path.to_path_buf()),
        env: None,
        on_stderr: None,
        on_host_request: None,
    })
    .map_err(|error| PluginInstallError::new(format!("Worker 握手冒烟失败：{error}")))?;

    let handshake = (|| -> Result<(), crate::error::PluginProtocolError> {
        client.start()?;
        let mut params = Map::new();
        params.insert(
            "apiVersion".to_string(),
            Value::from(crate::models::HOOK_API_VERSION),
        );
        params.insert(
            "omnicrawlVersion".to_string(),
            Value::from(crate::models::OMNICRAWL_VERSION),
        );
        params.insert("permissions".to_string(), Value::Array(Vec::new()));
        client.initialize(&params, Some(5000))?;
        client.shutdown(2000);
        Ok(())
    })();

    if let Err(error) = handshake {
        client.close();
        return Err(PluginInstallError::new(format!(
            "Worker 握手冒烟失败：{error}"
        )));
    }
    Ok(())
}

fn load_scope_registry(
    scope: &str,
    workspace_root: Option<&Path>,
) -> Result<(PathBuf, crate::models::PluginRegistryDocument), PluginInstallError> {
    let path = if scope == "project" {
        let Some(workspace_root) = workspace_root else {
            return Err(PluginInstallError::new("project scope 需要工作区路径。"));
        };
        project_registry_path(workspace_root)
    } else {
        user_registry_path()
    };
    let document = load_registry_document(&path)
        .map_err(|error| PluginInstallError::new(error.to_string()))?;
    Ok((path, document))
}

fn ensure_scope(scope: &str) -> Result<(), PluginInstallError> {
    if scope != "user" && scope != "project" {
        return Err(PluginInstallError::new(format!("非法 scope：{scope}")));
    }
    Ok(())
}

/// 离线/可测安装：把本地插件目录复制进 store，走冒烟与 registry。
#[allow(clippy::too_many_arguments)]
pub fn install_from_local_package(
    local_path: &Path,
    runner_path: &Path,
    scope: &str,
    workspace_root: Option<&Path>,
    enable: bool,
    yes: bool,
    confirm: Option<ConfirmCallback>,
    store_root: Option<&Path>,
) -> Result<InstallResult, PluginInstallError> {
    ensure_scope(scope)?;
    let src = resolve_path(&expand_user(&local_path.to_string_lossy()));
    if !src.is_dir() {
        return Err(PluginInstallError::new(format!(
            "本地插件目录不存在：{}",
            src.display()
        )));
    }
    let package_json = src.join("package.json");
    if !package_json.is_file() {
        return Err(PluginInstallError::new(format!(
            "缺少 package.json：{}",
            package_json.display()
        )));
    }

    let (_, npm) = detect_node_npm()?;
    let mut diagnostics: Vec<String> = vec!["offline local package install".to_string()];

    let tmp = TempDir::create("omnicrawl-plugin-local-")?;
    let package_dir = tmp.path().join("package");
    copy_tree(&src, &package_dir)?;
    let marker_file = package_dir.join(INTEGRITY_MARKER);
    if marker_file.exists() {
        let _ = std::fs::remove_file(&marker_file);
    }

    let mut hasher = Sha512::new();
    hasher.update(
        std::fs::read(&package_json).map_err(|error| PluginInstallError::new(error.to_string()))?,
    );
    hasher.update(src.to_string_lossy().as_bytes());
    let integrity = format!(
        "sha512-{}",
        base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
    );

    let package_bytes = std::fs::read(package_dir.join("package.json"))
        .map_err(|error| PluginInstallError::new(error.to_string()))?;
    let parsed: Value = serde_json::from_str(&crate::models::decode_utf8_sig(&package_bytes))
        .map_err(|error| PluginInstallError::new(error.to_string()))?;
    let manifest =
        parse_plugin_manifest(&parsed, &package_dir.join("package.json").to_string_lossy())
            .map_err(|error| PluginInstallError::new(error.to_string()))?;
    let exact_version = manifest.version.clone();
    let hooks: Vec<String> = manifest
        .hooks
        .iter()
        .map(|item| item.hook.clone())
        .collect();
    let summary = format!(
        "将安装插件 {}@{exact_version}\n  source: local:{}\n  integrity: {}...\n  hooks: {}\n  permissions: {}\n  scope: {scope}\n\n",
        manifest.name,
        src.display(),
        clip(&integrity, 40),
        hooks.join(", "),
        manifest.permissions.join(", ")
    );
    if !yes {
        let ok = match &confirm {
            Some(callback) => callback(&summary),
            None => true,
        };
        if !ok {
            return Err(PluginInstallError::new("用户取消安装。"));
        }
    }

    let lock_path = npm_install_production(&package_dir, &npm, true)?;
    let lockfile_hash = sha256_file(&lock_path)?;
    let content_hash = content_tree_hash(&package_dir)?;
    let integrity_report = collect_lockfile_integrity_report(&lock_path);
    let missing = missing_integrity_list(&integrity_report);
    if !missing.is_empty() {
        diagnostics.push(format!(
            "package-lock 中有 {} 个依赖缺少 integrity：{}",
            missing.len(),
            missing
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<String>>()
                .join(", ")
        ));
    }
    diagnostics.push(format!(
        "lock 依赖条目 {} 个，lockfileHash={}",
        integrity_report
            .get("packages")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        clip(&lockfile_hash, 12)
    ));

    let root = match store_root {
        Some(path) => path.to_path_buf(),
        None => user_store_root(),
    };
    std::fs::create_dir_all(&root).map_err(|error| PluginInstallError::new(error.to_string()))?;
    let integrity_prefix = {
        let text = integrity
            .split_once('-')
            .map(|(_, rest)| rest)
            .unwrap_or(integrity.as_str());
        let clipped = clip(text, 12);
        if clipped.is_empty() {
            "local".to_string()
        } else {
            clipped
        }
    };
    let target = root
        .join(escape_package_dir(&manifest.name))
        .join(format!("{exact_version}-{integrity_prefix}"));
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| PluginInstallError::new(error.to_string()))?;
    }
    if target.exists() {
        // 内容寻址目标不可原地覆盖：一致时幂等复用，不一致时拒绝。
        verify_store_integrity(&target, &integrity)?;
        verify_lockfile_hash(&target, &lockfile_hash)?;
        verify_content_tree_hash(&target, &content_hash)?;
        smoke_test_worker(&target, &manifest.name, runner_path)?;
    } else {
        let staging = target.parent().unwrap_or(&root).join(format!(
            ".{}.staging",
            target.file_name().unwrap_or_default().to_string_lossy()
        ));
        copy_tree(&package_dir, &staging)?;
        std::fs::write(staging.join(INTEGRITY_MARKER), format!("{integrity}\n"))
            .map_err(|error| PluginInstallError::new(error.to_string()))?;
        let result = (|| -> Result<(), PluginInstallError> {
            smoke_test_worker(&staging, &manifest.name, runner_path)?;
            std::fs::rename(&staging, &target)
                .map_err(|error| PluginInstallError::new(error.to_string()))?;
            Ok(())
        })();
        if staging.exists() {
            let _ = std::fs::remove_dir_all(&staging);
        }
        result?;
    }

    let (registry_path, mut document) = load_scope_registry(scope, workspace_root)?;
    let existing = document.get(&manifest.name).cloned();
    let version_ref = PluginVersionRef {
        version: exact_version.clone(),
        integrity: integrity.clone(),
        lockfile_hash: lockfile_hash.clone(),
        source: format!("local:{}", src.display()),
        store_path: target.to_string_lossy().to_string(),
        content_hash: content_hash.clone(),
    };
    let record = match &existing {
        None => PluginRecord {
            name: manifest.name.clone(),
            enabled: enable,
            active: if enable {
                Some(version_ref.clone())
            } else {
                None
            },
            candidate: if enable {
                None
            } else {
                Some(version_ref.clone())
            },
            previous: None,
            approved_permissions: manifest.permissions.clone(),
            local_path: String::new(),
            dev_mode: false,
        },
        Some(existing) => {
            let mut approved = existing.approved_permissions.clone();
            for item in &manifest.permissions {
                if !approved.contains(item) {
                    approved.push(item.clone());
                }
            }
            approved.sort();
            if enable || existing.enabled {
                PluginRecord {
                    name: manifest.name.clone(),
                    enabled: if enable { true } else { existing.enabled },
                    active: Some(version_ref.clone()),
                    candidate: None,
                    previous: existing.active.clone(),
                    approved_permissions: approved,
                    local_path: String::new(),
                    dev_mode: false,
                }
            } else {
                PluginRecord {
                    name: manifest.name.clone(),
                    enabled: false,
                    active: existing.active.clone(),
                    candidate: Some(version_ref.clone()),
                    previous: existing.previous.clone(),
                    approved_permissions: approved,
                    local_path: String::new(),
                    dev_mode: false,
                }
            }
        }
    };
    upsert_plugin_record(&mut document, record);
    save_registry_document(&registry_path, &document)
        .map_err(|error| PluginInstallError::new(error.to_string()))?;

    Ok(InstallResult {
        name: manifest.name.clone(),
        version: exact_version,
        scope: scope.to_string(),
        enabled: enable || existing.map(|item| item.enabled).unwrap_or(false),
        store_path: target.to_string_lossy().to_string(),
        integrity,
        diagnostics,
    })
}

fn missing_integrity_list(report: &Map<String, Value>) -> Vec<String> {
    report
        .get("missingIntegrity")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// 从 NPM registry 安装。
#[allow(clippy::too_many_arguments)]
pub fn install_from_npm(
    package_spec: &str,
    runner_path: &Path,
    scope: &str,
    workspace_root: Option<&Path>,
    enable: bool,
    yes: bool,
    confirm: Option<ConfirmCallback>,
    allow_network: bool,
) -> Result<InstallResult, PluginInstallError> {
    ensure_scope(scope)?;
    if !allow_network {
        return Err(PluginInstallError::new(
            "当前配置不允许网络安装（plugins.allow_network_install=false）。",
        ));
    }

    let (_, npm) = detect_node_npm()?;
    let spec = parse_package_spec(package_spec)?;
    let (exact_version, version_meta) = resolve_exact_version(&spec)?;
    let dist = version_meta
        .get("dist")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| PluginInstallError::new("版本 metadata 缺少 dist。"))?;
    let tarball_url = dist
        .get("tarball")
        .map(|item| match item {
            Value::String(text) => text.trim().to_string(),
            _ => String::new(),
        })
        .unwrap_or_default();
    let mut integrity = dist
        .get("integrity")
        .map(|item| match item {
            Value::String(text) => text.trim().to_string(),
            _ => String::new(),
        })
        .unwrap_or_default();
    if tarball_url.is_empty() {
        return Err(PluginInstallError::new("版本 metadata 缺少 tarball URL。"));
    }

    let mut diagnostics: Vec<String> = Vec::new();
    let store_root = user_store_root();
    std::fs::create_dir_all(&store_root)
        .map_err(|error| PluginInstallError::new(error.to_string()))?;

    let tmp = TempDir::create("omnicrawl-plugin-")?;
    let tarball_path = tmp.path().join("package.tgz");
    let data = download_tarball(&tarball_url, &tarball_path)?;
    let actual_integrity = sha512_integrity(&data);
    if !integrity.is_empty() && actual_integrity != integrity {
        return Err(PluginInstallError::new(
            "tarball integrity 与 registry 不一致。",
        ));
    }
    if integrity.is_empty() {
        integrity = actual_integrity;
        diagnostics.push("registry 未提供 integrity，已使用本地 sha512。".to_string());
    }

    let extract_root = tmp.path().join("extract");
    let package_dir = extract_tarball(&tarball_path, &extract_root)?;
    let package_bytes = std::fs::read(package_dir.join("package.json"))
        .map_err(|error| PluginInstallError::new(error.to_string()))?;
    let parsed: Value = serde_json::from_str(&crate::models::decode_utf8_sig(&package_bytes))
        .map_err(|error| PluginInstallError::new(error.to_string()))?;
    let manifest =
        parse_plugin_manifest(&parsed, &package_dir.join("package.json").to_string_lossy())
            .map_err(|error| PluginInstallError::new(error.to_string()))?;
    if manifest.name != spec.name {
        return Err(PluginInstallError::new(format!(
            "包名不匹配：期望 {}，实际 {}",
            spec.name, manifest.name
        )));
    }

    let hooks: Vec<String> = manifest
        .hooks
        .iter()
        .map(|item| item.hook.clone())
        .collect();
    let summary = format!(
        "将安装插件 {}@{exact_version}\n  source: {tarball_url}\n  integrity: {}...\n  hooks: {}\n  permissions: {}\n  scope: {scope}\n",
        manifest.name,
        clip(&integrity, 40),
        hooks.join(", "),
        manifest.permissions.join(", ")
    );
    if !yes {
        let ok = match &confirm {
            Some(callback) => callback(&summary),
            None => true,
        };
        if !ok {
            return Err(PluginInstallError::new("用户取消安装。"));
        }
    }

    let lock_path = npm_install_production(&package_dir, &npm, true)?;
    let lockfile_hash = sha256_file(&lock_path)?;
    let content_hash = content_tree_hash(&package_dir)?;
    let integrity_report = collect_lockfile_integrity_report(&lock_path);
    let missing = missing_integrity_list(&integrity_report);
    if !missing.is_empty() {
        diagnostics.push(format!(
            "package-lock 中有 {} 个依赖缺少 integrity：{}",
            missing.len(),
            missing
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<String>>()
                .join(", ")
        ));
    }
    diagnostics.push(format!(
        "lock 依赖条目 {} 个，lockfileHash={}",
        integrity_report
            .get("packages")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        clip(&lockfile_hash, 12)
    ));

    let integrity_prefix = {
        let text = integrity
            .split_once('-')
            .map(|(_, rest)| rest)
            .unwrap_or(integrity.as_str());
        clip(text, 12)
    };
    let target = store_root
        .join(escape_package_dir(&manifest.name))
        .join(format!("{exact_version}-{integrity_prefix}"));
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| PluginInstallError::new(error.to_string()))?;
    }
    let existing = load_scope_registry(scope, workspace_root)?
        .1
        .get(&manifest.name)
        .cloned();
    if target.exists() {
        verify_store_integrity(&target, &integrity)?;
        verify_lockfile_hash(&target, &lockfile_hash)?;
        verify_content_tree_hash(&target, &content_hash)?;
        smoke_test_worker(&target, &manifest.name, runner_path)?;
    } else {
        let staging = target.parent().unwrap_or(&store_root).join(format!(
            ".{}.staging",
            target.file_name().unwrap_or_default().to_string_lossy()
        ));
        if staging.exists() {
            let _ = std::fs::remove_dir_all(&staging);
        }
        std::fs::rename(&package_dir, &staging)
            .map_err(|error| PluginInstallError::new(error.to_string()))?;
        std::fs::write(staging.join(INTEGRITY_MARKER), format!("{integrity}\n"))
            .map_err(|error| PluginInstallError::new(error.to_string()))?;
        let result = (|| -> Result<(), PluginInstallError> {
            smoke_test_worker(&staging, &manifest.name, runner_path)?;
            std::fs::rename(&staging, &target)
                .map_err(|error| PluginInstallError::new(error.to_string()))?;
            Ok(())
        })();
        if staging.exists() {
            let _ = std::fs::remove_dir_all(&staging);
        }
        result?;
    }

    let (registry_path, mut document) = load_scope_registry(scope, workspace_root)?;
    let source_host = host_of(&tarball_url);
    let version_ref = PluginVersionRef {
        version: exact_version.clone(),
        integrity: integrity.clone(),
        lockfile_hash: lockfile_hash.clone(),
        source: source_host,
        store_path: target.to_string_lossy().to_string(),
        content_hash: content_hash.clone(),
    };
    let record = match &existing {
        None => PluginRecord {
            name: manifest.name.clone(),
            enabled: enable,
            active: if enable {
                Some(version_ref.clone())
            } else {
                None
            },
            candidate: if enable {
                None
            } else {
                Some(version_ref.clone())
            },
            previous: None,
            approved_permissions: manifest.permissions.clone(),
            local_path: String::new(),
            dev_mode: false,
        },
        Some(existing) => {
            // 权限增量必须单独确认；--yes 只跳过普通安装确认，不能批准新权限。
            let new_permissions: Vec<String> = manifest
                .permissions
                .iter()
                .filter(|item| !existing.approved_permissions.contains(item))
                .cloned()
                .collect();
            if !new_permissions.is_empty() {
                let mut sorted = new_permissions.clone();
                sorted.sort();
                let message = format!(
                    "插件 {} 新增权限：{}。是否批准？",
                    manifest.name,
                    sorted.join(", ")
                );
                let Some(callback) = &confirm else {
                    return Err(PluginInstallError::new(
                        "检测到新增权限；必须交互确认或提供独立的预批准权限策略。",
                    ));
                };
                if !callback(&message) {
                    return Err(PluginInstallError::new("用户拒绝新增权限。"));
                }
            }
            let mut approved = existing.approved_permissions.clone();
            for item in &manifest.permissions {
                if !approved.contains(item) {
                    approved.push(item.clone());
                }
            }
            approved.sort();
            if enable || existing.enabled {
                PluginRecord {
                    name: manifest.name.clone(),
                    enabled: if enable { true } else { existing.enabled },
                    active: Some(version_ref.clone()),
                    candidate: None,
                    previous: existing.active.clone(),
                    approved_permissions: approved,
                    local_path: existing.local_path.clone(),
                    dev_mode: false,
                }
            } else {
                PluginRecord {
                    name: manifest.name.clone(),
                    enabled: false,
                    active: existing.active.clone(),
                    candidate: Some(version_ref.clone()),
                    previous: existing.previous.clone(),
                    approved_permissions: approved,
                    local_path: existing.local_path.clone(),
                    dev_mode: false,
                }
            }
        }
    };
    upsert_plugin_record(&mut document, record);
    save_registry_document(&registry_path, &document)
        .map_err(|error| PluginInstallError::new(error.to_string()))?;

    Ok(InstallResult {
        name: manifest.name.clone(),
        version: exact_version,
        scope: scope.to_string(),
        enabled: enable || existing.map(|item| item.enabled).unwrap_or(false),
        store_path: target.to_string_lossy().to_string(),
        integrity,
        diagnostics,
    })
}

/// `urllib.parse.urlparse(url).netloc` 的可用子集。
fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() {
        "registry.npmjs.org".to_string()
    } else {
        host.to_string()
    }
}

/// 开发模式：本地路径插件，不进入可回滚 store。
pub fn register_local_dev_plugin(
    local_path: &Path,
    runner_path: &Path,
    scope: &str,
    workspace_root: Option<&Path>,
    enable: bool,
) -> Result<InstallResult, PluginInstallError> {
    let root = resolve_path(&expand_user(&local_path.to_string_lossy()));
    if !root.is_dir() {
        return Err(PluginInstallError::new(format!(
            "本地插件目录不存在：{}",
            root.display()
        )));
    }
    let package_path = root.join("package.json");
    if !package_path.is_file() {
        return Err(PluginInstallError::new(format!(
            "缺少 package.json：{}",
            package_path.display()
        )));
    }
    let bytes =
        std::fs::read(&package_path).map_err(|error| PluginInstallError::new(error.to_string()))?;
    let parsed: Value = serde_json::from_str(&crate::models::decode_utf8_sig(&bytes))
        .map_err(|error| PluginInstallError::new(error.to_string()))?;
    let manifest = parse_plugin_manifest(&parsed, &package_path.to_string_lossy())
        .map_err(|error| PluginInstallError::new(error.to_string()))?;
    // 本地开发允许先注册，doctor 再报。
    let _ = smoke_test_worker(&root, &manifest.name, runner_path);

    let (registry_path, mut document) = load_scope_registry(scope, workspace_root)?;
    let record = PluginRecord {
        name: manifest.name.clone(),
        enabled: enable,
        active: None,
        candidate: None,
        previous: None,
        approved_permissions: manifest.permissions.clone(),
        local_path: root.to_string_lossy().to_string(),
        dev_mode: true,
    };
    upsert_plugin_record(&mut document, record);
    save_registry_document(&registry_path, &document)
        .map_err(|error| PluginInstallError::new(error.to_string()))?;
    Ok(InstallResult {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        scope: scope.to_string(),
        enabled: enable,
        store_path: root.to_string_lossy().to_string(),
        integrity: "dev".to_string(),
        diagnostics: vec!["dev-mode local path registered".to_string()],
    })
}

pub fn set_enabled(
    name: &str,
    enabled: bool,
    scope: &str,
    workspace_root: Option<&Path>,
) -> Result<(), PluginInstallError> {
    let (registry_path, mut document) = load_scope_registry(scope, workspace_root)?;
    let Some(record) = document.plugins.iter_mut().find(|item| item.name == name) else {
        return Err(PluginInstallError::new(format!("未找到插件：{name}")));
    };
    record.enabled = enabled;
    save_registry_document(&registry_path, &document)
        .map_err(|error| PluginInstallError::new(error.to_string()))?;
    Ok(())
}

pub fn uninstall_plugin(
    name: &str,
    scope: &str,
    workspace_root: Option<&Path>,
    purge: bool,
) -> Result<(), PluginInstallError> {
    let (registry_path, mut document) = load_scope_registry(scope, workspace_root)?;
    let Some(record) = document.get(name).cloned() else {
        return Err(PluginInstallError::new(format!("未找到插件：{name}")));
    };
    let store_paths: Vec<PathBuf> = [&record.active, &record.candidate, &record.previous]
        .into_iter()
        .flatten()
        .filter(|item| !item.store_path.is_empty())
        .map(|item| PathBuf::from(&item.store_path))
        .collect();
    remove_plugin_record(&mut document, name);
    save_registry_document(&registry_path, &document)
        .map_err(|error| PluginInstallError::new(error.to_string()))?;
    if !purge || record.dev_mode {
        return Ok(());
    }
    // 项目注册表分散在任意工作区，当前进程无法证明 store 未被其他项目引用；
    // 安全默认是不做物理删除，user scope 仍可在受信任目录内清理。
    if scope == "project" {
        return Ok(());
    }
    let store_root = resolve_path(&user_store_root());
    let mut referenced = registry_store_references(&document);
    let mut other_paths = vec![user_registry_path()];
    if let Some(workspace_root) = workspace_root {
        other_paths.push(project_registry_path(workspace_root));
    }
    for other_path in other_paths {
        if other_path == registry_path {
            continue;
        }
        if let Ok(other_document) = load_registry_document(&other_path) {
            referenced.extend(registry_store_references(&other_document));
        }
    }
    for raw_path in store_paths {
        let path = expand_user(&raw_path.to_string_lossy());
        if path.is_symlink() {
            return Err(PluginInstallError::new(format!(
                "拒绝清理符号链接 store：{}",
                path.display()
            )));
        }
        let resolved = resolve_path(&path);
        if resolved == store_root || !resolved.starts_with(&store_root) {
            return Err(PluginInstallError::new(format!(
                "拒绝清理 store 根目录之外的路径：{}",
                path.display()
            )));
        }
        if referenced.contains(&resolved.to_string_lossy().to_string()) {
            continue;
        }
        if resolved.is_dir() {
            std::fs::remove_dir_all(&resolved)
                .map_err(|error| PluginInstallError::new(error.to_string()))?;
        }
    }
    Ok(())
}

fn registry_store_references(document: &crate::models::PluginRegistryDocument) -> Vec<String> {
    let mut references: Vec<String> = Vec::new();
    for item in &document.plugins {
        for reference in [&item.active, &item.candidate, &item.previous]
            .into_iter()
            .flatten()
        {
            if reference.store_path.is_empty() {
                continue;
            }
            let resolved = resolve_path(&expand_user(&reference.store_path));
            references.push(resolved.to_string_lossy().to_string());
        }
    }
    references
}

pub fn rollback_plugin(
    name: &str,
    runner_path: &Path,
    scope: &str,
    workspace_root: Option<&Path>,
) -> Result<PluginVersionRef, PluginInstallError> {
    let (registry_path, mut document) = load_scope_registry(scope, workspace_root)?;
    let Some(record) = document.get(name).cloned() else {
        return Err(PluginInstallError::new(format!(
            "插件 {name} 没有可回滚的 previous 版本。"
        )));
    };
    let Some(previous) = record.previous.clone() else {
        return Err(PluginInstallError::new(format!(
            "插件 {name} 没有可回滚的 previous 版本。"
        )));
    };
    if !previous.store_path.is_empty() {
        let store = PathBuf::from(&previous.store_path);
        if !store.is_dir() {
            return Err(PluginInstallError::new(format!(
                "previous 版本 store 不存在：{}",
                previous.store_path
            )));
        }
        verify_store_integrity(&store, &previous.integrity)?;
        verify_lockfile_hash(&store, &previous.lockfile_hash)?;
        verify_content_tree_hash(&store, &previous.content_hash)?;
        // 先冒烟 previous，失败则不改 active 指针，保证旧版本（当前 active）仍可用。
        if let Err(error) = smoke_test_worker(&store, name, runner_path) {
            return Err(PluginInstallError::new(format!(
                "回滚冒烟失败，active 保持不变：{error}"
            )));
        }
    }
    if let Some(slot) = document.plugins.iter_mut().find(|item| item.name == name) {
        slot.active = Some(previous.clone());
        slot.previous = record.active.clone();
        slot.candidate = None;
    }
    save_registry_document(&registry_path, &document)
        .map_err(|error| PluginInstallError::new(error.to_string()))?;
    Ok(previous)
}

pub fn list_plugins(scope: &str, workspace_root: Option<&Path>) -> Vec<Map<String, Value>> {
    let mut rows: Vec<Map<String, Value>> = Vec::new();
    let mut docs: Vec<(&str, PathBuf)> = Vec::new();
    if scope == "user" || scope == "all" {
        docs.push(("user", user_registry_path()));
    }
    if scope == "project" || scope == "all" {
        if let Some(workspace_root) = workspace_root {
            docs.push(("project", project_registry_path(workspace_root)));
        }
    }
    for (scope_name, path) in docs {
        let document = match load_registry_document(&path) {
            Ok(document) => document,
            Err(error) => {
                let mut row = Map::new();
                row.insert("scope".to_string(), Value::from(scope_name));
                row.insert("error".to_string(), Value::from(error.to_string()));
                rows.push(row);
                continue;
            }
        };
        let mut records = document.plugins.clone();
        records.sort_by(|left, right| left.name.cmp(&right.name));
        for record in records {
            let mut row = Map::new();
            row.insert("name".to_string(), Value::from(record.name.clone()));
            row.insert("scope".to_string(), Value::from(scope_name));
            row.insert("enabled".to_string(), Value::from(record.enabled));
            row.insert("devMode".to_string(), Value::from(record.dev_mode));
            row.insert(
                "active".to_string(),
                match &record.active {
                    Some(value) => Value::Object(value.to_dict()),
                    None => Value::Null,
                },
            );
            row.insert(
                "candidate".to_string(),
                match &record.candidate {
                    Some(value) => Value::Object(value.to_dict()),
                    None => Value::Null,
                },
            );
            row.insert(
                "previous".to_string(),
                match &record.previous {
                    Some(value) => Value::Object(value.to_dict()),
                    None => Value::Null,
                },
            );
            row.insert(
                "localPath".to_string(),
                Value::from(record.local_path.clone()),
            );
            row.insert(
                "approvedPermissions".to_string(),
                Value::from(record.approved_permissions.clone()),
            );
            row.insert(
                "registryPath".to_string(),
                Value::from(path.to_string_lossy().to_string()),
            );
            rows.push(row);
        }
    }
    rows
}

pub fn doctor(name: Option<&str>, workspace_root: Option<&Path>) -> Map<String, Value> {
    let mut report = Map::new();
    report.insert("ok".to_string(), Value::from(true));
    report.insert("node".to_string(), Value::Null);
    report.insert("npm".to_string(), Value::Null);
    let mut issues: Vec<Value> = Vec::new();

    match detect_node_npm() {
        Ok((node, npm)) => {
            let node_version = Command::new(&node)
                .arg("--version")
                .output()
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
                .unwrap_or_default();
            let npm_version = Command::new(&npm)
                .arg("--version")
                .output()
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
                .unwrap_or_default();
            report.insert("node".to_string(), Value::from(node_version));
            report.insert("npm".to_string(), Value::from(npm_version));
        }
        Err(error) => {
            report.insert("ok".to_string(), Value::from(false));
            issues.push(Value::from(error.to_string()));
        }
    }

    let mut plugins: Vec<Value> = Vec::new();
    for row in list_plugins("all", workspace_root) {
        if let Some(name) = name {
            let row_name = row
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if row_name != name {
                continue;
            }
        }
        let mut plugin_issues: Vec<Value> = Vec::new();
        if let Some(error) = row.get("error").and_then(Value::as_str) {
            plugin_issues.push(Value::from(error.to_string()));
            report.insert("ok".to_string(), Value::from(false));
        }
        let local = row
            .get("localPath")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let active = row.get("active").and_then(Value::as_object).cloned();
        let store = active
            .as_ref()
            .and_then(|map| map.get("storePath"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let root = if !local.is_empty() { local } else { store };
        if !root.is_empty() && !Path::new(&root).exists() {
            plugin_issues.push(Value::from(format!("路径不存在：{root}")));
            report.insert("ok".to_string(), Value::from(false));
        } else if !root.is_empty() {
            let package_path = Path::new(&root).join("package.json");
            let manifest_result = std::fs::read(&package_path)
                .ok()
                .and_then(|bytes| {
                    serde_json::from_str::<Value>(&crate::models::decode_utf8_sig(&bytes)).ok()
                })
                .ok_or_else(|| "无法读取 package.json".to_string())
                .and_then(|value| {
                    parse_plugin_manifest(&value, "").map_err(|error| error.to_string())
                });
            if let Err(error) = manifest_result {
                plugin_issues.push(Value::from(format!("manifest 无效：{error}")));
                report.insert("ok".to_string(), Value::from(false));
            }
            if let Some(active) = active.as_ref() {
                if let Some(store_path) = active.get("storePath").and_then(Value::as_str) {
                    let integrity = active
                        .get("integrity")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let lockfile_hash = active
                        .get("lockfileHash")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let store_path = PathBuf::from(store_path);
                    for outcome in [
                        verify_store_integrity(&store_path, &integrity),
                        verify_lockfile_hash(&store_path, &lockfile_hash),
                    ] {
                        if let Err(error) = outcome {
                            plugin_issues.push(Value::from(error.to_string()));
                            report.insert("ok".to_string(), Value::from(false));
                        }
                    }
                }
            }
        }
        let mut entry = row.clone();
        entry.insert("issues".to_string(), Value::Array(plugin_issues));
        plugins.push(Value::Object(entry));
    }

    report.insert("issues".to_string(), Value::Array(issues));
    report.insert("plugins".to_string(), Value::Array(plugins));
    report
}
