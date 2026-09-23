//! 工具审批模式：协议与 Python 侧同名（`omnicrawl/approval.py` 的三个取值）。
//!
//! TUI 的启动参数与本地 API 的 `[approval]` 配置都归一到这里；具体“某个调用该不该问人”
//! 交给内核已搬好的 `omnicrawl_controllers::approval::decide`（[`crate::host::approval_decision`]），
//! 本模块只负责模式本身的解析与展示。
//!
//! 三种模式的差别（与 Python 一致）：
//! - `auto`：全部放行；
//! - `manual`：shell 命令工具与“非只读 git 操作”弹确认，其余放行；
//! - `review`：删除类与下载并执行类调用、高风险 Git 操作交给审查模型（见 [`crate::review`]），
//!   其余放行；只有人工确认那一档才弹面板。

/// 审批模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    Manual,
    Review,
    Auto,
}

impl ApprovalMode {
    /// 状态栏用的短标签。
    pub fn label(self) -> &'static str {
        match self {
            Self::Manual => "MAN",
            Self::Review => "REV",
            Self::Auto => "AUTO",
        }
    }

    /// 插件 Hook 载荷与配置里的模式名（与 Python `config.approval_mode` 同形）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Review => "review",
            Self::Auto => "auto",
        }
    }

    /// 判定层（`omnicrawl-controllers`）的同名枚举。
    pub fn decision_mode(self) -> omnicrawl_controllers::approval::ApprovalMode {
        match self {
            Self::Manual => omnicrawl_controllers::approval::ApprovalMode::Manual,
            Self::Review => omnicrawl_controllers::approval::ApprovalMode::Review,
            Self::Auto => omnicrawl_controllers::approval::ApprovalMode::Auto,
        }
    }

    /// 配置值 → 模式；未知取值返回 `None`（由调用方决定是报错还是回落）。
    pub fn from_config(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "manual" => Some(Self::Manual),
            "review" => Some(Self::Review),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }

    /// 命令与参数里的字面量解析；只认 `manual` / `review` / `auto`。
    pub fn parse(value: &str) -> Result<Self, String> {
        Self::from_config(value).ok_or_else(|| {
            format!(
                "审批模式只支持 manual / review / auto，收到：{}",
                value.trim()
            )
        })
    }
}
