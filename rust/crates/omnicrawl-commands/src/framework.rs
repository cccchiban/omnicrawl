//! 声明式斜杠命令框架：注册、解析、执行。
//!
//! 设计约定
//! --------
//! 业务模块只声明命令的 *元数据 + 处理器*（[`Command`]），框架负责其余一切：
//!
//! - **解析**：把一行输入拆成命令名与参数（[`CommandRegistry::parse`]），
//!   兼容 `/name args` 与无斜杠的整词别名（如「退出」）。
//! - **注册**：按名与别名建立索引（[`CommandRegistry::register`]），
//!   注册期即检测重名与别名冲突，避免运行期出现「命令被后注册者静默覆盖」。
//! - **分发**：[`CommandRegistry::dispatch`] 命中命令后构造 [`CommandContext`] 并调用处理器，
//!   未命中返回 [`CommandResult::unhandled`]，调用方据此决定是否按普通对话处理。
//! - **列表**：[`CommandRegistry::display_names`] / [`CommandRegistry::options`] /
//!   [`CommandRegistry::help_text`] 由同一份声明派生，帮助、补全菜单与真实可执行命令
//!   不再各自硬编码。
//!
//! 本模块不依赖宿主：处理器要用的宿主能力经 [`CommandAgent`] 注入。命令类型只表达调度
//! 语义，远端/本地等通道差异由处理器读取 [`CommandContext::channel`] 自行判断。

use crate::agent::{CommandAgent, SubagentEventCallback};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

/// 内置命令候选的类别标签（交互端菜单用）。
pub const COMMAND_CATEGORY: &str = "命令";
/// 运行期发现的 Skill 候选用另一个类别，与内置命令分开。
pub const SKILL_CATEGORY: &str = "Skill";
/// 命令没写说明时的兜底文案。
pub const DEFAULT_COMMAND_DESCRIPTION: &str = "执行斜杠命令。";

/// 命令类型：决定框架与交互端默认的调度方式。
///
/// [`CommandType::immediate`] 为真时，交互端允许在 Agent 回合进行中立即执行该命令；
/// 否则必须排队，避免与后台回合并发修改状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CommandType {
    /// 纯界面动作（开关面板、退出）：无业务副作用。
    Ui,
    /// 只读查询：不修改任何状态。
    #[default]
    Query,
    /// 状态变更（会话、配置、工作区）：需要排队。
    Action,
    /// 网络/进程/文件 I/O 或模型循环：交互端应交给慢命令 worker。
    Background,
}

impl CommandType {
    /// 是否允许在 Agent 回合进行中立即执行。
    pub fn immediate(self) -> bool {
        matches!(self, CommandType::Ui | CommandType::Query)
    }

    /// 与 Python 枚举值一致的字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            CommandType::Ui => "ui",
            CommandType::Query => "query",
            CommandType::Action => "action",
            CommandType::Background => "background",
        }
    }
}

impl fmt::Display for CommandType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 命令声明非法（空命令名、重名或别名冲突）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandParseError {
    message: String,
}

impl CommandParseError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for CommandParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CommandParseError {}

/// 交互入口：决定命令语义中的远端/本地差异。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Channel {
    /// 全屏终端工作台。
    #[default]
    Tui,
    Telegram,
    Feishu,
    /// 飞书自建应用入口。
    Fsapp,
    /// 其他远程通道。
    Remote,
    /// 未登记的通道标识（原样透传，与 Python 的自由字符串一致）。
    Other(String),
}

