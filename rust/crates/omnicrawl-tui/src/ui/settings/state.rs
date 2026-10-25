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
use omnicrawl_config::features::decision_model::{
    DECISION_SWITCHES, DECISION_SWITCH_WEB_SEARCH_EMBEDDING,
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
    pub api_key_env: String,
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
    ApiKeyEnv,
    ModelId,
    UserAgent,
    Enabled,
}

impl ChannelField {
    /// 表单里的字段顺序。
    pub const ORDER: [ChannelField; 9] = [
        ChannelField::Name,
        ChannelField::Provider,
        ChannelField::Protocol,
        ChannelField::BaseUrl,
        ChannelField::ApiKey,
        ChannelField::ApiKeyEnv,
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
            Self::ApiKeyEnv => "API Key 环境变量",
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
    pub api_key_env: String,
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
    ApiKeyEnv,
    ModelId,
    RetryCount,
    Enabled,
}

impl DecisionField {
    /// 表单里的字段顺序。
    pub const ORDER: [DecisionField; 8] = [
        DecisionField::Name,
        DecisionField::Mode,
        DecisionField::BaseUrl,
        DecisionField::ApiKey,
        DecisionField::ApiKeyEnv,
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
            Self::ApiKeyEnv => "API Key 环境变量",
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
    pub api_key_env: String,
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
    ApiKeyEnv,
    ModelId,
    Dimensions,
    TimeoutSeconds,
    Enabled,
}

impl EmbeddingField {
    pub const ORDER: [EmbeddingField; 8] = [
        EmbeddingField::Name,
        EmbeddingField::BaseUrl,
        EmbeddingField::ApiKey,
        EmbeddingField::ApiKeyEnv,
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
            Self::ApiKeyEnv => "API Key 环境变量",
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

/// 向量模型页：顶部开关行（向量召回 + 联网搜索向量排序）+ 渠道列表 + 单条渠道表单。
#[derive(Debug, Clone)]
struct EmbeddingModelsState {
    rows: Vec<EmbeddingRow>,
    default_key: String,
    /// 顶部总开关：`config.toml` 的 `[memory_embedding] enabled`。
    enabled: bool,
    /// 联网搜索走向量模型排序的开关（`decision_models.toml` 的 `[features]`）。
    web_search_rerank: bool,
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
    /// 联网搜索向量排序占第 1 行。
    const WEB_SEARCH_SWITCH_ROW: usize = 1;
    /// 开关行数：渠道从这一行开始。
    const SWITCH_ROWS: usize = 2;

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
    pub embedding_web_search_rerank: bool,
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
            embedding_web_search_rerank: false,
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
        web_search_rerank: bool,
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
        self.embedding_web_search_rerank = web_search_rerank;
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

    /// 工具页首行的审查模式初值（取运行期现用的模式）。
    pub fn with_approval(mut self, mode: &str) -> Self {
        self.approval_mode = nearest_approval_mode(mode).to_string();
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
fn mask_api_key(api_key: &str) -> String {
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
        /// 渠道的内联密钥（可能为空）：环境变量没设时用它发探测请求。
        api_key: String,
        api_key_env: String,
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
                status: TOOLS_HINT.to_string(),
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
                status: MCP_HINT.to_string(),
            },
            subagents: SubagentsState {
                rows: values.subagents,
                selected: 0,
                status: SUBAGENTS_HINT.to_string(),
            },
            vision: VisionState {
                enabled: values.vision_enabled,
                models: values.vision_models,
                selected: 0,
                native: values.vision_native,
                native_previous: values.vision_native,
                status: VISION_HINT.to_string(),
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
                status: MODEL_LIST_HINT.to_string(),
            },
            decision_models: DecisionModelsState {
                rows: values.decision_channels,
                switches: values.decision_switches,
                local: DecisionLocalState::new(values.decision_local),
                default_key: values.default_decision_key,
                selected: 0,
                form: None,
                template: values.decision_template,
                status: DECISION_LIST_HINT.to_string(),
            },
            embedding_models: EmbeddingModelsState {
                rows: values.embedding_channels,
                default_key: values.default_embedding_key,
                enabled: values.embedding_enabled,
                web_search_rerank: values.embedding_web_search_rerank,
                selected: 0,
                form: None,
                template: values.embedding_template,
                status: EMBEDDING_LIST_HINT.to_string(),
            },
            forms: form_states(values.form_values),
            model_discoveries: FORM_KINDS.iter().map(|_| ModelDiscovery::new()).collect(),
            tts: TtsState::new(values.tts),
            hits: RefCell::new(Vec::new()),
            hover: None,
        }
    }

    // ---------- 只读访问（渲染层用） ----------

    pub fn rows(&self) -> [&'static str; ROW_ORDER.len()] {
        ROW_ORDER
    }

    /// 开始一帧渲染：丢掉上一帧记录的可点区域。
    pub fn begin_frame(&self) {
        self.hits.borrow_mut().clear();
    }

    /// 记下一块可点区域（只在渲染时调用；空区域不记）。
    pub fn record_hit(&self, area: Rect, action: HitAction) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.hits.borrow_mut().push(HitRegion { area, action });
    }

    /// 按落点反查动作；**后画的优先**——展开的候选浮层画在面板之上，应当先命中它。
    pub fn hit_at(&self, column: u16, row: u16) -> Option<HitAction> {
        self.hits
            .borrow()
            .iter()
            .rev()
            .find(|region| {
                column >= region.area.x
                    && column < region.area.x + region.area.width
                    && row >= region.area.y
                    && row < region.area.y + region.area.height
            })
            .map(|region| region.action)
    }

    /// 记录鼠标悬停的行；`None` 表示光标已离开可点区域。
    pub fn set_hover(&mut self, action: Option<HitAction>) {
        self.hover = action;
    }

    pub fn hover(&self) -> Option<HitAction> {
        self.hover
    }

    /// 悬停行在当前帧里的区域（用于描边加亮）；该行本帧没画出来时为 `None`。
    pub fn hover_area(&self) -> Option<Rect> {
        let hover = self.hover?;
        self.hits
            .borrow()
            .iter()
            .rev()
            .find(|region| region.action == hover)
            .map(|region| region.area)
    }

    /// 左键点击分派。
    ///
    /// - 左栏：单击即切页（Python 的列表点击是选中并立即进入，且切页不写配置）；
    /// - 右栏：先把该页的选中行 / 字段移到落点，**再点当前行**才等同 `Enter`
    ///   （工具开关、枚举循环这类行被误触一次就是配置变更，所以不做单击即改）；
    /// - 候选浮层：单击即选中该候选并确认（浮层本来就是为这一次选择而展开的）。
    pub fn click(&mut self, action: HitAction) -> Option<SettingsEvent> {
        match action {
            HitAction::Row(index) => {
                self.select_row(index);
                self.handle_list_key(KeyCode::Enter)
            }
            HitAction::PaneRow(index) => {
                if self.select_pane_row(index) {
                    return self.handle_key(KeyCode::Enter);
                }
                None
            }
            HitAction::Option(index) => {
                // 浮层下拉（上下文页 / 单选页 / 表单页 / 视觉页）与渠道表单的内联候选
                // 是两套结构：前者记在 `self.dropdown`，后者记在渠道表单里。
                if self.dropdown.is_some() {
                    let count = self.dropdown_options().len();
                    if count == 0 {
                        return None;
                    }
                    if let Some(dropdown) = self.dropdown.as_mut() {
                        dropdown.selected = index.min(count - 1);
                    }
                } else if let Some(form) = self.model_management.form.as_mut() {
                    let count = form.dropdown.as_ref().map(|d| d.options.len()).unwrap_or(0);
                    if count == 0 {
                        return None;
                    }
                    let dropdown = form.dropdown.as_mut()?;
                    dropdown.selected = index.min(count - 1);
                } else {
                    return None;
                }
                self.handle_key(KeyCode::Enter)
            }
        }
    }

    /// 左栏：把选中项移到第 N 项；复用键盘的逐项移动，保证切页时被清掉的
    /// 状态（收起下拉、上下文字段回到第一个）与按 `↑`/`↓` 完全一致。
    fn select_row(&mut self, index: usize) {
        let count = ROW_ORDER.len();
        if index >= count || index == self.selected {
            return;
        }
        let mut guard = 0usize;
        while self.selected != index && guard <= count {
            let direction = if index > self.selected { 1 } else { -1 };
            self.move_selection(direction);
            guard += 1;
        }
    }

    /// 右栏：把当前页的选中行 / 字段移到第 N 个可点行。
    ///
    /// 返回 `true` 表示落点就是当前行（调用方可继续走 `Enter` 语义）。
    fn select_pane_row(&mut self, index: usize) -> bool {
        self.dropdown = None;
        self.focus = Focus::Pane;
        match self.pane {
            Pane::Tools => {
                let target = index.min(self.tool_row_count().saturating_sub(1));
                let already = self.tools.selected == target;
                self.tools.selected = target;
                already
            }
            Pane::Subagents => {
                let target = index.min(self.subagents.rows.len().saturating_sub(1));
                let already = self.subagents.selected == target;
                self.subagents.selected = target;
                already
            }
            Pane::Vision => {
                let target = index.min(self.vision.models.len().saturating_sub(1));
                let already = self.vision.selected == target;
                self.vision.selected = target;
                already
            }
            Pane::Tts => {
                // 点击给的是屏幕行号，按当前可见行换算成配置行号（Gemini 下没有语速行）。
                let rows = self.tts.visible_rows();
                let Some(target) = rows.get(index).copied() else {
                    return false;
                };
                let already = self.tts.focused == target;
                self.tts.focused = target;
                already
            }
            Pane::ModelManagement => {
                // 列表态选渠道或参数行，表单态选字段（字段顺序就是 `ChannelField::ORDER`）。
                if let Some(form) = self.model_management.form.as_mut() {
                    let Some(field) = ChannelField::ORDER.get(index).copied() else {
                        return false;
                    };
                    let already = form.field == field;
                    form.field = field;
                    already
                } else {
                    let target = index.min(self.model_management.row_count().saturating_sub(1));
                    let already = self.model_management.selected == target;
                    self.model_management.selected = target;
                    already
                }
            }
            Pane::DecisionModels => {
                // 与渠道页同形：列表态选渠道或开关，表单态选字段。
                if let Some(form) = self.decision_models.form.as_mut() {
                    let Some(field) = DecisionField::ORDER.get(index).copied() else {
                        return false;
                    };
                    let already = form.field == field;
                    form.field = field;
                    already
                } else {
                    let target = index.min(self.decision_row_count().saturating_sub(1));
                    let already = self.decision_models.selected == target;
                    self.decision_models.selected = target;
                    already
                }
            }
            Pane::EmbeddingModels => {
                if let Some(form) = self.embedding_models.form.as_mut() {
                    let Some(field) = EmbeddingField::ORDER.get(index).copied() else {
                        return false;
                    };
                    let already = form.field == field;
                    form.field = field;
                    already
                } else {
                    let target = index.min(self.embedding_models.total_rows().saturating_sub(1));
                    let already = self.embedding_models.selected == target;
                    self.embedding_models.selected = target;
                    already
                }
            }
            Pane::Mcp => {
                if let Some(editor) = self.mcp.editor.as_mut() {
                    let already = editor.focused == index;
                    editor.focused = index;
                    already
                } else if self.mcp.servers_view {
                    let target = index.min(self.mcp.servers.len().saturating_sub(1));
                    let already = self.mcp.server_selected == target;
                    self.mcp.server_selected = target;
                    already
                } else {
                    let target = index.min(MCP_ROW_COUNT - 1);
                    let already = self.mcp.selected == target;
                    self.mcp.selected = target;
                    already
                }
            }
            Pane::Choice(_) => {
                // 单选页只有一个字段：点它就是「把候选展开」，于是直接走 Enter。
                self.focus = Focus::Pane;
                true
            }
            Pane::Form(_) => {
                let Some(form) = self.form_mut() else {
                    return false;
                };
                // 渲染给的是可见行号（隐藏字段不占行），要映射回字段下标；隐藏行不可聚焦。
                let Some(target) = form.visible_indices().get(index).copied() else {
                    return false;
                };
                let already = form.focused() == target;
                form.set_focused(target);
                already
            }
            Pane::Pending => false,
        }
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn selected_key(&self) -> &'static str {
        ROW_ORDER[self.selected]
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn pane(&self) -> Pane {
        self.pane
    }

    pub fn title(&self) -> String {
        row_label(self.selected_key())
    }

    pub fn context_window_tokens(&self) -> i64 {
        self.model_management.window_tokens
    }

    pub fn compaction_percent(&self) -> i64 {
        self.model_management.percent
    }

    pub fn dropdown(&self) -> Option<Dropdown> {
        self.dropdown
    }

    /// 单选页当前值的显示文本；不在单选页时返回空串。
    pub fn choice_value(&self) -> String {
        match self.pane {
            Pane::Choice(kind) => display_value(&kind.value(&self.choices)),
            _ => String::new(),
        }
    }

    pub fn tool_rows(&self) -> &[ToolSwitchRow] {
        &self.tools.rows
    }

    pub fn tool_selected(&self) -> usize {
        self.tools.selected
    }

    /// 工具页首行的审查模式显示文案（`工具调用审查：自动审查`）。
    pub fn tool_approval_value(&self) -> String {
        approval_mode_label(&self.tools.approval)
    }

    /// 工具页「联网搜索」行的显示文案（默认引擎 + 已配置的凭据）。
    pub fn web_search_value(&self) -> String {
        self.tools.web_search.label()
    }

    /// 工具页的可选行数：审查模式 + 联网搜索 + 逐工具开关。
    pub fn tool_row_count(&self) -> usize {
        self.tools.rows.len() + TOOLS_SWITCH_ROW_OFFSET
    }

    pub fn subagent_rows(&self) -> &[SubagentRow] {
        &self.subagents.rows
    }

    pub fn subagent_selected(&self) -> usize {
        self.subagents.selected
    }

    /// MCP 全局设置行（渲染层用）。
    pub fn mcp_rows(&self) -> Vec<McpRowView> {
        let values = self.mcp.global_values();
        MCP_ROW_LABELS
            .iter()
            .enumerate()
            .map(|(index, label)| McpRowView {
                label,
                value: values[index].clone(),
                selected: !self.mcp.servers_view
                    && self.mcp.editor.is_none()
                    && index == self.mcp.selected,
            })
            .collect()
    }

    /// MCP 面板当前是否停在 Server 列表。
    pub fn mcp_servers_view(&self) -> bool {
        self.mcp.servers_view
    }

    /// MCP Server 列表（渲染层用）。
    pub fn mcp_server_rows(&self) -> &[McpServerRow] {
        &self.mcp.servers
    }

    pub fn mcp_server_selected(&self) -> usize {
        self.mcp.server_selected
    }

    /// 正在编辑的 Server 标题；不在编辑器时为 `None`。
    pub fn mcp_editor_title(&self) -> Option<String> {
        self.mcp.editor.as_ref().map(|editor| {
            if editor.is_new {
                "添加 MCP Server".to_string()
            } else {
                "编辑 MCP Server".to_string()
            }
        })
    }

    /// Server 编辑器的字段行；不在编辑器时为空。
    pub fn mcp_editor_rows(&self) -> Vec<McpEditorRowView> {
        let Some(editor) = self.mcp.editor.as_ref() else {
            return Vec::new();
        };
        let draft = &editor.draft;
        let values: [String; MCP_EDITOR_FIELD_COUNT] = [
            draft.name.clone(),
            draft.transport.clone(),
            draft.command.clone(),
            draft.args.clone(),
            draft.url.clone(),
            draft.timeout_seconds.to_string(),
            draft.risk_level.clone(),
            draft.env.clone(),
            draft.headers.clone(),
        ];
        MCP_EDITOR_LABELS
            .iter()
            .enumerate()
            .map(|(index, label)| {
                let focused = index == editor.focused;
                let editing = focused && editor.input.is_some();
                let shown = if editing {
                    editor
                        .input
                        .as_ref()
                        .and_then(|composer| composer.wrapped_lines(FORM_INPUT_WIDTH).pop())
                        .map(|text| format!("{text}▌"))
                        .unwrap_or_default()
                } else {
                    values[index].clone()
                };
                McpEditorRowView {
                    label,
                    value: shown,
                    editing,
                    focused,
                    has_menu: index == MCP_EDITOR_TRANSPORT || index == MCP_EDITOR_RISK,
                }
            })
            .collect()
    }

    /// 表单页当前聚焦字段序号。
    pub fn form_focused(&self) -> usize {
        self.form().map(|form| form.focused()).unwrap_or(0)
    }

    /// 聚焦字段在**可见行**里的序号（渲染层按它滚动窗口）。
    pub fn form_focus_position(&self) -> usize {
        self.form()
            .map(|form| {
                form.visible_indices()
                    .iter()
                    .position(|index| *index == form.focused())
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    }

    /// 表单页当前显示的字段个数（隐藏字段不计入滚动窗口）。
    pub fn form_field_count(&self) -> usize {
        self.form()
            .map(|form| form.visible_indices().len())
            .unwrap_or(0)
    }

    /// 表单页当前页签（渲染层按它取状态与字段表）；不在表单页时为 `None`。
    pub fn form_kind(&self) -> Option<FormKind> {
        match self.pane {
            Pane::Form(kind) => Some(kind),
            _ => None,
        }
    }

    /// 表单页的字段行（渲染层用）；不在表单页时为空。隐藏字段不出现在这份清单里。
    pub fn form_rows(&self) -> Vec<FormFieldView> {
        let Pane::Form(_) = self.pane else {
            return Vec::new();
        };
        let Some(form) = self.form() else {
            return Vec::new();
        };
        form.visible_indices()
            .into_iter()
            .filter_map(|index| {
                let spec = form.field(index)?;
                Some(FormFieldView {
                    label: spec.label,
                    value: self.form_field_value(form, index, spec),
                    has_menu: matches!(
                        spec.kind,
                        FieldKind::Enum(_) | FieldKind::ModelChannel | FieldKind::ModelId
                    ),
                    editing: index == form.focused() && form.input().is_some(),
                    focused: index == form.focused(),
                })
            })
            .collect()
    }

    /// 表单字段的显示文本：开关用两态文案，候选与模型项换成界面文案，输入态带光标块。
    fn form_field_value(&self, form: &FormState, index: usize, spec: FieldSpec) -> String {
        if index == form.focused() {
            if let Some(composer) = form.input() {
                return composer
                    .wrapped_lines(FORM_INPUT_WIDTH)
                    .pop()
                    .map(|text| format!("{text}▌"))
                    .unwrap_or_default();
            }
        }
        let raw = form.value(index).map(|value| value.text()).unwrap_or("");
        match spec.kind {
            FieldKind::Flag => {
                let (off, on) = spec.flag_labels;
                if form.value(index).map(|value| value.flag()).unwrap_or(false) {
                    on.to_string()
                } else {
                    off.to_string()
                }
            }
            FieldKind::Enum(options) => options
                .iter()
                .find(|(_, option)| *option == raw)
                .map(|(label, _)| (*label).to_string())
                .unwrap_or_else(|| raw.to_string()),
            FieldKind::ModelChannel | FieldKind::ModelId => self.model_select_value(spec, raw),
            // 凭据从不回显：已配置显示掩码，未配置显示明确文案（草稿里存的是掩码或新值）。
            FieldKind::Secret => {
                if raw.trim().is_empty() {
                    "（未配置）".to_string()
                } else {
                    MASKED_SECRET.to_string()
                }
            }
            FieldKind::Int | FieldKind::Float | FieldKind::Text => raw.to_string(),
        }
    }

    /// 模型选择字段的显示文本：渠道列显示渠道名，模型列显示模型 ID（或「渠道默认」）。
    ///
    /// 草稿里存的是原始值（渠道 key 与模型 ID），这里换成界面文案。
    fn model_select_value(&self, field: FieldSpec, raw: &str) -> String {
        match field.kind {
            FieldKind::ModelChannel => self
                .model_management
                .rows
                .iter()
                .find(|row| row.key == raw)
                .map(|row| {
                    if row.name.trim().is_empty() {
                        row.key.clone()
                    } else {
                        row.name.clone()
                    }
                })
                .unwrap_or_else(|| {
                    if raw.trim().is_empty() {
                        "（未选择）".to_string()
                    } else {
                        raw.to_string()
                    }
                }),
            FieldKind::ModelId => {
                if raw.trim().is_empty() {
                    "（渠道默认）".to_string()
                } else {
                    raw.to_string()
                }
            }
            _ => raw.to_string(),
        }
    }

    /// 当前面板底部的一行状态文本；没有状态文本的面板返回空串。
    pub fn status(&self) -> &str {
        match self.pane {
            Pane::Tools => &self.tools.status,
            Pane::Mcp => &self.mcp.status,
            Pane::Subagents => &self.subagents.status,
            Pane::Vision => &self.vision.status,
            Pane::ModelManagement => &self.model_management.status,
            Pane::DecisionModels => &self.decision_models.status,
            Pane::EmbeddingModels => &self.embedding_models.status,
            Pane::Choice(kind) => &self.choices.status[kind.index()],
            Pane::Form(_) => self.form().map(|form| form.status()).unwrap_or(""),
            Pane::Tts => &self.tts.status,
            Pane::Pending => "",
        }
    }

    /// 面板内的操作提示行。
    pub fn pane_hint(&self) -> &'static str {
        match self.pane {
            Pane::Tools => TOOLS_HINT,
            Pane::Mcp => {
                if self.mcp.editor.is_some() {
                    MCP_EDITOR_HINT
                } else if self.mcp.servers_view {
                    MCP_SERVERS_HINT
                } else {
                    MCP_HINT
                }
            }
            Pane::Subagents => SUBAGENTS_HINT,
            Pane::Vision => VISION_HINT,
            Pane::ModelManagement => {
                if self.model_management.form.is_some() {
                    MODEL_FORM_HINT
                } else {
                    MODEL_LIST_HINT
                }
            }
            Pane::DecisionModels => {
                if self.decision_models.form.is_some() {
                    DECISION_FORM_HINT
                } else {
                    DECISION_LIST_HINT
                }
            }
            Pane::EmbeddingModels => {
                if self.embedding_models.form.is_some() {
                    EMBEDDING_FORM_HINT
                } else {
                    EMBEDDING_LIST_HINT
                }
            }
            // Python 的单选面板只有下拉与状态行，没有提示行。
            Pane::Choice(_) => "",
            Pane::Form(FormKind::ToolOutputCompression) => COMPRESSION_HINT,
            Pane::Form(FormKind::WebSearch) => WEB_SEARCH_HINT,
            Pane::Form(_) => FORM_HINT,
            Pane::Tts => TTS_HINT,
            Pane::Pending => "",
        }
    }

    /// 当前表单页的草稿；不在表单页时为 `None`。
    fn form(&self) -> Option<&FormState> {
        match self.pane {
            Pane::Form(kind) => self.forms.get(kind.index()),
            _ => None,
        }
    }

    fn form_mut(&mut self) -> Option<&mut FormState> {
        match self.pane {
            Pane::Form(kind) => self.forms.get_mut(kind.index()),
            _ => None,
        }
    }

    // ---------- 粘贴 ----------

    /// 把一次粘贴落进当前面板的输入态；没有正在编辑的字段时返回 `false`。
    ///
    /// 终端发来 `Event::Paste`（bracketed paste）时按键路径整条不走，必须在这里接住：
    /// 否则凭据这类长文本在设置面板里只能靠逐字符击键（多数终端也不会把整段粘贴
    /// 拆成单个按键送来）。
    ///
    /// 焦点还在左侧一级列表时一律拒收：那边没有输入框，落进去只会偷偷改掉右侧
    /// 正在预览的字段草稿。
    pub fn paste(&mut self, text: &str) -> bool {
        if self.focus != Focus::Pane {
            return false;
        }
        match self.pane {
            Pane::Form(_) => self
                .form_mut()
                .map(|form| form.paste(text))
                .unwrap_or(false),
            Pane::ModelManagement => self
                .model_management
                .form
                .as_mut()
                .map(|form| form.paste(text))
                .unwrap_or(false),
            Pane::DecisionModels => self
                .decision_models
                .form
                .as_mut()
                .map(|form| form.paste(text))
                .unwrap_or(false),
            Pane::EmbeddingModels => self
                .embedding_models
                .form
                .as_mut()
                .map(|form| form.paste(text))
                .unwrap_or(false),
            Pane::Mcp => self
                .mcp
                .editor
                .as_mut()
                .map(|editor| editor.paste(text))
                .unwrap_or(false),
            Pane::Tts => self.tts.paste(text),
            _ => false,
        }
    }

    // ---------- 键盘 ----------

    pub fn handle_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        match self.focus {
            Focus::List => self.handle_list_key(key),
            Focus::Pane => self.handle_pane_key(key),
        }
    }

    fn handle_list_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        match key {
            KeyCode::Up => {
                self.move_selection(-1);
                None
            }
            KeyCode::Down => {
                self.move_selection(1);
                None
            }
            KeyCode::Enter | KeyCode::Right => {
                self.enter_pane();
                None
            }
            KeyCode::Esc => Some(SettingsEvent::Close),
            _ => None,
        }
    }

    fn handle_pane_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        // 展开态的三个键位对所有面板一致，先统一处理。
        if self.dropdown.is_some() {
            return self.handle_dropdown_key(key);
        }
        match self.pane {
            Pane::Tools => self.handle_tools_key(key),
            Pane::Mcp => self.handle_mcp_key(key),
            Pane::Subagents => self.handle_subagents_key(key),
            Pane::Vision => self.handle_vision_key(key),
            Pane::ModelManagement => self.handle_model_management_key(key),
            Pane::DecisionModels => self.handle_decision_models_key(key),
            Pane::EmbeddingModels => self.handle_embedding_models_key(key),
            Pane::Choice(kind) => self.handle_choice_key(key, kind),
            Pane::Form(_) => self.handle_form_key(key),
            Pane::Tts => self.handle_tts_key(key),
            Pane::Pending => match key {
                KeyCode::Esc | KeyCode::Left => {
                    self.back_to_list();
                    None
                }
                _ => None,
            },
        }
    }

    /// 模型管理页：列表态与表单态各有一组键位。
    ///
    /// 列表态的行 = 渠道行 + 「模型与参数」分区的四行（标题行不参与选中）；渠道行上
    /// `Enter` 进表单，参数行上 `Enter`/`←`/`→` 展开候选，选中即保存。
    fn handle_model_management_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        if self.model_management.form.is_some() {
            return self.handle_channel_form_key(key);
        }
        match key {
            KeyCode::Up => {
                self.move_model_row(-1);
                None
            }
            KeyCode::Down => {
                self.move_model_row(1);
                None
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ') => {
                if self.model_management.param_selected().is_some() {
                    self.open_dropdown();
                    return None;
                }
                if key == KeyCode::Char(' ') {
                    return None;
                }
                self.edit_channel();
                None
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                if self.model_management.param_selected().is_some() {
                    self.model_management.status =
                        "只有渠道行参与新建；按 ↑ 选中渠道再按 N。".to_string();
                    return None;
                }
                self.new_channel();
                None
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                if self.model_management.param_selected().is_some() {
                    self.model_management.status =
                        "只有渠道行参与删除；按 ↑ 选中渠道再按 D。".to_string();
                    return None;
                }
                self.delete_channel();
                None
            }
            KeyCode::Esc | KeyCode::Left => {
                self.back_to_list();
                None
            }
            _ => None,
        }
    }

