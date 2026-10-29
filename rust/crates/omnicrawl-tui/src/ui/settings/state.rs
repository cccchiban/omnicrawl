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

mod tts;

mod mcp;

mod tools;

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
            "mcp" => Self::Mcp,
            "subagents" => Self::Subagents,
            "vision" => Self::Vision,
            "show_thinking" => Self::Choice(ChoiceKind::ShowThinking),
            "memory" => Self::Choice(ChoiceKind::Memory),
            "plugins" => Self::Choice(ChoiceKind::Plugins),
            "tts" => Self::Tts,
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

/// 工具页第二行的行号：联网搜索（`Enter` 进入独立的联网搜索表单页）。
pub const TOOLS_WEB_SEARCH_ROW: usize = 1;

/// 一把工具开关的可显示状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSwitchRow {
    pub name: String,
    pub label: String,
    pub enabled: bool,
    /// 是否已注册进当前工具表；未注册的行标注「未注册」（与 Python 一致）。
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

/// 一条结构化决策渠道（对应 `decision_models.toml` 的 `channels.<key>`）。
///
/// 与 [`ChannelRow`] 同形但**不是同一套字段**：决策服务只有一种接口（choice / score /
/// noul 三种提问类型共用 `/v1/decide`），因此没有 Provider / 协议 / User-Agent，模型名
/// 也不是对话模型（`jev-latest` 或固定版本号）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DecisionRow {
    pub key: String,
    pub name: String,
    /// 服务类型，当前只有 `jev`。
    pub mode: String,
    pub base_url: String,
    /// 内联凭据：只用于「是否已配置 + 掩码显示」与保存时的写回，界面从不渲染明文。
    pub api_key: String,
    pub model: String,
    /// 请求失败后的重试次数（不含首次请求）。
    pub retry_count: i64,
    pub enabled: bool,
}

/// 决策渠道表单的字段顺序（与渲染、`Tab` 顺序一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionField {
    Name,
    Mode,
    BaseUrl,
    ApiKey,
    ModelId,
    RetryCount,
    Enabled,
}

impl DecisionField {
    /// 表单里的字段顺序。
    pub const ORDER: [DecisionField; 7] = [
        DecisionField::Name,
        DecisionField::Mode,
        DecisionField::BaseUrl,
        DecisionField::ApiKey,
        DecisionField::ModelId,
        DecisionField::RetryCount,
        DecisionField::Enabled,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "渠道名称",
            // 与对话渠道的「请求方式」同文案：这里放的是决策服务类型（当前只有 Jev）。
            Self::Mode => "请求方式",
            Self::BaseUrl => "Base URL",
            Self::ApiKey => "API Key",
            Self::ModelId => "模型 ID",
            Self::RetryCount => "失败重试次数",
            Self::Enabled => "启用",
        }
    }

    /// 文本字段（可进入编辑态）；枚举与开关不是。
    pub fn is_text(self) -> bool {
        !matches!(self, Self::Mode | Self::Enabled)
    }

    fn index(self) -> usize {
        Self::ORDER
            .iter()
            .position(|field| *field == self)
            .unwrap_or(0)
    }
}

/// 决策渠道表单里展开的枚举候选（当前只有「请求方式」）。
#[derive(Debug, Clone)]
pub struct DecisionDropdown {
    pub field: DecisionField,
    pub options: Vec<String>,
    pub selected: usize,
}

/// 决策渠道表单：正在编辑的副本 + 字段游标 + 临时编辑态。
#[derive(Debug, Clone)]
struct DecisionForm {
    row: DecisionRow,
    field: DecisionField,
    dropdown: Option<DecisionDropdown>,
    /// 文本字段的输入缓冲（`None` 表示只在字段之间移动）。
    input: Option<Composer>,
    /// 正在编辑列表里的哪一条；`None` 表示这是还没落盘的草稿。
    index: Option<usize>,
    is_new: bool,
}

impl DecisionForm {
    /// 粘贴：文本字段直接落到输入态（没有输入态就先按当前字段进）。
    fn paste(&mut self, text: &str) -> bool {
        if self.input.is_none() {
            if !self.field.is_text() {
                return false;
            }
            self.dropdown = None;
            self.input = Some(decision_input_composer(&self.row, self.field));
        }
        paste_into(self.input.as_mut(), text)
    }
}

/// 一条向量渠道（对应 `embedding_models.toml` 的 `channels.<key>`）。
///
/// 比决策渠道少一个「请求方式」：向量接口只有 OpenAI 兼容一种形状，全部走云端。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EmbeddingRow {
    pub key: String,
    pub name: String,
    pub base_url: String,
    /// 内联凭据：只用于「是否已配置 + 掩码显示」与保存时写回，界面从不渲染明文。
    pub api_key: String,
    pub model: String,
    /// 期望维度；0 表示由服务端决定。
    pub dimensions: i64,
    pub timeout_seconds: i64,
    pub enabled: bool,
}

/// 向量渠道表单的字段顺序（与渲染、`Tab` 顺序一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingField {
    Name,
    BaseUrl,
    ApiKey,
    ModelId,
    Dimensions,
    TimeoutSeconds,
    Enabled,
}

