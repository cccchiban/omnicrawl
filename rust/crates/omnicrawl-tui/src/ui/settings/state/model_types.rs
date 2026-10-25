//! 由 `settings/state.rs` 按页面拆出：行定义与常量，语义未变。

use super::*;

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

const MODEL_LIST_HINT: &str = "↑↓ 选择渠道或参数  Enter 编辑或选择  N 新建  D 删除  Esc 返回";

const MODEL_FORM_HINT: &str = "↑↓/Tab 换字段  Enter 编辑或展开候选  Ctrl+S 保存  Esc 返回列表";

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

