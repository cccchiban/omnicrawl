//! 随包发布的只读技术文档（源在 `rust/assets/docs/`；对应 `omnicrawl/common/documentation.py`）。
//!
//! Python 侧按安装包目录读盘；Rust 侧要把这些文档打进二进制（二进制可能被单独分发，
//! 安装目录里不一定有 `docs/`），因此用 `include_str!` 固化内容。文件名与排序
//! （按小写字典序）与 Python 的 `glob("*.md")` 结果一致，由对照测试钉住。

use std::fmt;

pub const BUNDLED_DOC_URI_PREFIX: &str = "omnicrawl://docs/";

/// 内置文档 URI 无效或目标文档不可用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundledDocumentationError {
    message: String,
}

impl BundledDocumentationError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for BundledDocumentationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BundledDocumentationError {}

struct BundledDoc {
    name: &'static str,
    text: &'static str,
}

/// 文档表按文件名小写序排列，与 Python 侧 `sorted(names, key=str.lower)` 一致。
const BUNDLED_DOCS: [BundledDoc; 14] = [
    BundledDoc {
        name: "advisor_design.md",
        text: include_str!("../../../../rust/assets/docs/advisor_design.md"),
    },
    BundledDoc {
        name: "agent_gateway_desensitization_design.md",
        text: include_str!("../../../../rust/assets/docs/agent_gateway_desensitization_design.md"),
    },
    BundledDoc {
        name: "API.md",
        text: include_str!("../../../../rust/assets/docs/API.md"),
    },
    BundledDoc {
        name: "decision_api.md",
        text: include_str!("../../../../rust/assets/docs/decision_api.md"),
    },
    BundledDoc {
        name: "FSAPP.md",
        text: include_str!("../../../../rust/assets/docs/FSAPP.md"),
    },
    BundledDoc {
        name: "MCP_USAGE.md",
        text: include_str!("../../../../rust/assets/docs/MCP_USAGE.md"),
    },
    BundledDoc {
        name: "memory_system_design.md",
        text: include_str!("../../../../rust/assets/docs/memory_system_design.md"),
    },
    BundledDoc {
        name: "session_design.md",
        text: include_str!("../../../../rust/assets/docs/session_design.md"),
    },
    BundledDoc {
        name: "SKILL_INSTALLATION.md",
        text: include_str!("../../../../rust/assets/docs/SKILL_INSTALLATION.md"),
    },
    BundledDoc {
        name: "TELEGRAM.md",
        text: include_str!("../../../../rust/assets/docs/TELEGRAM.md"),
    },
    BundledDoc {
        name: "TERMINAL_UI.md",
        text: include_str!("../../../../rust/assets/docs/TERMINAL_UI.md"),
    },
    BundledDoc {
        name: "TOOL_CALLING.md",
        text: include_str!("../../../../rust/assets/docs/TOOL_CALLING.md"),
    },
    BundledDoc {
        name: "tool_output_compression_design.md",
        text: include_str!("../../../../rust/assets/docs/tool_output_compression_design.md"),
    },
    BundledDoc {
        name: "TTS.md",
        text: include_str!("../../../../rust/assets/docs/TTS.md"),
    },
];

/// 列出安装包内可读取的 Markdown 文档文件名。
pub fn bundled_doc_names() -> Vec<&'static str> {
    BUNDLED_DOCS.iter().map(|doc| doc.name).collect()
}

/// 根据已验证的文档文件名构造稳定 URI。
pub fn bundled_doc_uri(name: &str) -> Result<String, BundledDocumentationError> {
    validate_doc_name(name)?;
    Ok(format!("{BUNDLED_DOC_URI_PREFIX}{name}"))
}

/// 读取内置文档内容。
pub fn read_bundled_doc(uri: &str) -> Result<&'static str, BundledDocumentationError> {
    let Some(name) = uri.strip_prefix(BUNDLED_DOC_URI_PREFIX) else {
        return Err(BundledDocumentationError::new(format!(
            "不是有效的内置文档 URI：{uri}"
        )));
    };
    validate_doc_name(name)?;
    match BUNDLED_DOCS.iter().find(|doc| doc.name == name) {
        Some(doc) => Ok(doc.text),
        None => Err(BundledDocumentationError::new(format!(
            "内置文档不存在：{uri}"
        ))),
    }
}

/// 只允许单个 Markdown 文件名，拒绝目录穿越与其它后缀。
pub fn validate_doc_name(name: &str) -> Result<(), BundledDocumentationError> {
    let suffix = name
        .rfind('.')
        .filter(|index| *index > 0)
        .map(|index| &name[index..])
        .unwrap_or("");
    let invalid = name.is_empty()
        || name.contains('/')
        || name != name.rsplit('/').next().unwrap_or(name)
        || name == "."
        || name == ".."
        || !suffix.eq_ignore_ascii_case(".md");
    if invalid {
        return Err(BundledDocumentationError::new(format!(
            "内置文档 URI 仅允许单个 Markdown 文件名：{name}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sorted_case_insensitively() {
        let names = bundled_doc_names();
        let mut sorted: Vec<String> = names.iter().map(|name| name.to_lowercase()).collect();
        sorted.sort();
        let actual: Vec<String> = names.iter().map(|name| name.to_lowercase()).collect();
        assert_eq!(actual, sorted);
        assert_eq!(names.len(), 14);
    }

    #[test]
    fn uri_round_trip_reads_embedded_text() {
        let uri = bundled_doc_uri("API.md").expect("URI 应当合法");
        assert_eq!(uri, "omnicrawl://docs/API.md");
        let text = read_bundled_doc(&uri).expect("文档应当可读");
        assert!(text.contains("#"), "内置文档应当有正文");
    }

    #[test]
    fn traversal_and_other_prefixes_are_rejected() {
        assert!(read_bundled_doc("omnicrawl://docs/../config.toml").is_err());
        assert!(read_bundled_doc("file:///etc/passwd").is_err());
        assert!(bundled_doc_uri("API.txt").is_err());
        assert!(read_bundled_doc("omnicrawl://docs/README.md").is_err());
    }
}
