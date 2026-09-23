//! 配置对话（`omnicrawl/config_chat/` 的 Rust 移植）：用一句自然语言改配置。
//!
//! Python 侧由两个模块组成，这里原样对映：
//!
//! - [`router`]（`router.py`）：字符级双向 GRU + 别名向量检索，输出
//!   「动作 + 配置路径 + 取值」命令；
//! - [`service`]（`service.py`）：类型校验、TOML 写回、运行态同步。
//!
//! 与 Python 的差别有两处，都写在 `README.md`：
//!
//! - **权重不是 torch 存档**：内核不实现 pickle 解析，`data/config_router.bin` 由
//!   `rust/tools/gen_config_router_fixture.py` 从 `assets/router.pt` 转成
//!   「魔数 + 头部 JSON + f32 数据块」的自描述二进制（[`router_weights`] 负责读取与前向）；
//! - **进程外信息注入**：配置路径与 `AI_*` 环境变量走 `omnicrawl-config` 的
//!   `ConfigEnvironment`，运行态同步目标从 agent 换成 [`service::ConfigChatAgent`]。

pub mod assets;
pub mod router;
pub mod router_weights;
pub mod service;

pub use assets::{LabelEntry, LabelsDocument, ALIASES_FILENAME, LABELS_FILENAME};
pub use router::{
    spans_of, split_clauses, value_spans, ConfigRouter, ConfigRouterError, MAX_CLAUSE_CHARS,
    MAX_SPANS,
};
pub use router_weights::{
    RouterForward, RouterModelConfig, RouterWeights, WEIGHTS_FILENAME, WEIGHTS_MAGIC,
};
pub use service::{
    ConfigChange, ConfigChatAgent, ConfigChatCommand, ConfigChatError, ConfigChatService,
};
