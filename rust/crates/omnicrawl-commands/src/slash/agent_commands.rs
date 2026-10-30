//! 审批、推理强度、模型、顾问、模式、说话风格、Skill/记忆/MCP/插件、新建/退出/设置类命令。
//!
//! 从 `slash.rs` 拆出。

use super::*;

/// 查看当前工具审批模式；远程连接器额外提示不支持完全自动。
pub fn handle_approval_query_command(ctx: &CommandContext<'_>) -> CommandResult {
    let label = approval_mode_label(&ctx.agent.approval_mode());
    if ctx.is_remote() {
        let lines = [
            format!("当前工具审批模式：{label}。"),
            "可用切换：/approval:manual（手动确认）".to_string(),
            "          /approval:review（自动审查，默认）".to_string(),
            "远程不支持 /approval:auto（完全自动仅限本地 TUI）".to_string(),
        ];
        return CommandResult::message(lines.join("\n"));
    }
    CommandResult::message(format!("当前工具审批模式：{label}。")).with_refresh_context()
}

/// 切换审批模式并持久化；远程入口按安全边界拒绝完全自动。
fn apply_approval_mode(ctx: &CommandContext<'_>, mode: &str) -> CommandResult {
    let normalized = match normalize_approval_mode(mode) {
        Ok(value) => value,
        Err(error) => return CommandResult::message(error.message()),
    };
    let label = approval_mode_label(normalized);
    if ctx.is_remote() && normalized == APPROVAL_MODE_AUTO {
        // 完全自动仅限本地 TUI：远程连接器不得绕过工具审批。
        return CommandResult::message(format!(
            "❌ 远程不支持完全自动批准。完全自动仅限本地 TUI 配置；当前仍为 {}。",
            approval_mode_label(&ctx.agent.approval_mode())
        ));
    }
    if let Err(error) = ctx.agent.set_approval_mode(normalized) {
        return CommandResult::message(error.to_string());
    }
    let environment = ctx.agent.config_environment();
    match save_approval_mode(&environment, normalized, None) {
        Ok(path) => CommandResult::message(format!(
            "审批模式已切换为 {label}，并已同步到 {}。",
            path.display()
        ))
        .with_refresh_context(),
        Err(error) => CommandResult::message(format!(
            "审批模式已临时切换为 {label}，但写入 config.toml 失败：{}",
            error.message()
        ))
        .with_refresh_context(),
    }
}

/// 切换到手动确认模式。
pub fn handle_approval_manual_command(ctx: &CommandContext<'_>) -> CommandResult {
    apply_approval_mode(ctx, APPROVAL_MODE_MANUAL)
}

/// 切换到完全自动批准（仅限本地 TUI）。
pub fn handle_approval_auto_command(ctx: &CommandContext<'_>) -> CommandResult {
    apply_approval_mode(ctx, APPROVAL_MODE_AUTO)
}

/// 切换到自动审查模式。
pub fn handle_approval_review_command(ctx: &CommandContext<'_>) -> CommandResult {
    apply_approval_mode(ctx, APPROVAL_MODE_REVIEW)
}

/// 查看或切换推理强度（切换会同步写入 config.toml）。
pub fn handle_reasoning_command(ctx: &CommandContext<'_>) -> CommandResult {
    let effort = ctx.args.trim();
    if effort.is_empty() {
        let current = ctx
            .agent
            .reasoning_effort()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "默认".to_string());
        return CommandResult::message(format!(
            "当前推理强度：{current}。\n可选：/reasoning none|low|medium|high|xhigh|max"
        ))
        .with_refresh_context();
    }

    let normalized = match ctx.agent.set_reasoning_effort(effort) {
        Ok(value) => value,
        Err(error) => {
            return CommandResult::message(format!("推理强度切换失败：{error}"))
                .with_refresh_context()
        }
    };
    let environment = ctx.agent.config_environment();
    let path = match save_reasoning_effort(&environment, &normalized, None) {
        Ok(path) => path,
        Err(error) => {
            return CommandResult::message(format!(
                "推理强度已临时切换为 {normalized}，但写入 config.toml 失败：{}",
                error.message()
            ))
            .with_refresh_context()
        }
    };
    let env_message = reasoning_env_override_message(&environment);
    CommandResult::message(format!(
        "推理强度已切换为 {normalized}，并已同步到 {}{env_message}",
        path.display()
    ))
    .with_refresh_context()
}

