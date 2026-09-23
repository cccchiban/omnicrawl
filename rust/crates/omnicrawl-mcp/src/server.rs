//! 本地 stdio MCP Server（对应 `omnicrawl/mcp/server.py`）。
//!
//! 对外提供工作区只读文档（`project://`）、内置技术文档（`omnicrawl://docs/`）、
//! 健康检查与四个任务模板 Prompt；不暴露任何工作区工具——工具面留在 Host 一侧。
//! 二进制入口是 `omnicrawl-mcp-server`，工作区由 `MCP_WORKSPACE_ROOT` 给出。

use std::io::{BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::bundled::{
    bundled_doc_names, bundled_doc_uri, read_bundled_doc, BUNDLED_DOC_URI_PREFIX,
};
use crate::jsonrpc::{encode_frame, read_frame, FrameKind, McpClientError};

/// 与 Python 侧 `MAX_FILE_READ_CHARS` 一致：单份只读文档的上限。
pub const MAX_FILE_READ_CHARS: usize = 200_000;
/// 与 Python 侧同一份保护名单：这些路径不通过 MCP 暴露。
const PROTECTED_NAMES: [&str; 12] = [
    ".git",
    ".venv",
    "venv",
    "env",
    "__pycache__",
    ".codex-ref",
    ".env",
    "config.json",
    "config.toml",
    "models.toml",
    "config.yaml",
    "models.yaml",
];

/// Local MCP Server 参数校验或工具执行失败（对应 Python `LocalMCPServerError`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalMcpServerError {
    message: String,
}

impl LocalMcpServerError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

struct PromptArgument {
    name: &'static str,
    description: &'static str,
    required: bool,
}

struct PromptSpec {
    name: &'static str,
    description: &'static str,
    arguments: &'static [PromptArgument],
}

const DOC_TARGET: [PromptArgument; 2] = [
    PromptArgument {
        name: "target",
        description: "文档目标",
        required: true,
    },
    PromptArgument {
        name: "scope",
        description: "覆盖范围",
        required: false,
    },
];

const REVIEW_PATH: [PromptArgument; 2] = [
    PromptArgument {
        name: "path",
        description: "审查文件路径",
        required: true,
    },
    PromptArgument {
        name: "focus",
        description: "关注点",
        required: false,
    },
];

const TRIAGE_ERROR: [PromptArgument; 2] = [
    PromptArgument {
        name: "error",
        description: "错误日志或现象",
        required: true,
    },
    PromptArgument {
        name: "expected",
        description: "期望行为",
        required: false,
    },
];

const PLAN_GOAL: [PromptArgument; 2] = [
    PromptArgument {
        name: "goal",
        description: "变更目标",
        required: true,
    },
    PromptArgument {
        name: "constraints",
        description: "约束和回滚要求",
        required: false,
    },
];

const PROMPTS: [PromptSpec; 4] = [
    PromptSpec {
        name: "project_doc_writer",
        description: "编写项目技术文档的任务模板。",
        arguments: &DOC_TARGET,
    },
    PromptSpec {
        name: "code_review",
        description: "代码审查任务模板。",
        arguments: &REVIEW_PATH,
    },
    PromptSpec {
        name: "debug_triage",
        description: "排障分析任务模板。",
        arguments: &TRIAGE_ERROR,
    },
    PromptSpec {
        name: "safe_change_plan",
        description: "高风险改动前的安全方案模板。",
        arguments: &PLAN_GOAL,
    },
];

/// 提供本地 stdio MCP Server 的 Resource 和 Prompt 能力。
pub struct LocalMcpServer {
    workspace_root: PathBuf,
}

