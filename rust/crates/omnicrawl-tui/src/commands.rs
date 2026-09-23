//! 斜杠命令的 TUI 宿主接线：命令层看到的能力面（[`CommandAgent`]）与候选表。
//!
//! 命令层（`omnicrawl-commands`）不认宿主，处理器要的宿主能力全经 [`CommandAgent`]
//! 注入。本模块按「**有什么能力就报什么、没有就显式报错**」实现这份能力面：
//!
//! - 本进程真正拥有的能力（审批模式、模型与推理强度、插件状态、出任务查询、
//!   工作区根、只读 git 探测）如实实现；
//! - 会话生命周期（`/sessions`、`/archives`、`/history`、`/rename`、`/new`、`/archive`、
//!   `/resume`）由宿主各走一次内核往返：会话状态在内核进程里，宿主只负责投影与视图同步；
//! - 能力在内核侧、协议又没有对应方法的（工作区切换、MCP 清单、顾问工具表、计划模式、
//!   长期记忆）返回带原因的 [`AgentError`]，不再让这些输入悄悄变成一次模型对话；
//! - `/undo`、`/compact` 与 `/review` 由宿主在进命令层之前异步下发内核（见 `app.rs` 的
//!   `KernelCommand`），因此这里的对应方法不会被调用，只能报「走异步路径」。
//!
//! 命令处理器通过 `RefCell` 借用宿主：处理器不会回调宿主，借用不跨调用边界，
//! 因此不存在重入风险；每次借用都尽量收窄到一条语句。

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use omnicrawl_commands::agent::{AgentError, CommandAgent, GitOutput, SessionSummary, SubAgentRun};
use omnicrawl_commands::framework::CommandOption;
use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::advisor::AdvisorConfig;
use omnicrawl_config::models::llm::LlmConfig;
use omnicrawl_controllers::control::PluginWorkerRow;
use omnicrawl_extensions::skill::SkillMeta;
use omnicrawl_session::{PromptHistoryEntry, SessionIndexEntry};
use serde_json::{Map, Value};

use crate::app::App;

/// 只读 git 探测的超时（与 Python 的 30 秒一致）。
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
/// git 子进程的等待轮询间隔。
const GIT_POLL: Duration = Duration::from_millis(20);

/// 命令菜单的候选表：统一命令源（内置命令与它们的补全形态）。
///
/// 运行期 Skill 候选由内核侧发现，宿主暂时给不出同一份列表，因此菜单只列内置命令；
/// 命令名与参数提示都来自同一份注册表声明，帮助、补全与真实可执行命令不会各自硬编码。
pub fn command_options() -> Vec<CommandOption> {
    omnicrawl_commands::slash::registry().options(false)
}

/// 命令层看到的宿主能力面。
pub struct TuiHostAgent<'a> {
    app: RefCell<&'a mut App>,
}

impl<'a> TuiHostAgent<'a> {
    pub fn new(app: &'a mut App) -> Self {
        Self {
            app: RefCell::new(app),
        }
    }
}

/// 宿主缺少这项能力时的统一说法：点明缺什么，不让调用方以为命令成功了。
fn blocked<T>(what: &str, why: &str) -> Result<T, AgentError> {
    Err(AgentError::new(format!("{what}暂不可用：{why}")))
}