fn reasoning_env_override_message(_environment: &ConfigEnvironment) -> String {
    String::new()
}

/// 查看或切换当前模型。
///
/// 支持 `/model` 查看当前模型，`/model <selection>` 切换模型（selection 可为 models.toml
/// key/alias、profile/model_id 或裸 model_id）。切换即时生效：运行中的回合继续用旧模型，
/// 下一次请求自动使用新模型。
pub fn handle_model_command(ctx: &CommandContext<'_>) -> CommandResult {
    let selection = ctx.args.trim().to_string();
    if selection.is_empty() {
        let current = {
            let current = ctx.agent.current_model();
            if current.is_empty() {
                ctx.agent.llm_config().model
            } else {
                current
            }
        };
        return CommandResult::message(format!(
            "当前模型：{current}\n用法：/model <key|profile/model_id|model_id>"
        ))
        .with_refresh_context();
    }

    let environment = ctx.agent.config_environment();
    let mut persist = {
        let selection = selection.clone();
        let environment = environment.clone();
        move || -> Result<(), AgentError> {
            // 与 set_model 的 apply_model_selection 解析保持一致：优先持久化
            // models.toml 引用，否则写回 llm.model。
            let record = match load_model_store(&environment, None) {
                Ok(store) => store
                    .resolve_alias(&selection)
                    .ok()
                    .flatten()
                    .map(|record| (record.key.clone(), record.model_id.clone())),
                Err(_) => None,
            };
            match record {
                Some((key, model_id)) => {
                    let reference = ActiveModelRef {
                        source: "custom".to_string(),
                        key,
                        model_id,
                        ..ActiveModelRef::default()
                    };
                    save_active_model_ref(&environment, &reference, None)
                        .map(|_| ())
                        .map_err(|error| AgentError::new(error.message()))
                }
                None => save_llm_model(&environment, &selection, None)
                    .map(|_| ())
                    .map_err(|error| AgentError::new(error.message())),
            }
        }
    };

    if let Err(error) = ctx.agent.set_model(&selection, &mut persist) {
        return CommandResult::message(format!("模型切换失败：{error}")).with_refresh_context();
    }
    CommandResult::message(format!(
        "模型已切换为：{}（从下一次请求开始生效）",
        ctx.agent.current_model()
    ))
    .with_refresh_context()
}

/// 查看或设置顾问策略模型。
///
/// 支持：
/// - `/advisor`：查看当前顾问模型/effort，并列出可选模型与用法。
/// - `/advisor <model_key> [effort]`：设置顾问模型（models.toml key/alias、profile/model_id
///   或裸 model_id）与可选推理档位（缺省 high）。
/// - `/advisor off`：清除顾问选择（关闭功能，工具即时剥离）。
///
/// 保存后重建工具表，使 advisor 工具即时出现/消失。
pub fn handle_advisor_command(ctx: &CommandContext<'_>) -> CommandResult {
    let environment = ctx.agent.config_environment();
    let current_config = match load_advisor_config(&environment, None) {
        Ok(config) => config,
        Err(error) => return CommandResult::message(error.message()),
    };
    if ctx.argv.is_empty() {
        return CommandResult::message(advisor_status_message(
            ctx.agent,
            &current_config,
            &environment,
        ))
        .with_refresh_context();
    }

    let action = ctx.argv[0].trim().to_string();
    let folded = action.to_lowercase();
    if matches!(folded.as_str(), "off" | "none" | "clear" | "no") {
        return CommandResult::message(advisor_clear(ctx.agent, &environment))
            .with_refresh_context();
    }
    if matches!(folded.as_str(), "help" | "-h" | "--help") {
        return CommandResult::message(advisor_help_message(&current_config))
            .with_refresh_context();
    }

    let model_key = action;
    // effort 取 model_key 之后的整段剩余文本，与旧行为的 split(None, 2) 一致。
    let arguments = ctx.args.trim();
    let remainder = match arguments.find(' ') {
        Some(index) => arguments[index + 1..].trim().to_string(),
        None => String::new(),
    };
    let effort = if remainder.is_empty() {
        DEFAULT_ADVISOR_EFFORT.to_string()
    } else {
        remainder
    };
    let normalized_effort = match normalize_advisor_effort(&effort) {
        Ok(value) => value,
        Err(error) => {
            return CommandResult::message(format!("顾问设置失败：{error}")).with_refresh_context()
        }
    };

    // 校验模型选择可用（apply_model_selection 解析失败即报错），不写无效引用。
    if let Err(error) = apply_model_selection(&environment, &ctx.agent.llm_config(), &model_key) {
        return CommandResult::message(format!("顾问模型无法解析：{}", error.message()))
            .with_refresh_context();
    }

    let next_config = AdvisorConfig {
        enabled: true,
        model_key: model_key.clone(),
        effort: normalized_effort.clone(),
        disabled_for_models: current_config.disabled_for_models.clone(),
    };
    let path = match save_advisor_config(&environment, &next_config, None) {
        Ok(path) => path,
        Err(error) => {
            return CommandResult::message(format!("顾问设置写入失败：{}", error.message()))
                .with_refresh_context()
        }
    };
    // 内存配置同步 + 重建工具表，让 advisor 工具即时出现。
    if let Err(error) = ctx.agent.apply_advisor_config(&next_config) {
        return CommandResult::message(format!(
            "顾问已保存为 {model_key}（effort={normalized_effort}），但工具表刷新失败：{error}"
        ))
        .with_refresh_context();
    }
    CommandResult::message(format!(
        "顾问已启用：{model_key}（effort={normalized_effort}），已写入 {}。",
        path.display()
    ))
    .with_refresh_context()
}

