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

impl Plan {
    /// 是否走真隔离。非 Windows 上不存在这个分支，恒为 `false`。
    pub fn is_isolated(&self) -> bool {
        #[cfg(windows)]
        {
            matches!(self, Self::Isolated(_))
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
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
            // 网络通道与隔离身份同属「真隔离」这一档，必须一起就绪：容器没有网络能力，
            // 出网只能经宿主回环代理，没有代理就是彻底断网（不是回避隔离）。
            //
            // `ensure_network` 自身幂等且**不返回错误**：豁免或代理拉不起来时它把形态降级为
            // `NetStatus::Failed` 且不注入代理变量，于是这条命令照样在容器里跑，只是出不去。
            // 因此这里不必把它的结果变成命令失败——失败方向仍然是关闭。
            self.sandbox.ensure_network(None);
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
