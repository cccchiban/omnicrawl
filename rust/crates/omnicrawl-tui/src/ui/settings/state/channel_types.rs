//! 由 `settings/state.rs` 按页面拆出：行定义与常量，语义未变。

use super::*;

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

