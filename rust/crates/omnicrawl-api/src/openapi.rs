//! `/openapi.json` 与 `/docs`：FastAPI 自动文档的 Rust 等价实现。
//!
//! Python 侧由 FastAPI 从路由签名与 Pydantic 模型自动生成；Rust 用 axum 没有这套反射，
//! 因此这里按同一份路由清单手工构造 OpenAPI 3.1 文档——路径、方法、标签、路径/查询参数
//! 与请求体模型逐条对照 `omnicrawl/api/routes/*`，成功响应统一为 `{"data": ...}` 信封。
//!
//! 两个端点都在鉴权层之外（与 `/health` 同层），契约见 `omnicrawl/docs/API.md`：
//! 「除 `/health`、`/docs` 和 `/openapi.json` 外，请求必须携带 Bearer Token」。
//!
//! 所有对象/数组都显式构造，`json!` 只用于字面量结构——契约里嵌套了函数调用与
//! `format!`，靠 `json!` 的表达式解析容易踩坑，显式构造更稳。

use serde_json::{json, Map, Value};

/// 文档元信息，与 Python `create_app` 的 `FastAPI(...)` 参数一致。
const TITLE: &str = "OmniCrawl Local API";
const VERSION: &str = "1.0.0";
const DESCRIPTION: &str = "OmniCrawl 本地 Agent 的 HTTP/SSE 接口。";

/// 机器可读契约（OpenAPI 3.1）。
pub fn document() -> Value {
    let mut root = Map::new();
    root.insert("openapi".to_string(), json!("3.1.0"));
    root.insert("info".to_string(), Value::Object(info()));
    root.insert("paths".to_string(), Value::Object(paths()));
    root.insert("components".to_string(), Value::Object(components()));
    Value::Object(root)
}

/// Swagger UI 页面；从 CDN 加载资源并指向本服务的 `/openapi.json`。
pub fn docs_html() -> &'static str {
    DOCS_HTML
}

const DOCS_HTML: &str = r##"<!DOCTYPE html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8"/>
  <meta name="viewport" content="width=device-width, initial-scale=1"/>
  <title>OmniCrawl Local API - Swagger UI</title>
  <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/swagger-ui-dist@5/swagger-ui.css"/>
</head>
<body>
  <div id="swagger-ui"></div>
  <script src="https://cdn.jsdelivr.net/npm/swagger-ui-dist@5/swagger-ui-bundle.js"></script>
  <script>
    window.ui = SwaggerUIBundle({
      url: "/openapi.json",
      dom_id: "#swagger-ui",
      deepLinking: true,
      presets: [SwaggerUIBundle.presets.apis],
    });
  </script>
</body>
</html>
"##;

fn info() -> Map<String, Value> {
    let mut info = Map::new();
    info.insert("title".to_string(), json!(TITLE));
    info.insert("version".to_string(), json!(VERSION));
    info.insert("description".to_string(), json!(DESCRIPTION));
    info
}

fn components() -> Map<String, Value> {
    let mut components = Map::new();
    components.insert("schemas".to_string(), schemas());
    components
}

