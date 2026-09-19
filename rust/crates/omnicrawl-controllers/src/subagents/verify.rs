//! `omnicrawl/agent/subagents/verify.py` 的移植：`verify` SubAgent 的固定检查工具。
//!
//! 这里刻意不接收任意 Shell 文本，也不把命令列表放到用户配置中：模型只能选择 Host 维护的
//! 检查标识，再由宿主把它解析为固定 argv 并以 `shell=False` 启动。这样 verify profile 能跑
//! 必要的本地回归检查，同时不会成为 Bash、PowerShell、文件写入或联网执行的权限旁路。

use serde_json::{json, Value};

use crate::error::AgentError;
use crate::json::python_dumps;

/// 受控检查工具名。
pub const VERIFY_COMMAND_TOOL_NAME: &str = "verify_command";
const ALLOWED_ARGUMENTS: [&str; 2] = ["check", "timeout_seconds"];

/// 一项由 Host 固定维护的、无模型可控 argv 的验证检查。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyCheck {
    pub identifier: String,
    pub label: String,
    pub argv: Vec<String>,
}

/// 固定检查表；`python_executable` 对应 Python 侧的 `sys.executable`，由宿主注入。
pub fn verify_checks(python_executable: &str) -> Vec<VerifyCheck> {
    vec![
        VerifyCheck {
            identifier: "unit_tests".to_string(),
            label: "Python 单元测试".to_string(),
            argv: vec![
                python_executable.to_string(),
                "-m".to_string(),
                "unittest".to_string(),
                "discover".to_string(),
                "-s".to_string(),
                "tests".to_string(),
                "-q".to_string(),
            ],
        },
        VerifyCheck {
            identifier: "compileall".to_string(),
            label: "Python 编译检查".to_string(),
            argv: vec![
                python_executable.to_string(),
                "-m".to_string(),
                "compileall".to_string(),
                "-q".to_string(),
                "omnicrawl".to_string(),
                "main.py".to_string(),
                "tests".to_string(),
            ],
        },
        VerifyCheck {
            identifier: "git_diff_check".to_string(),
            label: "Git 差异空白检查".to_string(),
            argv: vec!["git".to_string(), "diff".to_string(), "--check".to_string()],
        },
    ]
}

/// 检查标识按固定顺序用顿号连接。
pub fn verify_check_ids(python_executable: &str) -> String {
    verify_checks(python_executable)
        .iter()
        .map(|check| check.identifier.clone())
        .collect::<Vec<_>>()
        .join("、")
}

pub fn verify_command_description() -> &'static str {
    "执行 Host 固定的本地验证检查。仅支持 unit_tests、compileall、git_diff_check；\
不能传递 Bash、PowerShell、命令文本、路径、环境变量或网络参数。"
}

/// 工具声明用的参数 Schema；与 Python `build_verify_command_tool` 逐字对齐。
pub fn verify_command_schema(
    python_executable: &str,
    max_timeout_seconds: i64,
) -> Result<String, AgentError> {
    if max_timeout_seconds < 1 {
        return Err(AgentError::new("verify 命令超时必须大于 0。"));
    }
    let ids: Vec<String> = verify_checks(python_executable)
        .iter()
        .map(|check| check.identifier.clone())
        .collect();
    let schema = json!({
        "type": "object",
        "properties": {
            "check": {
                "type": "string",
                "enum": ids,
                "description": "Host 固定验证检查的标识，不接受命令文本。",
            },
            "timeout_seconds": {
                "type": "integer",
                "minimum": 1,
                "maximum": max_timeout_seconds,
                "description": "可选的更短超时，不能超过 Host 配置上限。",
            },
        },
        "required": ["check"],
        "additionalProperties": false,
    });
    Ok(python_dumps(&schema, 0))
}

/// 只接受检查标识和不超过 Host 上限的整数超时。
pub fn parse_verify_arguments(
    arguments: &Value,
    python_executable: &str,
    max_timeout_seconds: i64,
) -> Result<(VerifyCheck, i64), AgentError> {
    let Some(object) = arguments.as_object() else {
        return Err(AgentError::new("verify_command 参数必须是对象。"));
    };
    let mut unsupported: Vec<String> = object
        .keys()
        .filter(|key| !ALLOWED_ARGUMENTS.contains(&key.as_str()))
        .cloned()
        .collect();
    if !unsupported.is_empty() {
        unsupported.sort();
        return Err(AgentError::new(format!(
            "verify_command 不支持参数：{}。",
            unsupported.join("、")
        )));
    }

    let checks = verify_checks(python_executable);
    let check_id = match object.get("check") {
        Some(Value::String(text)) => text.clone(),
        _ => String::new(),
    };
    let Some(check) = checks.iter().find(|item| item.identifier == check_id) else {
        let available = checks
            .iter()
            .map(|item| item.identifier.clone())
            .collect::<Vec<_>>()
            .join("、");
        return Err(AgentError::new(format!(
            "check 必须是固定检查标识：{available}。"
        )));
    };

    let timeout = match object.get("timeout_seconds") {
        None => max_timeout_seconds,
        Some(Value::Number(number)) if number.is_i64() || number.is_u64() => {
            number.as_i64().unwrap_or_default()
        }
        Some(_) => return Err(AgentError::new("timeout_seconds 必须是整数。")),
    };
    if timeout < 1 || timeout > max_timeout_seconds {
        return Err(AgentError::new(format!(
            "timeout_seconds 必须在 1 到 {max_timeout_seconds} 之间。"
        )));
    }
    Ok((check.clone(), timeout))
}
