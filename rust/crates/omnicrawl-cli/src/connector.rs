//! 内核自带连接器宿主：`omnicrawl --connector <name>` 时在进程内驱动消息平台接入。
//!
//! 连接器只做平台 I/O 与显示映射，回合、命令与工作区由这里的 [`AgentDriver`] 实现承担。
//! 模型请求由 `omnicrawl-llm` 直接发出；本模式不向模型宣告工具（工具实现在宿主与插件侧），
//! 若模型仍请求工具，批次按「本模式不可用」回观察，而不是静默丢弃。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use omnicrawl_connectors::agent::{
    AgentDriver, AgentStatus, AskUserHandler, ConfirmHandler, TurnError, TurnEvent, TurnOutcome,
    WorkspaceSwitch,
};
use omnicrawl_connectors::feishu::{check_config, load_feishu_config, ConfigSource, FeishuApi, FeishuBot};
use omnicrawl_connectors::http::{HttpTransport, UreqTransport};
use omnicrawl_connectors::telegram::{load_telegram_config, TelegramBot};
use omnicrawl_core::{
    AgentLoopLimits, AgentLoopObservation, AgentLoopRunner, AgentModelReply, LoopError, LoopGuards,
    ReplySource, SystemClock, ToolBatchHost, ToolCall, ToolResult,
};
use omnicrawl_llm::{ChatEndpoint, ChatRequestInput, DiscardSink, OpenAiChatRuntime};
use omnicrawl_protocol::{conversation_from_openai_messages, GenerationOptions, ToolSpec};

const USER_AGENT: &str = "omnicrawl-kernel-connector/0.0.1";
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_SYSTEM_PROMPT: &str = "你是 OmniCrawl 内核里的助手，回答保持简洁。";
/// 工具批次在连接器模式下的错误码：调用方据此区分「工具失败」与「本模式没有执行器」。
const TOOL_UNAVAILABLE: &str = "tool_unavailable";

/// 内核连接器模式自持的工具名，与 Python 侧 `omnicrawl/agent/toolkit/tools.py` 的常量一致。
/// 文件、shell、git、知识库与记忆等工具仍由宿主与插件侧提供，内核只挂不需要额外执行器的三个。
const TODO_TOOL_NAME: &str = "update_todos";
const ASK_USER_TOOL_NAME: &str = "ask_user";
const PAUSE_WORK_TOOL_NAME: &str = "pause_work";

/// 模型端点设置。连接器模式没有宿主来交接 `initialize`，因此只从环境变量取。
struct ModelSettings {
    model: String,
    base_url: String,
    api_key: String,
    system_prompt: String,
}

impl ModelSettings {
    fn from_env(env: &dyn Fn(&str) -> Option<String>) -> Result<ModelSettings, String> {
        let model = env("OMNICRAWL_MODEL")
            .or_else(|| env("OPENAI_MODEL"))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                "连接器模式需要模型名：请设置 OMNICRAWL_MODEL（或 OPENAI_MODEL）。".to_string()
            })?;
        let api_key = env("OPENAI_API_KEY")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "连接器模式需要凭据：请设置 OPENAI_API_KEY。".to_string())?;
        let base_url = env("OPENAI_BASE_URL")
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let system_prompt = env("OMNICRAWL_SYSTEM_PROMPT")
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());
        Ok(ModelSettings {
            model,
            base_url,
            api_key,
            system_prompt,
        })
    }
}

/// 内核侧模型端口：把循环给的上下文直接交给 Provider 运行时。
///
/// 增量不外发（连接器按整段文本发送），因此用空接收端；回合取消由 sink 之外的
/// 取消检查保证，模型请求前先看一次取消标志。
struct KernelReplySource {
    settings: Arc<ModelSettings>,
    cancel: Arc<AtomicBool>,
}

