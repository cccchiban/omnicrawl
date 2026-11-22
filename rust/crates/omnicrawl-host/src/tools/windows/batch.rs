//! `windows_batch`：一次调用执行整段桌面操作脚本。
//!
//! 存在的理由是**回合数**：`computer_use` 每做一个动作就要截图、推理、再往返一次，
//! 几十次重复点击会把上下文吃光。这里把「看清一次界面 → 写一段脚本 → 批量执行 → 看一张
//! 结果图」压成两次调用，中间的动作序列由本模块自己跑完。
//!
//! 三条设计取舍：
//!
//! - **不是任意代码**：只有一组固定的 op，不做表达式求值、不读文件、不执行 shell。
//!   它比 `computer_use` 快，但能力面比 `bash` + `pyautogui` 窄得多，这是故意的。
//! - **先校验再动手**：整份脚本先解析成计划，参数错、坐标越界、未知 op 一律在发出第一个
//!   输入事件之前报错，不留半套动作。
//! - **只在检查点截图**：默认只在开头与结尾各截一张，中间用 `expect` 采样像素自检。
//!   每次动作都配图等于把 `computer_use` 的开销原样搬过来，那就没有意义了。

use omnicrawl_controllers::json::python_dumps;
use serde_json::{json, Map, Value};

use super::super::error::ToolError;
use super::super::paths::WorkspacePaths;
use super::args::ensure_allowed_keys;
use super::batch_plan;
use super::batch_run;
use super::screenshot::{snapshot_attachments, DesktopOutcome};

const MAX_NOTES: usize = 40;

const ALLOWED_KEYS: [&str; 6] = [
    "steps",
    "window",
    "window_handle",
    "coord",
    "dry_run",
    "on_error",
];

pub fn windows_batch(
    paths: &WorkspacePaths,
    arguments: &Map<String, Value>,
) -> Result<DesktopOutcome, ToolError> {
    ensure_allowed_keys(arguments, &ALLOWED_KEYS)?;
    let plan = batch_plan::parse(arguments)?;
    let report = batch_run::run(&plan)?;

    let mut payload = json!({
        "action": "batch",
        "dry_run": plan.dry_run,
        "step_count": report.total,
        "completed": report.completed,
        "stopped_at": report.stopped_at,
        "notes": trimmed(&report.notes),
        "samples": report.samples,
        "shot_labels": report.shots,
    });

    // 预演不碰桌面，因此也不该产生「操作后截图」：让模型看到的是它写的脚本走了哪条路。
    let images = if plan.dry_run {
        Vec::new()
    } else {
        snapshot_attachments(paths, &mut payload)
    };

    Ok(DesktopOutcome {
        output: python_dumps(&payload, 2),
        images,
    })
}

/// 逐步流水只保留开头与结尾：中间几百条「移动到哪里」对模型没有信息量。
fn trimmed(notes: &[String]) -> Vec<Value> {
    if notes.len() <= MAX_NOTES {
        return notes
            .iter()
            .map(|note| Value::from(note.as_str()))
            .collect();
    }
    let head: Vec<Value> = notes
        .iter()
        .take(MAX_NOTES / 2)
        .map(|note| Value::from(note.as_str()))
        .collect();
    let tail: Vec<Value> = notes
        .iter()
        .skip(notes.len() - MAX_NOTES / 2)
        .map(|note| Value::from(note.as_str()))
        .collect();
    let mut result = head;
    result.push(Value::from(format!(
        "…… 中间省略 {} 步",
        notes.len() - MAX_NOTES
    )));
    result.extend(tail);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    fn paths(name: &str) -> (WorkspacePaths, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("omnicrawl-batch-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        (WorkspacePaths::new(&root), root)
    }

    /// 预演：整份脚本走完、不产生输入事件，也不截图（桌面没被动过）。
    #[test]
    fn a_dry_run_walks_the_script_without_touching_the_desktop() {
        let (paths, root) = paths("dry-run");
        let outcome = windows_batch(
            &paths,
            &arguments(json!({
                "dry_run": true,
                "steps": [
                    {"op": "click", "at": [10, 20]},
                    {"op": "repeat", "times": 3, "steps": [{"op": "wait", "ms": 1}]},
                    {"op": "type", "text": "hello"}
                ]
            })),
        )
        .expect("预演应当成功");
        let payload: Value = serde_json::from_str(&outcome.output).expect("结果是 JSON");
        assert_eq!(payload["dry_run"], true);
        assert_eq!(payload["completed"], 5, "{payload}");
        assert!(payload["stopped_at"].is_null(), "{payload}");
        assert!(outcome.images.is_empty(), "预演不该截图");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 参数错误发生在动手之前：整份脚本一步都不执行。
    #[test]
    fn a_bad_step_aborts_before_any_input() {
        let (paths, root) = paths("bad-step");
        let error = windows_batch(
            &paths,
            &arguments(json!({
                "dry_run": true,
                "steps": [
                    {"op": "click", "at": [10, 20]},
                    {"op": "nope"}
                ]
            })),
        )
        .expect_err("未知 op 应当报错");
        // 错误定位到具体步骤，模型才知道改哪一条。
        assert!(error.message.contains("steps[1]"), "{}", error.message);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 顶层坐标原点会传给每一步：窗口相对坐标不需要脚本重复写 coord。
    #[test]
    fn the_top_level_coord_applies_to_every_step() {
        let (paths, root) = paths("coord");
        let error = windows_batch(
            &paths,
            &arguments(json!({
                "dry_run": true,
                "coord": "client",
                "steps": [{"op": "click", "at": [5, 5]}]
            })),
        )
        .expect_err("没有窗口时窗口相对坐标不能落地");
        assert!(
            error.message.contains("没有可用的目标窗口"),
            "{}",
            error.message
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unsupported_top_level_arguments_are_named() {
        let (paths, root) = paths("top-keys");
        let error = windows_batch(
            &paths,
            &arguments(json!({"steps": [{"op": "focus"}], "speed": 2})),
        )
        .expect_err("顶层未知参数应当被拒");
        assert!(error.message.contains("speed"), "{}", error.message);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 长流水只保留首尾，中间折叠成一行。
    #[test]
    fn long_note_streams_are_folded() {
        let notes: Vec<String> = (0..100).map(|index| format!("第 {index} 步")).collect();
        let folded = trimmed(&notes);
        assert!(
            folded.len() <= MAX_NOTES + 1,
            "折叠后不该更长：{}",
            folded.len()
        );
        assert!(
            folded
                .iter()
                .any(|item| item.as_str().unwrap_or_default().contains("中间省略")),
            "{folded:?}"
        );
    }
}