impl EmbeddingField {
    pub const ORDER: [EmbeddingField; 7] = [
        EmbeddingField::Name,
        EmbeddingField::BaseUrl,
        EmbeddingField::ApiKey,
        EmbeddingField::ModelId,
        EmbeddingField::Dimensions,
        EmbeddingField::TimeoutSeconds,
        EmbeddingField::Enabled,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "渠道名称",
            Self::BaseUrl => "Base URL",
            Self::ApiKey => "API Key",
            Self::ModelId => "模型 ID",
            Self::Dimensions => "向量维度",
            Self::TimeoutSeconds => "超时秒数",
            Self::Enabled => "启用",
        }
    }

    /// 文本字段（可进入编辑态）；开关不是。
    pub fn is_text(self) -> bool {
        !matches!(self, Self::Enabled)
    }

    fn index(self) -> usize {
        Self::ORDER
            .iter()
            .position(|field| *field == self)
            .unwrap_or(0)
    }
}

/// 向量渠道表单：正在编辑的副本 + 字段游标 + 临时编辑态。
#[derive(Debug, Clone)]
struct EmbeddingForm {
    row: EmbeddingRow,
    field: EmbeddingField,
    input: Option<Composer>,
    /// 正在编辑列表里的哪一条；`None` 表示这是还没落盘的草稿。
    index: Option<usize>,
    is_new: bool,
}

impl EmbeddingForm {
    fn paste(&mut self, text: &str) -> bool {
        if self.input.is_none() {
            if !self.field.is_text() {
                return false;
            }
            self.input = Some(embedding_input_composer(&self.row, self.field));
        }
        paste_into(self.input.as_mut(), text)
    }
}

/// 向量模型页：顶部总开关（向量召回）+ 渠道列表 + 单条渠道表单。
#[derive(Debug, Clone)]
struct EmbeddingModelsState {
    rows: Vec<EmbeddingRow>,
    default_key: String,
    /// 顶部总开关：`config.toml` 的 `[memory_embedding] enabled`。
    enabled: bool,
    /// 行游标：开关区在前，渠道随后。
    selected: usize,
    form: Option<EmbeddingForm>,
    /// 「新建」用的模板（宿主从配置的默认向量渠道生成）。
    template: EmbeddingRow,
    status: String,
}

impl EmbeddingModelsState {
    /// 向量召回总开关占第 0 行。
    const MEMORY_SWITCH_ROW: usize = 0;
    /// 开关行数：渠道从这一行开始。
    const SWITCH_ROWS: usize = 1;

    fn channel_index(selected: usize) -> Option<usize> {
        selected.checked_sub(Self::SWITCH_ROWS)
    }

    fn total_rows(&self) -> usize {
        Self::SWITCH_ROWS + self.rows.len()
    }
}

/// 决策模型页的「功能开关」分区标题（开关行画在渠道列表下方）。
pub const DECISION_SWITCH_SECTION: &str = "功能开关";

/// 决策模型页的「自部署（OneJev）」分区标题（画在功能开关下方）。
pub const DECISION_LOCAL_SECTION: &str = "自部署 OneJev";

/// 自部署分区里的行号（顺序即界面顺序）。
pub const DECISION_LOCAL_ROW_SIZE: usize = 0;
pub const DECISION_LOCAL_ROW_DEVICE: usize = 1;
pub const DECISION_LOCAL_ROW_ROOT: usize = 2;
pub const DECISION_LOCAL_ROW_DOWNLOADED: usize = 3;
pub const DECISION_LOCAL_ROW_ENV: usize = 4;
/// 后台任务进度行：无任务在跑时不画（行仍占位，但值为空）。
pub const DECISION_LOCAL_ROW_PROGRESS: usize = 5;
pub const DECISION_LOCAL_ROW_PREPARE: usize = 6;
pub const DECISION_LOCAL_ROW_DOWNLOAD: usize = 7;
pub const DECISION_LOCAL_ROW_DELETE: usize = 8;
/// 删除专用虚拟环境（venv），保留权重与下载缓存。
pub const DECISION_LOCAL_ROW_REMOVE_ENV: usize = 9;
pub const DECISION_LOCAL_ROW_CHANNEL: usize = 10;
pub const DECISION_LOCAL_ROW_SERVICE: usize = 11;
pub const DECISION_LOCAL_ROW_START: usize = 12;
pub const DECISION_LOCAL_ROW_STOP: usize = 13;
/// 自部署分区的行数。
pub const DECISION_LOCAL_ROW_COUNT: usize = 14;

/// 进度条宽度（显示列）：状态行里够看又不会挤掉阶段文案。
pub const DECISION_PROGRESS_BAR_WIDTH: usize = 24;

/// 结构化决策模型的服务类型候选（Jev 原生 / 对话补全 / OneJev 自部署）。
///
/// 与配置域 [`omnicrawl_config::features::decision_model::DECISION_MODES`] 同源：
/// 这里重导出成界面用的名字，避免界面层直接引用配置域常量。
pub const DECISION_MODE_OPTIONS: [&str; 3] =
    omnicrawl_config::features::decision_model::DECISION_MODES;

/// 自部署的推理设备候选（与配置域 [`omnicrawl_config::features::decision_model::LOCAL_DEVICE_OPTIONS`] 同源）。
pub const DECISION_LOCAL_DEVICE_OPTIONS: [&str; 3] =
    omnicrawl_config::features::decision_model::LOCAL_DEVICE_OPTIONS;

/// 自部署分区的行标签（渲染与测试都读它）。
pub const DECISION_LOCAL_ROW_LABELS: [&str; DECISION_LOCAL_ROW_COUNT] = [
    "部署尺寸",
    "推理设备",
    "数据目录",
    "已下载尺寸",
    "运行环境",
    "任务进度",
    "准备运行环境",
    "下载当前尺寸",
    "删除当前尺寸",
    "删除运行环境",
    "使用自部署渠道",
    "本地服务",
    "启动 / 重启服务",
    "停止服务",
];

