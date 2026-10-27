//! 内核自持的 `computer_use` 工具：把桌面操作交给专职子代理。
//!
//! 与 `subagent` 同类（子回合需要模型运行时，因此由内核自持、宿主只声明工具名），
//! 但有四点不同：
//!
//! - **并发恒为 1**：桌面是独占资源，两个子回合并发会互抢前台窗口与焦点。
//! - **按时间收敛**：不设步数上限，靠 `timeout_seconds` 兜底，超时按已完成进度收尾。
//! - **可主动停下**：子代理判定受阻时经 `stop_check` 正常结束，状态为 `blocked`。
//! - **免审**：本工具进宿主自持白名单，子代理内部批次同样不经人工确认。
//!
//! 专职子代理不进 `subagent` 的角色枚举（见 `is_reserved_role`），模型只能经本工具驱动它。

use std::sync::{Mutex, MutexGuard, OnceLock};

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::computer_use::{load_computer_use_config, ComputerUseConfig};
use omnicrawl_controllers::subagents::coordinator::is_reserved_role;
use omnicrawl_controllers::subagents::definitions::{AgentDefinition, AgentDefinitionRegistry};
use serde_json::{json, Map, Value};

/// 工具名：与工具目录里的 `computer_use` 一致。
pub const COMPUTER_USE_TOOL_NAME: &str = "computer_use";
/// 专职子代理的角色名（定义文件 `assets/templates/subagents/computer-use.md`）。
pub const COMPUTER_USE_ROLE: &str = "computer-use";
/// 子代理允许使用的工具：桌面四件套加看图与只读排错。
const CHILD_TOOLS: [&str; 7] = [
    "windows_window",
    "windows_screenshot",
    "windows_input",
    "windows_control",
    "read_image",
    "list",
    "read",
];

/// 桌面操作的全局互斥：进程内同时只允许一个 Computer Use 子回合。
///
/// 用 `Mutex` 而不是让第二次调用失败——排队更符合「主代理连着做两步」的用法，
/// 排队等待的时间计入时间预算。
fn desktop_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// 取得桌面独占权；锁中毒只会让后续调用一直失败，因此忽略毒化继续用。
pub fn acquire_desktop() -> MutexGuard<'static, ()> {
    desktop_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 装入内核对 Computer Use 的运行期视图。
pub struct ComputerUseRuntime {
    pub config: ComputerUseConfig,
    pub definition: AgentDefinition,
}

impl ComputerUseRuntime {
    /// 从进程环境读数：开关与预算来自 `config.toml` 的 `[computer_use]`。
    pub fn load() -> Result<Self, String> {
        let env = ConfigEnvironment::from_process();
        let config = load_computer_use_config(&env, None)
            .map_err(|error| error.message().to_string())?
            .normalize()
            .map_err(|error| error.message().to_string())?;
        if !config.active() {
            return Err(disabled_text());
        }
        let definition = load_definition(&env)?;
        Ok(Self { config, definition })
    }

    /// 校验并读取 `prompt`，顺带取出可选的 `timeout_seconds`。
    ///
    /// 返回 `(prompt, timeout_seconds)`；没有给超时就用配置里的默认值。
    pub fn read_prompt(&self, arguments: &Map<String, Value>) -> Result<(String, f64), String> {
        let prompt = match arguments.get("prompt") {
            Some(Value::String(text)) => text.trim().to_string(),
            _ => return Err(invalid_argument("prompt 必须是非空字符串。")),
        };
        if prompt.is_empty() {
            return Err(invalid_argument("prompt 必须是非空字符串。"));
        }
        let timeout = match arguments.get("timeout_seconds") {
            None | Some(Value::Null) => self.config.timeout_seconds,
            Some(Value::Number(number)) => number
                .as_f64()
                .ok_or_else(|| invalid_argument("timeout_seconds 必须是数字。"))?,
            Some(_) => return Err(invalid_argument("timeout_seconds 必须是数字。")),
        };
        if timeout <= 0.0 || timeout > 3_600.0 {
            return Err(invalid_argument(
                "timeout_seconds 必须在 0 到 3600 秒之间。",
            ));
        }
        Ok((prompt, timeout))
    }

    /// 子代理的初始消息：只有任务指令，系统提示走角色定义。
    pub fn task_message(&self, prompt: &str) -> Value {
        json!({
            "role": "user",
            "content": format!("<computer_use_task>\n任务：\n{prompt}\n</computer_use_task>"),
        })
    }

