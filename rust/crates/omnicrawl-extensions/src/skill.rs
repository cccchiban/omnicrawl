//! `omnicrawl/extensions/skill.py` 的 Rust 移植：Skill 全生命周期
//! 「发现 → 索引 → 匹配 → 注入」。
//!
//! 两层架构与 Python 一致：本模块既放纯解析/校验函数，也放带状态的 `SkillManager`
//! （路径管理、来源标记、去重与同名冲突诊断）。
//!
//! 渐进式披露的格式与 Python 逐字对齐，因为它直接进模型上下文。

use crate::bm25;
use crate::path::{expand_user, home_directory, resolve_path};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const MAX_NAME_LENGTH: usize = 64;
pub const MAX_DESCRIPTION_LENGTH: usize = 1024;
pub const SKILL_FILE_NAME: &str = "SKILL.md";
pub const SKILL_MANIFEST_FILE_NAME: &str = "manifest.json";
pub const DEFAULT_MATCH_THRESHOLD: f64 = 0.3;
/// 默认扫描的作用域目录（优先级从低到高，后加载的覆盖先加载的）。
pub const SKILL_SCOPES: [(&str, &str); 3] = [
    ("enterprise", "OMNICRAWL_ENTERPRISE_DIR"),
    ("user", "~/.omnicrawl/skills"),
    ("project", ".omnicrawl/skills"),
];

/// 检索可用的作用域取值；`all` 表示不按作用域过滤。
pub const SEARCH_SCOPES: [&str; 5] = ["all", "project", "user", "enterprise", "path"];

/// 企业级 skills 目录的环境变量名（由管理员显式指定，不自动创建）。
pub const ENTERPRISE_SKILL_DIR_ENV: &str = "OMNICRAWL_ENTERPRISE_DIR";

/// Skill 加载过程中产生的警告或冲突信息。
#[derive(Debug, Clone, PartialEq)]
pub struct SkillDiagnostic {
    /// `"warning"` | `"collision"`；字段名与 Python 的 `type` 对齐。
    pub kind: String,
    pub message: String,
    pub path: String,
    pub collision: Option<Map<String, Value>>,
}

impl SkillDiagnostic {
    pub fn warning(message: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            kind: "warning".to_string(),
            message: message.into(),
            path: path.into(),
            collision: None,
        }
    }

    pub fn to_dict(&self) -> Map<String, Value> {
        let mut data = Map::new();
        data.insert("type".to_string(), Value::from(self.kind.clone()));
        data.insert("message".to_string(), Value::from(self.message.clone()));
        data.insert("path".to_string(), Value::from(self.path.clone()));
        data.insert(
            "collision".to_string(),
            match &self.collision {
                Some(value) => Value::Object(value.clone()),
                None => Value::Null,
            },
        );
        data
    }
}

/// Skill 轻量级索引条目——仅含元数据，不加载正文（渐进式披露）。
#[derive(Debug, Clone, PartialEq)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    /// SKILL.md 绝对路径。
    pub source_path: PathBuf,
    /// Skill 根目录（解析相对路径时使用）。
    pub base_dir: PathBuf,
    /// `"project"` | `"user"` | `"enterprise"` | `"path"`。
    pub scope: String,
    pub disable_model_invocation: bool,
}

impl SkillMeta {
    pub fn to_dict(&self) -> Map<String, Value> {
        let mut data = Map::new();
        data.insert("name".to_string(), Value::from(self.name.clone()));
        data.insert(
            "description".to_string(),
            Value::from(self.description.clone()),
        );
        data.insert(
            "source_path".to_string(),
            Value::from(self.source_path.to_string_lossy().to_string()),
        );
        data.insert(
            "base_dir".to_string(),
            Value::from(self.base_dir.to_string_lossy().to_string()),
        );
        data.insert("scope".to_string(), Value::from(self.scope.clone()));
        data.insert(
            "disable_model_invocation".to_string(),
            Value::from(self.disable_model_invocation),
        );
        data
    }
}

/// 完整 Skill，包含加载后的指令正文。
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub meta: SkillMeta,
    /// YAML frontmatter 之后的 Markdown 正文。
    pub body: String,
}