/// 自部署分区里的一行（只读信息行 + 动作行）。
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionLocalRow {
    pub label: &'static str,
    pub value: String,
    /// 该行是可执行的动作用（渲染成按钮样式）。
    pub action: bool,
    /// 该行下方是否再画一条进度条。
    pub bar: bool,
    /// 进度条比例；`None` 时画沿轨道循环滑动的滑块（任务在跑但进度不可量化）。
    pub progress: Option<f64>,
    /// 进度条的量化文案（`1.2 GB / 2.2 GB`），空串表示没有。
    pub progress_detail: String,
}

/// 自部署分区的初值（宿主在打开面板时读配置、磁盘与服务状态给出）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecisionLocalValues {
    /// 当前部署尺寸键（清单里的写法）。
    pub size: String,
    /// `auto` / `cuda` / `cpu`。
    pub device: String,
    /// 数据根目录（显示用；空串表示默认目录）。
    pub root: String,
    /// 清单里的尺寸（键、文案、是否已下载）。
    pub sizes: Vec<(String, String, bool)>,
    /// 运行环境是否就绪（venv + qev）。
    pub env_ready: bool,
    /// 环境状态文案（缺什么、用哪个解释器）。
    pub env_status: String,
    /// 本地服务状态文案。
    pub service_status: String,
    /// 本地服务是否在运行（决定「停止服务」是否有意义）。
    pub service_running: bool,
    /// 正在跑的后台任务名（`None` 表示没有任务，界面据此放开重复动作）。
    pub task: Option<String>,
    /// 当前任务的进度快照。
    pub progress: ProgressSnapshot,
}

/// 决策模型的一个功能开关行（标签与默认值来自配置域的开关表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionSwitchRow {
    pub key: String,
    pub label: String,
    pub enabled: bool,
}

/// 决策模型页的界面状态：列表 + 可选的表单。
///
/// 列表态的行由「决策渠道 + 功能开关 + 自部署分区」拼成：渠道在前、开关随后、
/// 自部署行在最后；表单态只编辑渠道。
#[derive(Debug, Clone)]
struct DecisionModelsState {
    rows: Vec<DecisionRow>,
    switches: Vec<DecisionSwitchRow>,
    /// 自部署分区：尺寸/设备/已下载状态 + 环境与服务状态。
    local: DecisionLocalState,
    default_key: String,
    selected: usize,
    form: Option<DecisionForm>,
    /// 「新建」用的模板（宿主从配置的默认决策渠道生成）。
    template: DecisionRow,
    status: String,
}

/// 自部署分区的界面状态。
#[derive(Debug, Clone, Default)]
struct DecisionLocalState {
    size: String,
    device: String,
    root: String,
    sizes: Vec<(String, String, bool)>,
    env_ready: bool,
    env_status: String,
    service_status: String,
    service_running: bool,
    /// 后台任务进行中（由宿主给出的 `task` 推导）：界面据此守卫重复动作。
    busy: bool,
    /// 当前任务名（如「下载 0.8B」「准备运行环境」），无任务时为空。
    task: String,
    /// 后台任务的进度快照（每帧由宿主刷新）。
    progress: ProgressSnapshot,
}

impl DecisionLocalState {
    fn new(values: DecisionLocalValues) -> Self {
        let sizes = if values.sizes.is_empty() {
            // 清单由宿主给出；缺失时至少留一行提示，不让分区变成空白。
            Vec::new()
        } else {
            values.sizes
        };
        let busy = values.task.is_some();
        Self {
            size: values.size,
            device: values.device,
            root: values.root,
            sizes,
            env_ready: values.env_ready,
            env_status: values.env_status,
            service_status: values.service_status,
            service_running: values.service_running,
            busy,
            task: values.task.unwrap_or_default(),
            progress: values.progress,
        }
    }

    /// 进度行的文案：阶段说明 + 量化细节；没任务时为空（渲染层不画这行）。
    fn progress_text(&self) -> String {
        if !self.busy {
            return String::new();
        }
        let stage = self.progress.stage.trim();
        let detail = self.progress.detail();
        let head = if self.task.trim().is_empty() {
            "正在执行".to_string()
        } else {
            self.task.trim().to_string()
        };
        match (stage.is_empty(), detail.is_empty()) {
            (true, _) => format!("{head}…"),
            (false, true) => format!("{head}：{stage}"),
            (false, false) => format!("{head}：{stage}（{detail}）"),
        }
    }

    /// 当前尺寸在清单里的下标（找不到时 0）。
    fn size_index(&self) -> usize {
        self.sizes
            .iter()
            .position(|(key, _, _)| key == &self.size)
            .unwrap_or(0)
    }

    /// 当前尺寸的界面文案（清单里取，取不到就给键本身）。
    fn size_label(&self) -> String {
        self.sizes
            .get(self.size_index())
            .map(|(_, label, _)| label.clone())
            .unwrap_or_else(|| self.size.clone())
    }

    /// 当前尺寸是否已下载。
    fn size_downloaded(&self) -> bool {
        self.sizes
            .get(self.size_index())
            .map(|(_, _, ready)| *ready)
            .unwrap_or(false)
    }