impl ReplySource for KernelReplySource {
    fn request_reply(&mut self, messages: &mut Vec<Value>) -> Result<AgentModelReply, LoopError> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(LoopError::Cancelled("回合已取消。".to_string()));
        }
        let conversation = conversation_from_openai_messages(messages);
        let identity = BTreeMap::new();
        let options = GenerationOptions::default();
        let tools: Vec<ToolSpec> = Vec::new();
        let endpoint = ChatEndpoint {
            base_url: self.settings.base_url.clone(),
            api_key: self.settings.api_key.clone(),
            user_agent: USER_AGENT.to_string(),
        };
        let runtime = OpenAiChatRuntime::new(endpoint);
        let input = ChatRequestInput {
            model: self.settings.model.as_str(),
            system_prompt: self.settings.system_prompt.as_str(),
            messages: &conversation,
            tools: &tools,
            options: &options,
            profile_request_timeout_seconds: options.request_timeout_seconds,
            prompt_cache_capable: false,
            prompt_cache_identity: &identity,
        };
        let mut sink = DiscardSink;
        let reply = runtime
            .run_turn(&input, &mut sink)
            .map_err(|error| LoopError::ReplySource(error.message))?;
        if reply.content.trim().is_empty() && reply.tool_calls.is_empty() {
            return Err(LoopError::ReplySource("模型返回了空响应。".to_string()));
        }
        let tool_calls = reply
            .tool_calls
            .iter()
            .map(|call| ToolCall {
                name: call.name.clone(),
                arguments: call.arguments.clone(),
                id: call.call_id.clone(),
                function_name: call.name.clone(),
            })
            .collect();
        Ok(AgentModelReply {
            message: json!({"role": "assistant", "content": reply.content}),
            content: reply.content,
            tool_calls,
            reasoning: reply.reasoning,
            content_streamed: false,
        })
    }
}

/// 工具端口：内核自持工具就地执行，其余一律回「不可用」观察。
struct UnavailableTools {
    shared: Arc<SharedState>,
}

impl ToolBatchHost for UnavailableTools {
    fn execute_tool_batch(
        &mut self,
        calls: &[ToolCall],
        _first_step: usize,
    ) -> Result<Vec<AgentLoopObservation>, LoopError> {
        Ok(calls.iter().map(|call| self.run_tool(call)).collect())
    }
}

impl UnavailableTools {
    fn run_tool(&self, call: &ToolCall) -> AgentLoopObservation {
        match call.name.as_str() {
            TODO_TOOL_NAME => self.update_todos(call),
            ASK_USER_TOOL_NAME => self.ask_user(call),
            PAUSE_WORK_TOOL_NAME => self.pause_work(call),
            _ => unavailable_observation(call),
        }
    }

    fn update_todos(&self, call: &ToolCall) -> AgentLoopObservation {
        let todos = call.arguments.get("todos").cloned().unwrap_or(Value::Null);
        let (ok, summary) = match &todos {
            Value::Array(items) => (true, format!("已更新 {} 项待办。", items.len())),
            _ => (false, "待办未更新：参数里没有 todos 数组。".to_string()),
        };
        if let Ok(mut slot) = self.shared.todos.lock() {
            *slot = Some(todos);
        }
        tool_observation(call, ok, summary, None)
    }

    fn ask_user(&self, call: &ToolCall) -> AgentLoopObservation {
        let handler = self
            .shared
            .ask_user
            .lock()
            .ok()
            .and_then(|slot| slot.clone());
        let Some(handler) = handler else {
            return tool_observation(
                call,
                false,
                "当前连接器没有绑定提问桥，无法向用户提问。".to_string(),
                Some("ask_user_unavailable"),
            );
        };
        let request = Value::Object(call.arguments.clone());
        match handler.ask(&request) {
            Some(answer) => tool_observation(call, true, answer, None),
            None => tool_observation(
                call,
                false,
                "提问没有取得答案（用户未回答或已超时）。".to_string(),
                Some("ask_user_no_answer"),
            ),
        }
    }

    fn pause_work(&self, call: &ToolCall) -> AgentLoopObservation {
        self.shared.paused.store(true, Ordering::SeqCst);
        tool_observation(
            call,
            true,
            "已请求暂停：本回合结束后不再自动继续。".to_string(),
            None,
        )
    }
}

fn unavailable_observation(call: &ToolCall) -> AgentLoopObservation {
    tool_observation(
        call,
        false,
        format!(
            "工具 {} 在当前模式下不可用：内核连接器进程只挂了内核自持工具（{}、{}、{}）。",
            call.name, TODO_TOOL_NAME, ASK_USER_TOOL_NAME, PAUSE_WORK_TOOL_NAME
        ),
        Some(TOOL_UNAVAILABLE),
    )
}

