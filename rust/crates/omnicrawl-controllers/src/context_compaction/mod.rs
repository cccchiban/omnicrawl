//! `omnicrawl/agent/context_compaction/` 的移植（按 Python 包结构分模块）。
//!
//! 已搬：`models.py` 与 `policy.py`（`policy` 模块）、`validation.py`、`projection.py`、
//! `ledger.py`、`evidence.py`。未搬：`service.py`（压缩编排）、`summary.py`（结构化摘要生成）
//! ——它们需要摘要模型调用。

pub mod evidence;
pub mod ledger;
pub mod policy;
pub mod projection;
pub mod service;
pub mod summary;
pub mod validation;

pub use evidence::*;
pub use ledger::*;
pub use policy::*;
pub use projection::*;
pub use service::*;
pub use summary::*;
pub use validation::*;