    /// 「已下载尺寸」行的值：列出全部已下载的档位。
    fn downloaded_text(&self) -> String {
        let ready: Vec<&str> = self
            .sizes
            .iter()
            .filter(|(_, _, ready)| *ready)
            .map(|(key, _, _)| key.as_str())
            .collect();
        if ready.is_empty() {
            "（无）".to_string()
        } else {
            ready.join("、")
        }
    }

    /// 设备行的文案。
    fn device_text(&self) -> String {
        match self.device.as_str() {
            "cpu" => "CPU".to_string(),
            "cuda" => "CUDA".to_string(),
            _ => "自动（按已装 torch 能力）".to_string(),
        }
    }

    /// 环境行的文案。
    fn env_text(&self) -> String {
        if self.env_ready {
            "已就绪".to_string()
        } else if self.env_status.trim().is_empty() {
            "未安装".to_string()
        } else {
            self.env_status.clone()
        }
    }

    /// 按方向键循环尺寸（在清单里滚动）。
    fn cycle_size(&mut self, direction: isize) {
        if self.sizes.is_empty() {
            return;
        }
        let count = self.sizes.len() as isize;
        let index = ((self.size_index() as isize + direction).rem_euclid(count)) as usize;
        self.size = self.sizes[index].0.clone();
    }

    /// 按行号循环该行的档位（尺寸 / 设备）。
    fn cycle(&mut self, row: usize, direction: isize) {
        match row {
            DECISION_LOCAL_ROW_SIZE => self.cycle_size(direction),
            DECISION_LOCAL_ROW_DEVICE => self.cycle_device(direction),
            _ => {}
        }
    }

    /// 按方向键循环设备档位。
    fn cycle_device(&mut self, direction: isize) {
        let options = DECISION_LOCAL_DEVICE_OPTIONS;
        let count = options.len() as isize;
        let current = options
            .iter()
            .position(|option| *option == self.device)
            .unwrap_or(0) as isize;
        let index = ((current + direction).rem_euclid(count)) as usize;
        self.device = options[index].to_string();
    }

    /// 分区行（渲染层读它）。
    ///
    /// 只有「任务进度」一行带进度条：`progress` 为 `Some` 时画实心条，
    /// 为 `None` 但任务在跑时画沿轨道循环滑动的滑块（如等服务绑端口）。
    fn rows(&self) -> Vec<DecisionLocalRow> {
        let row = |index: usize, value: String, action: bool| DecisionLocalRow {
            label: DECISION_LOCAL_ROW_LABELS[index],
            value,
            action,
            bar: false,
            progress: None,
            progress_detail: String::new(),
        };
        let mut rows = Vec::with_capacity(DECISION_LOCAL_ROW_COUNT);
        rows.push(row(DECISION_LOCAL_ROW_SIZE, self.size_label(), false));
        rows.push(row(DECISION_LOCAL_ROW_DEVICE, self.device_text(), false));
        rows.push(row(
            DECISION_LOCAL_ROW_ROOT,
            if self.root.trim().is_empty() {
                "（默认目录）".to_string()
            } else {
                self.root.clone()
            },
            false,
        ));
        rows.push(row(
            DECISION_LOCAL_ROW_DOWNLOADED,
            self.downloaded_text(),
            false,
        ));
        rows.push(row(DECISION_LOCAL_ROW_ENV, self.env_text(), false));
        rows.push(DecisionLocalRow {
            label: DECISION_LOCAL_ROW_LABELS[DECISION_LOCAL_ROW_PROGRESS],
            value: self.progress_text(),
            action: false,
            // 任务在跑就画进度条：比例已知画实心条，未知画循环滑块（等服务绑端口）。
            bar: self.busy,
            progress: if self.busy {
                self.progress.ratio()
            } else {
                None
            },
            progress_detail: if self.busy {
                self.progress.detail()
            } else {
                String::new()
            },
        });
        rows.push(row(
            DECISION_LOCAL_ROW_PREPARE,
            if self.env_ready {
                "（已就绪，可重装）".to_string()
            } else {
                String::new()
            },
            true,
        ));
        rows.push(row(
            DECISION_LOCAL_ROW_DOWNLOAD,
            if self.size_downloaded() {
                "（已下载）".to_string()
            } else {
                String::new()
            },
            true,
        ));
        rows.push(row(
            DECISION_LOCAL_ROW_DELETE,
            if self.size_downloaded() {
                String::new()
            } else {
                "（未下载）".to_string()
            },
            true,
        ));
        rows.push(row(
            DECISION_LOCAL_ROW_REMOVE_ENV,
            if self.env_ready {
                String::new()
            } else {
                "（未安装）".to_string()
            },
            true,
        ));
        rows.push(row(
            DECISION_LOCAL_ROW_CHANNEL,
            "把当前尺寸切成决策渠道".to_string(),
            true,
        ));
        rows.push(row(
            DECISION_LOCAL_ROW_SERVICE,
            self.service_status.clone(),
            false,
        ));
        rows.push(row(
            DECISION_LOCAL_ROW_START,
            if self.service_running {
                "（运行中，可重启）".to_string()
            } else {
                String::new()
            },
            true,
        ));
        rows.push(row(
            DECISION_LOCAL_ROW_STOP,
            if self.service_running {
                "（停用本机共享服务）".to_string()
            } else {
                "（未运行）".to_string()
            },
            true,
        ));
        rows
    }
}

