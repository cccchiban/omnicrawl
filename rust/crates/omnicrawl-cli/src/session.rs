//! 会话：在一条 NDJSON 流上承载协议 v1。
//!
//! 内核自己拥有回合循环（`omnicrawl-core`），把两个宿主端口经协议外发：`model.reply` 代模型回复、
//! `tool.batch` 代工具批次。宿主端口是过渡形态——内核自带 provider runtime 后只需换掉
//! [宿主端口实现] 一个实现，协议与循环都不动。
//!
//! 一个回合只跑一个（第二个 `turn.submit` 回 `-32002`）；回合进行中到达的 `turn.cancel` / `shutdown`
//! 会立即中止当前端口调用，其余请求照常应答，不阻塞宿主。


use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::desensitization::{
    load_desensitization_config, DesensitizationConfig,
};
use omnicrawl_controllers::compression as compression_logic;
use omnicrawl_controllers::context_compaction::TokenUsageSample;
use omnicrawl_controllers::context_compaction::{
    estimate_messages_tokens, RECALL_SESSION_EVIDENCE_TOOL_NAME,
};
use omnicrawl_controllers::shared::CONTEXT_OVERFLOW_ERROR_MARKERS;
use omnicrawl_controllers::tool_args::{public_tool_arguments, TODO_TOOL_NAME};
use omnicrawl_controllers::tool_impl::{project_todos, TodoItem};
use omnicrawl_controllers::turn::compaction::format_compaction_savings_notice;
use omnicrawl_controllers::turn::continuation::{
    self, ContinueConfig, ContinueStep, LastReplyFacts,
};
use omnicrawl_controllers::turn::tool_prune::{
    evicted_event_payload, eviction_measure_payload, prunable_tool, PruneCandidate,
    EVICTED_EVENT_TYPE, EVICTION_MEASURE_EVENT_TYPE,
};
use omnicrawl_controllers::turn::{is_continue_last_task_request, resolve_continue_request};
use omnicrawl_core::{
    AgentLoopLimits, AgentLoopObservation, AgentLoopResult, AgentLoopRunner, AgentModelReply,
    LoopError, LoopGuards, ReplySource, SystemClock, ToolBatchHost, ToolCall, ToolResult,
};
use omnicrawl_ipc::bridge::{
    initialize_result, method, unsupported_version_error, BridgeError, Command,
    ContextCompactionPayload, ContextPruneGroup, ContextPruneRequest, ContextPruneResult,
    HostEvent, InitializeParams, KernelModelConfig, KernelSessionConfig, MessagePayload,
    ModelHookRequest, ModelHookResult, ModelRequest, ModelRequestErrorPayload,
    ModelResponseAfterPayload, SessionAppendParams, SessionHistoryParams, SessionListParams,
    SessionModelSettings, SessionRenameParams, SessionResumeParams, SessionSettingsParams,
    SubagentEventPayload, SubagentQueryParams, SubagentRunParams, TextPayload, TokenUsagePayload,
    ToolBatch, ToolBatchResult, ToolCallArgumentsPayload, ToolCallStartedPayload, ToolEventPayload,
    ToolStartedPayload, TurnCancelParams, TurnFinishedPayload, TurnSubmitParams,
    WorkspaceSwitchParams,
};
use omnicrawl_ipc::frame::{error_code, ErrorObject, Frame, Id};
use omnicrawl_ipc::version::negotiate_version;
use omnicrawl_llm::desensitization::ner::NerLayerOptions;
use omnicrawl_llm::desensitization::rules::{
    CATEGORY_BANK_CARD, CATEGORY_DB_CONNECTION_STRING, CATEGORY_EMAIL, CATEGORY_EXTERNAL_IP,
    CATEGORY_INTERNAL_IP, CATEGORY_LICENSE_PLATE, CATEGORY_MAC_ADDRESS, CATEGORY_PEM_PRIVATE_KEY,
    CATEGORY_URL,
};
use omnicrawl_llm::desensitization::{DesensitizationOptions, DesensitizationRuntime};
use omnicrawl_llm::{
    build_runtime, to_openai_messages, ChatRequestInput, ModelDescriptor, ModelRuntime,
    ProviderProfile, RuntimeErrorKind, SinkFlow, TurnSink, CONTEXT_LENGTH_EXCEEDED_MESSAGE,
};
use omnicrawl_protocol::{
    conform_tool_names, conversation_from_openai_messages, normalize_call_ids,
    rotate_tail_call_ids, tool_spec_from_openai_item, GenerationOptions, ModelReply,
    ModelStreamEvent, ToolNameMap, ToolSpec,
};
use serde_json::{json, Map, Value};