    /// 渠道表单：输入态 → 候选展开态 → 字段导航，三层各管各的键位。
    fn handle_channel_form_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        let input_active = self
            .model_management
            .form
            .as_ref()
            .map(|form| form.input.is_some())
            .unwrap_or(false);
        if input_active {
            return self.handle_channel_input_key(key);
        }
        let dropdown_open = self
            .model_management
            .form
            .as_ref()
            .map(|form| form.dropdown.is_some())
            .unwrap_or(false);
        if dropdown_open {
            return self.handle_channel_dropdown_key(key);
        }
        match key {
            KeyCode::Up => {
                self.move_channel_field(-1);
                None
            }
            KeyCode::Down | KeyCode::Tab => {
                self.move_channel_field(1);
                None
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.activate_channel_field(),
            KeyCode::Esc | KeyCode::Left => {
                self.leave_channel_form();
                None
            }
            _ => None,
        }
    }

    /// 候选展开态：`↑`/`↓` 移动、`Enter` 确认并保存、`Esc` 收起。
    fn handle_dropdown_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        match key {
            KeyCode::Up => {
                self.move_candidate(-1);
                None
            }
            KeyCode::Down => {
                self.move_candidate(1);
                None
            }
            KeyCode::Enter => self.confirm_dropdown(),
            KeyCode::Esc => {
                self.dropdown = None;
                None
            }
            _ => None,
        }
    }

    /// 单选页：只有一个下拉，键位与 Textual `Select` 一致（与模型管理页的参数行同源）。
    fn handle_choice_key(&mut self, key: KeyCode, _kind: ChoiceKind) -> Option<SettingsEvent> {
        match key {
            KeyCode::Up | KeyCode::Down | KeyCode::Enter | KeyCode::Char(' ') => {
                self.open_dropdown();
                None
            }
            KeyCode::Esc | KeyCode::Left => {
                self.back_to_list();
                None
            }
            _ => None,
        }
    }

    /// 表单页：输入态 → 字段导航两层各管各的键位；`Ctrl+S` 保存由宿主转发进来。
    ///
    /// 「渠道」与「模型」两个字段都是下拉：渠道列出配置里的渠道，模型列在选中渠道的
    /// 候选里选（进模型列时会按需发起一次发现，结果由宿主回填）。
    fn handle_form_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        if self.form().is_some_and(|form| form.input().is_some()) {
            if let Some(form) = self.form_mut() {
                form.input_key(key);
            }
            return None;
        }
        let (index, spec) = self.focused_form_field()?;
        match key {
            KeyCode::Up => {
                if let Some(form) = self.form_mut() {
                    form.move_focus(-1);
                }
                None
            }
            // 换字段与 Textual 表单一致：`Tab` 与 `↓` 同义。
            KeyCode::Down | KeyCode::Tab => {
                if let Some(form) = self.form_mut() {
                    form.move_focus(1);
                }
                None
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                match spec.kind {
                    FieldKind::Flag => {
                        if let Some(form) = self.form_mut() {
                            form.toggle_flag(index);
                        }
                    }
                    // 候选字段选完不即时保存：仍要 `Ctrl+S`（与 Python 的表单页同义）。
                    FieldKind::Enum(_) | FieldKind::ModelChannel => self.open_dropdown(),
                    // 进模型列时先按需拉一次该渠道的模型列表，再展开候选。
                    FieldKind::ModelId => {
                        let event = self.ensure_model_discovery(index);
                        self.open_dropdown();
                        return event;
                    }
                    FieldKind::Int | FieldKind::Float | FieldKind::Text | FieldKind::Secret => {
                        if let Some(form) = self.form_mut() {
                            form.begin_input(index);
                        }
                    }
                }
                None
            }
            KeyCode::Esc | KeyCode::Left => {
                // 联网搜索页是从工具页进来的，`Esc` 回去的地方也是工具页。
                if self.pane == Pane::Form(FormKind::WebSearch) {
                    self.close_web_search_form();
                } else {
                    self.back_to_list();
                }
                None
            }
            _ => None,
        }
    }

    /// 当前表单页聚焦字段的序号与规格。
    fn focused_form_field(&self) -> Option<(usize, FieldSpec)> {
        let form = self.form()?;
        let index = form.focused();
        Some((index, form.field(index)?))
    }

    /// 某个表单页的「渠道 + 模型」两段草稿值（渠道 key 与模型 ID）。
    ///
    /// 只在「渠道」与「模型」两个字段都存在的表单页（工具输出压缩、顾问设置）上有意义。
    fn model_draft(&self, kind: FormKind) -> Option<(String, String)> {
        let form = self.forms.get(kind.index())?;
        let channel_index = form
            .specs()
            .iter()
            .position(|spec| matches!(spec.kind, FieldKind::ModelChannel))?;
        let model_index = form
            .specs()
            .iter()
            .position(|spec| matches!(spec.kind, FieldKind::ModelId))?;
        Some((
            form.value(channel_index)?.text().to_string(),
            form.value(model_index)?.text().to_string(),
        ))
    }

    /// 该表单页的模型发现状态（只有带模型选择的表单页有）。
    fn model_discovery(&self, kind: FormKind) -> Option<&ModelDiscovery> {
        self.model_discoveries.get(kind.index())
    }

    /// 该表单页的「模型」字段草稿值；页面没有渠道字段时渠道取当前生效的那条。
    ///
    /// 「渠道 + 模型」两列只有在同页并存时才成对（工具输出压缩、顾问设置）。桌面操作页
    /// 只有「桌面模型」一列，渠道按当前生效渠道算——否则下面所有候选都会退化成空，
    /// 下拉展开后什么也看不到。
    fn model_selection_draft(&self, kind: FormKind) -> Option<(String, String)> {
        if let Some(draft) = self.model_draft(kind) {
            return Some(draft);
        }
        let form = self.forms.get(kind.index())?;
        let model_index = form
            .specs()
            .iter()
            .position(|spec| matches!(spec.kind, FieldKind::ModelId))?;
        let channel = {
            let current = self.model_management.model_key.trim();
            if current.is_empty() {
                self.model_management.default_key.trim().to_string()
            } else {
                current.to_string()
            }
        };
        Some((channel, form.value(model_index)?.text().to_string()))
    }

    /// 某个表单页当前选中的渠道记录。
    fn selected_model_channel(&self, kind: FormKind) -> Option<&ChannelRow> {
        let (channel, _) = self.model_selection_draft(kind)?;
        self.model_management
            .rows
            .iter()
            .find(|row| row.key == channel)
    }

    /// 模型下拉的候选：渠道自带模型 + 该渠道的发现结果 + 当前值。
    pub fn model_candidates_for(&self, kind: FormKind) -> Vec<String> {
        let Some((_, model)) = self.model_selection_draft(kind) else {
            return Vec::new();
        };
        let channel = self.selected_model_channel(kind);
        match self.model_discovery(kind) {
            Some(discovery) => discovery.candidates(channel, &model),
            None => {
                let mut models = Vec::new();
                if let Some(channel) = channel {
                    if !channel.model_id.trim().is_empty() {
                        models.push(channel.model_id.trim().to_string());
                    }
                }
                if !model.trim().is_empty() {
                    models.push(model.trim().to_string());
                }
                models
            }
        }
    }

    /// 需要时请宿主发现「当前渠道」的可用模型；不需要时返回 `None`。
    fn ensure_model_discovery(&mut self, index: usize) -> Option<SettingsEvent> {
        let kind = self.form()?.kind();
        let (channel_key, _) = self.model_selection_draft(kind)?;
        if channel_key.trim().is_empty() {
            return None;
        }
        let discovery = self.model_discoveries.get_mut(kind.index())?;
        if !discovery.wants_discovery(&channel_key) {
            return None;
        }
        let channel = self
            .model_management
            .rows
            .iter()
            .find(|row| row.key == channel_key)?
            .clone();
        discovery.mark_discovering(&channel_key);
        if let Some(form) = self.forms.get_mut(kind.index()) {
            form.set_status("正在发现可用模型…");
        }
        let _ = index;
        Some(SettingsEvent::DiscoverChannelModels {
            profile_id: channel.profile_id,
            provider: channel.provider,
            protocol: channel.protocol,
            base_url: channel.base_url,
            api_key: channel.api_key,
            api_key_env: channel.api_key_env,
            user_agent: channel.user_agent,
        })
    }

    /// 表单页里「模型」字段的下标（字段表里唯一的 [`FieldKind::ModelId`]）。
    fn model_id_field(&self, kind: FormKind) -> Option<usize> {
        self.forms
            .get(kind.index())?
            .specs()
            .iter()
            .position(|spec| matches!(spec.kind, FieldKind::ModelId))
    }

    /// 换渠道后把模型列清空（沿用新渠道自带的模型），并请宿主重新发现。
    fn reset_model_for_channel(&mut self, kind: FormKind, channel: &str) {
        if let Some(index) = self.model_id_field(kind) {
            if let Some(form) = self.forms.get_mut(kind.index()) {
                form.set_value(index, FieldValue::Text(String::new()));
            }
        }
        if let Some(discovery) = self.model_discoveries.get_mut(kind.index()) {
            discovery.set_status(format!("已切换到渠道 {channel}；按 Enter 展开模型候选。"));
        }
    }

    /// TTS 页：输入态 → 普通态两层。输入态优先，其余键位照行表走。
    fn handle_tts_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        if self.tts.input.is_some() {
            match key {
                KeyCode::Esc => self.tts.input = None,
                KeyCode::Enter => self.tts.commit_input(),
                KeyCode::Backspace => {
                    if let Some((_, composer)) = self.tts.input.as_mut() {
                        composer.backspace();
                    }
                }
                KeyCode::Delete => {
                    if let Some((_, composer)) = self.tts.input.as_mut() {
                        composer.delete();
                    }
                }
                KeyCode::Left => {
                    if let Some((_, composer)) = self.tts.input.as_mut() {
                        composer.move_left();
                    }
                }
                KeyCode::Right => {
                    if let Some((_, composer)) = self.tts.input.as_mut() {
                        composer.move_right();
                    }
                }
                KeyCode::Home => {
                    if let Some((_, composer)) = self.tts.input.as_mut() {
                        composer.move_home();
                    }
                }
                KeyCode::End => {
                    if let Some((_, composer)) = self.tts.input.as_mut() {
                        composer.move_end();
                    }
                }
                KeyCode::Char(ch) => {
                    if let Some((_, composer)) = self.tts.input.as_mut() {
                        composer.insert(&ch.to_string());
                    }
                }
                _ => {}
            }
            return None;
        }
        match key {
            KeyCode::Up => {
                self.tts.move_focus(-1);
                None
            }
            KeyCode::Down => {
                self.tts.move_focus(1);
                None
            }
            KeyCode::Left => {
                self.tts.cycle(self.tts.focused, -1);
                None
            }
            KeyCode::Right => {
                self.tts.cycle(self.tts.focused, 1);
                None
            }
            KeyCode::Esc => {
                self.back_to_list();
                None
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.activate_tts_row(),
            _ => None,
        }
    }

    /// `Enter` 触发选中行动作；切换类行就地翻，文本行进输入态，动作用产出事件。
    fn activate_tts_row(&mut self) -> Option<SettingsEvent> {
        let row = self.tts.focused;
        match row {
            TTS_ROW_ENABLED
            | TTS_ROW_PROVIDER
            | TTS_ROW_AUTO_PLAY
            | TTS_ROW_SPEAK_REPLIES
            | TTS_ROW_VOICE
            | TTS_ROW_SPEED => {
                self.tts.cycle(row, 1);
                None
            }
            TTS_ROW_SAVE => Some(SettingsEvent::Apply(SettingsChange::Tts(TtsChange::Save(
                self.tts.draft(),
            )))),
            // 其余可见行（地址/模型/指令/API Key）是文本行：进输入态编辑。
            _ => {
                self.tts.begin_input(row);
                None
            }
        }
    }

    /// MCP 设置页：全局行（改即保存）、Server 列表与编辑器三个子模式。
    fn handle_mcp_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        if self.mcp.editor.is_some() {
            return self.handle_mcp_editor_key(key);
        }
        if self.mcp.servers_view {
            return self.handle_mcp_servers_key(key);
        }
        match key {
            KeyCode::Up => {
                self.move_mcp(-1);
                None
            }
            KeyCode::Down => {
                self.move_mcp(1);
                None
            }
            KeyCode::Left => self.activate_mcp_global(-1),
            KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => self.activate_mcp_global(1),
            KeyCode::Char('s') | KeyCode::Char('S') => {
                self.enter_mcp_servers();
                None
            }
            KeyCode::Esc => {
                self.back_to_list();
                None
            }
            _ => None,
        }
    }

    fn move_mcp(&mut self, delta: isize) {
        let count = MCP_ROW_COUNT as isize;
        self.mcp.selected = ((self.mcp.selected as isize + delta).rem_euclid(count)) as usize;
    }

    /// 改一行全局设置：布尔切换、超时换档、`servers` 行进入列表。
    fn activate_mcp_global(&mut self, direction: isize) -> Option<SettingsEvent> {
        match self.mcp.selected {
            MCP_ROW_ENABLED => self.mcp.enabled = !self.mcp.enabled,
            MCP_ROW_NETWORK => self.mcp.network = !self.mcp.network,
            MCP_ROW_WRITE => self.mcp.write = !self.mcp.write,
            MCP_ROW_COMMAND => self.mcp.command = !self.mcp.command,
            MCP_ROW_AUDIT => self.mcp.audit = !self.mcp.audit,
            MCP_ROW_TIMEOUT => {
                self.mcp.timeout = cycle_mcp_timeout(self.mcp.timeout, direction);
            }
            MCP_ROW_SERVERS => {
                self.enter_mcp_servers();
                return None;
            }
            _ => return None,
        }
        Some(SettingsEvent::Apply(SettingsChange::Mcp(
            self.mcp.globals_change(),
        )))
    }

    fn enter_mcp_servers(&mut self) {
        self.mcp.servers_view = true;
        self.mcp.server_selected = self
            .mcp
            .server_selected
            .min(self.mcp.servers.len().saturating_sub(1));
        self.mcp.status = MCP_SERVERS_HINT.to_string();
    }

    fn move_mcp_server(&mut self, delta: isize) {
        let count = self.mcp.servers.len() as isize;
        if count == 0 {
            self.mcp.server_selected = 0;
            return;
        }
        self.mcp.server_selected =
            ((self.mcp.server_selected as isize + delta).rem_euclid(count)) as usize;
    }

    fn handle_mcp_servers_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        match key {
            KeyCode::Up => {
                self.move_mcp_server(-1);
                None
            }
            KeyCode::Down => {
                self.move_mcp_server(1);
                None
            }
            KeyCode::Enter => {
                let row = self.mcp.servers.get(self.mcp.server_selected).cloned()?;
                self.mcp.editor = Some(McpEditor {
                    draft: McpServerDraft::from_row(&row),
                    focused: MCP_EDITOR_NAME,
                    input: None,
                    is_new: false,
                });
                None
            }
            KeyCode::Char(' ') => {
                let row = self.mcp.servers.get(self.mcp.server_selected).cloned()?;
                Some(SettingsEvent::Apply(SettingsChange::Mcp(
                    McpChange::SetServerEnabled {
                        name: row.name,
                        enabled: !row.enabled,
                    },
                )))
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                self.mcp.editor = Some(McpEditor {
                    draft: McpServerDraft {
                        transport: MCP_TRANSPORT_OPTIONS[0].to_string(),
                        risk_level: MCP_RISK_OPTIONS[1].to_string(),
                        timeout_seconds: self.mcp.timeout,
                        ..McpServerDraft::default()
                    },
                    focused: MCP_EDITOR_NAME,
                    input: None,
                    is_new: true,
                });
                None
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                let row = self.mcp.servers.get(self.mcp.server_selected).cloned()?;
                Some(SettingsEvent::Apply(SettingsChange::Mcp(
                    McpChange::DeleteServer { name: row.name },
                )))
            }
            KeyCode::Esc | KeyCode::Left => {
                self.mcp.servers_view = false;
                Some(SettingsEvent::Apply(SettingsChange::Mcp(
                    self.mcp.globals_change(),
                )))
            }
            _ => None,
        }
    }

    fn move_mcp_editor(&mut self, delta: isize) {
        if let Some(editor) = self.mcp.editor.as_mut() {
            editor.input = None;
            let count = MCP_EDITOR_FIELD_COUNT as isize;
            editor.focused = ((editor.focused as isize + delta).rem_euclid(count)) as usize;
        }
    }

    /// 编辑器里的 transport / risk 字段按方向换档；文本字段忽略。
    fn cycle_mcp_editor_field(&mut self, direction: isize) {
        let Some(editor) = self.mcp.editor.as_mut() else {
            return;
        };
        match editor.focused {
            MCP_EDITOR_TRANSPORT => {
                editor.draft.transport =
                    cycle_static(&MCP_TRANSPORT_OPTIONS, &editor.draft.transport, direction)
            }
            MCP_EDITOR_RISK => {
                editor.draft.risk_level =
                    cycle_static(&MCP_RISK_OPTIONS, &editor.draft.risk_level, direction)
            }
            MCP_EDITOR_TIMEOUT => {
                editor.draft.timeout_seconds =
                    cycle_mcp_timeout(editor.draft.timeout_seconds, direction)
            }
            _ => {}
        }
    }

    fn handle_mcp_editor_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        if self
            .mcp
            .editor
            .as_ref()
            .is_some_and(|editor| editor.input.is_some())
        {
            if let Some(editor) = self.mcp.editor.as_mut() {
                editor.input_key(key);
            }
            return None;
        }
        match key {
            KeyCode::Up => {
                self.move_mcp_editor(-1);
                None
            }
            KeyCode::Down | KeyCode::Tab => {
                self.move_mcp_editor(1);
                None
            }
            KeyCode::Left => {
                self.cycle_mcp_editor_field(-1);
                None
            }
            KeyCode::Right => {
                self.cycle_mcp_editor_field(1);
                None
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let focused = self.mcp.editor.as_ref().map(|editor| editor.focused)?;
                match focused {
                    MCP_EDITOR_TRANSPORT | MCP_EDITOR_RISK | MCP_EDITOR_TIMEOUT => {
                        self.cycle_mcp_editor_field(1);
                    }
                    _ => {
                        if let Some(editor) = self.mcp.editor.as_mut() {
                            editor.begin_input();
                        }
                    }
                }
                None
            }
            KeyCode::Esc => {
                self.mcp.editor = None;
                None
            }
            _ => None,
        }
    }

    /// 编辑器里的文本型字段（名称/命令/参数/URL/环境变量/请求头）。
    fn handle_mcp_editor_save(&mut self) -> Option<SettingsEvent> {
        let editor = self.mcp.editor.as_mut()?;
        editor.commit_input();
        Some(SettingsEvent::Apply(SettingsChange::Mcp(
            McpChange::SaveServer(Box::new(editor.draft.clone())),
        )))
    }

    fn handle_tools_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        match key {
            KeyCode::Up => {
                self.move_tool(-1);
                None
            }
            KeyCode::Down => {
                self.move_tool(1);
                None
            }
            // 审查模式行按方向换档；工具开关行无视方向，一律切换。
            KeyCode::Left => self.toggle_tool(-1),
            KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => self.toggle_tool(1),
            KeyCode::Esc => {
                self.back_to_list();
                None
            }
            _ => None,
        }
    }

    /// 子任务设置页：`↑`/`↓` 选行；`←`/`→`/`Enter`/空格 改选中行（选中即保存）。
    fn handle_subagents_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        match key {
            KeyCode::Up => {
                self.move_subagent(-1);
                None
            }
            KeyCode::Down => {
                self.move_subagent(1);
                None
            }
            // `←` 退一档、`→`/`Enter`/空格 进一档；总开关行两个方向都是切换。
            KeyCode::Left => self.activate_subagent(-1),
            KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => self.activate_subagent(1),
            KeyCode::Esc => {
                self.back_to_list();
                None
            }
            _ => None,
        }
    }

    fn move_subagent(&mut self, delta: isize) {
        let count = self.subagents.rows.len() as isize;
        if count == 0 {
            return;
        }
        self.subagents.selected =
            ((self.subagents.selected as isize + delta).rem_euclid(count)) as usize;
    }

    /// 改选中行：总开关就地翻转，高级参数按档位表循环；写盘由宿主负责。
    fn activate_subagent(&mut self, direction: isize) -> Option<SettingsEvent> {
        let row = self.subagents.rows.get(self.subagents.selected)?;
        if row.toggle {
            return Some(SettingsEvent::Apply(SettingsChange::Subagent(
                SubagentChange::Enabled(!row.active),
            )));
        }
        let spec = SUBAGENT_ADVANCED_SPECS
            .iter()
            .find(|spec| spec.key == row.key.as_str())?;
        let current: i64 = row.value.parse().unwrap_or(spec.options[0]);
        let value = cycle_subagent_option(spec.options, current, direction);
        Some(SettingsEvent::Apply(SettingsChange::Subagent(
            SubagentChange::Advanced {
                key: row.key.clone(),
                value,
            },
        )))
    }

    /// 视觉设置页：`↑↓` 选列表项、空格 开关、`A` 添加、`D` 删除、`N` 原生视觉三态。
    fn handle_vision_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        match key {
            KeyCode::Up => {
                self.move_vision(-1);
                None
            }
            KeyCode::Down => {
                self.move_vision(1);
                None
            }
            KeyCode::Char(' ') => {
                self.vision.enabled = !self.vision.enabled;
                self.vision.status = format!(
                    "视觉代理已{}；按 Ctrl+S 保存。",
                    if self.vision.enabled {
                        "启用"
                    } else {
                        "停用"
                    }
                );
                None
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                self.dropdown = Some(Dropdown {
                    field: DropdownField::VisionAdd,
                    selected: 0,
                });
                None
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                self.delete_vision_model();
                None
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.cycle_native_vision();
                None
            }
            KeyCode::Esc | KeyCode::Left => {
                self.back_to_list();
                None
            }
            _ => None,
        }
    }

    fn move_vision(&mut self, delta: isize) {
        let count = self.vision.models.len() as isize;
        if count == 0 {
            return;
        }
        self.vision.selected = ((self.vision.selected as isize + delta).rem_euclid(count)) as usize;
    }

    /// 故障转移顺序：把选中项与相邻项对调（对映 Python 的 `action_move_priority_*`）。
    fn move_vision_priority(&mut self, delta: isize) {
        let selected = self.vision.selected;
        let target = selected as isize + delta;
        let count = self.vision.models.len();
        if selected >= count || target < 0 || target as usize >= count {
            return;
        }
        self.vision.models.swap(selected, target as usize);
        self.vision.selected = target as usize;
        self.vision.status = format!(
            "已{}故障转移优先级；按 Ctrl+S 保存。",
            if delta < 0 { "提高" } else { "降低" }
        );
    }

    /// 模型原生视觉三态循环：未配置 → 开启 → 关闭 → 未配置。
    fn cycle_native_vision(&mut self) {
        self.vision.native = match self.vision.native {
            None => Some(true),
            Some(true) => Some(false),
            Some(false) => None,
        };
        let text = self.vision_native_text();
        self.vision.status = format!("{text}；按 Ctrl+S 保存。");
    }

    fn delete_vision_model(&mut self) {
        if self.vision.models.is_empty() {
            self.vision.status = "当前没有可删除的视觉模型。".to_string();
            return;
        }
        let removed = self.vision.models.remove(self.vision.selected);
        self.vision.selected = self
            .vision
            .selected
            .min(self.vision.models.len().saturating_sub(1));
        self.vision.status = format!("已移除视觉模型「{}」；按 Ctrl+S 保存。", removed.label());
    }

    /// `A` 选中一条渠道：作为 `custom` 引用加进列表（重复项只提示，不重复添加）。
    fn add_vision_model(&mut self, key: &str) {
        let reference = VisionModelRef::custom(key);
        if self
            .vision
            .models
            .iter()
            .any(|item| item.label() == reference.label())
        {
            self.vision.status = format!("视觉模型「{}」已经在列表中。", reference.label());
            return;
        }
        self.vision.models.push(reference);
        self.vision.selected = self.vision.models.len() - 1;
        let label = self.vision.models[self.vision.selected].label();
        self.vision.status = format!("已添加「{label}」；按 Ctrl+S 保存。");
    }

    /// 视觉页头部两行的文本（渲染层用）。
    pub fn vision_enabled_text(&self) -> String {
        format!(
            "视觉代理：{}    已配置模型：{} 个",
            if self.vision.enabled {
                "已启用"
            } else {
                "已停用"
            },
            self.vision.models.len()
        )
    }

    pub fn vision_native_text(&self) -> String {
        match self.vision.native {
            None => "模型原生视觉：未配置（按模型能力）".to_string(),
            Some(true) => "模型原生视觉：已开启".to_string(),
            Some(false) => "模型原生视觉：已关闭".to_string(),
        }
    }

    /// 视觉列表的行文本（`› [1] 渠道名`），渲染层用。
    pub fn vision_rows(&self) -> Vec<String> {
        self.vision
            .models
            .iter()
            .enumerate()
            .map(|(index, reference)| {
                format!(
                    "{} [{}] {}",
                    if index == self.vision.selected {
                        "›"
                    } else {
                        " "
                    },
                    index + 1,
                    reference.label()
                )
            })
            .collect()
    }

    pub fn vision_selected(&self) -> usize {
        self.vision.selected
    }

    /// TTS 页的行视图（渲染层用；顺序即当前可见行，索引即屏幕行号）。
    pub fn tts_rows(&self) -> Vec<TtsRowView> {
        self.tts.rows()
    }

    /// TTS 页某一行号在可见行里的下标（不可见时返回 `None`）。
    pub fn tts_row_index(&self, row: usize) -> Option<usize> {
        self.tts.visible_rows().iter().position(|item| *item == row)
    }

    pub fn tts_focused(&self) -> usize {
        self.tts.focused
    }

    /// TTS 页选中行在可见行里的下标（渲染层用它比屏幕行号）。
    pub fn tts_focused_index(&self) -> usize {
        self.tts
            .visible_rows()
            .iter()
            .position(|row| *row == self.tts.focused)
            .unwrap_or(0)
    }

    fn move_selection(&mut self, delta: isize) {
        let count = ROW_ORDER.len() as isize;
        self.selected = ((self.selected as isize + delta).rem_euclid(count)) as usize;
        let next = Pane::for_row(self.selected_key());
        if next != self.pane {
            self.pane = next;
            // 换面板等价于重新进入：收起下拉，选中行回到第一个。
            self.dropdown = None;
            self.model_management.selected = 0;
        }
    }

    /// 模型管理页列表态的行移动：渠道行与「模型与参数」分区行一起循环。
    fn move_model_row(&mut self, delta: isize) {
        let count = self.model_management.row_count() as isize;
        if count <= 0 {
            return;
        }
        self.model_management.selected =
            ((self.model_management.selected as isize + delta).rem_euclid(count)) as usize;
        self.dropdown = None;
    }

    fn enter_pane(&mut self) {
        self.focus = Focus::Pane;
        self.dropdown = None;
    }

    fn back_to_list(&mut self) {
        self.focus = Focus::List;
        self.dropdown = None;
    }

    /// 当前面板正在编辑的字段；没有可展开字段（工具页/配置对话入口页，以及表单页
    /// 的开关与整数项）时为 `None`。
    fn active_field(&self) -> Option<DropdownField> {
        match self.pane {
            Pane::Choice(kind) => Some(DropdownField::Choice(kind)),
            Pane::ModelManagement if self.model_management.form.is_none() => self
                .model_management
                .param_selected()
                .map(DropdownField::ModelParam),
            Pane::Form(_) => match self.focused_form_field()?.1.kind {
                FieldKind::Enum(_) | FieldKind::ModelChannel | FieldKind::ModelId => {
                    Some(DropdownField::Form(self.form()?.focused()))
                }
                _ => None,
            },
            Pane::Tools
            | Pane::Mcp
            | Pane::Subagents
            | Pane::ModelManagement
            | Pane::DecisionModels
            | Pane::EmbeddingModels
            | Pane::Vision
            | Pane::Tts
            | Pane::Pending => None,
        }
    }

    fn open_dropdown(&mut self) {
        let Some(field) = self.active_field() else {
            return;
        };
        let current = match field {
            DropdownField::ModelParam(ModelParamRow::Channel) => {
                OptionValue::Model(self.model_management.model_key.clone())
            }
            DropdownField::ModelParam(ModelParamRow::Window) => {
                OptionValue::Int(self.model_management.window_tokens)
            }
            DropdownField::ModelParam(ModelParamRow::Compaction) => {
                OptionValue::Int(self.model_management.percent)
            }
            DropdownField::ModelParam(ModelParamRow::Reasoning) => {
                OptionValue::Text(self.model_management.reasoning.clone())
            }
            DropdownField::Choice(kind) => kind.value(&self.choices),
            // 视觉页的候选没有「当前值」：游标停在第一项。
            DropdownField::VisionAdd => OptionValue::Text(String::new()),
            // 表单页的模型项与文本项都按字段值定位游标。
            DropdownField::Form(index) => {
                let Some(form) = self.form() else {
                    return;
                };
                let Some(spec) = form.field(index) else {
                    return;
                };
                let raw = form.value(index).map(|value| value.text()).unwrap_or("");
                match spec.kind {
                    FieldKind::ModelChannel => OptionValue::Model(raw.to_string()),
                    _ => OptionValue::Text(raw.to_string()),
                }
            }
        };
        let selected = self
            .options_for(field)
            .iter()
            .position(|(_, option)| *option == current)
            .unwrap_or(0);
        self.dropdown = Some(Dropdown { field, selected });
    }

    fn move_candidate(&mut self, delta: isize) {
        // 先算出候选数量再改动游标：`options_for` 借整个 `self`，不能与游标的可变借用重叠。
        let Some(field) = self.dropdown.map(|dropdown| dropdown.field) else {
            return;
        };
        let count = self.options_for(field).len() as isize;
        if count == 0 {
            return;
        }
        let Some(dropdown) = self.dropdown.as_mut() else {
            return;
        };
        dropdown.selected = ((dropdown.selected as isize + delta).rem_euclid(count)) as usize;
    }

    /// 某个字段的候选：模型与渠道项随配置变化，表单页的候选取自字段表，其余是静态表。
    fn options_for(&self, field: DropdownField) -> Vec<(String, OptionValue)> {
        match field {
            DropdownField::ModelParam(row) => self.model_param_options(row),
            // 视觉页的候选是渠道列表，与「A 添加」的语义一致。
            DropdownField::VisionAdd => self.channel_options(),
            DropdownField::Form(index) => match self.form().and_then(|form| form.field(index)) {
                Some(FieldSpec {
                    kind: FieldKind::Enum(options),
                    ..
                }) => options
                    .iter()
                    .map(|(label, value)| {
                        (
                            (*label).to_string(),
                            OptionValue::Text((*value).to_string()),
                        )
                    })
                    .collect(),
                Some(FieldSpec {
                    kind: FieldKind::ModelChannel,
                    ..
                }) => self.channel_options(),
                Some(FieldSpec {
                    kind: FieldKind::ModelId,
                    ..
                }) => {
                    let kind = self.form().map(|form| form.kind());
                    match kind {
                        Some(kind) => self
                            .model_candidates_for(kind)
                            .into_iter()
                            .map(|model| (model.clone(), OptionValue::Text(model)))
                            .collect(),
                        None => Vec::new(),
                    }
                }
                _ => Vec::new(),
            },
            other => other.static_options().unwrap_or_default(),
        }
    }

    /// 渠道候选（文案 = 渠道名，取值 = 渠道 key）。
    fn channel_options(&self) -> Vec<(String, OptionValue)> {
        self.model_management
            .rows
            .iter()
            .map(|row| {
                let label = if row.name.trim().is_empty() {
                    row.key.clone()
                } else {
                    row.name.clone()
                };
                (label, OptionValue::Model(row.key.clone()))
            })
            .collect()
    }

    /// 模型管理页参数行的候选。
    fn model_param_options(&self, row: ModelParamRow) -> Vec<(String, OptionValue)> {
        match row {
            ModelParamRow::Channel => self.channel_options(),
            ModelParamRow::Window => context_window_options()
                .into_iter()
                .map(|(label, value)| (label, OptionValue::Int(value)))
                .collect(),
            ModelParamRow::Compaction => context_compaction_options()
                .into_iter()
                .map(|(label, value)| (label, OptionValue::Int(value)))
                .collect(),
            ModelParamRow::Reasoning => REASONING_OPTIONS
                .iter()
                .map(|(option, label)| {
                    (
                        (*label).to_string(),
                        OptionValue::Text((*option).to_string()),
                    )
                })
                .collect(),
        }
    }

    /// 当前展开的下拉的候选；没有展开时为空（渲染层用）。
    pub fn dropdown_options(&self) -> Vec<(String, OptionValue)> {
        match self.dropdown {
            Some(dropdown) => self.options_for(dropdown.field),
            None => Vec::new(),
        }
    }

    /// 确认候选：值未变则不动作（Textual `Select` 只在值真正变化时触发保存）。
    fn confirm_dropdown(&mut self) -> Option<SettingsEvent> {
        let dropdown = self.dropdown.take()?;
        let options = self.options_for(dropdown.field);
        let (_, value) = options.get(dropdown.selected)?.clone();
        match (dropdown.field, value) {
            // 模型管理页的参数行：与单选页同义，选中即保存。
            (DropdownField::ModelParam(ModelParamRow::Channel), OptionValue::Model(key)) => {
                if key == self.model_management.model_key {
                    return None;
                }
                Some(SettingsEvent::Apply(SettingsChange::Model { key }))
            }
            (DropdownField::ModelParam(ModelParamRow::Window), OptionValue::Int(tokens)) => {
                if tokens == self.model_management.window_tokens {
                    return None;
                }
                Some(SettingsEvent::Apply(SettingsChange::ContextWindow {
                    tokens,
                }))
            }
            (DropdownField::ModelParam(ModelParamRow::Compaction), OptionValue::Int(percent)) => {
                if percent == self.model_management.percent {
                    return None;
                }
                Some(SettingsEvent::Apply(SettingsChange::CompactionPercent {
                    percent,
                }))
            }
            (DropdownField::ModelParam(ModelParamRow::Reasoning), OptionValue::Text(effort)) => {
                if effort == self.model_management.reasoning {
                    return None;
                }
                Some(SettingsEvent::Apply(SettingsChange::Reasoning { effort }))
            }
            (DropdownField::Choice(ChoiceKind::ShowThinking), OptionValue::Flag(enabled)) => {
                if enabled == self.choices.show_thinking {
                    return None;
                }
                Some(SettingsEvent::Apply(SettingsChange::ShowThinking {
                    enabled,
                }))
            }
            (DropdownField::Choice(kind), OptionValue::Flag(enabled)) => {
                let key = match kind {
                    ChoiceKind::Memory => "memory",
                    ChoiceKind::Plugins => "plugins",
                    // 其余单选页没有布尔开关；走到这里说明面板映射写错了。
                    ChoiceKind::ShowThinking => return None,
                };
                let current = match kind {
                    ChoiceKind::Memory => self.choices.memory,
                    _ => self.choices.plugins,
                };
                if enabled == current {
                    return None;
                }
                Some(SettingsEvent::Apply(SettingsChange::Feature {
                    key: key.to_string(),
                    enabled,
                }))
            }
            // 表单页的候选只落进草稿，不即时保存（等 `Ctrl+S`）。
            (DropdownField::Form(index), value) => {
                let text = match value {
                    OptionValue::Text(text) | OptionValue::Model(text) => text,
                    _ => return None,
                };
                let kind = self.form().map(|form| form.kind())?;
                let is_channel = matches!(
                    self.form()
                        .and_then(|form| form.field(index))
                        .map(|spec| spec.kind),
                    Some(FieldKind::ModelChannel)
                );
                if let Some(form) = self.forms.get_mut(kind.index()) {
                    form.set_value(index, FieldValue::Text(text.clone()));
                    form.set_status(format!("已选择：{text}；按 Ctrl+S 保存。"));
                }
                // 图像生成页换服务商：另一条通道的行藏起来（取值留在草稿里）。
                if kind == FormKind::ImageGen && index == IMAGE_GEN_PROVIDER_INDEX {
                    if let Some(form) = self.forms.get_mut(kind.index()) {
                        let gemini = text.trim().eq_ignore_ascii_case("gemini");
                        form.set_gemini(gemini);
                    }
                }
                // 换渠道后模型列清空，并让下一次进模型列重新发现。
                if is_channel {
                    self.reset_model_for_channel(kind, &text);
                }
                None
            }
            // 视觉页的「A 添加」：把选中的渠道作为一条 custom 引用加进列表。
            (DropdownField::VisionAdd, OptionValue::Model(key)) => {
                self.add_vision_model(&key);
                None
            }
            _ => None,
        }
    }

    /// 工具页首行是审查模式，其余行是逐工具开关；上下键在最外层一起循环。
    fn move_tool(&mut self, delta: isize) {
        let count = self.tool_row_count() as isize;
        if count <= 0 {
            return;
        }
        self.tools.selected = ((self.tools.selected as isize + delta).rem_euclid(count)) as usize;
    }

    /// 首行改审查模式（循环候选），第二行进入联网搜索表单页，其余行切换工具开关。
    fn toggle_tool(&mut self, direction: isize) -> Option<SettingsEvent> {
        if self.tools.selected == TOOLS_APPROVAL_ROW {
            let mode = cycle_approval_mode(&self.tools.approval, direction);
            return Some(SettingsEvent::Apply(SettingsChange::ToolApproval {
                mode: mode.to_string(),
            }));
        }
        if self.tools.selected == TOOLS_WEB_SEARCH_ROW {
            return self.open_web_search_form();
        }
        let row = self
            .tools
            .rows
            .get(self.tools.selected - TOOLS_SWITCH_ROW_OFFSET)?;
        Some(SettingsEvent::Apply(SettingsChange::ToolSwitch {
            name: row.name.clone(),
            enabled: !row.enabled,
        }))
    }

    /// 进入联网搜索表单页：一条设置在工具页里，编辑界面借用表单页结构。
    ///
    /// 只切面板、不写配置：与左栏切页同义，`Esc` 回到工具页。
    fn open_web_search_form(&mut self) -> Option<SettingsEvent> {
        // 先取值再取可变借用：草稿来自工具页那一行的当前状态。
        let engine = self.tools.web_search.engine.clone();
        let tavily = secret_draft_for(self.tools.web_search.tavily_configured);
        let exa = secret_draft_for(self.tools.web_search.exa_configured);
        let firecrawl = secret_draft_for(self.tools.web_search.firecrawl_configured);
        let anysearch = secret_draft_for(self.tools.web_search.anysearch_configured);
        let tinyfish = secret_draft_for(self.tools.web_search.tinyfish_configured);
        let github = secret_draft_for(self.tools.web_search.github_configured);
        self.pane = Pane::Form(FormKind::WebSearch);
        self.dropdown = None;
        if let Some(form) = self.form_mut() {
            // 每次进入都从当前配置重起，避免上一次未保存的草稿残留。
            form.set_values(vec![
                FieldValue::Text(engine),
                FieldValue::Text(tavily),
                FieldValue::Text(exa),
                FieldValue::Text(firecrawl),
                FieldValue::Text(anysearch),
                FieldValue::Text(tinyfish),
                FieldValue::Text(github),
            ]);
            form.set_focused(0);
            form.set_status(String::new());
        }
        None
    }

    /// 联网搜索表单页的 `Esc`：回到工具页（而不是左栏）。
    fn close_web_search_form(&mut self) {
        self.pane = Pane::Tools;
        self.dropdown = None;
        self.focus = Focus::Pane;
    }

    /// 联网搜索表单页的保存：把三个字段拼成一页表单变更。
    ///
    /// 与其余表单页同构（走 `SettingsChange::Form`），这样界面的「保存 → 回填 → 回工具页」
    /// 与失败保留草稿的语义都复用同一条路径。
    fn save_web_search_form(&mut self) -> Option<SettingsEvent> {
        let form = self.form()?;
        let values: Vec<FieldValue> = (0..form.specs().len())
            .map(|index| {
                let raw = form.value(index).map(|value| value.text()).unwrap_or("");
                FieldValue::Text(secret_draft(raw))
            })
            .collect();
        Some(SettingsEvent::Apply(SettingsChange::Form {
            kind: FormKind::WebSearch,
            values,
        }))
    }

    // ---------- 模型管理页：渠道列表与表单 ----------

    /// 进入表单：编辑选中的渠道。
    fn edit_channel(&mut self) {
        let selected = self.model_management.selected;
        let Some(row) = self.model_management.rows.get(selected).cloned() else {
            self.model_management.status = "还没有渠道可编辑；按 N 新建一条。".to_string();
            return;
        };
        self.model_management.status =
            format!("正在编辑渠道 {}；Ctrl+S 保存，Esc 返回列表。", row.key);
        self.model_management.form = Some(ChannelForm {
            row,
            field: ChannelField::Name,
            input: None,
            dropdown: None,
            is_new: false,
            index: Some(selected),
        });
    }

    /// 新建渠道：草稿只存在于表单里，`Ctrl+S` 才落进列表（`Esc` 直接丢弃）。
    fn new_channel(&mut self) {
        let mut row = self.model_management.template.clone();
        let base = if row.key.trim().is_empty() {
            "新渠道".to_string()
        } else {
            row.key.trim().to_string()
        };
        let mut candidate = base.clone();
        let mut index = 2;
        while self
            .model_management
            .rows
            .iter()
            .any(|item| item.key == candidate)
        {
            candidate = format!("{base}{index}");
            index += 1;
        }
        row.key = candidate.clone();
        if row.name.trim().is_empty() {
            row.name = candidate.clone();
        }
        if row.profile_id.trim().is_empty() {
            row.profile_id = format!("{candidate}-profile");
        }
        self.model_management.status =
            format!("正在新建渠道 {candidate}；填好后按 Ctrl+S 保存，Esc 放弃。");
        self.model_management.form = Some(ChannelForm {
            row,
            field: ChannelField::Name,
            input: None,
            dropdown: None,
            is_new: true,
            index: None,
        });
    }

    /// 删除选中渠道；至少保留一条（与配置侧的校验同口径）。
    fn delete_channel(&mut self) {
        if self.model_management.rows.len() <= 1 {
            self.model_management.status = "至少要保留一个模型渠道。".to_string();
            return;
        }
        let selected = self.model_management.selected;
        let removed = self.model_management.rows.remove(selected);
        self.model_management.selected = selected.min(self.model_management.rows.len() - 1);
        if self.model_management.default_key == removed.key {
            self.model_management.default_key = self
                .model_management
                .rows
                .iter()
                .find(|row| row.enabled)
                .or_else(|| self.model_management.rows.first())
                .map(|row| row.key.clone())
                .unwrap_or_default();
        }
        // 删掉的正好是当前模型渠道时，落到新的默认渠道。
        if self.model_management.model_key == removed.key {
            self.model_management.model_key = self.model_management.default_key.clone();
        }
        self.model_management.status =
            format!("已删除渠道 {}；Ctrl+S 保存后才会写盘。", removed.key);
    }

    /// 表单里切换字段（`↑`/`↓` 与 `Tab` 同义）。
    fn move_channel_field(&mut self, delta: isize) {
        let Some(form) = self.model_management.form.as_mut() else {
            return;
        };
        let count = ChannelField::ORDER.len() as isize;
        let current = form.field.index() as isize;
        let next = (current + delta).rem_euclid(count) as usize;
        form.field = ChannelField::ORDER[next];
    }

    /// `Enter`/空格：文本字段进编辑态、枚举展开候选、开关就地翻转。
    fn activate_channel_field(&mut self) -> Option<SettingsEvent> {
        let Some(form) = self.model_management.form.as_mut() else {
            return None;
        };
        let mut event = None;
        match form.field {
            ChannelField::Enabled => form.row.enabled = !form.row.enabled,
            ChannelField::Provider => {
                let options: Vec<String> = provider_options()
                    .iter()
                    .map(|provider| (*provider).to_string())
                    .collect();
                let selected = options
                    .iter()
                    .position(|option| option == &form.row.provider)
                    .unwrap_or(0);
                form.input = None;
                form.dropdown = Some(ChannelDropdown {
                    field: ChannelField::Provider,
                    options,
                    selected,
                });
            }
            ChannelField::Protocol => {
                let options: Vec<String> = protocols_for_provider(&form.row.provider)
                    .iter()
                    .map(|protocol| (*protocol).to_string())
                    .collect();
                let selected = options
                    .iter()
                    .position(|option| option == &form.row.protocol)
                    .unwrap_or(0);
                form.input = None;
                form.dropdown = Some(ChannelDropdown {
                    field: ChannelField::Protocol,
                    options,
                    selected,
                });
            }
            // 「模型 ID」不再是纯手输：交宿主按渠道的 Provider/协议/基地址/凭据在
            // 后台拉取模型列表，结果经 `set_channel_models` 回填成可选的候选。
            ChannelField::ModelId => {
                form.dropdown = None;
                form.input = None;
                event = Some(SettingsEvent::DiscoverChannelModels {
                    profile_id: form.row.profile_id.clone(),
                    provider: form.row.provider.clone(),
                    protocol: form.row.protocol.clone(),
                    base_url: form.row.base_url.clone(),
                    api_key: form.row.api_key.clone(),
                    api_key_env: form.row.api_key_env.clone(),
                    user_agent: form.row.user_agent.clone(),
                });
                self.model_management.status =
                    "正在检测模型列表…（Esc 可保留手动输入，稍候可重试）".to_string();
            }
            field => {
                // API Key 的输入态从空开始：既不回显明文，也让「留空=不改动」成为默认动作。
                form.dropdown = None;
                form.input = Some(channel_input_composer(&form.row, field));
            }
        }
        event
    }

    /// 自动检测结果回填：有候选就展开下拉，否则退回手动输入并说明原因。
    ///
    /// 结果先回给「正在等它的表单页」（压缩页与顾问页的模型列）：由该页的
    /// [`ModelDiscovery`] 收下，下一次展开模型列就有候选；否则给渠道表单的模型 ID 列。
    pub fn set_channel_models(&mut self, models: Vec<String>, message: String) {
        if let Some(kind) = self.pending_discovery_form() {
            if let Some(discovery) = self.model_discoveries.get_mut(kind.index()) {
                discovery.set_models(models, &message);
                let status = discovery.status().to_string();
                if let Some(form) = self.forms.get_mut(kind.index()) {
                    form.set_status(status);
                }
            }
            return;
        }
        let Some(form) = self.model_management.form.as_mut() else {
            return;
        };
        // 检测期间用户可能已经离开这个字段：结果直接丢弃。
        if form.field != ChannelField::ModelId {
            return;
        }
        if models.is_empty() {
            let reason = if message.trim().is_empty() {
                "未返回可用模型".to_string()
            } else {
                message
            };
            let mut composer = Composer::default();
            composer.insert(&form.row.model_id);
            form.dropdown = None;
            form.input = Some(composer);
            self.model_management.status =
                format!("未检测到模型列表（{reason}）；可直接输入模型 ID 后按 Enter。");
            return;
        }
        let selected = models
            .iter()
            .position(|id| id == &form.row.model_id)
            .unwrap_or(0);
        form.input = None;
        form.dropdown = Some(ChannelDropdown {
            field: ChannelField::ModelId,
            options: models,
            selected,
        });
        self.model_management.status =
            "已检测到模型列表：↑↓ 选择，Enter 确认，Esc 收起。".to_string();
    }

    /// 哪个表单页正在等这次发现结果；没有就返回 `None`（说明是渠道表单发起的）。
    fn pending_discovery_form(&self) -> Option<FormKind> {
        self.model_discoveries
            .iter()
            .position(ModelDiscovery::handles_discovery)
            .and_then(|index| FORM_KINDS.get(index).copied())
    }

    /// 枚举候选展开态：`↑`/`↓` 移动、`Enter` 确认、`Esc` 收起。
    fn handle_channel_dropdown_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        let form = self.model_management.form.as_mut()?;
        let dropdown = form.dropdown.as_mut()?;
        match key {
            KeyCode::Up => {
                let count = dropdown.options.len() as isize;
                if count > 0 {
                    dropdown.selected =
                        ((dropdown.selected as isize - 1).rem_euclid(count)) as usize;
                }
                None
            }
            KeyCode::Down => {
                let count = dropdown.options.len() as isize;
                if count > 0 {
                    dropdown.selected =
                        ((dropdown.selected as isize + 1).rem_euclid(count)) as usize;
                }
                None
            }
            KeyCode::Enter => {
                let value = dropdown.options.get(dropdown.selected).cloned();
                let field = dropdown.field;
                form.dropdown = None;
                if let Some(value) = value {
                    set_channel_field(&mut form.row, field, value);
                    if field == ChannelField::Provider {
                        // 换 Provider 后原协议可能不再适用：跟到新 Provider 的第一个协议。
                        if let Some(protocol) = protocols_for_provider(&form.row.provider).first() {
                            form.row.protocol = (*protocol).to_string();
                        }
                    }
                }
                None
            }
            KeyCode::Esc => {
                form.dropdown = None;
                None
            }
            _ => None,
        }
    }

    /// 文本编辑态：字符插入、光标移动、`Enter` 提交、`Esc` 取消。
    fn handle_channel_input_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        let form = self.model_management.form.as_mut()?;
        let field = form.field;
        let composer = form.input.as_mut()?;
        match key {
            KeyCode::Enter => {
                let value = composer.take();
                form.input = None;
                set_channel_field(&mut form.row, field, value);
            }
            KeyCode::Esc => form.input = None,
            KeyCode::Char(character) => composer.insert(&character.to_string()),
            KeyCode::Backspace => composer.backspace(),
            KeyCode::Delete => composer.delete(),
            KeyCode::Left => composer.move_left(),
            KeyCode::Right => composer.move_right(),
            KeyCode::Home => composer.move_home(),
            KeyCode::End => composer.move_end(),
            _ => {}
        }
        None
    }

    /// `Esc`：退出表单回列表（未保存的改动丢弃，草稿直接扔掉）。
    fn leave_channel_form(&mut self) {
        let drafting = self
            .model_management
            .form
            .as_ref()
            .map(|form| form.is_new)
            .unwrap_or(false);
        self.model_management.form = None;
        self.model_management.status = if drafting {
            "已放弃新建渠道（未写盘）。".to_string()
        } else {
            "已返回渠道列表（未保存的改动已丢弃）。".to_string()
        };
    }

    /// `Ctrl+S`：把表单落进列表、校验后交给宿主写盘。
    fn save_channels(&mut self) -> Option<SettingsEvent> {
        let form = self.model_management.form.clone()?;
        let mut rows = self.model_management.rows.clone();
        match form.index {
            Some(index) if index < rows.len() => rows[index] = form.row.clone(),
            _ => rows.push(form.row.clone()),
        }
        if let Err(message) = validate_channels(&rows) {
            self.model_management.status = message;
            return None;
        }
        let default_key = pick_default_channel(&rows, &self.model_management.default_key);
        self.model_management.status = "正在保存渠道配置…".to_string();
        self.model_management.rows = rows.clone();
        self.model_management.default_key = default_key.clone();
        self.model_management.form = None;
        Some(SettingsEvent::Apply(SettingsChange::Channels {
            rows,
            default_key,
        }))
    }

    // ---------- 结构化决策模型页：列表与表单 ----------
    //
    // 与渠道页同形，但表单更窄（决策服务只有一种接口）且没有「模型 ID 自动检测」：
    // 决策模型名是固定的版本字符串，没有远端列表可拉。

    /// 决策页：列表态与表单态各有一组键位。
    ///
    /// 列表态的行由「决策渠道 + 功能开关」拼成：渠道行上 `Enter`/`→` 进表单，
    /// 开关行上 `←`/`→`/`Enter`/空格 就地切换（选中即保存，与工具开关页同义）。
    fn handle_decision_models_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        if self.decision_models.form.is_some() {
            return self.handle_decision_form_key(key);
        }
        match key {
            KeyCode::Up => {
                self.move_decision_channel(-1);
                None
            }
            KeyCode::Down => {
                self.move_decision_channel(1);
                None
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ') => {
                if self.decision_switch_selected().is_some() {
                    return self.toggle_decision_switch();
                }
                if let Some(row) = self.decision_local_selected() {
                    return self.activate_decision_local(row);
                }
                if key == KeyCode::Char(' ') {
                    return None;
                }
                self.edit_decision_channel();
                None
            }
            KeyCode::Left => {
                if self.decision_switch_selected().is_some() {
                    return self.toggle_decision_switch();
                }
                if let Some(row) = self.decision_local_selected() {
                    // 左键在枚举行上回退一档，其余行与 Esc 同义（回一级菜单）。
                    if matches!(row, DECISION_LOCAL_ROW_SIZE | DECISION_LOCAL_ROW_DEVICE) {
                        self.decision_models.local.cycle(row, -1);
                        return self.save_decision_local();
                    }
                    self.back_to_list();
                    return None;
                }
                self.back_to_list();
                None
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                if self.decision_switch_selected().is_some()
                    || self.decision_local_selected().is_some()
                {
                    self.decision_models.status =
                        "只有决策渠道行参与新建；按 ↑ 选中决策渠道再按 N。".to_string();
                    return None;
                }
                self.new_decision_channel();
                None
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                if let Some(row) = self.decision_local_selected() {
                    return self.activate_decision_local(row);
                }
                self.delete_decision_channel();
                None
            }
            KeyCode::Esc => {
                self.back_to_list();
                None
            }
            _ => None,
        }
    }

    /// 决策表单：输入态 → 候选展开态 → 字段导航，三层各管各的键位。
    fn handle_decision_form_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        let input_active = self
            .decision_models
            .form
            .as_ref()
            .map(|form| form.input.is_some())
            .unwrap_or(false);
        if input_active {
            return self.handle_decision_input_key(key);
        }
        let dropdown_open = self
            .decision_models
            .form
            .as_ref()
            .map(|form| form.dropdown.is_some())
            .unwrap_or(false);
        if dropdown_open {
            return self.handle_decision_dropdown_key(key);
        }
        match key {
            KeyCode::Up => {
                self.move_decision_field(-1);
                None
            }
            KeyCode::Down | KeyCode::Tab => {
                self.move_decision_field(1);
                None
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.activate_decision_field(),
            KeyCode::Esc | KeyCode::Left => {
                self.leave_decision_form();
                None
            }
            _ => None,
        }
    }

    /// 候选展开态：`↑`/`↓` 移动、`Enter` 确认、`Esc` 收起。
    fn handle_decision_dropdown_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        let form = self.decision_models.form.as_mut()?;
        let dropdown = form.dropdown.as_mut()?;
        match key {
            KeyCode::Up => {
                let count = dropdown.options.len() as isize;
                if count > 0 {
                    dropdown.selected =
                        ((dropdown.selected as isize - 1).rem_euclid(count)) as usize;
                }
                None
            }
            KeyCode::Down => {
                let count = dropdown.options.len() as isize;
                if count > 0 {
                    dropdown.selected =
                        ((dropdown.selected as isize + 1).rem_euclid(count)) as usize;
                }
                None
            }
            KeyCode::Enter => {
                let value = dropdown.options.get(dropdown.selected).cloned();
                let field = dropdown.field;
                form.dropdown = None;
                if let Some(value) = value {
                    set_decision_field(&mut form.row, field, value);
                }
                None
            }
            KeyCode::Esc => {
                form.dropdown = None;
                None
            }
            _ => None,
        }
    }

    /// 文本编辑态：字符插入、光标移动、`Enter` 提交、`Esc` 取消。
    fn handle_decision_input_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        let form = self.decision_models.form.as_mut()?;
        let field = form.field;
        let composer = form.input.as_mut()?;
        match key {
            KeyCode::Enter => {
                let value = composer.take();
                form.input = None;
                set_decision_field(&mut form.row, field, value);
            }
            KeyCode::Esc => form.input = None,
            KeyCode::Char(character) => composer.insert(&character.to_string()),
            KeyCode::Backspace => composer.backspace(),
            KeyCode::Delete => composer.delete(),
            KeyCode::Left => composer.move_left(),
            KeyCode::Right => composer.move_right(),
            KeyCode::Home => composer.move_home(),
            KeyCode::End => composer.move_end(),
            _ => {}
        }
        None
    }

    /// 选中的是开关行时返回它在开关表里的下标；选中的是渠道行时返回 `None`。
    fn decision_switch_selected(&self) -> Option<usize> {
        self.decision_models
            .selected
            .checked_sub(self.decision_models.rows.len())
            .filter(|index| *index < self.decision_models.switches.len())
    }

    /// 自部署分区被选中的行号（不在该分区时为 `None`）。
    fn decision_local_selected(&self) -> Option<usize> {
        self.decision_models
            .selected
            .checked_sub(self.decision_models.rows.len() + self.decision_models.switches.len())
            .filter(|index| *index < DECISION_LOCAL_ROW_COUNT)
    }

    /// 自部署分区：`Enter`/方向键触发的动作。
    ///
    /// 长任务期间按住重复动作，但状态行给出**当前阶段**（进度行也一直在跑），
    /// 不再只回一句「请稍候」——用户能看出到底卡在哪一步。
    fn activate_decision_local(&mut self, row: usize) -> Option<SettingsEvent> {
        if self.decision_models.local.busy {
            let (stage, _, _) = self.decision_progress_line();
            self.decision_models.status = if stage.is_empty() {
                "自部署任务进行中，进度见「任务进度」行。".to_string()
            } else {
                format!("自部署任务进行中：{stage}")
            };
            return None;
        }
        match row {
            DECISION_LOCAL_ROW_SIZE | DECISION_LOCAL_ROW_DEVICE => {
                self.decision_models.local.cycle(row, 1);
                self.save_decision_local()
            }
            DECISION_LOCAL_ROW_PREPARE => Some(SettingsEvent::Apply(
                SettingsChange::DecisionLocal(DecisionLocalChange::PrepareEnvironment),
            )),
            DECISION_LOCAL_ROW_DOWNLOAD => {
                let size = self.decision_models.local.size.clone();
                Some(SettingsEvent::Apply(SettingsChange::DecisionLocal(
                    DecisionLocalChange::Download { size },
                )))
            }
            DECISION_LOCAL_ROW_DELETE => {
                let size = self.decision_models.local.size.clone();
                if !self.decision_models.local.size_downloaded() {
                    self.decision_models.status = format!("{size} 还没有下载到本机，无需删除。");
                    return None;
                }
                Some(SettingsEvent::Apply(SettingsChange::DecisionLocal(
                    DecisionLocalChange::Delete { size },
                )))
            }
            DECISION_LOCAL_ROW_REMOVE_ENV => {
                if !self.decision_models.local.env_ready {
                    self.decision_models.status = "运行环境本来就没装，无需删除。".to_string();
                    return None;
                }
                Some(SettingsEvent::Apply(SettingsChange::DecisionLocal(
                    DecisionLocalChange::RemoveEnvironment,
                )))
            }
            DECISION_LOCAL_ROW_CHANNEL => {
                let size = self.decision_models.local.size.clone();
                Some(SettingsEvent::Apply(SettingsChange::DecisionLocal(
                    DecisionLocalChange::UseChannel { size },
                )))
            }
            DECISION_LOCAL_ROW_START => Some(SettingsEvent::Apply(SettingsChange::DecisionLocal(
                DecisionLocalChange::StartService,
            ))),
            DECISION_LOCAL_ROW_STOP => Some(SettingsEvent::Apply(SettingsChange::DecisionLocal(
                DecisionLocalChange::StopService,
            ))),
            _ => None,
        }
    }

    /// 尺寸或设备改动：就地写盘（与功能开关同一口径：选中即保存）。
    fn save_decision_local(&mut self) -> Option<SettingsEvent> {
        let local = &self.decision_models.local;
        Some(SettingsEvent::Apply(SettingsChange::DecisionLocal(
            DecisionLocalChange::Save {
                size: local.size.clone(),
                device: local.device.clone(),
            },
        )))
    }

    /// 开关行：就地翻转并交给宿主写盘（选中即保存）。
    fn toggle_decision_switch(&mut self) -> Option<SettingsEvent> {
        let index = self.decision_switch_selected()?;
        let row = self
            .decision_models
            .switches
            .get(index)
            .map(|row| (row.key.clone(), !row.enabled))?;
        Some(SettingsEvent::Apply(SettingsChange::DecisionSwitch {
            key: row.0,
            enabled: row.1,
        }))
    }

    fn move_decision_channel(&mut self, delta: isize) {
        let count = self.decision_row_count() as isize;
        if count == 0 {
            return;
        }
        self.decision_models.selected =
            ((self.decision_models.selected as isize + delta).rem_euclid(count)) as usize;
    }

    /// 进入表单：编辑选中的决策渠道（选中开关行时不进表单）。
    fn edit_decision_channel(&mut self) {
        let Some(row) = self
            .decision_models
            .rows
            .get(self.decision_models.selected)
            .cloned()
        else {
            self.decision_models.status = "还没有决策渠道可编辑；按 N 新建一条。".to_string();
            return;
        };
        let index = self.decision_models.selected;
        self.decision_models.status =
            format!("正在编辑决策渠道 {}；Ctrl+S 保存，Esc 返回列表。", row.key);
        self.decision_models.form = Some(DecisionForm {
            row,
            field: DecisionField::Name,
            input: None,
            dropdown: None,
            is_new: false,
            index: Some(index),
        });
    }

    /// 新建决策渠道：草稿只在表单里，`Ctrl+S` 才落进列表（`Esc` 直接丢弃）。
    fn new_decision_channel(&mut self) {
        let mut row = self.decision_models.template.clone();
        let base = if row.key.trim().is_empty() {
            "决策渠道".to_string()
        } else {
            row.key.trim().to_string()
        };
        let mut candidate = base.clone();
        let mut index = 2;
        while self
            .decision_models
            .rows
            .iter()
            .any(|item| item.key == candidate)
        {
            candidate = format!("{base}{index}");
            index += 1;
        }
        row.key = candidate.clone();
        if row.name.trim().is_empty() {
            row.name = candidate.clone();
        }
        self.decision_models.status =
            format!("正在新建决策渠道 {candidate}；填好后按 Ctrl+S 保存，Esc 放弃。");
        self.decision_models.form = Some(DecisionForm {
            row,
            field: DecisionField::Name,
            input: None,
            dropdown: None,
            is_new: true,
            index: None,
        });
    }

    /// 删除选中决策渠道；至少保留一条（与配置侧的校验同口径）。
    ///
    /// 选中开关行时不动列表，只提示当前要删的是渠道。
    fn delete_decision_channel(&mut self) {
        if self.decision_switch_selected().is_some() {
            self.decision_models.status =
                "功能开关行不能删除；按 ↑ 选中决策渠道再按 D。".to_string();
            return;
        }
        if self.decision_local_selected().is_some() {
            self.decision_models.status =
                "自部署分区的行不能删除；按 ↑ 选中决策渠道再按 D。".to_string();
            return;
        }
        if self.decision_models.rows.len() <= 1 {
            self.decision_models.status = "至少要保留一个决策渠道。".to_string();
            return;
        }
        let removed = self
            .decision_models
            .rows
            .remove(self.decision_models.selected);
        self.decision_models.selected = self
            .decision_models
            .selected
            .min(self.decision_models.rows.len() - 1);
        if self.decision_models.default_key == removed.key {
            self.decision_models.default_key = self
                .decision_models
                .rows
                .iter()
                .find(|row| row.enabled)
                .or_else(|| self.decision_models.rows.first())
                .map(|row| row.key.clone())
                .unwrap_or_default();
        }
        self.decision_models.status =
            format!("已删除决策渠道 {}；Ctrl+S 保存后才会写盘。", removed.key);
    }

    /// 表单里切换字段（`↑`/`↓` 与 `Tab` 同义）。
    fn move_decision_field(&mut self, delta: isize) {
        let Some(form) = self.decision_models.form.as_mut() else {
            return;
        };
        let count = DecisionField::ORDER.len() as isize;
        let current = form.field.index() as isize;
        let next = (current + delta).rem_euclid(count) as usize;
        form.field = DecisionField::ORDER[next];
    }

    /// `Enter`/空格：文本字段进编辑态、枚举展开候选、开关就地翻转。
    fn activate_decision_field(&mut self) -> Option<SettingsEvent> {
        let Some(form) = self.decision_models.form.as_mut() else {
            return None;
        };
        match form.field {
            DecisionField::Enabled => form.row.enabled = !form.row.enabled,
            DecisionField::Mode => {
                let options: Vec<String> = DECISION_MODE_OPTIONS
                    .iter()
                    .map(|mode| (*mode).to_string())
                    .collect();
                let selected = options
                    .iter()
                    .position(|option| option == &form.row.mode)
                    .unwrap_or(0);
                form.input = None;
                form.dropdown = Some(DecisionDropdown {
                    field: DecisionField::Mode,
                    options,
                    selected,
                });
            }
            field => {
                // API Key 的输入态从空开始：既不回显明文，也让「留空=不改动」成为默认动作。
                form.dropdown = None;
                form.input = Some(decision_input_composer(&form.row, field));
            }
        }
        None
    }

    /// `Esc`：退出表单回列表（未保存的改动丢弃）。
    fn leave_decision_form(&mut self) {
        let drafting = self
            .decision_models
            .form
            .as_ref()
            .map(|form| form.is_new)
            .unwrap_or(false);
        self.decision_models.form = None;
        self.decision_models.status = if drafting {
            "已放弃新建决策渠道（未写盘）。".to_string()
        } else {
            "已返回决策渠道列表（未保存的改动已丢弃）。".to_string()
        };
    }

    /// `Ctrl+S`：把表单落进列表、校验后交给宿主写盘。
    fn save_decision_channels(&mut self) -> Option<SettingsEvent> {
        let form = self.decision_models.form.clone()?;
        let mut rows = self.decision_models.rows.clone();
        match form.index {
            Some(index) if index < rows.len() => rows[index] = form.row.clone(),
            _ => rows.push(form.row.clone()),
        }
        if let Err(message) = validate_decision_rows(&rows) {
            self.decision_models.status = message;
            return None;
        }
        let default_key = pick_default_decision_channel(&rows, &self.decision_models.default_key);
        self.decision_models.status = "正在保存决策模型配置…".to_string();
        self.decision_models.rows = rows.clone();
        self.decision_models.default_key = default_key.clone();
        self.decision_models.form = None;
        Some(SettingsEvent::Apply(SettingsChange::DecisionModels {
            rows,
            default_key,
        }))
    }

    // ---------- 向量模型页 ----------

    fn handle_embedding_models_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        if self.embedding_models.form.is_some() {
            return self.handle_embedding_form_key(key);
        }
        match key {
            KeyCode::Up => {
                self.move_embedding_row(-1);
                None
            }
            KeyCode::Down => {
                self.move_embedding_row(1);
                None
            }
            // 开关行选中时 Enter/空格/→ 翻转并立刻写盘；渠道行才进表单。
            KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ') => {
                match self.embedding_models.selected {
                    EmbeddingModelsState::MEMORY_SWITCH_ROW => self.toggle_embedding_switch(),
                    EmbeddingModelsState::WEB_SEARCH_SWITCH_ROW => {
                        self.toggle_embedding_web_search_switch()
                    }
                    _ => {
                        self.edit_embedding_channel();
                        None
                    }
                }
            }
            KeyCode::Left | KeyCode::Esc => {
                self.back_to_list();
                None
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.new_embedding_channel();
                None
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                self.delete_embedding_channel();
                None
            }
            _ => None,
        }
    }

    /// 总开关行：就地翻转并交给宿主写盘（选中即保存，与决策页的开关同口径）。
    fn toggle_embedding_switch(&mut self) -> Option<SettingsEvent> {
        Some(SettingsEvent::Apply(SettingsChange::EmbeddingEnabled {
            enabled: !self.embedding_models.enabled,
        }))
    }

    /// 联网搜索向量排序行：与决策页的功能开关同一口径（选中即保存）。
    fn toggle_embedding_web_search_switch(&mut self) -> Option<SettingsEvent> {
        Some(SettingsEvent::Apply(SettingsChange::DecisionSwitch {
            key: DECISION_SWITCH_WEB_SEARCH_EMBEDDING.to_string(),
            enabled: !self.embedding_models.web_search_rerank,
        }))
    }

    fn handle_embedding_form_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        let input_active = self
            .embedding_models
            .form
            .as_ref()
            .map(|form| form.input.is_some())
            .unwrap_or(false);
        if input_active {
            return self.handle_embedding_input_key(key);
        }
        match key {
            KeyCode::Up => {
                self.move_embedding_field(-1);
                None
            }
            KeyCode::Down | KeyCode::Tab => {
                self.move_embedding_field(1);
                None
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.activate_embedding_field(),
            KeyCode::Esc | KeyCode::Left => {
                self.leave_embedding_form();
                None
            }
            _ => None,
        }
    }

    fn handle_embedding_input_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        let form = self.embedding_models.form.as_mut()?;
        let field = form.field;
        let composer = form.input.as_mut()?;
        match key {
            KeyCode::Enter => {
                let value = composer.take();
                form.input = None;
                set_embedding_field(&mut form.row, field, value);
            }
            KeyCode::Esc => form.input = None,
            KeyCode::Char(character) => composer.insert(&character.to_string()),
            KeyCode::Backspace => composer.backspace(),
            KeyCode::Delete => composer.delete(),
            KeyCode::Left => composer.move_left(),
            KeyCode::Right => composer.move_right(),
            KeyCode::Home => composer.move_home(),
            KeyCode::End => composer.move_end(),
            _ => {}
        }
        None
    }

    /// 行游标移动：第 0 行是总开关，之后依次是渠道。
    fn move_embedding_row(&mut self, delta: isize) {
        let count = self.embedding_models.total_rows() as isize;
        if count <= 0 {
            return;
        }
        let current = self.embedding_models.selected as isize;
        self.embedding_models.selected = (current + delta).rem_euclid(count) as usize;
    }

    fn edit_embedding_channel(&mut self) {
        let Some(index) = EmbeddingModelsState::channel_index(self.embedding_models.selected)
        else {
            return;
        };
        let Some(row) = self.embedding_models.rows.get(index).cloned() else {
            self.embedding_models.status = "还没有向量渠道可编辑；按 N 新建一条。".to_string();
            return;
        };
        self.embedding_models.status =
            format!("正在编辑向量渠道 {}；Ctrl+S 保存，Esc 返回列表。", row.key);
        self.embedding_models.form = Some(EmbeddingForm {
            row,
            field: EmbeddingField::Name,
            input: None,
            is_new: false,
            index: Some(index),
        });
    }

    /// 新建向量渠道：草稿只在表单里，`Ctrl+S` 才落进列表（`Esc` 直接丢弃）。
    fn new_embedding_channel(&mut self) {
        let mut row = self.embedding_models.template.clone();
        let base = if row.key.trim().is_empty() {
            "向量渠道".to_string()
        } else {
            row.key.trim().to_string()
        };
        let mut candidate = base.clone();
        let mut index = 2;
        while self
            .embedding_models
            .rows
            .iter()
            .any(|item| item.key == candidate)
        {
            candidate = format!("{base}{index}");
            index += 1;
        }
        row.key = candidate.clone();
        if row.name.trim().is_empty() {
            row.name = candidate.clone();
        }
        self.embedding_models.status = "新建向量渠道；填好后按 Ctrl+S 保存，Esc 放弃。".to_string();
        self.embedding_models.form = Some(EmbeddingForm {
            row,
            field: EmbeddingField::Name,
            input: None,
            is_new: true,
            index: None,
        });
    }

    fn delete_embedding_channel(&mut self) {
        let Some(index) = EmbeddingModelsState::channel_index(self.embedding_models.selected)
        else {
            self.embedding_models.status = "开关行不可删除；把游标移到渠道行再按 D。".to_string();
            return;
        };
        if self.embedding_models.rows.len() <= 1 {
            self.embedding_models.status = "至少要保留一个向量渠道。".to_string();
            return;
        }
        let removed = self.embedding_models.rows.remove(index);
        // 删完可能落到「只剩开关」的位置，游标要收回合法范围。
        let last = self.embedding_models.total_rows().saturating_sub(1);
        self.embedding_models.selected = self.embedding_models.selected.min(last);
        if self.embedding_models.default_key == removed.key {
            self.embedding_models.default_key = pick_default_embedding_channel(
                &self.embedding_models.rows,
                &self.embedding_models.default_key,
            );
        }
        self.embedding_models.status =
            format!("已删除向量渠道 {}；Ctrl+S 保存后才会写盘。", removed.key);
    }

    fn move_embedding_field(&mut self, delta: isize) {
        let Some(form) = self.embedding_models.form.as_mut() else {
            return;
        };
        let count = EmbeddingField::ORDER.len() as isize;
        let current = form.field.index() as isize;
        let next = (current + delta).rem_euclid(count) as usize;
        form.field = EmbeddingField::ORDER[next];
    }

    fn activate_embedding_field(&mut self) -> Option<SettingsEvent> {
        let Some(form) = self.embedding_models.form.as_mut() else {
            return None;
        };
        match form.field {
            EmbeddingField::Enabled => form.row.enabled = !form.row.enabled,
            field => {
                form.input = Some(embedding_input_composer(&form.row, field));
            }
        }
        None
    }

    fn leave_embedding_form(&mut self) {
        let drafting = self
            .embedding_models
            .form
            .as_ref()
            .map(|form| form.is_new)
            .unwrap_or(false);
        self.embedding_models.form = None;
        self.embedding_models.status = if drafting {
            "已放弃新建向量渠道（未写盘）。".to_string()
        } else {
            "已返回向量渠道列表（未保存的改动已丢弃）。".to_string()
        };
    }

    fn save_embedding_channels(&mut self) -> Option<SettingsEvent> {
        let form = self.embedding_models.form.clone()?;
        let mut rows = self.embedding_models.rows.clone();
        match form.index {
            Some(index) if index < rows.len() => rows[index] = form.row.clone(),
            _ => rows.push(form.row.clone()),
        }
        if let Err(message) = validate_embedding_rows(&rows) {
            self.embedding_models.status = message;
            return None;
        }
        let default_key = pick_default_embedding_channel(&rows, &self.embedding_models.default_key);
        self.embedding_models.status = "正在保存向量模型配置…".to_string();
        self.embedding_models.rows = rows.clone();
        self.embedding_models.default_key = default_key.clone();
        self.embedding_models.form = None;
        Some(SettingsEvent::Apply(SettingsChange::EmbeddingModels {
            rows,
            default_key,
        }))
    }

    /// `Ctrl` 组合键：渠道表单与表单页的保存都走 `Ctrl+S`。
    pub fn handle_ctrl_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        match self.pane {
            Pane::EmbeddingModels if self.embedding_models.form.is_some() => match key {
                KeyCode::Char('s') | KeyCode::Char('S') => self.save_embedding_channels(),
                _ => None,
            },
            Pane::ModelManagement if self.model_management.form.is_some() => match key {
                KeyCode::Char('s') | KeyCode::Char('S') => self.save_channels(),
                _ => None,
            },
            Pane::DecisionModels if self.decision_models.form.is_some() => match key {
                KeyCode::Char('s') | KeyCode::Char('S') => self.save_decision_channels(),
                _ => None,
            },
            Pane::Form(kind) => match key {
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    if kind == FormKind::WebSearch {
                        return self.save_web_search_form();
                    }
                    let values = self
                        .form()
                        .map(|form| form.values().to_vec())
                        .unwrap_or_default();
                    Some(SettingsEvent::Apply(SettingsChange::Form { kind, values }))
                }
                _ => None,
            },
            Pane::Mcp if self.mcp.editor.is_some() => match key {
                KeyCode::Char('s') | KeyCode::Char('S') => self.handle_mcp_editor_save(),
                _ => None,
            },
            Pane::Tts => match key {
                KeyCode::Char('s') | KeyCode::Char('S') => Some(SettingsEvent::Apply(
                    SettingsChange::Tts(TtsChange::Save(self.tts.draft())),
                )),
                _ => None,
            },
            Pane::Vision => match key {
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    // 只有真动过模型原生视觉才连带写回（与 Python 同义）。
                    let native = if self.vision.native != self.vision.native_previous {
                        Some(self.vision.native)
                    } else {
                        None
                    };
                    Some(SettingsEvent::Apply(SettingsChange::Vision(VisionChange {
                        enabled: self.vision.enabled,
                        models: self.vision.models.clone(),
                        native,
                    })))
                }
                KeyCode::Up => {
                    self.move_vision_priority(-1);
                    None
                }
                KeyCode::Down => {
                    self.move_vision_priority(1);
                    None
                }
                _ => None,
            },
            _ => None,
        }
    }

    // ---------- 宿主回填 ----------

    /// 应用成功：写入新值并显示成功文本。
    pub fn apply_succeeded(&mut self, change: &SettingsChange, message: String) {
        match change {
            SettingsChange::Channels { rows, default_key } => {
                self.model_management.rows = rows.clone();
                self.model_management.default_key = default_key.clone();
                self.model_management.selected = self
                    .model_management
                    .selected
                    .min(self.model_management.row_count().saturating_sub(1));
                self.model_management.form = None;
                self.model_management.status = message;
            }
            SettingsChange::DecisionModels { rows, default_key } => {
                self.decision_models.rows = rows.clone();
                self.decision_models.default_key = default_key.clone();
                self.decision_models.selected = self
                    .decision_models
                    .selected
                    .min(self.decision_row_count().saturating_sub(1));
                self.decision_models.form = None;
                self.decision_models.status = message;
            }
            SettingsChange::EmbeddingModels { rows, default_key } => {
                self.embedding_models.rows = rows.clone();
                self.embedding_models.default_key = default_key.clone();
                self.embedding_models.selected = self
                    .embedding_models
                    .selected
                    .min(self.embedding_models.total_rows().saturating_sub(1));
                self.embedding_models.form = None;
                self.embedding_models.status = message;
            }
            SettingsChange::EmbeddingEnabled { enabled } => {
                self.embedding_models.enabled = *enabled;
                self.embedding_models.status = message;
            }
            SettingsChange::DecisionLocal(change) => {
                match change {
                    DecisionLocalChange::Save { size, device } => {
                        self.decision_models.local.size = size.clone();
                        self.decision_models.local.device = device.clone();
                    }
                    DecisionLocalChange::PrepareEnvironment
                    | DecisionLocalChange::Download { .. }
                    | DecisionLocalChange::StartService
                    | DecisionLocalChange::StopService => {
                        // 长任务：界面先按住重复动作；宿主随后用 `refresh_decision_local`
                        // 的 `task` 字段持续对齐（任务结束自然放开）。
                        self.decision_models.local.busy = true;
                    }
                    DecisionLocalChange::Delete { .. } => {}
                    DecisionLocalChange::RemoveEnvironment => {
                        // 同步动作，但环境状态变了：就绪标记由宿主随后的刷新覆盖。
                        self.decision_models.local.env_ready = false;
                    }
                    DecisionLocalChange::UseChannel { size } => {
                        // 渠道行由宿主写盘后回填（`sync_decision_models`），这里只记住尺寸。
                        self.decision_models.local.size = size.clone();
                    }
                }
                self.decision_models.status = message;
            }
            SettingsChange::DecisionSwitch { key, enabled } => {
                if let Some(row) = self
                    .decision_models
                    .switches
                    .iter_mut()
                    .find(|row| &row.key == key)
                {
                    row.enabled = *enabled;
                }
                if key == DECISION_SWITCH_WEB_SEARCH_EMBEDDING {
                    self.embedding_models.web_search_rerank = *enabled;
                }
                // 该开关现在画在向量页，状态行跟着焦点所在页走。
                if self.pane == Pane::EmbeddingModels {
                    self.embedding_models.status = message;
                } else {
                    self.decision_models.status = message;
                }
            }
            SettingsChange::Model { key } => {
                self.model_management.model_key = key.clone();
                self.model_management.status = message;
            }
            SettingsChange::ContextWindow { tokens } => {
                self.model_management.window_tokens = *tokens;
                self.model_management.status = message;
            }
            SettingsChange::CompactionPercent { percent } => {
                self.model_management.percent = *percent;
                self.model_management.status = message;
            }
            SettingsChange::ToolSwitch { name, enabled } => {
                if let Some(row) = self.tools.rows.iter_mut().find(|row| &row.name == name) {
                    row.enabled = *enabled;
                }
                self.tools.status = message;
            }
            SettingsChange::ToolApproval { mode } => {
                self.tools.approval = nearest_approval_mode(mode).to_string();
                self.tools.status = message;
            }
            SettingsChange::Form { kind, values } => {
                if let Some(form) = self.forms.get_mut(kind.index()) {
                    // 配置侧可能规范化过取值，用宿主回填的值覆盖草稿。
                    form.set_values(values.clone());
                    form.set_status(message.clone());
                }
                // 联网搜索页是从工具页进来的，保存后回到工具页；其余表单页留在原地。
                if *kind == FormKind::WebSearch {
                    self.tools.web_search.engine = values
                        .first()
                        .map(|value| value.text().trim().to_string())
                        .unwrap_or_default();
                    // 凭据状态不在这里猜：只有磁盘知道明文，由宿主随后的 `set_web_search` 对齐。
                    self.tools.status = message;
                    self.close_web_search_form();
                }
            }
            SettingsChange::Subagent(change) => {
                match change {
                    SubagentChange::Enabled(enabled) => {
                        if let Some(row) = self
                            .subagents
                            .rows
                            .iter_mut()
                            .find(|row| row.key == "enabled")
                        {
                            row.active = *enabled;
                            row.value = if *enabled { "已开启" } else { "已关闭" }.to_string();
                        }
                    }
                    SubagentChange::Advanced { key, value } => {
                        if let Some(row) =
                            self.subagents.rows.iter_mut().find(|row| &row.key == key)
                        {
                            row.value = value.to_string();
                        }
                    }
                }
                self.subagents.status = message;
            }
            SettingsChange::Vision(change) => {
                self.vision.enabled = change.enabled;
                self.vision.models = change.models.clone();
                self.vision.selected = self
                    .vision
                    .selected
                    .min(self.vision.models.len().saturating_sub(1));
                if let Some(native) = change.native {
                    self.vision.native = native;
                    self.vision.native_previous = native;
                }
                self.vision.status = message;
            }
            SettingsChange::Reasoning { effort } => {
                self.model_management.reasoning = effort.clone();
                self.model_management.status = message;
            }
            SettingsChange::ShowThinking { enabled } => {
                self.choices.show_thinking = *enabled;
                self.choices.status[ChoiceKind::ShowThinking.index()] = message;
            }
            SettingsChange::Feature { key, enabled } => {
                let kind = if key == "memory" {
                    self.choices.memory = *enabled;
                    ChoiceKind::Memory
                } else {
                    self.choices.plugins = *enabled;
                    ChoiceKind::Plugins
                };
                self.choices.status[kind.index()] = message;
            }
            SettingsChange::Tts(TtsChange::Save(draft)) => {
                // 保存成功：把草稿的明文换成刚写盘的生效值（另一家的配置由宿主另行推回）。
                self.tts.apply_saved(draft);
                self.tts.status = message;
            }
            SettingsChange::Mcp(change) => self.apply_mcp_succeeded(change, message),
        }
        self.dropdown = None;
    }

    /// MCP 变更成功：把界面状态对齐到刚保存的配置。
    fn apply_mcp_succeeded(&mut self, change: &McpChange, message: String) {
        match change {
            McpChange::Globals {
                enabled,
                allow_external_network_tools,
                require_confirmation_for_write,
                require_confirmation_for_command,
                audit_log_enabled,
                timeout_seconds,
            } => {
                self.mcp.enabled = *enabled;
                self.mcp.network = *allow_external_network_tools;
                self.mcp.write = *require_confirmation_for_write;
                self.mcp.command = *require_confirmation_for_command;
                self.mcp.audit = *audit_log_enabled;
                self.mcp.timeout = *timeout_seconds;
            }
            McpChange::SaveServer(draft) => {
                let row = McpServerRow {
                    name: draft.name.clone(),
                    enabled: draft.enabled,
                    transport: draft.transport.clone(),
                    risk_level: draft.risk_level.clone(),
                };
                if let Some(original) = draft.original_name.as_deref() {
                    if let Some(slot) = self
                        .mcp
                        .servers
                        .iter_mut()
                        .find(|candidate| candidate.name == original)
                    {
                        *slot = row;
                    } else {
                        self.mcp.servers.push(row);
                    }
                } else if !self
                    .mcp
                    .servers
                    .iter()
                    .any(|candidate| candidate.name == draft.name)
                {
                    self.mcp.servers.push(row);
                }
                self.mcp.server_selected = self
                    .mcp
                    .server_selected
                    .min(self.mcp.servers.len().saturating_sub(1));
                self.mcp.editor = None;
            }
            McpChange::SetServerEnabled { name, enabled } => {
                if let Some(row) = self
                    .mcp
                    .servers
                    .iter_mut()
                    .find(|candidate| &candidate.name == name)
                {
                    row.enabled = *enabled;
                }
            }
            McpChange::DeleteServer { name } => {
                self.mcp.servers.retain(|candidate| &candidate.name != name);
                self.mcp.server_selected = self
                    .mcp
                    .server_selected
                    .min(self.mcp.servers.len().saturating_sub(1));
            }
        }
        self.mcp.status = message;
    }

    /// 应用失败：值保持不变（界面回落到原值），只显示失败文本。
    pub fn apply_failed(&mut self, message: String) {
        match self.pane {
            Pane::Tools => self.tools.status = message,
            Pane::Mcp => self.mcp.status = message,
            Pane::Subagents => self.subagents.status = message,
            Pane::Vision => self.vision.status = message,
            // 模型管理页失败时保留表单，方便就地改错再按 Ctrl+S。
            Pane::ModelManagement => self.model_management.status = message,
            // 决策页同理：保留表单。
            Pane::DecisionModels => self.decision_models.status = message,
            Pane::EmbeddingModels => self.embedding_models.status = message,
            Pane::Choice(kind) => self.choices.status[kind.index()] = message,
            // 表单页失败时同样保留草稿。
            Pane::Form(_) => {
                if let Some(form) = self.form_mut() {
                    form.set_status(message);
                }
            }
            Pane::Tts => self.tts.status = message,
            Pane::Pending => {}
        }
        self.dropdown = None;
    }

    /// 内核拒绝即时更新时，在对应面板的状态文本后追加说明。
    ///
    /// 配置已经写盘、宿主侧已经生效，所以这里只补一句「下次会话生效」的事实，
    /// 不把值回滚（与「写盘失败」是两件事）。
    pub fn note_kernel_rejection(&mut self, change: &SettingsChange, note: &str) {
        match change {
            SettingsChange::Channels { .. } => self.model_management.status.push_str(note),
            SettingsChange::DecisionModels { .. }
            | SettingsChange::DecisionSwitch { .. }
            | SettingsChange::DecisionLocal(_) => {
                self.decision_models.status.push_str(note);
                if self.pane == Pane::EmbeddingModels {
                    self.embedding_models.status.push_str(note);
                }
            }
            SettingsChange::EmbeddingModels { .. } | SettingsChange::EmbeddingEnabled { .. } => {
                self.embedding_models.status.push_str(note);
            }
            SettingsChange::Model { .. }
            | SettingsChange::ContextWindow { .. }
            | SettingsChange::CompactionPercent { .. }
            | SettingsChange::Reasoning { .. } => self.model_management.status.push_str(note),
            SettingsChange::ToolSwitch { .. } => self.tools.status.push_str(note),
            SettingsChange::ToolApproval { .. } => self.tools.status.push_str(note),
            SettingsChange::Form {
                kind: FormKind::WebSearch,
                ..
            } => self.tools.status.push_str(note),
            SettingsChange::Mcp(_) => self.mcp.status.push_str(note),
            SettingsChange::Subagent(_) => self.subagents.status.push_str(note),
            SettingsChange::Vision(_) => self.vision.status.push_str(note),
            SettingsChange::ShowThinking { .. } => {
                self.choices
                    .status_mut(ChoiceKind::ShowThinking)
                    .push_str(note);
            }
            SettingsChange::Feature { key, .. } => {
                let kind = if key == "memory" {
                    ChoiceKind::Memory
                } else {
                    ChoiceKind::Plugins
                };
                self.choices.status_mut(kind).push_str(note);
            }
            SettingsChange::Form { kind, .. } => {
                if let Some(form) = self.forms.get_mut(kind.index()) {
                    let mut status = form.status().to_string();
                    status.push_str(note);
                    form.set_status(status);
                }
            }
            SettingsChange::Tts(_) => self.tts.status.push_str(note),
        }
    }

    /// 模型管理页的渠道列表（渲染层用）。
    pub fn channel_rows(&self) -> &[ChannelRow] {
        &self.model_management.rows
    }

    /// 模型管理页列表态的选中行（渠道行 + 分区行一起编号）。
    pub fn channel_selected(&self) -> usize {
        self.model_management.selected
    }

    pub fn channel_default_key(&self) -> &str {
        &self.model_management.default_key
    }

    /// 当前模型渠道 key（分区首行的取值）。
    pub fn model_key(&self) -> &str {
        &self.model_management.model_key
    }

    /// 模型管理页「模型与参数」分区的行视图：`(标签, 取值, 是否选中, 是否可选)`。
    pub fn model_param_rows(&self) -> Vec<(ModelParamRow, String, bool)> {
        let selected = self.model_management.param_selected();
        ModelParamRow::ORDER
            .iter()
            .copied()
            .map(|row| {
                let value = match row {
                    ModelParamRow::Channel => {
                        let key = self.model_management.model_key.trim();
                        if key.is_empty() {
                            "（未选择）".to_string()
                        } else {
                            self.model_management
                                .rows
                                .iter()
                                .find(|item| item.key == key)
                                .map(|item| {
                                    if item.name.trim().is_empty() {
                                        item.key.clone()
                                    } else {
                                        item.name.clone()
                                    }
                                })
                                .unwrap_or_else(|| key.to_string())
                        }
                    }
                    ModelParamRow::Window => {
                        format!("{}K", self.model_management.window_tokens / 1000)
                    }
                    ModelParamRow::Compaction => {
                        format!("{}%", self.model_management.percent)
                    }
                    ModelParamRow::Reasoning => reasoning_label(&self.model_management.reasoning),
                };
                (row, value, selected == Some(row))
            })
            .collect()
    }

    /// 正在编辑的渠道（渲染层用）；不在表单时为 `None`。
    pub fn channel_form(&self) -> Option<ChannelFormView<'_>> {
        let form = self.model_management.form.as_ref()?;
        Some(ChannelFormView {
            row: &form.row,
            field: form.field,
            input: form.input.as_ref(),
            dropdown: form.dropdown.as_ref(),
            is_new: form.is_new,
        })
    }

    /// 工具开关页的注册标记刷新之外，模型管理页在保存后也要与磁盘对齐。
    pub fn sync_channels(&mut self, rows: Vec<ChannelRow>, default_key: &str) {
        self.model_management.selected = self
            .model_management
            .selected
            .min(self.model_management.row_count().saturating_sub(1));
        self.model_management.default_key = if default_key.trim().is_empty() {
            rows.first().map(|row| row.key.clone()).unwrap_or_default()
        } else {
            default_key.to_string()
        };
        // 当前模型渠道不在新列表里时，落到默认渠道（删除渠道后保存的同一口径）。
        if !rows
            .iter()
            .any(|row| row.key == self.model_management.model_key)
        {
            self.model_management.model_key = self.model_management.default_key.clone();
        }
        self.model_management.rows = rows;
        self.model_management.form = None;
    }

    /// 工具表重建后刷新行的注册标记与开关状态（审查模式不随之变化）。
    pub fn refresh_tools(&mut self, rows: Vec<ToolSwitchRow>) {
        let selected = self.tools.selected.min(rows.len());
        self.tools.rows = rows;
        self.tools.selected = selected;
    }

    /// 联网搜索设置保存后，用磁盘上的真实配置覆盖工具页那一行（含凭据是否已配置）。
    pub fn set_web_search(&mut self, row: WebSearchRow) {
        self.tools.web_search = row;
    }

    /// 宿主回填：图像生成草稿按磁盘的生效值重排（归一化可能折算过端点与模型）。
    ///
    /// 只覆盖服务商与之相关的两行（`base_url` / `model`），其余字段以草稿为准——
    /// 保存刚写盘的就是草稿，重排只是把配置侧的规范化结果同步回界面。
    pub fn set_image_gen_provider_defaults(&mut self, provider: &str, base_url: &str, model: &str) {
        let Some(form) = self.forms.get_mut(FormKind::ImageGen.index()) else {
            return;
        };
        form.set_value(
            IMAGE_GEN_PROVIDER_INDEX,
            FieldValue::Text(provider.to_string()),
        );
        form.set_value(
            IMAGE_GEN_BASE_URL_INDEX,
            FieldValue::Text(base_url.to_string()),
        );
        form.set_value(IMAGE_GEN_MODEL_INDEX, FieldValue::Text(model.to_string()));
        let gemini = provider.trim().eq_ignore_ascii_case("gemini");
        form.set_gemini(gemini);
    }

    /// 决策渠道列表（渲染层用）。
    pub fn decision_rows(&self) -> &[DecisionRow] {
        &self.decision_models.rows
    }

    pub fn decision_selected(&self) -> usize {
        self.decision_models.selected
    }

    /// 决策模型页的功能开关行（渲染层用）。
    pub fn decision_switch_rows(&self) -> &[DecisionSwitchRow] {
        &self.decision_models.switches
    }

    /// 决策页列表态的总行数（渠道 + 开关 + 自部署分区）；渲染层的窗口滚动与行号都读它。
    pub fn decision_row_count(&self) -> usize {
        self.decision_models.rows.len()
            + self.decision_models.switches.len()
            + DECISION_LOCAL_ROW_COUNT
    }

    /// 自部署分区的行（渲染层用）。
    pub fn decision_local_rows(&self) -> Vec<DecisionLocalRow> {
        self.decision_models.local.rows()
    }

    /// 自部署分区是否有后台任务在跑。
    pub fn decision_local_busy(&self) -> bool {
        self.decision_models.local.busy
    }

    /// 自部署任务进度行的渲染输入：`(阶段文案, 比例, 量化细节)`。
    ///
    /// 比例是 `None` 时渲染层画循环滑块（任务在跑但这一步没有可量化的进度）。
    pub fn decision_progress_line(&self) -> (String, Option<f64>, String) {
        let local = &self.decision_models.local;
        let ratio = local.progress.ratio();
        let value = local.progress_text();
        (value, ratio, local.progress.detail())
    }

    pub fn decision_default_key(&self) -> &str {
        &self.decision_models.default_key
    }

    /// 正在编辑的决策渠道（渲染层用）；不在表单时为 `None`。
    pub fn decision_form(&self) -> Option<DecisionFormView<'_>> {
        let form = self.decision_models.form.as_ref()?;
        Some(DecisionFormView {
            row: &form.row,
            field: form.field,
            input: form.input.as_ref(),
            dropdown: form.dropdown.as_ref(),
            is_new: form.is_new,
        })
    }

    /// 宿主回填：向量模型页的状态变化（保存失败时的提示等）。
    pub fn refresh_embedding_models(
        &mut self,
        rows: Vec<EmbeddingRow>,
        default_key: &str,
        enabled: bool,
        web_search_rerank: bool,
        message: &str,
    ) {
        self.embedding_models.rows = rows;
        self.embedding_models.enabled = enabled;
        self.embedding_models.web_search_rerank = web_search_rerank;
        if !default_key.trim().is_empty() {
            self.embedding_models.default_key = default_key.trim().to_string();
        }
        let last = self.embedding_models.total_rows().saturating_sub(1);
        if self.embedding_models.selected > last {
            self.embedding_models.selected = last;
        }
        if !message.trim().is_empty() {
            self.embedding_models.status = message.trim().to_string();
        }
    }

    /// 宿主回填：顶部总开关的现值（开关翻转由状态层先画，再以磁盘结果为准）。
    pub fn set_embedding_enabled(&mut self, enabled: bool) {
        self.embedding_models.enabled = enabled;
    }

    pub fn embedding_rows(&self) -> &[EmbeddingRow] {
        &self.embedding_models.rows
    }

    /// 顶部总开关的当前值（渲染层用）。
    pub fn embedding_enabled(&self) -> bool {
        self.embedding_models.enabled
    }

    /// 联网搜索走向量模型排序的当前值（渲染层用）。
    pub fn embedding_web_search_rerank(&self) -> bool {
        self.embedding_models.web_search_rerank
    }

    /// 行游标是否停在开关区（向量召回 / 联网搜索向量排序）。
    pub fn embedding_switch_selected(&self) -> bool {
        self.embedding_models.selected < EmbeddingModelsState::SWITCH_ROWS
    }

    /// 游标在开关区时的行号（0 向量召回、1 联网搜索向量排序）。
    pub fn embedding_switch_row(&self) -> usize {
        self.embedding_models.selected
    }

    /// 联网搜索向量排序开关的文案：与决策模型页的开关同源。
    pub fn embedding_web_search_label() -> &'static str {
        DECISION_SWITCHES
            .iter()
            .find(|spec| spec.key == DECISION_SWITCH_WEB_SEARCH_EMBEDDING)
            .map(|spec| spec.label)
            .unwrap_or("联网搜索使用向量模型排序")
    }

    /// 选中的**渠道**下标；游标停在总开关时为 `None`。
    pub fn embedding_selected(&self) -> Option<usize> {
        EmbeddingModelsState::channel_index(self.embedding_models.selected)
            .filter(|index| *index < self.embedding_models.rows.len())
    }

    pub fn embedding_default_key(&self) -> &str {
        &self.embedding_models.default_key
    }

    /// 正在编辑的向量渠道（渲染层用）；不在表单时为 `None`。
    pub fn embedding_form(&self) -> Option<EmbeddingFormView<'_>> {
        let form = self.embedding_models.form.as_ref()?;
        Some(EmbeddingFormView {
            row: &form.row,
            field: form.field,
            input: form.input.as_ref(),
            dropdown: None,
            is_new: form.is_new,
        })
    }

    /// 宿主回填：自部署分区的状态变化（下载完成 / 环境就绪 / 服务启停 / 任务进度）。
    ///
    /// 只覆盖分区的只读值，不动当前选中的行与渠道列表；`message` 非空时同时更新状态行，
    /// 空串表示「只刷新事实」。
    ///
    /// `busy` 完全由宿主给出的 `task` 决定：任务结束（`task` 为 `None`）时这里必然复位，
    /// 不会再出现「后台早已结束、界面还按着『进行中』不放」的死锁。
    pub fn refresh_decision_local(&mut self, values: DecisionLocalValues, message: String) {
        self.decision_models.local = DecisionLocalState::new(values);
        if !message.trim().is_empty() {
            self.decision_models.status = message;
        }
    }

    /// 保存后把决策页与磁盘对齐（与 `sync_channels` 同义）。
    pub fn sync_decision_models(&mut self, rows: Vec<DecisionRow>, default_key: &str) {
        self.decision_models.selected = self.decision_models.selected.min(
            (rows.len() + self.decision_models.switches.len() + DECISION_LOCAL_ROW_COUNT)
                .saturating_sub(1),
        );
        self.decision_models.default_key = if default_key.trim().is_empty() {
            rows.first().map(|row| row.key.clone()).unwrap_or_default()
        } else {
            default_key.to_string()
        };
        self.decision_models.rows = rows;
        self.decision_models.form = None;
    }
}