impl Skill {
    pub fn to_dict(&self) -> Map<String, Value> {
        let mut data = Map::new();
        data.insert("meta".to_string(), Value::Object(self.meta.to_dict()));
        data.insert("body".to_string(), Value::from(self.body.clone()));
        data
    }
}

/// 一次只读检索的命中项：索引元数据与关键词得分（不加载正文）。
#[derive(Debug, Clone, PartialEq)]
pub struct SkillSearchHit {
    pub meta: SkillMeta,
    pub score: f64,
    pub reason: String,
}

/// 匹配结果。
#[derive(Debug, Clone, PartialEq)]
pub struct SkillMatchResult {
    pub skill: Skill,
    pub score: f64,
    pub reason: String,
}

impl SkillMatchResult {
    pub fn to_dict(&self) -> Map<String, Value> {
        let mut data = Map::new();
        data.insert("skill".to_string(), Value::Object(self.skill.to_dict()));
        data.insert("score".to_string(), Value::from(self.score));
        data.insert("reason".to_string(), Value::from(self.reason.clone()));
        data
    }
}

/// 校验 Skill 名称，返回错误列表（空列表 = 有效）。
pub fn validate_skill_name(name: &str) -> Vec<String> {
    let mut errors: Vec<String> = Vec::new();
    let length = name.chars().count();
    if length > MAX_NAME_LENGTH {
        errors.push(format!(
            "名称超过 {MAX_NAME_LENGTH} 字符（当前 {length} 字符）"
        ));
    }
    if name.is_empty()
        || !name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        errors.push("名称只能包含小写字母 a-z、数字 0-9、连字符".to_string());
    }
    if name.starts_with('-') || name.ends_with('-') {
        errors.push("名称不能以连字符开头或结尾".to_string());
    }
    if name.contains("--") {
        errors.push("名称不能包含连续连字符".to_string());
    }
    errors
}

/// 校验描述，返回错误列表。
pub fn validate_skill_description(description: &str) -> Vec<String> {
    let mut errors: Vec<String> = Vec::new();
    if description.trim().is_empty() {
        errors.push("description 是必填字段".to_string());
    } else {
        let length = description.chars().count();
        if length > MAX_DESCRIPTION_LENGTH {
            errors.push(format!(
                "描述超过 {MAX_DESCRIPTION_LENGTH} 字符（当前 {length} 字符）"
            ));
        }
    }
    errors
}

/// Skill 全生命周期管理：发现 → 索引 → 匹配 → 注入。
#[derive(Debug, Default)]
pub struct SkillManager {
    /// 保插入序：`match` 的候选顺序与 Python 的 dict 迭代序一致。
    index: Vec<(String, SkillMeta)>,
    body_cache: Vec<(String, String)>,
    diagnostics: Vec<SkillDiagnostic>,
    real_paths: HashSet<String>,
    last_cwd: Option<PathBuf>,
    last_extra_paths: Vec<String>,
    last_enterprise_dir: Option<String>,
}