use omnicrawl_controllers::context_compaction::SourceEvent;
use omnicrawl_controllers::subagents::batch::batch_summary;
use omnicrawl_controllers::subagents::coordinator::{
    failure_payload, json_result_text, task_event_payload,
    terminal_event_payload, top_level_error,
};
use omnicrawl_controllers::subagents::coordinator::{query_request, QueryRequest};
use omnicrawl_controllers::subagents::coordinator::{
    worktree_control_request, WorktreeControlRequest,
};
use omnicrawl_controllers::subagents::orchestration::require_completed_result;
use omnicrawl_controllers::subagents::tasks::{CancelToken, SubAgentTaskManager};
use omnicrawl_controllers::subagents::worktrees::{
    artifact_lines, discard_guard, WorktreeArtifacts,
};
use omnicrawl_session::{
    evict_tool_call_messages, tool_result_message, user_message_with_images, utc_now,
    SessionListQuery, SessionStore,
};

use crate::compression::KernelCompressor;
use crate::subagent::{PreparedTask, SubAgentExecution, SubAgentRuntime};
use crate::vision_proxy::KernelVisionProxy;

use crate::compaction::{
    compact_after_turn, compact_now, recall_session_evidence, recover_after_overflow, KernelSession,
};





pub(crate) use model_port::build_model_runtime_with_key;
pub(crate) use model_port::read_api_key;
pub(crate) use model_port::parse_options;
pub(crate) use protocol::run_stdio;

mod model_port;
mod tools;
mod turn;
mod session_query;
mod subagent_run;
mod protocol;

#[cfg(test)]
mod runtime_selection_tests {
    use super::*;
    use crate::session::model_port::{build_model_runtime, DesensitizationFingerprint, RuntimeBuildKey};

    fn config(provider: &str, protocol: &str) -> KernelModelConfig {
        KernelModelConfig {
            model: "m".to_string(),
            provider: provider.to_string(),
            protocol: protocol.to_string(),
            base_url: String::new(),
        api_key: String::new(),
            user_agent: String::new(),
            system_prompt: String::new(),
            context_messages: Vec::new(),
            tools: Vec::new(),
            options: Value::Null,
            request_timeout_seconds: None,
            context_window_tokens: 0,
            prompt_cache_capable: false,
            prompt_cache_identity: std::collections::BTreeMap::new(),
            native_vision: None,
            vision_capability: None,
            request_retry_count: 1,
        }
    }

    /// 缺凭据在**构造运行期**时就失败（与 Python 的 `create_*_client` 同），所以回退验证
    /// 改用显式凭据的入口；同时钉住「空 id 不会造出 `Profile  缺少…` 双空格文案」。
    #[test]
    fn empty_provider_falls_back_to_openai() {
        assert!(build_model_runtime_with_key(&config("", ""), "k".to_string()).is_ok());
        assert!(build_model_runtime_with_key(&config("gemini", ""), "k".to_string()).is_ok());
        let error = build_model_runtime(&config("", ""))
            .err()
            .expect("缺凭据必须失败");
        match error {
            LoopError::ReplySource(message) => assert_eq!(
                message,
                "模型 Profile 缺少 API Key。请在 Profile 中配置 api_key。",
            ),
            other => panic!("意外的错误类型：{other:?}"),
        }
    }

    #[test]
    fn protocol_must_match_provider() {
        let error = build_model_runtime(&config("gemini", "anthropic_messages"))
            .err()
            .expect("协议与 Provider 不匹配必须失败");
        match error {
            LoopError::ReplySource(message) => {
                assert!(
                    message.contains("anthropic_messages"),
                    "报错文案：{message}"
                );
            }
            other => panic!("应为 ReplySource，实际 {other:?}"),
        }
    }