impl LocalMcpServer {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: normalize(&workspace_root.into()),
        }
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// 运行 stdio JSON-RPC 循环直到 stdin 关闭。
    pub fn run_stdio(&self) -> std::io::Result<()> {
        let stdin = std::io::stdin();
        let mut reader = BufReader::new(stdin.lock());
        let mut stdout = std::io::stdout().lock();
        loop {
            let message = match read_frame(&mut reader, FrameKind::Request, String::new) {
                Ok(Some(message)) => message,
                Ok(None) => return Ok(()),
                Err(error) => {
                    // 帧坏了没法回一个合法的 JSON-RPC 响应：写到 stderr 便于诊断，继续读取。
                    eprintln!("[mcp-server] {}", error.message());
                    return Ok(());
                }
            };
            if let Some(response) = self.handle_message(&message) {
                stdout.write_all(&encode_frame(&response))?;
                stdout.flush()?;
            }
        }
    }

    /// 处理一条 JSON-RPC 消息；通知与无 id 的请求返回 `None`。
    pub fn handle_message(&self, message: &Map<String, Value>) -> Option<Value> {
        let method = message.get("method").cloned();
        let request_id = message.get("id").cloned();

        let result = match method.as_ref().and_then(|value| value.as_str()) {
            Some("initialize") => Ok(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}, "resources": {}, "prompts": {}},
                "serverInfo": {"name": "ai-voice-agent-local", "version": "0.1"},
            })),
            Some("notifications/initialized") => return None,
            Some("tools/list") => Ok(self.list_tools()),
            Some("tools/call") => self.call_tool(message),
            Some("resources/list") => Ok(self.list_resources()),
            Some("resources/read") => self.read_resource(message),
            Some("prompts/list") => Ok(self.list_prompts()),
            Some("prompts/get") => self.get_prompt(message),
            _ => {
                return Some(jsonrpc_error(
                    request_id,
                    -32601,
                    format!("未知 MCP 方法：{}", py_str(method.as_ref())),
                ))
            }
        };

        let result = match result {
            Ok(value) => value,
            // Python 侧错误响应不区分有没有 id：一律回错误对象。
            Err(error) => {
                return Some(jsonrpc_error(
                    request_id,
                    -32000,
                    error.message().to_string(),
                ))
            }
        };
        request_id.map(|request_id| json!({"jsonrpc": "2.0", "id": request_id, "result": result}))
    }

    /// Local MCP Server 不再暴露工作区工具。
    fn list_tools(&self) -> Value {
        json!({"tools": []})
    }

    fn call_tool(&self, message: &Map<String, Value>) -> Result<Value, LocalMcpServerError> {
        let params = read_params(message)?;
        let name = match params.get("name") {
            Some(Value::String(text)) if !text.is_empty() => text.clone(),
            _ => {
                return Err(LocalMcpServerError::new(
                    "tools/call.name 必须是非空字符串。",
                ))
            }
        };
        match params.get("arguments") {
            None | Some(Value::Object(_)) => {}
            _ => {
                return Err(LocalMcpServerError::new(
                    "tools/call.arguments 必须是 JSON 对象。",
                ))
            }
        }
        Err(LocalMcpServerError::new(format!("未知工具：{name}")))
    }

    fn list_resources(&self) -> Value {
        let mut resources: Vec<Value> = vec![
            json!({
                "uri": "project://agents-instructions",
                "name": "AGENTS.md",
                "description": "项目协作规范。",
                "mimeType": "text/markdown",
            }),
            json!({
                "uri": "server://local_project/health",
                "name": "Local MCP Server Health",
                "description": "本地 MCP Server 健康状态。",
                "mimeType": "text/plain",
            }),
        ];

        let mut document_paths: Vec<String> = vec!["README.md".to_string()];
        let docs_dir = self.workspace_root.join("docs");
        if docs_dir.is_dir() && !self.should_skip_path(&docs_dir) {
            let mut names: Vec<String> = std::fs::read_dir(&docs_dir)
                .map(|entries| {
                    entries
                        .filter_map(|entry| entry.ok())
                        .filter(|entry| entry.path().is_file())
                        .map(|entry| entry.file_name().to_string_lossy().to_string())
                        .filter(|name| is_markdown(name))
                        .filter(|name| !self.should_skip_path(&docs_dir.join(name)))
                        .collect()
                })
                .unwrap_or_default();
            names.sort_by_key(|name| name.to_lowercase());
            document_paths.extend(names.into_iter().map(|name| format!("docs/{name}")));
        }

        for relative in dedupe(document_paths) {
            let path = self.workspace_root.join(&relative);
            if path.is_file() && !self.should_skip_path(&path) {
                resources.push(json!({
                    "uri": format!("project://{relative}"),
                    "name": relative,
                    "description": "项目只读文档。",
                    "mimeType": "text/markdown",
                }));
            }
        }

        for name in bundled_doc_names() {
            resources.push(json!({
                "uri": bundled_doc_uri(name).unwrap_or_default(),
                "name": format!("omnicrawl/docs/{name}"),
                "description": "随 OmniCrawl 安装包提供的只读技术文档。",
                "mimeType": "text/markdown",
            }));
        }
        json!({"resources": resources})
    }

    fn read_resource(&self, message: &Map<String, Value>) -> Result<Value, LocalMcpServerError> {
        let params = read_params(message)?;
        let uri = match params.get("uri") {
            Some(Value::String(text)) if !text.trim().is_empty() => text.trim().to_string(),
            _ => {
                return Err(LocalMcpServerError::new(
                    "resources/read.uri 必须是非空字符串。",
                ))
            }
        };

        let text = if uri == "server://local_project/health" {
            format!("ok\nworkspace_root={}\n", self.workspace_root.display())
        } else if uri == "project://agents-instructions" {
            self.read_project_text("AGENTS.md")?
        } else if let Some(relative) = uri.strip_prefix("project://") {
            self.read_project_text(relative)?
        } else if uri.starts_with(BUNDLED_DOC_URI_PREFIX) {
            read_bundled_doc(&uri)
                .map_err(|error| LocalMcpServerError::new(error.message().to_string()))?
                .to_string()
        } else {
            return Err(LocalMcpServerError::new(format!(
                "不支持的 Resource URI：{uri}"
            )));
        };

        Ok(json!({
            "contents": [{"uri": uri, "mimeType": "text/markdown", "text": text}],
        }))
    }

    fn list_prompts(&self) -> Value {
        let prompts: Vec<Value> = PROMPTS
            .iter()
            .map(|spec| {
                json!({
                    "name": spec.name,
                    "description": spec.description,
                    "arguments": spec
                        .arguments
                        .iter()
                        .map(|argument| json!({
                            "name": argument.name,
                            "description": argument.description,
                            "required": argument.required,
                        }))
                        .collect::<Vec<Value>>(),
                })
            })
            .collect();
        json!({"prompts": prompts})
    }

    fn get_prompt(&self, message: &Map<String, Value>) -> Result<Value, LocalMcpServerError> {
        let params = read_params(message)?;
        let name = match params.get("name") {
            Some(Value::String(text)) if !text.is_empty() => text.clone(),
            _ => {
                return Err(LocalMcpServerError::new(
                    "prompts/get.name 必须是非空字符串。",
                ))
            }
        };
        let arguments = match params.get("arguments") {
            None => Map::new(),
            Some(Value::Object(map)) => map.clone(),
            Some(_) => {
                return Err(LocalMcpServerError::new(
                    "prompts/get.arguments 必须是 JSON 对象。",
                ))
            }
        };
        let Some(spec) = PROMPTS.iter().find(|spec| spec.name == name) else {
            return Err(LocalMcpServerError::new(format!("未知 Prompt：{name}")));
        };

        Ok(json!({
            "description": spec.description,
            "messages": [{
                "role": "user",
                "content": {"type": "text", "text": render_prompt(&name, &arguments)},
            }],
        }))
    }

    fn read_project_text(&self, raw_path: &str) -> Result<String, LocalMcpServerError> {
        let path = self.safe_path(raw_path)?;
        if !path.is_file() {
            return Err(LocalMcpServerError::new(format!(
                "不是文件：{}",
                self.relative_path(&path)
            )));
        }
        let text = std::fs::read(&path)
            .map_err(|error| {
                LocalMcpServerError::new(format!(
                    "读取文件失败：{}，{error}",
                    self.relative_path(&path)
                ))
            })
            .and_then(|bytes| {
                String::from_utf8(bytes).map_err(|_| {
                    LocalMcpServerError::new(format!(
                        "文件不是 UTF-8 文本或包含二进制内容：{}",
                        self.relative_path(&path)
                    ))
                })
            })?;
        if text.chars().count() > MAX_FILE_READ_CHARS {
            let head: String = text.chars().take(MAX_FILE_READ_CHARS).collect();
            return Ok(format!("{head}\n... 文件内容已截断。"));
        }
        Ok(text)
    }

    fn safe_path(&self, raw_path: &str) -> Result<PathBuf, LocalMcpServerError> {
        let raw_path = raw_path.trim();
        if raw_path.is_empty() {
            return Err(LocalMcpServerError::new("路径不能为空。"));
        }
        let candidate = PathBuf::from(raw_path);
        let candidate = if candidate.is_absolute() {
            candidate
        } else {
            self.workspace_root.join(candidate)
        };
        let resolved = normalize(&candidate);
        if is_protected_path(&resolved) {
            return Err(LocalMcpServerError::new(format!(
                "拒绝访问受保护路径：{}",
                self.relative_path(&resolved)
            )));
        }
        Ok(resolved)
    }

    fn relative_path(&self, path: &Path) -> String {
        match path.strip_prefix(&self.workspace_root) {
            Ok(relative) if relative.as_os_str().is_empty() => ".".to_string(),
            Ok(relative) => relative.to_string_lossy().to_string(),
            Err(_) => path.to_string_lossy().to_string(),
        }
    }

    fn should_skip_path(&self, path: &Path) -> bool {
        is_protected_path(path)
    }
}

