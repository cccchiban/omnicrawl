//! 对照：Rust 搜索与 git 工具 vs Python 真实现。
//!
//! 数据集是冻结的对照契约（期望值取自
//! `omnicrawl/workspace/tools.py` 与 `omnicrawl/agent/toolkit/git_tools.py`）。
//! 用例按生成顺序重放：list → find → grep；find 的候选顺序依赖修改时间，
//! 两侧都把 mtime 钉死到同一时刻，才能逐字对照。

use std::fs::FileTimes;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use regex::Regex;
use serde_json::{Map, Value};

use omnicrawl_tui::tools::{finding, git, grep, listing, ToolError, WorkspacePaths};

const FIXTURE: &str = include_str!("fixtures/workspace_tools_parity.json");
/// 与生成器一致的固定修改时间。
const FIXED_MTIME: u64 = 1_700_000_000;

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("对照数据集必须是合法 JSON")
}

fn arguments(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

/// 把绝对工作区路径与随机落盘文件名换成占位符，便于与数据集逐字对照。
fn normalize(text: &str, workspace: &Path) -> String {
    let mut normalized = text.to_string();
    for form in [
        workspace.to_string_lossy().to_string(),
        workspace.to_string_lossy().replace('\\', "/"),
    ] {
        normalized = normalized.replace(&form, "<WORKSPACE>");
    }
    let pattern = Regex::new(r"(find_results|grep_matches|grep_counts|grep_files)_[0-9a-f]+\.txt")
        .expect("文件名归一正则");
    // `${1}` 必须带大括号：`$1_` 会被当作名为 `1_` 的分组。
    pattern
        .replace_all(&normalized, "${1}_<ID>.txt")
        .to_string()
}

fn freeze_mtimes(root: &Path) {
    let fixed = UNIX_EPOCH + Duration::from_secs(FIXED_MTIME);
    let times = FileTimes::new().set_modified(fixed);
    let mut stack = vec![root.to_path_buf()];
    let mut paths = vec![root.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(reader) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in reader.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            paths.push(path);
        }
    }
    for path in paths.iter().rev() {
        if let Ok(file) = std::fs::OpenOptions::new().write(true).open(path) {
            let _ = file.set_times(times);
        } else if let Ok(handle) = std::fs::File::open(path) {
            let _ = handle.set_times(times);
        }
    }
}

/// 与生成器一致的搜索用工作区结构。
fn prepare_search_workspace(name: &str) -> (WorkspacePaths, PathBuf) {
    let root = std::env::temp_dir().join(format!("omnicrawl-tui-search-parity-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src").join("deep")).expect("建 src/deep");
    std::fs::create_dir_all(root.join("node_modules").join("pkg")).expect("建 node_modules/pkg");
    std::fs::create_dir_all(root.join(".git")).expect("建 .git");
    let write = |path: PathBuf, text: &str| {
        std::fs::write(&path, text).expect("写文件");
    };
    write(
        root.join("src").join("agent.py"),
        "class Agent:\n    def run(self):\n        return self.tools\n",
    );
    write(
        root.join("src").join("helper.py"),
        "def helper():\n    return 1\n",
    );
    write(
        root.join("src").join("deep").join("tool.py"),
        "TOOL = 'agent'\n",
    );
    write(root.join("notes.md"), "Agent 说明\n无关内容\n");
    write(root.join("README.md"), "项目说明\n");
    write(
        root.join("node_modules").join("pkg").join("index.js"),
        "// Agent in node_modules\n",
    );
    write(root.join(".gitignore"), "node_modules/\n");
    write(root.join(".git").join("HEAD"), "ref: refs/heads/main\n");
    freeze_mtimes(&root);
    let workspace = WorkspacePaths::new(root.clone());
    let resolved = workspace.root().to_path_buf();
    (workspace, resolved)
}