    /// 脱敏配置是运行时构建键的一部分：改了它，下一次取运行时必然换实例。
    ///
    /// 这条回归的是「改了消息脱敏却要等换模型才生效」——装饰器只在构建运行时包装一次。
    #[test]
    fn desensitization_config_participates_in_the_runtime_build_key() {
        let off = DesensitizationFingerprint(Some(DesensitizationConfig::default()));
        let on = DesensitizationFingerprint(Some(DesensitizationConfig {
            enabled: true,
            ..DesensitizationConfig::default()
        }));
        assert_ne!(off, on, "开关变了必须算变了");
        assert_eq!(
            off,
            DesensitizationFingerprint(Some(DesensitizationConfig::default())),
            "同样一份配置必须相等，否则每回合都会重建运行时"
        );
        // 「读不出来」与「读到一份未启用」在键上必须可区分：前者是「配置暂时读不到」，
        // 后者是明确的关闭。混同会在配置损坏时把装饰器静默换成不加脱敏的那份。
        assert_ne!(off, DesensitizationFingerprint(None));

        // 逐字段敏感：改一档匹配参数也算变了（这里取最容易漏掉的熵兜底阈值）。
        let tweaked = DesensitizationFingerprint(Some(DesensitizationConfig {
            entropy_min_bits: DesensitizationConfig::default().entropy_min_bits + 1.0,
            ..DesensitizationConfig::default()
        }));
        assert_ne!(off, tweaked);
    }

    /// `read()` 不能带噪声：同一份进程配置连读两次必须相等。
    #[test]
    fn desensitization_fingerprint_read_is_stable() {
        assert_eq!(
            DesensitizationFingerprint::read(),
            DesensitizationFingerprint::read()
        );
    }

    /// 构建键相等性只由字段决定（缓存命中判据）。
    #[test]
    fn runtime_build_key_compares_by_fields() {
        let key = RuntimeBuildKey {
            session_id: "s".to_string(),
            provider: "openai".to_string(),
            protocol: String::new(),
            base_url: String::new(),
            model: "m".to_string(),
            user_agent: String::new(),
            context_window_tokens: 0,
            api_key: "k".to_string(),
            desensitization: DesensitizationFingerprint(None),
        };
        assert_eq!(key, key.clone(), "同一份键必须自等（缓存才可能命中）");
        assert_ne!(
            key,
            RuntimeBuildKey {
                api_key: "k2".to_string(),
                ..key.clone()
            },
            "换凭据必须重建运行时"
        );
        assert_ne!(
            key,
            RuntimeBuildKey {
                desensitization: DesensitizationFingerprint(Some(DesensitizationConfig::default())),
                ..key.clone()
            },
            "脱敏配置变了必须重建运行时"
        );
    }
}

#[cfg(test)]
mod session_tests {
    use crate::session::model_port::provider_warning_event;
    use crate::session::turn::cancelled_turn_summary;
    use omnicrawl_ipc::bridge::HostEvent;
    use omnicrawl_protocol::ProviderWarning;

    #[test]
    fn cancelled_summary_counts_executed_tools() {
        assert_eq!(
            cancelled_turn_summary(&[]),
            "（上一回合被取消，未生成最终回复，未执行任何工具）"
        );
        assert_eq!(
            cancelled_turn_summary(&["read".to_string()]),
            "（上一回合被取消，未生成最终回复）已执行工具：read"
        );
        // 同一工具多次：Python 用 `name×次数`；不同工具之间用「，」连接。
        assert_eq!(
            cancelled_turn_summary(&["read".to_string(), "bash".to_string(), "read".to_string(),]),
            "（上一回合被取消，未生成最终回复）已执行工具：read×2，bash"
        );
        assert_eq!(
            cancelled_turn_summary(&["  ".to_string()]),
            "（上一回合被取消，未生成最终回复，未执行任何工具）"
        );
    }

    #[test]
    fn desensitization_warnings_become_notices_not_status() {
        let warning = ProviderWarning::new(
            "desensitization_malformed",
            "检测到 1 处疑似畸形脱敏占位符，已按原样保留。",
        );
        assert!(
            matches!(provider_warning_event(&warning), HostEvent::Notice(_)),
            "脱敏还原告警要进会话流，不能顶掉运行状态行"
        );
        let warning = ProviderWarning::new("prompt_cache_unsupported", "网关不支持缓存键。");
        assert!(
            matches!(provider_warning_event(&warning), HostEvent::Status(_)),
            "网关降级告警仍是进行中状态"
        );
    }
}
