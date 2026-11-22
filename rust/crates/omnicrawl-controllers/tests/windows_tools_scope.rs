//! 主代理不再声明 Windows 桌面工具：它们只由专职 Computer Use 子代理的子回合领取。
//!
//! 这条边界很容易被「组规则」或「父子交白名单」之类的旧逻辑悄悄破坏，因此单独钉住。

use omnicrawl_controllers::tool_catalog::{build_agent_tools, windows_tool_specs, ToolCatalogOptions};

const WINDOWS_TOOLS: [&str; 6] = [
    "windows_window",
    "windows_control",
    "windows_input",
    "windows_clipboard",
    "windows_screenshot",
    "windows_batch",
];

fn names(options: &ToolCatalogOptions<'_>) -> Vec<String> {
    build_agent_tools(options)
        .expect("工具表应当构建成功")
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

/// 主代理的表里没有桌面工具：它们改由 `computer_use` 这一个入口转交专职子代理。
#[test]
fn the_main_agent_does_not_declare_windows_tools() {
    let available: Vec<&str> = [
        "list",
        "read",
        "windows_window",
        "windows_control",
        "windows_input",
        "windows_clipboard",
        "windows_screenshot",
        "windows_batch",
    ]
    .to_vec();
    let options = ToolCatalogOptions {
        available: &available,
        ..ToolCatalogOptions::default()
    };
    let built = names(&options);
    for tool in WINDOWS_TOOLS {
        assert!(
            !built.contains(&tool.to_string()),
            "主代理不该声明 {tool}：{built:?}"
        );
    }
}

/// 子代理的子回合声明桌面工具：不声明就等于把 Computer Use 的能力挖空。
#[test]
fn the_dedicated_subagent_declares_windows_tools() {
    let available: Vec<&str> = [
        "list",
        "read",
        "read_image",
        "windows_window",
        "windows_control",
        "windows_input",
        "windows_clipboard",
        "windows_screenshot",
        "windows_batch",
    ]
    .to_vec();
    let options = ToolCatalogOptions {
        available: &available,
        windows_tools: true,
        ..ToolCatalogOptions::default()
    };
    let built = names(&options);
    for tool in WINDOWS_TOOLS {
        assert!(built.contains(&tool.to_string()), "子代理缺少 {tool}：{built:?}");
    }
}

/// 开声明开关但只给一半桌面工具时，仍按组规则报错（避免半套能力被当成完整）。
#[test]
fn a_partial_windows_group_is_still_rejected_for_the_subagent() {
    let available: Vec<&str> = ["windows_window", "windows_input"].to_vec();
    let options = ToolCatalogOptions {
        available: &available,
        windows_tools: true,
        ..ToolCatalogOptions::default()
    };
    let error = build_agent_tools(&options).expect_err("只给一半应当报错");
    assert_eq!(error.message(), "Windows 桌面工具必须作为完整工具组注册。");
}

/// 内置目录提供的桌面规格：一整套、带可用说明（参数契约是示例式的，由
/// `tool_declarations::declaration` 统一推断成 Schema，这里只钉住目录本身完整）。
#[test]
fn the_builtin_catalog_offers_the_whole_desktop_group() {
    let specs = windows_tool_specs();
    let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
    assert_eq!(names, WINDOWS_TOOLS.to_vec(), "桌面组必须完整：{names:?}");
    for spec in &specs {
        assert!(
            !spec.description.trim().is_empty(),
            "{} 缺少说明，模型会不知道它做什么",
            spec.name
        );
        let contract: serde_json::Value =
            serde_json::from_str(&spec.argument_schema).expect("参数契约应当是合法 JSON");
        assert!(
            contract.is_object(),
            "{} 的参数契约要是对象：{contract}",
            spec.name
        );
    }
}