/// 全部路径与操作；插入顺序即文档展示顺序（`serde_json` 开了保序）。
fn paths() -> Map<String, Value> {
    let mut paths = Map::new();

    paths.insert(
        "/health".to_string(),
        methods(&[("get", op("system", "无鉴权健康检查", "200", None, &[], &[]))]),
    );
    paths.insert(
        "/api/v1/runtime".to_string(),
        methods(&[(
            "get",
            op(
                "system",
                "读取工作区、会话、模型与活动任务",
                "200",
                None,
                &[],
                &[],
            ),
        )]),
    );

    paths.insert(
        "/api/v1/runs".to_string(),
        methods(&[(
            "post",
            op("runs", "提交生成任务", "202", Some("RunRequest"), &[], &[]),
        )]),
    );
    paths.insert(
        "/api/v1/runs/{run_id}".to_string(),
        methods(&[(
            "get",
            op("runs", "查询任务状态", "200", None, &["run_id"], &[]),
        )]),
    );
    paths.insert(
        "/api/v1/runs/{run_id}/events".to_string(),
        methods(&[(
            "get",
            op(
                "runs",
                "SSE 任务事件流",
                "200",
                None,
                &["run_id"],
                &[("follow", "boolean", false)],
            ),
        )]),
    );
    paths.insert(
        "/api/v1/runs/{run_id}/cancel".to_string(),
        methods(&[(
            "post",
            op("runs", "请求取消任务", "200", None, &["run_id"], &[]),
        )]),
    );
    paths.insert(
        "/api/v1/runs/{run_id}/confirmations/{confirmation_id}".to_string(),
        methods(&[(
            "post",
            op(
                "runs",
                "提交工具审批决议",
                "200",
                Some("ConfirmationDecision"),
                &["run_id", "confirmation_id"],
                &[],
            ),
        )]),
    );
    paths.insert(
        "/api/v1/runs/{run_id}/questions/{question_id}".to_string(),
        methods(&[(
            "post",
            op(
                "runs",
                "回答 ask_user 提问",
                "200",
                Some("UserQuestionAnswer"),
                &["run_id", "question_id"],
                &[],
            ),
        )]),
    );

    paths.insert(
        "/api/v1/monitors".to_string(),
        methods(&[("get", op("monitors", "列出后台任务", "200", None, &[], &[]))]),
    );
    paths.insert(
        "/api/v1/monitors/{monitor_id}".to_string(),
        methods(&[(
            "get",
            op(
                "monitors",
                "查询后台任务状态",
                "200",
                None,
                &["monitor_id"],
                &[],
            ),
        )]),
    );
    paths.insert(
        "/api/v1/monitors/{monitor_id}/events".to_string(),
        methods(&[(
            "get",
            op(
                "monitors",
                "后台任务日志 SSE",
                "200",
                None,
                &["monitor_id"],
                &[
                    ("follow", "boolean", false),
                    ("max_events", "integer", false),
                    ("cursor", "integer", false),
                ],
            ),
        )]),
    );

    paths.insert(
        "/api/v1/subagents/events".to_string(),
        methods(&[(
            "get",
            op(
                "subagents",
                "当前会话后台任务/审批 SSE",
                "200",
                None,
                &[],
                &[("follow", "boolean", false)],
            ),
        )]),
    );
    paths.insert(
        "/api/v1/subagents".to_string(),
        methods(&[(
            "get",
            op(
                "subagents",
                "列出当前会话可见的后台任务",
                "200",
                None,
                &[],
                &[],
            ),
        )]),
    );
    paths.insert(
        "/api/v1/subagents/{task_id}".to_string(),
        methods(&[(
            "get",
            op(
                "subagents",
                "查询单个后台任务",
                "200",
                None,
                &["task_id"],
                &[],
            ),
        )]),
    );
    paths.insert(
        "/api/v1/subagents/{task_id}/cancel".to_string(),
        methods(&[(
            "post",
            op(
                "subagents",
                "请求取消后台任务",
                "200",
                None,
                &["task_id"],
                &[],
            ),
        )]),
    );

    paths.insert(
        "/api/v1/sessions".to_string(),
        methods(&[
            (
                "get",
                op(
                    "sessions",
                    "列出会话",
                    "200",
                    None,
                    &[],
                    &[("limit", "integer", false), ("archived", "boolean", false)],
                ),
            ),
            ("post", op("sessions", "新建会话", "200", None, &[], &[])),
        ]),
    );
    paths.insert(
        "/api/v1/sessions/diagnostics".to_string(),
        methods(&[(
            "get",
            op("sessions", "提示历史损坏诊断总览", "200", None, &[], &[]),
        )]),
    );
    paths.insert(
        "/api/v1/sessions/{session_id}/diagnostics".to_string(),
        methods(&[(
            "get",
            op(
                "sessions",
                "指定会话的诊断",
                "200",
                None,
                &["session_id"],
                &[],
            ),
        )]),
    );
    paths.insert(
        "/api/v1/sessions/{session_id}/events".to_string(),
        methods(&[(
            "get",
            op(
                "sessions",
                "读取会话事件",
                "200",
                None,
                &["session_id"],
                &[],
            ),
        )]),
    );
    paths.insert(
        "/api/v1/sessions/{session_id}/resume".to_string(),
        methods(&[(
            "post",
            op("sessions", "恢复会话", "200", None, &["session_id"], &[]),
        )]),
    );
    paths.insert(
        "/api/v1/sessions/current".to_string(),
        methods(&[(
            "patch",
            op(
                "sessions",
                "重命名当前会话",
                "200",
                Some("RenameRequest"),
                &[],
                &[],
            ),
        )]),
    );
    paths.insert(
        "/api/v1/sessions/current/compact".to_string(),
        methods(&[(
            "post",
            op("sessions", "压缩当前会话", "200", None, &[], &[]),
        )]),
    );
    paths.insert(
        "/api/v1/sessions/current/archive".to_string(),
        methods(&[(
            "post",
            op("sessions", "归档当前会话", "200", None, &[], &[]),
        )]),
    );
    paths.insert(
        "/api/v1/sessions/{session_id}".to_string(),
        methods(&[(
            "delete",
            op(
                "sessions",
                "删除非活动会话",
                "200",
                None,
                &["session_id"],
                &[],
            ),
        )]),
    );
    paths.insert(
        "/api/v1/sessions/current/export".to_string(),
        methods(&[(
            "post",
            op(
                "sessions",
                "导出当前会话 Markdown",
                "200",
                Some("ExportRequest"),
                &[],
                &[],
            ),
        )]),
    );
    paths.insert(
        "/api/v1/sessions/{session_id}/artifacts/{artifact_path}".to_string(),
        methods(&[(
            "get",
            op(
                "sessions",
                "读取会话 HTML artifact",
                "200",
                None,
                &["session_id", "artifact_path"],
                &[],
            ),
        )]),
    );

    paths.insert(
        "/api/v1/projects".to_string(),
        methods(&[
            ("get", op("projects", "列出项目", "200", None, &[], &[])),
            (
                "post",
                op(
                    "projects",
                    "创建项目",
                    "200",
                    Some("ProjectRequest"),
                    &[],
                    &[],
                ),
            ),
            (
                "patch",
                op(
                    "projects",
                    "重命名项目",
                    "200",
                    Some("ProjectRenameRequest"),
                    &[],
                    &[],
                ),
            ),
            (
                "delete",
                op(
                    "projects",
                    "移除项目记录",
                    "200",
                    None,
                    &[],
                    &[("path", "string", true)],
                ),
            ),
        ]),
    );
    paths.insert(
        "/api/v1/projects/overview".to_string(),
        methods(&[("get", op("projects", "项目总览", "200", None, &[], &[]))]),
    );
    paths.insert(
        "/api/v1/projects/import".to_string(),
        methods(&[(
            "post",
            op(
                "projects",
                "导入已有项目",
                "200",
                Some("ProjectRequest"),
                &[],
                &[],
            ),
        )]),
    );
    paths.insert(
        "/api/v1/projects/pin".to_string(),
        methods(&[(
            "post",
            op(
                "projects",
                "设置置顶状态",
                "200",
                Some("ProjectPinRequest"),
                &[],
                &[],
            ),
        )]),
    );
    paths.insert(
        "/api/v1/projects/switch".to_string(),
        methods(&[(
            "post",
            op(
                "projects",
                "切换 Agent 工作区",
                "200",
                Some("ProjectPathRequest"),
                &[],
                &[],
            ),
        )]),
    );

    paths.insert(
        "/api/v1/models".to_string(),
        methods(&[(
            "get",
            op(
                "configuration",
                "模型列表（兼容旧扁平形状）",
                "200",
                None,
                &[],
                &[],
            ),
        )]),
    );
    paths.insert(
        "/api/v1/models/catalog".to_string(),
        methods(&[(
            "get",
            op("configuration", "双列模型目录", "200", None, &[], &[]),
        )]),
    );
    paths.insert(
        "/api/v1/models/refresh".to_string(),
        methods(&[(
            "post",
            op(
                "configuration",
                "强制刷新模型发现缓存",
                "200",
                None,
                &[],
                &[],
            ),
        )]),
    );
    paths.insert(
        "/api/v1/models/current".to_string(),
        methods(&[(
            "put",
            op(
                "configuration",
                "切换并保存当前模型",
                "200",
                Some("ModelChangeRequest"),
                &[],
                &[],
            ),
        )]),
    );
    paths.insert(
        "/api/v1/reasoning".to_string(),
        methods(&[(
            "put",
            op(
                "configuration",
                "切换并保存推理强度",
                "200",
                Some("ReasoningChangeRequest"),
                &[],
                &[],
            ),
        )]),
    );
    paths.insert(
        "/api/v1/approval".to_string(),
        methods(&[(
            "put",
            op(
                "configuration",
                "切换并保存审批模式",
                "200",
                Some("ApprovalChangeRequest"),
                &[],
                &[],
            ),
        )]),
    );

    paths.insert(
        "/api/v1/history".to_string(),
        methods(&[(
            "get",
            op(
                "support",
                "查询 Prompt 历史",
                "200",
                None,
                &[],
                &[
                    ("query", "string", false),
                    ("limit", "integer", false),
                    ("current_session_only", "boolean", false),
                ],
            ),
        )]),
    );
    paths.insert(
        "/api/v1/skills".to_string(),
        methods(&[("get", op("support", "Skill 列表", "200", None, &[], &[]))]),
    );
    paths.insert(
        "/api/v1/mcp".to_string(),
        methods(&[("get", op("support", "MCP 状态", "200", None, &[], &[]))]),
    );
    paths.insert(
        "/api/v1/memory/clean".to_string(),
        methods(&[("post", op("support", "清理过期记忆", "200", None, &[], &[]))]),
    );

    paths.insert(
        "/api/v1/settings".to_string(),
        methods(&[(
            "get",
            op("settings", "全量只读设置快照", "200", None, &[], &[]),
        )]),
    );
    for (path, summary, schema) in settings_writes() {
        paths.insert(
            path.to_string(),
            methods(&[(
                "put",
                op("settings", summary, "200", Some(schema), &[], &[]),
            )]),
        );
    }

    paths
}

