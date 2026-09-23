#![allow(dead_code)]

//! 对照测试共用脚手架：数据集装载、临时工作区与占位符还原。
//!
//! 集成测试各自是独立 crate，共用代码只有放在这里才不会重复。

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// 工作区占位符：数据集里的临时目录路径（两侧的路径本来就不一样）。
pub const WORKSPACE_PLACEHOLDER: &str = "{workspace}";
/// 随机 ID 占位符：会话与审计 ID 两侧都不可复现。
pub const ID_PLACEHOLDER: &str = "{id}";

pub fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp_parity.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("读取数据集失败：{}，{error}", path.display()));
    serde_json::from_str(&text).expect("数据集必须是合法 JSON")
}

/// 取数据集里的一个分组（数组）。
pub fn cases(data: &Value, group: &str) -> Vec<Value> {
    match data.get(group) {
        Some(Value::Array(items)) => items.clone(),
        other => panic!("数据集缺少分组 {group}：{other:?}"),
    }
}

pub fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    value
        .get(name)
        .unwrap_or_else(|| panic!("用例缺少字段 {name}：{value}"))
}

/// 干净的临时工作区（同名目录先删再建）。
pub fn temp_workspace(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("omnicrawl-mcp-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("创建临时工作区");
    root
}

/// 铺一份与数据集生成脚本相同的最小工作区。
pub fn prepare_workspace(root: &Path) {
    std::fs::create_dir_all(root.join("docs")).expect("创建 docs 目录");
    std::fs::write(root.join("README.md"), "# 项目\n").expect("写 README");
    std::fs::write(root.join("AGENTS.md"), "协作规范\n").expect("写 AGENTS.md");
    std::fs::write(root.join("docs").join("API.md"), "接口\n").expect("写文档");
    std::fs::write(root.join("config.toml"), "secret=1\n").expect("写受保护文件");
}

/// 递归把字符串里的占位符替换成实际取值。
pub fn substitute(value: &Value, placeholder: &str, replacement: &str) -> Value {
    match value {
        Value::String(text) => Value::String(text.replace(placeholder, replacement)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| substitute(item, placeholder, replacement))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), substitute(item, placeholder, replacement)))
                .collect::<Map<String, Value>>(),
        ),
        other => other.clone(),
    }
}

/// 把 `session-xxxxxxxxxxxx` / `mcp-xxxxxxxxxxxx` 折成带占位符的文本。
pub fn normalize_ids(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let matched = ["session-", "mcp-"]
            .iter()
            .find(|prefix| text[index..].starts_with(**prefix))
            .copied();
        if let Some(prefix) = matched {
            let start = index + prefix.len();
            let candidate = &text[start..(start + 12).min(text.len())];
            if candidate.len() == 12 && candidate.chars().all(|ch| ch.is_ascii_hexdigit()) {
                out.push_str(prefix);
                out.push_str(ID_PLACEHOLDER);
                index = start + 12;
                continue;
            }
        }
        let ch = text[index..].chars().next().expect("有效的 UTF-8 边界");
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
