//! `omnicrawl/extensions/plugin_registry.py` 的 Rust 移植：user/project 合并、tombstone、
//! replace 解析与原子写。
//!
//! 写盘沿用会话层的做法（同目录临时文件 + fsync + 原子 rename），
//! 读盘对 BOM、损坏 JSON 与结构非法分别给出与 Python 同形的文案。

use crate::error::PluginRegistryError;
use crate::models::decode_utf8_sig;
use crate::models::{
    default_timeout_for_mode, sort_handlers, PluginManifest, PluginRecord, PluginRegistryDocument,
    PluginVersionRef, ResolvedHandler, SEALED_HANDLER_PREFIX,
};
use crate::path::{home_directory, resolve_path};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// 返回统一用户配置目录下的插件根目录。
pub fn user_plugins_root() -> PathBuf {
    home_directory().join(".OmniCrawl").join("plugins")
}

pub fn user_registry_path() -> PathBuf {
    user_plugins_root().join("registry.json")
}

pub fn user_store_root() -> PathBuf {
    user_plugins_root().join("store")
}

pub fn project_registry_path(workspace_root: &Path) -> PathBuf {
    resolve_path(workspace_root)
        .join(".omnicrawl")
        .join("plugins.json")
}

pub fn project_lock_path(workspace_root: &Path) -> PathBuf {
    resolve_path(workspace_root)
        .join(".omnicrawl")
        .join("plugins.lock.json")
}

/// 读取注册表；文件不存在时返回空文档。损坏时抛错且不覆盖原文件。
pub fn load_registry_document(path: &Path) -> Result<PluginRegistryDocument, PluginRegistryError> {
    if !path.exists() {
        return Ok(PluginRegistryDocument::default());
    }
    let bytes = std::fs::read(path).map_err(|error| {
        PluginRegistryError::new(format!("读取注册表失败：{}，{error}", path.display()))
    })?;
    let text = decode_utf8_sig(&bytes);
    let data: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            return Err(PluginRegistryError::new(format!(
                "注册表 JSON 损坏：{}，第 {} 行：{}。已进入无插件降级保护，请使用 plugin doctor 恢复。",
                path.display(),
                error.line(),
                error
            )));
        }
    };
    if !data.is_object() {
        return Err(PluginRegistryError::new(format!(
            "注册表顶层必须是对象：{}",
            path.display()
        )));
    }
    PluginRegistryDocument::from_dict(&data).map_err(|error| {
        PluginRegistryError::new(format!("注册表结构无效：{}，{error}", path.display()))
    })
}

/// 同目录临时文件 + flush/fsync + 原子 replace。
pub fn atomic_write_json(
    path: &Path,
    data: &Map<String, Value>,
) -> Result<(), PluginRegistryError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| {
                PluginRegistryError::new(format!(
                    "创建注册表目录失败：{}，{error}",
                    parent.display()
                ))
            })?;
        }
    }
    let payload = format!("{}\n", pretty_json(data));
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|item| item.into_inner());
    let temp_path = unique_temp_path(path);
    let write_result = (|| -> std::io::Result<()> {
        let mut handle = std::fs::File::create(&temp_path)?;
        handle.write_all(payload.as_bytes())?;
        handle.flush()?;
        handle.sync_all()?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(PluginRegistryError::new(format!(
            "写入注册表失败：{}，{error}",
            path.display()
        )));
    }
    if let Err(error) = std::fs::rename(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(PluginRegistryError::new(format!(
            "写入注册表失败：{}，{error}",
            path.display()
        )));
    }
    Ok(())
}

pub fn save_registry_document(
    path: &Path,
    document: &PluginRegistryDocument,
) -> Result<(), PluginRegistryError> {
    atomic_write_json(path, &document.to_dict())
}

/// project scope 覆盖 user scope 的同名插件记录。
pub fn merge_registry_documents(
    user_doc: &PluginRegistryDocument,
    project_doc: &PluginRegistryDocument,
) -> PluginRegistryDocument {
    let mut merged = user_doc.clone();
    merged.schema_version = user_doc.schema_version.max(project_doc.schema_version);
    for record in &project_doc.plugins {
        match merged
            .plugins
            .iter_mut()
            .find(|item| item.name == record.name)
        {
            Some(slot) => *slot = record.clone(),
            None => merged.plugins.push(record.clone()),
        }
    }
    for item in &project_doc.disabled_handlers {
        if !merged.disabled_handlers.contains(item) {
            merged.disabled_handlers.push(item.clone());
        }
    }
    merged
}