fn tool_observation(
    call: &ToolCall,
    ok: bool,
    output: String,
    error_code: Option<&str>,
) -> AgentLoopObservation {
    AgentLoopObservation {
        tool_call: call.clone(),
        result: ToolResult {
            ok,
            output: output.clone(),
            full_output: output.clone(),
            error_code: error_code.map(|value| value.to_string()),
            retryable: false,
        },
        message: json!({
            "role": "tool",
            "tool_call_id": call.id.clone(),
            "content": output,
        }),
        followup_messages: Vec::new(),
    }
}

/// 连接器宿主：状态、命令与回合驱动。
struct KernelDriver {
    settings: Arc<ModelSettings>,
    shared: Arc<SharedState>,
    workspace: Mutex<PathBuf>,
    history: Mutex<Vec<Value>>,
    session_id: Mutex<String>,
    cancel: Arc<AtomicBool>,
}

/// 工具执行器与宿主共享的状态：待办、暂停标志与两条处理桥。
struct SharedState {
    todos: Mutex<Option<Value>>,
    paused: AtomicBool,
    confirm: Mutex<Option<Arc<dyn ConfirmHandler>>>,
    ask_user: Mutex<Option<Arc<dyn AskUserHandler>>>,
}

impl SharedState {
    fn new() -> SharedState {
        SharedState {
            todos: Mutex::new(None),
            paused: AtomicBool::new(false),
            confirm: Mutex::new(None),
            ask_user: Mutex::new(None),
        }
    }
}

