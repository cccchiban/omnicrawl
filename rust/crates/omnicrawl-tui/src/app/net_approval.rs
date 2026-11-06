//! 沙盒网络审批：把代理线程等着的 `ask` 请求搬到界面上，并把回答送回去。
//!
//! 为什么不能复用工具审批面板（`Waiting::Approval`）：那个面板挂在一条工具批次上，
//! 批次推进到它才会出现、回答后立刻消失。网络审批的发问方是**代理线程**——沙盒内某条
//! 命令发起一次连接时它才出现，与"当前有没有工具批次""批次推进到第几个调用"都无关，
//! 而且一条命令可以先后触发多次。因此它需要自己的面板与自己的按键通路。

use super::*;

impl App {
    /// 每帧从审批站取一次待决请求（界面因此能看到新请求、也能感知超时消失）。
    ///
    /// 只保留队列第一条：并发多条连接时逐条问，一次让用户回答一个。
    pub fn tick_net_approvals(&mut self) {
        let station = self.registry.sandbox().approvals();
        let now = Instant::now();
        let next = station
            .pending()
            .into_iter()
            .next()
            .filter(|pending| pending.remaining_seconds(now) > 0)
            .map(|pending| crate::state::NetApprovalPanel {
                id: pending.id,
                target: pending.target(),
                remaining_seconds: pending.remaining_seconds(now),
            });
        // 变了才置脏：这个函数每帧都跑，无谓的重画会让空闲界面每 20ms 刷一次。
        // 剩余秒数也算在内，面板上的倒计时因此会跟着走。
        if next != self.state.net_approval {
            self.state.net_approval = next;
            self.state.touch_conversation();
        }
    }

    /// 回答待决的网络审批；返回是否确有请求被回答。
    pub fn answer_net_approval(&mut self, approved: bool) -> bool {
        let Some(pending) = self.state.net_approval.take() else {
            return false;
        };
        self.registry
            .sandbox()
            .approvals()
            .answer(pending.id, approved);
        // 回执放在瞬时提示行：沙盒内的命令随后会自己给出成功或失败的具体报错，
        // 这里只说明"决定已生效"，不再重复一遍。
        self.state.notice(if approved {
            format!("已放行沙盒内的网络连接 {}", pending.target)
        } else {
            format!("已拒绝沙盒内的网络连接 {}", pending.target)
        });
        self.state.touch_conversation();
        true
    }
}
