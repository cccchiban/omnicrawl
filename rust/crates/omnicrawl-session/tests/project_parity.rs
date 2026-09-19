//! 项目列表存储的跨语言 parity：期望值来自 Python 真实现。
//!
//! 两组：纯函数（展示名清洗、路径键、隔离工作树判定、扫描排除、路径归一、git 根、
//! 条目解析、变量展开）与 ProjectStore 流程轨迹（创建 / 导入 / 改名 / 置顶 / 移除 /
//! 扫描 / 总览 / 损坏文件）。路径里的临时根在数据集里是 `<ROOT>` / `<TEMP>` 占位，
//! 两侧各自还原成自己的临时目录再比对。

use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

use omnicrawl_session::project::{
    clean_project_name, expand_vars, git_root, is_scan_excluded, normalize_project_path, path_key,
    under_agent_worktrees, OverviewSessionEntry, ProjectEntry, ProjectStore,
};
use omnicrawl_session::{parse_datetime, SessionStoreError};
use serde_json::{json, Map, Value};

const FIXTURE: &str = include_str!("fixtures/project_parity.json");
const ROOT_PLACEHOLDER: &str = "<ROOT>";
const TEMP_PLACEHOLDER: &str = "<TEMP>";
const FIXED_NOW: &str = "2026-01-02T03:04:05.123456+00:00";

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

struct Env {
    root: PathBuf,
    temp: PathBuf,
}

fn env() -> &'static Env {
    static ENV: OnceLock<Env> = OnceLock::new();
    ENV.get_or_init(|| {
        let raw_root =
            std::env::temp_dir().join(format!("omnicrawl-project-parity-{}", std::process::id()));
        fs::create_dir_all(&raw_root).expect("建工作根");
        let root =
            PathBuf::from(normalize_project_path(&raw_root.to_string_lossy()).expect("解析工作根"));
        let temp = PathBuf::from(
            normalize_project_path(&std::env::temp_dir().to_string_lossy()).expect("解析临时根"),
        );
        let prepared = Env { root, temp };
        prepared.build_layout();
        prepared
    })
}

impl Env {
    fn build_layout(&self) {
        for relative in [
            "data/alpha",
            "data/beta",
            "data/scanned-a",
            "data/scanned-b",
            "plain/keep",
            "repo/sub/deep",
            "wt/sub",
            "broken",
        ] {
            fs::create_dir_all(self.root.join(relative)).expect("建目录");
        }
        fs::write(self.root.join("data").join("plain.txt"), "x").expect("写文件");
        let repo_git = self.root.join("repo").join(".git");
        fs::create_dir_all(&repo_git).expect("建 .git");
        fs::write(repo_git.join("HEAD"), "ref: refs/heads/main\n").expect("写 HEAD");
        fs::create_dir_all(self.root.join("broken").join(".git")).expect("建断裂 .git");
        fs::write(
            self.root.join("wt").join(".git"),
            "gitdir: ../repo/.git/worktrees/wt\n",
        )
        .expect("写 gitfile");
    }

    fn unmask(&self, text: &str) -> String {
        text.replace(ROOT_PLACEHOLDER, &self.root.to_string_lossy())
            .replace(TEMP_PLACEHOLDER, &self.temp.to_string_lossy())
    }

    fn mask(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (base, placeholder) in [
            (self.root.to_string_lossy().to_string(), ROOT_PLACEHOLDER),
            (self.temp.to_string_lossy().to_string(), TEMP_PLACEHOLDER),
        ] {
            out = out.replace(&base.replace('\\', "\\\\"), placeholder);
            out = out.replace(&base, placeholder);
        }
        out
    }