/// 自部署分区要宿主做的一件事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionLocalChange {
    /// 写回尺寸与设备（枚举行改档即保存）。
    Save { size: String, device: String },
    /// 一键安装专用虚拟环境（venv + torch CUDA + qev）。
    PrepareEnvironment,
    /// 删除专用虚拟环境（venv），保留权重与下载缓存。
    RemoveEnvironment,
    /// 下载某个尺寸的权重。
    Download { size: String },
    /// 删除某个尺寸的权重。
    Delete { size: String },
    /// 把决策渠道切到自部署（按当前尺寸建/改一条 `onejev` 渠道并设为默认）。
    UseChannel { size: String },
    /// 启动（或按当前尺寸重启）本地服务。
    StartService,
    /// 停止本地服务并释放显存。
    StopService,
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
    /// 工具页「联网搜索」行的初值（默认引擎与各凭据是否已配置）。
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

    /// 工具页「联网搜索」行的初值（默认引擎与各凭据是否已配置）。
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

// ---------- TTS 页（云服务通道） ----------

/// TTS 页的行号。前 4 行与通道无关，随后 6 行属于当前服务商。
pub const TTS_ROW_ENABLED: usize = 0;
pub const TTS_ROW_PROVIDER: usize = 1;
pub const TTS_ROW_AUTO_PLAY: usize = 2;
/// 回合结束后朗读助手回复（不经工具面，因此不触发审批）。
pub const TTS_ROW_SPEAK_REPLIES: usize = 3;

/// 通道字段区起点：以下行按当前服务商取值，含义随 `provider` 变。
pub const TTS_ROW_BASE_URL: usize = 4;
pub const TTS_ROW_MODEL: usize = 5;
pub const TTS_ROW_VOICE: usize = 6;
/// OpenAI 的 `instructions` / Gemini 的语气指令（作为正文前缀）。
pub const TTS_ROW_INSTRUCTIONS: usize = 7;
/// OpenAI 的 `speed`；Gemini 没有该参数，该行在 Gemini 下不显示。
pub const TTS_ROW_SPEED: usize = 8;
/// 当前服务商的 API Key：直接填明文，保存时写进对应段的 `api_key`。
pub const TTS_ROW_API_KEY: usize = 9;
pub const TTS_ROW_SAVE: usize = 10;
/// TTS 页的行数。
pub const TTS_ROW_COUNT: usize = 11;

/// 与通道无关的行标签（顺序与行号一致）。
pub const TTS_ROW_LABELS: [&str; TTS_ROW_COUNT] = [
    "启用 TTS",
    "服务商 provider",
    "合成完成后自动播放",
    "自动朗读回复",
    "接口地址 base_url",
    "模型 model",
    "音色 voice",
    "语气指令 instructions",
    "语速 speed",
    "API Key",
    "保存",
];

/// 服务商候选（取值即配置值，与 `[tts_cloud].provider` 一一对应）。
pub const TTS_PROVIDER_OPTIONS: [&str; 2] = ["openai", "gemini"];

/// 语速候选（OpenAI 的取值域是 0.25~4.0，常用的几档够用）。
pub const TTS_SPEED_OPTIONS: [&str; 5] = ["0.75", "1", "1.25", "1.5", "2"];

/// OpenAI 内置音色（13 个，逐项取自官方 TTS 文档的 voice 列表）。
pub const TTS_OPENAI_VOICES: [&str; 13] = [
    "alloy", "ash", "ballad", "coral", "echo", "fable", "nova", "onyx", "sage", "shimmer", "verse",
    "marin", "cedar",
];

/// Gemini TTS 预置音色（官方语音列表，取自 Gemini-TTS 文档）。
pub const TTS_GEMINI_VOICES: [&str; 30] = [
    "Zephyr",
    "Puck",
    "Charon",
    "Kore",
    "Fenrir",
    "Leda",
    "Orus",
    "Aoede",
    "Callirrhoe",
    "Autonoe",
    "Enceladus",
    "Iapetus",
    "Umbriel",
    "Algieba",
    "Despina",
    "Erinome",
    "Algenib",
    "Rasalgethi",
    "Laomedeia",
    "Achernar",
    "Alnilam",
    "Schedar",
    "Gacrux",
    "Pulcherrima",
    "Achird",
    "Zubenelgenubi",
    "Vindemiatrix",
    "Sadachbia",
    "Sadaltager",
    "Sulafat",
];

/// TTS 页要宿主做的一件事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TtsChange {
    /// 保存整页草稿（写 `[tts]` / `[tts_cloud]` 与当前服务商段，并让工具表跟着变）。
    Save(TtsDraft),
}

/// TTS 页保存时的草稿：通用键 + 当前服务商那一段的字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtsDraft {
    pub enabled: bool,
    pub auto_play: bool,
    pub speak_replies: bool,
    pub provider: String,
    /// 当前服务商的字段：`[tts_api]` 或 `[tts_gemini]` 其一。
    pub channel: TtsChannelDraft,
}

/// 当前服务商的通道字段（两家的字段集不同，因此分开表达）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TtsChannelDraft {
    OpenAi {
        base_url: String,
        model: String,
        voice: String,
        instructions: String,
        speed: String,
        /// 明文密钥；空串表示「不改动磁盘原值」。
        api_key: String,
    },
    Gemini {
        base_url: String,
        model: String,
        voice: String,
        instructions: String,
        /// 明文密钥；空串表示「不改动磁盘原值」。
        api_key: String,
    },
}

/// TTS 页的初值（宿主在打开面板时读配置给出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtsValues {
    pub enabled: bool,
    pub auto_play: bool,
    pub speak_replies: bool,
    pub provider: String,
    /// 两家的配置都带上：切换服务商行时界面立刻显示另一家的值。
    pub openai: TtsChannelValues,
    pub gemini: TtsChannelValues,
}