impl KernelDriver {
    fn new(settings: Arc<ModelSettings>, workspace: PathBuf) -> KernelDriver {
        KernelDriver {
            settings,
            shared: Arc::new(SharedState::new()),
            workspace: Mutex::new(workspace),
            history: Mutex::new(Vec::new()),
            session_id: Mutex::new(new_session_id()),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn workspace_root(&self) -> Result<PathBuf, String> {
        self.workspace
            .lock()
            .map(|value| value.clone())
            .map_err(|_| "工作区状态被占用。".to_string())
    }

    /// 换工作区即换会话：历史与标识一起重置，避免把旧工作区的上下文带过去。
    fn reset_session(&self) -> Result<(), String> {
        self.history
            .lock()
            .map_err(|_| "会话历史被占用。".to_string())?
            .clear();
        let mut session_id = self
            .session_id
            .lock()
            .map_err(|_| "会话标识被占用。".to_string())?;
        *session_id = new_session_id();
        Ok(())
    }
}

impl AgentDriver for KernelDriver {
    fn status(&self) -> AgentStatus {
        let workspace_root = self
            .workspace_root()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|detail| detail);
        let session_id = self
            .session_id
            .lock()
            .map(|value| value.clone())
            .unwrap_or_else(|_| "?".to_string());
        AgentStatus {
            workspace_root,
            session_id,
        }
    }

    fn reset_conversation(&self) -> Result<(), String> {
        self.reset_session()
    }

    fn switch_workspace(&self, path: &str) -> Result<WorkspaceSwitch, String> {
        let candidate = PathBuf::from(path);
        if !candidate.is_dir() {
            return Err(format!("工作区不存在或不是目录：{path}"));
        }
        let root = candidate
            .canonicalize()
            .map_err(|error| format!("工作区路径无法解析：{error}"))?;
        *self
            .workspace
            .lock()
            .map_err(|_| "工作区状态被占用。".to_string())? = root.clone();
        self.reset_session()?;
        Ok(WorkspaceSwitch {
            workspace_root: root.display().to_string(),
            note: "已切换工作区，会话历史已清空。".to_string(),
        })
    }

    fn handle_command(&self, text: &str, _channel: &str) -> Result<Option<String>, String> {
        let trimmed = text.trim();
        if let Some(rest) = trimmed.strip_prefix("/workspace") {
            let path = rest.trim();
            if path.is_empty() {
                return Ok(Some("用法：/workspace <路径>".to_string()));
            }
            let switched = self.switch_workspace(path)?;
            return Ok(Some(format!(
                "工作区已切换到 {}。{}",
                switched.workspace_root, switched.note
            )));
        }
        match trimmed {
            "/status" => {
                let status = self.status();
                Ok(Some(format!(
                    "工作区：{}\n会话：{}",
                    status.workspace_root, status.session_id
                )))
            }
            "/new" | "/reset" => {
                self.reset_session()?;
                Ok(Some("已开启新会话。".to_string()))
            }
            "/help" => Ok(Some(
                "/status 查看工作区与会话；/new 开启新会话；/workspace <路径> 切换工作区".to_string(),
            )),
            _ => Ok(None),
        }
    }

    fn run_turn(&self, text: &str, events: &mut dyn FnMut(TurnEvent)) -> Result<String, TurnError> {
        self.cancel.store(false, Ordering::SeqCst);
        self.shared.paused.store(false, Ordering::SeqCst);
        let mut messages = self
            .history
            .lock()
            .map_err(|_| TurnError::Failed("会话历史被占用。".to_string()))?
            .clone();
        messages.push(json!({"role": "user", "content": text}));

        let mut model = KernelReplySource {
            settings: Arc::clone(&self.settings),
            cancel: Arc::clone(&self.cancel),
        };
        let mut tools = UnavailableTools {
            shared: Arc::clone(&self.shared),
        };
        let runner = AgentLoopRunner::new(Box::new(SystemClock::new()));
        let mut cancel_check = || {
            if self.cancel.load(Ordering::SeqCst) {
                Err(LoopError::Cancelled("回合已取消。".to_string()))
            } else {
                Ok(())
            }
        };
        let guards = LoopGuards {
            cancel_check: Some(&mut cancel_check),
            stop_check: None,
        };
        let outcome = runner
            .run(
                &mut messages,
                &mut model,
                &mut tools,
                AgentLoopLimits::default(),
                guards,
            )
            .map_err(|error| match error {
                LoopError::Cancelled(_) => TurnError::Cancelled,
                other => TurnError::Failed(format!("{other:?}")),
            })?;

        if let Ok(mut history) = self.history.lock() {
            *history = messages;
        }

        (events)(TurnEvent::Delta(outcome.final_text.clone()));
        if let Some(todos) = self.shared.todos.lock().ok().and_then(|slot| slot.clone()) {
            (events)(TurnEvent::TodoUpdate { todos });
        }
        let paused = outcome.paused || self.shared.paused.load(Ordering::SeqCst);
        (events)(TurnEvent::Finished(TurnOutcome {
            final_text: outcome.final_text.clone(),
            reasoning: outcome.reasoning.clone(),
            model_turns: outcome.model_turns as u64,
            tool_calls: outcome.tool_calls as u64,
            paused,
        }));
        Ok(outcome.final_text)
    }

    fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    fn temp_root(&self) -> PathBuf {
        self.workspace_root()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".omnicrawl")
            .join(".agent_tmp")
    }

    fn set_confirm_handler(&self, handler: Arc<dyn ConfirmHandler>) {
        if let Ok(mut slot) = self.shared.confirm.lock() {
            *slot = Some(handler);
        }
    }

    fn set_ask_user_handler(&self, handler: Arc<dyn AskUserHandler>) {
        if let Ok(mut slot) = self.shared.ask_user.lock() {
            *slot = Some(handler);
        }
    }
}

fn new_session_id() -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or(0);
    // 同一毫秒内连续重置会话也要拿到不同标识，否则转录会串到同一个会话上。
    let sequence = SEQUENCE.fetch_add(1, Ordering::SeqCst);
    format!("kernel-{millis}-{sequence}")
}

/// 启动指定连接器并阻塞到退出。
pub fn run(name: &str) -> Result<(), String> {
    match name {
        "telegram" => run_telegram(),
        "feishu" => run_feishu(),
        other => Err(format!("未知连接器：{other}；当前支持 telegram、feishu。")),
    }
}