/// 一个路径下的若干方法。
fn methods(entries: &[(&str, Value)]) -> Value {
    let mut map = Map::new();
    for (method, operation) in entries {
        map.insert((*method).to_string(), operation.clone());
    }
    Value::Object(map)
}

/// `PUT /settings/<domain>` 的路径、摘要与请求体模型。
fn settings_writes() -> [(&'static str, &'static str, &'static str); 11] {
    [
        (
            "/api/v1/settings/context",
            "更新上下文窗口",
            "ContextWindowSetting",
        ),
        (
            "/api/v1/settings/context_compaction",
            "更新上下文压缩触发比例",
            "ContextCompactionSetting",
        ),
        (
            "/api/v1/settings/show_thinking",
            "更新是否展示思考区",
            "BoolSetting",
        ),
        (
            "/api/v1/settings/features",
            "更新功能开关",
            "FeaturesSetting",
        ),
        (
            "/api/v1/settings/run_guard",
            "更新持续运转配置",
            "RunGuardSetting",
        ),
        (
            "/api/v1/settings/agent_workspace",
            "更新隔离工作区配置",
            "AgentWorkspaceSetting",
        ),
        ("/api/v1/settings/vision", "更新视觉总开关", "BoolSetting"),
        (
            "/api/v1/settings/image_gen",
            "更新图像生成配置",
            "ImageGenSetting",
        ),
        ("/api/v1/settings/tts", "更新 TTS 配置", "TtsSetting"),
        ("/api/v1/settings/tools", "更新内置工具开关", "ToolsSetting"),
        ("/api/v1/settings/mcp", "更新 MCP 配置并重连", "McpSetting"),
    ]
}

/// 单个操作对象；`body` 为 `components.schemas` 里的模型名。
fn op(
    tag: &str,
    summary: &str,
    success: &str,
    body: Option<&str>,
    path_params: &[&str],
    query_params: &[(&str, &str, bool)],
) -> Value {
    let mut parameters: Vec<Value> = Vec::new();
    for name in path_params {
        parameters.push(json!({
            "name": name,
            "in": "path",
            "required": true,
            "schema": {"type": "string"},
        }));
    }
    for (name, schema_type, required) in query_params {
        parameters.push(json!({
            "name": name,
            "in": "query",
            "required": required,
            "schema": {"type": schema_type},
        }));
    }

    let mut responses = Map::new();
    responses.insert(
        success.to_string(),
        json!({
            "description": "成功",
            "content": {"application/json": {"schema": {"type": "object"}}},
        }),
    );
    responses.insert(
        "422".to_string(),
        json!({"description": "请求参数校验失败。"}),
    );

    let mut operation = Map::new();
    operation.insert("tags".to_string(), json!([tag]));
    operation.insert("summary".to_string(), json!(summary));
    if !parameters.is_empty() {
        operation.insert("parameters".to_string(), Value::Array(parameters));
    }
    if let Some(schema) = body {
        operation.insert("requestBody".to_string(), request_body(schema));
    }
    operation.insert("responses".to_string(), Value::Object(responses));
    Value::Object(operation)
}

/// `application/json` 请求体，schema 引用 `components.schemas`。
fn request_body(schema: &str) -> Value {
    let mut reference = Map::new();
    reference.insert(
        "$ref".to_string(),
        Value::String(format!("#/components/schemas/{schema}")),
    );
    let mut content = Map::new();
    content.insert("schema".to_string(), Value::Object(reference));
    let mut media = Map::new();
    media.insert("application/json".to_string(), Value::Object(content));
    let mut body = Map::new();
    body.insert("required".to_string(), Value::Bool(true));
    body.insert("content".to_string(), Value::Object(media));
    Value::Object(body)
}

/// 请求体模型：字段、必填与限长对齐 `omnicrawl/api/models.py` 与 `routes/settings.py`。
fn schemas() -> Value {
    json!({
        "RunRequest": {
            "type": "object",
            "required": ["message"],
            "properties": {
                "message": {"type": "string", "minLength": 1, "maxLength": 200000},
            },
        },
        "ConfirmationDecision": {
            "type": "object",
            "required": ["approved"],
            "properties": {"approved": {"type": "boolean"}},
        },
        "UserQuestionAnswer": {
            "type": "object",
            "required": ["answer"],
            "properties": {"answer": {"type": "string", "minLength": 1}},
        },
        "RenameRequest": {
            "type": "object",
            "required": ["title"],
            "properties": {"title": {"type": "string", "minLength": 1, "maxLength": 200}},
        },
        "ExportRequest": {
            "type": "object",
            "required": ["markdown"],
            "properties": {
                "markdown": {"type": "string", "minLength": 1, "maxLength": 2000000},
            },
        },
        "ProjectRequest": {
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": {"type": "string", "minLength": 1, "maxLength": 200},
                "path": {"type": "string", "maxLength": 32768},
            },
        },
        "ProjectRenameRequest": {
            "type": "object",
            "required": ["path", "name"],
            "properties": {
                "path": {"type": "string", "minLength": 1, "maxLength": 32768},
                "name": {"type": "string", "minLength": 1, "maxLength": 200},
            },
        },
        "ProjectPinRequest": {
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {"type": "string", "minLength": 1, "maxLength": 32768},
                "pinned": {"type": "boolean", "default": true},
            },
        },
        "ProjectPathRequest": {
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {"type": "string", "minLength": 1, "maxLength": 32768},
            },
        },
        "ModelChangeRequest": {
            "type": "object",
            "properties": {
                "model": {"type": "string", "maxLength": 300},
                "source": {"type": "string", "maxLength": 30},
                "key": {"type": "string", "maxLength": 300},
                "profile": {"type": "string", "maxLength": 300},
                "model_id": {"type": "string", "maxLength": 300},
                "protocol": {"type": "string", "maxLength": 100},
            },
        },
        "ReasoningChangeRequest": {
            "type": "object",
            "required": ["effort"],
            "properties": {"effort": {"type": "string", "minLength": 1, "maxLength": 30}},
        },
        "ApprovalChangeRequest": {
            "type": "object",
            "required": ["mode"],
            "properties": {"mode": {"type": "string", "minLength": 1, "maxLength": 30}},
        },
        "BoolSetting": {
            "type": "object",
            "required": ["enabled"],
            "properties": {"enabled": {"type": "boolean"}},
        },
        "ContextWindowSetting": {
            "type": "object",
            "required": ["window_tokens"],
            "properties": {
                "window_tokens": {"type": "integer", "minimum": 1, "maximum": 1000000000},
            },
        },
        "ContextCompactionSetting": {
            "type": "object",
            "required": ["trigger_percent"],
            "properties": {
                "trigger_percent": {"type": "integer", "minimum": 1, "maximum": 100},
            },
        },
        "FeaturesSetting": {
            "type": "object",
            "properties": {
                "memory": {"type": "boolean"},
                "plugins": {"type": "boolean"},
                "subagents": {"type": "boolean"},
                "show_thinking": {"type": "boolean"},
            },
        },
        "AgentWorkspaceSetting": {
            "type": "object",
            "properties": {
                "enabled": {"type": "boolean"},
                "mode": {"type": "string"},
                "base_branch": {"type": "string"},
                "detached": {"type": "boolean"},
                "apply_on_exit": {"type": "boolean"},
                "cleanup_on_exit": {"type": "string"},
                "sync_uncommitted": {"type": "boolean"},
            },
        },
        "ImageGenSetting": {
            "type": "object",
            "properties": {
                "enabled": {"type": "boolean"},
                "base_url": {"type": "string"},
                "api_key_env": {"type": "string"},
                "model": {"type": "string"},
                "size": {"type": "string"},
                "quality": {"type": "string"},
                "output_format": {"type": "string"},
                "n": {"type": "integer", "minimum": 1, "maximum": 10},
                "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 600},
                "api_key": {"type": "string", "maxLength": 0},
            },
        },
        "TtsSetting": {
            "type": "object",
            "properties": {
                "enabled": {"type": "boolean"},
                "model_dir": {"type": "string"},
                "voice": {"type": "string"},
                "auto_play": {"type": "boolean"},
                "thread_count": {"type": "integer", "minimum": 1, "maximum": 32},
                "device": {"type": "string"},
                "streaming": {"type": "boolean"},
                "output_dir": {"type": "string"},
            },
        },
        "RunGuardSetting": {
            "type": "object",
            "description": "接受 {enabled} 或完整的 {guard, continuation} 子集。",
            "properties": {
                "enabled": {"type": "boolean"},
                "guard": {"type": "object"},
                "continuation": {"type": "object"},
            },
        },
        "ToolsSetting": {
            "type": "object",
            "description": "接受 {name, enabled} 或 {switches: {name: bool}}。",
            "properties": {
                "name": {"type": "string"},
                "enabled": {"type": "boolean"},
                "switches": {
                    "type": "object",
                    "additionalProperties": {"type": "boolean"},
                },
            },
        },
        "McpSetting": {
            "type": "object",
            "description": "字段全部可选：globals + {servers} 整表替换，或 {server, delete_server} 单点修改。写盘后立即重连，apply_runtime=false 只写盘。",
            "properties": {
                "enabled": {"type": "boolean"},
                "default_timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 360},
                "policy": {
                    "type": "object",
                    "properties": {
                        "require_confirmation_for_write": {"type": "boolean"},
                        "require_confirmation_for_command": {"type": "boolean"},
                        "allow_external_network_tools": {"type": "boolean"},
                        "audit_log_enabled": {"type": "boolean"},
                    },
                },
                "servers": {
                    "type": "object",
                    "description": "Server 名 → 规格；给出时整表替换。",
                    "additionalProperties": {"type": "object"},
                },
                "server": {"type": "object", "description": "单条 Server 增改；可带 original_name 改名。"},
                "delete_server": {"type": "string"},
                "apply_runtime": {"type": "boolean", "default": true},
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_lists_every_registered_path() {
        let document = document();
        assert_eq!(document["openapi"], "3.1.0");
        assert_eq!(document["info"]["title"], TITLE);
        let paths = document["paths"].as_object().expect("paths 是对象");
        for path in [
            "/health",
            "/api/v1/runtime",
            "/api/v1/runs",
            "/api/v1/runs/{run_id}/events",
            "/api/v1/subagents/events",
            "/api/v1/settings/tools",
        ] {
            assert!(paths.contains_key(path), "缺少路径：{path}");
        }
        assert_eq!(
            document["paths"]["/api/v1/runs"]["post"]["requestBody"]["content"]["application/json"]
                ["schema"]["$ref"],
            "#/components/schemas/RunRequest"
        );
    }

    #[test]
    fn docs_page_points_at_openapi() {
        assert!(docs_html().contains("/openapi.json"));
        assert!(docs_html().contains("SwaggerUIBundle"));
    }
}
