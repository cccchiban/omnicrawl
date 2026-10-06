//! 运行配置仓库（`omnicrawl/config/core/runtime.py`）的对照测试。
//!
//! 数据集是冻结的对照契约。
//! 临时目录里的绝对路径在数据集里写作 `{root}`，这里替换成测试自己的临时根；
//! 路径形态按 Windows 对照（`pathlib` 的字符串化规则已在 `runtime` 里对齐）。

use std::fs;
use std::path::{Path, PathBuf};

use omnicrawl_config::core::runtime as R;
use omnicrawl_config::toml::{Table, Value as TomlValue};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/config_runtime_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("解析对照数据集")
}

fn temp_root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("oc-config-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("建立临时根目录");
    path
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn env_for_case(case: &Value, home: &str, root: Option<&str>) -> R::ConfigEnvironment {
    let mut env = R::ConfigEnvironment::new(home, "win32");
    if let Some(map) = case["env"].as_object() {
        for (name, value) in map {
            if let Some(text) = value.as_str() {
                let text = match root {
                    Some(root) => text.replace("{root}", root),
                    None => text.to_string(),
                };
                env = env.with_env_value(name, &text);
            }
        }
    }
    env
}

fn json_to_value(value: &Value) -> TomlValue {
    match value {
        // 调用方已过滤 null；这里兜底成一个可比较的值。
        Value::Null => TomlValue::Boolean(false),
        Value::Bool(flag) => TomlValue::Boolean(*flag),
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                TomlValue::Integer(integer)
            } else if let Some(float) = number.as_f64() {
                TomlValue::Float(float)
            } else {
                TomlValue::Integer(0)
            }
        }
        Value::String(text) => TomlValue::String(text.clone()),
        Value::Array(items) => TomlValue::Array(
            items
                .iter()
                .filter(|item| !item.is_null())
                .map(json_to_value)
                .collect(),
        ),
        Value::Object(_) => TomlValue::Table(json_to_table(value)),
    }
}

fn json_to_table(value: &Value) -> Table {
    let mut table = Table::new();
    if let Some(map) = value.as_object() {
        for (key, item) in map {
            // 与 Python 的 `_strip_none` 同义：null 键与 null 元素不写进 TOML。
            if item.is_null() {
                continue;
            }
            table.insert(key.clone(), json_to_value(item));
        }
    }
    table
}

#[test]
fn directories_match_python() {
    let data = fixture();
    let dirs = &data["dirs"];
    let home = dirs["home"].as_str().expect("home");
    let env = R::ConfigEnvironment::new(home, "win32");
    assert_eq!(
        path_text(&R::user_config_dir(&env)),
        dirs["user_config_dir"].as_str().unwrap()
    );
    assert_eq!(
        path_text(&R::global_agents_path(&env)),
        dirs["global_agents"].as_str().unwrap()
    );
    assert_eq!(
        path_text(&R::default_config_path(&env)),
        dirs["default_config"].as_str().unwrap()
    );
    assert_eq!(
        path_text(&R::default_models_path(&env)),
        dirs["default_models"].as_str().unwrap()
    );
    assert_eq!(
        path_text(&R::default_subagents_path(&env)),
        dirs["default_subagents"].as_str().unwrap()
    );
    assert_eq!(
        path_text(&R::default_toml_config_path(&env)),
        dirs["default_toml_config"].as_str().unwrap()
    );
}

