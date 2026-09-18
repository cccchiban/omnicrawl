//! `agent/toolkit/tools.py` 工具目录的跨语言对照。
//!
//! 期望值来自 Python 真实现：`python rust/tools/gen_controllers_fixture.py` 重新生成
//! `tests/fixtures/controllers_parity.json` 的 `tool_catalog` 段后，本套件用同一批 runner
//! 绑定与开关重放 Rust 的注册规则，逐工具比对名称、说明、Schema 与两个开关。
//! 目录数据本身由 `rust/tools/gen_agent_tools_data.py` 从真实现导出（见 crate README）。

use omnicrawl_controllers::tool_catalog::{
    build_agent_tools, build_mcp_tools, McpItemRef, McpToolRef, ToolCatalogOptions, ToolSpec,
};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn section() -> Value {
    fixture()["tool_catalog"].clone()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("字符串数组")
        .iter()
        .map(|item| item.as_str().expect("字符串").to_string())
        .collect()
}

/// 与生成器中的 `MCP_SAMPLE` 保持一致。
fn mcp_tools() -> Vec<McpToolRef> {
    vec![McpToolRef {
        logical_name: "srv.tool".to_string(),
        server_name: "srv".to_string(),
        description: "工具说明".to_string(),
        argument_schema: r#"{"a": 1}"#.to_string(),
        requires_confirmation: true,
    }]
}

fn mcp_resources() -> Vec<McpItemRef> {
    vec![McpItemRef {
        logical_name: "file://x".to_string(),
        server_name: "srv".to_string(),
    }]
}

fn mcp_prompts() -> Vec<McpItemRef> {
    vec![McpItemRef {
        logical_name: "srv.prompt".to_string(),
        server_name: "srv".to_string(),
    }]
}

fn assert_spec_matches(tool: &ToolSpec, expected: &Value, case: &str) {
    assert_eq!(
        tool.name,
        expected["name"].as_str().expect("name"),
        "工具名（{case}）"
    );
    assert_eq!(
        tool.description,
        expected["description"].as_str().expect("description"),
        "工具说明（{}）",
        tool.name
    );
    assert_eq!(
        tool.argument_schema,
        expected["argument_schema"]
            .as_str()
            .expect("argument_schema"),
        "参数 Schema（{}）",
        tool.name
    );
    assert_eq!(
        tool.requires_confirmation,
        expected["requires_confirmation"]
            .as_bool()
            .expect("requires_confirmation"),
        "需确认标记（{}）",
        tool.name
    );
    assert_eq!(
        tool.model_output_is_bounded,
        expected["model_output_is_bounded"]
            .as_bool()
            .expect("model_output_is_bounded"),
        "输出有界标记（{}）",
        tool.name
    );
    assert_eq!(
        tool.run_in_subprocess,
        expected["run_in_subprocess"]
            .as_bool()
            .expect("run_in_subprocess"),
        "子进程标记（{}）",
        tool.name
    );
}

#[test]
fn tool_catalog_registration_matches_python() {
    let data = section();
    for case in data["cases"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let available = strings(&case["available"]);
        let available_refs: Vec<&str> = available.iter().map(String::as_str).collect();
        let subagent_types = strings(&case["subagent_types"]);
        let disabled = strings(&case["disabled"]);
        let has_sample = case["mcp_sample"].as_bool().expect("mcp_sample");
        let tools = if has_sample { mcp_tools() } else { Vec::new() };
        let resources = if has_sample {
            mcp_resources()
        } else {
            Vec::new()
        };
        let prompts = if has_sample {
            mcp_prompts()
        } else {
            Vec::new()
        };

        let options = ToolCatalogOptions {
            available: &available_refs,
            memory_enabled: case["memory_enabled"].as_bool().expect("memory_enabled"),
            subagent_types: &subagent_types,
            mcp_tools: &tools,
            mcp_resources: &resources,
            mcp_prompts: &prompts,
            disabled_tools: &disabled,
        };
        let result = build_agent_tools(&options);
        if !case["ok"].as_bool().expect("ok") {
            let error = result.expect_err("应当报错");
            assert_eq!(
                error.message(),
                case["error"].as_str().expect("error"),
                "组规则文案（{label}）"
            );
            continue;
        }
        let built = result.expect("应当成功");
        let expected = case["tools"].as_array().expect("tools");
        assert_eq!(
            built.len(),
            expected.len(),
            "工具数量（{label}）：{:?}",
            built
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>()
        );
        for (tool, expected) in built.iter().zip(expected.iter()) {
            assert_spec_matches(tool, expected, label);
        }
    }
}

#[test]
fn tool_catalog_mcp_samples_match_python() {
    let data = section();
    let available = vec!["list", "read"];
    let tools = mcp_tools();
    let resources = mcp_resources();
    let prompts = mcp_prompts();
    let options = ToolCatalogOptions {
        available: &available,
        subagent_types: &[],
        mcp_tools: &tools,
        mcp_resources: &resources,
        mcp_prompts: &prompts,
        ..ToolCatalogOptions::default()
    };
    let built = build_mcp_tools(&options);
    let expected = data["mcp_samples"].as_array().expect("mcp_samples");
    assert_eq!(built.len(), expected.len(), "MCP 工具条数");
    for (tool, expected) in built.iter().zip(expected.iter()) {
        assert_spec_matches(tool, expected, "MCP");
    }
}