    /// 子回合的结果投影：状态 + 摘要 + 截图路径，不含图片本身。
    pub fn result_payload(
        &self,
        status: &str,
        summary: &str,
        obstacle: &str,
        screenshots: &[String],
        model_turns: usize,
        tool_calls: usize,
    ) -> Value {
        json!({
            "status": status,
            "summary": summary,
            "obstacle": obstacle,
            "screenshots": screenshots,
            "usage": {
                "model_turns": model_turns,
                "tool_calls": tool_calls,
            },
        })
    }
}

/// 读取专职子代理定义；缺失或工具白名单被改坏时给出可诊断的错误。
fn load_definition(env: &ConfigEnvironment) -> Result<AgentDefinition, String> {
    let home = env.home().to_path_buf();
    let mut registry = AgentDefinitionRegistry::with_located_builtin(Some(home));
    let workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    registry.discover(&workspace, &[]);
    if registry.get(COMPUTER_USE_ROLE).is_none() || !is_reserved_role(COMPUTER_USE_ROLE) {
        return Err(disabled_text());
    }
    // 走 `get` 而不是 `list_all`：专属角色靠 `is_reserved_role` 才被排除在枚举之外，
    // 但定义本身仍在索引里，这里要拿到的就是它。
    registry
        .get(COMPUTER_USE_ROLE)
        .cloned()
        .ok_or_else(disabled_text)
}

/// 子代理实际可用的工具：白名单直供，不再要求它出现在父工具表里。
///
/// 桌面操作由这个专职子代理全权负责，主代理因此**不再声明** `windows_*`；如果这里仍按
/// 「必须真的在父工具表里」过滤，子代理会一个桌面工具都拿不到，整个 Computer Use 直接失效。
/// 白名单本身就是能力边界：桌面四件套加看图与只读排错，不多不少。
///
/// 与 `select_profile_tools` 的区别：那条路按权限 profile 过滤，`windows_*` 不在任何
/// profile 的白名单里，因此专职子代理必须走这份独立选择。
pub fn child_tool_names() -> Vec<String> {
    let mut names: Vec<String> = CHILD_TOOLS.iter().map(|name| (*name).to_string()).collect();
    names.sort();
    names.dedup();
    names
}

/// 从子代理收尾文本里取结构化回报；解析不出时退化为纯文本摘要。
///
/// 子代理被要求以 JSON 收尾（见角色定义），但它不一定照做，因此这里必须容错：
/// 找不到 JSON 就把整段文本当 `summary`，状态按 `completed` 处理。
pub fn parse_child_report(text: &str) -> (String, String, String, Vec<String>) {
    let trimmed = text.trim();
    let candidate = extract_json_object(trimmed);
    if let Some(object) = candidate {
        let status = object
            .get("status")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("completed")
            .to_lowercase();
        let status = if status == "blocked" {
            "blocked"
        } else {
            "completed"
        };
        let summary = object
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let obstacle = object
            .get("obstacle")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let screenshots: Vec<String> = object
            .get("screenshots")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if !summary.is_empty() || status == "blocked" {
            return (status.to_string(), summary, obstacle, screenshots);
        }
    }
    (
        "completed".to_string(),
        trimmed.to_string(),
        String::new(),
        Vec::new(),
    )
}

/// 从文本里挑出最后一个完整的 JSON 对象：子代理常在 JSON 前后夹带说明。
fn extract_json_object(text: &str) -> Option<Map<String, Value>> {
    let bytes = text.as_bytes();
    let mut starts: Vec<usize> = Vec::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut start: Option<usize> = None;
    for (index, byte) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => {
                if depth == 0 {
                    start = Some(index);
                }
                depth += 1;
            }
            b'}' => {
                if depth == 0 {
                    continue;
                }
                depth -= 1;
                if depth == 0 {
                    if let Some(begin) = start.take() {
                        starts.push(begin);
                    }
                }
            }
            _ => {}
        }
    }
    // 倒序取：真正的回报是收尾那段，前面的示例 JSON 可能更早出现。
    for begin in starts.into_iter().rev() {
        let mut end = text.len();
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        for (offset, byte) in text[begin..].bytes().enumerate() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    in_string = false;
                }
                continue;
            }
            match byte {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = begin + offset + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(&text[begin..end]) {
            return Some(object);
        }
    }
    None
}

