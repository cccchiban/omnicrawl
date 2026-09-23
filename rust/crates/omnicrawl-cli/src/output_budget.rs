//! 工具输出预算与落盘归档：超限输出写进会话 artifact，模型上下文只留头尾预览与落盘路径。
//!
//! 语义基准是 `omnicrawl/agent/controllers/tools/output.py` 的 `_apply_batch_output_budget`；
//! 判定面（单工具阈值 50K / 批次预算 200K、预览文案）在 `omnicrawl-controllers::output`，
//! 本模块只负责「取尺寸 → 落盘 → 替换模型可见文本」的接线。

use std::collections::HashMap;

use omnicrawl_controllers::output::{plan_batch_archive, rewrite_archived_results};
use omnicrawl_controllers::types::ToolResult as ControllerToolResult;
use omnicrawl_core::AgentLoopObservation;
use omnicrawl_session::SessionStore;
use serde_json::json;

/// 按单工具阈值与批次聚合预算裁剪模型可见输出。
///
/// `session` 为 `None`（会话不在内核）时只替换成预览、不落盘，与 Python 侧
/// 「会话系统未启用时降级为纯预览提示」一致。
pub fn apply_batch_output_budget(
    observations: &mut [AgentLoopObservation],
    session: Option<(&SessionStore, &str)>,
) {
    if observations.is_empty() {
        return;
    }
    let sizes: Vec<Option<usize>> = observations
        .iter()
        .map(|observation| {
            let size = observation.result.output.chars().count();
            if size == 0 {
                None
            } else {
                Some(size)
            }
        })
        .collect();
    if sizes.iter().all(Option::is_none) {
        return;
    }
    let indices = plan_batch_archive(&sizes);
    if indices.is_empty() {
        return;
    }

    let mut paths: HashMap<usize, String> = HashMap::new();
    for index in &indices {
        let path = match session {
            Some((store, session_id)) => store
                .write_tool_result_artifact(session_id, &observations[*index].result.output)
                .map(|relative| store.root().join(relative).to_string_lossy().to_string())
                .unwrap_or_default(),
            None => String::new(),
        };
        paths.insert(*index, path);
    }

    let mut results: Vec<ControllerToolResult> = observations
        .iter()
        .map(|observation| ControllerToolResult {
            ok: observation.result.ok,
            output: observation.result.output.clone(),
            full_output: observation.result.full_output.clone(),
            error_code: observation.result.error_code.clone(),
            retryable: observation.result.retryable,
            ..ControllerToolResult::default()
        })
        .collect();
    rewrite_archived_results(&mut results, &indices, &paths);

    for (observation, result) in observations.iter_mut().zip(results) {
        observation.result.output = result.output;
        observation.result.full_output = result.full_output;
        let content = observation.result.output.clone();
        observation.message = json!({
            "role": "tool",
            "tool_call_id": observation.tool_call.id.clone(),
            "content": content,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnicrawl_controllers::shared::TOOL_OUTPUT_INLINE_LIMIT_CHARS;
    use omnicrawl_core::{ToolCall, ToolResult};

    fn observation(id: &str, output: String) -> AgentLoopObservation {
        let call = ToolCall {
            name: "bash".to_string(),
            arguments: serde_json::Map::new(),
            id: id.to_string(),
            function_name: "bash".to_string(),
        };
        AgentLoopObservation {
            tool_call: call.clone(),
            result: ToolResult {
                ok: true,
                output: output.clone(),
                full_output: output.clone(),
                error_code: None,
                retryable: false,
            },
            message: json!({"role": "tool", "tool_call_id": id, "content": output}),
            followup_messages: Vec::new(),
        }
    }

    #[test]
    fn small_outputs_are_left_alone() {
        let mut observations = vec![observation("c1", "短输出".to_string())];
        apply_batch_output_budget(&mut observations, None);
        assert_eq!(observations[0].result.output, "短输出");
        assert_eq!(observations[0].message["content"], "短输出");
    }

    #[test]
    fn oversized_output_falls_back_to_preview_without_a_session() {
        let big = "x".repeat(TOOL_OUTPUT_INLINE_LIMIT_CHARS + 10);
        let mut observations = vec![observation("c1", big.clone())];
        apply_batch_output_budget(&mut observations, None);
        let output = &observations[0].result.output;
        assert!(output.contains("输出太大（"), "{output}");
        assert!(output.contains("完整内容未能保存到磁盘。"), "{output}");
        // 完整原文保留在 full_output，未被预览覆盖。
        assert_eq!(observations[0].result.full_output, big);
        assert_eq!(observations[0].message["content"], *output);
    }

    #[test]
    fn archived_output_is_written_to_the_session_artifact() {
        let root = std::env::temp_dir().join("omnicrawl-cli-output-budget");
        let _ = std::fs::remove_dir_all(&root);
        let store = SessionStore::open(&root);
        store.ensure().expect("会话根应当就绪");
        let session_id = store
            .start_session(
                &root.to_string_lossy(),
                "输出预算测试",
                omnicrawl_session::utc_now(),
            )
            .expect("应当能建会话")
            .session_id;

        let big = "y".repeat(TOOL_OUTPUT_INLINE_LIMIT_CHARS + 10);
        let mut observations = vec![observation("c1", big.clone())];
        apply_batch_output_budget(&mut observations, Some((&store, &session_id)));

        let output = &observations[0].result.output;
        assert!(output.contains("完整内容已保存到："), "{output}");
        // 预览文本里带的是绝对路径，且文件内容就是完整原文。
        let path_line = output.rsplit("完整内容已保存到：").next().unwrap();
        let path = std::path::Path::new(path_line.trim());
        assert!(path.is_file(), "artifact 应当落盘：{}", path.display());
        assert_eq!(std::fs::read_to_string(path).expect("读回 artifact"), big);
        assert!(path.starts_with(&root));
    }
}
