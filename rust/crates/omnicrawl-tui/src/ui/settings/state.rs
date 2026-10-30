//! 设置面板的状态机：左侧一级项、右侧二级面板、焦点与键盘语义。
//!
//! 键盘语义按 Python 侧逐项对映：
//! - 左侧列表：`↑`/`↓` 移动（循环）并实时切右侧面板；`Enter`/`→` 进入右侧；
//!   `Esc` 退出整个设置面板。
//! - 右侧 `Esc` 先回左侧；`←` 同义。
//! - 上下文页：两个字段（上下文长度、压缩阈值），`Tab` 切换字段；
//!   Textual `Select` 的键位是 `Enter`/`↓`/`Space`/`↑` 展开候选，
//!   展开后 `↑`/`↓` 移动、`Enter` 确认并立即保存、`Esc` 收起。
//! - 工具开关页：`↑`/`↓` 选行，`←`/`→`/`Enter`/`Space` 切换开关。

use crossterm::event::KeyCode;
use omnicrawl_config::features::approval::{
    approval_mode_label, normalize_approval_mode, APPROVAL_MODE_AUTO, APPROVAL_MODE_MANUAL,
    APPROVAL_MODE_REVIEW,
};
use omnicrawl_config::models::channels::{protocols_for_provider, provider_options};
use omnicrawl_onejev::ProgressSnapshot;
use ratatui::layout::Rect;
use std::cell::RefCell;

use crate::state::Composer;

use super::form::{paste_into, FieldKind, FieldSpec, FieldValue, FormKind, FormState, FORM_KINDS};
use super::hit::{HitAction, HitRegion};
use super::picker::ModelDiscovery;
use super::{
    choice_field_options, context_compaction_options, context_window_options,
    cycle_subagent_option, nearest_compaction_percent, nearest_context_window_tokens,
    normalize_reasoning, reasoning_label, row_label, REASONING_OPTIONS, ROW_ORDER,
    SUBAGENT_ADVANCED_SPECS,
};

mod apply_sync;

mod form_bridge;

mod pointer;

mod navigation;

mod view;

mod keys;

mod dropdown;

pub use form_bridge::*;

mod shared;

pub(crate) use shared::{mask_api_key, secret_draft, secret_draft_for};

pub(super) use shared::cycle_text;

mod decision_page;

pub use decision_page::{
    DecisionRow, DecisionField, DecisionDropdown, EmbeddingRow, EmbeddingField,
    DECISION_SWITCH_SECTION, DECISION_LOCAL_SECTION, DECISION_LOCAL_ROW_SIZE,
    DECISION_LOCAL_ROW_DEVICE, DECISION_LOCAL_ROW_ROOT, DECISION_LOCAL_ROW_DOWNLOADED,
    DECISION_LOCAL_ROW_ENV, DECISION_LOCAL_ROW_PROGRESS, DECISION_LOCAL_ROW_PREPARE,
    DECISION_LOCAL_ROW_DOWNLOAD, DECISION_LOCAL_ROW_DELETE, DECISION_LOCAL_ROW_REMOVE_ENV,
    DECISION_LOCAL_ROW_CHANNEL, DECISION_LOCAL_ROW_SERVICE, DECISION_LOCAL_ROW_START,
    DECISION_LOCAL_ROW_STOP, DECISION_LOCAL_ROW_COUNT, DECISION_PROGRESS_BAR_WIDTH,
    DECISION_MODE_OPTIONS, DECISION_LOCAL_DEVICE_OPTIONS, DECISION_LOCAL_ROW_LABELS,
    DecisionLocalRow, DecisionLocalValues, DecisionSwitchRow, DecisionLocalChange,
};

pub(crate) use decision_page::{DecisionForm, EmbeddingForm};

pub(super) use decision_page::{DecisionLocalState, DecisionModelsState, EmbeddingModelsState};

mod tts_page;

mod sessions_page;

mod sessions;

#[cfg(test)]
mod sessions_page_tests;

pub use sessions_page::{PendingDelete, SessionRow, SessionsState};

pub use tts_page::{
    TtsChange, TtsChannelDraft, TtsChannelValues, TtsDraft, TtsRowView, TtsState, TtsValues,
    TTS_GEMINI_VOICES, TTS_OPENAI_VOICES, TTS_PROVIDER_OPTIONS, TTS_ROW_COUNT, TTS_ROW_LABELS,
    TTS_SPEED_OPTIONS,
};

// 行号只被同目录的 `tts.rs` 与 `tests.rs` 读，不进 `state` 的对外名字空间。
pub(super) use tts_page::{
    TTS_ROW_AUTO_PLAY, TTS_ROW_ENABLED, TTS_ROW_PROVIDER, TTS_ROW_SAVE, TTS_ROW_SPEAK_REPLIES,
    TTS_ROW_SPEED, TTS_ROW_VOICE,
};
// 只有 `tests.rs` 读这两行，非测试构建下不引入。
#[cfg(test)]
pub(super) use tts_page::{TTS_ROW_API_KEY, TTS_ROW_MODEL};

mod pages;

