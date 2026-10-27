//! 表单类设置页：一页若干字段（开关/候选/正整数/模型）+ `Ctrl+S` 保存。
//!
//! 对映 Python 的 `AdvisorSettingsPane`、`ToolOutputCompressionSettingsPane` 这类面板。
//! 表单只持有界面草稿：键位在这里；读盘、写盘、校验与生效都在宿主（`app.rs`），
//! 所以本模块不依赖配置与内核，可以用 `TestBackend` 单独驱动。

use crossterm::event::KeyCode;

use crate::state::Composer;

/// 表单字段的编辑形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// 开关：`Enter`/空格 就地翻转，不写盘。
    Flag,
    /// 离散候选：`Enter`/空格 展开候选（候选表随字段给出）。
    Enum(&'static [(&'static str, &'static str)]),
    /// 正整数：`Enter`/空格 进输入态，合法性由宿主在保存时校验。
    Int,
    /// 小数：编辑方式同整数，合法性由宿主在保存时校验。
    Float,
    /// 自由文本：逗号/分号分隔的列表也归这里，切分由宿主负责。
    Text,
    /// 凭据文本：编辑方式同自由文本，但界面上只显示掩码，进输入态也从空起步
    /// （留空表示不改动磁盘上的原值，避免误触把已存凭据抹掉）。
    Secret,
    /// 模型渠道引用：候选是配置里的渠道列表（文案 = 渠道名，取值 = 渠道 key）。
    ModelChannel,
    /// 渠道内的模型 ID：候选按当前选中的渠道发现，取值 = 模型 ID（可为空，表示用渠道自带的模型）。
    ModelId,
}

/// 字段属于哪条通道：只服务某一条的字段在另一条下不显示（值仍进草稿，切换不丢）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldScope {
    /// 与通道无关，始终显示。
    Always,
    /// 只属于 OpenAI 兼容 Image API。
    OpenAi,
    /// 只属于 Gemini。
    Gemini,
}

/// 开关字段的两态文案（关, 开）。
pub type FlagLabels = (&'static str, &'static str);

const DEFAULT_FLAG_LABELS: FlagLabels = ("停用", "启用");

/// 表单里一个字段的静态规格。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldSpec {
    pub label: &'static str,
    pub kind: FieldKind,
    /// 只有开关字段用得到（其余字段忽略）。
    pub flag_labels: FlagLabels,
    /// 该字段属于哪条通道（见 [`FieldScope`]）。
    pub scope: FieldScope,
}

impl FieldSpec {
    const fn flag(label: &'static str) -> Self {
        Self {
            label,
            kind: FieldKind::Flag,
            flag_labels: DEFAULT_FLAG_LABELS,
            scope: FieldScope::Always,
        }
    }

    const fn toggled(label: &'static str, off: &'static str, on: &'static str) -> Self {
        Self {
            label,
            kind: FieldKind::Flag,
            flag_labels: (off, on),
            scope: FieldScope::Always,
        }
    }

    /// 用「关闭 / 开启」两态的开关（Python 各页里 `[("关闭", False), ("开启", True)]` 那一类）。
    const fn on_off(label: &'static str) -> Self {
        Self::toggled(label, "关闭", "开启")
    }

    /// 沿用「停用 / 启用」两态的开关（Python 里总开关用这一组文案）。
    const fn enabled(label: &'static str) -> Self {
        Self::flag(label)
    }

    const fn enumerated(
        label: &'static str,
        options: &'static [(&'static str, &'static str)],
    ) -> Self {
        Self {
            label,
            kind: FieldKind::Enum(options),
            flag_labels: DEFAULT_FLAG_LABELS,
            scope: FieldScope::Always,
        }
    }

    const fn int(label: &'static str) -> Self {
        Self {
            label,
            kind: FieldKind::Int,
            flag_labels: DEFAULT_FLAG_LABELS,
            scope: FieldScope::Always,
        }
    }