impl Channel {
    /// 按通道标识构造；未登记的标识落入 [`Channel::Other`]。
    pub fn parse(value: &str) -> Self {
        match value {
            "tui" => Channel::Tui,
            "telegram" => Channel::Telegram,
            "feishu" => Channel::Feishu,
            "fsapp" => Channel::Fsapp,
            "remote" => Channel::Remote,
            other => Channel::Other(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Channel::Tui => "tui",
            Channel::Telegram => "telegram",
            Channel::Feishu => "feishu",
            Channel::Fsapp => "fsapp",
            Channel::Remote => "remote",
            Channel::Other(other) => other.as_str(),
        }
    }

    /// 是否来自远程连接器（飞书 / Telegram）。
    ///
    /// 远程入口不得绕过工具审批，也不支持退出、设置面板这类本地交互。
    pub fn is_remote(&self) -> bool {
        matches!(
            self,
            Channel::Telegram | Channel::Feishu | Channel::Fsapp | Channel::Remote
        )
    }
}

/// 一行输入的解析结果。
///
/// `matched` 保存真正命中的名字或别名（已归一化、不含前导斜杠），
/// 处理器需要区分别名语义时可读取它。
#[derive(Debug, Clone)]
pub struct ParsedCommand {
    pub command: Arc<Command>,
    pub args: String,
    pub argv: Vec<String>,
    pub matched: String,
}

/// 延迟执行体：交互端把它交给工作线程，执行时把宿主能力面传进来。
///
/// Python 的 `deferred` 是无参闭包（在闭包里捕获了 agent）；Rust 这里改成「宿主当参数」，
/// 于是闭包只捕获自己需要的数据，宿主对象既不必包 `Arc`，也不必是 `Send + Sync`。
pub type DeferredCommand = Box<dyn FnOnce(&dyn CommandAgent) -> CommandResult + Send>;

/// 一次命令执行的结构化结果。
///
/// `message`/`error` 是面向用户的文本；`deferred` 用于把网络、进程、文件 I/O 或模型循环
/// 推迟到交互端的工作线程执行（见 `omnicrawl.ui.fullscreen` 的慢命令 worker）。其余字段是
/// 给交互层的调度提示，非交互入口（如连接器）可以直接忽略。
pub struct CommandResult {
    pub handled: bool,
    pub message: Option<String>,
    pub error: Option<String>,
    pub data: Option<Value>,
    // —— 交互层调度提示 ——
    pub refresh_context: bool,
    pub exit_requested: bool,
    pub open_settings: bool,
    pub open_config_chat: bool,
    pub clear_conversation: bool,
    pub replay_conversation: bool,
    pub workspace_switch_requested: bool,
    pub stream_subagent_conversation: bool,
    pub working_status: Option<String>,
    /// 延迟执行：交互端应在线程 worker 中调用它拿到真正结果。
    pub deferred: Option<DeferredCommand>,
}

impl Default for CommandResult {
    /// 处理器无输出时视为「已处理、无消息」。
    fn default() -> Self {
        Self {
            handled: true,
            message: None,
            error: None,
            data: None,
            refresh_context: false,
            exit_requested: false,
            open_settings: false,
            open_config_chat: false,
            clear_conversation: false,
            replay_conversation: false,
            workspace_switch_requested: false,
            stream_subagent_conversation: false,
            working_status: None,
            deferred: None,
        }
    }
}

impl fmt::Debug for CommandResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandResult")
            .field("handled", &self.handled)
            .field("message", &self.message)
            .field("error", &self.error)
            .field("refresh_context", &self.refresh_context)
            .field("exit_requested", &self.exit_requested)
            .field("deferred", &self.deferred.is_some())
            .finish()
    }
}

impl CommandResult {
    /// 未命中命令时由框架返回；调用方据此决定是否按普通对话处理。
    pub fn unhandled() -> Self {
        Self {
            handled: false,
            ..Self::default()
        }
    }