/// 保护名单与 Python `PROTECTED_NAMES` 一致，另加 `.env.*` 前缀规则。
fn is_protected_path(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(name) => {
            let name = name.to_string_lossy();
            PROTECTED_NAMES.contains(&name.as_ref()) || name.starts_with(".env.")
        }
        _ => false,
    })
}

fn is_markdown(name: &str) -> bool {
    if cfg!(windows) {
        // Python 的 `glob("*.md")` 在 Windows 上大小写不敏感。
        name.to_lowercase().ends_with(".md")
    } else {
        name.ends_with(".md")
    }
}

/// 与 Python 的 `dict.fromkeys(...)` 一致：去重并保留首次出现顺序。
fn dedupe(values: Vec<String>) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    for value in values {
        if !result.contains(&value) {
            result.push(value);
        }
    }
    result
}

fn read_params(message: &Map<String, Value>) -> Result<Map<String, Value>, LocalMcpServerError> {
    match message.get("params") {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(map)) => Ok(map.clone()),
        Some(_) => Err(LocalMcpServerError::new("JSON-RPC params 必须是对象。")),
    }
}

fn render_prompt(name: &str, arguments: &Map<String, Value>) -> String {
    // Python 模板对缺失参数取空串（`arguments.get('target', '')`），显式 null 渲染成 None。
    let text = |key: &str| match arguments.get(key) {
        None => String::new(),
        Some(value) => py_str(Some(value)),
    };
    match name {
        "project_doc_writer" => format!(
            "请编写项目技术文档。\n目标：{}\n范围：{}\n要求：先说明结论，再覆盖关键模块、数据流、边界条件和验证方式。",
            text("target"),
            text("scope")
        ),
        "code_review" => format!(
            "请进行代码审查，优先指出 bug、回归风险和缺失测试。\n文件：{}\n关注点：{}",
            text("path"),
            text("focus")
        ),
        "debug_triage" => format!(
            "请进行排障分析。\n错误现象：{}\n期望行为：{}\n要求：给出最可能原因、验证步骤和最小修复路径。",
            text("error"),
            text("expected")
        ),
        "safe_change_plan" => format!(
            "请为高风险改动制定安全方案。\n目标：{}\n约束：{}\n要求：覆盖影响面、执行步骤、验证方式和回滚思路。",
            text("goal"),
            text("constraints")
        ),
        _ => format!("未知 Prompt：{name}"),
    }
}