    const fn float(label: &'static str) -> Self {
        Self {
            label,
            kind: FieldKind::Float,
            flag_labels: DEFAULT_FLAG_LABELS,
            scope: FieldScope::Always,
        }
    }

    const fn text(label: &'static str) -> Self {
        Self {
            label,
            kind: FieldKind::Text,
            flag_labels: DEFAULT_FLAG_LABELS,
            scope: FieldScope::Always,
        }
    }

    const fn secret(label: &'static str) -> Self {
        Self {
            label,
            kind: FieldKind::Secret,
            flag_labels: DEFAULT_FLAG_LABELS,
            scope: FieldScope::Always,
        }
    }

    const fn model_channel(label: &'static str) -> Self {
        Self {
            label,
            kind: FieldKind::ModelChannel,
            flag_labels: DEFAULT_FLAG_LABELS,
            scope: FieldScope::Always,
        }
    }

    const fn model_id(label: &'static str) -> Self {
        Self {
            label,
            kind: FieldKind::ModelId,
            flag_labels: DEFAULT_FLAG_LABELS,
            scope: FieldScope::Always,
        }
    }

    /// 把字段限定到某条通道（另一条下不显示）。
    const fn scoped(mut self, scope: FieldScope) -> Self {
        self.scope = scope;
        self
    }

    /// 该字段在当前服务商下是否显示。
    pub fn visible_for(&self, gemini: bool) -> bool {
        match self.scope {
            FieldScope::Always => true,
            FieldScope::OpenAi => !gemini,
            FieldScope::Gemini => gemini,
        }
    }
}

/// 顾问推理档位候选（对映 `ADVISOR_EFFORT_OPTIONS`；文案与推理强度页一致）。
const EFFORT_OPTIONS: [(&str, &str); 6] = [
    ("关闭", "none"),
    ("低", "low"),
    ("中", "medium"),
    ("高", "high"),
    ("超高", "xhigh"),
    ("最大", "max"),
];

/// 压缩思考深度候选（对映 `THINKING_EFFORT_OPTIONS`：关闭思考由开关表达，不含 `none`）。
const THINKING_OPTIONS: [(&str, &str); 5] = [
    ("低", "low"),
    ("中", "medium"),
    ("高", "high"),
    ("超高", "xhigh"),
    ("最大", "max"),
];

/// 顾问设置页的字段；顺序与 `app.rs` 读值时的下标一一对应。
///
/// 「顾问渠道 + 顾问模型」两个下拉与「工具输出压缩」同构：先选渠道，再在该渠道的模型里选。
const ADVISOR_FIELDS: [FieldSpec; 4] = [
    FieldSpec::flag("启用"),
    FieldSpec::enumerated("effort", &EFFORT_OPTIONS),
    FieldSpec::model_channel("顾问渠道"),
    FieldSpec::model_id("顾问模型"),
];

/// Computer Use 页的字段；顺序与 `app.rs` 读值时的下标一一对应。
const COMPUTER_USE_FIELDS: [FieldSpec; 5] = [
    FieldSpec::flag("启用"),
    FieldSpec::int("单次时间预算（秒）"),
    FieldSpec::int("截图长边上限"),
    FieldSpec::enumerated("思考强度", &COMPUTER_USE_THINKING_OPTIONS),
    FieldSpec::model_id("桌面模型"),
];

/// Computer Use 页的思考强度候选：文案与推理强度页一致，取值来自配置域。
const COMPUTER_USE_THINKING_OPTIONS: [(&str, &str); 5] = [
    ("低", "low"),
    ("中", "medium"),
    ("高", "high"),
    ("超高", "xhigh"),
    ("最大", "max"),
];

