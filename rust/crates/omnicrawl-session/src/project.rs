//! 项目列表存储：对齐 Python `omnicrawl/state/project.py`。
//!
//! `ProjectStore` 只负责「项目列表」本身：读写 `<session_root>/projects.json`、扫描会话索引
//! 得到项目路径、创建与导入目录，以及只读聚合的项目总览。会话数据仍由 `SessionStore` 维护，
//! 二者通过规范化后的绝对路径关联。
//!
//! 与 Python 的三处已知差异（crate README 有记录）：`casefold()` 用 `to_lowercase()` 近似；
//! `os.path.expandvars` 只实现 `$VAR` / `${VAR}` /（Windows）`%VAR%` 这个子集；
//! 落盘用的临时文件名与 Python 的 `<file>.json.tmp` 不同（`locking::atomic_write_text` 约定），
//! 最终文件字节一致。

use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::error::SessionStoreError;
use crate::locking::atomic_write_text;
use crate::time::{datetime_to_millis, format_datetime, parse_datetime, utc_now};

pub const PROJECTS_FILE_NAME: &str = "projects.json";
/// Agent 隔离工作树宿主根目录名（主 Agent 隔离区 aw-*/SubAgent 隔离区 sw-*）。
///
/// 与 `workspace/agent_isolation.py` 的 `DEFAULT_WORKTREES_ROOT` 保持同值；state 层不反向
/// 依赖 workspace，因此在此独立定义同构常量。
pub const AGENT_WORKTREES_DIR_NAME: &str = "agent-worktrees";

fn error(message: impl Into<String>) -> SessionStoreError {
    SessionStoreError::new(message)
}

/// `.agent_sessions/projects.json` 中的一条项目记录。
///
/// 项目列表只保存本地路径、展示名和轻量状态，不复制项目文件，也不改变 Agent 当前工作区的
/// 安全边界；会话分组通过会话索引里的 `workspace_root` 动态关联。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectEntry {
    pub name: String,
    pub path: String,
    pub created_at: String,
    pub updated_at: String,
    pub pinned: bool,
    pub source: String,
}

impl ProjectEntry {
    /// 从 `projects.json` 的一条记录构造：逐字段校验与归一化，文案与 Python 一致。
    pub fn from_dict(data: &Map<String, Value>) -> Result<Self, SessionStoreError> {
        let name = data
            .get("name")
            .cloned()
            .unwrap_or(Value::String(String::new()));
        let path = data
            .get("path")
            .cloned()
            .unwrap_or(Value::String(String::new()));
        let created_at = data
            .get("created_at")
            .cloned()
            .unwrap_or(Value::String(String::new()));
        let updated_at = data
            .get("updated_at")
            .cloned()
            .unwrap_or(Value::String(String::new()));
        let pinned = data.get("pinned").cloned().unwrap_or(Value::Bool(false));
        let source = data
            .get("source")
            .cloned()
            .unwrap_or_else(|| Value::String("manual".to_string()));

        if !is_non_empty_string(&name) {
            return Err(error("项目名称必须是非空字符串。"));
        }
        if !is_non_empty_string(&path) {
            return Err(error("项目路径必须是非空字符串。"));
        }
        if !is_non_empty_string(&source) {
            return Err(error("项目来源必须是非空字符串。"));
        }

        Ok(Self {
            name: clean_project_name(name.as_str().expect("已校验为字符串"))?,
            path: normalize_project_path(path.as_str().expect("已校验为字符串"))?,
            created_at: format_datetime(parse_project_datetime(&created_at)?),
            updated_at: format_datetime(parse_project_datetime(&updated_at)?),
            pinned: json_truthy(&pinned),
            source: source.as_str().expect("已校验为字符串").trim().to_string(),
        })
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("项目记录可序列化")
    }
}

/// 基于 `<session_root>/projects.json` 的项目列表存储。
pub struct ProjectStore {
    root: PathBuf,
    path: PathBuf,
}