    /// 只有一条面向用户的消息。
    pub fn message(message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
            ..Self::default()
        }
    }

    /// 只有一条面向用户的错误文案。
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            error: Some(message.into()),
            ..Self::default()
        }
    }

    /// 是否成功；`error` 为空即视为成功。
    pub fn ok(&self) -> bool {
        self.error.is_none()
    }

    /// 执行延迟部分；无延迟时返回自身。
    ///
    /// 连接器等同步入口可以直接调用本方法把慢命令当普通命令执行。
    pub fn resolve(mut self, agent: &dyn CommandAgent) -> Self {
        if let Some(run) = self.deferred.take() {
            return run(agent);
        }
        self
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    pub fn with_error(mut self, message: impl Into<String>) -> Self {
        self.error = Some(message.into());
        self
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn with_refresh_context(mut self) -> Self {
        self.refresh_context = true;
        self
    }

    pub fn with_replay_conversation(mut self) -> Self {
        self.replay_conversation = true;
        self
    }

    pub fn with_clear_conversation(mut self) -> Self {
        self.clear_conversation = true;
        self
    }

    pub fn with_open_settings(mut self) -> Self {
        self.open_settings = true;
        self
    }

    pub fn with_open_config_chat(mut self) -> Self {
        self.open_config_chat = true;
        self
    }

    pub fn with_exit_requested(mut self) -> Self {
        self.exit_requested = true;
        self
    }

    pub fn with_workspace_switch_requested(mut self) -> Self {
        self.workspace_switch_requested = true;
        self
    }

    pub fn with_stream_subagent_conversation(mut self) -> Self {
        self.stream_subagent_conversation = true;
        self
    }

    pub fn with_working_status(mut self, status: impl Into<String>) -> Self {
        self.working_status = Some(status.into());
        self
    }

    pub fn with_deferred(mut self, run: DeferredCommand) -> Self {
        self.deferred = Some(run);
        self
    }
}

/// 命令执行上下文：处理器所需的输入与运行环境。
///
/// `agent` 是命令操作的宿主能力面（对映 Python 的 `LocalToolAgent`，也可能是测试替身）。
/// `channel` 标识来源入口，处理器据此落实通道能力差异。
pub struct CommandContext<'a> {
    pub agent: &'a dyn CommandAgent,
    pub command: Option<Arc<Command>>,
    pub raw: String,
    pub args: String,
    pub argv: Vec<String>,
    pub channel: Channel,
    /// 派生 SubAgent 任务（如 `/review`）的进度事件回调，可选。
    pub on_subagent_event: Option<SubagentEventCallback>,
}

impl CommandContext<'_> {
    /// 是否来自远程连接器（飞书 / Telegram）。
    pub fn is_remote(&self) -> bool {
        self.channel.is_remote()
    }

    /// 首个参数，无参数时为空串。
    pub fn arg(&self) -> &str {
        self.argv.first().map(String::as_str).unwrap_or("")
    }
}