impl Default for TtsValues {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_play: true,
            speak_replies: false,
            provider: "openai".to_string(),
            openai: TtsChannelValues::openai_default(),
            gemini: TtsChannelValues::gemini_default(),
        }
    }
}

/// 一家的通道初值（字段并集：按服务商取用其中一部分）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TtsChannelValues {
    pub base_url: String,
    pub model: String,
    pub voice: String,
    pub instructions: String,
    /// OpenAI 的语速（文本形式，便于编辑往返）。
    pub speed: String,
    /// 已配置的密钥在界面上只显示掩码占位；输入态从空起步。
    pub api_key: String,
}

impl TtsChannelValues {
    fn openai_default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o-mini-tts".to_string(),
            voice: "alloy".to_string(),
            speed: "1".to_string(),
            ..Self::default()
        }
    }

    fn gemini_default() -> Self {
        Self {
            base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
            model: "gemini-2.5-flash-preview-tts".to_string(),
            voice: "Kore".to_string(),
            ..Self::default()
        }
    }
}

/// TTS 页一行在渲染层的视图。
#[derive(Debug, Clone, PartialEq)]
pub struct TtsRowView {
    pub label: &'static str,
    pub value: String,
    /// 该行是可执行的动作用（渲染成按钮样式）。
    pub action: bool,
    /// 该行在**当前可见行**里的下标（屏幕行号）；选中判据与它比，不与配置行号比。
    pub index: usize,
    /// 该行是否是当前选中行。
    pub focused: bool,
}

/// TTS 页的编辑草稿。
struct TtsState {
    enabled: bool,
    auto_play: bool,
    speak_replies: bool,
    provider: String,
    openai: TtsChannelValues,
    gemini: TtsChannelValues,
    focused: usize,
    /// 文本输入态：`(行号, 缓冲)`，对映 Python 的 `Input` 获得焦点。
    input: Option<(usize, Composer)>,
    status: String,
}

impl TtsState {
    fn new(values: TtsValues) -> Self {
        Self {
            enabled: values.enabled,
            auto_play: values.auto_play,
            speak_replies: values.speak_replies,
            provider: values.provider,
            openai: values.openai,
            gemini: values.gemini,
            focused: TTS_ROW_ENABLED,
            input: None,
            status: String::new(),
        }
    }

    /// 当前服务商的那一份通道草稿。
    fn channel(&self) -> &TtsChannelValues {
        if self.is_gemini() {
            &self.gemini
        } else {
            &self.openai
        }
    }

    /// 当前服务商那一份通道草稿的可变引用。
    fn channel_mut(&mut self) -> &mut TtsChannelValues {
        if self.is_gemini() {
            &mut self.gemini
        } else {
            &mut self.openai
        }
    }

    fn is_gemini(&self) -> bool {
        self.provider == "gemini"
    }

    /// 当前服务商可见的行（Gemini 没有语速）。
    fn visible_rows(&self) -> Vec<usize> {
        (0..TTS_ROW_COUNT)
            .filter(|row| *row != TTS_ROW_SPEED || !self.is_gemini())
            .collect()
    }

    fn current_voices(&self) -> Vec<String> {
        if self.is_gemini() {
            TTS_GEMINI_VOICES
                .iter()
                .map(|name| name.to_string())
                .collect()
        } else {
            TTS_OPENAI_VOICES
                .iter()
                .map(|name| name.to_string())
                .collect()
        }
    }

    fn draft(&self) -> TtsDraft {
        let channel = self.channel();
        let channel = if self.is_gemini() {
            TtsChannelDraft::Gemini {
                base_url: channel.base_url.clone(),
                model: channel.model.clone(),
                voice: channel.voice.clone(),
                instructions: channel.instructions.clone(),
                api_key: secret_draft(&channel.api_key),
            }
        } else {
            TtsChannelDraft::OpenAi {
                base_url: channel.base_url.clone(),
                model: channel.model.clone(),
                voice: channel.voice.clone(),
                instructions: channel.instructions.clone(),
                speed: channel.speed.clone(),
                api_key: secret_draft(&channel.api_key),
            }
        };
        TtsDraft {
            enabled: self.enabled,
            auto_play: self.auto_play,
            speak_replies: self.speak_replies,
            provider: self.provider.clone(),
            channel,
        }
    }

    fn row_text(&self, row: usize) -> String {
        // 输入态：该行显示缓冲文本与光标。
        if let Some((input_row, composer)) = &self.input {
            if *input_row == row {
                return format!("{}▌", composer.text());
            }
        }
        let channel = self.channel();
        match row {
            TTS_ROW_ENABLED => if self.enabled { "启用" } else { "停用" }.to_string(),
            TTS_ROW_PROVIDER => self.provider.clone(),
            TTS_ROW_AUTO_PLAY => if self.auto_play { "开启" } else { "关闭" }.to_string(),
            TTS_ROW_SPEAK_REPLIES => if self.speak_replies {
                "开启"
            } else {
                "关闭"
            }
            .to_string(),
            TTS_ROW_BASE_URL => channel.base_url.clone(),
            TTS_ROW_MODEL => channel.model.clone(),
            TTS_ROW_VOICE => channel.voice.clone(),
            TTS_ROW_INSTRUCTIONS => channel.instructions.clone(),
            TTS_ROW_SPEED => channel.speed.clone(),
            // 明文永不回显：已配置的密钥只显示掩码占位。
            TTS_ROW_API_KEY => match channel.api_key.trim() {
                "" => "（未配置）".to_string(),
                _ => MASKED_SECRET.to_string(),
            },
            _ => String::new(),
        }
    }

