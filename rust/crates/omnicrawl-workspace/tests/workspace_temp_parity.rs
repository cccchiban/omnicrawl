//! 与 Python `omnicrawl/workspace/temp.py` 真实现的对照测试。
//!
//! 数据集由 `python rust/tools/gen_workspace_temp_fixture.py` 生成：同一批输入（配置片段、
//! 目录配置字符串、子路径、待清理条目、`.last_cleanup` 时间戳）喂给真实现，这里照原样重建
//! 目录与文件后重放 Rust 实现并逐字段比对。改了任一侧都要重跑生成脚本。
//!
//! 数据集里的路径用 `<ROOT>` 占位，两侧各自替换成自己的临时根目录；报错文案里带路径的部分
//! 会把两侧的分隔符都折成 `/` 再比，因此临时目录名字与平台不影响比对。
//!
//! 两处刻意留白的差异（`README.md` 有记录）：Python 的 `_write_marker_files` /
//! `_touch_cleanup_marker` 走文本模式，在 Windows 上写出 CRLF；Rust 侧统一写 LF。数据集记的
//! 是「读回来」的文本（Python 的 `read_text` 会把 CRLF 折回 LF），因此两侧文本一致。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Duration, Local, TimeZone};
use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_workspace::temp::{
    agent_temp_status_label, delete_temp_entry, format_seconds, load_agent_temp_workspace_config,
    local_to_system_time, preserved_root_names, resolve_agent_temp_dir, resolve_temp_child,
    temp_workspace_readme, AgentTempWorkspace, AgentTempWorkspaceConfig,
    DEFAULT_AGENT_TEMP_CLEANUP_INTERVAL_HOURS, DEFAULT_AGENT_TEMP_DIRECTORY,
    DEFAULT_AGENT_TEMP_SUBDIRECTORIES, LAST_CLEANUP_FILENAME,
};
use serde_json::Value;

const ROOT_TOKEN: &str = "<ROOT>";

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("workspace_temp_parity.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("读取 {} 失败：{error}", path.display()));
    serde_json::from_str(&text).expect("数据集必须是合法 JSON")
}

fn group<'a>(data: &'a Value, name: &str) -> &'a Vec<Value> {
    data.get(name)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("数据集缺少分组：{name}"))
}

fn text_of(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("缺少字符串字段 {key}：{value}"))
        .to_string()
}

fn bool_of(value: &Value, key: &str) -> bool {
    value
        .get(key)
        .and_then(Value::as_bool)
        .unwrap_or_else(|| panic!("缺少布尔字段 {key}：{value}"))
}

fn float_of(value: &Value, key: &str) -> f64 {
    value
        .get(key)
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("缺少数值字段 {key}：{value}"))
}

fn int_of(value: &Value, key: &str) -> i64 {
    value
        .get(key)
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("缺少整数字段 {key}：{value}"))
}

fn strings_of(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("缺少字符串数组字段 {key}：{value}"))
        .iter()
        .map(|item| item.as_str().expect("数组元素必须是字符串").to_string())
        .collect()
}

/// 分隔符统一折成 `/`，让两侧只比结构不比平台。
fn slash(text: &str) -> String {
    text.replace('\\', "/")
}

/// 把数据集里的 `<ROOT>` 占位换成当前临时根目录。
fn expand(expected: &str, root: &Path) -> String {
    slash(expected).replace(ROOT_TOKEN, &slash(&root.to_string_lossy()))
}

/// 报错文案比对：把本次临时根目录折回 `<ROOT>`，再统一分隔符。
/// 取可选字符串字段：字段缺失（或为 `null`）返回 `None`，出现但形状不对直接判失败。
///
/// 对照数据集里的 `error` / `dir` 字段是「有则必须是字符串」的语义；直接 `match`
/// `Option<&Value>` 会让编译器要求穷举所有 JSON 形状，这里先收敛成 `Option<&str>`。
fn optional_text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    match value.get(key) {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.as_str()),
        other => panic!("{key} 字段形状不对：{other:?}"),
    }
}

fn normalize_error(text: &str, root: &Path) -> String {
    slash(text).replace(&slash(&root.to_string_lossy()), ROOT_TOKEN)
}

fn expand_path(expected: &str, root: &Path) -> PathBuf {
    PathBuf::from(expected.replace(ROOT_TOKEN, &root.to_string_lossy()))
}

