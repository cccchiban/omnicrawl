//! 工具开关页、联网搜索页与 MCP 页的行表、草稿与状态。
//!
//! 从 `state.rs` 按页拆出：这三页的条目原先与导航类型混排在同一文件里，
//! 改动一页要在整份 `state.rs` 里来回找。

use super::*;

/// 工具开关页的界面状态。
#[derive(Debug, Clone)]
pub(super) struct ToolsState {
    rows: Vec<ToolSwitchRow>,
    selected: usize,
    /// 工具调用审查模式（`manual` / `review` / `auto`），与 `[approval] mode` 同源。
    approval: String,
    status: String,
}

/// 联网搜索页的初始投影：默认引擎与各凭据的配置状态（凭据本身不进界面）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchRow {
    pub engine: String,
    pub tavily_configured: bool,
    pub exa_configured: bool,
    pub firecrawl_configured: bool,
    pub anysearch_configured: bool,
    /// TinyFish API Key 是否已配置。它没有匿名通道，缺 Key 时引擎会如实报错。
    pub tinyfish_configured: bool,
    /// GitHub Token 是否已配置。它不影响引擎可用性，只影响额度。
    pub github_configured: bool,
}

impl Default for WebSearchRow {
    fn default() -> Self {
        Self {
            engine: "duckduckgo".to_string(),
            tavily_configured: false,
            exa_configured: false,
            firecrawl_configured: false,
            anysearch_configured: false,
            tinyfish_configured: false,
            github_configured: false,
        }
    }
}

/// 工具开关页首行的标签（工具调用审查模式）。
pub const TOOLS_APPROVAL_LABEL: &str = "工具调用审查";

/// 逐工具开关在工具页里的起始行号（首行是审查模式）。
pub const TOOLS_SWITCH_ROW_OFFSET: usize = 1;

/// 凭据字段在界面上的掩码占位：已配置的凭据都用它显示，草稿值等于它表示「不改动」。
pub const MASKED_SECRET: &str = "••••••••";

/// 审查模式的候选顺序（对映 Python 的 `_APPROVAL_OPTIONS`）。
pub const TOOLS_APPROVAL_OPTIONS: [&str; 3] = [
    APPROVAL_MODE_MANUAL,
    APPROVAL_MODE_REVIEW,
    APPROVAL_MODE_AUTO,
];

/// 把任意审批模式折算到候选档位（不认得的值落到默认的「自动审查」）。
pub fn nearest_approval_mode(mode: &str) -> &'static str {
    match normalize_approval_mode(mode) {
        Ok(normalized) => TOOLS_APPROVAL_OPTIONS
            .iter()
            .copied()
            .find(|option| *option == normalized)
            .unwrap_or(APPROVAL_MODE_REVIEW),
        Err(_) => APPROVAL_MODE_REVIEW,
    }
}

/// 在审查模式候选里按方向循环（对映 Python 的 `_change`）。
pub fn cycle_approval_mode(current: &str, direction: isize) -> &'static str {
    let current = nearest_approval_mode(current);
    let index = TOOLS_APPROVAL_OPTIONS
        .iter()
        .position(|option| *option == current)
        .unwrap_or(1);
    let count = TOOLS_APPROVAL_OPTIONS.len() as isize;
    TOOLS_APPROVAL_OPTIONS[((index as isize + direction).rem_euclid(count)) as usize]
}

/// MCP 设置页的行号（对映 Python `MCPSettingsPane._ROWS`）。
pub const MCP_ROW_ENABLED: usize = 0;
pub const MCP_ROW_NETWORK: usize = 1;
pub const MCP_ROW_WRITE: usize = 2;
pub const MCP_ROW_COMMAND: usize = 3;
pub const MCP_ROW_AUDIT: usize = 4;
pub const MCP_ROW_TIMEOUT: usize = 5;
pub const MCP_ROW_SERVERS: usize = 6;
/// MCP 全局设置的行数。
pub const MCP_ROW_COUNT: usize = 7;

/// MCP 全局设置的行标签（渲染与测试都读它）。
pub const MCP_ROW_LABELS: [&str; MCP_ROW_COUNT] = [
    "MCP 总开关",
    "外部网络工具",
    "写入操作确认",
    "命令操作确认",
    "审计日志",
    "默认超时",
    "MCP Server",
];

/// MCP 默认超时档位（秒，对映 Python 的 `MCP_TIMEOUT_OPTIONS`）。
pub const MCP_TIMEOUT_OPTIONS: [i64; 5] = [10, 30, 60, 120, 300];

