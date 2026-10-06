//! 对照：Rust 工作区工具 vs Python 真实现。
//!
//! 数据集是冻结的对照契约（期望值取自
//! `omnicrawl/workspace/tools.py` 与 `omnicrawl/agent/toolkit/host_tools.py`）。
//! 用例按生成顺序重放：read → write_file → edit_file，写/改类用例会改变文件状态，
//! 顺序本身也是对照的一部分。

use std::path::PathBuf;

use serde_json::{Map, Value};

use omnicrawl_tui::tools::declarations::declaration;
use omnicrawl_tui::tools::read::READ_MAX_LINE_LENGTH;
use omnicrawl_tui::tools::{
    edit, read, sample, write, RegistryOptions, ToolRegistry, WorkspacePaths,
};

const FIXTURE: &str = include_str!("fixtures/workspace_tools_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("对照数据集必须是合法 JSON")
}

/// 与 Python 生成器里一致的初始文件状态。
fn prepare_workspace(name: &str) -> (WorkspacePaths, PathBuf) {
    let root = std::env::temp_dir().join(format!("omnicrawl-tui-parity-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("创建临时工作区");
    let write = |path: &str, text: &str| {
        std::fs::write(root.join(path), text).expect("写初始文件");
    };
    write("window.txt", "第一行\n第二行\n第三行\n");
    write("long.txt", &format!("{}\n", "x".repeat(2100)));
    write("empty.txt", "");
    write(
        "many.txt",
        &(1..=50)
            .map(|index| format!("行{index}\n"))
            .collect::<String>(),
    );
    write("crlf.txt", "one\r\ntwo\r\n");
    write(
        "snippet.txt",
        &(1..=20)
            .map(|index| format!("行{index}\n"))
            .collect::<String>(),
    );
    write("edit.txt", "第一行\n第二行\n第三行\n");
    write("edit2.txt", "x\nx\n");
    write("crlf-edit.txt", "one\r\ntwo\r\n");
    write("locate.txt", "def alpha():\n    return 1\n");
    (WorkspacePaths::new(&root), root)
}

fn arguments(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

/// 失败结果与模型可见文本对照：Python 侧同样用 `formatted_message()`（带错误码前缀）。
fn compare_failure(case: &Value, error: &omnicrawl_tui::tools::ToolError) {
    assert_eq!(case["ok"], false, "用例 {:?}", case["arguments"]);
    assert_eq!(
        error.formatted(),
        case["output"],
        "用例 {:?}",
        case["arguments"]
    );
    assert_eq!(
        error.code.clone().map(Value::from).unwrap_or(Value::Null),
        case.get("code").cloned().unwrap_or(Value::Null),
        "用例 {:?}",
        case["arguments"]
    );
    assert_eq!(
        error.retryable,
        case.get("retryable")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "用例 {:?}",
        case["arguments"]
    );
}

#[test]
fn declarations_match_python() {
    let root = std::env::temp_dir().join("omnicrawl-tui-parity-declarations");
    let registry =
        ToolRegistry::new(&root, &RegistryOptions::default(), 360).expect("工具表应当构建成功");
    for entry in fixture()["declarations"].as_array().expect("声明用例") {
        let name = entry["name"].as_str().unwrap_or_default();
        let spec = registry
            .specs()
            .iter()
            .find(|spec| spec.name == name)
            .unwrap_or_else(|| panic!("工具表缺少 {name}"));
        assert_eq!(
            declaration(spec),
            entry["declaration"],
            "{name} 的声明与 Python 不一致"
        );
    }
}

#[test]
fn read_cases_match_python() {
    let cases = fixture()["files"]["read"]
        .as_array()
        .expect("read 用例")
        .clone();
    let (paths, _root) = prepare_workspace("read");
    for case in cases {
        match read::read(&paths, &arguments(&case["arguments"])) {
            Ok(output) => {
                assert_eq!(case["ok"], true, "用例 {:?}", case["arguments"]);
                assert_eq!(output, case["output"], "用例 {:?}", case["arguments"]);
            }
            Err(error) => compare_failure(&case, &error),
        }
    }
}

#[test]
fn write_cases_match_python() {
    let cases = fixture()["files"]["write_file"]
        .as_array()
        .expect("write 用例")
        .clone();
    let (paths, root) = prepare_workspace("write");
    for case in cases {
        let result = write::write_file(&paths, &arguments(&case["arguments"]));
        match result {
            Ok(output) => {
                assert_eq!(case["ok"], true, "用例 {:?}", case["arguments"]);
                assert_eq!(output, case["output"], "用例 {:?}", case["arguments"]);
            }
            Err(error) => compare_failure(&case, &error),
        }
    }
    // 追加与覆盖等价性：末态与 Python 侧同一序列一致。
    assert_eq!(
        std::fs::read_to_string(root.join("out/new.txt")).expect("读回"),
        "覆盖"
    );
}

#[test]
fn edit_cases_match_python() {
    let cases = fixture()["files"]["edit_file"]
        .as_array()
        .expect("edit 用例")
        .clone();
    let (paths, _root) = prepare_workspace("edit");
    for case in cases {
        let result = edit::edit_file(&paths, &arguments(&case["arguments"]));
        match result {
            Ok(output) => {
                assert_eq!(case["ok"], true, "用例 {:?}", case["arguments"]);
                assert_eq!(output, case["output"], "用例 {:?}", case["arguments"]);
            }
            Err(error) => compare_failure(&case, &error),
        }
    }
}

#[test]
fn sampling_matches_python() {
    for case in fixture()["sampling"].as_array().expect("采样用例") {
        let text = case["text"].as_str().unwrap_or_default();
        assert_eq!(
            sample::sample_command_output(text, None),
            case["expected"],
            "采样结果不一致（输入 {} 字符）",
            text.chars().count()
        );
    }
}

#[test]
fn locator_cases_match_python() {
    // `function_name` 定位与 Python 逐字对照：AST 路径与大括号扫描回退都在数据集里。
    for case in fixture()["locators"].as_array().expect("定位用例") {
        let (paths, root) = prepare_workspace("locators");
        // 数据集自带每个用例的初始文件内容，避免用例间互相污染。
        if let Some(files) = case["files"].as_object() {
            for (name, content) in files {
                std::fs::write(root.join(name), content.as_str().unwrap_or_default())
                    .expect("写定位用例文件");
            }
        }
        let label = case["label"].as_str().unwrap_or_default();
        match read::read(&paths, &arguments(&case["arguments"])) {
            Ok(output) => {
                assert_eq!(case["ok"], true, "用例 {label}");
                assert_eq!(output, case["output"], "用例 {label}");
            }
            Err(error) => {
                assert_eq!(case["ok"], false, "用例 {label}");
                assert_eq!(error.formatted(), case["output"], "用例 {label}");
            }
        }
    }
}

#[test]
fn long_line_truncation_constant_matches_python() {
    assert_eq!(READ_MAX_LINE_LENGTH, 2000);
}
