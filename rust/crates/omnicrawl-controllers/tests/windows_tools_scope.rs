//! 主代理不再声明 Windows 桌面工具：它们只由专职 Computer Use 子代理的子回合领取。
//!
//! 这条边界很容易被「组规则」或「父子交白名单」之类的旧逻辑悄悄破坏，因此单独钉住。

use omnicrawl_controllers::tool_catalog::{build_agent_tools, ToolCatalogOptions};

const WINDOWS_TOOLS: [&str; 5] = [
    "windows_window",
    "windows_control",
    "windows_input",
    "windows_clipboard",
    "windows_screenshot",
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
