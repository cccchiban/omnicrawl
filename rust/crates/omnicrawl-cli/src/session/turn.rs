//! 由 `session.rs` 按职责拆出，语义未变。

use super::*;

use super::model_port::{ActiveAssistantSlot, KernelModelPort, KernelModelRuntime, RemoteModelPort, RuntimeBuildKey, TurnUsage, model_config_of, plugin_model_hooks_of, provider_warning_event, read_api_key, session_config_of, settings_without_prune_is_empty, tool_call_prune_of};

use super::protocol::{Inbound, PortFailure, handle_workspace_switch, protocol_version_of};

use super::session_query::{SessionQuery};

use super::tools::{RemoteTools};

mod conn;
mod turn_run;
mod settle;


pub(crate) use conn::post_compaction_context_tokens;
pub(crate) use conn::Conn;
pub(crate) use conn::ProtocolSink;
pub(crate) use turn_run::run_turn;
pub(crate) use turn_run::drain_inbound_during_turn;
pub(crate) use turn_run::serve_model_hook;
pub(crate) use settle::TurnRunGuard;
pub(crate) use settle::cancelled_turn_summary;
pub(crate) use settle::is_context_overflow;
pub(crate) use settle::compaction_progress;
pub(crate) use settle::recover_context_overflow;
pub(crate) use settle::evicted_prefix;
pub(crate) use settle::run_session_tail;