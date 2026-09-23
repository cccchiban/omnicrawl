//! 命令输出采样：超长时保留首尾行并把完整输出落盘。
//!
//! 语义基准是 `omnicrawl/workspace/tools.py` 的 `_sample_command_output`：未超
//! `head + tail` 时原样返回；超出时按行对齐保留首尾（至少各一行，不切断多字节
//! 字符），中间以一行提示代替，保存成功时在提示里给出完整输出路径。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const COMMAND_OUTPUT_HEAD_CHARS: usize = 2_000;
pub const COMMAND_OUTPUT_TAIL_CHARS: usize = 6_000;
pub const COMMAND_OUTPUT_FILES_SUBDIR: &str = "files";
pub const COMMAND_OUTPUT_FILE_PREFIX: &str = "command_output_";
pub const COMMAND_OUTPUT_FILE_SUFFIX: &str = ".log";

/// 完整输出的落盘路径：`.omnicrawl/.agent_tmp/files/command_output_<随机>.log`。
pub fn command_output_path(workspace_root: &Path, temp_directory: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or_default();
    let process = std::process::id();
    let mut root = workspace_root.to_path_buf();
    for component in temp_directory.split(['/', '\\']) {
        if !component.is_empty() {
            root.push(component);
        }
    }
    root.join(COMMAND_OUTPUT_FILES_SUBDIR).join(format!(
        "{COMMAND_OUTPUT_FILE_PREFIX}{process:08x}{stamp:08x}{COMMAND_OUTPUT_FILE_SUFFIX}"
    ))
}

pub fn sample_command_output(text: &str, save_path: Option<&Path>) -> String {
    if text.is_empty()
        || text.chars().count() <= COMMAND_OUTPUT_HEAD_CHARS + COMMAND_OUTPUT_TAIL_CHARS
    {
        return text.to_string();
    }

    let lines = split_lines_keepends(text);
    let mut head_lines: Vec<&str> = Vec::new();
    let mut head_length = 0usize;
    for line in &lines {
        if !head_lines.is_empty() && head_length + line.chars().count() > COMMAND_OUTPUT_HEAD_CHARS
        {
            break;
        }
        head_lines.push(line);
        head_length += line.chars().count();
    }

    let mut tail_lines: Vec<&str> = Vec::new();
    let mut tail_length = 0usize;
    for line in lines.iter().rev() {
        if !tail_lines.is_empty() && tail_length + line.chars().count() > COMMAND_OUTPUT_TAIL_CHARS
        {
            break;
        }
        tail_lines.push(line);
        tail_length += line.chars().count();
    }
    tail_lines.reverse();

    if head_lines.len() + tail_lines.len() >= lines.len() {
        return text.to_string();
    }

    let omitted = lines.len() - head_lines.len() - tail_lines.len();
    let mut hint = format!(
        "\n… 系统已截断：共 {} 行，仅保留首部 {} 行与尾部 {} 行（省略 {} 行）。",
        lines.len(),
        head_lines.len(),
        tail_lines.len(),
        omitted
    );
    if let Some(path) = save_path {
        if write_full_output(path, text) {
            hint.push_str(&format!(" 完整输出已保存至：{}", path.display()));
        }
    }

    let mut sampled = head_lines.concat();
    sampled.push_str(&hint);
    sampled.push('\n');
    sampled.push_str(&tail_lines.concat());
    sampled
}

/// 落盘失败静默降级：只截断，不影响命令结果。
fn write_full_output(path: &Path, text: &str) -> bool {
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    std::fs::write(path, text).is_ok()
}

/// 按行拆分并保留行尾。
///
/// Python 的 `splitlines(keepends=True)` 还会在 `\v`、`\f`、`\u2028` 等处断行；
/// 命令输出里实际只会出现 `\n`（可能带 `\r`），这里按这两种处理。
pub fn split_lines_keepends(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => {
                lines.push(&text[start..=index]);
                index += 1;
                start = index;
            }
            b'\r' => {
                let end = if bytes.get(index + 1) == Some(&b'\n') {
                    index + 2
                } else {
                    index + 1
                };
                lines.push(&text[start..end]);
                index = end;
                start = index;
            }
            _ => index += 1,
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_output_is_returned_unchanged() {
        assert_eq!(sample_command_output("", None), "");
        let short = "ok\n12 passed\n";
        assert_eq!(sample_command_output(short, None), short);
    }

    #[test]
    fn long_output_keeps_head_and_tail_by_lines() {
        let text: String = (0..2_000)
            .map(|index| format!("行{index}\n"))
            .collect::<Vec<_>>()
            .concat();
        let sampled = sample_command_output(&text, None);
        assert!(sampled.starts_with("行0\n"));
        assert!(sampled.trim_end().ends_with("行1999"));
        assert!(sampled.contains("… 系统已截断：共 2000 行"), "{sampled}");
        assert!(!sampled.contains("完整输出"), "没有 save_path 时不提保存");
    }

    #[test]
    fn long_output_saves_full_text_and_reports_path() {
        let root = std::env::temp_dir().join("omnicrawl-tui-sample");
        let _ = std::fs::remove_dir_all(&root);
        let path = command_output_path(&root, ".omnicrawl/.agent_tmp");
        let text: String = (0..2_000)
            .map(|index| format!("行{index}\n"))
            .collect::<Vec<_>>()
            .concat();
        let sampled = sample_command_output(&text, Some(&path));
        assert!(sampled.contains("完整输出已保存至："), "{sampled}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("完整输出应落盘"),
            text
        );
        assert!(path.to_string_lossy().contains("command_output_"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn save_failure_degrades_to_truncation_only() {
        let root = std::env::temp_dir().join("omnicrawl-tui-sample-file");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时目录");
        // 把一个目录当成目标文件：写入必然失败，采样仍要返回截断结果。
        let path = root.join("dir-as-file");
        std::fs::create_dir_all(&path).expect("创建同名目录");
        let text: String = (0..2_000)
            .map(|index| format!("行{index}\n"))
            .collect::<Vec<_>>()
            .concat();
        let sampled = sample_command_output(&text, Some(&path));
        assert!(sampled.contains("… 系统已截断"));
        assert!(!sampled.contains("完整输出已保存至"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn lines_keep_line_endings() {
        assert_eq!(split_lines_keepends("a\nb"), vec!["a\n", "b"]);
        assert_eq!(split_lines_keepends("a\r\nb\r\n"), vec!["a\r\n", "b\r\n"]);
        assert_eq!(split_lines_keepends("a\rb"), vec!["a\r", "b"]);
        assert!(split_lines_keepends("").is_empty());
    }
}