impl CommandAgent for TuiHostAgent<'_> {
    // ── 配置环境 ──────────────────────────────────────────────

    fn config_environment(&self) -> ConfigEnvironment {
        ConfigEnvironment::from_process()
    }

    // ── 会话 ──────────────────────────────────────────────────

    fn current_session_id(&self) -> String {
        self.app.borrow().current_session_id()
    }

    fn workspace_root(&self) -> PathBuf {
        self.app.borrow().command_workspace_root()
    }

    fn reset_conversation(&self) -> Result<(), AgentError> {
        self.app
            .borrow_mut()
            .command_session_new()
            .map(|_| ())
            .map_err(AgentError::new)
    }

    fn rename_current_session(&self, title: &str) -> Result<SessionSummary, AgentError> {
        self.app
            .borrow_mut()
            .command_session_rename(title)
            .map_err(AgentError::new)
    }

    fn resume_session(&self, session_id: &str) -> Result<SessionSummary, AgentError> {
        self.app
            .borrow_mut()
            .command_session_resume(session_id)
            .map_err(AgentError::new)
    }

    fn archive_current_session(&self) -> Result<SessionSummary, AgentError> {
        self.app
            .borrow_mut()
            .command_session_archive()
            .map_err(AgentError::new)
    }

    fn undo_last_turn(&self) -> Result<(), AgentError> {
        blocked(
            "/undo",
            "它由宿主异步下发内核（整轮回退会改会话与工作区），命令层不直接执行。",
        )
    }

    fn compact_conversation_model(&self) -> Result<(), AgentError> {
        blocked(
            "/compact",
            "它由宿主异步下发内核（压缩要调用摘要模型，不能阻塞界面），命令层不直接执行。",
        )
    }

    fn list_sessions(&self, limit: usize) -> Result<Vec<SessionIndexEntry>, AgentError> {
        self.app
            .borrow_mut()
            .command_session_list(false, limit)
            .map_err(AgentError::new)
    }

    fn list_archived_sessions(&self, limit: usize) -> Result<Vec<SessionIndexEntry>, AgentError> {
        self.app
            .borrow_mut()
            .command_session_list(true, limit)
            .map_err(AgentError::new)
    }

    fn search_prompt_history(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<PromptHistoryEntry>, AgentError> {
        self.app
            .borrow_mut()
            .command_session_history(query, limit)
            .map_err(AgentError::new)
    }

    // ── 主 Agent 模式 ─────────────────────────────────────────

    fn activate_mode(&self, mode: &str) -> Result<String, AgentError> {
        // 模式提示词改的是 system prompt，必须由**宿主**装配（模板、Skill 与 AGENTS.md 都在这边），
        // 因此这里是「加载 → 重算 system prompt 与上下文消息 → 下发内核」的完整链路。
        self.app
            .borrow_mut()
            .command_activate_mode(mode)
            .map_err(AgentError::new)
    }

    // ── 审批 / 推理强度 / 模型 ────────────────────────────────

    fn approval_mode(&self) -> String {
        self.app.borrow().command_approval_mode().to_string()
    }

    fn set_approval_mode(&self, mode: &str) -> Result<(), AgentError> {
        self.app
            .borrow_mut()
            .command_set_approval_mode(mode)
            .map_err(AgentError::new)
    }

    fn reasoning_effort(&self) -> Option<String> {
        let effort = self.app.borrow().command_llm_config().reasoning_effort;
        if effort.is_empty() {
            None
        } else {
            Some(effort)
        }
    }

    fn set_reasoning_effort(&self, effort: &str) -> Result<String, AgentError> {
        self.app.borrow_mut().command_set_reasoning_effort(effort)
    }

    fn current_model(&self) -> String {
        self.app.borrow().command_current_model()
    }

    fn llm_config(&self) -> LlmConfig {
        self.app.borrow().command_llm_config()
    }

    fn set_model(
        &self,
        selection: &str,
        persist: &mut dyn FnMut() -> Result<(), AgentError>,
    ) -> Result<(), AgentError> {
        // 解析失败即报错：绝不把无效 key 静默降级成裸 model_id（与 Python 同口径）。
        let resolved = {
            let app = self.app.borrow();
            app.command_resolve_model(selection)?
        };
        // 协议要求：写盘成功之后才更新运行态。
        persist()?;
        self.app.borrow_mut().command_apply_model_view(resolved);
        Ok(())
    }

    fn apply_advisor_config(&self, _config: &AdvisorConfig) -> Result<(), AgentError> {
        blocked(
            "/advisor",
            "顾问工具由内核装配工具表；请用 /settings 的「顾问」页设置（那里已接线）。",
        )
    }

    // ── 工作区 ────────────────────────────────────────────────

    fn switch_workspace(&self, _path: &str) -> Result<PathBuf, AgentError> {
        blocked(
            "/workspace",
            "切换工作区要重建内核会话、MCP 与工具表，TUI 尚未接线。",
        )
    }

    // ── Skill / 记忆 ──────────────────────────────────────────

    fn skill_metas(&self) -> Option<Vec<SkillMeta>> {
        // 与本地 API（`omnicrawl-api`）同口径：宿主自己按工作区发现 Skill 目录，
        // 内核与宿主的发现规则一致，因此这份清单就是模型看到的清单。
        let root = self.app.borrow().command_workspace_root();
        let mut manager = omnicrawl_extensions::skill::SkillManager::new();
        manager.discover(Some(root.as_path()), &[]);
        Some(manager.list_all())
    }

    fn clean_memory(&self) -> Result<Vec<String>, AgentError> {
        blocked(
            "/memory:clean",
            "长期记忆由内核侧的记忆子系统持有，宿主没有清理入口。",
        )
    }

    // ── MCP / 插件 ────────────────────────────────────────────

    fn mcp_status(&self) -> String {
        // MCP 清单由宿主工具表持有连接，`McpClientManager::format_status()` 已能
        // 给出与 Python 同形的摘要（启用/连接数、工具与资源计数、逐 Server 状态）。
        // 未配置 MCP 时给一句明确说明，而不是空串。
        let app = self.app.borrow();
        match app.registry().mcp() {
            Some(manager) => manager.format_status(),
            None => "MCP 未配置：在 config.toml 的 [mcp] 段添加 Server 后可查看详情。".to_string(),
        }
    }

    fn plugins_status(&self) -> Option<String> {
        Some(self.app.borrow().command_plugins_status())
    }

    // ── SubAgent ──────────────────────────────────────────────

    fn list_subagent_tasks(&self) -> Result<Vec<Value>, AgentError> {
        let value = self
            .app
            .borrow_mut()
            .command_subagent_query("list", "")
            .map_err(AgentError::new)?;
        if unavailable(&value) {
            return Err(AgentError::new("后台子任务管理器未启用。"));
        }
        Ok(value
            .get("tasks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    fn get_subagent_task(&self, task_id: &str) -> Result<Option<Value>, AgentError> {
        let value = self
            .app
            .borrow_mut()
            .command_subagent_query("get", task_id)
            .map_err(AgentError::new)?;
        if unavailable(&value) {
            return Err(AgentError::new("后台子任务管理器未启用。"));
        }
        Ok(value.get("task").filter(|task| !task.is_null()).cloned())
    }

    fn cancel_subagent_task(&self, task_id: &str) -> Result<Value, AgentError> {
        let value = self
            .app
            .borrow_mut()
            .command_subagent_query("cancel", task_id)
            .map_err(AgentError::new)?;
        if unavailable(&value) {
            return Err(AgentError::new("后台子任务管理器未启用。"));
        }
        Ok(value
            .get("result")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new())))
    }

    fn run_subagent_task(&self, _request: SubAgentRun) -> Result<String, AgentError> {
        blocked(
            "/review",
            "它由宿主异步下发内核（子 Agent 的工具批次要回到宿主执行，同步等待会死锁），\
             命令层不直接执行。",
        )
    }

    fn remember_review_report(&self, report: &str) -> Result<(), AgentError> {
        self.app
            .borrow_mut()
            .command_session_append(report)
            .map(|_| ())
            .map_err(AgentError::new)
    }

    // ── 只读 git 探测 ─────────────────────────────────────────

    fn run_git(&self, workspace_root: &Path, args: &[&str]) -> Option<GitOutput> {
        run_git(workspace_root, args)
    }
}

