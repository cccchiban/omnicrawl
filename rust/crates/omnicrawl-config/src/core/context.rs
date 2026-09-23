//! 工作区上下文：对齐 Python `omnicrawl/workspace/context.py`。
//!
//! 工作区固定取启动目录，不向上查找项目标记，也不做项目根回退。

use std::env;
use std::path::{Path, PathBuf};

use super::runtime::{expand_user, ConfigEnvironment};

/// 启动目录覆盖变量。
pub const LAUNCH_CWD_ENV: &str = "AI_VOICE_CHAT_LAUNCH_CWD";

/// Agent 当前要操作的工作区路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectContext {
    pub workspace_root: PathBuf,
}

impl ProjectContext {
    /// 适合注入系统提示词的中文检测说明。
    pub fn detection_summary(&self) -> String {
        format!("使用启动目录作为工作区：{}", self.workspace_root.display())
    }
}

/// 检测本轮 Agent 的工作区目录。
///
/// `AI_VOICE_CHAT_LAUNCH_CWD` 优先于显式 `start_path`；两者都没有时使用当前目录。
/// 解析失败、路径不存在时回退到当前目录；已有文件则取其父目录。
pub fn detect_project_context(
    environment: &ConfigEnvironment,
    start_path: Option<&Path>,
) -> ProjectContext {
    let current = current_directory();
    let raw = environment.get_trimmed(LAUNCH_CWD_ENV);
    let candidate = if !raw.is_empty() {
        expand_user(environment, &raw)
    } else if let Some(path) = start_path {
        expand_user(environment, &path.to_string_lossy())
    } else {
        current.clone()
    };

    let workspace_root = match std::fs::canonicalize(&candidate) {
        Ok(resolved) if resolved.is_dir() => resolved,
        Ok(resolved) => resolved
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(current.clone()),
        Err(_) => current,
    };
    ProjectContext { workspace_root }
}

/// 返回适合启动面板展示的短标签。
pub fn project_context_status_label(context: &ProjectContext) -> String {
    context.workspace_root.to_string_lossy().to_string()
}

fn current_directory() -> PathBuf {
    env::current_dir()
        .ok()
        .and_then(|path| std::fs::canonicalize(path).ok())
        .or_else(|| env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn environment(home: &Path) -> ConfigEnvironment {
        ConfigEnvironment::new(home, std::env::consts::OS)
    }

    #[test]
    fn launch_environment_overrides_explicit_start_path() {
        let root = std::env::temp_dir().join("omnicrawl-context-env");
        let explicit = root.join("explicit");
        let from_env = root.join("from-env");
        fs::create_dir_all(&explicit).unwrap();
        fs::create_dir_all(&from_env).unwrap();
        let env = environment(&root).with_env_value(LAUNCH_CWD_ENV, &from_env.to_string_lossy());

        let context = detect_project_context(&env, Some(&explicit));
        assert_eq!(context.workspace_root, fs::canonicalize(from_env).unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn existing_file_uses_its_parent_directory() {
        let root = std::env::temp_dir().join("omnicrawl-context-file");
        fs::create_dir_all(&root).unwrap();
        let file = root.join("launch.txt");
        fs::write(&file, "").unwrap();
        let context = detect_project_context(&environment(&root), Some(&file));
        assert_eq!(context.workspace_root, fs::canonicalize(root).unwrap());
        let _ = fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn missing_path_falls_back_to_current_directory() {
        let missing = std::env::temp_dir().join("omnicrawl-context-does-not-exist");
        let context = detect_project_context(&environment(&missing), Some(&missing));
        let current = fs::canonicalize(env::current_dir().unwrap()).unwrap();
        assert_eq!(context.workspace_root, current);
    }
}
