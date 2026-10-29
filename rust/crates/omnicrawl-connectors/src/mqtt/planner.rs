//! 规划器边界：把一次唤醒变成一条原始动作。
//!
//! 实现放在宿主侧（决策模型优先，未配置时回落主模型），本 crate 只依赖这个面。
//! 这样策略层与协议层可以独立测试，不需要任何模型凭据。

use super::prompt::{PlanRequest, RawPlan};

/// 一次结构化决策的入口。
pub trait Planner: Send + Sync {
    /// 拿到现场模板，返回模型选出的动作（未校验）。
    ///
    /// 失败一律返回可读原因：调用方据此走规则里的安全兜底，不会把异常悄悄吞掉。
    fn plan(&self, request: &PlanRequest) -> Result<RawPlan, String>;

    /// 审计里记录的名字：decision / main_model。
    fn describe(&self) -> &'static str;
}
