//! `agent/subagents/read_only_commands.py` 的判定构件。
//!
//! 只读子代理只能跑一小撮可审计的命令；这里收的是与 shell 解析无关的那一层：
//! 可执行名归一化、命令链分段、curl 与浏览器 CLI 的写入判定。
//! 依赖 `shlex` 的「按 token 判定」与主入口 `read_only_command_denial_reason` 仍在宿主。

/// 受只读策略保护的命令工具名。
pub const READ_ONLY_COMMAND_TOOL_NAMES: [&str; 3] = ["bash", "powershell", "monitor"];

pub const BASH_COMMANDS: [&str; 30] = [
    "agent-browser-cli",
    "basename",
    "cat",
    "curl",
    "date",
    "dirname",
    "du",
    "echo",
    "file",
    "find",
    "git",
    "grep",
    "head",
    "jq",
    "ls",
    "nl",
    "printf",
    "pwd",
    "rg",
    "sed",
    "sort",
    "stat",
    "tail",
    "tr",
    "tree",
    "uname",
    "uniq",
    "wc",
    "which",
    "whoami",
];

pub const POWERSHELL_COMMANDS: [&str; 23] = [
    "agent-browser-cli",
    "curl",
    "curl.exe",
    "format-list",
    "format-table",
    "get-childitem",
    "get-command",
    "get-content",
    "get-date",
    "get-item",
    "get-location",
    "get-process",
    "get-service",
    "git",
    "invoke-restmethod",
    "invoke-webrequest",
    "measure-object",
    "resolve-path",
    "select-object",
    "select-string",
    "sort-object",
    "test-path",
    "where.exe",
];

pub const BROWSER_COMMANDS: [&str; 22] = [
    "click",
    "close",
    "console",
    "doctor",
    "exec",
    "fill",
    "logs",
    "lookup",
    "mouse-click",
    "network",
    "open",
    "profile-label",
    "restart",
    "save-pdf",
    "scan",
    "screenshot",
    "send-keys",
    "snapshot",
    "status",
    "stop",
    "tabs",
    "tabtree",
];

/// curl 里会写本地文件的选项。
pub const CURL_FILE_FLAGS: [&str; 8] = [
    "--cookie-jar",
    "--dump-header",
    "--output",
    "--output-dir",
    "--remote-header-name",
    "--remote-name",
    "--trace",
    "--trace-ascii",
];

/// curl 里可能修改远端状态的选项。
pub const CURL_REMOTE_WRITE_FLAGS: [&str; 8] = [
    "--data",
    "--data-ascii",
    "--data-binary",
    "--data-raw",
    "--form",
    "--form-string",
    "--json",
    "--upload-file",
];

/// 会被判成「可能修改远端」的 HTTP 方法。
pub const CURL_REMOTE_WRITE_METHODS: [&str; 4] = ["delete", "patch", "post", "put"];

/// find 里会写文件或执行外部命令的动作。
pub const FIND_WRITE_ACTIONS: [&str; 8] = [
    "-delete", "-exec", "-execdir", "-fls", "-fprint", "-fprintf", "-ok", "-okdir",
];

/// 命令首个 token 归一化成可执行名：去引号、取末段路径、去 Windows 扩展名。
///
/// `curl.exe` 与 `where.exe` 保留扩展名——它们本身就是白名单里的独立条目。
pub fn normalized_executable(token: &str) -> String {
    let normalized = token
        .trim()
        .trim_matches(|ch| ch == '"' || ch == '\'')
        .replace('\\', "/");
    let base = normalized.rsplit('/').next().unwrap_or("");
    let lowered = base.to_lowercase();
    for suffix in [".exe", ".cmd", ".bat"] {
        if lowered.ends_with(suffix) && !matches!(lowered.as_str(), "curl.exe" | "where.exe") {
            return lowered[..lowered.len() - suffix.len()].to_string();
        }
    }
    lowered
}