impl ProjectStore {
    pub fn open(session_root: impl AsRef<Path>) -> Self {
        let root = resolve_existing_or_lexical(session_root.as_ref());
        let path = root.join(PROJECTS_FILE_NAME);
        Self { root, path }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 建目录骨架；`projects.json` 不存在时写入空列表。
    pub fn ensure(&self) -> Result<(), SessionStoreError> {
        create_dir_all(&self.root)?;
        if !self.path.exists() {
            self.save_entries(&[])?;
        }
        Ok(())
    }

    /// 返回项目列表，置顶项目优先，其余按更新时间倒序排列。
    ///
    /// Agent 隔离工作树（`~/.omnicrawl/agent-worktrees/` 下的 aw-*/sw-*）不属于用户项目，
    /// 读取时兜底过滤，避免历史残留继续出现在界面。
    pub fn list_projects(&self) -> Result<Vec<ProjectEntry>, SessionStoreError> {
        self.ensure()?;
        let mut entries: Vec<ProjectEntry> = self
            .load_entries()?
            .into_iter()
            .filter(|entry| !under_agent_worktrees(&entry.path))
            .collect();
        entries.sort_by(|left, right| {
            sort_key(&left.pinned, &left.updated_at, &left.name).cmp(&sort_key(
                &right.pinned,
                &right.updated_at,
                &right.name,
            ))
        });
        Ok(entries)
    }

    /// 把会话索引里出现过的工作区同步进项目列表。
    ///
    /// 只补齐缺失路径，不删除用户手动导入的项目（手动项目目录暂时不可用时列表仍保留）。
    /// Agent 隔离工作树、系统临时目录下的 `tmp*` 残留、解释器库目录与已不存在的目录
    /// 都会被忽略，扫描残留也会被移除。
    pub fn scan_projects(
        &self,
        workspace_roots: &[String],
        current_workspace: Option<&str>,
        now: Option<DateTime<Utc>>,
    ) -> Result<Vec<ProjectEntry>, SessionStoreError> {
        self.ensure()?;
        let timestamp = now.unwrap_or_else(utc_now);
        let loaded = self.load_entries()?;
        let mut entries: Vec<ProjectEntry> = loaded
            .iter()
            .filter(|entry| entry.source != "scanned" || !is_scan_excluded(&entry.path))
            .cloned()
            .collect();
        let mut by_path: Map<String, Value> = Map::new();
        for entry in &entries {
            by_path.insert(path_key(&entry.path), Value::Null);
        }

        let mut candidates: Vec<String> = Vec::new();
        if let Some(workspace) = current_workspace {
            candidates.push(workspace.to_string());
        }
        candidates.extend(workspace_roots.iter().cloned());

        let mut changed = entries.len() != loaded.len();
        for raw_path in candidates {
            let project_path = normalize_project_path(&raw_path)?;
            if is_scan_excluded(&project_path) {
                continue;
            }
            let key = path_key(&project_path);
            if by_path.contains_key(&key) {
                continue;
            }
            entries.push(ProjectEntry {
                name: project_name_from_path(&project_path),
                path: project_path,
                created_at: format_datetime(timestamp),
                updated_at: format_datetime(timestamp),
                pinned: false,
                source: "scanned".to_string(),
            });
            by_path.insert(key, Value::Null);
            changed = true;
        }

        if changed {
            self.save_entries(&entries)?;
        }
        self.list_projects()
    }

    /// 只读聚合项目总览：显式项目 + 会话索引中的稳定目录。
    ///
    /// 聚合键是 git 仓库根（在 git 工作树内）否则目录自身，因此子目录会话会归并到仓库；
    /// 隔离工作树、临时目录与解释器库目录被排除；显式项目恒定保留（即使 `session_count = 0`），
    /// `scanned` 且 `session_count = 0` 的目录不展示；本方法不写盘。
    pub fn project_overview(
        &self,
        session_entries: &[OverviewSessionEntry],
        recent_limit: Option<usize>,
        sort_limit: usize,
    ) -> Result<Vec<ProjectOverview>, SessionStoreError> {
        self.ensure()?;
        let explicit: Vec<ProjectEntry> = self
            .load_entries()?
            .into_iter()
            .filter(|entry| entry.source != "scanned" || !is_scan_excluded(&entry.path))
            .collect();

        let mut merged: Vec<OverviewBuilder> = Vec::new();
        for entry in &explicit {
            let (key, target) = aggregate_key(&entry.path);
            match merged.iter_mut().find(|item| item.key == key) {
                None => merged.push(OverviewBuilder::new(
                    entry.name.clone(),
                    target,
                    key,
                    entry.source.clone(),
                    entry.pinned,
                )),
                Some(item) if item.source == "scanned" => {
                    item.source = entry.source.clone();
                    item.pinned = item.pinned || entry.pinned;
                    item.name = entry.name.clone();
                }
                Some(_) => {}
            }
        }

        let mut collected: Vec<(String, Vec<OverviewSession>)> = Vec::new();
        for entry in session_entries {
            let Some(raw) = entry
                .workspace_root
                .as_deref()
                .filter(|raw| !raw.is_empty())
            else {
                continue;
            };
            let Ok(path) = normalize_project_path(raw) else {
                continue;
            };
            if is_scan_excluded(&path) {
                continue;
            }
            let (key, target) = aggregate_key(&path);
            let stamp = entry
                .updated_at
                .map(format_datetime)
                .unwrap_or_else(String::new);
            let position = match merged.iter().position(|item| item.key == key) {
                Some(index) => index,
                None => {
                    merged.push(OverviewBuilder::new(
                        Path::new(&target)
                            .file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .filter(|name| !name.is_empty())
                            .unwrap_or_else(|| target.clone()),
                        target,
                        key.clone(),
                        "scanned".to_string(),
                        false,
                    ));
                    merged.len() - 1
                }
            };
            let item = &mut merged[position];
            item.session_count += 1;
            if item.recent_at.is_empty() || (!stamp.is_empty() && stamp > item.recent_at) {
                item.recent_at = stamp.clone();
            }
            match collected.iter_mut().find(|(stored, _)| *stored == key) {
                Some((_, list)) => list.push(OverviewSession {
                    session_id: entry.session_id.clone(),
                    title: entry.title.clone(),
                    updated_at: stamp,
                }),
                None => collected.push((
                    key,
                    vec![OverviewSession {
                        session_id: entry.session_id.clone(),
                        title: entry.title.clone(),
                        updated_at: stamp,
                    }],
                )),
            }
        }

        let limit = recent_limit.map(|value| value.max(1));
        for (key, mut sessions) in collected {
            if let Some(item) = merged.iter_mut().find(|item| item.key == key) {
                sessions.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
                if let Some(limit) = limit {
                    sessions.truncate(limit);
                }
                item.recent_sessions = sessions;
            }
        }

        let mut result: Vec<ProjectOverview> = merged
            .into_iter()
            .filter(|item| item.source != "scanned" || item.session_count > 0)
            .map(OverviewBuilder::finish)
            .collect();
        if sort_limit > 0 && result.len() > sort_limit {
            result.truncate(sort_limit);
        }
        result.sort_by(|left, right| {
            let left_millis = if left.recent_at.is_empty() {
                0
            } else {
                -parse_datetime(&left.recent_at)
                    .map(datetime_to_millis)
                    .unwrap_or(0)
            };
            let right_millis = if right.recent_at.is_empty() {
                0
            } else {
                -parse_datetime(&right.recent_at)
                    .map(datetime_to_millis)
                    .unwrap_or(0)
            };
            (!left.pinned, left_millis, left.name.to_lowercase()).cmp(&(
                !right.pinned,
                right_millis,
                right.name.to_lowercase(),
            ))
        });
        Ok(result)
    }

    /// 创建项目目录并写入项目列表；已有目录会作为幂等导入处理。
    pub fn create_project(
        &self,
        name: &str,
        path: &str,
        now: Option<DateTime<Utc>>,
    ) -> Result<ProjectEntry, SessionStoreError> {
        let cleaned_name = clean_project_name(name)?;
        let project_path = normalize_project_path(path)?;
        let candidate = Path::new(&project_path);
        if candidate.exists() && !candidate.is_dir() {
            return Err(error(format!("项目路径已存在但不是目录：{project_path}")));
        }
        if let Err(exc) = std::fs::create_dir_all(candidate) {
            return Err(error(format!(
                "创建项目目录失败：{project_path}，{}",
                io_message(&exc)
            )));
        }
        self.upsert_project(&cleaned_name, &project_path, "created", now)
    }

    /// 导入已有项目目录并写入项目列表。
    pub fn import_project(
        &self,
        name: &str,
        path: &str,
        now: Option<DateTime<Utc>>,
    ) -> Result<ProjectEntry, SessionStoreError> {
        let cleaned_name = clean_project_name(name)?;
        let project_path = normalize_project_path(path)?;
        let candidate = Path::new(&project_path);
        if !candidate.exists() {
            return Err(error(format!("导入项目路径不存在：{project_path}")));
        }
        if !candidate.is_dir() {
            return Err(error(format!("导入项目路径必须是目录：{project_path}")));
        }
        self.upsert_project(&cleaned_name, &project_path, "imported", now)
    }

    /// 只修改项目列表中的展示名，不重命名磁盘目录。
    pub fn rename_project(
        &self,
        path: &str,
        name: &str,
        now: Option<DateTime<Utc>>,
    ) -> Result<ProjectEntry, SessionStoreError> {
        let project_path = normalize_project_path(path)?;
        let cleaned_name = clean_project_name(name)?;
        let timestamp = format_datetime(now.unwrap_or_else(utc_now));
        let entries = self.load_entries()?;
        let mut updated: Vec<ProjectEntry> = Vec::with_capacity(entries.len());
        let mut renamed: Option<ProjectEntry> = None;
        for entry in entries {
            if path_key(&entry.path) != path_key(&project_path) {
                updated.push(entry);
                continue;
            }
            let renamed_entry = ProjectEntry {
                name: cleaned_name.clone(),
                path: entry.path.clone(),
                created_at: entry.created_at.clone(),
                updated_at: timestamp.clone(),
                pinned: entry.pinned,
                source: entry.source.clone(),
            };
            updated.push(renamed_entry.clone());
            renamed = Some(renamed_entry);
        }
        let Some(renamed) = renamed else {
            return Err(error(format!("未找到项目：{project_path}")));
        };
        self.save_entries(&updated)?;
        Ok(renamed)
    }

    /// 设置项目置顶状态。
    pub fn pin_project(
        &self,
        path: &str,
        pinned: bool,
        now: Option<DateTime<Utc>>,
    ) -> Result<ProjectEntry, SessionStoreError> {
        let project_path = normalize_project_path(path)?;
        let timestamp = format_datetime(now.unwrap_or_else(utc_now));
        let entries = self.load_entries()?;
        let mut updated: Vec<ProjectEntry> = Vec::with_capacity(entries.len());
        let mut pinned_entry: Option<ProjectEntry> = None;
        for entry in entries {
            if path_key(&entry.path) != path_key(&project_path) {
                updated.push(entry);
                continue;
            }
            let target = ProjectEntry {
                name: entry.name.clone(),
                path: entry.path.clone(),
                created_at: entry.created_at.clone(),
                updated_at: timestamp.clone(),
                pinned,
                source: entry.source.clone(),
            };
            updated.push(target.clone());
            pinned_entry = Some(target);
        }
        let Some(pinned_entry) = pinned_entry else {
            return Err(error(format!("未找到项目：{project_path}")));
        };
        self.save_entries(&updated)?;
        Ok(pinned_entry)
    }

    /// 切换项目置顶状态，供 UI 的单按钮置顶/取消置顶使用。
    pub fn toggle_project_pin(
        &self,
        path: &str,
        now: Option<DateTime<Utc>>,
    ) -> Result<ProjectEntry, SessionStoreError> {
        let project_path = normalize_project_path(path)?;
        let entries = self.load_entries()?;
        for entry in entries {
            if path_key(&entry.path) == path_key(&project_path) {
                return self.pin_project(&project_path, !entry.pinned, now);
            }
        }
        Err(error(format!("未找到项目：{project_path}")))
    }

    /// 从项目列表移除记录，不删除磁盘上的项目目录或会话文件。
    pub fn remove_project(&self, path: &str) -> Result<(), SessionStoreError> {
        let project_path = normalize_project_path(path)?;
        let entries = self.load_entries()?;
        let remaining: Vec<ProjectEntry> = entries
            .iter()
            .filter(|entry| path_key(&entry.path) != path_key(&project_path))
            .cloned()
            .collect();
        if remaining.len() == entries.len() {
            return Err(error(format!("未找到项目：{project_path}")));
        }
        self.save_entries(&remaining)
    }

    fn upsert_project(
        &self,
        name: &str,
        path: &str,
        source: &str,
        now: Option<DateTime<Utc>>,
    ) -> Result<ProjectEntry, SessionStoreError> {
        self.ensure()?;
        let timestamp = format_datetime(now.unwrap_or_else(utc_now));
        let normalized_path = normalize_project_path(path)?;
        if under_agent_worktrees(&normalized_path) {
            return Err(error(format!(
                "Agent 隔离工作树目录不能加入项目列表：{normalized_path}"
            )));
        }
        let entries = self.load_entries()?;
        let mut updated: Vec<ProjectEntry> = Vec::with_capacity(entries.len() + 1);
        let mut saved: Option<ProjectEntry> = None;
        for entry in entries {
            if path_key(&entry.path) != path_key(&normalized_path) {
                updated.push(entry);
                continue;
            }
            let target = ProjectEntry {
                name: name.to_string(),
                path: entry.path.clone(),
                created_at: entry.created_at.clone(),
                updated_at: timestamp.clone(),
                pinned: entry.pinned,
                source: source.to_string(),
            };
            updated.push(target.clone());
            saved = Some(target);
        }
        if saved.is_none() {
            let target = ProjectEntry {
                name: name.to_string(),
                path: normalized_path,
                created_at: timestamp.clone(),
                updated_at: timestamp,
                pinned: false,
                source: source.to_string(),
            };
            updated.push(target.clone());
            saved = Some(target);
        }
        self.save_entries(&updated)?;
        Ok(saved.expect("上面两个分支都赋值"))
    }

    fn load_entries(&self) -> Result<Vec<ProjectEntry>, SessionStoreError> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let text = match std::fs::read(&self.path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    return Err(error(format!(
                        "项目列表不是 UTF-8 文本：{}",
                        self.path.display()
                    )))
                }
            },
            Err(exc) => {
                return Err(error(format!(
                    "读取项目列表失败：{}，{}",
                    self.path.display(),
                    io_message(&exc)
                )))
            }
        };
        let data: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(_) => {
                return Err(error(format!(
                    "项目列表不是合法 JSON：{}",
                    self.path.display()
                )))
            }
        };

        let projects = match &data {
            Value::Object(map) => map.get("projects").cloned().unwrap_or(Value::Array(vec![])),
            _ => Value::Array(vec![]),
        };
        let Value::Array(items) = projects else {
            return Err(error("项目列表顶层字段 projects 必须是列表。"));
        };
        let mut entries: Vec<ProjectEntry> = Vec::with_capacity(items.len());
        for item in items {
            if let Value::Object(map) = item {
                entries.push(ProjectEntry::from_dict(&map)?);
            }
        }
        Ok(entries)
    }

    fn save_entries(&self, entries: &[ProjectEntry]) -> Result<(), SessionStoreError> {
        create_dir_all(&self.root)?;
        let payload = serde_json::json!({
            "projects": entries.iter().map(ProjectEntry::to_value).collect::<Vec<_>>(),
        });
        let text = format!(
            "{}\n",
            serde_json::to_string_pretty(&payload).expect("项目列表可序列化")
        );
        atomic_write_text(&self.path, &text, true).map_err(|exc| {
            error(format!(
                "写入项目列表失败：{}，{}",
                self.path.display(),
                io_message(&exc)
            ))
        })
    }
}