    fn rows(&self) -> Vec<TtsRowView> {
        self.visible_rows()
            .into_iter()
            .enumerate()
            .map(|(index, row)| TtsRowView {
                label: TTS_ROW_LABELS[row],
                value: self.row_text(row),
                action: row == TTS_ROW_SAVE || row == TTS_ROW_PROVIDER,
                index,
                focused: row == self.focused,
            })
            .collect()
    }

    /// 把选中行挪到相邻的可见行（隐藏行的位置会被跳过）。
    fn move_focus(&mut self, delta: isize) {
        let rows = self.visible_rows();
        let Some(index) = rows.iter().position(|row| *row == self.focused) else {
            self.focused = rows.first().copied().unwrap_or(TTS_ROW_ENABLED);
            return;
        };
        let next = (index as isize + delta).rem_euclid(rows.len() as isize) as usize;
        self.focused = rows[next];
    }

    fn cycle(&mut self, row: usize, direction: isize) {
        // 切换服务商后原来选中的行可能不再可见，落回第一行。
        let gemini = self.is_gemini();
        match row {
            TTS_ROW_ENABLED => self.enabled = !self.enabled,
            TTS_ROW_AUTO_PLAY => self.auto_play = !self.auto_play,
            TTS_ROW_SPEAK_REPLIES => self.speak_replies = !self.speak_replies,
            TTS_ROW_PROVIDER => {
                let options: Vec<String> =
                    TTS_PROVIDER_OPTIONS.iter().map(|v| v.to_string()).collect();
                self.provider = cycle_text(&options, &self.provider, direction);
                if !self.visible_rows().contains(&self.focused) {
                    self.focused = TTS_ROW_ENABLED;
                }
            }
            TTS_ROW_VOICE => {
                let voices = self.current_voices();
                let channel = if gemini {
                    &mut self.gemini
                } else {
                    &mut self.openai
                };
                channel.voice = cycle_text(&voices, &channel.voice, direction);
            }
            TTS_ROW_SPEED => {
                let options: Vec<String> =
                    TTS_SPEED_OPTIONS.iter().map(|v| v.to_string()).collect();
                self.openai.speed = cycle_text(&options, &self.openai.speed, direction);
            }
            _ => {}
        }
    }

    /// 保存成功：把草稿回落到新配置（界面显示值跟着更新）。
    fn apply_saved(&mut self, draft: &TtsDraft) {
        self.enabled = draft.enabled;
        self.auto_play = draft.auto_play;
        self.speak_replies = draft.speak_replies;
        self.provider = draft.provider.clone();
        match &draft.channel {
            TtsChannelDraft::OpenAi {
                base_url,
                model,
                voice,
                instructions,
                speed,
                api_key,
            } => {
                self.openai = TtsChannelValues {
                    base_url: base_url.clone(),
                    model: model.clone(),
                    voice: voice.clone(),
                    instructions: instructions.clone(),
                    speed: speed.clone(),
                    // 草稿里的密钥就是刚写盘的明文；界面仍只显示掩码。
                    api_key: api_key.clone(),
                };
            }
            TtsChannelDraft::Gemini {
                base_url,
                model,
                voice,
                instructions,
                api_key,
            } => {
                self.gemini = TtsChannelValues {
                    base_url: base_url.clone(),
                    model: model.clone(),
                    voice: voice.clone(),
                    instructions: instructions.clone(),
                    speed: self.gemini.speed.clone(),
                    api_key: api_key.clone(),
                };
            }
        }
    }

    /// 该行是否有输入态（只有文本行才有；开关候选类行就地切档）。
    fn editable(&self, row: usize) -> bool {
        matches!(
            row,
            TTS_ROW_BASE_URL | TTS_ROW_MODEL | TTS_ROW_INSTRUCTIONS | TTS_ROW_API_KEY
        )
    }

    fn begin_input(&mut self, row: usize) {
        if !self.editable(row) {
            return;
        }
        let mut composer = Composer::default();
        // 密钥不回显：进输入态也从空起步，留空即保持磁盘原值。
        if row != TTS_ROW_API_KEY {
            composer.set_text(&self.row_text(row));
        }
        self.input = Some((row, composer));
    }

    fn commit_input(&mut self) {
        let Some((row, composer)) = self.input.take() else {
            return;
        };
        let text = composer.text().to_string();
        let channel = self.channel_mut();
        match row {
            TTS_ROW_BASE_URL => channel.base_url = text,
            TTS_ROW_MODEL => channel.model = text,
            TTS_ROW_INSTRUCTIONS => channel.instructions = text,
            // 留空不改动磁盘原值（对映渠道表单的 API Key 行）。
            TTS_ROW_API_KEY => {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    channel.api_key = trimmed.to_string();
                }
            }
            _ => {}
        }
    }

    /// 粘贴：文本行直接落到输入态（没有输入态就先按当前行进；动作行不接收）。
    fn paste(&mut self, text: &str) -> bool {
        if self.input.is_none() {
            self.begin_input(self.focused);
            if self.input.is_none() {
                return false;
            }
        }
        match self.input.as_mut() {
            Some((_, composer)) => paste_into(Some(composer), text),
            None => false,
        }
    }
}

