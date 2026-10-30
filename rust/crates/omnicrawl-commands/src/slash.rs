//! 内置斜杠命令：声明、处理器与展示文案（`omnicrawl/commands/slash.py` 的 Rust 移植）。
//!
//! 全仓库唯一的命令注册表在 [`registry`]；命令在 [`build_registry`] 里逐条声明，
//! 解析/分发/帮助/补全统一由 [`CommandRegistry`] 负责，各入口（TUI、Telegram、飞书、
//! 本地 API）只调用 `registry().dispatch(...)`。
//!
//! 处理器只依赖 [`CommandAgent`] 与 `omnicrawl-config` 的写回函数：宿主副作用（会话生命周期、
//! 子代理、MCP、Skill、工具表重建）走 trait，配置写回直接用已搬好的 Rust 实现——这与 Python
//! `slash.py` 同时调 agent 方法与配置函数的结构一致。远端入口的安全边界（`/approval:auto`、
//! `/quit`、`/settings` 对远程通道的拒绝）逐条保留。

use crate::agent::{CommandAgent, SubAgentRun};
use crate::framework::{
    Channel, Command, CommandContext, CommandOption, CommandParseError, CommandRegistry,
    CommandResult, CommandType,
};
use chrono::{DateTime, Local};
use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::core::workspace::save_workspace_root;
use omnicrawl_config::features::advisor::{
    clear_advisor_config, load_advisor_config, save_advisor_config, AdvisorConfig,
    ADVISOR_EFFORT_OPTIONS, DEFAULT_ADVISOR_EFFORT,
};
use omnicrawl_config::features::approval::{
    approval_mode_label, normalize_approval_mode, save_approval_mode, APPROVAL_MODE_AUTO,
    APPROVAL_MODE_MANUAL, APPROVAL_MODE_REVIEW,
};
use omnicrawl_config::models::llm::{save_active_model_ref, save_reasoning_effort, ActiveModelRef};
use omnicrawl_config::models::llm_multi::apply_model_selection;
use omnicrawl_config::models::model_catalog::save_llm_model;
use omnicrawl_config::models::model_store::load_model_store;
use omnicrawl_controllers::json::python_dumps;
use omnicrawl_controllers::tool_args::public_tool_arguments;
use omnicrawl_controllers::AgentError;
use regex::Regex;
use serde_json::{Map, Value};
use std::path::Path;
use std::sync::OnceLock;

mod agent_commands;
mod registry;
mod render;
mod review;
mod session_commands;
mod skill_suggestions;
mod workspace_commands;

pub use registry::{build_registry, registry};
pub use render::{
    format_archived_sessions_list, format_dangerous_tool_detail, format_mcp_status,
    format_memory_clean_result, format_plugins_status, format_prompt_history,
    format_sessions_list, format_skills_list, format_skills_list_brief, format_subagent_task,
    format_subagent_tasks_list, format_tool_confirmation, render_subagent_task,
};
pub use review::{
    build_review_task_prompt, check_review_preconditions, extract_review_json,
    format_priority_tag, format_review_report, handle_review_command, python_general_float,
    REVIEW_DEFAULT_SCOPE, REVIEW_TASK_DESCRIPTION,
};
pub use skill_suggestions::{build_slash_command_options, build_slash_commands};
pub use workspace_commands::{
    handle_workspace_command, persist_workspace_root, workspace_switch_success_message,
    WORKSPACE_SWITCH_PENDING_MESSAGE,
};

// 处理器保持对外可见：它们原先是 `slash.rs` 的 `pub fn`，宿主虽然只经
// `registry().dispatch(..)` 调用，但路径一旦变化就是破坏性改动。
pub use session_commands::{
    handle_tasks_command, handle_task_command, handle_sessions_command, handle_archives_command,
    handle_archive_command, handle_history_command, handle_undo_command, handle_compact_command,
    handle_rename_command, handle_resume_command,
};

pub use agent_commands::{
    handle_approval_query_command, handle_approval_manual_command, handle_approval_auto_command,
    handle_approval_review_command, handle_reasoning_command, handle_model_command,
    handle_advisor_command, handle_mode_command, handle_speaking_style_command,
    handle_skills_command, handle_memory_clean_command, handle_mcp_command, handle_plugins_command,
    handle_new_command, handle_quit_command, handle_settings_command,
};


