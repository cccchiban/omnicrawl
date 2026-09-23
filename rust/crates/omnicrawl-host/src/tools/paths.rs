//! 路径解析与保护：工作区根的归一化、受保护路径拒绝、相对路径换算。
//!
//! 语义基准是 `omnicrawl/workspace/tools.py` 的 `safe_path` / `relative_path` /
//! `is_protected_path`。相对路径以工作区为基准，绝对路径允许指向工作区外（read 等
//! 工具借此读取本机其它文件），受保护路径一律拒绝。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::error::ToolError;

/// 任意层级出现即拒绝访问的名字（与 Python 的 `PROTECTED_NAMES` 一致）。
pub const PROTECTED_NAMES: [&str; 12] = [
    ".git",
    ".venv",
    "venv",
    "env",
    "__pycache__",
    ".codex-ref",
    ".env",
    "config.json",
    "config.toml",
    "models.toml",
    "config.yaml",
    "models.yaml",
];

/// 工作区根及其路径换算。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePaths {
    root: PathBuf,
}

impl WorkspacePaths {
    /// 工作区根先归一化（Python 侧同样 `resolve()` 后再用）。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: resolve_lenient(&root.into()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 解析工具传入的路径：空路径报错，相对路径挂到工作区根，受保护路径拒绝。
    pub fn safe_path(&self, raw: &str) -> Result<PathBuf, ToolError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(ToolError::new("路径不能为空。"));
        }
        let candidate = Path::new(raw);
        let candidate = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.root.join(candidate)
        };
        let resolved = resolve_lenient(&candidate);
        if Self::is_protected(&resolved) {
            return Err(ToolError::new(format!(
                "拒绝访问受保护路径：{}",
                self.relative(&resolved)
            )));
        }
        Ok(resolved)
    }

    /// 任意路径段命中受保护名单或 `.env.*` 变体即受保护。
    pub fn is_protected(path: &Path) -> bool {
        path.components().any(|component| {
            let text = component.as_os_str().to_string_lossy();
            PROTECTED_NAMES.contains(&text.as_ref()) || text.starts_with(".env.")
        })
    }

    /// 工作区相对路径；工作区之外原样返回文本（与 Python 一致，不做 `..` 归一）。
    pub fn relative(&self, path: &Path) -> String {
        if let Ok(relative) = path.strip_prefix(&self.root) {
            if relative.as_os_str().is_empty() {
                return ".".to_string();
            }
            return relative.to_string_lossy().to_string();
        }
        path.to_string_lossy().to_string()
    }

    /// 路径是否位于工作区内（链接与 `..` 都按解析后的结果判断）。
    pub fn is_within(&self, path: &Path) -> bool {
        resolve_lenient(path).starts_with(&self.root)
    }
}

/// 近似 Python `Path.resolve()`：目标不存在时也要给出归一化路径。
///
/// `std::fs::canonicalize` 要求路径存在，因此逐级向上找到第一个存在的祖先做
/// 规范化，再把剩余段落拼回去。Windows 上还要去掉 `\\?\` 前缀，否则与工作区根
/// 的字符串比较会全部失配。
pub fn resolve_lenient(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return strip_verbatim(canonical);
    }
    let mut tail: Vec<OsString> = Vec::new();
    let mut current = path.to_path_buf();
    while let Some(parent) = current.parent() {
        match current.file_name() {
            Some(name) => tail.push(name.to_os_string()),
            // 路径以分隔符或 `..` 结尾：这一段无法回拼，直接放弃快速路径。
            None => return path.to_path_buf(),
        }
        if parent.as_os_str().is_empty() {
            break;
        }
        if let Ok(canonical) = parent.canonicalize() {
            let mut resolved = strip_verbatim(canonical);
            for name in tail.iter().rev() {
                resolved.push(name);
            }
            return resolved;
        }
        current = parent.to_path_buf();
    }
    path.to_path_buf()
}

/// 去掉 Windows 规范化路径的 `\\?\` 前缀（保留 UNC 形式）。
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy().to_string();
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        let bytes = rest.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' {
            return PathBuf::from(rest.to_string());
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("omnicrawl-tui-paths-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).expect("创建临时工作区");
        resolve_lenient(&root)
    }

    #[test]
    fn relative_paths_resolve_under_workspace_root() {
        let root = temp_root("relative");
        let paths = WorkspacePaths::new(&root);
        let resolved = paths.safe_path("src/main.rs").expect("相对路径应当可用");
        assert_eq!(resolved, root.join("src").join("main.rs"));
        assert_eq!(
            paths.relative(&resolved),
            Path::new("src").join("main.rs").to_string_lossy()
        );
    }

    #[test]
    fn empty_and_protected_paths_are_rejected() {
        let root = temp_root("protected");
        let paths = WorkspacePaths::new(&root);
        assert_eq!(
            paths.safe_path("   ").unwrap_err().message,
            "路径不能为空。"
        );

        let error = paths
            .safe_path(".git/config")
            .expect_err("受保护路径应被拒绝");
        assert!(
            error.message.starts_with("拒绝访问受保护路径："),
            "{}",
            error.message
        );
        assert!(paths.safe_path("config.toml").is_err());
        assert!(paths.safe_path(".env.production").is_err());
    }

    #[test]
    fn outside_workspace_paths_keep_their_own_text() {
        let root = temp_root("outside");
        let paths = WorkspacePaths::new(&root);
        let outside = root.parent().expect("父目录").join("somewhere-else.txt");
        let resolved = paths
            .safe_path(&outside.to_string_lossy())
            .expect("区外绝对路径可用");
        assert_eq!(paths.relative(&resolved), outside.to_string_lossy());
        assert!(!paths.is_within(&resolved));
        assert!(paths.is_within(&paths.safe_path("src").expect("区内路径")));
    }

    #[test]
    fn nonexistent_targets_still_resolve_under_the_root() {
        let root = temp_root("missing");
        let paths = WorkspacePaths::new(&root);
        let resolved = paths
            .safe_path("src/not-created-yet/deep.txt")
            .expect("不存在的目标也要能解析");
        assert!(resolved.starts_with(&root));
        assert!(paths.relative(&resolved).starts_with("src"));
    }
}
