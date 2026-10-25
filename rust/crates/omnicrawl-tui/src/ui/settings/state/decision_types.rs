//! 由 `settings/state.rs` 按页面拆出：行定义与常量，语义未变。

use super::*;

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

const DECISION_LIST_HINT: &str =
    "↑↓ 选择  Enter/→ 编辑渠道、←/→ 切换开关  N 新建  D 删除  Esc 返回";

const DECISION_FORM_HINT: &str = "↑↓/Tab 换字段  Enter 编辑或展开候选  Ctrl+S 保存  Esc 返回列表";