/// 会话索引条目在总览聚合里用到的字段（对应 Python 侧会话索引条目对象）。
#[derive(Debug, Clone, Default)]
pub struct OverviewSessionEntry {
    pub workspace_root: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
    pub session_id: String,
    pub title: String,
}

/// 项目总览里的一条会话摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OverviewSession {
    pub session_id: String,
    pub title: String,
    pub updated_at: String,
}

/// 项目总览视图（对应 Python `project_overview` 返回的视图字典）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectOverview {
    pub name: String,
    pub path: String,
    pub source: String,
    pub pinned: bool,
    pub session_count: u64,
    pub recent_at: String,
    pub recent_sessions: Vec<OverviewSession>,
}

struct OverviewBuilder {
    key: String,
    name: String,
    path: String,
    source: String,
    pinned: bool,
    session_count: u64,
    recent_at: String,
    recent_sessions: Vec<OverviewSession>,
}

impl OverviewBuilder {
    fn new(name: String, path: String, key: String, source: String, pinned: bool) -> Self {
        Self {
            key,
            name,
            path,
            source,
            pinned,
            session_count: 0,
            recent_at: String::new(),
            recent_sessions: Vec::new(),
        }
    }

    fn finish(self) -> ProjectOverview {
        ProjectOverview {
            name: self.name,
            path: self.path,
            source: self.source,
            pinned: self.pinned,
            session_count: self.session_count,
            recent_at: self.recent_at,
            recent_sessions: self.recent_sessions,
        }
    }
}