impl SkillManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// 遍历所有作用域目录，构建 Skill 索引。
    ///
    /// 扫描顺序：企业级 → 个人级 → 项目级 → 额外路径。同名 Skill **保留先扫描到的**
    /// （企业级优先于个人级，个人级优先于项目级），并在 `diagnostics` 里留一条碰撞记录；
    /// 这与 Python `_add_skill` 的语义一致。
    pub fn discover(&mut self, cwd: Option<&Path>, extra_paths: &[String]) {
        let enterprise_dir = std::env::var(ENTERPRISE_SKILL_DIR_ENV)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        self.discover_with_enterprise(cwd, extra_paths, enterprise_dir.as_deref());
    }

    /// 与 [`Self::discover`] 同路径，但企业级目录由调用方显式给出（测试与宿主注入用）。
    pub fn discover_with_enterprise(
        &mut self,
        cwd: Option<&Path>,
        extra_paths: &[String],
        enterprise_dir: Option<&str>,
    ) {
        self.index.clear();
        self.body_cache.clear();
        self.diagnostics.clear();
        self.real_paths.clear();
        let work_dir = match cwd {
            Some(path) => resolve_path(path),
            None => match std::env::current_dir() {
                Ok(current) => resolve_path(&current),
                Err(_) => PathBuf::from("."),
            },
        };
        self.last_cwd = Some(work_dir.clone());
        self.last_extra_paths = extra_paths.to_vec();
        self.last_enterprise_dir = enterprise_dir.map(|value| value.to_string());
        let home_dir = home_directory();

        for (scope, _path_spec) in SKILL_SCOPES {
            match scope {
                // 企业级目录由环境变量显式指定，只读不建。
                "enterprise" => {
                    if let Some(raw) = enterprise_dir {
                        let expanded = expand_user(raw);
                        let candidate = if expanded.is_absolute() {
                            expanded
                        } else {
                            work_dir.join(expanded)
                        };
                        self.scan_directory(&resolve_path(&candidate), scope);
                    }
                }
                "user" => {
                    let user_skills = home_dir.join(".omnicrawl").join("skills");
                    ensure_scope_dir(&user_skills);
                    self.scan_directory(&user_skills, scope);
                }
                "project" => {
                    let project_skills = work_dir.join(".omnicrawl").join("skills");
                    ensure_scope_dir(&project_skills);
                    self.scan_directory(&project_skills, scope);
                }
                _ => {}
            }
        }

        for raw_path in extra_paths {
            let expanded = expand_user(raw_path);
            let candidate = if expanded.is_absolute() {
                expanded
            } else {
                work_dir.join(expanded)
            };
            let resolved = resolve_path(&candidate);
            if !resolved.exists() {
                self.diagnostics.push(SkillDiagnostic::warning(
                    "Skill 路径不存在",
                    resolved.to_string_lossy().to_string(),
                ));
                continue;
            }
            if resolved.is_dir() {
                self.scan_directory(&resolved, "path");
            } else if resolved.is_file() && has_md_suffix(&resolved) {
                if let Some((meta, body)) = self.parse_skill_file(&resolved, "path") {
                    self.add_skill(meta, body);
                }
            } else {
                self.diagnostics.push(SkillDiagnostic::warning(
                    "Skill 路径不是目录或 .md 文件",
                    resolved.to_string_lossy().to_string(),
                ));
            }
        }
    }

    /// 递归扫描目录下的 SKILL.md 和根级 .md 文件。
    ///
    /// 规则：找到 SKILL.md 就不再递归进入该子目录；跳过 `.` 开头目录；
    /// 根目录下的 .md 文件（非 SKILL.md）也作为独立 Skill 发现。
    fn scan_directory(&mut self, dir_path: &Path, scope: &str) {
        if !dir_path.is_dir() {
            return;
        }
        let entries = match std::fs::read_dir(dir_path) {
            Ok(value) => value,
            Err(_) => return,
        };
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            paths.push(entry.path());
        }
        paths.sort_by_key(|path| {
            path.file_name()
                .map(|item| item.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        });

        for path in paths {
            let name = path
                .file_name()
                .map(|item| item.to_string_lossy().to_string())
                .unwrap_or_default();
            if name.starts_with('.') {
                continue;
            }
            let entry = if is_symlink(&path) {
                match std::fs::canonicalize(&path) {
                    Ok(target) => target,
                    Err(_) => continue,
                }
            } else {
                path.clone()
            };
            if entry.is_dir() {
                let skill_md = entry.join(SKILL_FILE_NAME);
                if skill_md.is_file() {
                    if let Some((meta, body)) = self.parse_skill_file(&skill_md, scope) {
                        self.add_skill(meta, body);
                    }
                    // 找到 SKILL.md → 不再递归深入。
                    continue;
                }
                self.scan_directory(&entry, scope);
            } else if entry.is_file() && has_md_suffix(&entry) && name != SKILL_FILE_NAME {
                if let Some((meta, body)) = self.parse_skill_file(&entry, scope) {
                    self.add_skill(meta, body);
                }
            }
        }
    }

    /// 添加 Skill 到索引，处理符号链接去重和同名冲突。
    fn add_skill(&mut self, meta: SkillMeta, body: String) {
        let real_key = match std::fs::canonicalize(&meta.source_path) {
            Ok(value) => value.to_string_lossy().to_string(),
            Err(_) => meta.source_path.to_string_lossy().to_string(),
        };
        if self.real_paths.contains(&real_key) {
            return;
        }
        self.real_paths.insert(real_key);

        if let Some((_, existing)) = self.index.iter().find(|(name, _)| *name == meta.name) {
            // 同名：保留先扫描到的（企业级 > 个人级 > 项目级 > 额外路径），
            // 与 Python `_add_skill` 的语义一致；碰撞记录里 winner 是已索引的、loser 是新条目。
            let mut collision = Map::new();
            collision.insert("resourceType".to_string(), Value::from("skill"));
            collision.insert("name".to_string(), Value::from(meta.name.clone()));
            collision.insert(
                "winnerPath".to_string(),
                Value::from(existing.source_path.to_string_lossy().to_string()),
            );
            collision.insert(
                "loserPath".to_string(),
                Value::from(meta.source_path.to_string_lossy().to_string()),
            );
            self.diagnostics.push(SkillDiagnostic {
                kind: "collision".to_string(),
                message: format!("Skill 名称 \"{}\" 冲突", meta.name),
                path: meta.source_path.to_string_lossy().to_string(),
                collision: Some(collision),
            });
            return;
        }

        self.body_cache.push((meta.name.clone(), body));
        self.index.push((meta.name.clone(), meta));
    }

    /// 解析 SKILL.md 的 YAML frontmatter；缺少必填字段或读取失败时返回 None。
    fn parse_skill_file(&mut self, file_path: &Path, scope: &str) -> Option<(SkillMeta, String)> {
        let raw = std::fs::read_to_string(file_path).ok()?;
        let (frontmatter, body) = parse_frontmatter(&raw);
        let manifest = read_manifest(file_path.parent().unwrap_or(Path::new(".")));
        let parent_name = file_path
            .parent()
            .and_then(|item| item.file_name())
            .map(|item| item.to_string_lossy().to_string())
            .unwrap_or_default();

        let mut name = {
            let from_frontmatter = frontmatter_text(&frontmatter, "name");
            if !from_frontmatter.is_empty() {
                from_frontmatter
            } else {
                let from_manifest = manifest_text(&manifest, "skill_name");
                if !from_manifest.is_empty() {
                    from_manifest
                } else {
                    parent_name
                }
            }
        };
        let normalized_name = normalize_skill_name(&name);
        if normalized_name != name {
            self.diagnostics.push(SkillDiagnostic::warning(
                format!("Skill 名称 \"{name}\" 已兼容为 \"{normalized_name}\""),
                file_path.to_string_lossy().to_string(),
            ));
            name = normalized_name;
        }

        let file_name = file_path
            .file_name()
            .map(|item| item.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut description = frontmatter_text(&frontmatter, "description");
        if description.is_empty() && file_name == SKILL_FILE_NAME {
            description = infer_description(&raw, &body);
            if !description.is_empty() {
                self.diagnostics.push(SkillDiagnostic::warning(
                    "Skill 缺少 description，已从 Markdown 内容推断",
                    file_path.to_string_lossy().to_string(),
                ));
            }
        }

        for error in validate_skill_name(&name) {
            self.diagnostics.push(SkillDiagnostic::warning(
                error,
                file_path.to_string_lossy().to_string(),
            ));
        }
        for error in validate_skill_description(&description) {
            self.diagnostics.push(SkillDiagnostic::warning(
                error,
                file_path.to_string_lossy().to_string(),
            ));
        }
        // 缺少 description → 不加载。
        if description.is_empty() {
            return None;
        }

        let disable_model = match frontmatter.get("disable-model-invocation") {
            Some(Value::String(text)) => text.to_lowercase() == "true",
            Some(Value::Bool(flag)) => *flag,
            Some(Value::Number(number)) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
            _ => false,
        };

        let meta = SkillMeta {
            name,
            description,
            source_path: resolve_path(file_path),
            base_dir: resolve_path(file_path.parent().unwrap_or(Path::new("."))),
            scope: scope.to_string(),
            disable_model_invocation: disable_model,
        };
        Some((meta, body))
    }

    /// 基于用户输入与 Skill description 的关键词匹配；`disable-model-invocation` 的技能不参与。
    pub fn match_skills(
        &self,
        user_input: &str,
        max_results: usize,
        threshold: f64,
    ) -> Vec<SkillMatchResult> {
        let input_lower = user_input.to_lowercase();
        let mut results: Vec<SkillMatchResult> = Vec::new();
        for (_, meta) in &self.index {
            if meta.disable_model_invocation {
                continue;
            }
            let (score, reason) = score_match(&input_lower, meta);
            if score >= threshold {
                results.push(SkillMatchResult {
                    skill: self.load_skill(meta),
                    score,
                    reason,
                });
            }
        }
        results.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(max_results);
        results
    }

    /// 按名称精确获取 Skill；索引未命中时自动 re-discover 一次，
    /// 覆盖 AI 在当前会话内安装新 Skill 的场景。
    pub fn match_by_name(&mut self, name: &str) -> Option<Skill> {
        let mut meta = self
            .index
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, item)| item.clone());
        if meta.is_none() {
            self.reload();
            meta = self
                .index
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, item)| item.clone());
        }
        meta.as_ref().map(|item| self.load_skill(item))
    }

    /// 重新扫描所有作用域目录，捕获新增/变更的 Skill（保留当前 cwd）。
    pub fn reload(&mut self) {
        let cwd = self.last_cwd.clone();
        let extra = self.last_extra_paths.clone();
        let enterprise = self.last_enterprise_dir.clone();
        self.discover_with_enterprise(cwd.as_deref(), &extra, enterprise.as_deref());
    }

    /// 只读检索索引元数据：按作用域过滤后用 BM25 打分，命中的条目按名称排序取前若干条。
    ///
    /// `scope` 取 [`SEARCH_SCOPES`] 中的取值（`all` 表示不过滤）。检索单元是 Skill 的
    /// **名称 + 描述 + 正文**：名称与描述判断「这个 Skill 是做什么的」，正文决定「它具体
    /// 覆盖哪些细节」，只按元数据检索会漏掉「描述写得泛、正文才写清楚」的 Skill。
    ///
    /// `user_input` 为空时退化为「按作用域列出全部」，此时不打分也不设门槛，便于模型先
    /// 浏览再决定读哪个正文。
    pub fn search_meta(
        &self,
        user_input: &str,
        scope: &str,
        max_results: usize,
    ) -> Vec<SkillSearchHit> {
        let scoped: Vec<&SkillMeta> = self
            .index
            .iter()
            .map(|(_, meta)| meta)
            .filter(|meta| scope == "all" || meta.scope == scope)
            .collect();
        if user_input.trim().is_empty() {
            let mut listed: Vec<SkillSearchHit> = scoped
                .into_iter()
                .map(|meta| SkillSearchHit {
                    meta: meta.clone(),
                    score: 0.0,
                    reason: "未给关键词，按作用域列出".to_string(),
                })
                .collect();
            listed.sort_by(|left, right| left.meta.name.cmp(&right.meta.name));
            listed.truncate(max_results);
            return listed;
        }

        let documents: Vec<Vec<String>> = scoped
            .iter()
            .map(|meta| self.search_tokens(meta))
            .collect();
        let hits = bm25::rank(&bm25::tokenize(user_input), &documents);
        let mut ranked: Vec<SkillSearchHit> = hits
            .into_iter()
            .map(|hit| SkillSearchHit {
                meta: scoped[hit.index].clone(),
                score: hit.score,
                reason: format!("BM25 命中：{}", hit.matched.join(", ")),
            })
            .collect();
        ranked.sort_by(|left, right| left.meta.name.cmp(&right.meta.name));
        ranked.truncate(max_results);
        ranked
    }

    /// 一个 Skill 的检索 token：名称、描述与正文一起分词。
    fn search_tokens(&self, meta: &SkillMeta) -> Vec<String> {
        let body = self
            .body_cache
            .iter()
            .find(|(name, _)| name == &meta.name)
            .map(|(_, body)| body.as_str())
            .unwrap_or_default();
        bm25::tokenize(&format!(
            "{} {} {}",
            meta.name.replace('-', " "),
            meta.description,
            body
        ))
    }

    /// 兼容旧调用：Skill 指令不再注入 system prompt，原样返回。
    pub fn inject(&self, system_prompt: &str) -> String {
        system_prompt.to_string()
    }

    /// 返回所有已索引的 Skill 元数据（按名称排序）。
    pub fn list_all(&self) -> Vec<SkillMeta> {
        let mut items: Vec<SkillMeta> = self.index.iter().map(|(_, meta)| meta.clone()).collect();
        items.sort_by(|left, right| left.name.cmp(&right.name));
        items
    }

    /// 按名称获取完整 Skill。
    pub fn get(&self, name: &str) -> Option<Skill> {
        self.index
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, meta)| self.load_skill(meta))
    }

    pub fn diagnostics(&self) -> Vec<SkillDiagnostic> {
        self.diagnostics.clone()
    }

    pub fn count(&self) -> usize {
        self.index.len()
    }

    /// 获取完整 Skill 正文，优先使用缓存的 body。
    fn load_skill(&self, meta: &SkillMeta) -> Skill {
        let body = self
            .body_cache
            .iter()
            .find(|(name, _)| name == &meta.name)
            .map(|(_, body)| body.clone())
            .unwrap_or_default();
        Skill {
            meta: meta.clone(),
            body,
        }
    }
}