/// 工具输出压缩页的字段；顺序与 `app.rs` 读值时的下标一一对应。
const COMPRESSION_FIELDS: [FieldSpec; 9] = [
    FieldSpec::flag("启用"),
    FieldSpec::toggled("思考", "关闭", "开启"),
    FieldSpec::enumerated("思考深度", &THINKING_OPTIONS),
    FieldSpec::int("最小压缩字符数"),
    FieldSpec::int("单次压缩输入上限"),
    FieldSpec::int("压缩结果上限"),
    FieldSpec::int("单条压缩超时（秒）"),
    FieldSpec::model_channel("压缩渠道"),
    FieldSpec::model_id("压缩模型"),
];

/// 隔离工作区的模式候选。
const WORKSPACE_MODE_OPTIONS: [(&str, &str); 2] = [("worktree", "worktree"), ("local", "local")];

/// 隔离工作区的退出清理策略候选。
const WORKSPACE_CLEANUP_OPTIONS: [(&str, &str); 3] =
    [("auto", "auto"), ("keep", "keep"), ("never", "never")];

/// 图像生成默认尺寸候选（对映 Python 的 `_SIZES`）。
const IMAGE_GEN_SIZES: [(&str, &str); 8] = [
    ("auto", "auto"),
    ("1024x1024", "1024x1024"),
    ("1536x1024", "1536x1024"),
    ("1024x1536", "1024x1536"),
    ("2048x2048", "2048x2048"),
    ("2048x1152", "2048x1152"),
    ("3840x2160", "3840x2160"),
    ("2160x3840", "2160x3840"),
];

/// 图像生成质量候选（对映 Python 的 `_QUALITIES`）。
const IMAGE_GEN_QUALITIES: [(&str, &str); 4] = [
    ("auto", "auto"),
    ("low", "low"),
    ("medium", "medium"),
    ("high", "high"),
];

/// 图像生成输出格式候选（对映 Python 的 `_FORMATS`）。
const IMAGE_GEN_FORMATS: [(&str, &str); 3] = [("png", "png"), ("jpeg", "jpeg"), ("webp", "webp")];

/// 图像生成服务商候选（取值即配置值，与 `[image_gen].provider` 一一对应）。
const IMAGE_GEN_PROVIDERS: [(&str, &str); 2] = [("openai", "openai"), ("gemini", "gemini")];

/// Gemini 画面比例候选（官方 14 种）；首项是「不下发」。
const IMAGE_GEN_ASPECT_RATIOS: [(&str, &str); 15] = [
    ("（默认）", ""),
    ("1:1", "1:1"),
    ("2:3", "2:3"),
    ("3:2", "3:2"),
    ("3:4", "3:4"),
    ("4:3", "4:3"),
    ("4:5", "4:5"),
    ("5:4", "5:4"),
    ("9:16", "9:16"),
    ("16:9", "16:9"),
    ("21:9", "21:9"),
    ("1:4", "1:4"),
    ("4:1", "4:1"),
    ("1:8", "1:8"),
    ("8:1", "8:1"),
];

/// Gemini 分辨率档位候选；首项是「不下发」。
const IMAGE_GEN_IMAGE_SIZES: [(&str, &str); 4] =
    [("（默认）", ""), ("1K", "1K"), ("2K", "2K"), ("4K", "4K")];

/// 图像生成默认张数候选（对映 Python 的 `_COUNTS`）。
const IMAGE_GEN_COUNTS: [(&str, &str); 4] = [("1", "1"), ("2", "2"), ("3", "3"), ("4", "4")];

/// 图像生成请求超时候选（对映 Python 的 `_TIMEOUTS`，单位秒）。
const IMAGE_GEN_TIMEOUTS: [(&str, &str); 6] = [
    ("30", "30"),
    ("60", "60"),
    ("120", "120"),
    ("180", "180"),
    ("300", "300"),
    ("600", "600"),
];