fn sort_key(pinned: &bool, updated_at: &str, name: &str) -> (bool, i64, String) {
    let millis = parse_datetime(updated_at)
        .map(datetime_to_millis)
        .unwrap_or(0);
    (!*pinned, -millis, name.to_lowercase())
}

fn aggregate_key(path: &str) -> (String, String) {
    let root = git_root(path);
    let target = root.unwrap_or_else(|| normalize_project_path(path).unwrap_or_default());
    (path_key(&target), target)
}

/// 向上查找有效 git 仓库根（含 `.git` 目录且含 HEAD，或 gitfile），找不到返回 None。
///
/// - 仅目录名 `.git` 但无 HEAD（`git init` 中断残留）不算仓库，避免把容器目录误当项目；
/// - worktree 场景 `.git` 是文件（指向真实仓库），仍视为同一仓库根。
pub fn git_root(path: &str) -> Option<String> {
    let mut cursor = resolve_existing_or_lexical(Path::new(path));
    loop {
        let dot_git = cursor.join(".git");
        if dot_git.is_dir() {
            if dot_git.join("HEAD").exists() {
                return Some(cursor.to_string_lossy().to_string());
            }
        } else if dot_git.is_file() {
            return Some(cursor.to_string_lossy().to_string());
        }
        let parent = cursor.parent().map(Path::to_path_buf)?;
        if parent == cursor {
            return None;
        }
        cursor = parent;
    }
}

