//! 对照：Rust LaTeX 转换层 vs Python 真实现 `omnicrawl/ui/fullscreen/rendering/latex.py`。
//!
//! 数据集由 `python rust/tools/gen_latex_fixture.py` 生成（期望值取自 Python 真实现）。
//! 覆盖行内 / 块级 / 数学 fenced / 裸公式四类转换、块级分段与块级公式快判。
//! 改了任一侧实现都要重跑生成脚本再跑本测试。

use serde_json::Value;

use omnicrawl_tui::ui::fullscreen::rendering::latex::{
    has_block_formula, latex_to_text, split_blocks,
};

const FIXTURE: &str = include_str!("fixtures/latex_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("对照数据集必须是合法 JSON")
}

#[test]
fn latex_to_text_matches_python() {
    let data = fixture();
    let cases = data["conversion"]
        .as_array()
        .expect("conversion 必须是数组");
    let mut failures = Vec::new();
    for case in cases {
        let text = case["text"].as_str().expect("text 必须是字符串");
        let expected = case["expected"].as_str().expect("expected 必须是字符串");
        let actual = latex_to_text(text);
        if actual != expected {
            failures.push(format!(
                "输入 {text:?}\n  期望 {expected:?}\n  实际 {actual:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "转换对照失败：\n{}",
        failures.join("\n")
    );
}

#[test]
fn split_blocks_matches_python() {
    let data = fixture();
    let cases = data["split_blocks"]
        .as_array()
        .expect("split_blocks 必须是数组");
    let mut failures = Vec::new();
    for case in cases {
        let text = case["text"].as_str().expect("text 必须是字符串");
        let expected: Vec<(String, Option<String>)> = case["blocks"]
            .as_array()
            .expect("blocks 必须是数组")
            .iter()
            .map(|part| {
                let pair = part.as_array().expect("分段必须是数组");
                let chunk = pair[0].as_str().unwrap_or("").to_string();
                let formula = pair[1].as_str().map(str::to_string);
                (chunk, formula)
            })
            .collect();
        let actual = split_blocks(text);
        if actual != expected {
            failures.push(format!(
                "输入 {text:?}\n  期望 {expected:?}\n  实际 {actual:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "分段对照失败：\n{}",
        failures.join("\n")
    );
}

#[test]
fn has_block_formula_matches_python() {
    let data = fixture();
    let cases = data["has_block_formula"]
        .as_array()
        .expect("has_block_formula 必须是数组");
    let mut failures = Vec::new();
    for case in cases {
        let text = case["text"].as_str().expect("text 必须是字符串");
        let expected = case["expected"].as_bool().expect("expected 必须是布尔");
        let actual = has_block_formula(text);
        if actual != expected {
            failures.push(format!("输入 {text:?}：期望 {expected}，实际 {actual}"));
        }
    }
    assert!(
        failures.is_empty(),
        "块级公式快判对照失败：\n{}",
        failures.join("\n")
    );
}