/// 确保作用域目录存在；创建失败时保持静默降级。
fn ensure_scope_dir(dir_path: &Path) {
    let _ = std::fs::create_dir_all(dir_path);
}

fn has_md_suffix(path: &Path) -> bool {
    path.extension().map(|item| item == "md").unwrap_or(false)
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

/// 解析 YAML frontmatter，返回 (frontmatter, body)；无 frontmatter 时返回空表与原文。
pub fn parse_frontmatter(content: &str) -> (Map<String, Value>, String) {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.starts_with("---") {
        return (Map::new(), normalized);
    }
    let Some(end_index) = normalized[3..].find("\n---").map(|index| index + 3) else {
        return (Map::new(), normalized);
    };
    let yaml_str = &normalized[4..end_index];
    let body = normalized[end_index + 4..].trim().to_string();

    let mut result = Map::new();
    let lines: Vec<&str> = yaml_str.split('\n').collect();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].trim();
        index += 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = split_frontmatter_line(line) else {
            continue;
        };
        let value = value.trim();
        // 块标量（`>-` / `>` / `|-` / `|`）：值在后续缩进行里，逐行收进来。
        if let Some(style) = block_scalar_style(value) {
            let mut collected: Vec<String> = Vec::new();
            while index < lines.len() {
                let raw = lines[index];
                if raw.trim().is_empty() {
                    collected.push(String::new());
                    index += 1;
                    continue;
                }
                if !raw.starts_with(' ') && !raw.starts_with('\t') {
                    break;
                }
                collected.push(raw.trim().to_string());
                index += 1;
            }
            // 收尾空行不属于内容（`-` 截断指示符的常见写法）。
            while collected.last().is_some_and(|item| item.is_empty()) {
                collected.pop();
            }
            result.insert(key, Value::from(join_block_scalar(&collected, style)));
            continue;
        }
        let lowered = value.to_lowercase();
        if lowered == "true" || lowered == "false" {
            result.insert(key, Value::from(lowered == "true"));
            continue;
        }
        let chars: Vec<char> = value.chars().collect();
        if chars.len() >= 2
            && chars[0] == chars[chars.len() - 1]
            && (chars[0] == '"' || chars[0] == '\'')
        {
            let inner: String = chars[1..chars.len() - 1].iter().collect();
            result.insert(key, Value::from(inner));
            continue;
        }
        result.insert(key, Value::from(value));
    }
    (result, body)
}