/// 消息脱敏页的字段；顺序与 `app.rs` 读值时的下标一一对应。
const DESENSITIZATION_FIELDS: [FieldSpec; 21] = [
    FieldSpec::enabled("启用"),
    FieldSpec::toggled(
        "屏蔽异常时中止",
        "关闭（降级发送原文并告警）",
        "开启（不静默发送原文）",
    ),
    FieldSpec::toggled("严格还原", "关闭（保留占位符并告警）", "开启（中止并报错）"),
    FieldSpec::toggled("熵检测兜底", "关闭（仅键名 / 结构匹配）", "开启"),
    FieldSpec::on_off("纯字母令牌脱敏"),
    FieldSpec::on_off("纯数字令牌脱敏"),
    FieldSpec::int("熵兜底长度下限"),
    FieldSpec::float("熵兜底阈值（0–8）"),
    FieldSpec::text("追加敏感键名"),
    FieldSpec::text("豁免键名"),
    FieldSpec::on_off("PEM 私钥"),
    FieldSpec::on_off("数据库连接串"),
    FieldSpec::on_off("邮箱地址"),
    FieldSpec::on_off("银行卡号"),
    FieldSpec::on_off("内网 IP"),
    FieldSpec::on_off("外网 IP"),
    FieldSpec::on_off("网址"),
    FieldSpec::on_off("MAC 地址"),
    FieldSpec::on_off("中国大陆车牌"),
    FieldSpec::on_off("gitleaks 规则"),
    FieldSpec::text("gitleaks 配置路径"),
];

/// 持续运转（运行护栏）页的字段；顺序与 `app.rs` 读值时的下标一一对应。
const RUN_GUARD_FIELDS: [FieldSpec; 12] = [
    FieldSpec::flag("启用"),
    FieldSpec::flag("推理护栏"),
    FieldSpec::int("窗口字符数"),
    FieldSpec::int("重复子串长度"),
    FieldSpec::float("重复率阈值（0～1）"),
    FieldSpec::int("检查间隔（块）"),
    FieldSpec::int("推理块上限"),
    FieldSpec::int("推理字符上限"),
    FieldSpec::int("护栏重试次数"),
    FieldSpec::text("自动重试错误码"),
    FieldSpec::flag("自动续跑"),
    FieldSpec::int("续跑次数上限"),
];

/// 隔离工作区页的字段；顺序与 `app.rs` 读值时的下标一一对应。
const AGENT_WORKSPACE_FIELDS: [FieldSpec; 9] = [
    FieldSpec::flag("启用"),
    FieldSpec::enumerated("隔离模式", &WORKSPACE_MODE_OPTIONS),
    FieldSpec::text("基线引用"),
    FieldSpec::flag("Detached HEAD"),
    FieldSpec::flag("带入未提交变更"),
    FieldSpec::flag("退出时应用变更"),
    FieldSpec::enumerated("退出时清理", &WORKSPACE_CLEANUP_OPTIONS),
    FieldSpec::text("复制的目录"),
    FieldSpec::text("环境脚本"),
];

/// 图像生成页的字段；顺序与 `app.rs` 读值时的下标一一对应。
///
/// `api_key` 不在界面上（凭据不进界面状态）：保存时按磁盘原值继承，
/// 界面只编辑凭据的环境变量名。
///
/// 一个段承载两条通道：`provider` 决定这次出图走哪家，带 `scoped` 的行只在对应通道下显示
/// （值仍在草稿里，切换服务商不会丢掉另一家的取值）。隐藏行不参与导航与焦点。
const IMAGE_GEN_FIELDS: [FieldSpec; 12] = [
    FieldSpec::flag("启用"),
    FieldSpec::enumerated("服务商 provider", &IMAGE_GEN_PROVIDERS),
    FieldSpec::text("接口地址"),
    FieldSpec::text("API Key"),
    FieldSpec::text("模型"),
    FieldSpec::enumerated("默认尺寸", &IMAGE_GEN_SIZES).scoped(FieldScope::OpenAi),
    FieldSpec::enumerated("默认质量", &IMAGE_GEN_QUALITIES).scoped(FieldScope::OpenAi),
    FieldSpec::enumerated("输出格式", &IMAGE_GEN_FORMATS).scoped(FieldScope::OpenAi),
    FieldSpec::enumerated("画面比例", &IMAGE_GEN_ASPECT_RATIOS).scoped(FieldScope::Gemini),
    FieldSpec::enumerated("分辨率档位", &IMAGE_GEN_IMAGE_SIZES).scoped(FieldScope::Gemini),
    FieldSpec::enumerated("默认张数", &IMAGE_GEN_COUNTS),
    FieldSpec::enumerated("请求超时（秒）", &IMAGE_GEN_TIMEOUTS),
];