fn advisor_clear(agent: &dyn CommandAgent, environment: &ConfigEnvironment) -> String {
    let path = match clear_advisor_config(environment, None) {
        Ok(path) => path,
        Err(error) => return format!("清除顾问失败：{}", error.message()),
    };
    let next_config = AdvisorConfig::default();
    if let Err(error) = agent.apply_advisor_config(&next_config) {
        return format!(
            "顾问已清除（{}），但工具表刷新失败：{error}",
            path.display()
        );
    }
    format!(
        "顾问已关闭并清除选择（{}）。advisor 工具已从工具表剥离。",
        path.display()
    )
}

fn advisor_status_message(
    agent: &dyn CommandAgent,
    config: &AdvisorConfig,
    environment: &ConfigEnvironment,
) -> String {
    if config.active() {
        return format!(
            "当前顾问：{}（effort={}）\n用法：/advisor <model_key> [effort]，/advisor off 关闭。",
            config.model_key,
            config.display_effort()
        );
    }
    format!(
        "顾问未启用。\n用法：/advisor <model_key> [effort]，effort 可选 {}（缺省 {}）。\n可用模型：\n{}",
        ADVISOR_EFFORT_OPTIONS.join("/"),
        DEFAULT_ADVISOR_EFFORT,
        advisor_model_candidates(agent, environment)
    )
}

fn advisor_help_message(config: &AdvisorConfig) -> String {
    let state = if config.active() {
        format!(
            "当前顾问：{}（effort={}）",
            config.model_key,
            config.display_effort()
        )
    } else {
        "顾问未启用".to_string()
    };
    format!(
        "{state}\n用法：\n  /advisor                   查看状态与可用模型\n  \
         /advisor <model_key> [effort]  设置顾问模型与推理档位\n  \
         /advisor off               关闭并清除顾问\neffort 可选：{}（缺省 {}）",
        ADVISOR_EFFORT_OPTIONS.join("/"),
        DEFAULT_ADVISOR_EFFORT
    )
}

/// 列出可作顾问的候选模型：models.toml enabled 模型 + 当前主模型。
fn advisor_model_candidates(agent: &dyn CommandAgent, environment: &ConfigEnvironment) -> String {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(store) = load_model_store(environment, None) {
        for record in &store.models {
            if !record.enabled {
                continue;
            }
            let mut label = record.key.clone();
            if !record.aliases.is_empty() {
                label.push_str(&format!("（别名：{}）", record.aliases.join("/")));
            }
            candidates.push(format!("  - {label}"));
        }
    }
    let current = {
        let current = agent.current_model();
        if current.is_empty() {
            agent.llm_config().model
        } else {
            current
        }
    };
    if !current.is_empty() {
        candidates.push(format!("  - {current}（当前主模型）"));
    }
    if candidates.is_empty() {
        "  （无可用模型，请先配置 models.toml）".to_string()
    } else {
        candidates.join("\n")
    }
}