/// Python `str(value)` 的可用子集：缺失值渲染成 `None`，字符串原样。
fn py_str(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => omnicrawl_controllers::json::python_repr(other),
    }
}

fn jsonrpc_error(request_id: Option<Value>, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": request_id.unwrap_or(Value::Null),
        "error": {"code": code, "message": message},
    })
}

/// 规范化路径（Python 侧的 `Path.resolve()`；不要求目标存在，也不跟随符号链接）。
fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

/// 读取 stdin 一帧（供二进制入口与集成测试共用）。
pub fn read_request(
    reader: &mut impl BufRead,
) -> Result<Option<Map<String, Value>>, McpClientError> {
    read_frame(reader, FrameKind::Request, String::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(name: &str) -> (LocalMcpServer, PathBuf) {
        let root = std::env::temp_dir().join(format!("omnicrawl-mcp-server-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("docs")).expect("创建临时工作区");
        std::fs::write(root.join("README.md"), "# 项目\n").expect("写 README");
        std::fs::write(root.join("AGENTS.md"), "协作规范\n").expect("写 AGENTS.md");
        std::fs::write(root.join("docs").join("API.md"), "接口\n").expect("写文档");
        std::fs::write(root.join("config.toml"), "secret=1\n").expect("写受保护文件");
        (LocalMcpServer::new(&root), root)
    }

    fn call(server: &LocalMcpServer, message: Value) -> Value {
        server
            .handle_message(message.as_object().expect("请求必须是对象"))
            .expect("请求应当有响应")
    }

    #[test]
    fn initialize_reports_protocol_and_capabilities() {
        let (server, _root) = server("initialize");
        let response = call(
            &server,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}),
        );
        assert_eq!(response["result"]["protocolVersion"], json!("2024-11-05"));
        assert_eq!(
            response["result"]["serverInfo"]["name"],
            json!("ai-voice-agent-local")
        );
    }

    #[test]
    fn notifications_get_no_response() {
        let (server, _root) = server("notification");
        let response = server.handle_message(
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
                .as_object()
                .expect("对象"),
        );
        assert!(response.is_none());
    }

    #[test]
    fn resources_list_includes_project_and_bundled_docs() {
        let (server, root) = server("resources");
        let response = call(
            &server,
            json!({"jsonrpc": "2.0", "id": 1, "method": "resources/list"}),
        );
        let resources = response["result"]["resources"].as_array().expect("资源表");
        let uris: Vec<String> = resources
            .iter()
            .map(|item| item["uri"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(uris[0], "project://agents-instructions");
        assert_eq!(uris[1], "server://local_project/health");
        assert!(
            uris.contains(&"project://README.md".to_string()),
            "{uris:?}"
        );
        assert!(
            uris.contains(&"project://docs/API.md".to_string()),
            "{uris:?}"
        );
        assert!(
            uris.contains(&"omnicrawl://docs/API.md".to_string()),
            "{uris:?}"
        );
        // 受保护文件不进资源表。
        assert!(!uris.iter().any(|uri| uri.contains("config.toml")));

        let health = call(
            &server,
            json!({"jsonrpc": "2.0", "id": 2, "method": "resources/read", "params": {"uri": "server://local_project/health"}}),
        );
        assert_eq!(
            health["result"]["contents"][0]["text"],
            json!(format!("ok\nworkspace_root={}\n", root.display()))
        );
    }

    #[test]
    fn protected_and_unknown_uris_are_rejected() {
        let (server, _root) = server("protected");
        let protected = call(
            &server,
            json!({"jsonrpc": "2.0", "id": 1, "method": "resources/read", "params": {"uri": "project://config.toml"}}),
        );
        assert!(
            protected["error"]["message"]
                .as_str()
                .expect("错误文案")
                .starts_with("拒绝访问受保护路径："),
            "{protected}"
        );

        let unknown = call(
            &server,
            json!({"jsonrpc": "2.0", "id": 2, "method": "resources/read", "params": {"uri": "file:///etc/passwd"}}),
        );
        assert_eq!(
            unknown["error"]["message"],
            json!("不支持的 Resource URI：file:///etc/passwd")
        );
    }

    #[test]
    fn prompts_list_and_render_templates() {
        let (server, _root) = server("prompts");
        let listed = call(
            &server,
            json!({"jsonrpc": "2.0", "id": 1, "method": "prompts/list"}),
        );
        assert_eq!(
            listed["result"]["prompts"]
                .as_array()
                .expect("Prompt 表")
                .len(),
            4
        );

        let rendered = call(
            &server,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "prompts/get",
                "params": {"name": "debug_triage", "arguments": {"error": "崩了"}},
            }),
        );
        let text = rendered["result"]["messages"][0]["content"]["text"]
            .as_str()
            .expect("Prompt 文本");
        assert_eq!(
            text,
            "请进行排障分析。\n错误现象：崩了\n期望行为：\n要求：给出最可能原因、验证步骤和最小修复路径。"
        );
    }

    #[test]
    fn unknown_method_and_tool_report_jsonrpc_errors() {
        let (server, _root) = server("unknown");
        let method = call(
            &server,
            json!({"jsonrpc": "2.0", "id": 1, "method": "does/not/exist"}),
        );
        assert_eq!(method["error"]["code"], json!(-32601));
        assert_eq!(
            method["error"]["message"],
            json!("未知 MCP 方法：does/not/exist")
        );

        let missing = call(&server, json!({"jsonrpc": "2.0", "id": 2}));
        assert_eq!(missing["error"]["message"], json!("未知 MCP 方法：None"));

        let tool = call(
            &server,
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "read"}}),
        );
        assert_eq!(tool["error"]["code"], json!(-32000));
        assert_eq!(tool["error"]["message"], json!("未知工具：read"));
    }

    #[test]
    fn oversized_documents_are_truncated() {
        let (server, root) = server("truncate");
        let long = "字".repeat(MAX_FILE_READ_CHARS + 10);
        std::fs::write(root.join("README.md"), &long).expect("写长文档");
        let response = call(
            &server,
            json!({"jsonrpc": "2.0", "id": 1, "method": "resources/read", "params": {"uri": "project://README.md"}}),
        );
        let text = response["result"]["contents"][0]["text"]
            .as_str()
            .expect("文档文本");
        assert!(text.ends_with("\n... 文件内容已截断。"));
        assert_eq!(
            text.chars().count(),
            MAX_FILE_READ_CHARS + "\n... 文件内容已截断。".chars().count()
        );
    }
}
