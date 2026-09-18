//! `omnicrawl/agent/context_compaction/` 的移植（按 Python 包结构分模块）。
//!
//! 已搬：`models.py` 与 `policy.py`（`policy` 模块）、`validation.py`、`projection.py`。
//! 未搬：`service.py`（摘要模型调用编排）、`summary.py`（结构化摘要生成）、`evidence.py`
//! （会话证据检索）、`ledger.py` 的存储面——它们需要模型调用或 Session I/O。

pub mod policy;
pub mod projection;
pub mod validation;

pub use policy::*;
pub use projection::*;
pub use validation::*;