fn normalize_advisor_effort(effort: &str) -> Result<String, AgentError> {
    let mut normalized = effort.trim().to_lowercase();
    if matches!(normalized.as_str(), "disabled" | "off" | "none") {
        normalized = "none".to_string();
    }
    if !ADVISOR_EFFORT_OPTIONS.contains(&normalized.as_str()) {
        let allowed = ADVISOR_EFFORT_OPTIONS.join("/");
        return Err(AgentError::new(format!(
            "effort 仅支持 {allowed}，当前值：{effort}。"
        )));
    }
    Ok(normalized)
}

/// 启用主 Agent 模式（`/plan`、`/programming`）。
pub fn handle_mode_command(ctx: &CommandContext<'_>) -> CommandResult {
    // 主 Agent 模式切换命令：命令名（不含前导斜杠）→ `activate_mode` 的模式标识。
    let name = match ctx.command.as_ref() {
        Some(command) => command.name.clone(),
        None => "plan".to_string(),
    };
    let mode = match name.as_str() {
        "plan" => "plan",
        "programming" => "programming",
        _ => {
            return CommandResult::message(format!("未知模式：{name}"));
        }
    };
    let activated = match ctx.agent.activate_mode(mode) {
        Ok(activated) => activated,
        Err(error) => {
            return CommandResult::message(format!("模式启用失败：{error}")).with_refresh_context()
        }
    };
    let label = match activated.as_str() {
        "plan" => "计划模式".to_string(),
        "programming" => "编程模式".to_string(),
        other => other.to_string(),
    };
    CommandResult::message(format!("已启用{label}。后续任务将遵循该模式提示词。"))
        .with_refresh_context()
}

/// 简洁回复模式的模式标识（模板 `templates/speaking-style.md`）。
const SPEAKING_STYLE_MODE: &str = "speaking-style";

/// `/speaking-style [on|off]`：开关简洁回复模式。
///
/// 与 `/plan` 同一条注入链路：模式提示词由宿主装配进 system prompt 的
/// `<active_mode_prompt>` 区块，因此这里只切模式、不碰对话历史。
/// 无参数时报告当前状态；`on` 时若已启用，重复启用视为幂等成功。
pub fn handle_speaking_style_command(ctx: &CommandContext<'_>) -> CommandResult {
    let argument = ctx.arg().trim().to_lowercase();
    match argument.as_str() {
        "" => CommandResult::message(format_speaking_style_state(&ctx.agent.active_mode())),
        "on" | "1" | "true" | "yes" | "开" | "开启" => {
            let active = ctx.agent.active_mode();
            if active == SPEAKING_STYLE_MODE {
                return CommandResult::message(format_speaking_style_state(&active));
            }
            match ctx.agent.activate_mode(SPEAKING_STYLE_MODE) {
                Ok(_) => CommandResult::message(
                    "已开启简洁回复模式：回复不超过十行，只保留结论与必要依据。",
                )
                .with_refresh_context(),
                Err(error) => CommandResult::message(format!("模式启用失败：{error}"))
                    .with_refresh_context(),
            }
        }
        "off" | "0" | "false" | "no" | "关" | "关闭" => {
            let active = ctx.agent.active_mode();
            if active != SPEAKING_STYLE_MODE {
                return CommandResult::message(format!(
                    "简洁回复模式未开启（当前模式：{}）。",
                    if active.is_empty() { "无" } else { active.as_str() }
                ));
            }
            match ctx.agent.deactivate_mode() {
                Ok(_) => CommandResult::message("已关闭简洁回复模式。").with_refresh_context(),
                Err(error) => CommandResult::message(format!("模式关闭失败：{error}"))
                    .with_refresh_context(),
            }
        }
        _ => CommandResult::message(
            "用法：/speaking-style on 开启，/speaking-style off 关闭，不带参数查看状态。",
        ),
    }
}

fn format_speaking_style_state(active: &str) -> String {
    if active == SPEAKING_STYLE_MODE {
        "简洁回复模式：已开启（回复不超过十行）。/speaking-style off 关闭。".to_string()
    } else if active.is_empty() {
        "简洁回复模式：未开启。/speaking-style on 开启。".to_string()
    } else {
        format!("简洁回复模式：未开启（当前模式：{active}）。/speaking-style on 开启。")
    }
}

