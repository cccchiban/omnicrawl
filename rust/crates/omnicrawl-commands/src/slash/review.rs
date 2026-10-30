//! 代码评审（`/review`）：前置条件、任务提示词与报告格式化。
//!
//! 从 `slash.rs` 拆出。

use super::*;

// ── 代码评审（/review）─────────────────────────────────────────

/// 评审子任务的任务描述（写进任务快照，供 `/tasks` 展示）。
pub const REVIEW_TASK_DESCRIPTION: &str = "评审当前代码变更";

/// `/review` 不带参数时的评审范围说明。
pub const REVIEW_DEFAULT_SCOPE: &str = "工作区未提交改动（git diff）";

/// 评审前置检查：返回错误消息（应阻止评审），可评审时返回 `None`。
///
/// 覆盖两类「无意义评审」：工作区不是 git 仓库、工作区没有可评审的改动（无提交 +
/// 无未提交改动）。不做精确范围校验——非法范围由子 Agent 的 git 工具在收集 diff 时
/// 给出明确错误。
pub(super) fn check_review_preconditions(
    agent: &dyn CommandAgent,
    workspace_root: &Path,
    scope: &str,
) -> Option<String> {
    let git_missing = || Some("未找到 git 可执行文件，请确认 Git 已安装。".to_string());

    let is_worktree = match agent.run_git(workspace_root, &["rev-parse", "--is-inside-work-tree"]) {
        Some(output) => output,
        None => return git_missing(),
    };
    if is_worktree.code != 0 || is_worktree.stdout.trim() != "true" {
        return Some("当前工作区不是 git 仓库，无法评审。".to_string());
    }

    if !scope.is_empty() {
        // 指定范围评审：至少需要仓库存在提交。
        let head = match agent.run_git(workspace_root, &["rev-parse", "--verify", "HEAD"]) {
            Some(output) => output,
            None => return git_missing(),
        };
        if head.code != 0 {
            return Some("当前仓库还没有任何提交，无法按指定范围评审。".to_string());
        }
        return None;
    }

    // 默认范围（工作区未提交改动）：存在未提交改动或未跟踪文件才值得评审。
    let status = match agent.run_git(workspace_root, &["status", "--porcelain"]) {
        Some(output) => output,
        None => return git_missing(),
    };
    if status.code != 0 {
        return Some("无法读取 git 工作区状态，无法评审。".to_string());
    }
    if status.stdout.trim().is_empty() {
        return Some("工作区没有未提交改动，无需评审。".to_string());
    }
    None
}

/// 构造评审子 Agent 的任务 prompt：只描述评审范围，评审标准由定义注入。
pub(super) fn build_review_task_prompt(scope: &str) -> String {
    let scope_text = if scope.is_empty() {
        format!(
            "评审范围：{REVIEW_DEFAULT_SCOPE}。\
             请先用 git status 查看变更文件，再用 `git diff`（含 `git diff --cached`）收集改动。"
        )
    } else {
        format!(
            "评审范围由 /review 参数指定：`{scope}`。\
             请先使用 git 工具确认当前分支与提交历史，再用 `git diff <范围>` 收集改动。"
        )
    };
    format!(
        "请评审当前工作区的代码变更。\n{scope_text}\n\n\
         执行步骤：\n\
         1. 用 git 工具收集 diff（status/diff/log/show 等只读子命令）；\n\
         2. 必要时用 read / grep 阅读受影响的文件与上下文；\n\
         3. 严格按系统提示中的评审标准独立评审。\n\n\
         输出要求：只输出系统提示中 OUTPUT FORMAT 规定的 JSON 审查结果本身，\
         不要输出 Markdown 代码块、XML 或任何额外解释。"
    )
}

