//! 工作区工具执行体：路径安全、读取、写入、替换、命令、后台监控、知识库、记忆、联网、视觉与注册表。
//!
//! 语义基准是 `omnicrawl/workspace/tools.py`（工作区文件/搜索/命令的共享实现）、
//! `omnicrawl/workspace/monitor.py`（后台命令监控）、`omnicrawl/knowledge/__init__.py`
//! （知识库读写与索引）、`omnicrawl/agent/toolkit/memory_tools.py`（记忆工具）、
//! `omnicrawl/agent/toolkit/image_tools.py`（本机图片读取）、`omnicrawl/net/`（搜索与抓取）与
//! `omnicrawl/agent/controllers/tools/implementations.py`（工具包装）。已搬入 `read` /
//! `read_image` / `write_file` / `Edit_file` / `bash` / `powershell` / `list` / `find` / `grep` /
//! `git` / `monitor`、五个 `kb_*`、四个 `memory_*` 与 `web_search` / `fetcher` 执行体；
//! 工具表、参数归一化、Schema 校验与压缩复用内核已搬好的 `omnicrawl-controllers`。

pub mod advisor;
pub mod arguments;
pub mod command;
pub mod declarations;
pub mod edit;
pub mod error;
pub mod fetcher;
pub mod finding;
pub mod git;
pub mod grep;
pub mod image_gen;
pub mod knowledge;
pub mod listing;
pub mod memory;
pub mod monitor;
pub mod paths;
pub mod read;
pub mod read_image;
pub mod registry;
pub mod sample;
pub mod search_common;
pub mod tts;
pub mod web_search;
pub mod web_transport;
pub mod windows;
pub mod wreq_transport;
pub mod write;

pub use advisor::AdvisorOptions;
pub use error::{ToolError, ToolOutcome};
pub use fetcher::FetcherOptions;
pub use image_gen::ImageGenOptions;
pub use knowledge::KnowledgeBase;
pub use memory::MemoryOptions;
pub use monitor::MonitorManager;
pub use paths::WorkspacePaths;
pub use registry::{RegistryOptions, ToolRegistry, IMPLEMENTED_TOOLS};
pub use tts::TtsOptions;
pub use web_search::WebSearchOptions;
pub use web_transport::{WebError, WebErrorKind, WebRequest, WebResponse, WebTransport};
pub use wreq_transport::WreqWebTransport;