/// 执行计划的输入条目：`(manifest, scope, record)`，顺序即加载顺序。
#[derive(Debug, Clone)]
pub struct ManifestEntry {
    pub manifest: PluginManifest,
    pub scope: String,
    pub record: PluginRecord,
}

/// 根据已加载 manifest 构建不可变执行计划。
pub fn build_execution_plan(
    manifests: &[ManifestEntry],
    disabled_handlers: &[String],
    max_timeout_ms: i64,
) -> Vec<ResolvedHandler> {
    let disabled: HashSet<&String> = disabled_handlers.iter().collect();
    let mut candidates: Vec<ResolvedHandler> = Vec::new();
    for entry in manifests {
        let manifest = &entry.manifest;
        let record = &entry.record;
        if !record.enabled {
            continue;
        }
        let version = match record.active.as_ref() {
            Some(active) => active.version.clone(),
            None => {
                if record.dev_mode {
                    manifest.version.clone()
                } else {
                    String::new()
                }
            }
        };
        let integrity_prefix = match record
            .active
            .as_ref()
            .filter(|item| !item.integrity.is_empty())
        {
            Some(active) => {
                let tail = active
                    .integrity
                    .split_once('-')
                    .map(|(_, rest)| rest)
                    .unwrap_or(active.integrity.as_str());
                tail.chars().take(12).collect()
            }
            None => String::new(),
        };
        for handler in &manifest.hooks {
            let key = format!("{}/{}", manifest.name, handler.id);
            if disabled.contains(&key) {
                continue;
            }
            let timeout = handler
                .timeout_ms
                .or(manifest.timeout_ms)
                .unwrap_or_else(|| default_timeout_for_mode(&handler.mode));
            let timeout = timeout.min(max_timeout_ms);
            candidates.push(ResolvedHandler {
                key,
                plugin_name: manifest.name.clone(),
                plugin_version: if version.is_empty() {
                    manifest.version.clone()
                } else {
                    version.clone()
                },
                handler_id: handler.id.clone(),
                hook: handler.hook.clone(),
                mode: handler.mode.clone(),
                priority: handler.priority,
                scope: entry.scope.clone(),
                timeout_ms: timeout,
                replaces: handler.replaces.clone(),
                integrity_prefix: integrity_prefix.clone(),
                local_path: record.local_path.clone(),
                permissions: manifest.permissions.clone(),
            });
        }
    }
    resolve_replacements(candidates)
}

/// 处理显式 replaces：冲突或循环时丢弃相关 Handler。
pub fn resolve_replacements(handlers: Vec<ResolvedHandler>) -> Vec<ResolvedHandler> {
    let mut by_key: Vec<(String, ResolvedHandler)> = handlers
        .iter()
        .map(|item| (item.key.clone(), item.clone()))
        .collect();
    let mut replace_edges: Vec<(String, Vec<String>)> = Vec::new();
    let mut targets: Vec<(String, Vec<String>)> = Vec::new();

    for item in &handlers {
        for target in &item.replaces {
            if target.starts_with(SEALED_HANDLER_PREFIX) {
                // sealed 目标在 manifest 阶段已拒绝；这里双保险。
                by_key.retain(|(key, _)| key != &item.key);
                continue;
            }
            // 只能替换同一 Hook 上的 Handler。
            if let Some((_, existing)) = by_key.iter().find(|(key, _)| key == target) {
                if existing.hook != item.hook {
                    by_key.retain(|(key, _)| key != &item.key);
                    continue;
                }
            }
            match replace_edges.iter_mut().find(|(key, _)| key == &item.key) {
                Some((_, edges)) => edges.push(target.clone()),
                None => replace_edges.push((item.key.clone(), vec![target.clone()])),
            }
            match targets.iter_mut().find(|(key, _)| key == target) {
                Some((_, sources)) => sources.push(item.key.clone()),
                None => targets.push((target.clone(), vec![item.key.clone()])),
            }
        }
    }

    // 多个插件同时替换同一目标 → 冲突，相关替换方不启用。
    let mut conflicted: HashSet<String> = HashSet::new();
    for (_, sources) in &targets {
        if sources.len() > 1 {
            for source in sources {
                conflicted.insert(source.clone());
            }
        }
    }

    let mut cyclic: HashSet<String> = HashSet::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut visiting: HashSet<String> = HashSet::new();
    for (key, _) in &replace_edges {
        dfs(
            key,
            &replace_edges,
            &mut visited,
            &mut visiting,
            &mut cyclic,
        );
    }

    let mut disabled_sources: HashSet<String> = conflicted.clone();
    disabled_sources.extend(cyclic.iter().cloned());
    let mut removed_targets: HashSet<String> = HashSet::new();
    for (source, edges) in &replace_edges {
        if disabled_sources.contains(source) {
            continue;
        }
        if !by_key.iter().any(|(key, _)| key == source) {
            continue;
        }
        for target in edges {
            removed_targets.insert(target.clone());
        }
    }

    let result: Vec<ResolvedHandler> = by_key
        .into_iter()
        .filter(|(key, _)| !removed_targets.contains(key) && !disabled_sources.contains(key))
        .map(|(_, item)| item)
        .collect();
    sort_handlers(&result)
}

