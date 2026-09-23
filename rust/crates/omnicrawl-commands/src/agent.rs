//! 命令处理器依赖的宿主能力面。
//!
//! Python 侧处理器拿 `CommandContext.agent`（`Any`）按名调用 `LocalToolAgent` 的方法；
//! Rust 把这份能力面收成一个 trait，命令层因此不依赖任何宿主 crate，也不会与
//! `omnicrawl-host` 互相引用。
//!
//! 约定两条：
//!
//! - **不做静默降级**：宿主没有这项能力时显式返回 [`AgentError`]，命令层按 Python 的
//!   同款文案包一层（如「会话列表读取失败：…」）；只有 Python 本身写了兜底的分支
//!   （`skill_metas` / `plugins_status` / `compaction_notice`）才给默认实现。
//! - **返回值与 Python 同形**：会话与提示历史直接复用 `omnicrawl-session` 的条目类型，
//!   子代理任务快照沿用 `omnicrawl-controllers` 的 `Value` 形状（`SubAgentTaskSnapshot::as_dict`），
//!   避免命令层再造一套数据契约。

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::advisor::AdvisorConfig;
use omnicrawl_config::models::llm::LlmConfig;
pub use omnicrawl_controllers::AgentError;
use omnicrawl_extensions::skill::SkillMeta;
use omnicrawl_session::{PromptHistoryEntry, SessionIndexEntry};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// SubAgent 进度事件回调（对映 Python 的 `Callable[[str, dict], None]`）。
///
/// 回调要能随延迟执行体进工作线程，因此要求 `Send + Sync`；调用方用 `Arc` 共享。
pub type SubagentEventCallback = Arc<dyn Fn(&str, &Map<String, Value>) + Send + Sync>;

/// 一次 `/review` 派生的子任务请求。
#[derive(Clone)]
pub struct SubAgentRun {
    pub agent_type: String,
    pub description: String,
    pub prompt: String,
    pub on_event: Option<SubagentEventCallback>,
}

/// 会话快照：`/rename`、`/archive`、`/resume` 用到的展示字段。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionSummary {
    pub session_id: String,
    pub title: String,
    pub message_count: usize,
}

/// 只读 git 探测结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOutput {
    pub code: i32,
    pub stdout: String,
}

/// 斜杠命令处理器依赖的宿主能力面。
///
/// 实现方可以只用 `omnicrawl-host` + `omnicrawl-config` 拼一个宿主后端，也可以由内核
/// （`omnicrawl-cli`）实现——命令层不关心能力从哪来，只要求「缺能力时报错、不撒谎」。
pub trait CommandAgent {
    // ── 配置环境 ──────────────────────────────────────────────

    /// 配置读写的环境注入点：默认取进程真实环境（与 Python 的 runtime 默认值同源）。
    fn config_environment(&self) -> ConfigEnvironment {
        ConfigEnvironment::from_process()
    }

    // ── 会话 ──────────────────────────────────────────────────

    /// 当前会话 ID（无会话时为空串）。
    fn current_session_id(&self) -> String;

    /// 当前工作区根目录。
    fn workspace_root(&self) -> PathBuf;

    /// 清空当前对话并开启新会话。
    fn reset_conversation(&self) -> Result<(), AgentError>;

    /// 重命名当前会话。
    fn rename_current_session(&self, title: &str) -> Result<SessionSummary, AgentError>;

    /// 恢复指定会话。
    fn resume_session(&self, session_id: &str) -> Result<SessionSummary, AgentError>;

    /// 归档当前会话，并立即开启一个新的空会话。
    fn archive_current_session(&self) -> Result<SessionSummary, AgentError>;

    /// 持久化回退最近一轮对话与工作区中被 Git 记录的更改。
    fn undo_last_turn(&self) -> Result<(), AgentError>;

    /// 用结构化摘要模型压缩当前会话上下文。
    fn compact_conversation_model(&self) -> Result<(), AgentError>;

    /// 上一次压缩留下的提示文本（Python 侧是 `_last_compaction_notice`，没有就为空）。
    fn compaction_notice(&self) -> String {
        String::new()
    }

    /// 列出当前工作区最近会话。
    fn list_sessions(&self, limit: usize) -> Result<Vec<SessionIndexEntry>, AgentError>;