/// 把简单命令链拆成片段；任何重定向、空片段、未闭合引号都直接拒绝。
pub fn split_shell_segments(command: &str) -> Result<Vec<String>, String> {
    let chars: Vec<char> = command.chars().collect();
    let mut segments: Vec<String> = Vec::new();
    let mut current: Vec<char> = Vec::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut index = 0usize;
    while index < chars.len() {
        let ch = chars[index];
        if escaped {
            current.push(ch);
            escaped = false;
            index += 1;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            current.push(ch);
            escaped = true;
            index += 1;
            continue;
        }
        if let Some(active) = quote {
            current.push(ch);
            if ch == active {
                quote = None;
            }
            index += 1;
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            current.push(ch);
            index += 1;
            continue;
        }
        if ch == '>' || ch == '<' {
            return Err("不允许输入或输出重定向。".to_string());
        }
        if matches!(ch, ';' | '\n' | '|' | '&') {
            let segment = collect_segment(&current);
            if segment.is_empty() {
                return Err("命令链包含空片段。".to_string());
            }
            segments.push(segment);
            current.clear();
            if index + 1 < chars.len() && chars[index + 1] == ch {
                index += 1;
            }
            index += 1;
            continue;
        }
        current.push(ch);
        index += 1;
    }
    if quote.is_some() {
        return Err("命令包含未闭合引号。".to_string());
    }
    let segment = collect_segment(&current);
    if segment.is_empty() {
        return Err("命令为空。".to_string());
    }
    segments.push(segment);
    Ok(segments)
}

fn collect_segment(chars: &[char]) -> String {
    chars.iter().collect::<String>().trim().to_string()
}

/// curl 参数里的写入判定；返回空串表示看着只读。
pub fn curl_denial_reason(arguments: &[String]) -> Option<String> {
    for (index, token) in arguments.iter().enumerate() {
        let lowered = token.to_lowercase();
        let option = lowered
            .split_once('=')
            .map_or(lowered.as_str(), |(head, _)| head)
            .to_string();
        if CURL_REMOTE_WRITE_FLAGS.contains(&option.as_str())
            || token.starts_with("-d")
            || token.starts_with("-F")
            || token.starts_with("-T")
        {
            return Some(format!("curl {option} 可能修改远端状态。"));
        }
        if option == "--request" || token.starts_with("-X") {
            let method = if lowered.contains('=') {
                lowered
                    .split_once('=')
                    .map(|(_, tail)| tail)
                    .unwrap_or_default()
                    .to_string()
            } else if token.starts_with("-X") && token.chars().count() > 2 {
                token[2..].to_lowercase()
            } else if index + 1 < arguments.len() {
                arguments[index + 1].to_lowercase()
            } else {
                String::new()
            };
            if CURL_REMOTE_WRITE_METHODS.contains(&method.as_str()) {
                return Some(format!("curl {} 可能修改远端状态。", method.to_uppercase()));
            }
        }
        if CURL_FILE_FLAGS.contains(&option.as_str()) {
            return Some(format!("curl {option} 会写入本地文件。"));
        }
        if token.starts_with('-') && !token.starts_with("--") {
            let flags = &token[1..];
            if flags.chars().any(|ch| matches!(ch, 'o' | 'O' | 'D' | 'c')) {
                return Some(format!("curl {token} 可能写入本地文件。"));
            }
        }
    }
    None
}

/// 浏览器 CLI 子命令与 `--out`/`--file` 的判定；返回空串表示看着只读。
pub fn browser_cli_denial_reason(arguments: &[String]) -> Option<String> {
    let Some(first) = arguments.first() else {
        return Some("agent-browser-cli 缺少子命令。".to_string());
    };
    let subcommand = first.to_lowercase();
    if subcommand.starts_with('-') {
        return if subcommand == "--help" || subcommand == "--version" {
            None
        } else {
            Some("未知浏览器 CLI 选项。".to_string())
        };
    }
    if !BROWSER_COMMANDS.contains(&subcommand.as_str()) {
        return Some(format!("agent-browser-cli {first} 不在运行期允许列表中。"));
    }
    for (index, token) in arguments.iter().enumerate() {
        if token == "--out" || token.starts_with("--out=") {
            return Some("显式 --out 可能覆盖工作区文件；请使用 CLI 默认临时目录。".to_string());
        }
        if token == "--file" && index + 1 >= arguments.len() {
            return Some("--file 缺少输入路径。".to_string());
        }
    }
    None
}