/// 识别块标量指示符，返回 `>`（折叠）或 `|`（保留换行）。
fn block_scalar_style(value: &str) -> Option<char> {
    let style = value.chars().next()?;
    if style != '>' && style != '|' {
        return None;
    }
    // 只允许 `>`、`>-`、`>+`、`|`、`|-`、`|+` 这几种写法。
    let indicator = value[style.len_utf8()..].trim();
    if indicator.is_empty() || indicator == "-" || indicator == "+" {
        Some(style)
    } else {
        None
    }
}

/// 块标量正文：`>` 折叠成一行（空行分段），`|` 逐行保留。
fn join_block_scalar(lines: &[String], style: char) -> String {
    if style == '|' {
        return lines.join("\n");
    }
    let mut paragraphs: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in lines {
        if line.is_empty() {
            if !current.is_empty() {
                paragraphs.push(current.join(" "));
                current.clear();
            }
            continue;
        }
        current.push(line.as_str());
    }
    if !current.is_empty() {
        paragraphs.push(current.join(" "));
    }
    paragraphs.join("\n")
}

/// `^([a-zA-Z][\w-]*):\s*(.*)`：首字符必须是 ASCII 字母。
fn split_frontmatter_line(line: &str) -> Option<(String, String)> {
    let mut chars = line.char_indices();
    let (_, first) = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    let mut head_end = first.len_utf8();
    for (index, ch) in chars {
        if ch.is_alphanumeric() || ch == '_' || ch == '-' {
            head_end = index + ch.len_utf8();
            continue;
        }
        break;
    }
    let rest = &line[head_end..];
    let body = rest.strip_prefix(':')?;
    Some((line[..head_end].to_string(), body.to_string()))
}