    /// 列出当前工作区已归档会话。
    fn list_archived_sessions(&self, limit: usize) -> Result<Vec<SessionIndexEntry>, AgentError>;

    /// 查询当前工作区的用户提示历史（只读展示，不注入模型上下文）。
    fn search_prompt_history(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<PromptHistoryEntry>, AgentError>;

    // ── 主 Agent 模式 ─────────────────────────────────────────

    /// 加载并启用主 Agent 模式模板，返回真正生效的模式标识。
    fn activate_mode(&self, mode: &str) -> Result<String, AgentError>;

    // ── 审批 / 推理强度 / 模型 ────────────────────────────────

    /// 当前工具审批模式。
    fn approval_mode(&self) -> String;

    /// 运行时切换审批模式；持久化由调用方负责写 `config.toml`。
    fn set_approval_mode(&self, mode: &str) -> Result<(), AgentError>;

    /// 当前推理强度（未配置时为 `None`）。
    fn reasoning_effort(&self) -> Option<String>;

    /// 运行时切换推理强度；返回归一化后的档位。
    fn set_reasoning_effort(&self, effort: &str) -> Result<String, AgentError>;

    /// 当前会话用于展示/切换的模型标识。
    ///
    /// 自定义模型优先返回 models.toml key，否则返回真实 model_id；实际请求用
    /// [`CommandAgent::llm_config`] 的 `model`。
    fn current_model(&self) -> String;

    /// 当前模型配置视图（模型切换校验、顾问候选展示用）。
    fn llm_config(&self) -> LlmConfig;

    /// 原子切换运行时模型；`persist` 必须在运行态更新前成功。
    ///
    /// Python 侧是 `set_model(selection, persist=...)`：解析失败即报错，禁止把无效
    /// key 或损坏的配置静默降级成裸 model_id。
    fn set_model(
        &self,
        selection: &str,
        persist: &mut dyn FnMut() -> Result<(), AgentError>,
    ) -> Result<(), AgentError>;

    /// 替换顾问策略配置并重建工具表（advisor 工具据此即时出现/消失）。
    fn apply_advisor_config(&self, config: &AdvisorConfig) -> Result<(), AgentError>;

    // ── 工作区 ────────────────────────────────────────────────

    /// 切换当前工作区，返回切换后的工作区根。
    fn switch_workspace(&self, path: &str) -> Result<PathBuf, AgentError>;

    // ── Skill / 记忆 ──────────────────────────────────────────

    /// 全部已加载 Skill 的元数据；`None` 表示 Skill 子系统未启用。
    fn skill_metas(&self) -> Option<Vec<SkillMeta>> {
        None
    }

    /// 清理过期长期记忆，返回被删除的记忆路径。
    fn clean_memory(&self) -> Result<Vec<String>, AgentError>;

    // ── MCP / 插件 ────────────────────────────────────────────

    /// MCP 子系统状态文本（`/mcp` 展示用）。
    fn mcp_status(&self) -> String;

    /// 插件子系统只读状态；`None` 表示宿主没有插件状态接口。
    ///
    /// 有接口但没注入 PluginManager 时，宿主应返回
    /// `omnicrawl_controllers::control::plugins_status_text(None)` 的结果，与 Python
    /// `format_plugins_status` 的两条分支一一对应。
    fn plugins_status(&self) -> Option<String> {
        None
    }

    // ── SubAgent ──────────────────────────────────────────────

    /// 当前会话可见的后台 SubAgent 任务快照（安全字段，不含原始 prompt 与完整结果）。
    fn list_subagent_tasks(&self) -> Result<Vec<Value>, AgentError>;

    /// 读取当前会话的单个后台 SubAgent 任务；跨会话任务不可见。
    fn get_subagent_task(&self, task_id: &str) -> Result<Option<Value>, AgentError>;

    /// 请求取消当前会话的后台 SubAgent 任务，不等待其最终退出。
    fn cancel_subagent_task(&self, task_id: &str) -> Result<Value, AgentError>;

    /// 派生一个子 Agent 任务并等待其结束，返回子 Agent 输出文本。
    fn run_subagent_task(&self, request: SubAgentRun) -> Result<String, AgentError>;

    /// 把评审报告注入父模型上下文（下一轮请求可见）。
    fn remember_review_report(&self, report: &str) -> Result<(), AgentError>;

    // ── 只读 git 探测 ─────────────────────────────────────────

    /// 在工作区根下执行一次只读 git 命令。
    ///
    /// 实现方按 Python 同口径调用：`git -c color.ui=never --no-pager <args>`、捕获 stdout、
    /// 30 秒超时；git 缺失或超时返回 `None`（Python 两种情况都回落「未找到 git 可执行文件」）。
    fn run_git(&self, workspace_root: &Path, args: &[&str]) -> Option<GitOutput>;
}

#[cfg(test)]
pub(crate) mod testing {
    //! 命令层单测与对照用例共用的宿主替身。
    //!
    //! 只实现命令真正读到的那几项能力：其余方法返回固定的「未启用」，让缺失能力在测试里
    //! 也走显式失败路径，而不是静默成功。