/// 命令处理器。
pub type Handler = fn(&CommandContext<'_>) -> CommandResult;

/// 一条可分发命令的声明。
///
/// 字段与设计文档一致：`name` / `aliases` / `description` / `usage` / `command_type` /
/// `arg_prompt` / `hidden` / `handler`。另有三个纯展示用扩展字段，用于表达本仓库既有
/// 命令的补全形态与参数提示：
///
/// - `completions`：共享同一处理器的额外补全/帮助形态（纯展示，不参与分发）。
/// - `usage` 缺省时由 `name` 生成 `/name`。
/// - `parameters`：可选参数 `(参数, 一句话说明)`；交互端在命令名后提示并补全 `--chat`
///   这类开关参数，处理器自行解析 `ctx.args`。
#[derive(Debug, Clone)]
pub struct Command {
    pub name: String,
    pub handler: Handler,
    pub aliases: Vec<String>,
    pub description: String,
    pub usage: String,
    pub command_type: CommandType,
    pub arg_prompt: Option<String>,
    pub hidden: bool,
    pub completions: Vec<String>,
    pub parameters: Vec<(String, String)>,
}

impl Command {
    /// 新建一条命令声明；名字与别名的合法性在 [`CommandRegistry::register`] 校验。
    pub fn new(name: impl Into<String>, handler: Handler) -> Self {
        Self {
            name: name.into(),
            handler,
            aliases: Vec::new(),
            description: String::new(),
            usage: String::new(),
            command_type: CommandType::default(),
            arg_prompt: None,
            hidden: false,
            completions: Vec::new(),
            parameters: Vec::new(),
        }
    }

    pub fn aliases<I, S>(mut self, aliases: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.aliases = aliases.into_iter().map(Into::into).collect();
        self
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    pub fn usage(mut self, usage: impl Into<String>) -> Self {
        self.usage = usage.into();
        self
    }

    pub fn command_type(mut self, command_type: CommandType) -> Self {
        self.command_type = command_type;
        self
    }

    pub fn arg_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.arg_prompt = Some(prompt.into());
        self
    }

    pub fn hidden(mut self) -> Self {
        self.hidden = true;
        self
    }

    pub fn completions<I, S>(mut self, completions: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.completions = completions.into_iter().map(Into::into).collect();
        self
    }

    pub fn parameters<I, K, V>(mut self, parameters: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.parameters = parameters
            .into_iter()
            .map(|(name, hint)| (name.into(), hint.into()))
            .collect();
        self
    }

    /// 帮助与补全中展示的命令串。
    pub fn display(&self) -> String {
        format!("/{}", self.name)
    }

    /// 用法文本；未声明时回退为 `/name`。
    pub fn effective_usage(&self) -> String {
        if self.usage.is_empty() {
            self.display()
        } else {
            self.usage.clone()
        }
    }

    /// 是否接受参数（决定补全插入后是否补空格）。
    pub fn takes_argument(&self) -> bool {
        self.arg_prompt.is_some()
    }
}

/// 交互端菜单元数据；结构沿用既有 TUI 契约。
///
/// `parameters` 只在命令声明了可选参数时出现（Python 侧内置命令总是带上该键、
/// Skill 候选则没有该键），因此这里用 `Option` 保持同一份 JSON 形状。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommandOption {
    pub command: String,
    pub insert: String,
    pub title: String,
    pub description: String,
    pub category: String,
    pub search: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Vec<(String, String)>>,
}

impl CommandOption {
    /// 运行期发现的 Skill 候选（对映 `build_slash_command_options` 的 Skill 分支）。
    pub fn skill(name: &str, description: &str) -> Self {
        let command = format!("/skill:{name}");
        let plain_name = name.replace('-', " ");
        Self {
            insert: command.clone(),
            title: name.to_string(),
            description: description.to_string(),
            category: SKILL_CATEGORY.to_string(),
            search: format!("{command} /{name} {plain_name} {description}"),
            command,
            parameters: None,
        }
    }
}

/// 归一化命令名/别名：去前导斜杠与空白、大小写折叠。
///
/// Python 侧用 `str.casefold()`；命令名与别名都是 ASCII 或中文，`to_lowercase()` 与它同效。
pub fn normalize(token: &str) -> String {
    token.trim().trim_start_matches('/').trim().to_lowercase()
}

/// 命令注册表：唯一的命令事实来源。
///
/// 典型用法（声明式注册）::
///
/// ```text
/// let mut registry = CommandRegistry::new();
/// registry.register(
///     Command::new("compact", handle_compact)
///         .aliases(["c"])
///         .description("压缩当前会话上下文。")
///         .usage("/compact")
///         .command_type(CommandType::Background),
/// )?;
/// ```
#[derive(Debug, Default)]
pub struct CommandRegistry {
    commands: Vec<Arc<Command>>,
    index: HashMap<String, usize>,
    aliases: HashMap<String, usize>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    // ── 注册 ──────────────────────────────────────────────────

