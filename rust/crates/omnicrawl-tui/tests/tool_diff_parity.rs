//! 对照：Rust 工具卡渲染层 vs Python 真实现 `omnicrawl/ui/fullscreen/rendering/tool_diff.py`。
//!
//! 数据集是冻结的对照契约（期望值取自 Python 真实现）。
//! 覆盖标题（状态色点、文件变更分支、工作区摘要分支、MCP 命名空间、路径压缩）、
//! 正文（文件变更预览 / fetcher 精选 / read 与记忆类隐藏 / 其余原样输出）与纯文本标题。
//! 每条用例都比对**纯文本**与**(样式, 文本) 运行段**——正文的 diff 着色与「隐藏类正文为空」
//! 都在运行段里，只看纯文本会漏掉。
//!
//! 改了任一侧实现都要重跑生成脚本再跑本测试。

use serde_json::Value;

use omnicrawl_controllers::tool_args::normalize_tool_name;
use omnicrawl_tui::ui::conversation::collapsed_body;
use omnicrawl_tui::ui::fullscreen::rendering::tool_diff::{
    describe_file_change, is_file_change_tool, plain_tool_title, tool_disclosure_body,
    tool_disclosure_title, tool_operation,
};
use omnicrawl_tui::ui::fullscreen::text::StyledText;
use omnicrawl_tui::ui::fullscreen::tool_labels::{canonical_tool_name, tool_display};

const FIXTURE: &str = include_str!("fixtures/tool_diff_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("对照数据集必须是合法 JSON")
}

/// 把富文本归一成 (样式, 文本) 运行段：相邻同样式合并，空段丢弃。
fn runs_of(text: &StyledText) -> Vec<(String, String)> {
    let mut runs: Vec<(String, String)> = Vec::new();
    for span in text.spans() {
        if span.text.is_empty() {
            continue;
        }
        match runs.last_mut() {
            Some(last) if last.0 == span.style => last.1.push_str(&span.text),
            _ => runs.push((span.style.clone(), span.text.clone())),
        }
    }
    runs
}