/// 平台标记为 `any` 的用例两侧一致，其余只在对应平台跑。
fn platform_matches(case: &Value) -> bool {
    match case.get("platform").and_then(Value::as_str) {
        None | Some("any") => true,
        Some("windows") => cfg!(windows),
        Some("unix") => !cfg!(windows),
        Some(other) => panic!("数据集里有未知平台标记：{other}"),
    }
}

fn parse_local(text: &str) -> DateTime<Local> {
    let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S")
        .unwrap_or_else(|error| panic!("解析 {text} 失败：{error}"));
    Local
        .from_local_datetime(&naive)
        .single()
        .unwrap_or_else(|| panic!("{text} 不是唯一的本地时间"))
}

/// 临时目录：测试结束即删除，避免污染系统临时目录。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut base = std::env::temp_dir();
        base.push(format!(
            "oc-temp-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&base).expect("创建临时目录");
        Self {
            path: omnicrawl_workspace::resolve_path(&base),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 递归收集 `<ROOT>/...` → `dir`/`file`，与数据集里的 `after` 形状一致。
fn walk(root: &Path, base: &Path) -> BTreeMap<String, String> {
    let mut collected = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory).expect("读目录") {
            let entry = entry.expect("目录项");
            let path = entry.path();
            let is_dir = entry.file_type().expect("文件类型").is_dir();
            let relative = path
                .strip_prefix(base)
                .unwrap_or_else(|_| panic!("{} 不在 {} 内", path.display(), base.display()));
            collected.insert(
                format!("{ROOT_TOKEN}/{}", slash(&relative.to_string_lossy())),
                if is_dir { "dir" } else { "file" }.to_string(),
            );
            if is_dir {
                stack.push(path);
            }
        }
    }
    collected
}

fn config_from(value: &Value) -> AgentTempWorkspaceConfig {
    AgentTempWorkspaceConfig {
        enabled: bool_of(value, "enabled"),
        directory: text_of(value, "directory"),
        cleanup_enabled: bool_of(value, "cleanup_enabled"),
        cleanup_interval_hours: int_of(value, "cleanup_interval_hours"),
        subdirectories: DEFAULT_AGENT_TEMP_SUBDIRECTORIES
            .iter()
            .map(|item| item.to_string())
            .collect(),
    }
}

#[test]
fn constants_match_python() {
    let data = fixture();
    let constants = data.get("constants").expect("缺少 constants");

    assert_eq!(
        DEFAULT_AGENT_TEMP_DIRECTORY,
        text_of(constants, "default_directory")
    );
    assert_eq!(
        DEFAULT_AGENT_TEMP_CLEANUP_INTERVAL_HOURS,
        int_of(constants, "default_interval_hours")
    );
    assert_eq!(
        DEFAULT_AGENT_TEMP_SUBDIRECTORIES
            .iter()
            .map(|item| item.to_string())
            .collect::<Vec<_>>(),
        strings_of(constants, "subdirectories")
    );
    assert_eq!(
        LAST_CLEANUP_FILENAME,
        text_of(constants, "last_cleanup_filename")
    );
    assert_eq!(
        preserved_root_names().into_iter().collect::<Vec<_>>(),
        strings_of(constants, "preserved_root_names")
    );
    assert_eq!(
        temp_workspace_readme(),
        text_of(constants, "readme"),
        "README 正文必须逐字符一致"
    );

    let defaults = AgentTempWorkspaceConfig::default();
    let expected = constants
        .get("config_defaults")
        .expect("缺少 config_defaults");
    assert_eq!(defaults.enabled, bool_of(expected, "enabled"));
    assert_eq!(defaults.directory, text_of(expected, "directory"));
    assert_eq!(
        defaults.cleanup_enabled,
        bool_of(expected, "cleanup_enabled")
    );
    assert_eq!(
        defaults.cleanup_interval_hours,
        int_of(expected, "cleanup_interval_hours")
    );
}

#[test]
fn config_cases_match_python() {
    let data = fixture();
    let root = TempDir::new("temp-config");
    let env = ConfigEnvironment::new(root.path(), std::env::consts::OS);

    for case in group(&data, "config_cases") {
        let name = text_of(case, "name");
        // 每条用例各用一个目录，免得「文件不存在」这条要看上一条用例的残留。
        let directory = root.path().join("config").join(&name);
        std::fs::create_dir_all(&directory).expect("创建用例目录");
        let path = directory.join("config.toml");
        match case.get("file") {
            None | Some(Value::Null) => assert!(!path.exists()),
            Some(Value::String(content)) => {
                std::fs::write(&path, content.as_bytes()).expect("写配置")
            }
            other => panic!("{name} 的 file 字段形状不对：{other:?}"),
        }

        let actual = load_agent_temp_workspace_config(&env, Some(&path));
        match optional_text(case, "error") {
            Some(expected) => {
                let error = actual
                    .err()
                    .unwrap_or_else(|| panic!("{name} 应当报错，实际成功"));
                assert_eq!(
                    normalize_error(error.message(), root.path()),
                    normalize_error(expected, root.path()),
                    "报错文案不一致：{name}"
                );
            }
            None => {
                let config = actual.unwrap_or_else(|error| panic!("{name} 报错了：{error}"));
                let expected = case.get("config").expect("缺少 config");
                assert_eq!(config.enabled, bool_of(expected, "enabled"), "{name}");
                assert_eq!(config.directory, text_of(expected, "directory"), "{name}");
                assert_eq!(
                    config.cleanup_enabled,
                    bool_of(expected, "cleanup_enabled"),
                    "{name}"
                );
                assert_eq!(
                    config.cleanup_interval_hours,
                    int_of(expected, "cleanup_interval_hours"),
                    "{name}"
                );
            }
        }
    }
}

#[test]
fn resolve_cases_match_python() {
    let data = fixture();
    let root = TempDir::new("temp-resolve");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("创建工作区");

    for case in group(&data, "resolve_cases") {
        if !platform_matches(case) {
            continue;
        }
        let directory = text_of(case, "directory");
        let actual = resolve_agent_temp_dir(&workspace, &directory);
        match optional_text(case, "dir") {
            Some(expected) => {
                let resolved =
                    actual.unwrap_or_else(|error| panic!("{directory:?} 报错了：{error}"));
                assert_eq!(
                    slash(&resolved.to_string_lossy()),
                    expand(expected, root.path()),
                    "解析结果不一致：{directory:?}"
                );
            }
            None => {
                let error = actual
                    .err()
                    .unwrap_or_else(|| panic!("{directory:?} 应当报错，实际成功"));
                assert_eq!(
                    normalize_error(error.message(), root.path()),
                    normalize_error(&text_of(case, "error"), root.path()),
                    "报错文案不一致：{directory:?}"
                );
            }
        }
    }
}

#[test]
fn child_cases_match_python() {
    let data = fixture();
    let root = TempDir::new("temp-child");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("创建工作区");
    let instance = AgentTempWorkspace::new(workspace.as_path(), None).expect("构造临时工作区句柄");

    for case in group(&data, "child_cases") {
        if !platform_matches(case) {
            continue;
        }
        let relative = text_of(case, "relative");
        let actual = resolve_temp_child(instance.root(), &relative);
        match optional_text(case, "dir") {
            Some(expected) => {
                let resolved =
                    actual.unwrap_or_else(|error| panic!("{relative:?} 报错了：{error}"));
                assert_eq!(
                    slash(&resolved.to_string_lossy()),
                    expand(expected, root.path()),
                    "子路径解析不一致：{relative:?}"
                );
            }
            None => {
                let error = actual
                    .err()
                    .unwrap_or_else(|| panic!("{relative:?} 应当报错，实际成功"));
                assert_eq!(
                    normalize_error(error.message(), root.path()),
                    normalize_error(&text_of(case, "error"), root.path()),
                    "报错文案不一致：{relative:?}"
                );
            }
        }
    }
}

#[test]
fn delete_cases_match_python() {
    let data = fixture();
    let root = TempDir::new("temp-delete");
    let workspace = root.path().join("delete");
    let instance = AgentTempWorkspace::new(workspace.as_path(), None).expect("构造临时工作区句柄");
    instance.ensure().expect("初始化临时目录");
    // 与数据集里的三档对应：根目录自身、临时目录外的文件、临时目录内的普通文件。
    std::fs::write(workspace.join("outside.txt"), b"outside").expect("写临时目录外文件");
    std::fs::write(instance.root().join("files").join("inside.txt"), b"inside")
        .expect("写临时目录内文件");

    for case in group(&data, "delete_cases") {
        let entry = expand_path(&text_of(case, "entry"), root.path());
        let actual = delete_temp_entry(instance.root(), &entry);
        match optional_text(case, "error") {
            Some(expected) => {
                let error = match actual {
                    Ok(()) => panic!("{} 应当被拒绝，实际成功", entry.display()),
                    Err(error) => error,
                };
                assert_eq!(
                    normalize_error(error.message(), root.path()),
                    normalize_error(expected, root.path()),
                    "报错文案不一致：{}",
                    entry.display()
                );
            }
            None => {
                assert!(
                    case.get("deleted")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    "数据集缺少 deleted 标记"
                );
                if let Err(error) = actual {
                    panic!("{} 应当删除成功：{error}", entry.display());
                }
                assert!(!entry.exists(), "{} 仍存在", entry.display());
            }
        }
    }
}

#[test]
fn cleanup_matches_python() {
    let data = fixture();
    let root = TempDir::new("temp-cleanup");
    let case = data.get("cleanup").expect("缺少 cleanup 分组");
    let cleaned_at = parse_local(&text_of(case, "now"));

    let workspace = root.path().join("cleanup");
    let instance = AgentTempWorkspace::new(workspace.as_path(), None).expect("构造临时工作区句柄");
    instance.ensure().expect("初始化临时目录");
    for (relative, content) in case
        .get("tree")
        .and_then(Value::as_object)
        .expect("缺少清理用例的 tree")
    {
        let target = instance.root().join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("建父目录");
        }
        std::fs::write(
            &target,
            content.as_str().expect("tree 内容是字符串").as_bytes(),
        )
        .expect("铺清理用例的文件");
    }

    let result = instance.clean(Some(cleaned_at)).expect("清理");
    let expected_deleted = strings_of(case, "deleted_entries");
    assert_eq!(
        result.deleted_entries, expected_deleted,
        "删除清单或顺序不一致"
    );
    assert_eq!(
        result.failed_entries,
        strings_of(case, "failed_entries"),
        "不应有删除失败"
    );
    assert_eq!(
        result.root.as_path(),
        instance.root(),
        "结果里的根目录应当一致"
    );

    assert_eq!(
        std::fs::read_to_string(instance.root().join(LAST_CLEANUP_FILENAME)).expect("读标记"),
        text_of(case, "marker_text"),
        "清理标记内容不一致"
    );
    assert_eq!(
        std::fs::read_to_string(instance.root().join("README.md")).expect("读 README"),
        text_of(case, "readme"),
        "README 内容不一致"
    );
    assert_eq!(
        std::fs::read_to_string(instance.root().join(".gitignore")).expect("读 .gitignore"),
        text_of(case, "gitignore"),
        ".gitignore 内容不一致"
    );

    let expected_after: BTreeMap<String, String> = case
        .get("after")
        .and_then(Value::as_object)
        .expect("缺少清理用例的 after")
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                value.as_str().expect("after 值是字符串").to_string(),
            )
        })
        .collect();
    assert_eq!(walk(instance.root(), root.path()), expected_after);
}