/// Server 编辑器字段序号（渲染与键位处理共用）。
pub const MCP_EDITOR_NAME: usize = 0;
pub const MCP_EDITOR_TRANSPORT: usize = 1;
pub const MCP_EDITOR_COMMAND: usize = 2;
pub const MCP_EDITOR_ARGS: usize = 3;
pub const MCP_EDITOR_URL: usize = 4;
pub const MCP_EDITOR_TIMEOUT: usize = 5;
pub const MCP_EDITOR_RISK: usize = 6;
pub const MCP_EDITOR_ENV: usize = 7;
pub const MCP_EDITOR_HEADERS: usize = 8;
/// Server 编辑器的字段数。
pub const MCP_EDITOR_FIELD_COUNT: usize = 9;

/// Server 编辑器字段标签（渲染与测试都读它）。
pub const MCP_EDITOR_LABELS: [&str; MCP_EDITOR_FIELD_COUNT] = [
    "名称",
    "传输",
    "命令",
    "参数",
    "URL",
    "超时（秒）",
    "风险等级",
    "环境变量",
    "请求头",
];

/// 传输候选取值（对映 `MCP_TRANSPORT_STDIO` / `MCP_TRANSPORT_STREAMABLE_HTTP`）。
pub const MCP_TRANSPORT_OPTIONS: [&str; 2] = ["stdio", "streamable_http"];
/// 风险等级候选取值（对映 `MCP_RISK_*`）。
pub const MCP_RISK_OPTIONS: [&str; 3] = ["trusted", "restricted", "external"];

/// 在 MCP 超时档位表里按方向循环取值（当前值不在表里时先取最近一档）。
pub fn cycle_mcp_timeout(current: i64, direction: isize) -> i64 {
    let index = MCP_TIMEOUT_OPTIONS
        .iter()
        .enumerate()
        .min_by_key(|(_, option)| (**option - current).abs())
        .map(|(index, _)| index)
        .unwrap_or(0);
    let count = MCP_TIMEOUT_OPTIONS.len() as isize;
    MCP_TIMEOUT_OPTIONS[((index as isize + direction).rem_euclid(count)) as usize]
}

/// MCP 全局设置行的只读视图（渲染层用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpRowView {
    pub label: &'static str,
    pub value: String,
    pub selected: bool,
}

/// Server 编辑器一行字段的只读视图（渲染层用）。
pub struct McpEditorRowView {
    pub label: &'static str,
    pub value: String,
    pub editing: bool,
    pub focused: bool,
    pub has_menu: bool,
}

/// Server 的可编辑草稿 + 字段游标。
#[derive(Debug, Clone)]
pub(super) struct McpEditor {
    draft: McpServerDraft,
    focused: usize,
    /// 文本字段的输入缓冲（`None` 表示不在编辑态）。
    input: Option<Composer>,
    /// 新建（而非修改已有条目）。
    is_new: bool,
}

impl McpEditor {
    fn field_text(&self, index: usize) -> String {
        match index {
            MCP_EDITOR_NAME => self.draft.name.clone(),
            MCP_EDITOR_TRANSPORT => self.draft.transport.clone(),
            MCP_EDITOR_COMMAND => self.draft.command.clone(),
            MCP_EDITOR_ARGS => self.draft.args.clone(),
            MCP_EDITOR_URL => self.draft.url.clone(),
            MCP_EDITOR_TIMEOUT => self.draft.timeout_seconds.to_string(),
            MCP_EDITOR_RISK => self.draft.risk_level.clone(),
            MCP_EDITOR_ENV => self.draft.env.clone(),
            MCP_EDITOR_HEADERS => self.draft.headers.clone(),
            _ => String::new(),
        }
    }

    fn set_field(&mut self, index: usize, text: String) {
        match index {
            MCP_EDITOR_NAME => self.draft.name = text,
            MCP_EDITOR_COMMAND => self.draft.command = text,
            MCP_EDITOR_ARGS => self.draft.args = text,
            MCP_EDITOR_URL => self.draft.url = text,
            MCP_EDITOR_ENV => self.draft.env = text,
            MCP_EDITOR_HEADERS => self.draft.headers = text,
            MCP_EDITOR_TIMEOUT => {
                if let Ok(value) = text.trim().parse::<i64>() {
                    self.draft.timeout_seconds = value;
                }
            }
            _ => {}
        }
    }

    /// 文本字段进输入态（枚举/超时字段不支持自由输入）。
    fn begin_input(&mut self) {
        let index = self.focused;
        if matches!(
            index,
            MCP_EDITOR_TRANSPORT | MCP_EDITOR_RISK | MCP_EDITOR_TIMEOUT
        ) {
            return;
        }
        let mut composer = Composer::default();
        composer.insert(&self.field_text(index));
        self.input = Some(composer);
    }

