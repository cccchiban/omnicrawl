//! `omnicrawl/agent/toolkit/tools.py` 的工具目录：注册规则由本模块重放，目录数据由
//! `rust/tools/gen_agent_tools_data.py` 从真实现导出到 `data/agent_tools.json`。
//!
//! 工具说明与参数 Schema 是**数据**（1,000 余行、易抄错），注册规则是**逻辑**：
//! 哪些工具随 runner 缺席而省略、知识库与 Windows 两组「整组注册或整组不注册」、
//! 记忆组由开关控制、SubAgent 角色枚举来自运行时注册、`disabled_tools` 过滤。宿主提供
//! 执行函数，这里只回答「模型能看到哪些工具、契约长什么样」。

use crate::error::AgentError;
use crate::json::python_dumps;
use serde_json::Value;
use std::sync::OnceLock;

const AGENT_TOOLS_DATA: &str = include_str!("../data/agent_tools.json");

const KNOWLEDGE_RUNNERS: [&str; 5] = ["kb_search", "kb_read", "kb_write", "kb_append", "kb_list"];

const WINDOWS_RUNNERS: [&str; 5] = [
    "windows_window",
    "windows_control",
    "windows_input",
    "windows_clipboard",
    "windows_screenshot",
];

/// 一条工具声明：模型侧看到的名字、说明与参数契约（不含执行函数）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub argument_schema: String,
    pub requires_confirmation: bool,
    pub model_output_is_bounded: bool,
    pub run_in_subprocess: bool,
}

impl ToolSpec {
    fn from_entry(entry: &Value) -> Self {
        Self {
            name: entry["name"].as_str().unwrap_or_default().to_string(),
            description: entry["description"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            argument_schema: entry["argument_schema"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            requires_confirmation: entry["requires_confirmation"].as_bool().unwrap_or(false),
            model_output_is_bounded: entry["model_output_is_bounded"].as_bool().unwrap_or(false),
            run_in_subprocess: entry["run_in_subprocess"].as_bool().unwrap_or(true),
        }
    }

    fn runner(entry: &Value) -> Option<&str> {
        entry["runner"].as_str()
    }
}

/// MCP Tool 元数据（`registry.tools` 的一条）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpToolRef {
    pub logical_name: String,
    pub server_name: String,
    pub description: String,
    pub argument_schema: String,
    pub requires_confirmation: bool,
}

/// MCP Resource / Prompt 元数据（`logical_name` 装 resource 的 `logical_uri`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpItemRef {
    pub logical_name: String,
    pub server_name: String,
}

/// 构建工具表所需的宿主事实：哪些 runner 已绑定、开关与运行时枚举。
#[derive(Debug, Clone, Default)]
pub struct ToolCatalogOptions<'a> {
    pub available: &'a [&'a str],
    pub memory_enabled: bool,
    pub subagent_types: &'a [String],
    pub mcp_tools: &'a [McpToolRef],
    pub mcp_resources: &'a [McpItemRef],
    pub mcp_prompts: &'a [McpItemRef],
    pub disabled_tools: &'a [String],
}

fn catalog() -> &'static Value {
    static CATALOG: OnceLock<Value> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(AGENT_TOOLS_DATA).expect("内置工具目录数据必须是合法 JSON")
    })
}

fn group(name: &str) -> &'static [Value] {
    catalog()["groups"][name]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn group_rule_error(key: &str) -> AgentError {
    AgentError::new(
        catalog()["group_rules"]["errors"][key]
            .as_str()
            .unwrap_or("工具组注册规则被违反。")
            .to_string(),
    )
}

/// 模板变量：名称/说明里可能出现的占位符。
#[derive(Default)]
struct TemplateVars<'a> {
    logical_name: &'a str,
    server_name: &'a str,
    description: &'a str,
    requires_confirmation: bool,
    argument_schema: &'a str,
}

fn render_template(template: &str, vars: &TemplateVars<'_>) -> String {
    template
        .replace("{logical_name}", vars.logical_name)
        .replace("{logical_uri}", vars.logical_name)
        .replace("{server_name}", vars.server_name)
        .replace("{description}", vars.description)
        .replace(
            "{requires_confirmation}",
            if vars.requires_confirmation {
                "True"
            } else {
                "False"
            },
        )
        .replace("{argument_schema}", vars.argument_schema)
}

