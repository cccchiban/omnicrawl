//! 剪切板写入：把会话流里拖选出来的文本放进系统剪切板。
//!
//! 为什么要自己写：TUI 开了鼠标捕获（`EnableMouseCapture`），终端自带的选择被应用吃掉，
//! 所以拖选与复制必须由界面自己实现（Python 侧由 Textual 提供同等能力）。
//!
//! 平台分支：
//! - Windows：复用宿主已有的 Win32 实现（`CF_UNICODETEXT`）。**不用系统自带的 `clip.exe`**——
//!   它要求输入带 UTF-16LE BOM 才按 Unicode 解码，而那个 BOM 会被一起粘进剪切板
//!   （实测粘出来前面多一个 `\u{feff}`）。
//! - 其余平台：依次试 `pbcopy` / `wl-copy` / `xclip`（都是 UTF-8，不需要转码）。

/// 把文本写进系统剪切板。
pub fn copy_text(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Err("没有可复制的内容。".to_string());
    }
    imp::copy(text)
}

#[cfg(windows)]
mod imp {
    use omnicrawl_host::tools::windows::clipboard::write_clipboard_text;

    pub fn copy(text: &str) -> Result<(), String> {
        write_clipboard_text(text).map_err(|error| error.to_string())
    }
}

#[cfg(not(windows))]
mod imp {
    use std::io::Write;
    use std::process::{Command, Stdio};

    pub fn copy(text: &str) -> Result<(), String> {
        let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
            &[("pbcopy", &[])]
        } else {
            &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])]
        };
        let mut last_error = "没有可用的剪切板命令".to_string();
        for (program, args) in candidates {
            let mut command = Command::new(program);
            command
                .args(*args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    last_error = format!("{program} 启动失败：{error}");
                    continue;
                }
            };
            if let Some(mut stdin) = child.stdin.take() {
                if let Err(error) = stdin.write_all(text.as_bytes()) {
                    last_error = format!("{program} 写入失败：{error}");
                    continue;
                }
            }
            match child.wait() {
                Ok(status) if status.success() => return Ok(()),
                Ok(status) => last_error = format!("{program} 退出码 {status}"),
                Err(error) => last_error = format!("{program} 等待失败：{error}"),
            }
        }
        Err(last_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_rejected_before_touching_the_clipboard() {
        // 不碰真实剪切板：空内容直接报错（拖到空白处松手不会清空用户的剪切板）。
        assert!(copy_text("").is_err());
    }
}
