//! 沙盒开关到执行路径的桥：惰性准备隔离环境，并区分「关 / dry-run / 真隔离」。
//!
//! 设计（`sandbox_windows_design.md` §13）要求「沙盒关闭时，全部既有测试零变化」，
//! 因此 [`IsolationGate`] 在关闭状态下不做任何事，非 Windows 平台恒为关闭。

use std::sync::Arc;

use super::Sandbox;

/// 一条命令该怎么跑。
pub enum Plan {
    /// 以宿主身份直接跑（沙盒关闭）。
    Host,
    /// 真隔离：拿一个准备好的容器身份。
    #[cfg(windows)]
    Isolated(Arc<super::windows::Isolation>),
}

pub struct IsolationGate {
    sandbox: Arc<Sandbox>,
}

impl IsolationGate {
    pub fn new(sandbox: Arc<Sandbox>) -> Self {
        Self { sandbox }
    }

    pub fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }

    /// 是否真隔离（`enabled` 且 `mode = appcontainer`）。
    pub fn isolates(&self) -> bool {
        cfg!(windows) && self.sandbox.isolates()
    }

    /// 决定执行方式。
    ///
    /// 真隔离模式下**任何准备失败都不回退到宿主身份**：沙盒的价值就在于失败方向是
    /// 「跑不了」，而不是「悄悄不隔离」。错误会带着具体原因返回给模型。
    pub fn plan(&self) -> Result<Plan, String> {
        #[cfg(windows)]
        {
            if !self.sandbox.isolates() {
                return Ok(Plan::Host);
            }
            self.sandbox.isolation().map(Plan::Isolated)
        }
        #[cfg(not(windows))]
        {
            if self.sandbox.isolates() {
                return Err("沙盒真隔离目前只在 Windows 上实现；请把 `[sandbox] mode` 设为 off 或关闭沙盒。".to_string());
            }
            Ok(Plan::Host)
        }
    }

    /// 输出里的一行标记，让模型与用户都能看出这条命令在沙盒里跑。
    pub fn label(&self) -> String {
        if self.sandbox.isolates() {
            "[sandbox:appcontainer]".to_string()
        } else {
            String::new()
        }
    }
}
