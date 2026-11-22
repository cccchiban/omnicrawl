//! 声明必须满足 OpenAI 严格 Schema 编译的硬约束：每个 `required` 名都在同层 `properties` 里。
//!
//! 违反时不是「这个参数没被要求」这么轻——中转网关把请求里所有 tools 一律按 strict 编译，
//! 一条不合规的声明就让整份请求 400（`Unsatisfiable schema: required property ... is
//! unsatisfiable. Compilation limits apply to all strict schemas in the request.`）。
//! cad-mcp 的 `cad_export` 正是这样踩中的：顶层 `anyOf` 的分支只写 `required`、没有
//! `properties`。

use omnicrawl_controllers::tool_catalog::{build_agent_tools, ToolCatalogOptions};
use omnicrawl_controllers::tool_declarations::declaration;
use serde_json::Value;

/// 递归检查：每个 `required` 名都必须出现在同层 `properties` 的键里。
fn offending_paths(node: &Value, path: &str, out: &mut Vec<String>) {
    match node {
        Value::Object(map) => {
            if let Some(Value::Array(names)) = map.get("required") {
                let declared: Vec<&String> = match map.get("properties") {
                    Some(Value::Object(properties)) => properties.keys().collect(),
                    _ => Vec::new(),
                };
                for name in names.iter().filter_map(Value::as_str) {
                    if !declared.iter().any(|key| key.as_str() == name) {
                        out.push(format!("{path}.required[{name}]"));
                    }
                }
            }
            for (key, child) in map {
                offending_paths(child, &format!("{path}.{key}"), out);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                offending_paths(child, &format!("{path}[{index}]"), out);
            }
        }
        _ => {}
    }
}

#[test]
fn every_builtin_declaration_satisfies_strict_schema_compilation() {
    // 把内置目录里所有工具都纳入（包括桌面组与知识库组），MCP 的工具另由各自的
    // Schema 决定，这里只能覆盖仓库自带的那部分。
    let specs = build_agent_tools(&ToolCatalogOptions {
        available: &[
            "list",
            "read",
            "read_image",
            "write_file",
            "Edit_file",
            "bash",
            "powershell",
            "grep",
            "find",
            "monitor",
            "fetcher",
            "web_search",
            "image_gen",
            "advisor",
            "computer_use",
            "config_edit",
            "kb_search",
            "kb_read",
            "kb_write",
            "kb_append",
            "kb_list",
            "kb_delete",
            "update_todos",
            "ask_user",
            "pause_work",
            "windows_window",
            "windows_control",
            "windows_input",
            "windows_clipboard",
            "windows_screenshot",
            "windows_batch",
        ],
        windows_tools: true,
        ..ToolCatalogOptions::default()
    })
    .expect("工具表应当构建成功");
    assert!(!specs.is_empty(), "内置目录不该是空的");

    let mut failures: Vec<String> = Vec::new();
    for spec in &specs {
        let declared = declaration(spec);
        let parameters = &declared["function"]["parameters"];
        let mut bad = Vec::new();
        offending_paths(parameters, &spec.name, &mut bad);
        // 顶层必须仍然是对象 Schema，否则上游直接判非法函数定义。
        if parameters.get("type").and_then(Value::as_str) != Some("object") {
            bad.push(format!("{}.type 不是 object", spec.name));
        }
        failures.extend(bad);
    }

    assert!(
        failures.is_empty(),
        "这些 required 落在同层 properties 之外，会被严格编译拒绝：{failures:?}"
    );
}

#[test]
fn the_cad_export_shape_from_the_reported_400_is_accepted() {
    // 线上报错原文：`Invalid schema for 'tools[6].parameters': Unsatisfiable schema:
    // required property 'destinations' is unsatisfiable`。cad-mcp 的 cad_export 把两组
    // 互斥参数写成顶层 anyOf，分支只带 required。
    let spec = omnicrawl_controllers::tool_catalog::ToolSpec {
        name: "cad_export".to_string(),
        description: "导出".to_string(),
        argument_schema: r#"{"type":"object","properties":{"requestId":{"type":"string"},"source":{"type":"string"},"destinations":{"type":"array","items":{"type":"string"}},"action":{"type":"string"},"operationId":{"type":"string"},"epoch":{"type":"string"}},"anyOf":[{"required":["requestId","source","destinations"]},{"required":["action","operationId","epoch"]}]}"#
            .to_string(),
        requires_confirmation: false,
        model_output_is_bounded: false,
        run_in_subprocess: true,
    };
    let declared = declaration(&spec);
    let parameters = &declared["function"]["parameters"];
    let mut bad = Vec::new();
    offending_paths(parameters, "cad_export", &mut bad);
    assert!(bad.is_empty(), "仍会被拒绝：{bad:?} / {parameters}");
    // 参数契约本身不能因为归一化而丢失。
    for key in ["destinations", "requestId", "action", "epoch"] {
        assert!(
            parameters["properties"].get(key).is_some(),
            "{key} 丢了：{parameters}"
        );
    }
}