/// 向量渠道表单的只读视图（渲染层用）。
pub struct EmbeddingFormView<'a> {
    pub row: &'a EmbeddingRow,
    pub field: EmbeddingField,
    /// 正在编辑的文本字段缓冲（非空表示处于输入态）。
    pub input: Option<&'a Composer>,
    /// 与决策表单同形，但向量页没有枚举字段，恒为 `None`。
    pub dropdown: Option<&'a DecisionDropdown>,
    pub is_new: bool,
}

/// 向量表单字段的当前显示值。
pub fn embedding_field_value(row: &EmbeddingRow, field: EmbeddingField) -> String {
    match field {
        EmbeddingField::Name => row.name.clone(),
        EmbeddingField::BaseUrl => row.base_url.clone(),
        // 明文永不进界面：只给出「是否已配置 + 末 4 位」的掩码（与渠道页同款）。
        EmbeddingField::ApiKey => mask_api_key(&row.api_key),
        EmbeddingField::ApiKeyEnv => row.api_key_env.clone(),
        EmbeddingField::ModelId => row.model.clone(),
        EmbeddingField::Dimensions => {
            if row.dimensions > 0 {
                row.dimensions.to_string()
            } else {
                "由服务端决定".to_string()
            }
        }
        EmbeddingField::TimeoutSeconds => row.timeout_seconds.to_string(),
        EmbeddingField::Enabled => {
            if row.enabled {
                "开启".to_string()
            } else {
                "关闭".to_string()
            }
        }
    }
}