/// 联网搜索的引擎候选（文案 = 取值，都是引擎名；清单来自配置域）。
const WEB_SEARCH_ENGINE_OPTIONS: [(&str, &str); 10] = [
    ("duckduckgo", "duckduckgo"),
    ("github", "github"),
    ("arxiv", "arxiv"),
    ("huggingface", "huggingface"),
    ("tavily", "tavily"),
    ("exa", "exa"),
    ("firecrawl", "firecrawl"),
    ("anysearch", "anysearch"),
    ("tinyfish", "tinyfish"),
    ("mixed", "mixed"),
];

/// 联网搜索页的字段；顺序与 `app.rs` 读值时的下标一一对应。
///
/// 五把 API Key 是凭据：界面只显示掩码、进输入态也从空起步，留空表示保留磁盘原值。
///
/// GitHub Token 与 AnySearch Key 都是可选项：前者留空即维持未配置（GitHub 引擎照常抓公开
/// 搜索页），后者留空即走匿名额度。TinyFish 的 Key 必填，但同样允许留空保存（引擎被调用时
/// 才如实报缺凭据）。
const WEB_SEARCH_FIELDS: [FieldSpec; 7] = [
    FieldSpec::enumerated("搜索引擎", &WEB_SEARCH_ENGINE_OPTIONS),
    FieldSpec::secret("Tavily API Key"),
    FieldSpec::secret("Exa API Key"),
    FieldSpec::secret("Firecrawl API Key"),
    FieldSpec::secret("AnySearch API Key"),
    FieldSpec::secret("TinyFish API Key"),
    FieldSpec::secret("GitHub Token"),
];

/// 表单类设置页。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormKind {
    /// 顾问设置：启用 + effort + 顾问模型。
    Advisor,
    /// 工具输出压缩：开关 + 思考深度 + 四个预算 + 压缩模型。
    ToolOutputCompression,
    /// 消息脱敏：总开关 + 各检测项 + 熵兜底 + gitleaks + NER。
    Desensitization,
    /// 持续运转：总开关 + 推理护栏 + 自动续跑。
    RunGuard,
    /// 隔离工作区：开关 + 模式 + 基线 + 复制目录与环境脚本。
    AgentWorkspace,
    /// 图像生成：开关 + 接口/凭据变量/模型 + 四个默认参数。
    ImageGen,
    /// 联网搜索：引擎 + Tavily/Exa/Firecrawl/AnySearch/TinyFish 凭据。
    WebSearch,
    /// Computer Use：总开关 + 时间预算 + 截图上限 + 桌面模型。
    ComputerUse,
}

/// 全部表单页，下标与 [`FormKind::index`] 一致。
pub const FORM_KINDS: [FormKind; 8] = [
    FormKind::Advisor,
    FormKind::ToolOutputCompression,
    FormKind::Desensitization,
    FormKind::RunGuard,
    FormKind::AgentWorkspace,
    FormKind::ImageGen,
    FormKind::WebSearch,
    FormKind::ComputerUse,
];

impl FormKind {
    pub fn index(self) -> usize {
        match self {
            Self::Advisor => 0,
            Self::ToolOutputCompression => 1,
            Self::Desensitization => 2,
            Self::RunGuard => 3,
            Self::AgentWorkspace => 4,
            Self::ImageGen => 5,
            Self::WebSearch => 6,
            Self::ComputerUse => 7,
        }
    }