fn read_manifest(skill_dir: &Path) -> Map<String, Value> {
    let manifest_path = skill_dir.join(SKILL_MANIFEST_FILE_NAME);
    if !manifest_path.is_file() {
        return Map::new();
    }
    let Ok(text) = std::fs::read_to_string(&manifest_path) else {
        return Map::new();
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// frontmatter 里取字符串：布尔按 Python 的 `str()` 写法回落。
fn frontmatter_text(frontmatter: &Map<String, Value>, key: &str) -> String {
    value_text(frontmatter.get(key)).trim().to_string()
}

fn manifest_text(manifest: &Map<String, Value>, key: &str) -> String {
    value_text(manifest.get(key)).trim().to_string()
}

fn value_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(flag)) => if *flag { "True" } else { "False" }.to_string(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

/// `re.sub(r"[^a-z0-9-]+", "-", name.strip().lower())` + 折叠连字符 + 去首尾。
fn normalize_skill_name(name: &str) -> String {
    let lowered = name.trim().to_lowercase();
    let filtered: String = lowered
        .chars()
        .map(|ch| {
            if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let mut collapsed = String::with_capacity(filtered.len());
    let mut previous_dash = false;
    for ch in filtered.chars() {
        if ch == '-' {
            if previous_dash {
                continue;
            }
            previous_dash = true;
        } else {
            previous_dash = false;
        }
        collapsed.push(ch);
    }
    let trimmed = collapsed.trim_matches('-').to_string();
    if trimmed.is_empty() {
        name.to_string()
    } else {
        trimmed
    }
}

/// 从正文推断 description：先找标题，再找第一行有内容的正文。
pub fn infer_description(raw: &str, body: &str) -> String {
    let source = if body.is_empty() { raw } else { body };
    let lines: Vec<String> = source.lines().map(|line| line.trim().to_string()).collect();

    for line in &lines {
        if !line.starts_with('#') {
            continue;
        }
        let heading = line.trim_start_matches('#').trim();
        if !heading.is_empty() {
            return take_chars(heading, MAX_DESCRIPTION_LENGTH);
        }
    }

    for line in &lines {
        if line.is_empty() || line.starts_with("---") {
            continue;
        }
        let clean = strip_leading_markers(line);
        if !clean.is_empty() {
            return take_chars(&clean, MAX_DESCRIPTION_LENGTH);
        }
    }
    String::new()
}

/// `re.sub(r"^[>\-\*\d、.()\s]+", "", line)`：剥掉行首的引用/列表/序号标记。
fn strip_leading_markers(line: &str) -> String {
    let mut text = line;
    loop {
        let mut chars = text.chars();
        let Some(first) = chars.next() else { break };
        let is_marker = first == '>'
            || first == '-'
            || first == '*'
            || first == '、'
            || first == '.'
            || first == '('
            || first == ')'
            || first.is_ascii_digit()
            || first.is_whitespace();
        if !is_marker {
            break;
        }
        text = &text[first.len_utf8()..];
    }
    text.trim().to_string()
}

fn take_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// 多级打分：名称整体命中 → 名称分词 → 描述关键词子串 → Jaccard 相似度。
fn score_match(input_lower: &str, meta: &SkillMeta) -> (f64, String) {
    let mut reasons: Vec<String> = Vec::new();

    let name_parts = meta.name.replace('-', " ");
    if input_lower.contains(&name_parts) {
        return (1.0, format!("Skill 名称命中：{}", meta.name));
    }

    let name_keywords: HashSet<String> = name_parts
        .split_whitespace()
        .map(|item| item.to_string())
        .collect();
    let mut name_hits: Vec<String> = name_keywords
        .iter()
        .filter(|item| item.chars().count() >= 2 && input_lower.contains(item.as_str()))
        .cloned()
        .collect();
    name_hits.sort();
    let name_bonus = name_hits.len() as f64 * 0.25;
    if !name_hits.is_empty() {
        reasons.push(format!("名称分词命中：{}", name_hits.join(", ")));
    }

    let desc_lower = meta.description.to_lowercase();
    let desc_keywords = extract_keywords(&desc_lower);
    let input_keywords = extract_keywords(input_lower);

    if desc_keywords.is_empty() && name_keywords.is_empty() {
        return (0.0, "无有效关键词".to_string());
    }

    let intersection: HashSet<&String> = desc_keywords.intersection(&input_keywords).collect();
    let union: HashSet<&String> = desc_keywords.union(&input_keywords).collect();
    let jaccard = if union.is_empty() {
        0.0
    } else {
        intersection.len() as f64 / union.len() as f64
    };

    // 按排序后的顺序累加：Python 的 set 迭代序受哈希随机化影响，排序可保证两侧一致。
    let mut sorted_desc: Vec<&String> = desc_keywords.iter().collect();
    sorted_desc.sort();
    let mut substring_bonus = 0.0;
    for keyword in sorted_desc {
        if keyword.chars().count() >= 2 && input_lower.contains(keyword.as_str()) {
            substring_bonus += 0.15;
        }
    }

    let score = (jaccard + substring_bonus + name_bonus).min(1.0);
    if !intersection.is_empty() {
        let mut hits: Vec<&String> = intersection.into_iter().collect();
        hits.sort();
        let text: Vec<String> = hits.iter().map(|item| (*item).clone()).collect();
        reasons.push(format!("关键词命中：{}", text.join(", ")));
    }

    let reason = if reasons.is_empty() {
        "低相关度".to_string()
    } else {
        reasons.join("; ")
    };
    (score, reason)
}

/// 从文本中提取中文和英文关键词：CJK 连续片段按 2-4 字切块，英文取长度 ≥3 的单词。
fn extract_keywords(text: &str) -> HashSet<String> {
    let mut words: HashSet<String> = HashSet::new();
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if is_cjk(chars[index]) {
            let start = index;
            while index < chars.len() && is_cjk(chars[index]) {
                index += 1;
            }
            let run = &chars[start..index];
            let mut offset = 0;
            while offset < run.len() {
                let take = (run.len() - offset).min(4);
                if take < 2 {
                    break;
                }
                words.insert(run[offset..offset + take].iter().collect());
                offset += take;
            }
        } else {
            index += 1;
        }
    }

    let mut run = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphabetic() {
            run.push(ch);
            continue;
        }
        if run.len() >= 3 {
            words.insert(run.to_lowercase());
        }
        run.clear();
    }
    if run.len() >= 3 {
        words.insert(run.to_lowercase());
    }
    words
}

/// `[一-鿿]`：CJK 统一表意文字基本区。
fn is_cjk(ch: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&ch)
}