pub use pages::{
    cycle_approval_mode, cycle_mcp_timeout, nearest_approval_mode, McpEditorRowView, McpRowView,
    WebSearchRow, MASKED_SECRET, MCP_EDITOR_ARGS, MCP_EDITOR_COMMAND, MCP_EDITOR_ENV,
    MCP_EDITOR_FIELD_COUNT, MCP_EDITOR_HEADERS, MCP_EDITOR_LABELS, MCP_EDITOR_NAME,
    MCP_EDITOR_RISK, MCP_EDITOR_TIMEOUT, MCP_EDITOR_TRANSPORT, MCP_EDITOR_URL, MCP_RISK_OPTIONS,
    MCP_ROW_AUDIT, MCP_ROW_COMMAND, MCP_ROW_COUNT, MCP_ROW_ENABLED, MCP_ROW_LABELS, MCP_ROW_NETWORK,
    MCP_ROW_SERVERS, MCP_ROW_TIMEOUT, MCP_ROW_WRITE, MCP_TIMEOUT_OPTIONS, MCP_TRANSPORT_OPTIONS,
    TOOLS_APPROVAL_LABEL, TOOLS_APPROVAL_OPTIONS, TOOLS_SWITCH_ROW_OFFSET,
};

use pages::{ChoicesState, McpState, SubagentsState, ToolsState, VisionState};

pub(crate) use pages::{cycle_static, McpEditor};

mod tts;

mod mcp;

mod tools;

mod web_search;

mod vision;

mod model;

mod decision;

mod embedding;

/// 键盘焦点所在的一栏。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// 左侧一级项列表。
    List,
    /// 右侧二级面板。
    Pane,
}

/// 右侧当前挂载的二级面板。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// 模型管理：渠道列表 + 渠道表单，列表下方接「模型与参数」分区行。
    ModelManagement,
    /// 工具开关：逐工具启用/关闭。
    Tools,
    /// MCP 设置：全局策略 + Server 列表 / 编辑器。
    Mcp,
    /// 子任务设置：分组行（总开关 + 高级参数），选中即改。
    Subagents,
    /// 视觉：模型原生视觉三态 + 代理开关 + 故障转移列表。
    Vision,
    /// 结构化决策模型：决策渠道列表 + 单条决策渠道表单。
    DecisionModels,
    /// 向量模型：向量渠道列表 + 单条向量渠道表单。
    EmbeddingModels,
    /// 单选页：一个下拉候选，选中即保存。
    Choice(ChoiceKind),
    /// 表单页：若干字段 + `Ctrl+S` 保存。
    Form(FormKind),
    /// TTS 设置页：开关 / 服务商 / 播放行为 + 当前服务商的接口参数。
    Tts,
    /// 会话管理页：列出会话并二次确认后删除（走内核 `session.delete`）。
    Sessions,
    /// 未知一级项（一级表与面板表不同步时的兜底）。
    Pending,
}

impl Pane {
    fn for_row(key: &str) -> Self {
        match key {
            "model_management" => Self::ModelManagement,
            "decision_models" => Self::DecisionModels,
            "embedding_models" => Self::EmbeddingModels,
            "advisor" => Self::Form(FormKind::Advisor),
            "tool_output_compression" => Self::Form(FormKind::ToolOutputCompression),
            "desensitization" => Self::Form(FormKind::Desensitization),
            "run_guard" => Self::Form(FormKind::RunGuard),
            "agent_workspace" => Self::Form(FormKind::AgentWorkspace),
            "image_gen" => Self::Form(FormKind::ImageGen),
            "computer_use" => Self::Form(FormKind::ComputerUse),
            "tools" => Self::Tools,
            "web_search" => Self::Form(FormKind::WebSearch),
            "mcp" => Self::Mcp,
            "subagents" => Self::Subagents,
            "vision" => Self::Vision,
            "show_thinking" => Self::Choice(ChoiceKind::ShowThinking),
            "memory" => Self::Choice(ChoiceKind::Memory),
            "plugins" => Self::Choice(ChoiceKind::Plugins),
            "tts" => Self::Tts,
            "sessions" => Self::Sessions,
            _ => Self::Pending,
        }
    }
}

/// 单选页：各自一个下拉候选，选中即保存。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChoiceKind {
    /// 思考显示：开启 / 关闭。
    ShowThinking,
    /// 记忆功能：开启 / 关闭。
    Memory,
    /// 插件功能：开启 / 关闭。
    Plugins,
}

impl ChoiceKind {
    pub fn index(self) -> usize {
        match self {
            Self::ShowThinking => 0,
            Self::Memory => 1,
            Self::Plugins => 2,
        }
    }

    /// 折叠框里显示的当前值（对映 Textual `Select` 显示选中项文案）。
    fn value(self, choices: &ChoicesState) -> OptionValue {
        match self {
            Self::ShowThinking => OptionValue::Flag(choices.show_thinking),
            Self::Memory => OptionValue::Flag(choices.memory),
            Self::Plugins => OptionValue::Flag(choices.plugins),
        }
    }
}

/// 模型管理页「模型与参数」分区里的行（选中即改，顺序即界面顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelParamRow {
    /// 当前模型渠道（选中即切换当前模型）。
    Channel,
    /// 上下文长度（Token）。
    Window,
    /// 上下文压缩阈值（百分比）。
    Compaction,
    /// 推理强度档位。
    Reasoning,
}

impl ModelParamRow {
    /// 分区里的行序。
    pub const ORDER: [ModelParamRow; 4] = [
        ModelParamRow::Channel,
        ModelParamRow::Window,
        ModelParamRow::Compaction,
        ModelParamRow::Reasoning,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Channel => "当前模型渠道",
            Self::Window => "上下文长度",
            Self::Compaction => "上下文阈值",
            Self::Reasoning => "推理强度",
        }
    }
}

/// 模型管理页「模型与参数」分区的标题。
pub const MODEL_PARAM_SECTION: &str = "模型与参数";

/// 工具页首行的行号：工具调用审查模式。
pub const TOOLS_APPROVAL_ROW: usize = 0;