/// 路径是否位于 Agent 隔离工作树宿主根（`~/.omnicrawl/agent-worktrees/`）下。
pub fn under_agent_worktrees(path: &str) -> bool {
    let expanded = expand_user(path);
    let root = home_directory()
        .join(".omnicrawl")
        .join(AGENT_WORKTREES_DIR_NAME)
        .to_string_lossy()
        .to_string();
    path_is_relative_to(&normcase(&expanded), &normcase(&root))
}

/// 自动扫描候选是否应被忽略：隔离工作树 / 系统临时残留 / 解释器库目录 / 已删除目录。
pub fn is_scan_excluded(path: &str) -> bool {
    if under_agent_worktrees(path) {
        return true;
    }
    let expanded = PathBuf::from(expand_user(path));
    let temp_root = normcase(&realpath(&std::env::temp_dir().to_string_lossy()));
    let normalized = normcase(&realpath(&expanded.to_string_lossy()));
    let in_temp = normalized == temp_root
        || normalized.starts_with(&format!("{temp_root}{}", std::path::MAIN_SEPARATOR));
    if in_temp {
        let name = expanded
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        if normalized == temp_root || name.starts_with("tmp") {
            return true;
        }
    }
    let normed = normcase(&expanded.to_string_lossy());
    let separator = std::path::MAIN_SEPARATOR;
    let fragments = [
        format!("{separator}site-packages"),
        format!("{separator}python{separator}python"),
        format!("{separator}anaconda3"),
        format!("{separator}miniconda3"),
        format!("{separator}node_modules"),
        format!("{separator}.venv"),
        format!("{separator}venv"),
    ];
    if fragments.iter().any(|fragment| normed.contains(fragment)) {
        return true;
    }
    !Path::new(path).exists()
}