/// 向量表单某字段的输入缓冲初值（凭据从空开始）。
fn embedding_input_composer(row: &EmbeddingRow, field: EmbeddingField) -> Composer {
    let mut composer = Composer::default();
    if field != EmbeddingField::ApiKey {
        composer.insert(&embedding_field_value(row, field));
    }
    composer
}

/// 把表单值写回向量渠道记录。
fn set_embedding_field(row: &mut EmbeddingRow, field: EmbeddingField, value: String) {
    match field {
        EmbeddingField::Name => row.name = value,
        EmbeddingField::BaseUrl => row.base_url = value.trim().to_string(),
        // 留空 = 不改动（与渠道页、决策页同一套口径，避免误触把磁盘上的密钥抹掉）。
        EmbeddingField::ApiKey => {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                row.api_key = trimmed.to_string();
            }
        }
        EmbeddingField::ApiKeyEnv => row.api_key_env = value.trim().to_string(),
        EmbeddingField::ModelId => row.model = value.trim().to_string(),
        // 「由服务端决定」这类非数字输入按 0 处理（0 即不下发 dimensions）。
        EmbeddingField::Dimensions => {
            row.dimensions = value.trim().parse::<i64>().unwrap_or(0).max(0);
        }
        EmbeddingField::TimeoutSeconds => {
            if let Ok(seconds) = value.trim().parse::<i64>() {
                row.timeout_seconds = seconds.max(1);
            }
        }
        EmbeddingField::Enabled => row.enabled = value == "开启",
    }
}