/// 一把工具开关的可显示状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSwitchRow {
    pub name: String,
    pub label: String,
    pub enabled: bool,
    /// 是否已注册进当前工具表；界面只画名称与开关状态，不再单独标注。
    pub registered: bool,
}

/// 子任务设置页的一行（对映 Python 的 `_SubagentsPane.sections`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentRow {
    pub key: String,
    pub label: String,
    /// 显示文本：开关是「已开启/已关闭」，高级参数是档位数字。
    pub value: String,
    /// 总开关行就地切换；其余行按档位循环。
    pub toggle: bool,
    /// 总开关行当前是否开启（其余行忽略）。
    pub active: bool,
    /// 所属分区标题（渲染层在同一分区的第一行前画出来）。
    pub section: &'static str,
}

/// 子任务设置的一次变更。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentChange {
    /// 子任务功能总开关。
    Enabled(bool),
    /// 某个高级参数的档位（整数；配置侧按该键的类型写成整数或浮点）。
    Advanced { key: String, value: i64 },
}

/// MCP 设置页的一个 Server 行（对映 Python 的 `MCPServerListPane` 行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerRow {
    pub name: String,
    pub enabled: bool,
    pub transport: String,
    pub risk_level: String,
}

/// MCP 设置页的初始值（全局开关/策略 + Server 列表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSettingsValues {
    pub enabled: bool,
    pub allow_external_network_tools: bool,
    pub require_confirmation_for_write: bool,
    pub require_confirmation_for_command: bool,
    pub audit_log_enabled: bool,
    pub timeout_seconds: i64,
    pub servers: Vec<McpServerRow>,
}

impl Default for McpSettingsValues {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_external_network_tools: false,
            require_confirmation_for_write: true,
            require_confirmation_for_command: true,
            audit_log_enabled: true,
            timeout_seconds: 30,
            servers: Vec::new(),
        }
    }
}

/// 一个 MCP Server 的编辑草稿（新增或修改）。
///
/// `env` / `headers` 在界面上是大小写敏感的 `KEY=VALUE` 列表（分号分隔），
/// 原样交给宿主解析；`original_name` 非空表示这是对已有条目的重命名。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct McpServerDraft {
    pub name: String,
    pub enabled: bool,
    pub transport: String,
    pub command: String,
    pub args: String,
    pub url: String,
    pub timeout_seconds: i64,
    pub risk_level: String,
    pub env: String,
    pub headers: String,
    pub original_name: Option<String>,
}

impl McpServerDraft {
    /// 从已有行建草稿（`原值` 只能拿得到展示字段，其余保持空/默认，宿主保存时补全）。
    pub fn from_row(row: &McpServerRow) -> Self {
        Self {
            name: row.name.clone(),
            enabled: row.enabled,
            transport: row.transport.clone(),
            risk_level: row.risk_level.clone(),
            original_name: Some(row.name.clone()),
            ..Self::default()
        }
    }
}

/// MCP 设置的一次变更。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpChange {
    /// 全局开关与策略（Server 列表不动）。
    Globals {
        enabled: bool,
        allow_external_network_tools: bool,
        require_confirmation_for_write: bool,
        require_confirmation_for_command: bool,
        audit_log_enabled: bool,
        timeout_seconds: i64,
    },
    /// 新增/修改一个 Server（按 `original_name` 重命名）。
    SaveServer(Box<McpServerDraft>),
    /// 启用/禁用一个 Server。
    SetServerEnabled { name: String, enabled: bool },
    /// 删除一个 Server。
    DeleteServer { name: String },
}

/// 一条视觉模型引用（对映 Python 的 `ActiveModelRef` 在界面上的投影）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisionModelRef {
    pub source: String,
    pub key: String,
    pub profile: String,
    pub model_id: String,
    pub protocol: String,
}

impl VisionModelRef {
    /// 从渠道 key 建一条 `custom` 引用（界面只在这里产生新条目）。
    pub fn custom(key: &str) -> Self {
        Self {
            source: "custom".to_string(),
            key: key.to_string(),
            profile: String::new(),
            model_id: String::new(),
            protocol: String::new(),
        }
    }

    /// 行里显示的文案（对映 Python 的 `_model_ref_label`）。
    pub fn label(&self) -> String {
        if self.source == "custom" {
            return if self.key.is_empty() {
                "custom/unknown".to_string()
            } else {
                self.key.clone()
            };
        }
        if !self.profile.is_empty() && !self.model_id.is_empty() {
            return format!("{}/{}", self.profile, self.model_id);
        }
        if !self.model_id.is_empty() {
            return self.model_id.clone();
        }
        if !self.profile.is_empty() {
            return self.profile.clone();
        }
        "unknown".to_string()
    }
}

/// 视觉设置的一次保存：代理开关 + 故障转移列表 + 可选的模型原生视觉改动。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisionChange {
    pub enabled: bool,
    pub models: Vec<VisionModelRef>,
    /// `Some(值)` 表示模型原生视觉这一轮有改动要写回（内层 `None` 代表删掉该键）。
    pub native: Option<Option<bool>>,
}

/// 一条模型渠道（对应 config.toml 的 `llm.profiles[profile_id]` 与 models.toml 的条目）。
///
/// 界面只持有这份纯数据；读盘、写盘与校验都在宿主侧（`omnicrawl-config`）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChannelRow {
    pub key: String,
    pub profile_id: String,
    pub name: String,
    pub provider: String,
    pub protocol: String,
    pub base_url: String,
    /// 渠道的内联密钥。只用于「是否已配置 + 掩码显示」与保存时的写回：
    /// 界面从不渲染明文（`channel_field_value` 走 [`mask_api_key`]），输入态也从空开始。
    pub api_key: String,
    pub model_id: String,
    pub user_agent: String,
    pub enabled: bool,
}