fn mcp_template(kind: &str, field: &str) -> String {
    catalog()["mcp"][kind][field]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// MCP 三类动态工具：Tool → Resource → Prompt（与注册表遍历顺序一致）。
pub fn build_mcp_tools(options: &ToolCatalogOptions<'_>) -> Vec<ToolSpec> {
    let mut tools = Vec::new();
    for item in options.mcp_tools {
        tools.push(ToolSpec {
            name: item.logical_name.clone(),
            description: render_template(
                &mcp_template("tool", "description"),
                &TemplateVars {
                    logical_name: &item.logical_name,
                    server_name: &item.server_name,
                    description: &item.description,
                    requires_confirmation: item.requires_confirmation,
                    argument_schema: &item.argument_schema,
                },
            ),
            argument_schema: item.argument_schema.clone(),
            requires_confirmation: item.requires_confirmation,
            model_output_is_bounded: false,
            run_in_subprocess: true,
        });
    }
    for item in options.mcp_resources {
        tools.push(ToolSpec {
            name: render_template(
                &mcp_template("resource", "name"),
                &TemplateVars {
                    logical_name: &item.logical_name,
                    server_name: &item.server_name,
                    ..TemplateVars::default()
                },
            ),
            description: render_template(
                &mcp_template("resource", "description"),
                &TemplateVars {
                    logical_name: &item.logical_name,
                    server_name: &item.server_name,
                    ..TemplateVars::default()
                },
            ),
            argument_schema: mcp_template("resource", "argument_schema"),
            requires_confirmation: false,
            model_output_is_bounded: false,
            run_in_subprocess: true,
        });
    }
    for item in options.mcp_prompts {
        tools.push(ToolSpec {
            name: render_template(
                &mcp_template("prompt", "name"),
                &TemplateVars {
                    logical_name: &item.logical_name,
                    server_name: &item.server_name,
                    ..TemplateVars::default()
                },
            ),
            description: render_template(
                &mcp_template("prompt", "description"),
                &TemplateVars {
                    logical_name: &item.logical_name,
                    server_name: &item.server_name,
                    ..TemplateVars::default()
                },
            ),
            argument_schema: mcp_template("prompt", "argument_schema"),
            requires_confirmation: false,
            model_output_is_bounded: false,
            run_in_subprocess: true,
        });
    }
    tools
}

/// 构建 Agent 可用工具表；执行函数仍由宿主绑定提供。
///
/// `options.disabled_tools` 中的名称（含 MCP 动态工具）不会出现在结果里：模型不可见即
/// 不可调用，与审批模式无关。
pub fn build_agent_tools(options: &ToolCatalogOptions<'_>) -> Result<Vec<ToolSpec>, AgentError> {
    let has = |name: &str| options.available.contains(&name);
    let present = |names: &[&str]| names.iter().filter(|name| has(name)).count();

    // 知识库组先校验：Python 在构建任何工具之前就检查「要么整组、要么一个不给」。
    let knowledge_present = present(&KNOWLEDGE_RUNNERS);
    if knowledge_present > 0 && knowledge_present < KNOWLEDGE_RUNNERS.len() {
        return Err(group_rule_error("knowledge_partial"));
    }

    let mut tools = build_mcp_tools(options);

    for entry in group("core") {
        if include_entry(entry, options) {
            tools.push(ToolSpec::from_entry(entry));
        }
    }
    if knowledge_present == KNOWLEDGE_RUNNERS.len() {
        for entry in group("knowledge") {
            tools.push(ToolSpec::from_entry(entry));
        }
    }
    for entry in group("meta") {
        if include_entry(entry, options) {
            tools.push(ToolSpec::from_entry(entry));
        }
    }

    let windows_present = present(&WINDOWS_RUNNERS);
    if windows_present > 0 {
        if windows_present < WINDOWS_RUNNERS.len() {
            return Err(group_rule_error("windows_partial"));
        }
        for entry in group("windows") {
            tools.push(ToolSpec::from_entry(entry));
        }
    }

    if has("subagent") {
        let entry = group("subagent")
            .first()
            .expect("内置目录必须有 subagent 定义");
        let mut spec = ToolSpec::from_entry(entry);
        spec.argument_schema = subagent_schema(&spec.argument_schema, options.subagent_types)?;
        tools.push(spec);
    }

    if options.memory_enabled {
        for entry in group("memory") {
            tools.push(ToolSpec::from_entry(entry));
        }
    }

    tools.retain(|tool| !options.disabled_tools.contains(&tool.name));
    Ok(tools)
}

fn include_entry(entry: &Value, options: &ToolCatalogOptions<'_>) -> bool {
    match ToolSpec::runner(entry) {
        None => true,
        Some(runner) => options.available.contains(&runner),
    }
}

/// 把 SubAgent 角色枚举替换成运行时注册的角色（去空白、去重、排序）。
fn subagent_schema(template: &str, subagent_types: &[String]) -> Result<String, AgentError> {
    let mut roles: Vec<String> = subagent_types
        .iter()
        .map(|name| name.trim().to_lowercase())
        .filter(|name| !name.is_empty())
        .collect();
    roles.sort();
    roles.dedup();
    if roles.is_empty() {
        return Err(group_rule_error("subagent_without_types"));
    }

    let mut schema: Value = serde_json::from_str(template)
        .map_err(|error| AgentError::new(format!("SubAgent 参数 Schema 解析失败：{error}")))?;
    match schema.pointer_mut("/properties/tasks/items/properties/subagent_type") {
        Some(Value::Object(field)) => {
            field.clear();
            field.insert("type".to_string(), Value::from("string"));
            field.insert(
                "enum".to_string(),
                Value::Array(roles.into_iter().map(Value::from).collect()),
            );
        }
        _ => return Err(AgentError::new("SubAgent 参数 Schema 缺少角色枚举。")),
    }
    Ok(python_dumps(&schema, 0))
}