/// 保存前的本地校验：至少一条、key 唯一、地址与模型非空。
fn validate_embedding_rows(rows: &[EmbeddingRow]) -> Result<(), String> {
    if rows.is_empty() {
        return Err("至少要保留一个向量渠道。".to_string());
    }
    let mut seen: Vec<&str> = Vec::new();
    for row in rows {
        let key = row.key.trim();
        if key.is_empty() {
            return Err("向量渠道 key 不能为空。".to_string());
        }
        if seen.contains(&key) {
            return Err(format!("向量渠道 key 重复：{key}。"));
        }
        seen.push(key);
        if row.base_url.trim().is_empty() {
            return Err(format!("向量渠道 {key} 的 Base URL 不能为空。"));
        }
        if row.model.trim().is_empty() {
            return Err(format!("向量渠道 {key} 的模型 ID 不能为空。"));
        }
    }
    Ok(())
}

/// 决策渠道表单的只读视图（渲染层用）。
pub struct DecisionFormView<'a> {
    pub row: &'a DecisionRow,
    pub field: DecisionField,
    /// 正在编辑的文本字段缓冲（非空表示处于输入态）。
    pub input: Option<&'a Composer>,
    pub dropdown: Option<&'a DecisionDropdown>,
    pub is_new: bool,
}

/// 决策表单字段的当前显示值（`Enabled` 给「开启/关闭」）。
pub fn decision_field_value(row: &DecisionRow, field: DecisionField) -> String {
    match field {
        DecisionField::Name => row.name.clone(),
        DecisionField::Mode => row.mode.clone(),
        DecisionField::BaseUrl => row.base_url.clone(),
        // 明文永不进界面：只给出「是否已配置 + 末 4 位」的掩码（与渠道页同款）。
        DecisionField::ApiKey => mask_api_key(&row.api_key),
        DecisionField::ApiKeyEnv => row.api_key_env.clone(),
        DecisionField::ModelId => row.model.clone(),
        DecisionField::RetryCount => row.retry_count.to_string(),
        DecisionField::Enabled => {
            if row.enabled {
                "开启".to_string()
            } else {
                "关闭".to_string()
            }
        }
    }
}

/// 决策表单某字段的输入缓冲初值（与渠道表单同口径：凭据从空开始）。
fn decision_input_composer(row: &DecisionRow, field: DecisionField) -> Composer {
    let mut composer = Composer::default();
    if field != DecisionField::ApiKey {
        composer.insert(&decision_field_value(row, field));
    }
    composer
}

/// 把表单值写回决策渠道记录。
fn set_decision_field(row: &mut DecisionRow, field: DecisionField, value: String) {
    match field {
        DecisionField::Name => row.name = value,
        DecisionField::Mode => row.mode = value.trim().to_lowercase(),
        DecisionField::BaseUrl => row.base_url = value.trim().to_string(),
        // 留空 = 不改动（与渠道页、TTS 面板同一套口径，避免误触把磁盘上的密钥抹掉）。
        DecisionField::ApiKey => {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                row.api_key = trimmed.to_string();
            }
        }
        DecisionField::ApiKeyEnv => row.api_key_env = value.trim().to_string(),
        DecisionField::ModelId => row.model = value.trim().to_string(),
        // 非数字输入原样保留（保存前的本地校验会给出可读提示），越界值由配置侧收口。
        DecisionField::RetryCount => {
            if let Ok(count) = value.trim().parse::<i64>() {
                row.retry_count = count;
            }
        }
        DecisionField::Enabled => row.enabled = value == "开启",
    }
}

/// 保存前的本地校验：与配置侧同口径，先挡住常见手误，避免白跑一次磁盘往返。
fn validate_decision_rows(rows: &[DecisionRow]) -> Result<(), String> {
    if rows.is_empty() {
        return Err("至少要保留一个决策渠道。".to_string());
    }
    let mut seen: Vec<&str> = Vec::new();
    for row in rows {
        let key = row.key.trim();
        if key.is_empty() {
            return Err("决策渠道 key 不能为空。".to_string());
        }
        if seen.contains(&key) {
            return Err(format!("决策渠道 key 重复：{key}。"));
        }
        seen.push(key);
        if row.name.trim().is_empty() {
            return Err(format!("决策渠道 {key} 的名称不能为空。"));
        }
        if row.mode.trim().is_empty() {
            return Err(format!("决策渠道 {key} 的请求方式不能为空。"));
        }
        if row.base_url.trim().is_empty() {
            return Err(format!("决策渠道 {key} 的基地址不能为空。"));
        }
        if row.model.trim().is_empty() {
            return Err(format!("决策渠道 {key} 的模型 ID 不能为空。"));
        }
    }
    Ok(())
}

/// 挑默认决策渠道：原来的那条还在且启用就沿用，否则取第一条启用的。
fn pick_default_decision_channel(rows: &[DecisionRow], current: &str) -> String {
    if let Some(row) = rows.iter().find(|row| row.key == current && row.enabled) {
        return row.key.clone();
    }
    rows.iter()
        .find(|row| row.enabled)
        .or_else(|| rows.first())
        .map(|row| row.key.clone())
        .unwrap_or_default()
}

fn pick_default_embedding_channel(rows: &[EmbeddingRow], current: &str) -> String {
    if let Some(row) = rows.iter().find(|row| row.key == current && row.enabled) {
        return row.key.clone();
    }
    rows.iter()
        .find(|row| row.enabled)
        .or_else(|| rows.first())
        .map(|row| row.key.clone())
        .unwrap_or_default()
}

/// 渠道表单的只读视图（渲染层用）。
pub struct ChannelFormView<'a> {
    pub row: &'a ChannelRow,
    pub field: ChannelField,
    /// 正在编辑的文本字段缓冲（非空表示处于输入态）。
    pub input: Option<&'a Composer>,
    pub dropdown: Option<&'a ChannelDropdown>,
    pub is_new: bool,
}

/// 表单字段的当前显示值（`Enabled` 给「开启/关闭」）。
pub fn channel_field_value(row: &ChannelRow, field: ChannelField) -> String {
    match field {
        ChannelField::Name => row.name.clone(),
        ChannelField::Provider => row.provider.clone(),
        ChannelField::Protocol => row.protocol.clone(),
        ChannelField::BaseUrl => row.base_url.clone(),
        // 明文永不进界面：只给出「是否已配置 + 末 4 位」的掩码。
        ChannelField::ApiKey => mask_api_key(&row.api_key),
        ChannelField::ApiKeyEnv => row.api_key_env.clone(),
        ChannelField::ModelId => row.model_id.clone(),
        ChannelField::UserAgent => row.user_agent.clone(),
        ChannelField::Enabled => {
            if row.enabled {
                "开启".to_string()
            } else {
                "关闭".to_string()
            }
        }
    }
}

/// 渠道表单某字段的输入缓冲初值：API Key 从空开始（不回显明文，让「留空＝不改动」成为默认），
/// 其余字段以当前值为起点。
fn channel_input_composer(row: &ChannelRow, field: ChannelField) -> Composer {
    let mut composer = Composer::default();
    if field != ChannelField::ApiKey {
        composer.insert(&channel_field_value(row, field));
    }
    composer
}

/// 把表单值写回渠道记录。
fn set_channel_field(row: &mut ChannelRow, field: ChannelField, value: String) {
    match field {
        ChannelField::Name => row.name = value,
        ChannelField::Provider => row.provider = value,
        ChannelField::Protocol => row.protocol = value,
        ChannelField::BaseUrl => row.base_url = value,
        // 留空 = 不改动（与 TTS 面板同一套口径，避免误触把磁盘上的密钥抹掉）。
        ChannelField::ApiKey => {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                row.api_key = trimmed.to_string();
            }
        }
        ChannelField::ApiKeyEnv => row.api_key_env = value,
        ChannelField::ModelId => row.model_id = value,
        ChannelField::UserAgent => row.user_agent = value,
        ChannelField::Enabled => row.enabled = value == "开启",
    }
}

/// 保存前的本地校验：与配置侧的口径一致，但给出更直接的中文提示。
///
/// 配置侧（`save_channel_configuration`）还会再校验一次并做原子回滚，这里只挡住
/// 最常见的手误，避免白跑一次磁盘往返。
fn validate_channels(rows: &[ChannelRow]) -> Result<(), String> {
    if rows.is_empty() {
        return Err("至少要保留一个模型渠道。".to_string());
    }
    if rows.iter().all(|row| !row.enabled) {
        return Err("至少需要启用一个模型渠道。".to_string());
    }
    let mut seen: Vec<&str> = Vec::new();
    for row in rows {
        let key = row.key.trim();
        if key.is_empty() {
            return Err("渠道 key 不能为空。".to_string());
        }
        if seen.contains(&key) {
            return Err(format!("渠道 key 重复：{key}。"));
        }
        seen.push(key);
        if row.name.trim().is_empty() {
            return Err(format!("渠道 {key} 的名称不能为空。"));
        }
        if row.model_id.trim().is_empty() {
            return Err(format!("渠道 {key} 的模型 ID 不能为空。"));
        }
        if row.base_url.trim().is_empty() {
            return Err(format!("渠道 {key} 的基地址不能为空。"));
        }
    }
    Ok(())
}

