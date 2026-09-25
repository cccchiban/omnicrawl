//! 对照：Rust 工具卡渲染层 vs Python 真实现 `omnicrawl/ui/fullscreen/rendering/tool_diff.py`。
//!
//! 数据集由 `python rust/tools/gen_tool_diff_fixture.py` 生成（期望值取自 Python 真实现）。
//! 覆盖标题（状态色点、文件变更分支、工作区摘要分支、MCP 命名空间、路径压缩）、
//! 正文（文件变更预览 / fetcher 精选 / read 与记忆类隐藏 / 其余原样输出）与纯文本标题。
//! 每条用例都比对**纯文本**与**(样式, 文本) 运行段**——正文的 diff 着色与「隐藏类正文为空」
//! 都在运行段里，只看纯文本会漏掉。
//!
//! 改了任一侧实现都要重跑生成脚本再跑本测试。

use serde_json::Value;

use omnicrawl_tui::ui::fullscreen::rendering::tool_diff::{
    plain_tool_title, tool_disclosure_body, tool_disclosure_title,
};
use omnicrawl_tui::ui::fullscreen::text::StyledText;

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