/// 从子 Agent 输出中提取评审 JSON 对象；带围栏/前后缀时宽容解析。
pub(super) fn extract_review_json(text: &str) -> Option<Map<String, Value>> {
    let stripped = text.trim();
    if stripped.is_empty() {
        return None;
    }
    let mut candidates: Vec<String> = Vec::new();
    // 1) 整体直接解析
    candidates.push(stripped.to_string());
    // 2) 去掉 ```json ... ``` 围栏
    if let Some(captures) = review_fence_pattern().captures(stripped) {
        if let Some(group) = captures.get(1) {
            candidates.push(group.as_str().to_string());
        }
    }
    // 3) 取第一个 { 到最后一个 } 的子串
    if let (Some(first), Some(last)) = (stripped.find('{'), stripped.rfind('}')) {
        if last > first {
            candidates.push(stripped[first..=last].to_string());
        }
    }
    for candidate in candidates {
        if let Ok(parsed) = serde_json::from_str::<Value>(&candidate) {
            if let Some(object) = parsed.as_object() {
                return Some(object.clone());
            }
        }
    }
    None
}

/// 评审围栏：`r"```(?:json)?\s*(\{.*\})\s*```"`（DOTALL）。
fn review_fence_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"(?s)```(?:json)?\s*(\{.*\})\s*```").expect("评审围栏正则必须合法")
    })
}

/// 把 JSON 中的 priority 数值映射为 `[P0]`-`[P3]` 标签。
pub(super) fn format_priority_tag(priority: Option<&Value>) -> String {
    let Some(priority) = priority else {
        return String::new();
    };
    let value = match priority {
        Value::Number(number) => match number.as_i64() {
            Some(value) => value,
            None => match number.as_f64() {
                Some(float) => float as i64,
                None => return String::new(),
            },
        },
        Value::String(text) => match text.trim().parse::<i64>() {
            Ok(value) => value,
            Err(_) => return String::new(),
        },
        _ => return String::new(),
    };
    if !(0..=3).contains(&value) {
        return String::new();
    }
    format!("[P{value}] ")
}

/// Python `format(value, "g")`：6 位有效数字、去掉尾随零、超出范围走科学计数法。
pub(super) fn python_general_float(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 {
            "inf".to_string()
        } else {
            "-inf".to_string()
        };
    }
    if value == 0.0 {
        return "0".to_string();
    }
    const PRECISION: i32 = 6;
    let exponent = value.abs().log10().floor() as i32;
    if (-4..PRECISION).contains(&exponent) {
        let decimals = (PRECISION - 1 - exponent).max(0) as usize;
        return strip_trailing_zeros(&format!("{value:.decimals$}"));
    }
    let mantissa = value / 10f64.powi(exponent);
    let rendered = strip_trailing_zeros(&format!("{mantissa:.5}"));
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{rendered}e{sign}{:02}", exponent.abs())
}

fn strip_trailing_zeros(text: &str) -> String {
    if !text.contains('.') {
        return text.to_string();
    }
    let trimmed = text.trim_end_matches('0');
    let trimmed = trimmed.trim_end_matches('.');
    trimmed.to_string()
}