/// 渠道表单的字段顺序（与渲染、`Tab` 顺序一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelField {
    Name,
    Provider,
    Protocol,
    BaseUrl,
    ApiKey,
    ModelId,
    UserAgent,
    Enabled,
}

impl ChannelField {
    /// 表单里的字段顺序。
    pub const ORDER: [ChannelField; 8] = [
        ChannelField::Name,
        ChannelField::Provider,
        ChannelField::Protocol,
        ChannelField::BaseUrl,
        ChannelField::ApiKey,
        ChannelField::ModelId,
        ChannelField::UserAgent,
        ChannelField::Enabled,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "渠道名称",
            // 对映 Python `ChannelEditorScreen` 的「请求方式」标签（选项值仍是 provider key）。
            Self::Provider => "请求方式",
            Self::Protocol => "请求协议",
            Self::BaseUrl => "Base URL",
            Self::ApiKey => "API Key",
            Self::ModelId => "模型 ID",
            Self::UserAgent => "User-Agent（可选）",
            Self::Enabled => "启用",
        }
    }

    /// 文本字段（可进入编辑态）；枚举与开关不是。
    pub fn is_text(self) -> bool {
        !matches!(self, Self::Provider | Self::Protocol | Self::Enabled)
    }

    fn index(self) -> usize {
        Self::ORDER
            .iter()
            .position(|field| *field == self)
            .unwrap_or(0)
    }
}

/// 打开设置面板时的初始值（由宿主从配置与工具表读出来后传入）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsValues {
    pub context_window_tokens: i64,
    pub compaction_percent: i64,
    /// 当前模型渠道 key（取值来自 channel 列表的 key）。
    pub model_key: String,
    /// 推理强度档位（归一化后），显示在模型管理页的参数分区里。
    pub reasoning_effort: String,
    /// 渠道页的初始列表、默认渠道与「新建」模板（模板由宿主从配置的默认渠道生成）。
    pub channels: Vec<ChannelRow>,
    pub default_channel_key: String,
    pub channel_template: ChannelRow,
    /// 结构化决策模型页的初始列表、默认渠道与「新建」模板。
    pub decision_channels: Vec<DecisionRow>,
    pub default_decision_key: String,
    pub decision_template: DecisionRow,
    /// 向量模型页的初始列表、默认渠道与「新建」模板。
    pub embedding_channels: Vec<EmbeddingRow>,
    pub default_embedding_key: String,
    pub embedding_template: EmbeddingRow,
    /// 向量召回总开关（`config.toml` 的 `[memory_embedding] enabled`）。
    pub embedding_enabled: bool,
    /// 联网搜索走向量模型排序的开关（`decision_models.toml` 的 `[features]`）。
    /// 决策模型页下方那一组功能开关的当前值（键与顺序取自配置域的开关表）。
    pub decision_switches: Vec<DecisionSwitchRow>,
    /// 决策模型页自部署分区的初值（尺寸清单、环境与服务状态）。
    pub decision_local: DecisionLocalValues,
    pub show_thinking: bool,
    pub memory_enabled: bool,
    pub plugins_enabled: bool,
    pub tools: Vec<ToolSwitchRow>,
    /// 工具调用审查模式（`manual` / `review` / `auto`），取自 `[approval] mode`。
    pub approval_mode: String,
    /// 联网搜索页的初值（默认引擎与各凭据是否已配置）。
    pub web_search: WebSearchRow,
    /// 子任务设置页的行（总开关 + 高级参数）。
    pub subagents: Vec<SubagentRow>,
    /// MCP 设置页的初始值（全局策略 + Server 列表）。
    pub mcp: McpSettingsValues,
    /// 视觉设置页的初始值：代理开关、故障转移列表与模型原生视觉三态值。
    pub vision_enabled: bool,
    pub vision_models: Vec<VisionModelRef>,
    pub vision_native: Option<bool>,
    /// 表单页的初始字段值（由宿主从配置读出后传入）。
    pub form_values: Vec<(FormKind, Vec<FieldValue>)>,
    /// TTS 页的初始值（通用键 + 两条通道的配置）。
    pub tts: TtsValues,
}

impl SettingsValues {
    /// 上下文长度与压缩比例都折算到候选档位后再进界面。
    ///
    /// 单选页取与 Python 侧一致的缺省值（推理强度 `none`、思考显示开启、
    /// 记忆与插件关闭），由 [`SettingsValues::with_choices`] / [`SettingsValues::with_model`]
    /// 按当前配置覆盖。
    pub fn new(
        context_window_tokens: i64,
        compaction_percent: i64,
        tools: Vec<ToolSwitchRow>,
    ) -> Self {
        Self {
            context_window_tokens: nearest_context_window_tokens(context_window_tokens),
            compaction_percent: nearest_compaction_percent(compaction_percent),
            model_key: String::new(),
            reasoning_effort: "none".to_string(),
            channels: Vec::new(),
            default_channel_key: String::new(),
            channel_template: ChannelRow::default(),
            decision_channels: Vec::new(),
            default_decision_key: String::new(),
            decision_template: DecisionRow::default(),
            embedding_channels: Vec::new(),
            default_embedding_key: String::new(),
            embedding_template: EmbeddingRow::default(),
            embedding_enabled: false,
            decision_switches: Vec::new(),
            decision_local: DecisionLocalValues::default(),
            show_thinking: true,
            memory_enabled: false,
            plugins_enabled: false,
            tools,
            approval_mode: APPROVAL_MODE_REVIEW.to_string(),
            web_search: WebSearchRow::default(),
            subagents: Vec::new(),
            mcp: McpSettingsValues::default(),
            vision_enabled: false,
            vision_models: Vec::new(),
            vision_native: None,
            form_values: Vec::new(),
            tts: TtsValues::default(),
        }
    }