mod tests {
    // 测试里逐字段填充假 Agent 比一次性结构体字面量更贴近用例语义，保留这种写法。
    #![allow(clippy::field_reassign_with_default)]
    use super::*;
    use crate::agent::testing::FakeAgent;
    use crate::agent::GitOutput;
    use crate::framework::Channel;
    use chrono::{TimeZone, Utc};
    use omnicrawl_extensions::skill::SkillMeta;
    use omnicrawl_session::index::SessionIndexEntry;
    use omnicrawl_session::PromptHistoryEntry;
    use serde_json::json;
    use std::path::PathBuf;

    fn session_entry(session_id: &str, title: &str, day: u32) -> SessionIndexEntry {
        let created = Utc.with_ymd_and_hms(2026, 9, day, 3, 4, 5).unwrap();
        SessionIndexEntry {
            session_id: session_id.to_string(),
            title: title.to_string(),
            workspace_root: "D:/work".to_string(),
            path: format!("sessions/{session_id}.jsonl"),
            created_at: created,
            updated_at: created,
            event_count: 4,
            message_count: 2,
            last_event_type: "assistant_message".to_string(),
            archived_at: None,
        }
    }

    fn skill(name: &str, description: &str) -> SkillMeta {
        SkillMeta {
            name: name.to_string(),
            description: description.to_string(),
            source_path: PathBuf::from("D:/work/.omnicrawl/skills/")
                .join(name)
                .join("SKILL.md"),
            base_dir: PathBuf::from("D:/work/.omnicrawl/skills/").join(name),
            scope: "project".to_string(),
            disable_model_invocation: false,
        }
    }

    #[test]
    fn registry_declares_all_builtin_commands_in_order() {
        let registry = build_registry().expect("内置命令表必须合法");
        let names: Vec<String> = registry
            .commands(true)
            .iter()
            .map(|command| command.name.clone())
            .collect();
        assert_eq!(names.len(), 29);
        assert_eq!(names[0], "tasks");
        assert!(names.contains(&"memory:clean".to_string()));
        assert!(names.contains(&"approval:auto".to_string()));
        assert!(names.contains(&"programming".to_string()));
        assert!(names.contains(&"speaking-style".to_string()));
        assert_eq!(names.last().map(String::as_str), Some("workspace"));
    }

    #[test]
    fn registry_is_built_once() {
        assert!(std::ptr::eq(registry(), registry()));
    }

    #[test]
    fn quit_is_immediate_and_workspace_defers() {
        assert!(registry()
            .parse("/quit")
            .expect("quit 可解析")
            .command
            .command_type
            .immediate());
        assert!(!registry()
            .parse("/workspace")
            .expect("workspace 可解析")
            .command
            .command_type
            .immediate());
    }