    use super::*;
    use omnicrawl_config::core::runtime::ConfigEnvironment;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// 可配置的宿主替身；`calls` 记录被调用过的方法名，便于断言调用轨迹。
    pub(crate) struct FakeAgent {
        pub approval_mode: String,
        pub workspace: PathBuf,
        pub sessions: Vec<SessionIndexEntry>,
        pub archived: Vec<SessionIndexEntry>,
        pub history: Vec<PromptHistoryEntry>,
        pub skills: Option<Vec<SkillMeta>>,
        pub git: HashMap<String, Option<GitOutput>>,
        pub llm: LlmConfig,
        pub subagent_tasks: Vec<Value>,
        pub task_snapshot: Option<Value>,
        pub cancel_result: Value,
        pub subagent_report: String,
        pub memory_paths: Vec<String>,
        pub mcp_status: String,
        pub plugins_status: Option<String>,
        pub compaction_notice: String,
        pub reasoning_effort: Option<String>,
        pub current_model: String,
        pub session_id: String,
        pub review_report: Mutex<String>,
        pub calls: Mutex<Vec<String>>,
    }

    impl Default for FakeAgent {
        fn default() -> Self {
            let environment = ConfigEnvironment::new(PathBuf::new(), "windows");
            Self {
                approval_mode: "review".to_string(),
                workspace: PathBuf::new(),
                sessions: Vec::new(),
                archived: Vec::new(),
                history: Vec::new(),
                skills: None,
                git: HashMap::new(),
                llm: LlmConfig::with_environment(&environment),
                subagent_tasks: Vec::new(),
                task_snapshot: None,
                cancel_result: Value::Null,
                subagent_report: String::new(),
                memory_paths: Vec::new(),
                mcp_status: String::new(),
                plugins_status: None,
                compaction_notice: String::new(),
                reasoning_effort: None,
                current_model: String::new(),
                session_id: String::new(),
                review_report: Mutex::new(String::new()),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl FakeAgent {
        fn record(&self, name: &str) {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(name.to_string());
            }
        }

        pub(crate) fn calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .map(|calls| calls.clone())
                .unwrap_or_default()
        }

        pub(crate) fn review_report(&self) -> String {
            self.review_report
                .lock()
                .map(|text| text.clone())
                .unwrap_or_default()
        }
    }

    impl CommandAgent for FakeAgent {
        fn current_session_id(&self) -> String {
            self.session_id.clone()
        }

        fn workspace_root(&self) -> PathBuf {
            self.workspace.clone()
        }

        fn reset_conversation(&self) -> Result<(), AgentError> {
            self.record("reset_conversation");
            Ok(())
        }

        fn rename_current_session(&self, title: &str) -> Result<SessionSummary, AgentError> {
            self.record("rename_current_session");
            Ok(SessionSummary {
                session_id: self.session_id.clone(),
                title: title.to_string(),
                message_count: 0,
            })
        }

        fn resume_session(&self, session_id: &str) -> Result<SessionSummary, AgentError> {
            self.record("resume_session");
            Ok(SessionSummary {
                session_id: session_id.to_string(),
                title: "恢复的会话".to_string(),
                message_count: 3,
            })
        }

        fn archive_current_session(&self) -> Result<SessionSummary, AgentError> {
            self.record("archive_current_session");
            Ok(SessionSummary {
                session_id: self.session_id.clone(),
                title: "已归档".to_string(),
                message_count: 1,
            })
        }

        fn undo_last_turn(&self) -> Result<(), AgentError> {
            self.record("undo_last_turn");
            Ok(())
        }

        fn compact_conversation_model(&self) -> Result<(), AgentError> {
            self.record("compact_conversation_model");
            Ok(())
        }