    /// 覆盖模型管理页的渠道列表、默认渠道与「新建」模板。
    pub fn with_channels(
        mut self,
        channels: Vec<ChannelRow>,
        default_channel_key: &str,
        template: ChannelRow,
    ) -> Self {
        let fallback = channels
            .first()
            .map(|row| row.key.clone())
            .unwrap_or_default();
        self.default_channel_key = if default_channel_key.trim().is_empty() {
            fallback
        } else {
            default_channel_key.to_string()
        };
        self.channels = channels;
        self.channel_template = template;
        self
    }

    /// 覆盖决策模型页的初始列表、默认渠道与「新建」模板。
    pub fn with_decision_models(
        mut self,
        channels: Vec<DecisionRow>,
        default_key: &str,
        template: DecisionRow,
    ) -> Self {
        let fallback = channels
            .first()
            .map(|row| row.key.clone())
            .unwrap_or_default();
        self.default_decision_key = if default_key.trim().is_empty() {
            fallback
        } else {
            default_key.to_string()
        };
        self.decision_channels = channels;
        self.decision_template = template;
        self
    }

    /// 覆盖决策模型页下方的功能开关（未显式给出时按配置域的默认值铺一行一组开关）。
    pub fn with_decision_switches(mut self, switches: Vec<DecisionSwitchRow>) -> Self {
        self.decision_switches = switches;
        self
    }

    /// 覆盖向量模型页的初始列表、默认渠道、「新建」模板与两条开关。
    pub fn with_embedding_models(
        mut self,
        channels: Vec<EmbeddingRow>,
        default_key: &str,
        template: EmbeddingRow,
        enabled: bool,
    ) -> Self {
        let fallback = channels
            .first()
            .map(|row| row.key.clone())
            .unwrap_or_default();
        self.default_embedding_key = if default_key.trim().is_empty() {
            fallback
        } else {
            default_key.to_string()
        };
        self.embedding_channels = channels;
        self.embedding_template = template;
        self.embedding_enabled = enabled;
        self
    }

    /// 覆盖决策模型页自部署分区的初值。
    pub fn with_decision_local(mut self, values: DecisionLocalValues) -> Self {
        self.decision_local = values;
        self
    }

    /// 覆盖模型管理页的当前模型渠道。
    pub fn with_model(mut self, model_key: &str) -> Self {
        let fallback = if self.default_channel_key.trim().is_empty() {
            self.channels
                .first()
                .map(|row| row.key.clone())
                .unwrap_or_default()
        } else {
            self.default_channel_key.clone()
        };
        self.model_key = if model_key.trim().is_empty() {
            fallback
        } else {
            model_key.to_string()
        };
        self
    }

    /// 覆盖三个开关单选页与推理档位的初始值（宿主从配置与运行时读出当前值后调用）。
    pub fn with_choices(
        mut self,
        reasoning_effort: &str,
        show_thinking: bool,
        memory_enabled: bool,
        plugins_enabled: bool,
    ) -> Self {
        self.reasoning_effort = normalize_reasoning(reasoning_effort);
        self.show_thinking = show_thinking;
        self.memory_enabled = memory_enabled;
        self.plugins_enabled = plugins_enabled;
        self
    }

    /// TTS 页的初值（通用键 + 两条通道的配置）。
    pub fn with_tts(mut self, values: TtsValues) -> Self {
        self.tts = values;
        self
    }

    /// 联网搜索页的初值（默认引擎与各凭据是否已配置）。
    pub fn with_web_search(mut self, row: WebSearchRow) -> Self {
        self.web_search = row;
        self
    }

    /// 覆盖一页表单的初始字段值（宿主从配置读出当前值后调用）。
    pub fn with_form(mut self, kind: FormKind, values: Vec<FieldValue>) -> Self {
        self.form_values.push((kind, values));
        self
    }

    /// 覆盖子任务设置页的行（宿主从配置读出当前值后调用）。
    pub fn with_subagents(mut self, rows: Vec<SubagentRow>) -> Self {
        self.subagents = rows;
        self
    }

    /// 覆盖 MCP 设置页的初始值（宿主从配置读出后调用）。
    pub fn with_mcp(mut self, values: McpSettingsValues) -> Self {
        self.mcp = values;
        self
    }

    /// 覆盖视觉设置页的初始值（宿主从配置与 models.toml 读出后调用）。
    pub fn with_vision(
        mut self,
        enabled: bool,
        models: Vec<VisionModelRef>,
        native: Option<bool>,
    ) -> Self {
        self.vision_enabled = enabled;
        self.vision_models = models;
        self.vision_native = native;
        self
    }
}

/// 展开中的候选下拉（Textual `Select` 展开态的对映）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dropdown {
    pub field: DropdownField,
    pub selected: usize,
}

/// 能展开候选的字段：模型管理页的参数分区行、某个开关单选页、表单页某个字段，
/// 或视觉页的「A 添加」。表单页的「渠道」与「模型」两列各有一套候选，由字段类型区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropdownField {
    /// 模型管理页「模型与参数」分区里的一行。
    ModelParam(ModelParamRow),
    Choice(ChoiceKind),
    Form(usize),
    /// 视觉页的「A 添加视觉模型」候选（渠道列表）。
    VisionAdd,
}