#[test]
fn legacy_directories_match_python() {
    let data = fixture();
    let home = data["dirs"]["home"].as_str().unwrap();
    for case in data["legacy_dirs"].as_array().unwrap() {
        let platform = case["platform"].as_str().unwrap();
        let mut env = R::ConfigEnvironment::new(home, platform);
        for name in ["APPDATA", "XDG_CONFIG_HOME"] {
            if let Some(text) = case["env"][name].as_str() {
                env = env.with_env_value(name, text);
            }
        }
        let actual: Vec<String> = R::legacy_user_config_dirs(&env)
            .iter()
            .map(|path| path_text(path))
            .collect();
        let expected: Vec<String> = case["expected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item.as_str().unwrap().to_string())
            .collect();
        assert_eq!(actual, expected, "用例：{platform} / {:?}", case["env"]);
    }
}

#[test]
fn resolve_paths_match_python() {
    let data = fixture();
    for case in data["resolves"].as_array().unwrap() {
        let home = case["managed_home"].as_str().unwrap();
        let env = env_for_case(case, home, None);
        let explicit = case["explicit"].as_str().map(PathBuf::from);
        let func = case["func"].as_str().unwrap();
        let result = match func {
            "config" => R::resolve_config_path(&env, explicit.as_deref()),
            "models" => R::resolve_models_path(&env, explicit.as_deref()),
            "subagents" => R::resolve_subagents_path(&env, explicit.as_deref()),
            "config_write" => R::resolve_config_write_path(&env, explicit.as_deref()),
            "models_write" => R::resolve_models_write_path(&env, explicit.as_deref()),
            "subagents_write" => R::resolve_subagents_write_path(&env, explicit.as_deref()),
            other => panic!("未知的解析函数：{other}"),
        };
        let name = case["name"].as_str().unwrap();
        match (result, case.get("expected_path")) {
            (Ok(path), Some(expected)) => {
                assert_eq!(path_text(&path), expected.as_str().unwrap(), "用例：{name}");
            }
            (Err(error), _) => {
                assert_eq!(
                    error.message(),
                    case["error"].as_str().unwrap(),
                    "用例：{name}"
                );
            }
            (Ok(path), None) => panic!("用例 {name} 应报错，实际得到 {}", path.display()),
        }
    }
}

#[test]
fn get_section_matches_python() {
    let data = fixture();
    for case in data["sections"].as_array().unwrap() {
        let table = json_to_table(&case["data"]);
        let key = case["key"].as_str().unwrap();
        let name = case["name"].as_str().unwrap();
        match (R::get_section(&table, key), case.get("expected")) {
            (Ok(section), Some(expected)) => {
                assert_eq!(section, json_to_table(expected), "用例：{name}");
            }
            (Err(error), _) => {
                assert_eq!(
                    error.message(),
                    case["error"].as_str().unwrap(),
                    "用例：{name}"
                );
            }
            (Ok(_), None) => panic!("用例 {name} 应报错"),
        }
    }
}

#[test]
fn load_config_data_matches_python() {
    let data = fixture();
    let root = temp_root("loads");
    for (index, case) in data["loads"].as_array().unwrap().iter().enumerate() {
        let case_root = root.join(format!("case-{index}"));
        let case_root_text = path_text(&case_root);
        let user_dir = case_root.join(R::USER_CONFIG_DIRNAME);
        let user_dir_text = path_text(&user_dir);
        fs::create_dir_all(&user_dir).expect("建立用户配置目录");
        if let Some(layout) = case["layout"].as_object() {
            for (relative, content) in layout {
                let path = user_dir.join(relative);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).expect("建立目录");
                }
                fs::write(&path, content.as_str().unwrap().as_bytes()).expect("写配置文件");
            }
        }
        let env = env_for_case(case, &case_root_text, Some(&user_dir_text));
        let explicit = case["explicit"].as_str().map(|name| user_dir.join(name));
        let name = case["name"].as_str().unwrap();
        let result = R::load_config_data(&env, explicit.as_deref());
        let root_text = user_dir_text;
        match (result, case.get("expected")) {
            (Ok(table), Some(expected)) => {
                assert_eq!(table, json_to_table(expected), "用例：{name}");
            }
            (Err(error), _) => {
                let actual = error.message().to_string();
                if let Some(full) = case.get("error").and_then(|item| item.as_str()) {
                    assert_eq!(actual, full.replace("{root}", &root_text), "用例：{name}");
                } else {
                    let prefix = case["error_prefix"]
                        .as_str()
                        .unwrap()
                        .replace("{root}", &root_text);
                    assert!(actual.starts_with(&prefix), "用例：{name}，实际：{actual}");
                }
            }
            (Ok(_), None) => panic!("用例 {name} 应报错"),
        }
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn dump_document_matches_python() {
    let data = fixture();
    for case in data["dumps"].as_array().unwrap() {
        let table = json_to_table(&case["data"]);
        let name = case["name"].as_str().unwrap();
        assert_eq!(
            omnicrawl_config::toml::dump_document(&table),
            case["text"].as_str().unwrap(),
            "用例：{name}"
        );
        assert_eq!(
            R::dump_toml_text(&table),
            case["text"].as_str().unwrap(),
            "用例：{name}"
        );
    }
}

fn snapshot(root: &Path) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let items = match fs::read_dir(&directory) {
            Ok(items) => items,
            Err(_) => continue,
        };
        for item in items.flatten() {
            let path = item.path();
            let relative = path
                .strip_prefix(root)
                .expect("相对路径")
                .to_string_lossy()
                .replace('\\', "/");
            if path.is_dir() {
                entries.push(format!("{relative}/"));
                stack.push(path);
            } else {
                let content = fs::read_to_string(&path).unwrap_or_default();
                let content = content.replace("\r\n", "\n");
                entries.push(format!("{relative} = {}", content.trim()));
            }
        }
    }
    // pathlib 在 Windows 上按 casefold 后的字符串排序。
    entries.sort_by_key(|entry| entry.to_lowercase());
    entries
}

