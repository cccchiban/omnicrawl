//! 资源文件（对映 Python 侧 `omnicrawl/config_chat/assets/` 下的两份 JSON）。
//!
//! - `labels.json`：可修改的配置项清单，`router` 只用 `kind`（`section` 用于「开关要落到
//!   `<段>.enabled`」的改写），`service` 只用 `type`（取值类型校验）；
//! - `aliases.json`：配置路径 → 自然语言别名列表（别名表的向量在 [`crate::router`] 建索引）。
//!
//! 两份文件是 Python 侧同名资源的**逐字节副本**，由 `rust/tools/gen_config_router_fixture.py`
//! 复制过来并在对照数据集里留下 sha256，内核不再解析 Python 包。

use std::collections::HashMap;

use serde::Deserialize;

pub const LABELS_FILENAME: &str = "labels.json";
pub const ALIASES_FILENAME: &str = "aliases.json";

/// `labels.json` 的一条记录。六字段与 Python 侧一一对应。
#[derive(Debug, Clone, Deserialize)]
pub struct LabelEntry {
    pub path: String,
    /// `key` 或 `section`。
    #[serde(default)]
    pub kind: String,
    /// 所属段（段自身为空串）。
    #[serde(default)]
    pub section: String,
    #[serde(default)]
    pub name: String,
    /// 取值类型：`bool` / `int` / `float` / `str` / `list` / `dict` / `section`。
    #[serde(rename = "type", default)]
    pub type_name: String,
    /// 界面展示用的默认值（Python 侧也只作为文本）。
    #[serde(default)]
    pub default: String,
}

/// `labels.json` 的顶层结构。
#[derive(Debug, Clone, Deserialize)]
pub struct LabelsDocument {
    pub keys: Vec<LabelEntry>,
}

impl LabelsDocument {
    /// 解析标签表；错误文案由调用方包装（Python 侧同样只透传 `json` 的消息）。
    pub fn parse(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|error| error.to_string())
    }

    /// 按路径索引，顺序即文件里的顺序。
    pub fn by_path(&self) -> HashMap<String, LabelEntry> {
        self.keys
            .iter()
            .map(|item| (item.path.clone(), item.clone()))
            .collect()
    }

    /// `kind == "section"` 的路径集合（对映 Python 的 `self.sections`）。
    pub fn sections(&self) -> Vec<String> {
        self.keys
            .iter()
            .filter(|item| item.kind == "section")
            .map(|item| item.path.clone())
            .collect()
    }
}

/// 解析别名表：配置路径 → 别名列表（保序）。
pub fn parse_aliases(text: &str) -> Result<HashMap<String, Vec<String>>, String> {
    serde_json::from_str(text).map_err(|error| error.to_string())
}