impl DropdownField {
    /// 静态候选（开关类单选页）。
    ///
    /// 模型管理页的参数行、表单页的字段与视觉页的候选都随环境变化（渠道与模型列表
    /// 来自配置，表单项跟着表单页的字段表走），由 [`SettingsState::options_for`] 提供，
    /// 因此这里是 `None`。
    fn static_options(self) -> Option<Vec<(String, OptionValue)>> {
        match self {
            Self::Choice(kind) => Some(choice_field_options(kind)),
            Self::ModelParam(_) | Self::Form(_) | Self::VisionAdd => None,
        }
    }
}

/// 候选的取值：上下文页是整数，推理强度是文本，其余开关是布尔，模型页是渠道标识。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionValue {
    Int(i64),
    Text(String),
    Flag(bool),
    /// 模型/渠道标识（候选的显示文案另给，二者不同）。
    Model(String),
}

/// 候选取值在折叠框里的显示文本。
pub fn display_value(value: &OptionValue) -> String {
    match value {
        OptionValue::Text(text) => reasoning_label(text),
        OptionValue::Flag(true) => "开启".to_string(),
        OptionValue::Flag(false) => "关闭".to_string(),
        OptionValue::Int(number) => number.to_string(),
        OptionValue::Model(text) => text.clone(),
    }
}

/// 宿主需要执行的一次设置变更。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsChange {
    /// 切换模型渠道（值取渠道在 config.toml/models.toml 里的 key）。
    Model { key: String },
    /// 上下文长度（Token）；压缩阈值按当前百分比同步重算。
    ContextWindow { tokens: i64 },
    /// 压缩阈值百分比；按当前上下文长度换算 Token。
    CompactionPercent { percent: i64 },
    /// 单个内置工具开关。
    ToolSwitch { name: String, enabled: bool },
    /// 工具调用审查模式（`manual` / `review` / `auto`）。
    ToolApproval { mode: String },
    /// 子任务设置的一行（选中即改，没有保存键）。
    Subagent(SubagentChange),
    /// MCP 设置（全局策略或 Server 列表，保存后重建 MCP 连接）。
    Mcp(McpChange),
    /// 视觉设置：`Ctrl+S` 一次写入代理配置（必要时连带模型原生视觉）。
    Vision(VisionChange),
    /// 推理强度（归一化后的档位）。
    Reasoning { effort: String },
    /// 思考显示的开关。
    ShowThinking { enabled: bool },
    /// 功能开关：`key` 取 `memory` / `plugins`。
    Feature { key: String, enabled: bool },
    /// 保存整份渠道配置（渠道列表 + 默认渠道 key）。
    Channels {
        rows: Vec<ChannelRow>,
        default_key: String,
    },
    /// 保存整份结构化决策模型配置（决策渠道列表 + 默认渠道 key）。
    DecisionModels {
        rows: Vec<DecisionRow>,
        default_key: String,
    },
    /// 保存整份向量模型配置（向量渠道列表 + 默认渠道 key）。
    EmbeddingModels {
        rows: Vec<EmbeddingRow>,
        default_key: String,
    },
    /// 向量模型页顶部的总开关（`config.toml` 的 `[memory_embedding] enabled`）。
    EmbeddingEnabled { enabled: bool },
    /// 决策模型页的一个功能开关（键取自配置域的开关表）。
    DecisionSwitch { key: String, enabled: bool },
    /// 决策模型页的自部署分区：写尺寸/设备、准备环境、下载/删除尺寸、切渠道、启停服务。
    DecisionLocal(DecisionLocalChange),
    /// 保存一页表单：字段值按该页字段表的顺序给出。
    Form {
        kind: FormKind,
        values: Vec<FieldValue>,
    },
    /// TTS 页：保存草稿。
    Tts(TtsChange),
}

/// 状态机产出的界面事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsEvent {
    /// 关闭整个设置面板。
    Close,
    /// 应用一项设置（宿主负责写盘、推内核、按需重建工具表）。
    Apply(SettingsChange),
    /// 自动检测某个渠道的模型列表（宿主在后台发起，结果经
    /// [`SettingsState::set_channel_models`] 回填进「模型 ID」下拉）。
    DiscoverChannelModels {
        profile_id: String,
        provider: String,
        protocol: String,
        base_url: String,
        /// 渠道的内联密钥（可能为空）：发探测请求时用它鉴权。
        api_key: String,
        user_agent: String,
    },
    /// 删除一个会话（宿主走内核 `session.delete`，成功后回填新列表）。
    DeleteSession { session_id: String },
    /// 重新读取会话列表（宿主走内核 `session.list` 后回填）。
    RefreshSessions,
}

const TOOLS_HINT: &str = "↑↓ 选择  ←→/Enter/空格 切换或换档  Esc 返回";
const MCP_HINT: &str = "↑↓ 选择  ←→/Enter/空格 修改  S 管理 Server  Esc 返回";
const MCP_SERVERS_HINT: &str = "↑↓ 选择  Enter 编辑  Space 启用/禁用  A 添加  D 删除  Esc 返回";
const MCP_EDITOR_HINT: &str = "↑↓/Tab 换字段  ←→ 换档  Enter 编辑  Ctrl+S 保存  Esc 取消";
const SUBAGENTS_HINT: &str = "↑↓ 选择  ←→/Enter/空格 切换或换档  Esc 返回";
const VISION_HINT: &str =
    "↑↓ 选择  空格 启用/停用  A 添加  D 删除  N 原生视觉  Ctrl+↑↓ 排序  Ctrl+S 保存  Esc 返回";
