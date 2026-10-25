//! 由 `settings/state.rs` 按页面拆出：行定义与常量，语义未变。

use super::*;

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

const EMBEDDING_LIST_HINT: &str = "↑↓ 选择  Enter/→ 编辑渠道或切换开关  N 新建  D 删除  Esc 返回";

const EMBEDDING_FORM_HINT: &str = "↑↓/Tab 换字段  Enter 编辑  Ctrl+S 保存  Esc 返回列表";