    pub fn specs(self) -> &'static [FieldSpec] {
        match self {
            Self::Advisor => &ADVISOR_FIELDS,
            Self::ToolOutputCompression => &COMPRESSION_FIELDS,
            Self::Desensitization => &DESENSITIZATION_FIELDS,
            Self::RunGuard => &RUN_GUARD_FIELDS,
            Self::AgentWorkspace => &AGENT_WORKSPACE_FIELDS,
            Self::ImageGen => &IMAGE_GEN_FIELDS,
            Self::WebSearch => &WEB_SEARCH_FIELDS,
            Self::ComputerUse => &COMPUTER_USE_FIELDS,
        }
    }
}

/// 表单字段的取值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    Flag(bool),
    Text(String),
}

impl FieldValue {
    pub fn flag(&self) -> bool {
        matches!(self, Self::Flag(true))
    }

    /// 文本取值；开关字段返回空串（调用点按字段类型取值）。
    pub fn text(&self) -> &str {
        match self {
            Self::Text(text) => text,
            Self::Flag(_) => "",
        }
    }
}

/// 把粘贴文本插进一个字段缓冲；没有缓冲（不在输入态）时返回 `false`。
///
/// 设置字段都是单行输入框（对映 Textual `Input`）：粘贴只取首行，换行不会把
/// `Enter` 提交语义带进来——否则整段多行文本会被后面的换行拆成多次提交。
pub(crate) fn paste_into(input: Option<&mut Composer>, text: &str) -> bool {
    let Some(composer) = input else {
        return false;
    };
    let line = text.split(['\r', '\n']).next().unwrap_or("");
    if line.is_empty() {
        return true;
    }
    composer.insert(line);
    true
}

/// 一页表单的编辑草稿。
#[derive(Debug, Clone)]
pub struct FormState {
    kind: FormKind,
    values: Vec<FieldValue>,
    focused: usize,
    /// 正整数字段的输入缓冲；`None` 表示不在编辑态。
    input: Option<Composer>,
    status: String,
    /// 隐藏字段的判定依据（图像生成页：是否走 Gemini）。其余页恒为 `false`。
    gemini: bool,
}

impl FormState {
    /// 按字段表补齐/截断初值：缺的字段按字段类型补（开关补「关闭」、其余补空文本）。
    pub fn new(kind: FormKind, values: Vec<FieldValue>) -> Self {
        let specs = kind.specs();
        let mut values = values;
        values.truncate(specs.len());
        while values.len() < specs.len() {
            let spec = specs[values.len()];
            values.push(match spec.kind {
                FieldKind::Flag => FieldValue::Flag(false),
                _ => FieldValue::Text(String::new()),
            });
        }
        let mut state = Self {
            kind,
            values,
            focused: 0,
            input: None,
            status: String::new(),
            gemini: false,
        };
        let focused = state.first_visible(0);
        state.focused = focused;
        state
    }

    pub fn kind(&self) -> FormKind {
        self.kind
    }