/// 凭据字段的草稿 → 写盘取值：掩码或空串都表示「不改动」，原样交给宿主判断。
fn secret_draft(draft: &str) -> String {
    let trimmed = draft.trim();
    if trimmed == MASKED_SECRET {
        String::new()
    } else {
        trimmed.to_string()
    }
}

/// 已配置的凭据以掩码进草稿；未配置的留空。
fn secret_draft_for(configured: bool) -> String {
    if configured {
        MASKED_SECRET.to_string()
    } else {
        String::new()
    }
}

/// 密钥在界面上的展示：只露末 4 位，其余打码；空值直接显示「（未配置）」。
pub(crate) fn mask_api_key(api_key: &str) -> String {
    let trimmed = api_key.trim();
    if trimmed.is_empty() {
        return "（未配置）".to_string();
    }
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= 4 {
        return "*".repeat(chars.len());
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{}…{}", "*".repeat(4), tail)
}

/// 在候选表里按方向循环取值；当前值不在候选表里时从首项起步。
fn cycle_text(options: &[String], current: &str, direction: isize) -> String {
    if options.is_empty() {
        return current.to_string();
    }
    let index = options
        .iter()
        .position(|option| option == current)
        .map(|index| index as isize)
        .unwrap_or(-direction);
    let next = (index + direction).rem_euclid(options.len() as isize) as usize;
    options[next].clone()
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
const WEB_SEARCH_HINT: &str = "↑↓/Tab 换字段  Enter 编辑或展开候选  Ctrl+S 保存  Esc 返回工具页（凭据留空＝不改动；引擎只由这里决定）";
const DECISION_LIST_HINT: &str =
    "↑↓ 选择  Enter/→ 编辑渠道、←/→ 切换开关  N 新建  D 删除  Esc 返回";
const DECISION_FORM_HINT: &str = "↑↓/Tab 换字段  Enter 编辑或展开候选  Ctrl+S 保存  Esc 返回列表";
const EMBEDDING_LIST_HINT: &str = "↑↓ 选择  Enter/→ 编辑渠道或切换开关  N 新建  D 删除  Esc 返回";
const EMBEDDING_FORM_HINT: &str = "↑↓/Tab 换字段  Enter 编辑  Ctrl+S 保存  Esc 返回列表";

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

/// 工具开关页的界面状态。
#[derive(Debug, Clone)]
struct ToolsState {
    rows: Vec<ToolSwitchRow>,
    selected: usize,
    /// 工具调用审查模式（`manual` / `review` / `auto`），与 `[approval] mode` 同源。
    approval: String,
    /// 联网搜索的当前配置（默认引擎 + 各凭据是否已配置，凭据本身不进界面）。
    web_search: WebSearchRow,
    status: String,
}

/// 联网搜索在工具页上的只读投影：默认引擎与各凭据的配置状态。
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

impl WebSearchRow {
    /// 工具页那一行的值文案：默认引擎，接口型引擎已配凭据时附一句说明。
    pub fn label(&self) -> String {
        let mut text = self.engine.clone();
        let mut configured: Vec<&str> = Vec::new();
        if self.tavily_configured {
            configured.push("Tavily 已配置");
        }
        if self.exa_configured {
            configured.push("Exa 已配置");
        }
        if self.firecrawl_configured {
            configured.push("Firecrawl 已配置");
        }
        if self.anysearch_configured {
            configured.push("AnySearch 已配置");
        }
        if self.tinyfish_configured {
            configured.push("TinyFish 已配置");
        }
        if self.github_configured {
            configured.push("GitHub 已配置");
        }
        if !configured.is_empty() {
            text.push_str(&format!("（{}）", configured.join("、")));
        }
        text
    }
}

/// 工具开关页首行的标签（工具调用审查模式）。
pub const TOOLS_APPROVAL_LABEL: &str = "工具调用审查";

/// 工具开关页第二行的标签（联网搜索设置页的入口）。
pub const TOOLS_WEB_SEARCH_LABEL: &str = "联网搜索";

/// 逐工具开关在工具页里的起始行号（前两行分别是审查模式与联网搜索）。
pub const TOOLS_SWITCH_ROW_OFFSET: usize = 2;

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
struct McpEditor {
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
fn cycle_static(options: &[&str], current: &str, direction: isize) -> String {
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
struct McpState {
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
struct SubagentsState {
    rows: Vec<SubagentRow>,
    selected: usize,
    status: String,
}

/// 视觉设置页的界面状态。
#[derive(Debug, Clone)]
struct VisionState {
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
struct ChoicesState {
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
    /// 本次渲染记录下来的可点区域：渲染只拿 `&self`，所以用 `RefCell`。
    hits: RefCell<Vec<HitRegion>>,
    /// 鼠标悬停所在的行（事件层写、渲染层读，用来加亮那一行）。
    hover: Option<HitAction>,
}

impl SettingsState {
    pub fn new(values: SettingsValues) -> Self {
        Self {
            selected: 0,
            focus: Focus::List,
            pane: Pane::for_row(ROW_ORDER[0]),
            dropdown: None,
            tools: ToolsState {
                rows: values.tools,
                selected: 0,
                approval: nearest_approval_mode(&values.approval_mode).to_string(),
                web_search: values.web_search,
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
            forms: form_states(values.form_values),
            model_discoveries: FORM_KINDS.iter().map(|_| ModelDiscovery::new()).collect(),
            tts: TtsState::new(values.tts),
            hits: RefCell::new(Vec::new()),
            hover: None,
        }
    }
}

#[cfg(test)]
mod tests;