    fn unmask_value(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.unmask(text)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.unmask_value(item)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, item)| (key.clone(), self.unmask_value(item)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    fn mask_value(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.mask(text)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.mask_value(item)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, item)| (key.clone(), self.mask_value(item)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

#[test]
fn clean_name_matches_python() {
    let fixture = fixture();
    for case in fixture["pure"]["clean_name"]
        .as_array()
        .expect("clean_name")
    {
        let input = case["input"].as_str().expect("input");
        assert_eq!(
            clean_project_name(input).expect("应成功"),
            case["expected"].as_str().expect("expected"),
            "展示名清洗（{input}）"
        );
    }
    for case in fixture["pure"]["clean_name_errors"]
        .as_array()
        .expect("clean_name_errors")
    {
        let input = case["input"].as_str().expect("input");
        let error = clean_project_name(input).expect_err("应报错");
        assert_eq!(
            error.to_string(),
            case["error"].as_str().expect("error"),
            "展示名错误（{input}）"
        );
    }
}

#[test]
fn path_key_matches_python() {
    let fixture = fixture();
    for case in fixture["pure"]["path_key"].as_array().expect("path_key") {
        let input = case["input"].as_str().expect("input");
        assert_eq!(
            path_key(input),
            case["expected"].as_str().expect("expected"),
            "路径键（{input}）"
        );
    }
}

#[test]
fn agent_worktrees_judgement_matches_python() {
    let fixture = fixture();
    for case in fixture["pure"]["under_agent_worktrees"]
        .as_array()
        .expect("under_agent_worktrees")
    {
        let input = case["input"].as_str().expect("input");
        assert_eq!(
            under_agent_worktrees(input),
            case["expected"].as_bool().expect("expected"),
            "隔离工作树判定（{input}）"
        );
    }
}

#[test]
fn normalize_path_matches_python() {
    let fixture = fixture();
    let env = env();
    for case in fixture["pure"]["normalize_path"]
        .as_array()
        .expect("normalize_path")
    {
        let input = env.unmask(case["input"].as_str().expect("input"));
        let produced = env.mask(&normalize_project_path(&input).expect("应成功"));
        assert_eq!(
            produced,
            case["expected"].as_str().expect("expected"),
            "路径归一（{input}）"
        );
    }
    for case in fixture["pure"]["normalize_path_errors"]
        .as_array()
        .expect("normalize_path_errors")
    {
        let input = case["input"].as_str().expect("input");
        let error = normalize_project_path(input).expect_err("应报错");
        assert_eq!(
            error.to_string(),
            case["error"].as_str().expect("error"),
            "路径错误（{input}）"
        );
    }
}

#[test]
fn entry_from_dict_matches_python() {
    let fixture = fixture();
    let env = env();
    for case in fixture["pure"]["entry_from_dict"]
        .as_array()
        .expect("entry_from_dict")
    {
        let input = env.unmask_value(&case["input"]);
        let map: &Map<String, Value> = input.as_object().expect("input 是对象");
        let entry = ProjectEntry::from_dict(map).expect("应成功");
        assert_eq!(
            env.mask_value(&entry.to_value()),
            case["expected"],
            "条目解析（{}）",
            serde_json::to_string(&case["input"]).unwrap_or_default()
        );
    }
    for case in fixture["pure"]["entry_from_dict_errors"]
        .as_array()
        .expect("entry_from_dict_errors")
    {
        let input = env.unmask_value(&case["input"]);
        let map: &Map<String, Value> = input.as_object().expect("input 是对象");
        let error = ProjectEntry::from_dict(map).expect_err("应报错");
        assert_eq!(
            env.mask(&error.to_string()),
            case["error"].as_str().expect("error"),
            "条目错误（{}）",
            serde_json::to_string(&case["input"]).unwrap_or_default()
        );
    }
}

#[test]
fn scan_excluded_matches_python() {
    let fixture = fixture();
    let env = env();
    for case in fixture["pure"]["scan_excluded"]
        .as_array()
        .expect("scan_excluded")
    {
        let input = env.unmask(case["input"].as_str().expect("input"));
        assert_eq!(
            is_scan_excluded(&input),
            case["expected"].as_bool().expect("expected"),
            "扫描排除（{input}）"
        );
    }
}

#[test]
fn git_root_matches_python() {
    let fixture = fixture();
    let env = env();
    for case in fixture["pure"]["git_root"].as_array().expect("git_root") {
        let input = env.unmask(case["input"].as_str().expect("input"));
        let produced = git_root(&input).map(|value| env.mask(&value));
        let expected = case["expected"].as_str().map(|value| env.mask(value));
        assert_eq!(produced, expected, "git 根（{input}）");
    }
}

#[test]
fn expand_vars_matches_python() {
    let fixture = fixture();
    for case in fixture["pure"]["expand_vars"]
        .as_array()
        .expect("expand_vars")
    {
        let input = case["input"].as_str().expect("input");
        assert_eq!(
            expand_vars(input),
            case["expected"].as_str().expect("expected"),
            "变量展开（{input}）"
        );
    }
}

#[test]
fn store_traces_match_python() {
    let fixture = fixture();
    let env = env();
    let cases = fixture["store"].as_array().expect("store");
    assert!(!cases.is_empty(), "数据集为空");

    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let store = ProjectStore::open(env.root.join(label));
        let mut results: Vec<Value> = Vec::new();

        for op in case["ops"].as_array().expect("ops") {
            let kind = op["op"].as_str().expect("op");
            match kind {
                "write_file" => {
                    fs::create_dir_all(store.root()).expect("建目录");
                    fs::write(store.path(), op["text"].as_str().expect("text")).expect("写文件");
                    results.push(json!({"op": kind, "ok": true, "value": Value::Null}));
                }
                "list_projects" => {
                    results.push(outcome(env, kind, entries_value(store.list_projects())));
                }
                "overview" => {
                    let sessions =
                        overview_entries(env, op["sessions"].as_array().expect("sessions"));
                    let recent_limit = op["recent_limit"].as_u64().map(|value| value as usize);
                    let produced = store.project_overview(&sessions, recent_limit, 0);
                    results.push(outcome(
                        env,
                        kind,
                        produced.map(|items| {
                            Value::Array(
                                items
                                    .iter()
                                    .map(|item| {
                                        serde_json::to_value(item).expect("总览条目可序列化")
                                    })
                                    .collect(),
                            )
                        }),
                    ));
                }
                "scan_projects" => {
                    let roots: Vec<String> = op["roots"]
                        .as_array()
                        .expect("roots")
                        .iter()
                        .map(|item| env.unmask(item.as_str().expect("root")))
                        .collect();
                    let current = op["current"].as_str().map(|item| env.unmask(item));
                    let produced =
                        store.scan_projects(&roots, current.as_deref(), Some(fixed_now()));
                    results.push(outcome(env, kind, entries_value(produced)));
                }
                other => {
                    let result: Result<Value, SessionStoreError> = match other {
                        "create_project" => store
                            .create_project(
                                op["name"].as_str().expect("name"),
                                &env.unmask(op["path"].as_str().expect("path")),
                                Some(fixed_now()),
                            )
                            .map(|entry| entry.to_value()),
                        "import_project" => store
                            .import_project(
                                op["name"].as_str().expect("name"),
                                &env.unmask(op["path"].as_str().expect("path")),
                                Some(fixed_now()),
                            )
                            .map(|entry| entry.to_value()),
                        "rename_project" => store
                            .rename_project(
                                &env.unmask(op["path"].as_str().expect("path")),
                                op["name"].as_str().expect("name"),
                                Some(fixed_now()),
                            )
                            .map(|entry| entry.to_value()),
                        "pin_project" => store
                            .pin_project(
                                &env.unmask(op["path"].as_str().expect("path")),
                                op["pinned"].as_bool().expect("pinned"),
                                Some(fixed_now()),
                            )
                            .map(|entry| entry.to_value()),
                        "toggle_project_pin" => store
                            .toggle_project_pin(
                                &env.unmask(op["path"].as_str().expect("path")),
                                Some(fixed_now()),
                            )
                            .map(|entry| entry.to_value()),
                        "remove_project" => store
                            .remove_project(&env.unmask(op["path"].as_str().expect("path")))
                            .map(|_| Value::Null),
                        unknown => panic!("未知的 op：{unknown}"),
                    };
                    results.push(outcome(env, other, result));
                }
            }
        }

        assert_eq!(
            Value::Array(results),
            case["results"],
            "步骤结果（{label}）"
        );

        let produced_file = if store.path().exists() {
            env.mask(&fs::read_to_string(store.path()).expect("读 projects.json"))
        } else {
            String::new()
        };
        assert_eq!(
            produced_file,
            case["file"].as_str().expect("file"),
            "落盘内容（{label}）"
        );
    }
}

fn outcome(env: &Env, kind: &str, result: Result<Value, SessionStoreError>) -> Value {
    match result {
        Ok(value) => json!({"op": kind, "ok": true, "value": env.mask_value(&value)}),
        Err(error) => json!({"op": kind, "ok": false, "error": env.mask(&error.to_string())}),
    }
}

fn entries_value(
    entries: Result<Vec<ProjectEntry>, SessionStoreError>,
) -> Result<Value, SessionStoreError> {
    entries.map(|items| Value::Array(items.iter().map(ProjectEntry::to_value).collect()))
}

fn overview_entries(env: &Env, items: &[Value]) -> Vec<OverviewSessionEntry> {
    items
        .iter()
        .map(|item| OverviewSessionEntry {
            workspace_root: Some(env.unmask(item["workspace_root"].as_str().unwrap_or(""))),
            updated_at: item["updated_at"]
                .as_str()
                .map(|text| parse_datetime(text).expect("会话时间戳")),
            session_id: item["session_id"].as_str().unwrap_or("").to_string(),
            title: item["title"].as_str().unwrap_or("").to_string(),
        })
        .collect()
}

fn fixed_now() -> chrono::DateTime<chrono::Utc> {
    parse_datetime(FIXED_NOW).expect("固定时刻")
}