/// 项目路径归一化：展开环境变量与 `~`，再解析成绝对路径（不存在时按词法归一）。
pub fn normalize_project_path(raw_path: &str) -> Result<String, SessionStoreError> {
    if raw_path.trim().is_empty() {
        return Err(error("项目路径必须是非空字符串。"));
    }
    let candidate = expand_user(&expand_vars(raw_path.trim()));
    Ok(resolve_existing_or_lexical(Path::new(&candidate))
        .to_string_lossy()
        .to_string())
}

/// 展示名清洗：折叠空白、空名报错、超过 80 字符截断成 `...`。
pub fn clean_project_name(value: &str) -> Result<String, SessionStoreError> {
    let name = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        return Err(error("项目名称不能为空。"));
    }
    let characters: Vec<char> = name.chars().collect();
    if characters.len() > 80 {
        let kept: String = characters[..77].iter().collect();
        return Ok(format!("{kept}..."));
    }
    Ok(name)
}

fn project_name_from_path(path: &str) -> String {
    let name = Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let trimmed = name.trim().to_string();
    if trimmed.is_empty() {
        path.to_string()
    } else {
        trimmed
    }
}

/// 路径比较键：`os.path.normcase` + `casefold()`（Windows 小写化并统一分隔符）。
pub fn path_key(path: &str) -> String {
    normcase(path).to_lowercase()
}