/// 按用例的 `compare` 字段对照输出：默认逐字，`lines_sorted` 只比条目集合。
fn assert_output_matches(case: &Value, output: &str, workspace: &Path) {
    let normalized = normalize(output, workspace);
    if case.get("compare").and_then(Value::as_str) == Some("lines_sorted") {
        let mut actual: Vec<&str> = normalized.lines().collect();
        let expected_text = normalize(
            case["output"].as_str().unwrap_or_default(),
            Path::new("<none>"),
        );
        let mut expected: Vec<&str> = expected_text.lines().collect();
        actual.sort_unstable();
        expected.sort_unstable();
        assert_eq!(actual, expected, "用例 {:?}", case["arguments"]);
        return;
    }
    assert_eq!(
        normalized,
        normalize(
            case["output"].as_str().unwrap_or_default(),
            Path::new("<none>")
        ),
        "用例 {:?}",
        case["arguments"]
    );
}

fn compare_failure(case: &Value, error: &ToolError, workspace: &Path) {
    assert_eq!(case["ok"], false, "用例 {:?}", case["arguments"]);
    assert_eq!(
        normalize(&error.formatted(), workspace),
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
}

#[test]
#[cfg(windows)]
// 数据集里的期望值一律是 Windows 形态（路径分隔符、盘符与平台常量），
// 被测实现也按 win32 分支做字符串化：POSIX 主机上必然形态不符。
// 这条对照只在 Windows 主机上有意义。
fn list_cases_match_python() {
    let (paths, root) = prepare_search_workspace("list");
    for case in fixture()["search"]["list"].as_array().expect("list 用例") {
        match listing::list_files(&paths, &arguments(&case["arguments"])) {
            Ok(output) => {
                assert_eq!(case["ok"], true, "用例 {:?}", case["arguments"]);
                assert_output_matches(case, &output, &root);
            }
            Err(error) => compare_failure(case, &error, &root),
        }
    }
}

#[test]
#[cfg(windows)]
// 数据集里的期望值一律是 Windows 形态（路径分隔符、盘符与平台常量），
// 被测实现也按 win32 分支做字符串化：POSIX 主机上必然形态不符。
// 这条对照只在 Windows 主机上有意义。
fn find_cases_match_python() {
    let (paths, root) = prepare_search_workspace("find");
    for case in fixture()["search"]["find"].as_array().expect("find 用例") {
        match finding::find_files(&paths, &arguments(&case["arguments"])) {
            Ok(output) => {
                assert_eq!(case["ok"], true, "用例 {:?}", case["arguments"]);
                assert_output_matches(case, &output, &root);
            }
            Err(error) => compare_failure(case, &error, &root),
        }
    }
}

#[test]
#[cfg(windows)]
// 数据集里的期望值一律是 Windows 形态（路径分隔符、盘符与平台常量），
// 被测实现也按 win32 分支做字符串化：POSIX 主机上必然形态不符。
// 这条对照只在 Windows 主机上有意义。
fn grep_cases_match_python() {
    let (paths, root) = prepare_search_workspace("grep");
    for case in fixture()["search"]["grep"].as_array().expect("grep 用例") {
        match grep::grep(&paths, &arguments(&case["arguments"])) {
            Ok(output) => {
                assert_eq!(case["ok"], true, "用例 {:?}", case["arguments"]);
                assert_output_matches(case, &output, &root);
            }
            Err(error) => compare_failure(case, &error, &root),
        }
    }
}

#[test]
fn git_cases_match_python() {
    let root = std::env::temp_dir().join("omnicrawl-tui-git-parity");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("建 git 工作区");
    let paths = WorkspacePaths::new(root.clone());
    let cases = fixture()["git"].as_array().expect("git 用例").clone();

    // 前若干条是纯校验用例；最后一条 status 需要真仓库。
    let mut initialised = false;
    for case in cases {
        if case["arguments"]["action"] == "status" && !initialised {
            let init = std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(paths.root())
                .status();
            if init.map(|status| !status.success()).unwrap_or(true) {
                eprintln!("跳过：本机没有可用的 git");
                return;
            }
            initialised = true;
        }
        let result = git::git_tool(&paths, &arguments(&case["arguments"]));
        assert_eq!(result.ok, case["ok"], "用例 {:?}", case["arguments"]);
        assert_eq!(
            normalize(&result.output, &root),
            case["output"],
            "用例 {:?}",
            case["arguments"]
        );
    }
}