/// 内核回执里的 `unavailable` 标记（内核没有任务管理器时为真）。
fn unavailable(value: &Value) -> bool {
    value
        .get("unavailable")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// 在工作区根下跑一次只读 git：与 Python 同口径（`git -c color.ui=never --no-pager`、
/// 捕获 stdout、30 秒超时），git 缺失或超时返回 `None`。
pub fn run_git(workspace_root: &Path, args: &[&str]) -> Option<GitOutput> {
    let mut command = std::process::Command::new("git");
    command
        .arg("-c")
        .arg("color.ui=never")
        .arg("--no-pager")
        .args(args)
        .current_dir(workspace_root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // 不弹控制台窗口：宿主本身就在终端里，git 子进程不需要自己的窗口。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + GIT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(GIT_POLL),
            // 超时或等待失败：杀掉子进程并放弃（Python 超时同样回落「未找到 git」）。
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let output = child.wait_with_output().ok()?;
    Some(GitOutput {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
    })
}

/// 插件状态行：把运行期快照转成命令层用的只读行。
///
/// 字段名与 `omnicrawl-extensions` 的 `list_status()` 一致；缺字段按未知处理。
pub fn plugin_row(row: &Map<String, Value>) -> PluginWorkerRow {
    let text = |key: &str| {
        row.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let flag = |key: &str| row.get(key).and_then(Value::as_bool).unwrap_or(false);
    PluginWorkerRow {
        name: text("name"),
        version: text("version"),
        scope: text("scope"),
        active: flag("active"),
        circuit_open: flag("circuitOpen"),
        dev_mode: flag("devMode"),
        handlers: row
            .get("handlers")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        last_error: text("lastError"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_rows_keep_field_names_of_runtime_snapshot() {
        let row: Map<String, Value> = serde_json::from_str(
            r#"{"name":"demo","version":"1.2.3","scope":"workspace","active":true,
                "circuitOpen":false,"devMode":true,"handlers":["turn.start","tool.call.before"],
                "lastError":""}"#,
        )
        .expect("测试 JSON 应当合法");
        let row = plugin_row(&row);
        assert_eq!(row.name, "demo");
        assert_eq!(row.version, "1.2.3");
        assert_eq!(row.scope, "workspace");
        assert!(row.active && !row.circuit_open && row.dev_mode);
        assert_eq!(row.handlers.len(), 2);
        assert!(row.last_error.is_empty());
    }

    #[test]
    fn plugin_rows_tolerate_missing_fields() {
        let row: Map<String, Value> = Map::new();
        let row = plugin_row(&row);
        assert!(row.name.is_empty());
        assert!(!row.active && !row.circuit_open && !row.dev_mode);
        assert!(row.handlers.is_empty());
    }

    #[test]
    fn unavailable_flag_is_read_from_kernel_reply() {
        assert!(unavailable(&serde_json::json!({"unavailable": true})));
        assert!(!unavailable(&serde_json::json!({"unavailable": false})));
        assert!(!unavailable(&serde_json::json!({})));
    }

    #[test]
    fn options_come_from_the_registry() {
        let options = command_options();
        let names: Vec<&str> = options
            .iter()
            .map(|option| option.command.as_str())
            .collect();
        assert!(names.contains(&"/settings"), "{names:?}");
        assert!(names.contains(&"/tasks"), "{names:?}");
        assert!(names.contains(&"/approval:auto"), "{names:?}");
        // 菜单用的就是注册表声明：命令名与插入文本一致，参数提示带在候选上。
        let settings = options
            .iter()
            .find(|option| option.command == "/settings")
            .expect("应有 /settings");
        assert_eq!(settings.insert, "/settings");
        assert_eq!(
            settings.parameters.as_ref().map(Vec::len),
            Some(1),
            "参数提示来自命令声明"
        );
    }
}
