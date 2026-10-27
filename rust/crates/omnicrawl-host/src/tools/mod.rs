//! 工作区工具执行体：路径安全、读取、写入、替换、命令、后台监控、知识库、记忆、联网、视觉与注册表。
//!
//! 语义基准是 `omnicrawl/workspace/tools.py`（工作区文件/搜索/命令的共享实现）、
//! `omnicrawl/workspace/monitor.py`（后台命令监控）、`omnicrawl/knowledge/__init__.py`
//! （知识库读写与索引）、`omnicrawl/agent/toolkit/memory_tools.py`（记忆工具）、
//! `omnicrawl/agent/toolkit/image_tools.py`（本机图片读取）、`omnicrawl/net/`（搜索与抓取）与
//! `omnicrawl/agent/controllers/tools/implementations.py`（工具包装）。已搬入 `read` /
//! `read_image` / `write_file` / `Edit_file` / `bash` / `powershell` / `list` / `find` / `grep` /
//! `skill_search` / `git` / `monitor`、五个 `kb_*`、四个 `memory_*`、`web_search` / `fetcher` 与
//! `config_edit` 执行体；
//! 工具表、参数归一化、Schema 校验与压缩复用内核已搬好的 `omnicrawl-controllers`。

pub mod advisor;
pub mod arguments;
pub mod charset;
pub mod client_hints;
pub mod command;
pub mod config_tool;
pub mod cookies;
pub mod decision_choice;
pub mod decision_prune;
pub mod decision_search;
pub mod decision_suggest;
pub mod declarations;
pub mod edit;
pub mod error;
pub mod fetch_metadata;
pub mod fetcher;
pub mod finding;
pub mod git;
pub mod grep;
pub mod hsts;
pub mod http_cache;
pub mod image_gen;
pub mod knowledge;
pub mod listing;
pub mod memory;
pub mod monitor;
pub mod paths;
pub mod read;
pub mod read_image;
pub mod referrer;
pub mod registry;
pub mod robots;
pub mod robots_cache;
pub mod sample;
pub mod search_common;
pub mod skills;
pub mod tls_transport;
pub mod tts;
pub mod web_search;
pub mod web_transport;
pub mod windows;
pub mod write;

pub use advisor::AdvisorOptions;
pub use cookies::{CookieJar, SharedJar};
pub use decision_choice::{
    chosen_option, custody_options_from_config, ChoiceClient, CustodyContext, CustodyOptions,
};
pub use decision_prune::{
    evicted_call_ids_for, prune_options_from_config, PruneChannel, PruneClient, PruneOptions,
};
pub use decision_search::{rerank_options_from_config, RerankClient, RerankOptions};
pub use decision_suggest::{
    insert_suggestions, split_suggestion, subagent_suggest_options_from_config,
    suggest_options_from_config, suggestion_lines, SuggestClient, SuggestKind, SuggestOptions,
    SuggestionInputs,
};
pub use error::{ToolError, ToolOutcome};
pub use fetcher::{fetch_batch, tls_session_cache_from_config, FetcherOptions, FetchBatch};
pub use image_gen::{image_gen_options_from_config, ImageGenOptions};
pub use knowledge::KnowledgeBase;
pub use memory::MemoryOptions;
pub use monitor::MonitorManager;
pub use paths::WorkspacePaths;
pub use registry::{RegistryOptions, ToolRegistry, IMPLEMENTED_TOOLS};
pub use tls_transport::TlsWebTransport;
pub use tts::{tts_backend, TtsOptions};
pub use web_search::{web_search_options_from_config, Bm25RankOptions, WebSearchOptions};
pub use web_transport::{WebError, WebErrorKind, WebRequest, WebResponse, WebTransport};