        fn compaction_notice(&self) -> String {
            self.compaction_notice.clone()
        }

        fn list_sessions(&self, limit: usize) -> Result<Vec<SessionIndexEntry>, AgentError> {
            self.record("list_sessions");
            Ok(self.sessions.iter().take(limit).cloned().collect())
        }

        fn list_archived_sessions(
            &self,
            limit: usize,
        ) -> Result<Vec<SessionIndexEntry>, AgentError> {
            self.record("list_archived_sessions");
            Ok(self.archived.iter().take(limit).cloned().collect())
        }

        fn search_prompt_history(
            &self,
            query: &str,
            limit: usize,
        ) -> Result<Vec<PromptHistoryEntry>, AgentError> {
            self.record("search_prompt_history");
            let needle = query.trim();
            Ok(self
                .history
                .iter()
                .filter(|entry| needle.is_empty() || entry.display.contains(needle))
                .take(limit)
                .cloned()
                .collect())
        }

        fn activate_mode(&self, mode: &str) -> Result<String, AgentError> {
            self.record("activate_mode");
            Ok(mode.to_string())
        }

        fn approval_mode(&self) -> String {
            self.approval_mode.clone()
        }

        fn set_approval_mode(&self, mode: &str) -> Result<(), AgentError> {
            self.record("set_approval_mode");
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(format!("set_approval_mode:{mode}"));
            }
            Ok(())
        }

        fn reasoning_effort(&self) -> Option<String> {
            self.reasoning_effort.clone()
        }

        fn set_reasoning_effort(&self, effort: &str) -> Result<String, AgentError> {
            self.record("set_reasoning_effort");
            Ok(effort.trim().to_lowercase())
        }

        fn current_model(&self) -> String {
            self.current_model.clone()
        }

        fn llm_config(&self) -> LlmConfig {
            self.llm.clone()
        }

        fn set_model(
            &self,
            selection: &str,
            persist: &mut dyn FnMut() -> Result<(), AgentError>,
        ) -> Result<(), AgentError> {
            self.record("set_model");
            persist()?;
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(format!("set_model:{selection}"));
            }
            Ok(())
        }

        fn apply_advisor_config(&self, config: &AdvisorConfig) -> Result<(), AgentError> {
            self.record("apply_advisor_config");
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(format!("advisor_enabled:{}", config.enabled));
            }
            Ok(())
        }

        fn switch_workspace(&self, path: &str) -> Result<PathBuf, AgentError> {
            self.record("switch_workspace");
            Ok(PathBuf::from(path))
        }

        fn skill_metas(&self) -> Option<Vec<SkillMeta>> {
            self.skills.clone()
        }

        fn clean_memory(&self) -> Result<Vec<String>, AgentError> {
            self.record("clean_memory");
            Ok(self.memory_paths.clone())
        }

        fn mcp_status(&self) -> String {
            self.mcp_status.clone()
        }

        fn plugins_status(&self) -> Option<String> {
            self.plugins_status.clone()
        }

        fn list_subagent_tasks(&self) -> Result<Vec<Value>, AgentError> {
            self.record("list_subagent_tasks");
            Ok(self.subagent_tasks.clone())
        }

        fn get_subagent_task(&self, _task_id: &str) -> Result<Option<Value>, AgentError> {
            self.record("get_subagent_task");
            Ok(self.task_snapshot.clone())
        }

        fn cancel_subagent_task(&self, _task_id: &str) -> Result<Value, AgentError> {
            self.record("cancel_subagent_task");
            Ok(self.cancel_result.clone())
        }

        fn run_subagent_task(&self, request: SubAgentRun) -> Result<String, AgentError> {
            self.record("run_subagent_task");
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(format!("subagent_type:{}", request.agent_type));
                calls.push(format!("subagent_prompt:{}", request.prompt));
            }
            Ok(self.subagent_report.clone())
        }

        fn remember_review_report(&self, report: &str) -> Result<(), AgentError> {
            self.record("remember_review_report");
            if let Ok(mut slot) = self.review_report.lock() {
                *slot = report.to_string();
            }
            Ok(())
        }

        fn run_git(&self, _workspace_root: &Path, args: &[&str]) -> Option<GitOutput> {
            self.git.get(&args.join(" ")).cloned().unwrap_or(None)
        }
    }
}