fn run_telegram() -> Result<(), String> {
    let env = |key: &str| std::env::var(key).ok();
    let settings = Arc::new(ModelSettings::from_env(&env)?);
    let config = load_telegram_config(&env, None)?;
    let workspace = std::env::current_dir().map_err(|error| error.to_string())?;
    let driver = Arc::new(KernelDriver::new(settings, workspace));
    let transport: Arc<dyn HttpTransport> = Arc::new(UreqTransport::new());
    let bot = Arc::new(TelegramBot::new(&config, transport, Arc::clone(&driver))?);
    eprintln!("[kernel] telegram 连接器已启动，等待平台消息。");
    bot.run().map_err(|error| error.to_string())
}

fn run_feishu() -> Result<(), String> {
    let env = |key: &str| std::env::var(key).ok();
    let settings = Arc::new(ModelSettings::from_env(&env)?);
    // [feishu] 配置段通常来自 config.toml；内核没有配置读取层，这里只认环境变量。
    let data = Value::Object(serde_json::Map::new());
    let config = load_feishu_config(ConfigSource {
        environment: &env,
        data: &data,
    })?;
    if check_config(&config).get("ready").and_then(Value::as_bool) != Some(true) {
        return Err(
            "飞书连接器还没有就绪：请设置 FEISHU_APP_ID 与 FEISHU_APP_SECRET（可选 FEISHU_ALLOWED_USER_IDS）。"
                .to_string(),
        );
    }
    let workspace = std::env::current_dir().map_err(|error| error.to_string())?;
    let driver = Arc::new(KernelDriver::new(settings, workspace));
    let api = Arc::new(FeishuApi::with_ureq(&config.app_id, &config.app_secret));
    let bot = Arc::new(FeishuBot::new(
        api,
        Arc::clone(&driver),
        config.allowed_user_ids.clone(),
        config.confirmation_timeout_seconds,
    ));
    eprintln!("[kernel] feishu 连接器已启动，等待平台消息。");
    bot.run().map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn driver(workspace: &str) -> KernelDriver {
        let settings = ModelSettings {
            model: "test-model".to_string(),
            base_url: "http://127.0.0.1:1/v1".to_string(),
            api_key: "test-key".to_string(),
            system_prompt: "test".to_string(),
        };
        KernelDriver::new(Arc::new(settings), PathBuf::from(workspace))
    }

    fn temp_workspace(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("omnicrawl-connector-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("测试工作区应能创建");
        root
    }

    #[test]
    fn status_reports_workspace_and_session() {
        let workspace = temp_workspace("status");
        let host = driver(workspace.to_str().unwrap());
        let status = host.status();
        assert!(status.workspace_root.contains("omnicrawl-connector-status"));
        assert!(status.session_id.starts_with("kernel-"));
    }

    #[test]
    fn command_returns_none_for_regular_text() {
        let workspace = temp_workspace("plain");
        let host = driver(workspace.to_str().unwrap());
        assert_eq!(host.handle_command("你好", "telegram").unwrap(), None);
        assert_eq!(host.handle_command("/unknown", "telegram").unwrap(), None);
    }

    #[test]
    fn new_command_rotates_session_id() {
        let workspace = temp_workspace("reset");
        let host = driver(workspace.to_str().unwrap());
        let before = host.status().session_id;
        let reply = host.handle_command("/new", "telegram").unwrap();
        assert_eq!(reply.as_deref(), Some("已开启新会话。"));
        assert_ne!(host.status().session_id, before);
    }

    #[test]
    fn workspace_command_switches_and_rejects_missing_dir() {
        let workspace = temp_workspace("switch");
        let other = temp_workspace("switch-target");
        let host = driver(workspace.to_str().unwrap());

        let reply = host
            .handle_command(&format!("/workspace {}", other.display()), "telegram")
            .unwrap()
            .expect("workspace 命令应有回执");
        assert!(reply.contains("switch-target"));
        assert!(host.status().workspace_root.contains("switch-target"));

        let missing = host
            .handle_command("/workspace /definitely/not/here", "telegram")
            .unwrap_err();
        assert!(missing.contains("工作区不存在"));
    }

    #[test]
    fn workspace_command_without_path_explains_usage() {
        let workspace = temp_workspace("usage");
        let host = driver(workspace.to_str().unwrap());
        let reply = host.handle_command("/workspace", "telegram").unwrap();
        assert_eq!(reply.as_deref(), Some("用法：/workspace <路径>"));
    }

    #[test]
    fn temp_root_lives_under_workspace() {
        let workspace = temp_workspace("temp");
        let host = driver(workspace.to_str().unwrap());
        let temp_root = host.temp_root();
        assert!(temp_root.starts_with(&workspace));
        assert!(temp_root.ends_with(".omnicrawl/.agent_tmp") || temp_root.ends_with(".omnicrawl\\.agent_tmp"));
    }

    fn tools_with(shared: Arc<SharedState>) -> UnavailableTools {
        UnavailableTools { shared }
    }

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            name: name.to_string(),
            arguments: arguments.as_object().cloned().unwrap_or_default(),
            id: "call-1".to_string(),
            function_name: name.to_string(),
        }
    }

    #[test]
    fn tool_batch_reports_unavailable_instead_of_silently_dropping() {
        let mut tools = tools_with(Arc::new(SharedState::new()));
        let calls = vec![call("read_file", json!({"path": "a.txt"}))];
        let observations = tools.execute_tool_batch(&calls, 1).expect("批次应可执行");
        assert_eq!(observations.len(), 1);
        assert_eq!(
            observations[0].result.error_code.as_deref(),
            Some(TOOL_UNAVAILABLE)
        );
        assert!(observations[0].result.output.contains("read_file"));
    }

    #[test]
    fn update_todos_records_its_state() {
        let shared = Arc::new(SharedState::new());
        let mut tools = tools_with(Arc::clone(&shared));
        let calls = vec![call(
            TODO_TOOL_NAME,
            json!({"todos": [{"id": "a", "step": "写测试", "completed": false}]}),
        )];
        let observations = tools.execute_tool_batch(&calls, 1).expect("批次应可执行");
        assert!(observations[0].result.ok);
        assert!(observations[0].result.output.contains("1 项待办"));
        let todos = shared.todos.lock().unwrap().clone().expect("待办应被记录");
        assert_eq!(todos.as_array().map(|items| items.len()), Some(1));
    }

    #[test]
    fn ask_user_uses_the_connector_bridge() {
        struct Fixed;
        impl AskUserHandler for Fixed {
            fn ask(&self, _request: &Value) -> Option<String> {
                Some("选 A".to_string())
            }
        }

        let shared = Arc::new(SharedState::new());
        *shared.ask_user.lock().unwrap() = Some(Arc::new(Fixed));
        let mut tools = tools_with(Arc::clone(&shared));
        let observations = tools
            .execute_tool_batch(&[call(ASK_USER_TOOL_NAME, json!({"question": "选哪个"}))], 1)
            .expect("批次应可执行");
        assert!(observations[0].result.ok);
        assert_eq!(observations[0].result.output, "选 A");
    }

    #[test]
    fn ask_user_without_bridge_reports_unavailable() {
        let mut tools = tools_with(Arc::new(SharedState::new()));
        let observations = tools
            .execute_tool_batch(&[call(ASK_USER_TOOL_NAME, json!({"question": "在吗"}))], 1)
            .expect("批次应可执行");
        assert!(!observations[0].result.ok);
        assert_eq!(
            observations[0].result.error_code.as_deref(),
            Some("ask_user_unavailable")
        );
    }

    #[test]
    fn pause_work_marks_the_shared_flag() {
        let shared = Arc::new(SharedState::new());
        let mut tools = tools_with(Arc::clone(&shared));
        let observations = tools
            .execute_tool_batch(&[call(PAUSE_WORK_TOOL_NAME, json!({}))], 1)
            .expect("批次应可执行");
        assert!(observations[0].result.ok);
        assert!(shared.paused.load(Ordering::SeqCst));
    }

    #[test]
    fn unreachable_model_endpoint_fails_the_turn_without_events() {
        let workspace = temp_workspace("offline");
        let host = driver(workspace.to_str().unwrap());
        let mut events = Vec::new();
        let result = host.run_turn("你好", &mut |event| events.push(event));
        assert!(matches!(result, Err(TurnError::Failed(_))));
        assert!(events.is_empty());
    }
}