const MODEL_LIST_HINT: &str = "↑↓ 选择渠道或参数  Enter 编辑或选择  N 新建  D 删除  Esc 返回";
const MODEL_FORM_HINT: &str = "↑↓/Tab 换字段  Enter 编辑或展开候选  Ctrl+S 保存  Esc 返回列表";
const TTS_HINT: &str = "↑↓ 选择  ←→/Enter/空格 切换  Enter 执行  Ctrl+S 保存  Esc 返回";
const FORM_HINT: &str = "↑↓/Tab 换字段  Enter 编辑或展开候选  Ctrl+S 保存  Esc 返回";
/// 工具输出压缩页的提示（模型选择与其余表单页同款，都是两个下拉）。
const COMPRESSION_HINT: &str = "↑↓/Tab 换字段  Enter 展开候选  Ctrl+S 保存  Esc 返回";
/// 联网搜索页的提示：凭据留空表示不改动磁盘上的原值。
const WEB_SEARCH_HINT: &str = "↑↓/Tab 换字段  Enter 编辑或展开候选  Ctrl+S 保存  Esc 返回（凭据留空＝不改动；引擎只由这里决定）";
const DECISION_LIST_HINT: &str =
    "↑↓ 选择  Enter/→ 编辑渠道、←/→ 切换开关  N 新建  D 删除  Esc 返回";
const DECISION_FORM_HINT: &str = "↑↓/Tab 换字段  Enter 编辑或展开候选  Ctrl+S 保存  Esc 返回列表";
const EMBEDDING_LIST_HINT: &str = "↑↓ 选择  Enter/→ 编辑渠道或切换开关  N 新建  D 删除  Esc 返回";
const EMBEDDING_FORM_HINT: &str = "↑↓/Tab 换字段  Enter 编辑  Ctrl+S 保存  Esc 返回列表";
/// 会话管理页：删除要按两次（先确认再执行），当前会话拒绝删除。
const SESSIONS_HINT: &str = "↑↓ 选择  D 删除（连按两次确认）  R 刷新  Esc 返回";

/// 图像生成页 `provider` 字段的下标（隐藏字段的判定依据，与 `form.rs` 的字段表对齐）。
const IMAGE_GEN_PROVIDER_INDEX: usize = 1;
/// 图像生成页 `base_url` 字段的下标。
const IMAGE_GEN_BASE_URL_INDEX: usize = 2;
/// 图像生成页 `model` 字段的下标。
const IMAGE_GEN_MODEL_INDEX: usize = 4;

/// 按 [`FORM_KINDS`] 的顺序建好每页表单；宿主没给初值的页用空草稿。
///
/// 图像生成页的可见字段取决于服务商草稿，因此建好后再同步一次隐藏状态。
fn form_states(values: Vec<(FormKind, Vec<FieldValue>)>) -> Vec<FormState> {
    let mut forms: Vec<FormState> = FORM_KINDS
        .iter()
        .map(|kind| FormState::new(*kind, Vec::new()))
        .collect();
    for (kind, fields) in values {
        if let Some(slot) = forms.get_mut(kind.index()) {
            *slot = FormState::new(kind, fields);
        }
    }
    if let Some(form) = forms.get_mut(FormKind::ImageGen.index()) {
        let gemini = form_provider_is_gemini(form);
        form.set_gemini(gemini);
    }
    forms
}

/// 图像生成页草稿里的 `provider` 是否为 `gemini`。
fn form_provider_is_gemini(form: &FormState) -> bool {
    form.values()
        .get(IMAGE_GEN_PROVIDER_INDEX)
        .map(|value| value.text().trim().eq_ignore_ascii_case("gemini"))
        .unwrap_or(false)
}

/// 表单页一行的只读视图（渲染层用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormFieldView {
    pub label: &'static str,
    /// 值的显示文本（开关是两态文案，候选与模型项是界面文案）。
    pub value: String,
    /// 该字段能展开候选。
    pub has_menu: bool,
    /// 该字段正在输入。
    pub editing: bool,
    /// 该字段是当前聚焦项。
    pub focused: bool,
}

/// 输入态在字段行里显示的宽度（对映渠道表单按剩余宽度截断的做法）。
const FORM_INPUT_WIDTH: u16 = 40;

/// 渠道表单里展开的枚举候选（Provider / 协议）。
#[derive(Debug, Clone)]
pub struct ChannelDropdown {
    pub field: ChannelField,
    pub options: Vec<String>,
    pub selected: usize,
}

/// 渠道表单：正在编辑的副本 + 字段游标 + 临时编辑态。
#[derive(Debug, Clone)]
struct ChannelForm {
    row: ChannelRow,
    field: ChannelField,
    /// 展开的枚举候选（Provider / 协议）。
    dropdown: Option<ChannelDropdown>,
    /// 文本字段的输入缓冲（`None` 表示只在字段之间移动）。
    input: Option<Composer>,
    /// 正在编辑列表里的哪一条；`None` 表示这是还没落盘的草稿。
    index: Option<usize>,
    /// 是否新建出来的渠道。
    is_new: bool,
}

impl ChannelForm {
    /// 粘贴：文本字段直接落到输入态（没有输入态就先按当前字段进，对映 Textual 常驻 `Input`）。
    ///
    /// 模型 ID 列在这里只当普通文本处理，不触发自动检测——检测归 `Enter`。
    fn paste(&mut self, text: &str) -> bool {
        if self.input.is_none() {
            if !self.field.is_text() {
                return false;
            }
            self.dropdown = None;
            self.input = Some(channel_input_composer(&self.row, self.field));
        }
        paste_into(self.input.as_mut(), text)
    }
}