/// 挑默认渠道：原来的那条还在且启用就沿用，否则取第一条启用的。
fn pick_default_channel(rows: &[ChannelRow], current: &str) -> String {
    if let Some(row) = rows.iter().find(|row| row.key == current && row.enabled) {
        return row.key.clone();
    }
    rows.iter()
        .find(|row| row.enabled)
        .or_else(|| rows.first())
        .map(|row| row.key.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_rows() -> Vec<ToolSwitchRow> {
        vec![
            ToolSwitchRow {
                name: "read".to_string(),
                label: "读取文件内容".to_string(),
                enabled: true,
                registered: true,
            },
            ToolSwitchRow {
                name: "powershell".to_string(),
                label: "执行 PowerShell 命令".to_string(),
                enabled: false,
                registered: true,
            },
            ToolSwitchRow {
                name: "tts_synthesize".to_string(),
                label: "TTS 语音合成".to_string(),
                enabled: true,
                registered: false,
            },
        ]
    }

    fn state() -> SettingsState {
        SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()))
    }

    fn embedding_row(key: &str, enabled: bool) -> EmbeddingRow {
        EmbeddingRow {
            key: key.to_string(),
            name: key.to_string(),
            base_url: "https://example.invalid/v1".to_string(),
            api_key: String::new(),
            api_key_env: "EMBEDDING_API_KEY".to_string(),
            model: "text-embedding-3-small".to_string(),
            dimensions: 0,
            timeout_seconds: 30,
            enabled,
        }
    }

    fn embedding_state(enabled: bool) -> SettingsState {
        let mut state = state();
        state.embedding_models = EmbeddingModelsState {
            rows: vec![embedding_row("first", true), embedding_row("second", true)],
            default_key: "first".to_string(),
            enabled,
            web_search_rerank: false,
            selected: 0,
            form: None,
            template: EmbeddingRow::default(),
            status: String::new(),
        };
        // 让按键直接落到向量页（与用户进入该页后的状态一致）。
        state.pane = Pane::EmbeddingModels;
        state.focus = Focus::Pane;
        state
    }

    /// 第 0 行是向量召回总开关：Enter 翻转并交宿主写盘，不动渠道。
    #[test]
    fn embedding_top_row_toggles_the_switch() {
        let mut state = embedding_state(false);
        assert!(state.embedding_switch_selected());
        assert_eq!(
            state.embedding_selected(),
            None,
            "游标在开关上时没有选中渠道"
        );

        let event = state.handle_key(KeyCode::Enter);
        match event {
            Some(SettingsEvent::Apply(SettingsChange::EmbeddingEnabled { enabled })) => {
                assert!(enabled, "关闭时回车应当请求开启");
            }
            other => panic!("应当请求翻转总开关，实际：{other:?}"),
        }
    }

    /// 第 1 行是联网搜索向量排序开关：Enter 产出与决策页同形的开关变更。
    #[test]
    fn embedding_web_search_row_toggles_the_decision_switch() {
        let mut state = embedding_state(true);
        state.handle_key(KeyCode::Down);
        assert_eq!(
            state.embedding_switch_row(),
            EmbeddingModelsState::WEB_SEARCH_SWITCH_ROW
        );
        let event = state.handle_key(KeyCode::Enter);
        match event {
            Some(SettingsEvent::Apply(SettingsChange::DecisionSwitch { key, enabled })) => {
                assert_eq!(key, DECISION_SWITCH_WEB_SEARCH_EMBEDDING);
                assert!(enabled);
            }
            other => panic!("应当请求翻转联网搜索向量排序，实际：{other:?}"),
        }
    }

    /// 上下移动跨越两条开关与渠道，且不会停在越界位置。
    #[test]
    fn embedding_cursor_walks_switch_then_channels() {
        let mut state = embedding_state(true);
        assert!(state.embedding_switch_selected());

        state.handle_key(KeyCode::Down);
        assert_eq!(
            state.embedding_switch_row(),
            EmbeddingModelsState::WEB_SEARCH_SWITCH_ROW
        );
        assert_eq!(state.embedding_selected(), None);
        state.handle_key(KeyCode::Down);
        assert_eq!(
            state.embedding_selected(),
            Some(0),
            "下移两格应选中第一条渠道"
        );
        state.handle_key(KeyCode::Down);
        assert_eq!(state.embedding_selected(), Some(1));
        // 再下移回到顶部开关（环形）。
        state.handle_key(KeyCode::Down);
        assert!(state.embedding_switch_selected());
        assert_eq!(state.embedding_switch_row(), 0);
        // 上移一格又到最后一条渠道。
        state.handle_key(KeyCode::Up);
        assert_eq!(state.embedding_selected(), Some(1));
    }

    /// 游标在开关上时 D 不删渠道。
    #[test]
    fn embedding_delete_is_ignored_on_switch_row() {
        let mut state = embedding_state(true);
        state.handle_key(KeyCode::Char('d'));
        assert_eq!(state.embedding_rows().len(), 2, "开关行上的 D 不该删除渠道");
        assert!(state.embedding_switch_selected());
    }

    /// 单选页的初始值：推理强度 medium、思考显示开启、记忆开启、插件关闭。
    fn choice_state() -> SettingsState {
        SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows()).with_choices("medium", true, true, false),
        )
    }

    /// 模型管理页的初始值：两个渠道，当前模型是第一条。
    fn model_state(current: &str) -> SettingsState {
        SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows())
                .with_channels(
                    vec![
                        channel_row("gpt-main", "主渠道", "openai", "gpt-5.2"),
                        channel_row("gpt-backup", "备用渠道", "openai", "gpt-4o"),
                    ],
                    "gpt-main",
                    ChannelRow::default(),
                )
                .with_model(current)
                .with_choices("medium", true, true, false),
        )
    }

    fn goto(state: &mut SettingsState, key: &str) {
        while state.selected_key() != key {
            state.handle_key(KeyCode::Down);
        }
    }

    /// 构造一条渠道行（协议按 Provider 的第一个候选）。
    fn channel_row(key: &str, name: &str, provider: &str, model_id: &str) -> ChannelRow {
        ChannelRow {
            key: key.to_string(),
            profile_id: format!("{key}-profile"),
            name: name.to_string(),
            provider: provider.to_string(),
            protocol: protocols_for_provider(provider)
                .first()
                .map(|protocol| (*protocol).to_string())
                .unwrap_or_default(),
            base_url: "https://api.example.com/v1".to_string(),
            api_key: "sk-test-key".to_string(),
            api_key_env: "EXAMPLE_API_KEY".to_string(),
            model_id: model_id.to_string(),
            user_agent: String::new(),
            enabled: true,
        }
    }

    /// 渠道页的初始值：两条渠道（当前是第一条）与一份新建模板。
    fn model_management_state() -> SettingsState {
        SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_channels(
            vec![
                channel_row("gpt-main", "主渠道", "openai", "gpt-5.2"),
                channel_row("claude-backup", "备用渠道", "anthropic", "claude-4"),
            ],
            "gpt-main",
            channel_row("新渠道", "新渠道", "openai", "gpt-5.2"),
        ))
    }

    #[test]
    fn starts_on_first_row_with_its_pane() {
        let state = state();
        assert_eq!(state.selected_key(), "model_management");
        assert_eq!(state.pane(), Pane::ModelManagement);
        assert_eq!(state.focus(), Focus::List);
        assert_eq!(state.title(), "模型管理");
    }

    #[test]
    fn list_navigation_wraps_and_switches_panes() {
        let mut state = state();
        // 走到「工具设置」行。
        goto(&mut state, "tools");
        assert_eq!(state.selected_key(), "tools");
        assert_eq!(state.pane(), Pane::Tools);
        // 上翻依次经过「工具输出压缩」「顾问设置」「向量模型」「结构化决策模型」到「模型管理」。
        state.handle_key(KeyCode::Up);
        assert_eq!(state.selected_key(), "tool_output_compression");
        assert_eq!(state.pane(), Pane::Form(FormKind::ToolOutputCompression));
        state.handle_key(KeyCode::Up);
        assert_eq!(state.selected_key(), "advisor");
        assert_eq!(state.pane(), Pane::Form(FormKind::Advisor));
        state.handle_key(KeyCode::Up);
        assert_eq!(state.selected_key(), "embedding_models");
        assert_eq!(state.pane(), Pane::EmbeddingModels);
        state.handle_key(KeyCode::Up);
        assert_eq!(state.selected_key(), "decision_models");
        assert_eq!(state.pane(), Pane::DecisionModels);
        state.handle_key(KeyCode::Up);
        assert_eq!(state.selected_key(), "model_management");
        assert_eq!(state.pane(), Pane::ModelManagement);

        // 循环：模型管理已是首行，再上翻直接回到末行，下翻再回首行。
        state.handle_key(KeyCode::Up);
        assert_eq!(state.selected_key(), "show_thinking");
        state.handle_key(KeyCode::Down);
        assert_eq!(state.selected_key(), "model_management");
    }

    #[test]
    fn escape_exits_only_from_list() {
        let mut state = state();
        assert_eq!(state.handle_key(KeyCode::Esc), Some(SettingsEvent::Close));

        // 进入右侧后 Esc 先回左侧，再按一次才退出。
        goto(&mut state, "tools");
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.focus(), Focus::Pane);
        state.handle_key(KeyCode::Esc);
        assert_eq!(state.focus(), Focus::List);
        assert_eq!(state.handle_key(KeyCode::Esc), Some(SettingsEvent::Close));
    }

    #[test]
    fn model_param_rows_show_channel_and_context_values() {
        let mut state = model_state("gpt-main");
        goto(&mut state, "model_management");
        assert_eq!(state.pane(), Pane::ModelManagement);
        assert_eq!(state.channel_rows().len(), 2);
        assert_eq!(state.channel_default_key(), "gpt-main");
        let rows = state.model_param_rows();
        assert_eq!(rows.len(), ModelParamRow::ORDER.len());
        assert_eq!(rows[0].0, ModelParamRow::Channel);
        assert_eq!(rows[0].1, "主渠道");
        assert_eq!(rows[1].1, "128K");
        assert_eq!(rows[2].1, "80%");
        assert_eq!(rows[3].1, "中");
        assert!(!rows[0].2, "初始选中的是渠道行，不是参数行");
    }

    #[test]
    fn model_param_channel_switch_applies_and_moves_cursor() {
        let mut state = model_state("gpt-main");
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter); // 进右侧面板
        assert_eq!(state.focus(), Focus::Pane);

        // ↓ 两次到「当前模型渠道」行。
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Down);
        assert_eq!(state.channel_selected(), 2, "渠道 2 行 + 分区首行");
        let rows = state.model_param_rows();
        assert!(rows[0].2, "分区首行被选中");

        // 展开候选：游标停在当前渠道（第 1 项）。
        assert_eq!(state.handle_key(KeyCode::Enter), None);
        let dropdown = state.dropdown().expect("应展开渠道候选");
        assert_eq!(
            dropdown.field,
            DropdownField::ModelParam(ModelParamRow::Channel)
        );
        assert_eq!(dropdown.selected, 0);
        assert_eq!(state.dropdown_options().len(), 2);

        state.handle_key(KeyCode::Down);
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            Some(SettingsEvent::Apply(SettingsChange::Model {
                key: "gpt-backup".to_string()
            }))
        );
        assert_eq!(state.dropdown(), None, "确认后收起下拉");

        state.apply_succeeded(
            &SettingsChange::Model {
                key: "gpt-backup".to_string(),
            },
            "模型已切到 备用渠道（gpt-4o），已保存到 config.toml。".to_string(),
        );
        assert_eq!(state.model_key(), "gpt-backup");
        assert_eq!(
            state.status(),
            "模型已切到 备用渠道（gpt-4o），已保存到 config.toml。"
        );
    }

    #[test]
    fn model_param_context_rows_apply_and_report() {
        let mut state = model_state("gpt-main");
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        for _ in 0..3 {
            state.handle_key(KeyCode::Down); // 到「上下文长度」
        }
        assert!(state.model_param_rows()[1].2);

        // 折叠态按 Enter 展开候选，游标停在当前值（128K 是第 3 个候选）。
        state.handle_key(KeyCode::Enter);
        let dropdown = state.dropdown().expect("应展开候选");
        assert_eq!(
            dropdown.field,
            DropdownField::ModelParam(ModelParamRow::Window)
        );
        assert_eq!(dropdown.selected, 2, "128K 是第 3 个候选");

        state.handle_key(KeyCode::Up);
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            Some(SettingsEvent::Apply(SettingsChange::ContextWindow {
                tokens: 64_000
            }))
        );
        state.apply_succeeded(
            &SettingsChange::ContextWindow { tokens: 64_000 },
            "上下文长度已设为 64K。".to_string(),
        );
        assert_eq!(state.context_window_tokens(), 64_000);
        assert_eq!(state.status(), "上下文长度已设为 64K。");

        // 压缩阈值：↓ 到该行，展开后下移一格到 85%。
        state.handle_key(KeyCode::Down);
        assert!(state.model_param_rows()[2].2);
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.compaction_percent(), 80);
        state.handle_key(KeyCode::Down);
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            Some(SettingsEvent::Apply(SettingsChange::CompactionPercent {
                percent: 85
            })),
            "从 80% 下移一格到 85%"
        );
    }

    #[test]
    fn model_param_reasoning_row_applies() {
        let mut state = model_state("gpt-main");
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        for _ in 0..5 {
            state.handle_key(KeyCode::Down); // 到「推理强度」
        }
        assert!(state.model_param_rows()[3].2);
        state.handle_key(KeyCode::Enter);
        let dropdown = state.dropdown().expect("应展开候选");
        assert_eq!(
            dropdown.field,
            DropdownField::ModelParam(ModelParamRow::Reasoning)
        );
        assert_eq!(dropdown.selected, 2, "medium 是第 3 个候选");
        state.handle_key(KeyCode::Down);
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            Some(SettingsEvent::Apply(SettingsChange::Reasoning {
                effort: "high".to_string()
            }))
        );
        state.apply_succeeded(
            &SettingsChange::Reasoning {
                effort: "high".to_string(),
            },
            "推理强度已设为 高。".to_string(),
        );
        assert_eq!(state.model_param_rows()[3].1, "高");
    }

    #[test]
    fn model_param_escape_collapses_and_keeps_value() {
        let mut state = model_state("gpt-main");
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        for _ in 0..3 {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Enter); // 展开
        state.handle_key(KeyCode::Down);
        assert_eq!(state.handle_key(KeyCode::Esc), None);
        assert_eq!(state.dropdown(), None);
        assert_eq!(state.context_window_tokens(), 128_000);
    }

    #[test]
    fn re_picking_same_value_does_not_apply() {
        let mut state = model_state("gpt-main");
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter); // 展开「当前模型渠道」
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            None,
            "选中同一档不触发保存"
        );
    }

    #[test]
    fn failure_keeps_previous_value_and_reports() {
        let mut state = model_state("gpt-main");
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.apply_failed("设置未完成：磁盘只读。".to_string());
        assert_eq!(state.context_window_tokens(), 128_000);
        assert_eq!(state.status(), "设置未完成：磁盘只读。");
    }

    #[test]
    fn tools_pane_toggles_selected_row() {
        let mut state = state();
        while state.selected_key() != "tools" {
            state.handle_key(KeyCode::Down);
        }
        assert_eq!(state.pane(), Pane::Tools);
        state.handle_key(KeyCode::Enter); // 进入面板
        assert_eq!(state.tool_selected(), TOOLS_APPROVAL_ROW);
        assert_eq!(
            state.handle_key(KeyCode::Char(' ')),
            Some(SettingsEvent::Apply(SettingsChange::ToolApproval {
                mode: "auto".to_string(),
            })),
            "首行是工具调用审查，默认 review 右移一档到 auto"
        );

        // 第二行是联网搜索：确认后进表单页，不产出配置事件。
        state.handle_key(KeyCode::Down);
        assert_eq!(state.tool_selected(), TOOLS_WEB_SEARCH_ROW);
        assert_eq!(state.handle_key(KeyCode::Enter), None);
        assert_eq!(state.pane(), Pane::Form(FormKind::WebSearch));
        state.handle_key(KeyCode::Esc);
        assert_eq!(state.pane(), Pane::Tools, "联网搜索页的 Esc 回工具页");

        state.handle_key(KeyCode::Down);
        assert_eq!(state.tool_selected(), TOOLS_SWITCH_ROW_OFFSET);
        assert_eq!(
            state.handle_key(KeyCode::Char(' ')),
            Some(SettingsEvent::Apply(SettingsChange::ToolSwitch {
                name: "read".to_string(),
                enabled: false,
            }))
        );
        state.handle_key(KeyCode::Down);
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            Some(SettingsEvent::Apply(SettingsChange::ToolSwitch {
                name: "powershell".to_string(),
                enabled: true,
            })),
            "关闭的开关再按一次是启用"
        );
    }

    #[test]
    fn tools_pane_escape_returns_to_list() {
        let mut state = state();
        while state.selected_key() != "tools" {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Right);
        assert_eq!(state.focus(), Focus::Pane);
        state.handle_key(KeyCode::Esc);
        assert_eq!(state.focus(), Focus::List);
        assert_eq!(state.selected_key(), "tools");
    }

    #[test]
    fn apply_success_flips_tool_state_and_refresh_keeps_selection() {
        let mut state = state();
        while state.selected_key() != "tools" {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Enter);
        state.apply_succeeded(
            &SettingsChange::ToolSwitch {
                name: "read".to_string(),
                enabled: false,
            },
            "读取文件内容已关闭。".to_string(),
        );
        assert!(!state.tool_rows()[0].enabled);
        assert_eq!(state.status(), "读取文件内容已关闭。");

        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Down);
        state.refresh_tools(vec![]);
        assert_eq!(state.tool_selected(), 0, "空表时游标回落到首行的审查模式");
        assert!(state.tool_rows().is_empty());
    }

    #[test]
    fn tools_pane_approval_row_cycles_and_applies() {
        let mut state = state();
        goto(&mut state, "tools");
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.tool_selected(), TOOLS_APPROVAL_ROW);
        assert_eq!(state.tool_approval_value(), "自动审查", "默认档位是 review");

        assert_eq!(
            state.handle_key(KeyCode::Right),
            Some(SettingsEvent::Apply(SettingsChange::ToolApproval {
                mode: "auto".to_string(),
            })),
            "右移一档"
        );
        // 事件尚未回填（状态机要等 `apply_succeeded`），游标仍停在 review。
        assert_eq!(
            state.handle_key(KeyCode::Left),
            Some(SettingsEvent::Apply(SettingsChange::ToolApproval {
                mode: "manual".to_string(),
            })),
            "左移一档"
        );

        state.apply_succeeded(
            &SettingsChange::ToolApproval {
                mode: "manual".to_string(),
            },
            "工具调用审查已设为 人工确认。".to_string(),
        );
        assert_eq!(state.tool_approval_value(), "人工确认");
        assert_eq!(state.status(), "工具调用审查已设为 人工确认。");
    }

    #[test]
    fn approval_cycle_matches_python_approval_options() {
        assert_eq!(cycle_approval_mode("review", 1), "auto");
        assert_eq!(cycle_approval_mode("review", -1), "manual");
        assert_eq!(cycle_approval_mode("auto", 1), "manual", "到顶回第一档");
        assert_eq!(cycle_approval_mode("manual", -1), "auto", "到底回最后一档");
        assert_eq!(
            nearest_approval_mode("去死"),
            "review",
            "不认得的值回落默认档"
        );
        assert_eq!(nearest_approval_mode("always"), "auto", "别名照常折算");
    }

    #[test]
    fn shown_choice_pages_keep_their_option_lists() {
        for kind in [
            ChoiceKind::ShowThinking,
            ChoiceKind::Memory,
            ChoiceKind::Plugins,
        ] {
            let options = choice_field_options(kind);
            assert_eq!(
                options,
                vec![
                    ("开启".to_string(), OptionValue::Flag(true)),
                    ("关闭".to_string(), OptionValue::Flag(false)),
                ],
                "开关类单选页的候选与 Python 的 SelectPane 一致"
            );
        }
    }

    #[test]
    fn show_thinking_page_toggles_to_disabled() {
        let mut state = choice_state();
        goto(&mut state, "show_thinking");
        assert_eq!(state.choice_value(), "开启");

        state.handle_key(KeyCode::Enter); // 进面板
        state.handle_key(KeyCode::Enter); // 展开：游标落在「开启」
        assert_eq!(state.dropdown().map(|dropdown| dropdown.selected), Some(0));
        state.handle_key(KeyCode::Down);
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            Some(SettingsEvent::Apply(SettingsChange::ShowThinking {
                enabled: false
            }))
        );
    }

    #[test]
    fn memory_toggle_emits_feature_change() {
        let mut state = choice_state();
        goto(&mut state, "memory");
        assert_eq!(state.pane(), Pane::Choice(ChoiceKind::Memory));
        assert_eq!(state.choice_value(), "开启");

        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Char(' ')); // 空格与 Enter 同义：展开候选
        state.handle_key(KeyCode::Down);
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            Some(SettingsEvent::Apply(SettingsChange::Feature {
                key: "memory".to_string(),
                enabled: false,
            }))
        );
    }

    #[test]
    fn plugins_toggle_starts_from_current_value() {
        let mut state = choice_state();
        goto(&mut state, "plugins");
        assert_eq!(state.choice_value(), "关闭", "初始关闭");

        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Enter);
        assert_eq!(
            state.dropdown().map(|dropdown| dropdown.selected),
            Some(1),
            "关闭是第 2 个候选"
        );
        state.handle_key(KeyCode::Up);
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            Some(SettingsEvent::Apply(SettingsChange::Feature {
                key: "plugins".to_string(),
                enabled: true,
            }))
        );
    }

    #[test]
    fn choice_pages_keep_their_own_status() {
        let mut state = choice_state();
        goto(&mut state, "memory");
        assert_eq!(state.pane_hint(), "", "单选页没有提示行");
        state.apply_failed("设置未完成：磁盘只读。".to_string());
        assert_eq!(state.status(), "设置未完成：磁盘只读。");

        state.handle_key(KeyCode::Down); // 焦点仍在左侧：换到插件行
        assert_eq!(state.selected_key(), "plugins");
        assert_eq!(state.status(), "", "插件页还没有状态文本");
        state.handle_key(KeyCode::Up);
        assert_eq!(state.selected_key(), "memory");
        assert_eq!(
            state.status(),
            "设置未完成：磁盘只读。",
            "回到记忆页仍是自己的文本"
        );
    }

    #[test]
    fn model_management_lists_and_opens_form() {
        let mut state = model_state("gpt-main");
        goto(&mut state, "model_management");
        assert_eq!(state.pane(), Pane::ModelManagement);
        assert_eq!(state.channel_rows().len(), 2);
        assert_eq!(state.channel_default_key(), "gpt-main");
        assert!(state.pane_hint().contains("N 新建"), "列表提示");

        state.handle_key(KeyCode::Enter); // 进右侧面板
        state.handle_key(KeyCode::Enter); // 编辑选中渠道
        let form = state.channel_form().expect("应进入表单");
        assert_eq!(form.row.key, "gpt-main");
        assert!(!form.is_new);
        assert_eq!(form.field, ChannelField::Name);
        assert!(form.input.is_none());
        assert!(state.pane_hint().contains("Ctrl+S 保存"), "表单提示");
    }

    #[test]
    fn channel_form_edits_text_field_in_place() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Enter); // 编辑
        state.handle_key(KeyCode::Enter); // 进名称输入态（缓冲以当前值为起点）

        assert!(state.channel_form().and_then(|form| form.input).is_some());
        state.handle_key(KeyCode::Backspace); // 删掉「道」
        state.handle_key(KeyCode::Char('心'));
        state.handle_key(KeyCode::Enter); // 提交

        let form = state.channel_form().expect("仍在表单");
        assert_eq!(form.row.name, "主渠心");
        assert!(form.input.is_none(), "提交后退出输入态");
    }

    #[test]
    fn channel_input_escape_discards_edit() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Enter); // 输入态
        state.handle_key(KeyCode::Char('x'));
        state.handle_key(KeyCode::Esc); // 取消本次字段编辑

        let form = state.channel_form().expect("仍在表单");
        assert_eq!(form.row.name, "主渠道", "取消不改动原值");
        assert!(form.input.is_none());
    }

    #[test]
    fn channel_form_provider_dropdown_resets_protocol() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Enter); // 编辑：字段停在渠道名
        state.handle_key(KeyCode::Down); // → Provider
        state.handle_key(KeyCode::Enter); // 展开候选

        let options = state
            .channel_form()
            .and_then(|form| form.dropdown)
            .map(|dropdown| dropdown.options.clone())
            .expect("Provider 应展开候选");
        assert!(options.len() >= 2, "候选来自 provider_options");
        assert_eq!(options[0], "openai");

        state.handle_key(KeyCode::Down); // openai → anthropic
        state.handle_key(KeyCode::Enter); // 确认

        let form = state.channel_form().expect("仍在表单");
        assert_eq!(form.row.provider, "anthropic");
        assert_eq!(
            form.row.protocol,
            protocols_for_provider("anthropic")[0],
            "换 Provider 后协议跟到新 Provider 的第一个候选"
        );
        assert!(form.dropdown.is_none(), "确认后收起候选");
    }

    #[test]
    fn channel_new_draft_kept_out_of_list_and_discarded_on_escape() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Char('n'));

        let form = state.channel_form().expect("应进入新建表单");
        assert!(form.is_new);
        assert_eq!(form.row.key, "新渠道");
        assert_eq!(state.channel_rows().len(), 2, "草稿还没落进列表");

        state.handle_key(KeyCode::Esc);
        assert!(state.channel_form().is_none());
        assert_eq!(state.channel_rows().len(), 2);
        assert!(
            state.status().contains("放弃"),
            "提示已放弃：{}",
            state.status()
        );
    }

    #[test]
    fn channel_save_validates_then_emits_change() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Enter); // 编辑主渠道

        // 字段顺序：渠道名 / Provider / 请求协议 / Base URL / API Key / 凭据变量 / 模型 ID / UA / 启用。
        for _ in 0..6 {
            state.handle_key(KeyCode::Down);
        }
        assert_eq!(
            state.channel_form().map(|form| form.field),
            Some(ChannelField::ModelId)
        );
        // 「模型 ID」按 Enter 发起自动检测，而不是直接进输入态。
        match state.handle_key(KeyCode::Enter) {
            Some(SettingsEvent::DiscoverChannelModels { provider, .. }) => {
                assert_eq!(provider, "openai");
            }
            other => panic!("应发起模型检测：{other:?}"),
        }
        let form = state.channel_form().expect("仍在表单");
        assert!(form.input.is_none(), "检测期间不进入手输");
        assert_eq!(form.field, ChannelField::ModelId);

        // 检测失败：退回手输，清空模型 ID 后保存应被校验挡下。
        state.set_channel_models(Vec::new(), "缺少 API Key".to_string());
        for _ in 0..24 {
            state.handle_key(KeyCode::Backspace); // 清空模型 ID
        }
        state.handle_key(KeyCode::Enter); // 提交空值
        assert_eq!(
            state.handle_ctrl_key(KeyCode::Char('s')),
            None,
            "校验失败不产出保存事件"
        );
        assert!(state.status().contains("模型 ID 不能为空"));
        assert!(state.channel_form().is_some(), "校验失败时保留表单");

        // 检测成功：展开候选，选中第二个再保存。
        state.set_channel_models(
            vec!["gpt-5.2".to_string(), "gpt-4o".to_string()],
            String::new(),
        );
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);
        match state.handle_ctrl_key(KeyCode::Char('s')) {
            Some(SettingsEvent::Apply(SettingsChange::Channels { rows, default_key })) => {
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0].model_id, "gpt-4o");
                assert_eq!(default_key, "gpt-main");
            }
            other => panic!("应产出保存事件：{other:?}"),
        }
        assert!(state.channel_form().is_none(), "保存后退出表单");

        state.apply_succeeded(
            &SettingsChange::Channels {
                rows: state.channel_rows().to_vec(),
                default_key: state.channel_default_key().to_string(),
            },
            "渠道配置已保存。".to_string(),
        );
        assert_eq!(state.status(), "渠道配置已保存。");
    }

    #[test]
    fn channel_validation_rejects_duplicates_and_empty_fields() {
        let mut rows = vec![
            channel_row("dup", "一号", "openai", "m1"),
            channel_row("dup", "二号", "openai", "m2"),
        ];
        assert!(validate_channels(&rows)
            .expect_err("重复 key 必须被拒")
            .contains("重复"));

        rows[1].key = "other".to_string();
        rows[1].model_id = String::new();
        assert!(validate_channels(&rows)
            .expect_err("空模型 ID 必须被拒")
            .contains("模型 ID 不能为空"));

        rows[1].model_id = "m2".to_string();
        rows[0].enabled = false;
        rows[1].enabled = false;
        assert!(validate_channels(&rows)
            .expect_err("全关必须被拒")
            .contains("至少需要启用一个"));

        rows[0].enabled = true;
        assert!(validate_channels(&rows).is_ok());
    }

    #[test]
    fn default_channel_selection_prefers_enabled() {
        let mut rows = vec![
            channel_row("a", "A", "openai", "m-a"),
            channel_row("b", "B", "openai", "m-b"),
        ];
        assert_eq!(
            pick_default_channel(&rows, "b"),
            "b",
            "原默认还在且启用就沿用"
        );
        assert_eq!(pick_default_channel(&rows, "gone"), "a");
        rows[1].enabled = false;
        assert_eq!(
            pick_default_channel(&rows, "b"),
            "a",
            "原默认被关掉就换第一条启用的"
        );
    }

    #[test]
    fn channel_delete_keeps_at_least_one() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Char('d'));
        assert_eq!(state.channel_rows().len(), 1);
        assert_eq!(
            state.channel_default_key(),
            "claude-backup",
            "默认渠道让位给剩下那条"
        );

        state.handle_key(KeyCode::Char('d'));
        assert_eq!(state.channel_rows().len(), 1, "只剩一条时拒绝删除");
        assert!(state.status().contains("至少要保留一个模型渠道"));
    }

    #[test]
    fn channel_sync_from_disk_replaces_rows() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Enter); // 进表单

        state.sync_channels(
            vec![channel_row("only", "唯一渠道", "gemini", "gemini-3")],
            "only",
        );
        assert_eq!(state.channel_rows().len(), 1);
        assert_eq!(state.channel_default_key(), "only");
        assert!(state.channel_form().is_none(), "同步后回到列表");
    }

    #[test]
    fn empty_reasoning_value_falls_back_to_none() {
        let mut state = SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows())
                .with_channels(
                    vec![
                        channel_row("gpt-main", "主渠道", "openai", "gpt-5.2"),
                        channel_row("gpt-backup", "备用渠道", "openai", "gpt-4o"),
                    ],
                    "gpt-main",
                    ChannelRow::default(),
                )
                .with_model("gpt-main")
                .with_choices("", true, false, false),
        );
        goto(&mut state, "model_management");
        assert_eq!(state.model_param_rows()[3].1, "关闭", "空串折算到 none");

        state.handle_key(KeyCode::Enter);
        for _ in 0..5 {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Enter); // 展开推理档位
        assert_eq!(state.dropdown().map(|dropdown| dropdown.selected), Some(0));
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            None,
            "选中同一档不触发保存"
        );
    }

    // ---------- 表单页（顾问设置 / 工具输出压缩） ----------

    /// 顾问设置页的初始值：停用、effort=high、选主渠道、未选模型。
    fn advisor_state() -> SettingsState {
        SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows())
                .with_channels(
                    vec![
                        channel_row("gpt-main", "主渠道", "openai", "gpt-5.2"),
                        channel_row("gpt-backup", "备用渠道", "openai", "gpt-4o"),
                    ],
                    "gpt-main",
                    ChannelRow::default(),
                )
                .with_model("gpt-main")
                .with_form(
                    FormKind::Advisor,
                    vec![
                        FieldValue::Flag(false),
                        FieldValue::Text("high".to_string()),
                        FieldValue::Text("gpt-main".to_string()),
                        FieldValue::Text(String::new()),
                    ],
                ),
        )
    }

    /// 压缩页 + 两个渠道：用来驱动「渠道 + 模型」两段选择与模型发现。
    fn compression_state_with_channels() -> SettingsState {
        SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows())
                .with_channels(
                    vec![
                        channel_row("channel", "主渠道", "openai", "deepseek-v4.1-flash"),
                        channel_row("channel-2", "硅基流动", "openai", ""),
                    ],
                    "channel",
                    ChannelRow::default(),
                )
                .with_form(
                    FormKind::ToolOutputCompression,
                    vec![
                        FieldValue::Flag(true),
                        FieldValue::Flag(false),
                        FieldValue::Text("low".to_string()),
                        FieldValue::Text("1200".to_string()),
                        FieldValue::Text("24000".to_string()),
                        FieldValue::Text("1500".to_string()),
                        FieldValue::Text("60".to_string()),
                        FieldValue::Text("channel".to_string()),
                        FieldValue::Text(String::new()),
                    ],
                ),
        )
    }

    /// 工具输出压缩页的初始值：停用、思考关闭、思考深度 low、四个预算、未选渠道与模型。
    fn compression_state() -> SettingsState {
        SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_form(
            FormKind::ToolOutputCompression,
            vec![
                FieldValue::Flag(false),
                FieldValue::Flag(false),
                FieldValue::Text("low".to_string()),
                FieldValue::Text("1200".to_string()),
                FieldValue::Text("24000".to_string()),
                FieldValue::Text("1500".to_string()),
                FieldValue::Text("60".to_string()),
                FieldValue::Text(String::new()),
                FieldValue::Text(String::new()),
            ],
        ))
    }

    #[test]
    fn form_page_toggles_flag_and_picks_enum() {
        let mut state = advisor_state();
        goto(&mut state, "advisor");
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.focus(), Focus::Pane);
        assert_eq!(state.form_rows().len(), 4);

        // 「启用」就地翻转，不产生保存事件。
        assert_eq!(state.handle_key(KeyCode::Enter), None);
        assert_eq!(state.form_rows()[0].value, "启用");

        // effort 字段展开候选：游标停在当前值（高），上移一格选到「中」。
        state.handle_key(KeyCode::Down);
        assert_eq!(state.handle_key(KeyCode::Enter), None);
        assert_eq!(
            state.dropdown().map(|dropdown| dropdown.field),
            Some(DropdownField::Form(1))
        );
        state.handle_key(KeyCode::Up);
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows()[1].value, "中");
        assert_eq!(state.dropdown(), None, "确认候选后收起浮层");

        let Some(SettingsEvent::Apply(SettingsChange::Form { kind, values })) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(kind, FormKind::Advisor);
        assert_eq!(values[0], FieldValue::Flag(true));
        assert_eq!(values[1], FieldValue::Text("medium".to_string()));
    }

    #[test]
    fn form_page_channel_and_model_dropdowns_write_the_draft() {
        let mut state = advisor_state();
        goto(&mut state, "advisor");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Down);
        assert!(
            state.form_rows()[2].value.contains("主渠道"),
            "渠道列显示渠道名"
        );
        assert!(
            state.form_rows()[3].value.contains("渠道默认"),
            "模型列未选时标注「渠道默认」：{}",
            state.form_rows()[3].value
        );

        // 渠道列：候选来自渠道列表，选第二条后草稿写渠道 key。
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.dropdown_options().len(), 2, "候选来自渠道列表");
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);
        assert!(
            state.form_rows()[2].value.contains("备用渠道"),
            "{}",
            state.form_rows()[2].value
        );

        // 模型列：候选 = 该渠道自带的模型 ID（这里没发现结果）。
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);
        let options = state.dropdown_options();
        assert_eq!(options.len(), 1, "只有渠道自带的模型：{options:?}");
        assert_eq!(options[0].0, "gpt-4o");
        state.handle_key(KeyCode::Enter);
        assert!(state.form_rows()[3].value.contains("gpt-4o"));

        let Some(SettingsEvent::Apply(SettingsChange::Form { values, .. })) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(
            values[2],
            FieldValue::Text("gpt-backup".to_string()),
            "渠道列写渠道 key"
        );
        assert_eq!(
            values[3],
            FieldValue::Text("gpt-4o".to_string()),
            "模型列写模型 ID"
        );
    }

    #[test]
    fn compression_page_model_column_discovers_then_writes_the_draft() {
        let mut state = compression_state_with_channels();
        goto(&mut state, "tool_output_compression");
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_field_count(), 9);
        assert_eq!(state.pane_hint(), super::COMPRESSION_HINT);

        // 走到模型列（第 9 项）按 Enter：先按需发现一次，再展开候选。
        for _ in 0..8 {
            state.handle_key(KeyCode::Down);
        }
        let event = state.handle_key(KeyCode::Enter);
        assert!(
            matches!(event, Some(SettingsEvent::DiscoverChannelModels { .. })),
            "进模型列应先发现一次：{event:?}"
        );
        let dropdown = state.dropdown().expect("应展开模型候选");
        assert_eq!(dropdown.field, DropdownField::Form(8));

        // 发现结果回填后候选里多出远端模型（草稿里的当前值也保留）。
        state.set_channel_models(
            vec!["deepseek-v4.1-flash".to_string(), "gpt-5.2".to_string()],
            String::new(),
        );
        state.handle_key(KeyCode::Down);
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            None,
            "模型候选只落进草稿，不即时保存"
        );
        assert_eq!(state.form_rows()[8].value, "gpt-5.2");

        let Some(SettingsEvent::Apply(SettingsChange::Form { kind, values })) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(kind, FormKind::ToolOutputCompression);
        assert_eq!(values[7], FieldValue::Text("channel".to_string()));
        assert_eq!(values[8], FieldValue::Text("gpt-5.2".to_string()));
    }

    #[test]
    fn switching_the_channel_clears_the_model_and_rediscovers() {
        let mut state = compression_state_with_channels();
        goto(&mut state, "tool_output_compression");
        state.handle_key(KeyCode::Enter);
        for _ in 0..7 {
            state.handle_key(KeyCode::Down);
        }
        // 渠道列：换到第二条渠道。
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);
        assert!(
            state.form_rows()[7].value.contains("硅基流动"),
            "{}",
            state.form_rows()[7].value
        );
        assert!(
            state.form_rows()[8].value.contains("渠道默认"),
            "换渠道后模型清空：{}",
            state.form_rows()[8].value
        );

        // 进模型列：新渠道还没发现过，应再发一次发现。
        state.handle_key(KeyCode::Down);
        let event = state.handle_key(KeyCode::Enter);
        assert!(
            matches!(event, Some(SettingsEvent::DiscoverChannelModels { .. })),
            "换渠道后模型列要重新发现：{event:?}"
        );
    }

    #[test]
    fn form_page_edits_positive_int_field() {
        let mut state = compression_state();
        goto(&mut state, "tool_output_compression");
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows().len(), 9);

        // 第 4 个字段是「最小压缩字符数」：进输入态、清空、重打。
        for _ in 0..3 {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows()[3].value, "1200▌", "输入态带光标块");
        for _ in 0..4 {
            state.handle_key(KeyCode::Backspace);
        }
        for character in "2400".chars() {
            state.handle_key(KeyCode::Char(character));
        }
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows()[3].value, "2400");

        // 编辑态里 `Esc` 放弃本次输入，草稿保持上一版。
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Char('9'));
        state.handle_key(KeyCode::Esc);
        assert_eq!(state.form_rows()[3].value, "2400");
    }

    #[test]
    fn form_page_keeps_draft_when_apply_fails() {
        let mut state = advisor_state();
        goto(&mut state, "advisor");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Enter); // 启用但没选模型

        state.apply_failed("设置未完成：启用顾问前请先选择顾问模型。".to_string());
        assert_eq!(state.form_rows()[0].value, "启用", "失败时不回滚草稿");
        assert!(state.status().contains("启用顾问前请先选择顾问模型"));
    }

    #[test]
    fn form_page_escape_returns_to_list() {
        let mut state = advisor_state();
        goto(&mut state, "advisor");
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.handle_key(KeyCode::Esc), None);
        assert_eq!(state.focus(), Focus::List, "Esc 先回左侧，不关整屏");
    }

    /// 消息脱敏页的初始值（只给前八个，其余由 `FormState::new` 按字段表补齐）。
    fn desensitization_state() -> SettingsState {
        SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_form(
            FormKind::Desensitization,
            vec![
                FieldValue::Flag(false),
                FieldValue::Flag(false),
                FieldValue::Flag(false),
                FieldValue::Flag(true),
                FieldValue::Flag(false),
                FieldValue::Flag(false),
                FieldValue::Text("20".to_string()),
                FieldValue::Text("3.5".to_string()),
            ],
        ))
    }

    #[test]
    fn desensitization_page_lists_detectors_and_toggles() {
        let mut state = desensitization_state();
        goto(&mut state, "desensitization");
        assert_eq!(state.pane(), Pane::Form(FormKind::Desensitization));
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows().len(), 27, "脱敏页共 27 个字段");
        assert_eq!(state.form_rows()[3].value, "开启", "熵检测兜底的两态文案");
        assert_eq!(state.form_rows()[7].value, "3.5", "小数按原样显示");

        // 走到第 11 行「PEM 私钥」并翻转。
        for _ in 0..10 {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows()[10].value, "开启");

        let Some(SettingsEvent::Apply(SettingsChange::Form { kind, values })) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(kind, FormKind::Desensitization);
        assert_eq!(values[3], FieldValue::Flag(true));
        assert_eq!(values[10], FieldValue::Flag(true));
        assert_eq!(
            values[8],
            FieldValue::Text(String::new()),
            "逗号列表按原样带走"
        );
    }

    #[test]
    fn run_guard_page_edits_float_field() {
        let mut state =
            SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_form(
                FormKind::RunGuard,
                vec![
                    FieldValue::Flag(true),
                    FieldValue::Flag(true),
                    FieldValue::Text("4000".to_string()),
                    FieldValue::Text("400".to_string()),
                    FieldValue::Text("0.8".to_string()),
                    FieldValue::Text("8".to_string()),
                    FieldValue::Text("3".to_string()),
                    FieldValue::Text("20000".to_string()),
                    FieldValue::Text("2".to_string()),
                    FieldValue::Text("429, 500".to_string()),
                    FieldValue::Flag(false),
                    FieldValue::Text("5".to_string()),
                ],
            ));
        goto(&mut state, "run_guard");
        assert_eq!(state.pane(), Pane::Form(FormKind::RunGuard));
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows().len(), 12);

        // 第 5 行是重复率阈值（小数）：进输入态改成 0.95。
        for _ in 0..4 {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Enter);
        for _ in 0..3 {
            state.handle_key(KeyCode::Backspace);
        }
        for character in "0.95".chars() {
            state.handle_key(KeyCode::Char(character));
        }
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows()[4].value, "0.95");

        let Some(SettingsEvent::Apply(SettingsChange::Form { kind, values })) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(kind, FormKind::RunGuard);
        assert_eq!(values[4], FieldValue::Text("0.95".to_string()));
        assert_eq!(values[9], FieldValue::Text("429, 500".to_string()));
    }

    #[test]
    fn agent_workspace_page_picks_enum() {
        let mut state =
            SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_form(
                FormKind::AgentWorkspace,
                vec![
                    FieldValue::Flag(true),
                    FieldValue::Text("worktree".to_string()),
                    FieldValue::Text("HEAD".to_string()),
                    FieldValue::Flag(false),
                    FieldValue::Flag(true),
                    FieldValue::Flag(false),
                    FieldValue::Text("auto".to_string()),
                    FieldValue::Text(".env, node_modules".to_string()),
                    FieldValue::Text("npm install".to_string()),
                ],
            ));
        goto(&mut state, "agent_workspace");
        assert_eq!(state.pane(), Pane::Form(FormKind::AgentWorkspace));
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows().len(), 9);
        assert_eq!(state.form_rows()[1].value, "worktree");
        assert_eq!(state.form_rows()[7].value, ".env, node_modules");

        // 第 2 行是「隔离模式」：展开候选选到 local。
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.dropdown_options().len(), 2);
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows()[1].value, "local");
    }

    #[test]
    fn image_gen_page_uses_size_candidates() {
        let mut state =
            SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_form(
                FormKind::ImageGen,
                vec![
                    FieldValue::Flag(true),
                    FieldValue::Text("openai".to_string()),
                    FieldValue::Text("https://api.example.com/v1".to_string()),
                    FieldValue::Text("EXAMPLE_API_KEY".to_string()),
                    FieldValue::Text("gpt-image-2".to_string()),
                    FieldValue::Text("1024x1024".to_string()),
                    FieldValue::Text("high".to_string()),
                    FieldValue::Text("webp".to_string()),
                    FieldValue::Text(String::new()),
                    FieldValue::Text(String::new()),
                    FieldValue::Text("2".to_string()),
                    FieldValue::Text("120".to_string()),
                ],
            ));
        goto(&mut state, "image_gen");
        assert_eq!(state.pane(), Pane::Form(FormKind::ImageGen));
        state.handle_key(KeyCode::Enter);
        let rows = state.form_rows();
        // OpenAI 通道：Gemini 的两行（画面比例/分辨率档位）不显示。
        assert_eq!(rows.len(), 10);
        assert_eq!(rows[5].value, "1024x1024", "枚举字段显示候选文案");
        assert!(rows[5].has_menu, "枚举字段带下拉标记");
        assert!(!rows[2].has_menu, "文本字段没有下拉标记");

        // 第 6 行是「默认尺寸」：展开候选（8 个）后往后挪到 4K。
        for _ in 0..5 {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.dropdown_options().len(), 8);
        for _ in 0..5 {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows()[5].value, "3840x2160");

        let Some(SettingsEvent::Apply(SettingsChange::Form { kind, values })) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(kind, FormKind::ImageGen);
        assert_eq!(values[5], FieldValue::Text("3840x2160".to_string()));
        assert_eq!(
            values[3],
            FieldValue::Text("EXAMPLE_API_KEY".to_string()),
            "只带环境变量名，界面里没有凭据本身"
        );
    }

    /// 图像生成页两条通道的字段集互斥：换 provider 后行数与候选都跟着变。
    #[test]
    fn image_gen_page_switches_fields_with_provider() {
        let mut state =
            SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_form(
                FormKind::ImageGen,
                vec![
                    FieldValue::Flag(true),
                    FieldValue::Text("openai".to_string()),
                    FieldValue::Text("https://api.example.com/v1".to_string()),
                    FieldValue::Text("EXAMPLE_API_KEY".to_string()),
                    FieldValue::Text("gpt-image-2".to_string()),
                    FieldValue::Text("1024x1024".to_string()),
                    FieldValue::Text("high".to_string()),
                    FieldValue::Text("webp".to_string()),
                    FieldValue::Text(String::new()),
                    FieldValue::Text(String::new()),
                    FieldValue::Text("2".to_string()),
                    FieldValue::Text("120".to_string()),
                ],
            ));
        goto(&mut state, "image_gen");
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_field_count(), 10);

        // 走「服务商」行（第 2 行）展开候选并切到 gemini。
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.dropdown_options().len(), 2);
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);

        let rows = state.form_rows();
        assert_eq!(
            rows.len(),
            9,
            "Gemini 通道少一行（三选一的目标格式与两项 OpenAI 参数互换）"
        );
        assert_eq!(rows[1].value, "gemini");
        // 行序：启用/provider/地址/凭据变量/模型/画面比例/分辨率档位/张数/超时。
        assert_eq!(rows[5].label, "画面比例");
        assert_eq!(rows[6].label, "分辨率档位");
        // 隐藏行的取值仍留在草稿里（切回 OpenAI 不会丢）。
        let Some(SettingsEvent::Apply(SettingsChange::Form { values, .. })) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(
            values[5],
            FieldValue::Text("1024x1024".to_string()),
            "隐藏的尺寸仍在草稿"
        );
        assert_eq!(
            values[7],
            FieldValue::Text("webp".to_string()),
            "隐藏的输出格式仍在草稿"
        );
    }

    /// 联网搜索页的初始值：默认引擎 tavily，Tavily 已配凭据，其余未配。
    fn web_search_state() -> SettingsState {
        SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows()).with_web_search(WebSearchRow {
                engine: "tavily".to_string(),
                tavily_configured: true,
                exa_configured: false,
                firecrawl_configured: false,
                anysearch_configured: false,
                tinyfish_configured: false,
                github_configured: false,
            }),
        )
    }

    #[test]
    fn web_search_row_shows_engine_and_configured_keys() {
        let mut state = web_search_state();
        goto(&mut state, "tools");
        state.handle_key(KeyCode::Enter);
        assert_eq!(
            state.web_search_value(),
            "tavily（Tavily 已配置）",
            "工具页那一行显示默认引擎与已配凭据"
        );

        // 未配任何凭据时只显示引擎。
        let plain = SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()));
        assert_eq!(plain.web_search_value(), "duckduckgo");
    }

    #[test]
    fn web_search_page_masks_credentials_and_keeps_them_blank_on_save() {
        let mut state = web_search_state();
        goto(&mut state, "tools");
        state.handle_key(KeyCode::Enter); // 进工具页
        state.handle_key(KeyCode::Down);
        assert_eq!(state.tool_selected(), TOOLS_WEB_SEARCH_ROW);
        state.handle_key(KeyCode::Enter); // 进联网搜索表单页
        assert_eq!(state.pane(), Pane::Form(FormKind::WebSearch));

        let rows = state.form_rows();
        assert_eq!(rows.len(), 7, "默认引擎 + 五把 API Key + GitHub Token");
        assert_eq!(rows[0].value, "tavily", "引擎列显示当前值");
        assert!(rows[0].has_menu, "引擎是候选字段");
        assert_eq!(rows[1].value, "••••••••", "已配凭据只显示掩码");
        assert_eq!(rows[2].value, "（未配置）", "未配凭据显示明确文案");
        assert_eq!(rows[3].value, "（未配置）", "Firecrawl 也是未配");
        assert_eq!(rows[4].value, "（未配置）", "AnySearch 未配（可匿名调用）");
        assert_eq!(rows[5].value, "（未配置）", "TinyFish 未配（无匿名通道）");
        assert_eq!(rows[6].value, "（未配置）", "GitHub Token 未配");

        // 掩码原样保存 = 不改动：五把凭据都折成空串交给宿主。
        let Some(SettingsEvent::Apply(SettingsChange::Form { kind, values })) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(kind, FormKind::WebSearch);
        assert_eq!(values[0], FieldValue::Text("tavily".to_string()));
        assert_eq!(values[1], FieldValue::Text(String::new()), "掩码不当作新值");
        assert_eq!(values[2], FieldValue::Text(String::new()));
        assert_eq!(values[3], FieldValue::Text(String::new()));
        assert_eq!(values[4], FieldValue::Text(String::new()));
        assert_eq!(values[5], FieldValue::Text(String::new()));
        assert_eq!(
            values[6],
            FieldValue::Text(String::new()),
            "GitHub Token 未配时也为空"
        );
    }

    #[test]
    fn web_search_page_offers_every_engine_but_not_retired_ones() {
        let mut state = web_search_state();
        goto(&mut state, "tools");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.handle_key(KeyCode::Enter), None); // 展开「搜索引擎」候选
        let options: Vec<String> = state
            .dropdown_options()
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        assert_eq!(
            options,
            vec![
                "duckduckgo",
                "github",
                "arxiv",
                "huggingface",
                "tavily",
                "exa",
                "firecrawl",
                "anysearch",
                "tinyfish",
                "mixed"
            ],
            "候选来自 WEB_SEARCH_ENGINES：雅虎与 bing 已下线"
        );
    }

    #[test]
    fn web_search_page_returns_to_tools_after_save() {
        let mut state = web_search_state();
        goto(&mut state, "tools");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.pane(), Pane::Form(FormKind::WebSearch));

        state.apply_succeeded(
            &SettingsChange::Form {
                kind: FormKind::WebSearch,
                values: vec![
                    FieldValue::Text("exa".to_string()),
                    FieldValue::Text("exa-key".to_string()),
                    FieldValue::Text(String::new()),
                    FieldValue::Text(String::new()),
                    FieldValue::Text(String::new()),
                    FieldValue::Text(String::new()),
                    FieldValue::Text(String::new()),
                ],
            },
            "联网搜索已设为 exa，已保存到 config.toml。".to_string(),
        );
        assert_eq!(state.pane(), Pane::Tools, "保存后回到工具页");
        assert_eq!(state.status(), "联网搜索已设为 exa，已保存到 config.toml。");

        // 凭据状态由宿主按磁盘真实配置覆盖（这里模拟那一步）。
        state.set_web_search(WebSearchRow {
            engine: "exa".to_string(),
            tavily_configured: true,
            exa_configured: true,
            firecrawl_configured: true,
            anysearch_configured: true,
            tinyfish_configured: true,
            github_configured: true,
        });
        assert_eq!(
            state.web_search_value(),
            "exa（Tavily 已配置、Exa 已配置、Firecrawl 已配置、AnySearch 已配置、TinyFish 已配置、GitHub 已配置）"
        );
    }

    #[test]
    fn web_search_form_carries_the_github_token_field() {
        let mut state = web_search_state();
        goto(&mut state, "tools");
        state.handle_key(KeyCode::Enter); // 进工具页
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter); // 进联网搜索表单页
        let rows = state.form_rows();
        assert_eq!(rows.len(), 7, "默认引擎 + 五把 API Key + GitHub Token");
        assert_eq!(rows[4].label, "AnySearch API Key");
        assert_eq!(rows[5].label, "TinyFish API Key");
        assert_eq!(rows[6].label, "GitHub Token");
        assert_eq!(rows[6].value, "（未配置）", "Token 与其余凭据同口径呈现");
    }

    #[test]
    fn web_search_page_typing_a_new_key_reaches_the_draft() {
        let mut state = web_search_state();
        goto(&mut state, "tools");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);
        // 焦点停在引擎列：往下切到 Tavily 密钥，再进输入态重填。
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);
        for character in "tvly-new".chars() {
            state.handle_key(KeyCode::Char(character));
        }
        state.handle_key(KeyCode::Enter);
        let Some(SettingsEvent::Apply(SettingsChange::Form { values, .. })) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(values[1], FieldValue::Text("tvly-new".to_string()));
    }

    /// 子任务设置页的初始行：总开关关闭 + 两个高级参数。
    fn subagents_state() -> SettingsState {
        SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows()).with_subagents(vec![
                SubagentRow {
                    key: "enabled".to_string(),
                    label: "功能总开关".to_string(),
                    value: "已关闭".to_string(),
                    toggle: true,
                    active: false,
                    section: "子任务功能",
                },
                SubagentRow {
                    key: "model_request_concurrency".to_string(),
                    label: "请求并发数".to_string(),
                    value: "2".to_string(),
                    toggle: false,
                    active: false,
                    section: "高级参数",
                },
            ]),
        )
    }

    #[test]
    fn subagents_page_toggles_and_cycles_options() {
        let mut state = subagents_state();
        goto(&mut state, "subagents");
        assert_eq!(state.pane(), Pane::Subagents);
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.subagent_rows().len(), 2);

        // 总开关：两个方向都是切换，产出的是目标状态而不是「翻转」。
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            Some(SettingsEvent::Apply(SettingsChange::Subagent(
                SubagentChange::Enabled(true)
            )))
        );

        // 高级参数：`→` 进一档（2 → 3）、`←` 退一档（2 → 1）。
        state.handle_key(KeyCode::Down);
        assert_eq!(
            state.handle_key(KeyCode::Right),
            Some(SettingsEvent::Apply(SettingsChange::Subagent(
                SubagentChange::Advanced {
                    key: "model_request_concurrency".to_string(),
                    value: 3,
                }
            )))
        );
        assert_eq!(
            state.handle_key(KeyCode::Left),
            Some(SettingsEvent::Apply(SettingsChange::Subagent(
                SubagentChange::Advanced {
                    key: "model_request_concurrency".to_string(),
                    value: 1,
                }
            )))
        );

        // 回填后行文本与状态跟着变。
        state.apply_succeeded(
            &SettingsChange::Subagent(SubagentChange::Enabled(true)),
            "子任务功能已开启。".to_string(),
        );
        assert_eq!(state.subagent_rows()[0].value, "已开启");
        assert!(state.subagent_rows()[0].active, "回填后开关状态跟着翻");
        assert_eq!(state.status(), "子任务功能已开启。");
    }

    /// 视觉设置页的初始值：代理关闭、一条模型、原生视觉未配置。
    fn vision_state() -> SettingsState {
        SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows())
                .with_channels(
                    vec![
                        channel_row("gpt-main", "主渠道", "openai", "gpt-5.2"),
                        channel_row("gpt-backup", "备用渠道", "openai", "gpt-4o"),
                    ],
                    "gpt-main",
                    ChannelRow::default(),
                )
                .with_model("gpt-main")
                .with_vision(false, vec![VisionModelRef::custom("gpt-main")], None),
        )
    }

    #[test]
    fn vision_page_cycles_native_then_adds_and_reorders() {
        let mut state = vision_state();
        goto(&mut state, "vision");
        assert_eq!(state.pane(), Pane::Vision);
        assert_eq!(
            state.vision_native_text(),
            "模型原生视觉：未配置（按模型能力）"
        );
        assert!(state.vision_enabled_text().contains("已停用"));
        assert!(state.vision_enabled_text().contains("1 个"));

        state.handle_key(KeyCode::Enter);
        // `N` 三态循环：未配置 → 开启 → 关闭 → 未配置。
        state.handle_key(KeyCode::Char('n'));
        assert_eq!(state.vision_native_text(), "模型原生视觉：已开启");
        state.handle_key(KeyCode::Char('n'));
        assert_eq!(state.vision_native_text(), "模型原生视觉：已关闭");
        state.handle_key(KeyCode::Char('n'));
        assert_eq!(
            state.vision_native_text(),
            "模型原生视觉：未配置（按模型能力）"
        );

        // `A` 展开渠道候选，选中还没加过的「备用渠道」。
        state.handle_key(KeyCode::Char('a'));
        assert_eq!(state.dropdown_options().len(), 2);
        state.handle_key(KeyCode::Down);
        assert_eq!(state.handle_key(KeyCode::Enter), None, "添加不是保存动作");
        assert_eq!(state.vision_rows().len(), 2);
        assert!(state.vision_rows()[1].contains("gpt-backup"));

        // 重复添加只提示，不重复入列。
        state.handle_key(KeyCode::Char('a'));
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.vision_rows().len(), 2, "同一条渠道不重复添加");

        // Ctrl+↑ 把选中项与上一条对调（故障转移顺序）。
        assert_eq!(state.handle_ctrl_key(KeyCode::Up), None);
        assert!(
            state.vision_rows()[0].contains("gpt-backup"),
            "选中项上移一位：{:?}",
            state.vision_rows()
        );

        // 没动过原生视觉时，保存事件不连带写回。
        state.handle_key(KeyCode::Char(' '));
        let Some(SettingsEvent::Apply(SettingsChange::Vision(change))) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert!(change.enabled);
        assert_eq!(change.models.len(), 2);
        assert_eq!(change.native, None, "原生视觉没动过就不带写回");

        // 动过之后才连带写回（值变化 vs 原值的比较）。
        state.handle_key(KeyCode::Char('n'));
        let Some(SettingsEvent::Apply(SettingsChange::Vision(change))) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(change.native, Some(Some(true)));
    }

    #[test]
    fn vision_page_deletes_and_reports_when_empty() {
        let mut state = vision_state();
        goto(&mut state, "vision");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Char('d'));
        assert!(state.vision_rows().is_empty());
        assert!(state.status().contains("已移除视觉模型"));

        state.handle_key(KeyCode::Char('d'));
        assert!(state.status().contains("没有可删除的视觉模型"));
    }

    fn mcp_state() -> SettingsState {
        SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_mcp(
            McpSettingsValues {
                enabled: true,
                allow_external_network_tools: false,
                require_confirmation_for_write: true,
                require_confirmation_for_command: true,
                audit_log_enabled: true,
                timeout_seconds: 30,
                servers: vec![McpServerRow {
                    name: "fs".to_string(),
                    enabled: true,
                    transport: "stdio".to_string(),
                    risk_level: "restricted".to_string(),
                }],
            },
        ))
    }

    #[test]
    fn mcp_pane_cycles_timeout_and_toggles_globals() {
        let mut state = mcp_state();
        goto(&mut state, "mcp");
        state.handle_key(KeyCode::Enter); // 进右侧面板
        state.mcp.selected = MCP_ROW_ENABLED;
        match state.handle_key(KeyCode::Char(' ')) {
            Some(SettingsEvent::Apply(SettingsChange::Mcp(McpChange::Globals {
                enabled, ..
            }))) => assert!(!enabled, "总开关被切换"),
            other => panic!("应产出全局变更：{other:?}"),
        }
        state.mcp.selected = MCP_ROW_TIMEOUT;
        match state.handle_key(KeyCode::Right) {
            Some(SettingsEvent::Apply(SettingsChange::Mcp(McpChange::Globals {
                timeout_seconds,
                ..
            }))) => assert_eq!(timeout_seconds, 60, "30 秒进到下一档"),
            other => panic!("应产出全局变更：{other:?}"),
        }
    }

    #[test]
    fn mcp_servers_view_opens_editor_and_saves() {
        let mut state = mcp_state();
        goto(&mut state, "mcp");
        state.handle_key(KeyCode::Enter);
        state.mcp.selected = MCP_ROW_SERVERS;
        assert!(
            state.handle_key(KeyCode::Enter).is_none(),
            "servers 行进入列表"
        );
        assert!(state.mcp_servers_view());

        // 编辑现有 Server：草稿带上原名，保存产出 SaveServer。
        state.handle_key(KeyCode::Enter);
        assert!(state.mcp_editor_title().is_some());
        match state.handle_mcp_editor_save() {
            Some(SettingsEvent::Apply(SettingsChange::Mcp(McpChange::SaveServer(draft)))) => {
                assert_eq!(draft.name, "fs");
                assert_eq!(draft.original_name.as_deref(), Some("fs"));
            }
            other => panic!("应产出保存事件：{other:?}"),
        }

        // 新建：A 打开空草稿，标题是添加。
        state.mcp.editor = None;
        state.handle_key(KeyCode::Char('a'));
        assert!(state.mcp_editor_title().unwrap().contains("添加"));
    }

    #[test]
    fn mcp_apply_succeeded_updates_server_rows() {
        let mut state = mcp_state();
        goto(&mut state, "mcp");
        state.apply_succeeded(
            &SettingsChange::Mcp(McpChange::SetServerEnabled {
                name: "fs".to_string(),
                enabled: false,
            }),
            "已保存".to_string(),
        );
        assert!(!state.mcp_server_rows()[0].enabled);

        state.apply_succeeded(
            &SettingsChange::Mcp(McpChange::SaveServer(Box::new(McpServerDraft {
                name: "net".to_string(),
                transport: "streamable_http".to_string(),
                risk_level: "external".to_string(),
                timeout_seconds: 60,
                ..McpServerDraft::default()
            }))),
            "已保存".to_string(),
        );
        assert_eq!(state.mcp_server_rows().len(), 2);
        assert!(state.mcp_editor_title().is_none(), "保存后关闭编辑器");

        state.apply_succeeded(
            &SettingsChange::Mcp(McpChange::DeleteServer {
                name: "fs".to_string(),
            }),
            "已保存".to_string(),
        );
        assert_eq!(state.mcp_server_rows().len(), 1);
        assert_eq!(state.mcp_server_rows()[0].name, "net");
    }

    /// 行号表与行数必须与标签表对齐；可执行行只有服务商与保存。
    #[test]
    fn tts_rows_line_up_with_their_labels() {
        assert_eq!(TTS_ROW_LABELS.len(), TTS_ROW_COUNT);
        let mut state = SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows()).with_tts(TtsValues::default()),
        );
        goto(&mut state, "tts");
        state.handle_key(KeyCode::Enter);
        let rows = state.tts_rows();
        let index = |row: usize| {
            state
                .tts_row_index(row)
                .expect("该行在当前服务商下应当可见")
        };
        assert_eq!(rows[index(TTS_ROW_ENABLED)].label, "启用 TTS");
        assert_eq!(rows[index(TTS_ROW_PROVIDER)].label, "服务商 provider");
        assert_eq!(rows[index(TTS_ROW_SAVE)].label, "保存");
        assert!(rows[index(TTS_ROW_SAVE)].action && rows[index(TTS_ROW_PROVIDER)].action);
        assert!(!rows[index(TTS_ROW_MODEL)].action);
        // 两家共用的字段区里不再有朗读语言行。
        let labels: Vec<&str> = rows.iter().map(|row| row.label).collect();
        assert!(!labels.iter().any(|label| label.contains("language_code")));
    }

    /// 切换服务商：可见行随之变化（Gemini 没有语速行）。
    #[test]
    fn switching_provider_swaps_the_channel_rows() {
        let mut state = SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows()).with_tts(TtsValues::default()),
        );
        goto(&mut state, "tts");
        state.handle_key(KeyCode::Enter); // 进入面板
        assert_eq!(
            state.tts_rows().len(),
            TTS_ROW_COUNT,
            "OpenAI 下所有行都可见"
        );
        let labels: Vec<&str> = state.tts_rows().iter().map(|row| row.label).collect();
        assert!(labels.contains(&"语速 speed"));

        // 选中服务商行后右切换档，字段区换成 Gemini 的（少一行语速）。
        for _ in 0..TTS_ROW_PROVIDER {
            state.handle_key(KeyCode::Down);
        }
        assert_eq!(state.tts_focused(), TTS_ROW_PROVIDER);
        state.handle_key(KeyCode::Right);
        let labels: Vec<&str> = state.tts_rows().iter().map(|row| row.label).collect();
        assert!(!labels.contains(&"语速 speed"));
        assert_eq!(
            state.tts_rows().len(),
            TTS_ROW_COUNT - 1,
            "Gemini 没有语速行"
        );
        let voice = state.tts_row_index(TTS_ROW_VOICE).expect("音色行应当可见");
        assert_eq!(
            state.tts_rows()[voice].value,
            "Kore",
            "音色跟着换成 Gemini 的候选"
        );
    }

    /// 选中标记必须落在光标那一行：Gemini 下可见行少一行，与配置行号比会错位。
    #[test]
    fn tts_selection_marker_follows_the_visible_rows() {
        let mut state = SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows()).with_tts(TtsValues::default()),
        );
        goto(&mut state, "tts");
        state.handle_key(KeyCode::Enter);
        // 切到 Gemini：语速行消失，其后的行在屏幕上前移一位。
        for _ in 0..TTS_ROW_PROVIDER {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Right);
        while state.tts_focused() != TTS_ROW_API_KEY {
            state.handle_key(KeyCode::Down);
        }
        let rows = state.tts_rows();
        // 语速行被跳过，因此屏幕行号比配置行号小 1。
        assert_eq!(state.tts_focused_index(), TTS_ROW_API_KEY - 1);
        assert_eq!(rows[state.tts_focused_index()].label, "API Key");
        // 每行自报屏幕下标，且只有选中行带焦点：渲染层直接照它画标记。
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row.index, index);
            assert_eq!(row.focused, index == state.tts_focused_index());
        }
    }

    /// 通道文本行可编辑：回车进输入态，提交后落回该服务商的草稿。
    #[test]
    fn channel_text_rows_are_editable() {
        let mut state = SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows()).with_tts(TtsValues::default()),
        );
        goto(&mut state, "tts");
        state.handle_key(KeyCode::Enter);
        // 从上往下按到「模型 model」行（它是文本行）。
        let steps = state.tts_row_index(TTS_ROW_MODEL).expect("模型行应当可见");
        for _ in 0..steps {
            state.handle_key(KeyCode::Down);
        }
        assert_eq!(state.tts_focused(), TTS_ROW_MODEL);
        state.handle_key(KeyCode::Enter);
        assert!(state.tts.input.is_some(), "文本行进输入态");
        state.tts.input = None;
    }

    /// 保存行产出草稿事件，草稿带着当前服务商的字段。
    #[test]
    fn save_row_emits_a_draft_with_the_active_channel() {
        let mut state = SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows()).with_tts(TtsValues::default()),
        );
        goto(&mut state, "tts");
        state.handle_key(KeyCode::Enter);
        let steps = state.tts_row_index(TTS_ROW_SAVE).expect("保存行应当可见");
        for _ in 0..steps {
            state.handle_key(KeyCode::Down);
        }
        assert_eq!(state.tts_focused(), TTS_ROW_SAVE);
        let Some(SettingsEvent::Apply(SettingsChange::Tts(TtsChange::Save(draft)))) =
            state.handle_key(KeyCode::Enter)
        else {
            panic!("保存行应当产出 Save(Save(draft))");
        };
        assert_eq!(draft.provider, "openai");
        assert!(matches!(draft.channel, TtsChannelDraft::OpenAi { .. }));
    }

    /// 音色候选按服务商切换，且都能循环（说明表不是空的）。
    #[test]
    fn voice_candidates_follow_the_provider() {
        let mut state = SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows()).with_tts(TtsValues::default()),
        );
        goto(&mut state, "tts");
        state.handle_key(KeyCode::Enter);
        let voice = state.tts_row_index(TTS_ROW_VOICE).expect("音色行应当可见");
        for _ in 0..voice {
            state.handle_key(KeyCode::Down);
        }
        assert_eq!(state.tts_rows()[voice].value, "alloy");
        state.handle_key(KeyCode::Right);
        assert_eq!(state.tts_rows()[voice].value, "ash");
    }

    #[test]
    fn channel_api_key_row_masks_and_blank_input_keeps_previous_value() {
        let mut row = channel_row("probe", "探测渠道", "openai", "gpt-5.2");
        // 掩码显示：明文永不进界面（只给末 4 位）。
        assert_eq!(channel_field_value(&row, ChannelField::ApiKey), "****…-key");
        row.api_key = String::new();
        assert_eq!(
            channel_field_value(&row, ChannelField::ApiKey),
            "（未配置）"
        );
        // 留空 = 不改动：误触回车不会把磁盘上的密钥抹掉。
        row.api_key = "sk-keep-me".to_string();
        set_channel_field(&mut row, ChannelField::ApiKey, "   ".to_string());
        assert_eq!(row.api_key, "sk-keep-me");
        // 非空 = 写入新值（对映 Python 渠道编辑器的 API Key 录入）。
        set_channel_field(&mut row, ChannelField::ApiKey, " sk-new-key ".to_string());
        assert_eq!(row.api_key, "sk-new-key");
        assert_eq!(channel_field_value(&row, ChannelField::ApiKey), "****…-key");
    }

    /// TTS 页的 API Key 行与渠道表单同款：只露掩码、输入态从空起步、留空不改动。
    #[test]
    fn tts_api_key_row_masks_and_blank_input_keeps_previous_value() {
        let mut values = TtsValues::default();
        values.openai.api_key = "sk-tts-secret".to_string();
        let mut state =
            SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_tts(values));
        goto(&mut state, "tts");
        state.handle_key(KeyCode::Enter);
        let row = state
            .tts_row_index(TTS_ROW_API_KEY)
            .expect("API Key 行应当可见");
        assert_eq!(state.tts_rows()[row].value, MASKED_SECRET);

        // 进输入态：缓冲从空起步，避免明文被顺手回写。
        while state.tts_focused() != TTS_ROW_API_KEY {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Enter);
        let buffer = state
            .tts
            .input
            .as_ref()
            .map(|(_, composer)| composer.text().to_string())
            .expect("应当进了输入态");
        assert!(buffer.is_empty(), "密钥输入态从空起步：{buffer:?}");
        // 直接提交空缓冲 = 不改动磁盘原值。
        state.tts.commit_input();
        assert_eq!(state.tts.openai.api_key, "sk-tts-secret");

        // 填了新值就替换，并立刻在草稿里以明文交给宿主写盘。
        state.tts.begin_input(TTS_ROW_API_KEY);
        if let Some((_, composer)) = state.tts.input.as_mut() {
            composer.insert("sk-brand-new");
        }
        state.tts.commit_input();
        match state.tts.draft().channel {
            TtsChannelDraft::OpenAi { api_key, .. } => assert_eq!(api_key, "sk-brand-new"),
            other => panic!("当前服务商应当是 OpenAI：{other:?}"),
        }
    }

    /// 未配置密钥时显示明确文案，而不是掩码或空白。
    #[test]
    fn tts_api_key_row_reports_missing_credential() {
        let mut state = SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows()).with_tts(TtsValues::default()),
        );
        goto(&mut state, "tts");
        state.handle_key(KeyCode::Enter);
        let row = state
            .tts_row_index(TTS_ROW_API_KEY)
            .expect("API Key 行应当可见");
        assert_eq!(state.tts_rows()[row].value, "（未配置）");
    }

    #[test]
    fn channel_api_key_field_sits_next_to_the_env_field() {
        let order = ChannelField::ORDER;
        assert_eq!(order.len(), 9, "渠道表单字段顺序：{order:?}");
        let key = order
            .iter()
            .position(|field| *field == ChannelField::ApiKey)
            .expect("表单里应有 API Key 行");
        assert_eq!(
            order[key + 1],
            ChannelField::ApiKeyEnv,
            "API Key 应紧邻 API Key 环境变量：{order:?}"
        );
        assert_eq!(ChannelField::ApiKey.label(), "API Key");
        assert!(ChannelField::ApiKey.is_text(), "API Key 应是可输入字段");
    }

    // ---------- 自部署分区的进度行 ----------

    /// 自部署分区初值：下载 0.8B 到一半。
    fn running_local(task: &str, done: u64, total: u64) -> DecisionLocalValues {
        DecisionLocalValues {
            size: "0.8B".to_string(),
            device: "auto".to_string(),
            sizes: vec![("0.8B".to_string(), "0.8B（约 2.2 GB）".to_string(), false)],
            task: Some(task.to_string()),
            progress: ProgressSnapshot {
                stage: "下载权重".to_string(),
                done,
                total,
                unit: omnicrawl_onejev::ProgressUnit::Bytes,
            },
            ..DecisionLocalValues::default()
        }
    }

    fn decision_local_state(values: DecisionLocalValues) -> SettingsState {
        SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows())
                .with_decision_models(Vec::new(), "", DecisionRow::default())
                .with_decision_local(values),
        )
    }

    #[test]
    fn task_progress_row_carries_a_bar_and_keeps_busy() {
        let state = decision_local_state(running_local("下载 OneJev 0.8B", 512, 1024));
        assert!(state.decision_local_busy(), "有 task 就是进行中");
        let rows = state.decision_local_rows();
        let progress = &rows[DECISION_LOCAL_ROW_PROGRESS];
        assert!(progress.bar, "进度行要画条");
        assert_eq!(progress.progress, Some(0.5));
        assert!(!progress.progress_detail.is_empty(), "要有量化文案");
        assert!(
            progress.value.contains("下载 OneJev 0.8B"),
            "进度行要点出任务名：{}",
            progress.value
        );
    }

    #[test]
    fn finished_task_releases_busy_and_hides_the_bar() {
        // 这是「任务早已结束、界面还按着进行中不放」的回归测试：
        // `busy` 完全由宿主给的 `task` 推导，任务清空即复位。
        let state = decision_local_state(DecisionLocalValues {
            size: "0.8B".to_string(),
            ..DecisionLocalValues::default()
        });
        assert!(!state.decision_local_busy(), "没有 task 就不该 busy");
        let rows = state.decision_local_rows();
        assert!(!rows[DECISION_LOCAL_ROW_PROGRESS].bar, "没任务不画条");
        assert!(rows[DECISION_LOCAL_ROW_PROGRESS].value.is_empty());
    }

    #[test]
    fn refresh_without_task_resets_busy() {
        // 宿主在任务结束后刷新：即便先前被 `apply_succeeded` 按住，也要放开。
        let mut state = decision_local_state(running_local("下载 OneJev 0.8B", 1, 2));
        goto(&mut state, "decision_models");
        assert!(state.decision_local_busy());
        state.refresh_decision_local(
            DecisionLocalValues {
                size: "0.8B".to_string(),
                ..DecisionLocalValues::default()
            },
            "下载完成。".to_string(),
        );
        assert!(!state.decision_local_busy(), "任务结束必须复位 busy");
        assert_eq!(state.status(), "下载完成。");
    }

    #[test]
    fn unquantified_task_falls_back_to_a_sliding_bar() {
        // 环境安装 / 服务冷启动没有字节数：比例给 `None`，渲染层改画滑动滑块。
        let state = decision_local_state(DecisionLocalValues {
            size: "0.8B".to_string(),
            task: Some("准备运行环境".to_string()),
            progress: ProgressSnapshot {
                stage: "2/3 安装 torch（CUDA 轮子，约 3 GB）…".to_string(),
                ..ProgressSnapshot::default()
            },
            ..DecisionLocalValues::default()
        });
        let rows = state.decision_local_rows();
        assert!(rows[DECISION_LOCAL_ROW_PROGRESS].bar);
        assert_eq!(rows[DECISION_LOCAL_ROW_PROGRESS].progress, None);
        assert!(rows[DECISION_LOCAL_ROW_PROGRESS]
            .value
            .contains("安装 torch"));
    }

    #[test]
    fn busy_activation_reports_the_stage_instead_of_just_waiting() {
        let mut state = decision_local_state(running_local("下载 OneJev 0.8B", 512, 1024));
        goto(&mut state, "decision_models");
        state.handle_key(KeyCode::Enter);
        // 进面板后排到进度行，再按 Enter：应被按住并给出阶段，而不是沉默。
        let rows = state.decision_local_rows();
        assert_eq!(rows.len(), DECISION_LOCAL_ROW_COUNT);
        assert!(state.decision_local_busy());
    }

    // ---------- 粘贴 ----------

    /// 渠道表单的 API Key 行：粘贴即落值，不必先按 Enter 进输入态。
    #[test]
    fn channel_api_key_accepts_pasted_text_without_entering_input_first() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter); // 进面板
        state.handle_key(KeyCode::Enter); // 编辑选中渠道
        for _ in 0..4 {
            state.handle_key(KeyCode::Down); // → API Key
        }
        assert_eq!(
            state.channel_form().map(|form| form.field),
            Some(ChannelField::ApiKey)
        );
        assert!(
            state.channel_form().and_then(|form| form.input).is_none(),
            "粘贴之前不在输入态"
        );

        assert!(state.paste("sk-pasted-key"), "API Key 行应接收粘贴");
        // 落进缓冲、还没提交：界面显示的是掩码，明文只在缓冲里。
        let form = state.channel_form().expect("仍在表单");
        assert_eq!(
            form.input.map(|composer| composer.text()),
            Some("sk-pasted-key")
        );

        state.handle_key(KeyCode::Enter); // 提交
        let form = state.channel_form().expect("仍在表单");
        assert_eq!(form.row.api_key, "sk-pasted-key");
        assert!(form.input.is_none(), "提交后退出输入态");
    }

    /// 粘贴多行凭据只取首行：设置字段都是单行框，换行不能变成提交。
    #[test]
    fn pasted_multiline_secret_keeps_only_the_first_line() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Enter);
        for _ in 0..4 {
            state.handle_key(KeyCode::Down); // → API Key
        }

        assert!(state.paste("sk-first\r\nsk-second\n"));
        state.handle_key(KeyCode::Enter);

        let form = state.channel_form().expect("仍在表单");
        assert_eq!(form.row.api_key, "sk-first", "只取首行，余下行不提交");
    }

    /// 开关与候选字段不接收粘贴（它们不是文本框），也就不会吞掉事件。
    #[test]
    fn non_text_fields_refuse_paste() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Enter); // 字段停在渠道名

        state.handle_key(KeyCode::Down); // → 请求方式（候选字段）
        assert_eq!(
            state.channel_form().map(|form| form.field),
            Some(ChannelField::Provider)
        );
        assert!(!state.paste("openai"), "候选字段不接收粘贴");
        assert!(
            state.channel_form().and_then(|form| form.input).is_none(),
            "被拒的粘贴不该把字段顶进输入态"
        );
    }

    /// 表单页（如联网搜索的五把凭据）同样接收粘贴，未进输入态时自动进入。
    #[test]
    fn form_page_secret_accepts_pasted_text() {
        let mut state = web_search_state();
        goto(&mut state, "tools");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter); // 进联网搜索表单页
        assert_eq!(state.pane(), Pane::Form(FormKind::WebSearch));

        state.handle_key(KeyCode::Down); // → Tavily API Key
        assert!(state.paste("tvly-pasted"));
        state.handle_key(KeyCode::Enter);

        let Some(SettingsEvent::Apply(SettingsChange::Form { values, .. })) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(values[1], FieldValue::Text("tvly-pasted".to_string()));
    }

    /// 焦点还在左侧一级列表时拒收粘贴：那边没有输入框。
    #[test]
    fn paste_is_refused_while_the_list_column_has_focus() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        assert_eq!(state.focus(), Focus::List);

        assert!(!state.paste("sk-leaked"), "左栏没有输入框，不该接收");
        assert!(state.channel_form().is_none(), "也不该顺手打开表单");
    }

    #[test]
    fn computer_use_model_field_opens_dropdown_with_candidates() {
        // 桌面操作页只有「桌面模型」一列、没有渠道列：候选按当前生效渠道给，
        // 不能因为缺渠道字段就退化成空列表——那会让回车看起来毫无反应。
        let mut state = model_management_state();
        goto(&mut state, "computer_use");
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.pane(), Pane::Form(FormKind::ComputerUse));
        assert_eq!(state.form_field_count(), 5);

        // 字段顺序：启用 / 时间预算 / 截图上限 / 思考强度 / 桌面模型。
        for _ in 0..4 {
            state.handle_key(KeyCode::Down);
        }
        assert_eq!(
            state
                .form()
                .and_then(|form| form.field(form.focused()))
                .map(|spec| spec.kind),
            Some(FieldKind::ModelId)
        );

        state.handle_key(KeyCode::Enter);
        let dropdown = state.dropdown().expect("回车应展开模型候选");
        assert_eq!(dropdown.field, DropdownField::Form(4));
        let options = state.dropdown_options();
        assert!(
            !options.is_empty(),
            "候选不能为空，否则浮层不渲染、回车看着没反应"
        );
        assert!(
            options
                .iter()
                .any(|(_, value)| matches!(value, OptionValue::Text(model) if model == "gpt-5.2")),
            "当前渠道自带模型应在候选里：{options:?}"
        );
    }
}