fn dfs(
    node: &str,
    edges: &[(String, Vec<String>)],
    visited: &mut HashSet<String>,
    visiting: &mut HashSet<String>,
    cyclic: &mut HashSet<String>,
) {
    if visited.contains(node) {
        return;
    }
    if visiting.contains(node) {
        cyclic.insert(node.to_string());
        return;
    }
    visiting.insert(node.to_string());
    if let Some((_, nexts)) = edges.iter().find(|(key, _)| key == node) {
        for next in nexts {
            dfs(next, edges, visited, visiting, cyclic);
            if cyclic.contains(next) {
                cyclic.insert(node.to_string());
            }
        }
    }
    visiting.remove(node);
    visited.insert(node.to_string());
}

pub fn set_plugin_enabled(
    document: &mut PluginRegistryDocument,
    name: &str,
    enabled: bool,
) -> Result<(), PluginRegistryError> {
    let Some(record) = document.plugins.iter_mut().find(|item| item.name == name) else {
        return Err(PluginRegistryError::new(format!(
            "注册表中不存在插件：{name}"
        )));
    };
    record.enabled = enabled;
    Ok(())
}

pub fn upsert_plugin_record(document: &mut PluginRegistryDocument, record: PluginRecord) {
    match document
        .plugins
        .iter_mut()
        .find(|item| item.name == record.name)
    {
        Some(slot) => *slot = record,
        None => document.plugins.push(record),
    }
}

pub fn remove_plugin_record(document: &mut PluginRegistryDocument, name: &str) {
    document.plugins.retain(|item| item.name != name);
    let prefix = format!("{name}/");
    document
        .disabled_handlers
        .retain(|item| !item.starts_with(&prefix));
}

pub fn add_tombstone(
    document: &mut PluginRegistryDocument,
    handler_key: &str,
) -> Result<(), PluginRegistryError> {
    if handler_key.starts_with(SEALED_HANDLER_PREFIX) {
        return Err(PluginRegistryError::new(format!(
            "不能 tombstone sealed Handler：{handler_key}"
        )));
    }
    if !document
        .disabled_handlers
        .iter()
        .any(|item| item == handler_key)
    {
        document.disabled_handlers.push(handler_key.to_string());
    }
    Ok(())
}

pub fn version_ref_dict(reference: Option<&PluginVersionRef>) -> Option<Map<String, Value>> {
    reference.map(PluginVersionRef::to_dict)
}

/// `json.dumps(dict(data), ensure_ascii=False, indent=2)`：缩进两格、UTF-8 原样输出。
fn pretty_json(data: &Map<String, Value>) -> String {
    serde_json::to_string_pretty(&Value::Object(data.clone())).unwrap_or_default()
}

/// `tempfile.mkstemp` 的等价物：同目录下 `.{文件名}.{随机}.tmp`。
fn unique_temp_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|item| item.to_string_lossy().to_string())
        .unwrap_or_default();
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    parent.join(format!(".{name}.{nanos:x}{sequence:x}.tmp"))
}