/// 把评审子 Agent 返回的 JSON 渲染为可读报告；解析失败时原样展示。
pub(super) fn format_review_report(review_text: &str) -> String {
    let Some(data) = extract_review_json(review_text) else {
        let stripped = review_text.trim();
        return if stripped.is_empty() {
            "（评审子 Agent 未返回内容。）".to_string()
        } else {
            stripped.to_string()
        };
    };

    let correctness = text_of(data.get("overall_correctness"));
    let explanation = text_of(data.get("overall_explanation")).trim().to_string();
    let confidence = data.get("overall_confidence_score");

    let correctness_norm = correctness.to_lowercase();
    let verdict = if correctness_norm.contains("incorrect") {
        "patch is incorrect ❌"
    } else if correctness_norm.contains("correct") {
        "patch is correct ✅"
    } else {
        "无法判定 ⚠️（overall_correctness 缺失或值无效）"
    };
    let confidence_text = match confidence.and_then(Value::as_f64) {
        Some(score) => format!("（置信度 {}）", python_general_float(score)),
        None => String::new(),
    };
    let mut lines = vec![
        "## 代码评审结果".to_string(),
        String::new(),
        format!("**总体结论**：{verdict}{confidence_text}"),
    ];
    if !explanation.is_empty() {
        lines.push(String::new());
        lines.push(explanation);
    }

    let findings = match data.get("findings") {
        Some(Value::Array(findings)) => findings.clone(),
        _ => {
            lines.push(String::new());
            lines.push("⚠️ 评审结果缺少有效的 findings 列表，无法展示问题明细。".to_string());
            return lines.join("\n");
        }
    };
    if findings.is_empty() {
        lines.push(String::new());
        lines.push("未发现问题。".to_string());
        return lines.join("\n");
    }

    lines.push(String::new());
    lines.push(format!("**发现问题 {} 项**：", findings.len()));
    lines.push(String::new());
    for (index, finding) in findings.iter().enumerate() {
        let number = index + 1;
        let Some(finding) = finding.as_object() else {
            continue;
        };
        let title = text_or(finding.get("title"), &format!("问题 {number}"));
        let tag = format_priority_tag(finding.get("priority"));
        let confidence_suffix = match finding.get("confidence_score").and_then(Value::as_f64) {
            Some(score) => format!("（置信度 {}）", python_general_float(score)),
            None => String::new(),
        };
        lines.push(format!("{number}. **{tag}{title}**{confidence_suffix}"));
        if let Some(location) = finding.get("code_location").and_then(Value::as_object) {
            let path = text_of(location.get("absolute_file_path"));
            let range = location.get("line_range").and_then(Value::as_object);
            let start = range
                .and_then(|map| map.get("start"))
                .and_then(Value::as_i64);
            let end = range.and_then(|map| map.get("end")).and_then(Value::as_i64);
            if !path.is_empty() {
                match (start, end) {
                    (Some(start), Some(end)) => {
                        lines.push(format!("   - 位置：`{path}` 行 {start}-{end}"))
                    }
                    _ => lines.push(format!("   - 位置：`{path}`")),
                }
            }
        }
        let body = text_of(finding.get("body")).trim().to_string();
        if !body.is_empty() {
            let indented: Vec<String> = body.lines().map(|line| format!("  {line}")).collect();
            lines.push(String::new());
            lines.push(indented.join("\n"));
        }
        lines.push(String::new());
    }
    lines.join("\n").trim_end().to_string()
}

/// 把评审范围转发给 review 子 Agent，并把 JSON 结果渲染为可读报告。
///
/// 主线程只解析参数并立即返回：git 预检、子 Agent 模型循环与报告注入都推迟到 `deferred` 中，
/// 由交互端的工作线程执行（`ctx.on_subagent_event` 透传给子任务，供 UI 显示审查进度）。
pub fn handle_review_command(ctx: &CommandContext<'_>) -> CommandResult {
    let scope = ctx.args.trim().to_string();
    let workspace_root = ctx.agent.workspace_root();
    let on_event = ctx.on_subagent_event.clone();

    let run_review = move |agent: &dyn CommandAgent| -> CommandResult {
        if let Some(error) = check_review_preconditions(agent, &workspace_root, &scope) {
            return CommandResult::message(error);
        }
        let report = match agent.run_subagent_task(SubAgentRun {
            agent_type: "review".to_string(),
            description: REVIEW_TASK_DESCRIPTION.to_string(),
            prompt: build_review_task_prompt(&scope),
            on_event: on_event.clone(),
        }) {
            Ok(report) => report,
            Err(error) => return CommandResult::message(format!("评审失败：{error}")),
        };
        let rendered = format_review_report(&report);
        // 报告进父模型上下文：下一轮模型请求能看到报告并继续处理（如修复、提交）。
        // 注入失败不影响报告展示。
        let _ = agent.remember_review_report(&rendered);
        CommandResult::message(rendered)
    };

    CommandResult::default()
        .with_working_status("正在评审")
        .with_stream_subagent_conversation()
        .with_deferred(Box::new(run_review))
}