/// 列出当前已加载的 Skill。
///
/// QQ 机器人单条消息有字数上限、回复条数也有限，介绍内容会把列表挤成一堆长消息，
/// 因此该通道只列名称；其余入口保留 description。
pub fn handle_skills_command(ctx: &CommandContext<'_>) -> CommandResult {
    let text = if ctx.channel == Channel::Qqbot {
        format_skills_list_brief(ctx.agent)
    } else {
        format_skills_list(ctx.agent)
    };
    CommandResult::message(text)
}

/// 清理过期长期记忆。
pub fn handle_memory_clean_command(ctx: &CommandContext<'_>) -> CommandResult {
    CommandResult::message(format_memory_clean_result(ctx.agent))
}

/// 读取 MCP 状态；真正的连接推迟到 `deferred`。
pub fn handle_mcp_command(_ctx: &CommandContext<'_>) -> CommandResult {
    let read_status = |agent: &dyn CommandAgent| -> CommandResult {
        CommandResult::message(format_mcp_status(agent))
    };
    CommandResult::message("正在读取 MCP 状态").with_deferred(Box::new(read_status))
}

/// 读取插件子系统只读状态。
pub fn handle_plugins_command(ctx: &CommandContext<'_>) -> CommandResult {
    CommandResult::message(format_plugins_status(ctx.agent))
}

/// 清空当前对话并开启新会话。
pub fn handle_new_command(ctx: &CommandContext<'_>) -> CommandResult {
    let old_session_id = ctx.agent.current_session_id();
    if let Err(error) = ctx.agent.reset_conversation() {
        return CommandResult::message(error.to_string());
    }
    let message = if old_session_id.is_empty() {
        "已新开会话。".to_string()
    } else {
        format!("已新开会话，旧会话：{old_session_id}")
    };
    CommandResult::message(message)
        .with_refresh_context()
        .with_clear_conversation()
}

/// 请求交互端退出当前 TUI；远程入口无退出语义，只给出提示。
pub fn handle_quit_command(ctx: &CommandContext<'_>) -> CommandResult {
    if ctx.is_remote() {
        return CommandResult::message("远程连接不支持 /quit；如需停止请关闭对应 Bot 会话。");
    }
    CommandResult::default().with_exit_requested()
}

/// 请求交互端打开设置面板。
pub fn handle_settings_command(ctx: &CommandContext<'_>) -> CommandResult {
    if ctx.is_remote() {
        return CommandResult::message("远程连接不支持 /settings；请在本地 TUI 中打开设置面板。");
    }
    if !ctx.args.trim().is_empty() {
        return CommandResult::error("/settings 不接受参数。");
    }
    CommandResult::message("打开设置面板")
        .with_open_settings()
        .with_refresh_context()
}

/// 按 Python `str(Path)` 的口径渲染路径：分隔符规范成当前平台的分隔符。
///
/// Python 的 `Path.__str__` 在 Windows 上输出 `\`，因此同一段路径在两侧的文案不同；
/// 界面文案要与 Python 逐字一致就得跟这条规则走（不是简单的 `display()`）。
pub(crate) fn path_text(path: &Path) -> String {
    let text = path.display().to_string();
    if cfg!(windows) {
        text.replace('/', "\\")
    } else {
        text
    }
}

/// `/workspace` 带参数时的即时反馈。
///
/// 文案只有这一处来源：命令层的内联延迟执行体与宿主的慢命令 worker（TUI 把重建放
/// 到工作线程）都读它，避免两份"正在切换"。
pub const WORKSPACE_SWITCH_PENDING_MESSAGE: &str = "正在切换工作区";

/// `/workspace` 切换成功后的回执文案（`root` 是解析后的根，不是用户输入的原文）。
pub fn workspace_switch_success_message(root: &Path) -> String {
    format!("已切换工作区：{}", path_text(root))
}

/// 把新工作区写回 `config.toml`（跨进程同步：远程入口在任务开始前重读并跟随）。
///
/// 失败只提示不阻断：切换本身已经生效，持久化只是让下一个进程看到同一个根。
pub fn persist_workspace_root(agent: &dyn CommandAgent, workspace: &str) {
    let environment = agent.config_environment();
    if let Err(error) = save_workspace_root(&environment, Path::new(workspace), None) {
        eprintln!(
            "[commands] 工作区持久化到 config.toml 失败：{}",
            error.message()
        );
    }
}
