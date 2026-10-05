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
    /// 模型渠道引用：候选是配置里的渠道列表（文案 = 渠道名，取值 = 渠道 key）。
    ModelChannel,
    /// 渠道内的模型 ID：候选按当前选中的渠道发现，取值 = 模型 ID（可为空，表示用渠道自带的模型）。
    ModelId,
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
}

impl FieldSpec {
    const fn flag(label: &'static str) -> Self {
        Self {
            label,
            kind: FieldKind::Flag,
            flag_labels: DEFAULT_FLAG_LABELS,
        }
    }

    const fn toggled(label: &'static str, off: &'static str, on: &'static str) -> Self {
        Self {
            label,
            kind: FieldKind::Flag,
            flag_labels: (off, on),
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
        }
    }

    const fn int(label: &'static str) -> Self {
        Self {
            label,
            kind: FieldKind::Int,
            flag_labels: DEFAULT_FLAG_LABELS,
        }
    }

    const fn float(label: &'static str) -> Self {
        Self {
            label,
            kind: FieldKind::Float,
            flag_labels: DEFAULT_FLAG_LABELS,
        }
    }

    const fn text(label: &'static str) -> Self {
        Self {
            label,
            kind: FieldKind::Text,
            flag_labels: DEFAULT_FLAG_LABELS,
        }
    }

    const fn model_channel(label: &'static str) -> Self {
        Self {
            label,
            kind: FieldKind::ModelChannel,
            flag_labels: DEFAULT_FLAG_LABELS,
        }
    }

    const fn model_id(label: &'static str) -> Self {
        Self {
            label,
            kind: FieldKind::ModelId,
            flag_labels: DEFAULT_FLAG_LABELS,
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

/// NER 推理设备候选（对映 Python 的 `NER_DEVICES`）。
const NER_DEVICE_OPTIONS: [(&str, &str); 3] = [("auto", "auto"), ("cpu", "cpu"), ("cuda", "cuda")];

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
const DESENSITIZATION_FIELDS: [FieldSpec; 27] = [
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
    FieldSpec::on_off("NER 兜底"),
    FieldSpec::enumerated("NER 设备", &NER_DEVICE_OPTIONS),
    FieldSpec::text("NER 模型路径"),
    FieldSpec::text("NER 实体类型"),
    FieldSpec::int("NER 最小实体长度"),
    FieldSpec::int("NER 缓存容量"),
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
const IMAGE_GEN_FIELDS: [FieldSpec; 9] = [
    FieldSpec::flag("启用"),
    FieldSpec::text("接口地址"),
    FieldSpec::text("API Key 环境变量"),
    FieldSpec::text("模型"),
    FieldSpec::enumerated("默认尺寸", &IMAGE_GEN_SIZES),
    FieldSpec::enumerated("默认质量", &IMAGE_GEN_QUALITIES),
    FieldSpec::enumerated("输出格式", &IMAGE_GEN_FORMATS),
    FieldSpec::enumerated("默认张数", &IMAGE_GEN_COUNTS),
    FieldSpec::enumerated("请求超时（秒）", &IMAGE_GEN_TIMEOUTS),
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
}

/// 全部表单页，下标与 [`FormKind::index`] 一致。
pub const FORM_KINDS: [FormKind; 6] = [
    FormKind::Advisor,
    FormKind::ToolOutputCompression,
    FormKind::Desensitization,
    FormKind::RunGuard,
    FormKind::AgentWorkspace,
    FormKind::ImageGen,
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

/// 一页表单的编辑草稿。
#[derive(Debug, Clone)]
pub struct FormState {
    kind: FormKind,
    values: Vec<FieldValue>,
    focused: usize,
    /// 正整数字段的输入缓冲；`None` 表示不在编辑态。
    input: Option<Composer>,
    status: String,
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
        Self {
            kind,
            values,
            focused: 0,
            input: None,
            status: String::new(),
        }
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

    /// 直接把焦点移到第 N 个字段（鼠标点击用）；越界时不动。
    pub fn set_focused(&mut self, index: usize) {
        if index < self.specs().len() {
            self.focused = index;
        }
    }

    pub fn input(&self) -> Option<&Composer> {
        self.input.as_ref()
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

    /// 字段之间循环移动；一移动就退出编辑态。
    pub fn move_focus(&mut self, delta: isize) {
        self.input = None;
        let count = self.specs().len() as isize;
        if count == 0 {
            return;
        }
        self.focused = ((self.focused as isize + delta).rem_euclid(count)) as usize;
    }

    pub fn toggle_flag(&mut self, index: usize) {
        if let Some(FieldValue::Flag(flag)) = self.values.get_mut(index) {
            *flag = !*flag;
        }
    }

    /// 进输入态：缓冲区以当前值起步（对映 Python 的 `Input` 预填当前值）。
    pub fn begin_input(&mut self, index: usize) {
        if !matches!(
            self.field(index).map(|spec| spec.kind),
            Some(FieldKind::Int) | Some(FieldKind::Float) | Some(FieldKind::Text)
        ) {
            return;
        }
        let mut composer = Composer::default();
        if let Some(value) = self.value(index) {
            composer.insert(value.text());
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