fn parse_project_datetime(raw: &Value) -> Result<DateTime<Utc>, SessionStoreError> {
    let Some(text) = raw.as_str() else {
        return Err(error("项目时间戳必须是非空字符串。"));
    };
    if text.trim().is_empty() {
        return Err(error("项目时间戳必须是非空字符串。"));
    }
    parse_datetime(text).map_err(|_| error(format!("项目时间戳格式无效：{text}")))
}

fn is_non_empty_string(value: &Value) -> bool {
    value
        .as_str()
        .map(|text| !text.trim().is_empty())
        .unwrap_or(false)
}

/// Python 的 truthiness：字符串非空、数字非零、数组/对象非空、`true` 为真。
fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|value| value != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

fn normcase(path: &str) -> String {
    if cfg!(windows) {
        path.replace('/', "\\").to_lowercase()
    } else {
        path.to_string()
    }
}

fn path_parts(path: &str) -> Vec<String> {
    path.split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .map(|part| part.to_lowercase())
        .collect()
}

fn path_is_relative_to(child: &str, root: &str) -> bool {
    let child_parts = path_parts(child);
    let root_parts = path_parts(root);
    if root_parts.len() > child_parts.len() {
        return false;
    }
    child_parts[..root_parts.len()] == root_parts[..]
}

