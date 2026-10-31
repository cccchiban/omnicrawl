//! `omnicrawl-trigger`：外部事件拉起独立 loop 的本机 REST 接口。
//!
//! 外部组件（定时任务、传感器脚本、CI 失败钩子、监控告警）自己判定条件，命中后提交一条
//! 自定义消息；本服务为它拉起一个**独立 loop**——自己的会话、自己的内核进程、自己的一次回合，
//! 跑完把结果落盘并按需投递到消息平台。
//!
//! 为什么不复用本地 API 的 `POST /api/v1/runs`：那条路是交互式入口，占单活动生成名额
//! （`RUN_ACTIVE`）且写当前会话，无人值守任务会和用户抢名额、把噪音写进用户的对话历史。
//!
//! 边界：
//! * 只监听回环地址，且**不做鉴权**——任何能访问该地址的本机程序都能拉起 loop。因此配置拒绝
//!   非回环地址，且工具白名单默认收窄到只读 + 记忆/知识库 + 联网检索。
//! * 不写第二套 Agent：一个 loop 就是一个 `omnicrawl_api::AgentService`（同一张工具表、
//!   同一份提示词装配、同一套自动审查），口径与 `[mqtt]` 回路一致。
//! * 条件判定完全在外部组件：本服务不内置调度器、不解析 cron。
//!
//! 契约见 `omnicrawl://docs/trigger_api.md`；实施进度见 `PROGRESS.md`。

pub mod app;
pub mod config;
pub mod notify;
pub mod pool;
pub mod records;
pub mod request;
pub mod routes;
pub mod server;

pub use app::{ApiError, AppState, build_app, build_router};
pub use config::{SESSION_DIRNAME, TriggerSettings, kernel_program, load_settings};
pub use pool::{PoolConfig, TriggerPool, UNATTENDED_PROMPT};
pub use records::{
    NewTrigger, NotifyOutcome, NotifySpec, RecordError, TriggerRecord, TriggerState, TriggerStore,
};
pub use server::{EnsureOutcome, ServiceState, ensure_running, service_ready, stop};