    #[test]
    fn shell_tool_confirmation_shows_command() {
        let text = format_tool_confirmation(
            "bash",
            &json!({"command": "rm -rf build"})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert!(text.starts_with("Agent 想要执行 Bash 命令。"));
        assert!(text.contains("命令：rm -rf build"));
        assert!(text.ends_with("是否允许执行？"));
    }

    #[test]
    fn read_only_tool_confirmation_hides_arguments() {
        let text = format_tool_confirmation(
            "read",
            &json!({"path": "a.py", "offset": 10})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert_eq!(text, "Agent 想要读取文件内容。\n\n是否允许执行？");
    }

    #[test]
    fn mcp_tool_confirmation_uses_dotted_name() {
        let text = format_tool_confirmation(
            "ocr.scan",
            &json!({"path": "a.png"}).as_object().unwrap().clone(),
        );
        assert!(text.contains("执行 MCP 工具 ocr.scan"));
        assert!(text.contains("参数："));
    }

    #[test]
    fn edit_tool_confirmation_shows_replacement() {
        let text = format_tool_confirmation(
            "Edit_file",
            &json!({"path": "a.py", "old_text": "foo", "new_text": "bar"})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert!(text.contains("文件：a.py"));
        assert!(text.contains("替换 \"foo\""));
        assert!(text.contains("→ \"bar\""));
    }

    #[test]
    fn python_general_float_matches_python_format_g() {
        assert_eq!(python_general_float(0.0), "0");
        assert_eq!(python_general_float(1.0), "1");
        assert_eq!(python_general_float(0.9), "0.9");
        assert_eq!(python_general_float(0.85), "0.85");
        assert_eq!(python_general_float(0.123456789), "0.123457");
        assert_eq!(python_general_float(0.00001), "1e-05");
        assert_eq!(python_general_float(1234567.0), "1.23457e+06");
    }

    #[test]
    fn truncate_keeps_character_budget() {
        assert_eq!(truncate_for_display("abcdef", 6), "abcdef");
        assert_eq!(truncate_for_display("abcdefg", 6), "abc...");
        assert_eq!(truncate_for_display("中文中文中文", 4), "中...");
    }

    #[test]
    fn extract_review_json_accepts_fenced_payload() {
        let fenced = "前缀\n```json\n{\"overall_correctness\": \"correct\"}\n```\n后缀";
        let parsed = extract_review_json(fenced).expect("应解析出 JSON 对象");
        assert_eq!(
            parsed.get("overall_correctness").and_then(Value::as_str),
            Some("correct")
        );
        assert!(extract_review_json("没有 JSON").is_none());
    }

    #[test]
    fn review_report_renders_findings() {
        let report = json!({
            "overall_correctness": "patch is incorrect",
            "overall_explanation": "边界条件漏了",
            "overall_confidence_score": 0.9,
            "findings": [
                {
                    "title": "越界写入",
                    "priority": 1,
                    "confidence_score": 0.85,
                    "code_location": {
                        "absolute_file_path": "D:/a.py",
                        "line_range": {"start": 10, "end": 12}
                    },
                    "body": "第一行\n第二行"
                }
            ]
        })
        .to_string();
        let rendered = format_review_report(&report);
        assert!(rendered.starts_with("## 代码评审结果\n"));
        assert!(rendered.contains("**总体结论**：patch is incorrect ❌（置信度 0.9）"));
        assert!(rendered.contains("边界条件漏了"));
        assert!(rendered.contains("**发现问题 1 项**："));
        assert!(rendered.contains("1. **[P1] 越界写入**（置信度 0.85）"));
        assert!(rendered.contains("   - 位置：`D:/a.py` 行 10-12"));
        assert!(rendered.contains("  第一行\n  第二行"));
    }

    #[test]
    fn review_report_falls_back_to_raw_text() {
        assert_eq!(format_review_report("  自由文本  "), "自由文本");
        assert_eq!(format_review_report("   "), "（评审子 Agent 未返回内容。）");
    }

    #[test]
    fn priority_tag_rejects_out_of_range() {
        assert_eq!(format_priority_tag(Some(&json!(0))), "[P0] ");
        assert_eq!(format_priority_tag(Some(&json!(3))), "[P3] ");
        assert_eq!(format_priority_tag(Some(&json!(4))), "");
        assert_eq!(format_priority_tag(Some(&json!(-1))), "");
        assert_eq!(format_priority_tag(Some(&json!("2"))), "[P2] ");
        assert_eq!(format_priority_tag(None), "");
    }

    #[test]
    fn review_preconditions_report_missing_git() {
        let agent = FakeAgent::default();
        assert_eq!(
            check_review_preconditions(&agent, Path::new("D:/work"), ""),
            Some("未找到 git 可执行文件，请确认 Git 已安装。".to_string())
        );
    }

    #[test]
    fn review_preconditions_reject_non_repository() {
        let mut agent = FakeAgent::default();
        agent.git.insert(
            "rev-parse --is-inside-work-tree".to_string(),
            Some(GitOutput {
                code: 128,
                stdout: String::new(),
            }),
        );
        assert_eq!(
            check_review_preconditions(&agent, Path::new("D:/work"), ""),
            Some("当前工作区不是 git 仓库，无法评审。".to_string())
        );
    }

    #[test]
    fn review_preconditions_require_commit_for_explicit_scope() {
        let mut agent = FakeAgent::default();
        agent.git.insert(
            "rev-parse --is-inside-work-tree".to_string(),
            Some(GitOutput {
                code: 0,
                stdout: "true\n".to_string(),
            }),
        );
        agent.git.insert(
            "rev-parse --verify HEAD".to_string(),
            Some(GitOutput {
                code: 128,
                stdout: String::new(),
            }),
        );
        assert_eq!(
            check_review_preconditions(&agent, Path::new("D:/work"), "HEAD~3"),
            Some("当前仓库还没有任何提交，无法按指定范围评审。".to_string())
        );
    }

    #[test]
    fn review_preconditions_skip_clean_worktree() {
        let mut agent = FakeAgent::default();
        agent.git.insert(
            "rev-parse --is-inside-work-tree".to_string(),
            Some(GitOutput {
                code: 0,
                stdout: "true\n".to_string(),
            }),
        );
        agent.git.insert(
            "status --porcelain".to_string(),
            Some(GitOutput {
                code: 0,
                stdout: "\n".to_string(),
            }),
        );
        assert_eq!(
            check_review_preconditions(&agent, Path::new("D:/work"), ""),
            Some("工作区没有未提交改动，无需评审。".to_string())
        );
    }

    #[test]
    fn review_preconditions_pass_with_changes() {
        let mut agent = FakeAgent::default();
        agent.git.insert(
            "rev-parse --is-inside-work-tree".to_string(),
            Some(GitOutput {
                code: 0,
                stdout: "true\n".to_string(),
            }),
        );
        agent.git.insert(
            "status --porcelain".to_string(),
            Some(GitOutput {
                code: 0,
                stdout: " M a.py\n".to_string(),
            }),
        );
        assert_eq!(
            check_review_preconditions(&agent, Path::new("D:/work"), ""),
            None
        );
    }

    #[test]
    fn sessions_list_marks_current_session() {
        let mut agent = FakeAgent::default();
        agent.session_id = "20260920-030405-abcdef".to_string();
        agent.sessions = vec![
            session_entry("20260920-030405-abcdef", "", 20),
            session_entry("20260919-030405-bbbbbb", "归档前", 19),
        ];
        let text = format_sessions_list(&agent);
        assert!(text.starts_with("最近会话："));
        assert!(text.contains("* 20260920-030405-abcdef"));
        assert!(text.contains("  20260919-030405-bbbbbb"));
        assert!(text.contains("未命名会话"));
        assert!(text.contains("恢复会话：/resume <session_id>"));
    }

    #[test]
    fn sessions_list_reports_empty_workspace() {
        let agent = FakeAgent::default();
        assert_eq!(format_sessions_list(&agent), "当前工作区还没有可恢复会话。");
        assert_eq!(
            format_archived_sessions_list(&agent),
            "当前工作区还没有已归档会话。"
        );
    }

    #[test]
    fn prompt_history_formats_rows_and_query_title() {
        let mut agent = FakeAgent::default();
        agent.session_id = "s1".to_string();
        agent.history = vec![PromptHistoryEntry {
            display: "  第一行\n  第二行  ".to_string(),
            timestamp: 1_700_000_000_000,
            project: "D:/work".to_string(),
            session_id: "s1".to_string(),
            pasted_contents: Map::new(),
        }];
        let text = format_prompt_history(&agent, "行");
        assert!(text.starts_with("提示历史（关键词：行）："));
        assert!(text.contains("1. * "));
        assert!(text.contains("第一行 第二行"));
        assert!(text.contains("筛选历史：/history <关键词>"));
        assert_eq!(
            format_prompt_history(&agent, "查不到"),
            "当前工作区还没有匹配的提示历史。"
        );
    }

    #[test]
    fn skills_list_reports_disabled_subsystem() {
        let agent = FakeAgent::default();
        assert_eq!(format_skills_list(&agent), "Skill 子系统未启用。");
    }

    #[test]
    fn skills_list_marks_manual_skills() {
        let mut agent = FakeAgent::default();
        agent.skills = Some(vec![skill("bili-note", "提取 B 站内容")]);
        let text = format_skills_list(&agent);
        assert!(text.starts_with("已加载 1 个 Skill："));
        assert!(text.contains("  bili-note  (project)"));
        assert!(text.contains("    提取 B 站内容"));
    }

    /// 发现诊断必须出现在 `/skills` 输出里：路径写错、同名冲突时用户能当场看到原因。
    #[test]
    fn skills_list_appends_discovery_diagnostics() {
        let mut agent = FakeAgent::default();
        agent.skills = Some(vec![skill("bili-note", "提取 B 站内容")]);
        agent.skill_diagnostics = Some(vec![
            omnicrawl_extensions::skill::SkillDiagnostic::warning(
                "Skill 路径不存在",
                "D:\\missing-skills",
            ),
            omnicrawl_extensions::skill::SkillDiagnostic {
                kind: "collision".to_string(),
                message: "Skill 名称 \"demo\" 冲突".to_string(),
                path: "C:\\skills\\demo\\SKILL.md".to_string(),
                collision: None,
            },
        ]);
        let text = format_skills_list(&agent);
        assert!(text.contains("发现诊断 2 条："), "{text}");
        assert!(
            text.contains("[!] Skill 路径不存在（D:\\missing-skills）"),
            "{text}"
        );
        assert!(text.contains("[!] Skill 名称 \"demo\" 冲突"), "{text}");
    }

    /// 没有诊断时不多打一行。
    #[test]
    fn skills_list_omits_empty_diagnostics() {
        let mut agent = FakeAgent::default();
        agent.skills = Some(vec![skill("bili-note", "提取 B 站内容")]);
        agent.skill_diagnostics = Some(Vec::new());
        assert!(!format_skills_list(&agent).contains("发现诊断"));
    }

    /// QQ 机器人只列名称：description 常是整段说明，会把列表撑成多条长消息。
    #[test]
    fn qqbot_skills_list_shows_names_only() {
        let mut agent = FakeAgent::default();
        agent.skills = Some(vec![skill("bili-note", "提取 B 站内容")]);
        let text = registry()
            .dispatch("/skills", &agent, Channel::Qqbot, None)
            .message
            .unwrap_or_default();
        assert!(text.starts_with("已加载 1 个 Skill："), "{text}");
        assert!(text.contains("  bili-note  (project)"), "{text}");
        assert!(!text.contains("提取 B 站内容"), "{text}");
    }

    /// 本地与其它远程入口保持原样，仍带 description。
    #[test]
    fn non_qqbot_skills_list_keeps_descriptions() {
        let mut agent = FakeAgent::default();
        agent.skills = Some(vec![skill("bili-note", "提取 B 站内容")]);
        for channel in [Channel::Tui, Channel::Telegram, Channel::Feishu] {
            let text = registry()
                .dispatch("/skills", &agent, channel.clone(), None)
                .message
                .unwrap_or_default();
            assert!(text.contains("提取 B 站内容"), "{channel:?}: {text}");
        }
    }

    #[test]
    fn plugins_status_falls_back_to_interface_hint() {
        let agent = FakeAgent::default();
        assert_eq!(format_plugins_status(&agent), "插件状态接口不可用。");
    }

    #[test]
    fn memory_clean_reports_paths() {
        let mut agent = FakeAgent::default();
        agent.memory_paths = vec!["~/.omnicrawl/memory/a.md".to_string()];
        assert!(format_memory_clean_result(&agent).starts_with("已清理 1 条过期记忆："));
        let empty = FakeAgent::default();
        assert_eq!(
            format_memory_clean_result(&empty),
            "没有需要清理的过期记忆。"
        );
    }

    #[test]
    fn subagent_task_list_and_detail() {
        let mut agent = FakeAgent::default();
        agent.subagent_tasks = vec![json!({
            "task_id": "task-1",
            "agent_type": "review",
            "status": "running",
            "description": "评审当前代码变更"
        })];
        let text = format_subagent_tasks_list(&agent);
        assert!(text.contains("- task-1 · review · running · 评审当前代码变更"));
        assert!(text.contains("查看详情：/task <task_id>"));

        agent.task_snapshot = Some(json!({
            "task_id": "task-1",
            "status": "succeeded",
            "agent_type": "review",
            "description": "评审当前代码变更",
            "result": {"summary": " 没问题 ", "artifacts": ["a.md"]}
        }));
        let detail = format_subagent_task(&agent, "task-1");
        assert!(detail.contains("任务：task-1"));
        assert!(detail.contains("摘要：\n没问题"));
        assert!(detail.contains("关联 artifact：1 项"));
    }

    #[test]
    fn mode_commands_activate_their_template() {
        let mut agent = FakeAgent::default();
        let plan = registry().dispatch("/plan", &agent, Channel::Tui, None);
        assert!(plan
            .message
            .unwrap_or_default()
            .contains("已启用计划模式"));
        assert!(plan.refresh_context);

        let programming = registry().dispatch("/programming", &agent, Channel::Tui, None);
        assert!(programming
            .message
            .unwrap_or_default()
            .contains("已启用编程模式"));
        assert!(programming.refresh_context);
        assert_eq!(
            agent
                .calls()
                .iter()
                .filter(|call| *call == "activate_mode")
                .count(),
            2
        );
    }

    #[test]
    fn speaking_style_command_toggles_the_mode() {
        let mut agent = FakeAgent::default();
        let off = registry().dispatch("/speaking-style", &agent, Channel::Qqbot, None);
        assert!(off
            .message
            .unwrap_or_default()
            .contains("简洁回复模式：未开启"));

        let on = registry().dispatch("/speaking-style on", &agent, Channel::Qqbot, None);
        assert!(on
            .message
            .unwrap_or_default()
            .contains("已开启简洁回复模式"));
        assert!(on.refresh_context);
        assert!(agent.calls().iter().any(|call| call == "activate_mode"));

        // 宿主侧已记录为生效：不带参数的查询要如实反映，且重复开启不再激活。
        agent.active_mode = "speaking-style".to_string();
        let query = registry().dispatch("/speaking-style", &agent, Channel::Qqbot, None);
        assert!(query
            .message
            .unwrap_or_default()
            .contains("简洁回复模式：已开启"));
        let again = registry().dispatch("/speaking-style 开", &agent, Channel::Qqbot, None);
        assert!(again
            .message
            .unwrap_or_default()
            .contains("简洁回复模式：已开启"));

        let disable = registry().dispatch("/speaking-style off", &agent, Channel::Qqbot, None);
        assert!(disable.message.unwrap_or_default().contains("已关闭"));
        assert!(agent.calls().iter().any(|call| call == "deactivate_mode"));

        let usage = registry().dispatch("/speaking-style 呀", &agent, Channel::Qqbot, None);
        assert!(usage.message.unwrap_or_default().contains("用法"));
    }

    #[test]
    fn remote_channel_rejects_auto_approval() {
        let agent = FakeAgent::default();
        let result = registry().dispatch("/approval:auto", &agent, Channel::Telegram, None);
        let message = result.message.unwrap_or_default();
        assert!(message.starts_with("❌ 远程不支持完全自动批准。"));
        assert!(agent.calls().iter().all(|call| call != "set_approval_mode"));
    }

    #[test]
    fn quit_and_settings_follow_channel_capabilities() {
        let agent = FakeAgent::default();
        let remote_quit = registry().dispatch("/quit", &agent, Channel::Telegram, None);
        assert!(remote_quit
            .message
            .unwrap_or_default()
            .contains("远程连接不支持 /quit"));
        assert!(!remote_quit.exit_requested);

        let local_quit = registry().dispatch("/quit", &agent, Channel::Tui, None);
        assert!(local_quit.exit_requested);

        let remote_settings = registry().dispatch("/settings", &agent, Channel::Telegram, None);
        assert!(remote_settings
            .message
            .unwrap_or_default()
            .contains("远程连接不支持 /settings"));
        assert!(!remote_settings.open_settings);
    }

    #[test]
    fn settings_opens_the_panel_and_rejects_arguments() {
        let agent = FakeAgent::default();
        let panel = registry().dispatch("/settings", &agent, Channel::Tui, None);
        assert!(panel.open_settings);
        assert!(!panel.clear_conversation);

        let invalid = registry().dispatch("/settings --other", &agent, Channel::Tui, None);
        assert_eq!(invalid.error.as_deref(), Some("/settings 不接受参数。"));
    }

    #[test]
    fn compact_rejects_arguments_and_defers_work() {
        let agent = FakeAgent::default();
        let invalid = registry().dispatch("/compact now", &agent, Channel::Tui, None);
        assert_eq!(invalid.message.as_deref(), Some("参数错误：/compact。"));

        let slow = registry().dispatch("/compact", &agent, Channel::Tui, None);
        assert_eq!(slow.message.as_deref(), Some("正在压缩当前会话上下文…"));
        assert_eq!(slow.working_status.as_deref(), Some("正在压缩上下文"));
        assert!(slow.deferred.is_some());
        let resolved = slow.resolve(&agent);
        assert!(resolved
            .message
            .unwrap_or_default()
            .contains("已压缩当前会话，完整转录仍保留"));
        assert!(agent
            .calls()
            .iter()
            .any(|call| call == "compact_conversation_model"));
    }

    #[test]
    fn mcp_status_is_read_inside_deferred() {
        let mut agent = FakeAgent::default();
        agent.mcp_status = "MCP：已启用".to_string();
        let result = registry().dispatch("/mcp", &agent, Channel::Tui, None);
        assert_eq!(result.message.as_deref(), Some("正在读取 MCP 状态"));
        let resolved = result.resolve(&agent);
        assert_eq!(resolved.message.as_deref(), Some("MCP：已启用"));
    }

    #[test]
    fn workspace_without_arguments_is_read_only() {
        let mut agent = FakeAgent::default();
        agent.workspace = PathBuf::from("D:/work");
        let result = registry().dispatch("/workspace", &agent, Channel::Tui, None);
        let message = result.message.unwrap_or_default();
        // 文案按 Python `str(Path)` 渲染：Windows 上是反斜杠。
        assert!(
            message.contains(&format!("当前工作区：{}", path_text(&agent.workspace))),
            "{message}"
        );
        assert!(result.deferred.is_none());
    }

    #[test]
    fn workspace_switch_defers_and_reports_new_root() {
        let mut agent = FakeAgent::default();
        agent.workspace = PathBuf::from("D:/work");
        let result = registry().dispatch("/workspace D:/other", &agent, Channel::Tui, None);
        assert!(result.workspace_switch_requested);
        assert!(result.deferred.is_some());
        let resolved = result.resolve(&agent);
        // 切换后的文案用 agent 返回的解析结果，而不是用户输入的原文。
        let expected = format!("已切换工作区：{}", path_text(Path::new("D:/other")));
        assert!(
            resolved.message.unwrap_or_default().contains(&expected),
            "{expected}"
        );
        assert!(agent.calls().iter().any(|call| call == "switch_workspace"));
    }

    #[test]
    fn review_runs_subagent_through_deferred() {
        let mut agent = FakeAgent::default();
        agent.workspace = PathBuf::from("D:/work");
        agent.git.insert(
            "rev-parse --is-inside-work-tree".to_string(),
            Some(GitOutput {
                code: 0,
                stdout: "true\n".to_string(),
            }),
        );
        agent.git.insert(
            "status --porcelain".to_string(),
            Some(GitOutput {
                code: 0,
                stdout: " M a.py\n".to_string(),
            }),
        );
        agent.subagent_report = "自由文本".to_string();
        let result = registry().dispatch("/review", &agent, Channel::Tui, None);
        assert!(result.message.is_none());
        assert_eq!(result.working_status.as_deref(), Some("正在评审"));
        assert!(result.stream_subagent_conversation);
        let resolved = result.resolve(&agent);
        assert_eq!(resolved.message.as_deref(), Some("自由文本"));
        let calls = agent.calls();
        assert!(calls.iter().any(|call| call == "subagent_type:review"));
        assert!(calls
            .iter()
            .any(|call| call.starts_with("subagent_prompt:请评审当前工作区的代码变更。")));
        assert_eq!(agent.review_report(), "自由文本");
    }

    #[test]
    fn slash_commands_and_options_append_skills() {
        let mut agent = FakeAgent::default();
        agent.skills = Some(vec![skill("bili-note", "提取 B 站内容")]);
        let commands = build_slash_commands(&agent);
        assert!(commands.contains(&"/skill:bili-note".to_string()));
        assert!(commands.contains(&"/undo".to_string()));
        assert!(!commands.contains(&"/secret".to_string()));

        let options = build_slash_command_options(&agent);
        let skill_option = options
            .iter()
            .find(|option| option.command == "/skill:bili-note")
            .expect("Skill 候选");
        assert_eq!(skill_option.category, crate::framework::SKILL_CATEGORY);
        assert!(skill_option.parameters.is_none());
        let builtin = options
            .iter()
            .find(|option| option.command == "/undo")
            .expect("内置候选");
        assert_eq!(builtin.insert, "/undo");
        assert_eq!(builtin.parameters, Some(Vec::new()));
    }

    #[test]
    fn task_command_reports_cancel_outcomes() {
        let mut agent = FakeAgent::default();
        agent.cancel_result = json!({"ok": true, "status": "cancelled"});
        let cancelled = registry().dispatch("/task cancel task-1", &agent, Channel::Tui, None);
        assert_eq!(
            cancelled.message.as_deref(),
            Some("已取消后台子任务：task-1。")
        );

        agent.cancel_result = json!({"ok": false});
        let missing = registry().dispatch("/task cancel task-9", &agent, Channel::Tui, None);
        assert_eq!(
            missing.message.as_deref(),
            Some("未找到当前会话的 SubAgent 任务。")
        );

        let usage = registry().dispatch("/task", &agent, Channel::Tui, None);
        assert_eq!(
            usage.message.as_deref(),
            Some("用法：/task <task_id>；/task cancel <task_id>。")
        );
    }
}