/// 模型管理页的界面状态：渠道列表 + 可选的渠道表单 + 「模型与参数」分区。
///
/// 分区里的行与渠道列表共用一套选中逻辑：前 `rows.len()` 行是渠道，之后依次是
/// [`ModelParamRow::ORDER`] 的四行（`MODEL_PARAM_SECTION` 标题行不参与选中）。
#[derive(Debug, Clone)]
struct ModelManagementState {
    rows: Vec<ChannelRow>,
    default_key: String,
    selected: usize,
    form: Option<ChannelForm>,
    /// 「新建」用的模板（宿主从配置的默认渠道生成）。
    template: ChannelRow,
    /// 当前模型渠道 key（分区首行的取值）。
    model_key: String,
    window_tokens: i64,
    percent: i64,
    reasoning: String,
    status: String,
}

impl ModelManagementState {
    /// 分区行的起始行号（渠道行之后）。
    fn param_base(&self) -> usize {
        self.rows.len()
    }

    /// 渠道 + 分区行的总行数（标题行不计）。
    fn row_count(&self) -> usize {
        self.rows.len() + ModelParamRow::ORDER.len()
    }

    /// 选中行落在哪个分区参数上；选中的是渠道行时返回 `None`。
    fn param_selected(&self) -> Option<ModelParamRow> {
        ModelParamRow::ORDER
            .get(self.selected.checked_sub(self.param_base())?)
            .copied()
    }
}

pub struct SettingsState {
    selected: usize,
    focus: Focus,
    pane: Pane,
    dropdown: Option<Dropdown>,
    tools: ToolsState,
    mcp: McpState,
    subagents: SubagentsState,
    vision: VisionState,
    choices: ChoicesState,
    model_management: ModelManagementState,
    decision_models: DecisionModelsState,
    embedding_models: EmbeddingModelsState,
    /// 每个表单页一份草稿，下标与 `FormKind::index` 一致。
    forms: Vec<FormState>,
    /// 每个表单页一份模型发现状态，下标与 `FormKind::index` 一致（不带模型选择的页留空）。
    model_discoveries: Vec<ModelDiscovery>,
    /// TTS 页的草稿状态机。
    tts: TtsState,
    /// 会话管理页：会话列表与删除确认。
    sessions: SessionsState,
    /// 本次渲染记录下来的可点区域：渲染只拿 `&self`，所以用 `RefCell`。
    hits: RefCell<Vec<HitRegion>>,
    /// 鼠标悬停所在的行（事件层写、渲染层读，用来加亮那一行）。
    hover: Option<HitAction>,
}

impl SettingsState {
    pub fn new(values: SettingsValues) -> Self {
        let mut forms = form_states(values.form_values);
        // 联网搜索页的草稿由配置投影起步（引擎 + 凭据掩码），与其他表单页同一条初始化路径。
        if let Some(form) = forms.get_mut(FormKind::WebSearch.index()) {
            form.set_values(web_search::web_search_form_values(&values.web_search));
        }
        Self {
            selected: 0,
            focus: Focus::List,
            pane: Pane::for_row(ROW_ORDER[0]),
            dropdown: None,
            tools: ToolsState {
                rows: values.tools,
                selected: 0,
                approval: nearest_approval_mode(&values.approval_mode).to_string(),
                status: String::new(),
            },
            mcp: McpState {
                enabled: values.mcp.enabled,
                network: values.mcp.allow_external_network_tools,
                write: values.mcp.require_confirmation_for_write,
                command: values.mcp.require_confirmation_for_command,
                audit: values.mcp.audit_log_enabled,
                timeout: values.mcp.timeout_seconds,
                servers: values.mcp.servers,
                selected: 0,
                server_selected: 0,
                servers_view: false,
                editor: None,
                status: String::new(),
            },
            subagents: SubagentsState {
                rows: values.subagents,
                selected: 0,
                status: String::new(),
            },
            vision: VisionState {
                enabled: values.vision_enabled,
                models: values.vision_models,
                selected: 0,
                native: values.vision_native,
                native_previous: values.vision_native,
                status: String::new(),
            },
            choices: ChoicesState {
                show_thinking: values.show_thinking,
                memory: values.memory_enabled,
                plugins: values.plugins_enabled,
                status: Default::default(),
            },
            model_management: ModelManagementState {
                rows: values.channels,
                default_key: values.default_channel_key,
                selected: 0,
                form: None,
                template: values.channel_template,
                model_key: values.model_key,
                window_tokens: values.context_window_tokens,
                percent: values.compaction_percent,
                reasoning: values.reasoning_effort,
                status: String::new(),
            },
            decision_models: DecisionModelsState {
                rows: values.decision_channels,
                switches: values.decision_switches,
                local: DecisionLocalState::new(values.decision_local),
                default_key: values.default_decision_key,
                selected: 0,
                form: None,
                template: values.decision_template,
                status: String::new(),
            },
            embedding_models: EmbeddingModelsState {
                rows: values.embedding_channels,
                default_key: values.default_embedding_key,
                enabled: values.embedding_enabled,
                selected: 0,
                form: None,
                template: values.embedding_template,
                status: String::new(),
            },
            forms,
            model_discoveries: FORM_KINDS.iter().map(|_| ModelDiscovery::new()).collect(),
            tts: TtsState::new(values.tts),
            sessions: SessionsState::default(),
            hits: RefCell::new(Vec::new()),
            hover: None,
        }
    }
}

#[cfg(test)]
mod tests;