    /// 粘贴：文本字段直接落到输入态（没有输入态就先按当前字段进）。
    fn paste(&mut self, text: &str) -> bool {
        if self.input.is_none() {
            self.begin_input();
            if self.input.is_none() {
                return false;
            }
        }
        paste_into(self.input.as_mut(), text)
    }

    fn commit_input(&mut self) {
        if let Some(mut composer) = self.input.take() {
            let text = composer.take();
            self.set_field(self.focused, text);
        }
    }

    fn input_key(&mut self, key: KeyCode) {
        let Some(composer) = self.input.as_mut() else {
            return;
        };
        match key {
            KeyCode::Enter => {
                let text = composer.take();
                self.input = None;
                self.set_field(self.focused, text);
            }
            KeyCode::Esc => self.input = None,
            KeyCode::Char(character) => composer.insert(&character.to_string()),
            KeyCode::Backspace => composer.backspace(),
            KeyCode::Delete => composer.delete(),
            KeyCode::Left => composer.move_left(),
            KeyCode::Right => composer.move_right(),
            KeyCode::Home => composer.move_home(),
            KeyCode::End => composer.move_end(),
            _ => {}
        }
    }
}

/// 在静态字符串候选里按方向循环（当前值不在表里时从首项起步）。
pub(super) fn cycle_static(options: &[&str], current: &str, direction: isize) -> String {
    if options.is_empty() {
        return current.to_string();
    }
    let index = options
        .iter()
        .position(|option| *option == current)
        .map(|index| index as isize)
        .unwrap_or(-direction);
    let next = (index + direction).rem_euclid(options.len() as isize) as usize;
    options[next].to_string()
}

/// MCP 设置页的界面状态（全局行 + Server 列表 + 编辑器）。
#[derive(Debug, Clone)]
pub(super) struct McpState {
    enabled: bool,
    network: bool,
    write: bool,
    command: bool,
    audit: bool,
    timeout: i64,
    servers: Vec<McpServerRow>,
    /// 全局行的游标。
    selected: usize,
    /// Server 列表的游标。
    server_selected: usize,
    /// `true` 表示当前停在 Server 列表（而不是全局行）。
    servers_view: bool,
    /// `Some` 表示正在编辑一个 Server。
    editor: Option<McpEditor>,
    status: String,
}

impl McpState {
    fn global_values(&self) -> Vec<String> {
        vec![
            if self.enabled {
                "已开启"
            } else {
                "已关闭"
            }
            .to_string(),
            if self.network {
                "已允许"
            } else {
                "已禁止"
            }
            .to_string(),
            if self.write {
                "需要确认"
            } else {
                "免确认"
            }
            .to_string(),
            if self.command {
                "需要确认"
            } else {
                "免确认"
            }
            .to_string(),
            if self.audit { "已开启" } else { "已关闭" }.to_string(),
            format!("{} 秒", self.timeout),
            format!("管理（{} 个）", self.servers.len()),
        ]
    }

    fn globals_change(&self) -> McpChange {
        McpChange::Globals {
            enabled: self.enabled,
            allow_external_network_tools: self.network,
            require_confirmation_for_write: self.write,
            require_confirmation_for_command: self.command,
            audit_log_enabled: self.audit,
            timeout_seconds: self.timeout,
        }
    }
}

/// 子任务设置页的界面状态。
#[derive(Debug, Clone)]
pub(super) struct SubagentsState {
    rows: Vec<SubagentRow>,
    selected: usize,
    status: String,
}

/// 视觉设置页的界面状态。
#[derive(Debug, Clone)]
pub(super) struct VisionState {
    enabled: bool,
    models: Vec<VisionModelRef>,
    selected: usize,
    /// 模型原生视觉的三态值：`None` 表示未配置（对映 Python 的 `_NATIVE_CYCLE`）。
    native: Option<bool>,
    /// 进面板时的原值：只有真正改过才写回。
    native_previous: Option<bool>,
    status: String,
}

/// 三个单选页的当前值与状态文本（Python 每个面板实例各存一份状态）。
#[derive(Debug, Clone)]
pub(super) struct ChoicesState {
    show_thinking: bool,
    memory: bool,
    plugins: bool,
    status: [String; 3],
}

impl ChoicesState {
    fn status_mut(&mut self, kind: ChoiceKind) -> &mut String {
        &mut self.status[kind.index()]
    }
}