#[test]
fn due_cases_match_python() {
    let data = fixture();
    let root = TempDir::new("temp-due");
    let due = data.get("due").expect("缺少 due 分组");
    let base = parse_local(&text_of(due, "base_time"));
    assert_eq!(
        format_seconds(base),
        text_of(due, "base_time"),
        "ISO 秒级格式必须一致"
    );

    let workspace = root.path().join("due");
    let instance = AgentTempWorkspace::new(workspace.as_path(), None).expect("构造临时工作区句柄");
    instance.ensure().expect("初始化临时目录");
    assert_eq!(
        instance.config().cleanup_interval_hours,
        int_of(due, "interval_hours")
    );

    let marker = instance.root().join(LAST_CLEANUP_FILENAME);
    std::fs::write(&marker, b"last_cleanup=seed\n").expect("写标记");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&marker)
        .expect("打开标记")
        .set_modified(local_to_system_time(base))
        .expect("设置标记时间");
    assert_eq!(instance.last_cleanup_time(), Some(base));

    for case in group(due, "cases") {
        let delta = float_of(case, "delta_seconds");
        let now = base + Duration::milliseconds((delta * 1000.0).round() as i64);
        assert_eq!(
            instance.is_cleanup_due(Some(now)),
            bool_of(case, "due"),
            "到期判定不一致：{delta}"
        );
        let expected_seconds = float_of(case, "seconds_until_next");
        let actual_seconds = instance.seconds_until_next_cleanup(Some(now));
        // 文件时间的存储精度两侧不完全一致，允许毫秒级误差。
        assert!(
            (actual_seconds - expected_seconds).abs() < 0.01,
            "剩余秒数不一致：{delta} 期望 {expected_seconds} 实际 {actual_seconds}"
        );
    }
}

#[test]
fn status_labels_match_python() {
    let data = fixture();
    for case in group(&data, "status_labels") {
        let config = config_from(case.get("config").expect("缺少 config"));
        assert_eq!(
            agent_temp_status_label(&config),
            text_of(case, "label"),
            "状态文案不一致：{:?}",
            config
        );
    }
}