    pub fn specs(&self) -> &'static [FieldSpec] {
        self.kind.specs()
    }

    pub fn values(&self) -> &[FieldValue] {
        &self.values
    }

    pub fn focused(&self) -> usize {
        self.focused
    }

    /// 第 N 个字段在当前服务商下是否显示。
    pub fn field_visible(&self, index: usize) -> bool {
        match self.specs().get(index) {
            Some(spec) => spec.visible_for(self.gemini),
            None => false,
        }
    }

    /// 当前显示的字段下标（渲染、导航与命中都按这份清单走）。
    pub fn visible_indices(&self) -> Vec<usize> {
        (0..self.specs().len())
            .filter(|index| self.field_visible(*index))
            .collect()
    }

    /// 从 `index` 起向后找到第一个显示的字段（含自身）；全隐藏时返回原下标。
    fn first_visible(&self, index: usize) -> usize {
        let count = self.specs().len();
        if count == 0 {
            return index;
        }
        for offset in 0..count {
            let candidate = (index + offset) % count;
            if self.field_visible(candidate) {
                return candidate;
            }
        }
        index
    }

    /// 设置服务商（图像生成页）；焦点落到当前仍显示的字段上。
    pub fn set_gemini(&mut self, gemini: bool) {
        self.gemini = gemini;
        let focused = self.first_visible(self.focused);
        self.focused = focused;
    }

    /// 直接把焦点移到第 N 个字段（鼠标点击用）；越界或该字段隐藏时不动。
    pub fn set_focused(&mut self, index: usize) {
        if index < self.specs().len() && self.field_visible(index) {
            self.focused = index;
        }
    }

    pub fn input(&self) -> Option<&Composer> {
        self.input.as_ref()
    }

    /// 粘贴：把文本落进正在编辑的字段缓冲。
    ///
    /// 还没进输入态时按字段类型决定：可输入字段先进入输入态（对映 Textual `Input`
    /// 一直处于焦点，粘贴直接落值），开关与候选字段不接收、返回 `false`。
    pub fn paste(&mut self, text: &str) -> bool {
        if self.input.is_none() {
            let Some(FieldKind::Int | FieldKind::Float | FieldKind::Text | FieldKind::Secret) =
                self.field(self.focused).map(|spec| spec.kind)
            else {
                return false;
            };
            self.begin_input(self.focused);
        }
        paste_into(self.input.as_mut(), text)
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn set_status(&mut self, text: impl Into<String>) {
        self.status = text.into();
    }

    pub fn field(&self, index: usize) -> Option<FieldSpec> {
        self.specs().get(index).copied()
    }

    pub fn value(&self, index: usize) -> Option<&FieldValue> {
        self.values.get(index)
    }

    pub fn set_value(&mut self, index: usize, value: FieldValue) {
        if let Some(slot) = self.values.get_mut(index) {
            *slot = value;
        }
    }

    /// 用宿主回填的值覆盖草稿（字段数不足以覆盖全部字段时只改给出的部分）。
    pub fn set_values(&mut self, values: Vec<FieldValue>) {
        for (index, value) in values.into_iter().enumerate() {
            self.set_value(index, value);
        }
    }

    /// 字段之间循环移动；一移动就退出编辑态。跳过当前服务商下隐藏的字段。
    pub fn move_focus(&mut self, delta: isize) {
        self.input = None;
        let visible = self.visible_indices();
        let count = visible.len();
        if count == 0 {
            return;
        }
        let position = visible
            .iter()
            .position(|index| *index == self.focused)
            .unwrap_or(0) as isize;
        let next = (position + delta).rem_euclid(count as isize) as usize;
        self.focused = visible[next];
    }

    pub fn toggle_flag(&mut self, index: usize) {
        if let Some(FieldValue::Flag(flag)) = self.values.get_mut(index) {
            *flag = !*flag;
        }
    }

    /// 进输入态：缓冲区以当前值起步（对映 Python 的 `Input` 预填当前值）。
    ///
    /// 凭据字段例外：从空起步，且留空提交表示「不改动」（与 TTS 页的密钥行同一口径）。
    pub fn begin_input(&mut self, index: usize) {
        if !matches!(
            self.field(index).map(|spec| spec.kind),
            Some(FieldKind::Int)
                | Some(FieldKind::Float)
                | Some(FieldKind::Text)
                | Some(FieldKind::Secret)
        ) {
            return;
        }
        let mut composer = Composer::default();
        if let Some(value) = self.value(index) {
            if !matches!(
                self.field(index).map(|spec| spec.kind),
                Some(FieldKind::Secret)
            ) {
                composer.insert(value.text());
            }
        }
        self.input = Some(composer);
    }

    /// 编辑态键位：`Enter` 落进字段、`Esc` 放弃本次输入。
    pub fn input_key(&mut self, key: KeyCode) {
        let Some(composer) = self.input.as_mut() else {
            return;
        };
        match key {
            KeyCode::Enter => {
                let text = composer.take();
                self.input = None;
                self.set_value(self.focused, FieldValue::Text(text));
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