fn expected_runs(case: &Value) -> Vec<(String, String)> {
    case["expected_spans"]
        .as_array()
        .expect("expected_spans 必须是数组")
        .iter()
        .map(|run| {
            let pair = run.as_array().expect("运行段必须是 [样式, 文本]");
            (
                pair[0].as_str().unwrap_or_default().to_string(),
                pair[1].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn arguments(case: &Value) -> Value {
    case["arguments"].clone()
}

fn result_text(case: &Value) -> String {
    case["result_text"].as_str().unwrap_or_default().to_string()
}

#[test]
fn tool_disclosure_title_matches_python() {
    let data = fixture();
    let cases = data["titles"].as_array().expect("titles 必须是数组");
    assert!(!cases.is_empty(), "数据集不应为空");
    for case in cases {
        let tool = case["tool"].as_str().unwrap_or_default();
        let status = case["status"].as_str().unwrap_or_default();
        let duration = case["duration_seconds"].as_f64().unwrap_or_default();
        let expanded = case["expanded"].as_bool().unwrap_or(false);
        let actual = tool_disclosure_title(
            tool,
            &arguments(case),
            status,
            duration,
            expanded,
            &result_text(case),
        );
        assert_eq!(
            actual.plain(),
            case["expected_plain"].as_str().unwrap_or_default(),
            "标题纯文本不一致（tool={tool} status={status}）"
        );
        assert_eq!(
            runs_of(&actual),
            expected_runs(case),
            "标题样式段不一致（tool={tool} status={status}）"
        );
    }
}

#[test]
fn tool_disclosure_body_matches_python() {
    let data = fixture();
    let cases = data["bodies"].as_array().expect("bodies 必须是数组");
    assert!(!cases.is_empty(), "数据集不应为空");
    for case in cases {
        let tool = case["tool"].as_str().unwrap_or_default();
        let actual = tool_disclosure_body(tool, &arguments(case), &result_text(case));
        assert_eq!(
            actual.plain(),
            case["expected_plain"].as_str().unwrap_or_default(),
            "正文纯文本不一致（tool={tool}）"
        );
        assert_eq!(
            runs_of(&actual),
            expected_runs(case),
            "正文样式段不一致（tool={tool}）"
        );
    }
}

#[test]
fn edit_aliases_match_canonical_tool_rendering() {
    let args = serde_json::json!({
        "path": "src/main.rs",
        "old_text": "old\nkeep\nend",
        "new_text": "new\nkeep\nend"
    });
    let result = "已修改 src/main.rs，替换 1 处。\n首个替换位置上下文（第 40-44 行，前后各 2 行）：\n40: before\n41: context\n42: new\n43: keep\n44: end";
    let canonical_body = tool_disclosure_body("Edit_file", &args, result);
    let plain = canonical_body.plain();
    assert!(plain.contains("  42 │ - old\n"));
    assert!(plain.contains("  42 │ + new\n"));
    assert!(plain.ends_with("结果：替换 1 处"));
    assert!(!plain.contains("首个替换位置上下文"));
    assert_eq!(canonical_tool_name("Edit_file"), "Edit_file");

    for alias in [
        "edit_file",
        "EDIT_FILE",
        "EditFile",
        "edit-file",
        " edit_file ",
    ] {
        assert_eq!(normalize_tool_name(alias, &["Edit_file"]), "Edit_file");
        assert_eq!(canonical_tool_name(alias), "Edit_file");
        assert_eq!(tool_display(alias), tool_display("Edit_file"));
        assert_eq!(tool_operation(alias), "Edit_file");
        assert!(is_file_change_tool(alias));
        let change = describe_file_change(alias, &args, 42);
        assert_eq!(change.stats_label, "+1 -1");
        assert!(change.body.plain().contains("  42 │ + new\n"));

        for (status, output) in [("调用中", ""), ("成功", result)] {
            let title = tool_disclosure_title(alias, &args, status, 0.5, false, output);
            let expected_title =
                tool_disclosure_title("Edit_file", &args, status, 0.5, false, output);
            assert_eq!(title.plain(), expected_title.plain());
            assert_eq!(runs_of(&title), runs_of(&expected_title));
            assert!(title.plain().starts_with("● Edit_file src/main.rs · +1 -1"));

            let body = tool_disclosure_body(alias, &args, output);
            let expected_body = tool_disclosure_body("Edit_file", &args, output);
            assert_eq!(body.plain(), expected_body.plain());
            assert_eq!(runs_of(&body), runs_of(&expected_body));
            let (head, hidden, tail) = collapsed_body(alias, &body);
            assert_eq!(hidden, 0);
            assert!(tail.is_empty());
            assert_eq!(head.len(), body.split_lines().len());
        }
    }
}

#[test]
fn edit_name_normalization_preserves_other_tools_and_mcp_names() {
    for name in [
        "write_file",
        "custom_tool",
        "edit_files",
        "filesystem.edit_file",
    ] {
        assert_eq!(canonical_tool_name(name), name);
        assert_eq!(tool_display(name).name, name);
    }
    assert!(!is_file_change_tool("filesystem.edit_file"));
    assert_eq!(tool_operation("filesystem.edit_file"), "edit_file");
    assert_eq!(tool_display("filesystem.edit_file").icon, "⌁");
}

#[test]
fn plain_tool_title_matches_python() {
    let data = fixture();
    let cases = data["plain_titles"]
        .as_array()
        .expect("plain_titles 必须是数组");
    assert!(!cases.is_empty(), "数据集不应为空");
    for case in cases {
        let tool = case["tool"].as_str().unwrap_or_default();
        let status = case["status"].as_str().unwrap_or_default();
        let duration = case["duration_seconds"].as_f64().unwrap_or_default();
        let expanded = case["expanded"].as_bool().unwrap_or(false);
        let actual = plain_tool_title(
            tool,
            &arguments(case),
            status,
            duration,
            expanded,
            &result_text(case),
        );
        assert_eq!(
            actual,
            case["expected"].as_str().unwrap_or_default(),
            "纯文本标题不一致（tool={tool} status={status}）"
        );
    }
}
