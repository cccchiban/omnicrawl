//! 由 `settings/state.rs` 按页面拆出：行定义与常量，语义未变。

use super::*;

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

const TOOLS_HINT: &str = "↑↓ 选择  ←→/Enter/空格 切换或换档  Esc 返回";

const SUBAGENTS_HINT: &str = "↑↓ 选择  ←→/Enter/空格 切换或换档  Esc 返回";

const FORM_HINT: &str = "↑↓/Tab 换字段  Enter 编辑或展开候选  Ctrl+S 保存  Esc 返回";

/// 工具输出压缩页的提示（模型选择与其余表单页同款，都是两个下拉）。
const COMPRESSION_HINT: &str = "↑↓/Tab 换字段  Enter 展开候选  Ctrl+S 保存  Esc 返回";

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

/// 子任务设置页的界面状态。
#[derive(Debug, Clone)]
struct SubagentsState {
    rows: Vec<SubagentRow>,
    selected: usize,
    status: String,
}