    /// 注册一条命令；重名或别名冲突立即报错，避免静默覆盖。
    ///
    /// 校验全部通过前不写入任何索引，因此失败时注册表保持原样。
    pub fn register(&mut self, command: Command) -> Result<Arc<Command>, CommandParseError> {
        let key = normalize(&command.name);
        if key.is_empty() {
            return Err(CommandParseError::new("命令名不能为空。"));
        }
        if self.index.contains_key(&key) || self.aliases.contains_key(&key) {
            return Err(CommandParseError::new(format!(
                "命令重复注册：{}",
                command.name
            )));
        }

        let mut alias_keys: Vec<String> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        for alias in &command.aliases {
            let alias_key = normalize(alias);
            if alias_key.is_empty() {
                return Err(CommandParseError::new(format!(
                    "命令 {} 含空别名。",
                    command.name
                )));
            }
            if alias_key == key || seen.contains(&alias_key) {
                return Err(CommandParseError::new(format!(
                    "命令 {} 的别名冲突：{}",
                    command.name, alias
                )));
            }
            if self.index.contains_key(&alias_key) || self.aliases.contains_key(&alias_key) {
                return Err(CommandParseError::new(format!(
                    "命令 {} 的别名冲突：{}",
                    command.name, alias
                )));
            }
            seen.push(alias_key.clone());
            alias_keys.push(alias_key);
        }

        let command = Arc::new(command);
        let position = self.commands.len();
        self.commands.push(Arc::clone(&command));
        self.index.insert(key, position);
        for alias_key in alias_keys {
            self.aliases.insert(alias_key, position);
        }
        Ok(command)
    }

    // ── 解析 ──────────────────────────────────────────────────

    /// 按命令名或别名解析；未注册时返回 `None`。
    pub fn resolve(&self, token: &str) -> Option<Arc<Command>> {
        let key = normalize(token);
        if key.is_empty() {
            return None;
        }
        let position = self.index.get(&key).or_else(|| self.aliases.get(&key))?;
        self.commands.get(*position).cloned()
    }

    /// 解析一行输入；不是已注册命令时返回 `None`。
    ///
    /// `/name args` 与无斜杠的整词别名都支持；无斜杠输入不接受参数，
    /// 因此「普通对话文本」不会被误判成命令。
    pub fn parse(&self, text: &str) -> Option<ParsedCommand> {
        let stripped = text.trim();
        if stripped.is_empty() {
            return None;
        }
        let (token, args) = if let Some(rest) = stripped.strip_prefix('/') {
            // 与 Python 的 `stripped[1:].strip().partition(" ")` 同义：按第一个空格切一次。
            let rest = rest.trim();
            match rest.find(' ') {
                Some(index) => (
                    rest[..index].to_string(),
                    rest[index + 1..].trim().to_string(),
                ),
                None => (rest.to_string(), String::new()),
            }
        } else {
            // 无斜杠：仅整词匹配别名（如「退出」），避免把普通输入当命令。
            (stripped.to_string(), String::new())
        };
        let command = self.resolve(&token)?;
        Some(ParsedCommand {
            command,
            argv: args.split_whitespace().map(str::to_string).collect(),
            args,
            matched: normalize(&token),
        })
    }

    // ── 执行 ──────────────────────────────────────────────────

    /// 解析并分发一条输入；未命中原样返回 [`CommandResult::unhandled`]。
    pub fn dispatch(
        &self,
        text: &str,
        agent: &dyn CommandAgent,
        channel: Channel,
        on_subagent_event: Option<SubagentEventCallback>,
    ) -> CommandResult {
        let Some(parsed) = self.parse(text) else {
            return CommandResult::unhandled();
        };
        self.invoke(parsed, agent, text.trim(), channel, on_subagent_event)
    }

    /// 调用已解析命令的处理器。
    pub fn invoke(
        &self,
        parsed: ParsedCommand,
        agent: &dyn CommandAgent,
        raw: &str,
        channel: Channel,
        on_subagent_event: Option<SubagentEventCallback>,
    ) -> CommandResult {
        let raw = if raw.is_empty() {
            parsed.command.display()
        } else {
            raw.to_string()
        };
        let context = CommandContext {
            agent,
            command: Some(Arc::clone(&parsed.command)),
            raw,
            args: parsed.args.clone(),
            argv: parsed.argv.clone(),
            channel,
            on_subagent_event,
        };
        (parsed.command.handler)(&context)
    }