fn write_layout(root: &Path, layout: &Value) {
    if let Some(map) = layout.as_object() {
        for (relative, content) in map {
            let path = root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("建立目录");
            }
            fs::write(&path, content.as_str().unwrap().as_bytes()).expect("写文件");
        }
    }
}

#[test]
fn migrate_legacy_user_config_matches_python() {
    let data = fixture();
    let root = temp_root("migrate");
    for (index, case) in data["migrations"].as_array().unwrap().iter().enumerate() {
        let case_root = root.join(format!("case-{index}"));
        fs::create_dir_all(&case_root).expect("建立用例根目录");
        write_layout(&case_root, &case["layout"]);
        write_layout(&case_root, &case["target_layout"]);
        let user_dir = case_root.join(R::USER_CONFIG_DIRNAME);
        fs::create_dir_all(&user_dir).expect("建立用户配置目录");
        let legacy_dirs: Vec<PathBuf> = case["legacy_names"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| case_root.join(item.as_str().unwrap()))
            .collect();
        let env = R::ConfigEnvironment::new(path_text(&case_root), "win32");
        R::migrate_legacy_user_config(&env, Some(&legacy_dirs)).expect("迁移应成功");
        let expected: Vec<String> = case["expected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item.as_str().unwrap().to_string())
            .collect();
        assert_eq!(snapshot(&case_root), expected, "用例：{}", case["name"]);
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn migrate_conflict_backup_names_match_python() {
    let data = fixture();
    let root = temp_root("conflict");
    for case in data["conflict_sequences"].as_array().unwrap() {
        let target = root.join(R::USER_CONFIG_DIRNAME);
        fs::create_dir_all(&target).expect("建立用户配置目录");
        fs::write(target.join("config.toml"), b"a = 1\n").expect("写初始配置");
        let env = R::ConfigEnvironment::new(path_text(&root), "win32");
        let mut snapshots: Vec<Vec<String>> = Vec::new();
        for round in 0..2 {
            let legacy = root.join("legacy");
            fs::create_dir_all(&legacy).expect("建立旧目录");
            fs::write(legacy.join("config.toml"), format!("a = {}\n", round + 2))
                .expect("写旧配置");
            R::migrate_legacy_user_config(&env, Some(&[legacy])).expect("迁移应成功");
            snapshots.push(snapshot(&target));
        }
        let expected: Vec<Vec<String>> = case["expected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|round| {
                round
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| item.as_str().unwrap().to_string())
                    .collect()
            })
            .collect();
        assert_eq!(snapshots, expected, "用例：{}", case["name"]);
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn json_helpers_are_not_misused() {
    // 数据集里的 `null` 只用来表达 `_strip_none`：键被丢弃，数组元素被过滤。
    let table = json_to_table(&json!({"a": null, "b": 1, "list": [1, null, 2]}));
    assert_eq!(table.len(), 2);
    assert_eq!(
        table["list"],
        TomlValue::Array(vec![TomlValue::Integer(1), TomlValue::Integer(2)])
    );
}