fn expand_user(path: &str) -> String {
    if path == "~" {
        return home_directory().to_string_lossy().to_string();
    }
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => home_directory().join(rest).to_string_lossy().to_string(),
        None => path.to_string(),
    }
}

/// `os.path.expandvars` 的可用子集：`$VAR` / `${VAR}`，Windows 另有 `%VAR%`。
/// 未定义的变量原样保留（与 Python 一致）。
pub fn expand_vars(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let characters: Vec<char> = text.chars().collect();
    let mut index = 0usize;
    while index < characters.len() {
        let current = characters[index];
        if current == '$' && index + 1 < characters.len() {
            let (name, next) = if characters[index + 1] == '{' {
                let mut cursor = index + 2;
                let mut name = String::new();
                while cursor < characters.len() && characters[cursor] != '}' {
                    name.push(characters[cursor]);
                    cursor += 1;
                }
                (name, (cursor + 1).min(characters.len()))
            } else {
                let mut cursor = index + 1;
                let mut name = String::new();
                while cursor < characters.len()
                    && (characters[cursor].is_alphanumeric() || characters[cursor] == '_')
                {
                    name.push(characters[cursor]);
                    cursor += 1;
                }
                (name, cursor)
            };
            if !name.is_empty() {
                if let Ok(value) = std::env::var(&name) {
                    result.push_str(&value);
                    index = next;
                    continue;
                }
            }
            result.push('$');
            index += 1;
            continue;
        }
        if cfg!(windows) && current == '%' {
            let mut cursor = index + 1;
            let mut name = String::new();
            while cursor < characters.len() && characters[cursor] != '%' {
                name.push(characters[cursor]);
                cursor += 1;
            }
            if cursor < characters.len() {
                if let Ok(value) = std::env::var(&name) {
                    result.push_str(&value);
                    index = cursor + 1;
                    continue;
                }
            }
            result.push('%');
            index += 1;
            continue;
        }
        result.push(current);
        index += 1;
    }
    result
}

fn home_directory() -> PathBuf {
    for name in ["HOME", "USERPROFILE"] {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                return PathBuf::from(value);
            }
        }
    }
    std::env::temp_dir()
}

fn realpath(path: &str) -> String {
    match std::fs::canonicalize(path) {
        Ok(resolved) => strip_verbatim(&resolved).to_string_lossy().to_string(),
        Err(_) => path.to_string(),
    }
}

/// 解析成绝对路径：存在时走真实路径解析，不存在时按词法归一（对齐 `Path.resolve(strict=False)`）。
fn resolve_existing_or_lexical(path: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return strip_verbatim(&resolved);
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    lexical_normalize(&absolute)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    result.push("..");
                }
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

/// Windows 的 `canonicalize` 会给出 `\\?\` 前缀，Python 的 `Path.resolve()` 不带；
/// UNC 形态（`\\?\UNC\server\share`）要还原成 `\\server\share`。
fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest.to_string());
    }
    path.to_path_buf()
}

fn create_dir_all(path: &Path) -> Result<(), SessionStoreError> {
    std::fs::create_dir_all(path).map_err(|exc| {
        error(format!(
            "创建目录失败：{}，{}",
            path.display(),
            io_message(&exc)
        ))
    })
}

/// Python 侧把 `OSError` 直接拼进文案；这里保留系统给出的描述文本。
fn io_message(exc: &std::io::Error) -> String {
    exc.to_string()
}