/// 从子代理的工具调用记录里收集截图路径。
///
/// `windows_screenshot` 把路径放在顶层 `path`；`windows_input` / `windows_control` /
/// `windows_window` 的确认截图放在 `screenshot.path`。子代理不一定把路径写进收尾 JSON，
/// 这里补一份以防它漏掉，去重后按时间顺序返回。
pub fn collect_screenshots(observations: &[String]) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for output in observations {
        let Ok(value) = serde_json::from_str::<Value>(output) else {
            continue;
        };
        for candidate in [value.get("path"), value.pointer("/screenshot/path")] {
            let Some(path) = candidate.and_then(Value::as_str) else {
                continue;
            };
            let path = path.trim();
            if path.is_empty() || paths.iter().any(|item| item == path) {
                continue;
            }
            paths.push(path.to_string());
        }
    }
    // 只留最后几张：父代理关心的是最终画面，不是每一步。
    let keep = 3usize;
    if paths.len() > keep {
        paths.drain(0..paths.len() - keep);
    }
    paths
}

/// 一次桌面子回合的结果：线程里产出，主线程据此投影给模型。
#[derive(Debug, Clone, Default)]
pub struct ComputerUseOutcome {
    /// `completed` / `blocked` / `timeout` / `cancelled` / `failed`。
    pub status: String,
    pub summary: String,
    /// 只在该子代理判定受阻时非空。
    pub obstacle: String,
    /// 子代理自己回报的截图路径。
    pub screenshots: Vec<String>,
    pub model_turns: usize,
    pub tool_calls: usize,
    /// 子回合里所有工具结果的原文：截图路径漏报时从这里补。
    pub tool_outputs: Vec<String>,
}

pub fn disabled_text() -> String {
    json!({
        "error": {
            "code": "COMPUTER_USE_DISABLED",
            "message": "Computer Use 未启用。请在配置中设置 computer_use.enabled=true。",
        }
    })
    .to_string()
}

fn invalid_argument(message: &str) -> String {
    json!({
        "error": {
            "code": "INVALID_ARGUMENTS",
            "message": message,
        }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_role_never_enters_enumeration() {
        assert!(is_reserved_role("computer-use"));
        assert!(is_reserved_role(" Computer-Use "));
        assert!(!is_reserved_role("explore"));
    }

    #[test]
    fn report_json_is_parsed_from_surrounding_text() {
        let text = "我先查看了窗口。\n{\"status\": \"blocked\", \"summary\": \"找不到保存按钮\", \
                    \"obstacle\": \"窗口高度不足\", \"screenshots\": [\"a.png\"]}\n以上。";
        let (status, summary, obstacle, screenshots) = parse_child_report(text);
        assert_eq!(status, "blocked");
        assert_eq!(summary, "找不到保存按钮");
        assert_eq!(obstacle, "窗口高度不足");
        assert_eq!(screenshots, vec!["a.png".to_string()]);
    }

    #[test]
    fn plain_text_falls_back_to_completed_summary() {
        let (status, summary, obstacle, screenshots) = parse_child_report("  已经点完了  ");
        assert_eq!(status, "completed");
        assert_eq!(summary, "已经点完了");
        assert!(obstacle.is_empty());
        assert!(screenshots.is_empty());
    }

    #[test]
    fn unknown_status_is_normalised_to_completed() {
        let text = "{\"status\": \"whatever\", \"summary\": \"完成\"}";
        let (status, summary, _, _) = parse_child_report(text);
        assert_eq!(status, "completed");
        assert_eq!(summary, "完成");
    }

    /// 白名单直供：主代理不再声明 `windows_*`，子代理的工具也不能因此清空。
    #[test]
    fn child_tools_come_from_the_whitelist() {
        let names = child_tool_names();
        for expected in [
            "windows_window",
            "windows_screenshot",
            "windows_input",
            "windows_control",
            "read_image",
            "list",
            "read",
        ] {
            assert!(names.contains(&expected.to_string()), "缺少 {expected}：{names:?}");
        }
        assert_eq!(names.len(), CHILD_TOOLS.len(), "白名单之外的不得混入：{names:?}");
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "顺序稳定：{names:?}");
    }

    #[test]
    fn screenshots_are_deduplicated_and_bounded() {
        let observations = vec![
            json!({"path": "a.png"}).to_string(),
            json!({"path": "b.png"}).to_string(),
            json!({"path": "a.png"}).to_string(),
            json!({"path": "c.png"}).to_string(),
            json!({"path": "d.png"}).to_string(),
        ];
        let paths = collect_screenshots(&observations);
        assert_eq!(paths, vec!["b.png", "c.png", "d.png"]);
    }

    #[test]
    fn operation_snapshots_count_as_screenshots() {
        // 输入类工具把确认截图放在 `screenshot.path`，漏了它父代理就只剩空列表。
        let observations = vec![
            json!({"action": "click", "screenshot": {"path": "click.png"}}).to_string(),
            json!({"action": "list", "controls": []}).to_string(),
        ];
        assert_eq!(collect_screenshots(&observations), vec!["click.png"]);
    }
}