    // ── 列表与元数据 ──────────────────────────────────────────

    /// 按注册顺序返回命令列表。
    pub fn commands(&self, include_hidden: bool) -> Vec<Arc<Command>> {
        self.commands
            .iter()
            .filter(|command| include_hidden || !command.hidden)
            .cloned()
            .collect()
    }

    /// 返回所有可输入的命令串（含补全形态），用于 Tab 补全。
    pub fn display_names(&self, include_hidden: bool) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for command in self.commands(include_hidden) {
            names.push(command.display());
            names.extend(command.completions.iter().cloned());
        }
        names
    }

    /// 构造交互端菜单元数据；结构沿用既有 TUI 契约。
    ///
    /// `insert` 与命令名完全一致（命令补全不带尾随空格，需要参数时手动空格，
    /// 参数候选菜单照旧出现）。
    pub fn options(&self, include_hidden: bool) -> Vec<CommandOption> {
        let mut options: Vec<CommandOption> = Vec::new();
        for command in self.commands(include_hidden) {
            let mut displays = vec![command.display()];
            displays.extend(command.completions.iter().cloned());
            for display in displays {
                options.push(CommandOption {
                    insert: display.clone(),
                    title: display.clone(),
                    description: if command.description.is_empty() {
                        DEFAULT_COMMAND_DESCRIPTION.to_string()
                    } else {
                        command.description.clone()
                    },
                    category: COMMAND_CATEGORY.to_string(),
                    search: display.clone(),
                    parameters: Some(command.parameters.clone()),
                    command: display,
                });
            }
        }
        options
    }

    /// 渲染帮助列表；`hidden` 命令默认不出现。
    pub fn help_text(&self, include_hidden: bool) -> String {
        let mut lines = vec!["可用命令：".to_string()];
        for command in self.commands(include_hidden) {
            lines.push(format!(
                "  {:<30} {}",
                command.effective_usage(),
                command.description
            ));
            for extra in &command.completions {
                lines.push(format!("  {extra:<30} {}", command.description));
            }
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::testing::FakeAgent;

    fn handle_compact(ctx: &CommandContext<'_>) -> CommandResult {
        CommandResult::message(format!("compact:{}", ctx.args))
    }

    fn handle_echo(ctx: &CommandContext<'_>) -> CommandResult {
        CommandResult::message(format!(
            "{}|{}|{}|{}|{}",
            ctx.args,
            ctx.argv.len(),
            ctx.raw,
            ctx.channel.as_str(),
            ctx.arg()
        ))
    }

    fn handle_silent(_ctx: &CommandContext<'_>) -> CommandResult {
        CommandResult::default()
    }

    fn handle_slow(_ctx: &CommandContext<'_>) -> CommandResult {
        CommandResult::message("正在执行").with_deferred(Box::new(|_agent: &dyn CommandAgent| {
            CommandResult::message("done")
        }))
    }

    fn registry() -> CommandRegistry {
        let mut registry = CommandRegistry::new();
        registry
            .register(
                Command::new("compact", handle_compact)
                    .aliases(["c"])
                    .description("压缩当前会话上下文。")
                    .usage("/compact [--model]")
                    .command_type(CommandType::Background)
                    .arg_prompt("压缩方式")
                    .completions(["/compact --model"]),
            )
            .expect("compact 应可注册");
        registry
            .register(
                Command::new("echo", handle_echo)
                    .command_type(CommandType::Query)
                    .arg_prompt("文本"),
            )
            .expect("echo 应可注册");
        registry
            .register(Command::new("silent", handle_silent))
            .expect("silent 应可注册");
        registry
            .register(Command::new("slow", handle_slow))
            .expect("slow 应可注册");
        registry
            .register(
                Command::new("quit", handle_silent)
                    .aliases(["退出"])
                    .command_type(CommandType::Ui),
            )
            .expect("quit 应可注册");
        registry
            .register(
                Command::new("secret", handle_silent)
                    .description("内部命令。")
                    .hidden(),
            )
            .expect("secret 应可注册");
        registry
    }

    #[test]
    fn resolve_by_name_alias_and_slash() {
        let registry = registry();
        assert_eq!(registry.resolve("compact").unwrap().name, "compact");
        assert_eq!(registry.resolve("/compact").unwrap().name, "compact");
        assert_eq!(registry.resolve("C").unwrap().name, "compact");
        assert_eq!(registry.resolve("/退出").unwrap().name, "quit");
        assert!(registry.resolve("missing").is_none());
    }

    #[test]
    fn duplicate_name_is_rejected() {
        let mut registry = registry();
        let error = registry
            .register(Command::new("compact", handle_compact))
            .unwrap_err();
        assert_eq!(error.message(), "命令重复注册：compact");
    }

    #[test]
    fn alias_conflict_leaves_registry_untouched() {
        let mut registry = registry();
        assert!(registry
            .register(Command::new("other", handle_compact).aliases(["c", "new"]))
            .is_err());
        assert!(registry.resolve("other").is_none());
        assert!(registry.resolve("new").is_none());
    }

    #[test]
    fn empty_name_is_rejected() {
        let mut registry = registry();
        let error = registry
            .register(Command::new("   ", handle_compact))
            .unwrap_err();
        assert_eq!(error.message(), "命令名不能为空。");
    }

    #[test]
    fn parse_splits_name_and_arguments() {
        let parsed = registry()
            .parse("/compact --model now")
            .expect("应解析成功");
        assert_eq!(parsed.command.name, "compact");
        assert_eq!(parsed.args, "--model now");
        assert_eq!(parsed.argv, vec!["--model", "now"]);
        assert_eq!(parsed.matched, "compact");
    }

    #[test]
    fn parse_command_without_arguments() {
        let parsed = registry().parse("  /compact  ").expect("应解析成功");
        assert_eq!(parsed.args, "");
        assert!(parsed.argv.is_empty());
    }

    #[test]
    fn plain_sentence_is_not_a_command() {
        let registry = registry();
        assert!(registry.parse("帮我看看这个 bug").is_none());
        assert!(registry.parse("").is_none());
        assert!(registry.parse("/unknown").is_none());
    }

    #[test]
    fn bare_alias_matches_only_as_whole_word() {
        let registry = registry();
        assert_eq!(registry.parse("退出").unwrap().command.name, "quit");
        // 带参数的整词别名不再匹配，避免把闲聊误判成命令。
        assert!(registry.parse("退出 了").is_none());
    }

    #[test]
    fn alias_carries_arguments_through() {
        let parsed = registry().parse("/c --model").expect("应解析成功");
        assert_eq!(parsed.command.name, "compact");
        assert_eq!(parsed.args, "--model");
    }

    #[test]
    fn unhandled_text_returns_sentinel() {
        let agent = FakeAgent::default();
        let result = registry().dispatch("普通对话", &agent, Channel::Tui, None);
        assert!(!result.handled);
        assert!(result.message.is_none());
    }

    #[test]
    fn handler_receives_parsed_context() {
        let agent = FakeAgent::default();
        let result = registry().dispatch("/echo hello world", &agent, Channel::Telegram, None);
        assert!(result.handled);
        assert_eq!(
            result.message.as_deref(),
            Some("hello world|2|/echo hello world|telegram|hello")
        );
    }

    #[test]
    fn remote_flag_reflects_channel() {
        assert!(Channel::parse("telegram").is_remote());
        assert!(Channel::parse("fsapp").is_remote());
        assert!(!Channel::parse("tui").is_remote());
        assert!(!Channel::Other("web".to_string()).is_remote());
    }

    #[test]
    fn silent_handler_is_handled_without_message() {
        let agent = FakeAgent::default();
        let result = registry().dispatch("/silent", &agent, Channel::Tui, None);
        assert!(result.handled);
        assert!(result.message.is_none());
    }

    #[test]
    fn deferred_result_runs_lazily() {
        let agent = FakeAgent::default();
        let result = registry().dispatch("/slow", &agent, Channel::Tui, None);
        assert_eq!(result.message.as_deref(), Some("正在执行"));
        assert!(result.ok());
        let resolved = result.resolve(&agent);
        assert_eq!(resolved.message.as_deref(), Some("done"));
    }

    #[test]
    fn resolve_without_deferred_keeps_message() {
        let agent = FakeAgent::default();
        let result = registry().dispatch("/silent", &agent, Channel::Tui, None);
        assert!(result.resolve(&agent).ok());
    }

    #[test]
    fn error_marks_result_not_ok() {
        assert!(!CommandResult::error("失败").ok());
        assert!(CommandResult::message("成功").ok());
    }

    #[test]
    fn immediate_only_for_ui_and_query() {
        assert!(CommandType::Ui.immediate());
        assert!(CommandType::Query.immediate());
        assert!(!CommandType::Action.immediate());
        assert!(!CommandType::Background.immediate());
        assert_eq!(CommandType::default(), CommandType::Query);
        assert_eq!(CommandType::Background.as_str(), "background");
    }

    #[test]
    fn display_names_include_completions_and_hidden() {
        let names = registry().display_names(true);
        assert!(names.contains(&"/compact".to_string()));
        assert!(names.contains(&"/compact --model".to_string()));
        assert!(names.contains(&"/secret".to_string()));
        assert_eq!(names.iter().filter(|name| *name == "/compact").count(), 1);
    }

    #[test]
    fn options_insert_matches_command_name() {
        let options = registry().options(false);
        let compact = options
            .iter()
            .find(|option| option.command == "/compact")
            .expect("compact 候选");
        assert_eq!(compact.insert, "/compact");
        assert_eq!(compact.category, COMMAND_CATEGORY);
        assert_eq!(compact.parameters.as_ref().map(Vec::len), Some(0));
        let completion = options
            .iter()
            .find(|option| option.command == "/compact --model")
            .expect("补全候选");
        assert_eq!(completion.insert, "/compact --model");
        assert_eq!(completion.description, "压缩当前会话上下文。");
        assert!(options.iter().all(|option| option.command != "/secret"));
    }

    #[test]
    fn skill_option_has_no_parameters_key() {
        let option = CommandOption::skill("bili-note", "提取 B 站内容");
        assert_eq!(option.command, "/skill:bili-note");
        assert_eq!(option.title, "bili-note");
        assert_eq!(option.category, SKILL_CATEGORY);
        assert!(option.parameters.is_none());
        assert!(option.search.contains("bili note"));
        let rendered = serde_json::to_string(&option).expect("应可序列化");
        assert!(!rendered.contains("parameters"));
    }

    #[test]
    fn hidden_excluded_from_options_and_help() {
        let registry = registry();
        assert!(registry
            .options(false)
            .iter()
            .all(|option| option.command != "/secret"));
        assert!(!registry.help_text(false).contains("secret"));
        assert!(registry.help_text(true).contains("/secret"));
    }

    #[test]
    fn help_lists_usage_and_description() {
        let text = registry().help_text(false);
        assert!(text.starts_with("可用命令："));
        assert!(text.contains("/compact [--model]"));
        assert!(text.contains("压缩当前会话上下文。"));
        assert!(text.contains("/compact --model"));
    }

    #[test]
    fn normalize_strips_slash_and_folds_case() {
        assert_eq!(normalize("  /CoMpAct "), "compact");
        assert_eq!(normalize("///Quit"), "quit");
    }
}
