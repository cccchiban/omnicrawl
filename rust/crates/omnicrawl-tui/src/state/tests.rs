//! 由 `state.rs` 迁出：状态机与回放用例。

use super::*;

#[cfg(test)]
mod tests {
    use super::*;
    use omnicrawl_core::{ToolCall, ToolResult};
    use omnicrawl_ipc::bridge::{
        TextPayload, TodoUpdatePayload, TokenUsagePayload, ToolEventPayload, ToolStartedPayload,
        TurnFinishedPayload,
    };
    use serde_json::json;

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            name: name.to_string(),
            arguments: json!({"path": "a.py"})
                .as_object()
                .cloned()
                .unwrap_or_default(),
            id: id.to_string(),
            function_name: name.to_string(),
        }
    }

    fn state() -> AppState {
        let mut state = AppState::new(
            "demo".to_string(),
            "test-model".to_string(),
            ApprovalMode::Manual,
        );
        // 用例只验流水：目录指到临时区，且不给可执行文件（否则真会弹窗跑起工作台）。
        // 每次新建一个子目录：用例并行跑，共用目录时清理会互相删掉对方的流水。
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        state.subagent_stream_root =
            std::env::temp_dir().join(format!("oc-state-subwin-{}-{unique}", std::process::id()));
        state
    }

    #[test]
    fn deltas_merge_into_one_record_per_segment() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问一句".to_string(), Vec::new());
        state.apply(
            &HostEvent::ReasoningDelta(TextPayload { text: "想".into() }),
            now,
        );
        state.apply(
            &HostEvent::ReasoningDelta(TextPayload {
                text: "一下".into(),
            }),
            now,
        );
        state.apply(
            &HostEvent::Delta(TextPayload {
                text: "回答".into(),
            }),
            now,
        );
        state.apply(
            &HostEvent::Delta(TextPayload {
                text: "结束".into(),
            }),
            now,
        );

        assert_eq!(
            state.records.len(),
            3,
            "用户 + 思考 + 正文：{:?}",
            state.records
        );
        assert_eq!(
            state.records[0],
            Record::User(UserRecord::text_only("问一句"))
        );
        assert_eq!(state.records[1], Record::Reasoning("想一下".to_string()));
        assert_eq!(state.records[2], Record::Assistant("回答结束".to_string()));
    }

    /// 思考段逐帧铺开：大突发不会在同一帧里整段蹦出，但几帧内一定能铺完。
    #[test]
    fn reasoning_reveal_spreads_a_burst_across_frames() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问一句".to_string(), Vec::new());
        let burst: String = "想".repeat(300);
        state.apply(
            &HostEvent::ReasoningDelta(TextPayload {
                text: burst.clone(),
            }),
            now,
        );

        let total = burst.chars().count();
        assert_eq!(
            state.reasoning_visible_chars(1, total),
            0,
            "首帧一个字符都不该先显示出来"
        );
        assert!(state.is_reasoning_revealing());

        // 逐帧推进：每次只铺开一部分，绝不会一帧到齐。
        let mut frames = 0;
        while state.tick_reasoning_reveal() {
            frames += 1;
            let visible = state.reasoning_visible_chars(1, total);
            assert!(visible < total, "还有剩就该「没铺完」，第 {frames} 帧");
            assert!(frames < 200, "铺开过程不该无限长");
        }
        assert_eq!(
            state.reasoning_visible_chars(1, total),
            total,
            "最终必须铺完"
        );
        assert!(!state.is_reasoning_revealing());

        // 记录本身始终是完整思考：截断只发生在渲染层。
        assert_eq!(state.records[1], Record::Reasoning(burst));
    }

    /// 慢流（积压小于每帧最小步长）立刻显示，不额外滞后于模型。
    #[test]
    fn reasoning_reveal_keeps_up_with_a_slow_stream() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问一句".to_string(), Vec::new());
        state.apply(
            &HostEvent::ReasoningDelta(TextPayload { text: "想".into() }),
            now,
        );
        assert_eq!(state.reasoning_visible_chars(1, 1), 0, "首片先不显示");
        state.tick_reasoning_reveal();
        assert_eq!(
            state.reasoning_visible_chars(1, 1),
            1,
            "一帧之内就该跟上单字符增量"
        );
    }

    /// 折叠/展开正在流式输出的思考段：不能把已铺开的内容清掉重铺。
    ///
    /// 回归的是「点击折叠（或展开）后，后续增量被当成首片、整段思考从头再流一遍」：
    /// `settle` 只关掉逐帧揭示，必须保留当前下标，后续增量才认得「还是这一条」。
    #[test]
    fn toggling_a_reasoning_block_does_not_restart_the_stream() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问一句".to_string(), Vec::new());
        state.apply(
            &HostEvent::ReasoningDelta(TextPayload {
                text: "第一段".repeat(20),
            }),
            now,
        );
        // 铺一部分但还没铺完，此时用户点了折叠。
        state.tick_reasoning_reveal();
        state.toggle_reasoning_expanded(1);
        assert_eq!(
            state.reasoning_visible_chars(1, 60),
            60,
            "用户操作后当场显示全文，不等逐帧铺开"
        );
        assert!(!state.is_reasoning_revealing(), "不该还在逐帧揭示");

        // 思考继续流：新内容直接追加显示，不能退回从第 0 个字符重铺。
        state.apply(
            &HostEvent::ReasoningDelta(TextPayload {
                text: "第二段".to_string(),
            }),
            now,
        );
        assert_eq!(
            state.reasoning_visible_chars(1, 63),
            63,
            "后续增量应整段可见，不得从头重铺"
        );
        assert!(
            !state.tick_reasoning_reveal(),
            "已经停止揭示的段不该再驱动帧循环"
        );
        assert_eq!(
            state.reasoning_visible_chars(1, 63),
            63,
            "推进帧也不能把可见长度退回去"
        );
    }

    /// 同一段思考反复折叠/展开，可见长度始终等于全文。
    #[test]
    fn repeated_toggles_keep_the_full_text_visible() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问一句".to_string(), Vec::new());
        state.apply(
            &HostEvent::ReasoningDelta(TextPayload {
                text: "想".repeat(100),
            }),
            now,
        );
        for _ in 0..4 {
            state.toggle_reasoning_expanded(1);
            assert_eq!(state.reasoning_visible_chars(1, 100), 100);
            state.apply(
                &HostEvent::ReasoningDelta(TextPayload { text: "续".into() }),
                now,
            );
        }
        assert_eq!(
            state.reasoning_visible_chars(1, 104),
            104,
            "四次折叠/展开后仍应按全文显示"
        );
    }

    /// 收口路径（回合结束/失败/回放）当场补全，不留半句在屏幕上。
    #[test]
    fn reasoning_reveal_settles_on_turn_end_and_replay() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问一句".to_string(), Vec::new());
        state.apply(
            &HostEvent::ReasoningDelta(TextPayload {
                text: "想".repeat(200),
            }),
            now,
        );
        state.settle_reasoning_reveal();
        assert_eq!(state.reasoning_visible_chars(1, 200), 200);
        assert!(!state.is_reasoning_revealing());

        // 又一段流式思考后回合结束：同样立刻铺完。
        state.apply(
            &HostEvent::ReasoningDelta(TextPayload {
                text: "再".repeat(200),
            }),
            now,
        );
        state.apply(
            &HostEvent::TurnFinished(TurnFinishedPayload {
                turn_id: "t1".to_string(),
                final_text: "答复".to_string(),
                reasoning: String::new(),
                model_turns: 1,
                tool_calls: 0,
                paused: false,
                post_compaction_context_tokens: None,
            }),
            now,
        );
        assert!(
            !state.is_reasoning_revealing(),
            "回合结束后不该还有内容在慢慢铺"
        );
        assert_eq!(state.reasoning_visible_chars(1, 400), 400);

        // 回放历史属于「已发生的事实」，直接全文显示。
        state.apply(
            &HostEvent::ReasoningDelta(TextPayload {
                text: "又".repeat(100),
            }),
            now,
        );
        state.replay_history(&[]);
        assert!(!state.is_reasoning_revealing());
    }

    /// 正文（非思考）不受显现机制影响：它一直是即时逐片追加。
    #[test]
    fn reasoning_reveal_never_truncates_assistant_text() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问一句".to_string(), Vec::new());
        state.apply(
            &HostEvent::Delta(TextPayload {
                text: "正".repeat(500),
            }),
            now,
        );
        assert_eq!(state.records[1], Record::Assistant("正".repeat(500)));
        assert!(
            !state.is_reasoning_revealing(),
            "正文不参与显现，不该出现「还在铺」的状态"
        );
    }

    #[test]
    fn tool_cards_track_status_body_and_elapsed() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "跑一下".to_string(), Vec::new());
        let tool_call = call("c1", "bash");
        state.apply(
            &HostEvent::ToolStarted(ToolStartedPayload {
                step: 1,
                call: tool_call.clone(),
            }),
            now,
        );
        state.apply(
            &HostEvent::ToolFinished(ToolEventPayload {
                call: tool_call.clone(),
                result: ToolResult {
                    ok: false,
                    output: "第一行\n第二行".to_string(),
                    full_output: String::new(),
                    error_code: Some("denied".to_string()),
                    retryable: false,
                },
            }),
            now + Duration::from_millis(120),
        );

        match state.records.last() {
            Some(Record::Tool(card)) => {
                assert_eq!(card.status, ToolStatus::Denied);
                assert_eq!(card.body, "第一行\n第二行");
                assert_eq!(card.elapsed, Some(Duration::from_millis(120)));
            }
            other => panic!("应当有工具卡，实际：{other:?}"),
        }
    }

    /// 同一个后台任务的每批增量都落在同一张卡上：状态行换新、事件行累积。
    #[test]
    fn monitor_batches_merge_into_one_card_per_task() {
        let mut state = state();
        state.push_monitor_batch(
            "m1",
            "running",
            "Monitor · m1 · running\n[system] 已启动，shell=bash。".to_string(),
        );
        state.push_monitor_batch(
            "m1",
            "running",
            "Monitor · m1 · running\n[stdout] ready in 300ms".to_string(),
        );
        state.push_monitor_batch(
            "m1",
            "completed",
            "Monitor · m1 · completed\n[stdout] done".to_string(),
        );

        let cards: Vec<&ToolCard> = state
            .records
            .iter()
            .filter_map(|record| match record {
                Record::Tool(card) => Some(card),
                _ => None,
            })
            .collect();
        assert_eq!(cards.len(), 1, "一个后台任务只占一张卡：{cards:?}");
        assert_eq!(cards[0].call_id, "monitor:m1");
        assert_eq!(cards[0].status, ToolStatus::Ok, "终态跟随最新快照");
        assert_eq!(
            cards[0].body,
            "Monitor · m1 · completed\n[system] 已启动，shell=bash。\n[stdout] ready in 300ms\n[stdout] done",
            "状态行原地替换，事件行按批追加"
        );
        assert!(cards[0].elapsed.is_some(), "终态卡片的耗时要落定");
    }

    /// 不同后台任务各自一张卡，互不覆盖。
    #[test]
    fn monitor_batches_of_different_tasks_keep_separate_cards() {
        let mut state = state();
        state.push_monitor_batch("m1", "running", "Monitor · m1 · running".to_string());
        state.push_monitor_batch("m2", "running", "Monitor · m2 · running".to_string());
        state.push_monitor_batch(
            "m1",
            "running",
            "Monitor · m1 · running\n[stdout] 一行".to_string(),
        );

        let ids: Vec<&str> = state
            .records
            .iter()
            .filter_map(|record| match record {
                Record::Tool(card) => Some(card.call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["monitor:m1", "monitor:m2"]);
        let m1 = match &state.records[0] {
            Record::Tool(card) => card,
            other => panic!("第一张应是 m1 的卡：{other:?}"),
        };
        assert_eq!(m1.body.lines().count(), 2, "m1 的第二批追加到自己的卡上");
    }

    /// 「立即结束」按钮的两类卡片都要能定位到任务 id。
    ///
    /// 真实 `monitor` 工具卡的 `call_id` 是内核 id（如 `call_1`），任务 id 只在参数里；
    /// 只认 `monitor:` 前缀时按钮画得出来却点不动、也不亮——用户报的正是这个。
    #[test]
    fn monitor_task_id_resolves_both_card_shapes() {
        let mut state = state();
        // 归并卡：任务 id 在 call_id 里。
        state.push_monitor_batch("m1", "running", "Monitor · m1 · running".to_string());
        assert_eq!(state.monitor_task_id("monitor:m1").as_deref(), Some("m1"));

        // 真实工具卡：任务 id 在参数里，工具名可能带命名空间前缀。
        let mut running = call("call_1", "monitor");
        running.arguments = json!({"action": "poll", "monitor_id": "m2", "wait_seconds": 30})
            .as_object()
            .cloned()
            .unwrap_or_default();
        state.begin_tool_run(&running, Instant::now());
        assert_eq!(state.monitor_task_id("call_1").as_deref(), Some("m2"));

        // 非 monitor 工具、缺 monitor_id、终态卡片都不该给出任务 id。
        let bash = call("call_2", "bash");
        state.begin_tool_run(&bash, Instant::now());
        assert_eq!(state.monitor_task_id("call_2"), None);
        let mut no_id = call("call_3", "monitor");
        no_id.arguments = json!({"action": "list"})
            .as_object()
            .cloned()
            .unwrap_or_default();
        state.begin_tool_run(&no_id, Instant::now());
        assert_eq!(state.monitor_task_id("call_3"), None);
        assert_eq!(state.monitor_task_id("monitor:"), None);
        assert_eq!(state.monitor_task_id("不存在"), None);

        state.finish_tool_run(
            &running,
            &ToolResult {
                ok: true,
                output: "已返回".to_string(),
                full_output: String::new(),
                error_code: None,
                retryable: false,
            },
            Instant::now(),
        );
        assert_eq!(
            state.monitor_task_id("call_1"),
            None,
            "跑完的卡片没有按钮，也就不该再给出任务 id"
        );
    }

    #[test]
    fn stream_rollback_drops_half_streamed_record() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问".to_string(), Vec::new());
        state.apply(
            &HostEvent::Delta(TextPayload {
                text: "半截".into(),
            }),
            now,
        );
        state.apply(&HostEvent::StreamRollback, now);
        assert_eq!(state.records.len(), 1, "正文记录应被撤销");
        assert!(state.telemetry.rate.value().is_none(), "回滚后速度归零");
    }

    /// 状态行写在输入块里：`Status` / `RetryStatus` / `ProtocolWait` 都必须把输入块置脏。
    ///
    /// 这三个事件以前直接给 `state.status` 赋值（绕过 `set_status`），三块刷新判定「没变」
    /// 就让事件循环整帧跳过绘制——状态文字停在旧值上不刷新。用户报的「模型重试之后状态
    /// 显示不会自动刷新」就是这条。
    #[test]
    fn kernel_status_events_mark_the_composer_dirty() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问".to_string(), Vec::new());
        let base = state.refresh_versions();

        state.apply(
            &HostEvent::RetryStatus(omnicrawl_ipc::bridge::MessagePayload {
                message: "正在重试(第1次)".to_string(),
            }),
            now,
        );
        let after_retry = state.refresh_versions();
        assert_eq!(state.status.as_deref(), Some("正在重试(第1次)"));
        assert_eq!(
            after_retry.composer,
            base.composer + 1,
            "重试提示写在输入框上边框上，必须置脏输入块"
        );

        state.apply(&HostEvent::ProtocolWait, now);
        let after_wait = state.refresh_versions();
        assert_eq!(state.status.as_deref(), Some("等待协议…"));
        assert_eq!(after_wait.composer, after_retry.composer + 1);

        state.apply(
            &HostEvent::Status(omnicrawl_ipc::bridge::MessagePayload {
                message: "网关降级，正在重试".to_string(),
            }),
            now,
        );
        assert_eq!(
            state.refresh_versions().composer,
            after_wait.composer + 1,
            "普通状态提示同样要置脏"
        );
    }

    /// 重试提示只是「此刻在等什么」，模型一旦重新出内容就该让位回运行态。
    ///
    /// 不留旧提示的话，状态行会一直写着「正在重试(第N次)」，看起来像卡在重试上。
    #[test]
    fn streaming_clears_the_retry_prompt() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问".to_string(), Vec::new());
        state.apply(
            &HostEvent::RetryStatus(omnicrawl_ipc::bridge::MessagePayload {
                message: "请求失败，正在自动重试（第1次）".to_string(),
            }),
            now,
        );
        assert!(state.status.is_some());

        state.apply(
            &HostEvent::Delta(TextPayload {
                text: "答复".into(),
            }),
            now,
        );
        assert_eq!(state.status, None, "正文到达后重试提示应当让位");
        let after = state.refresh_versions();
        state.apply(
            &HostEvent::RetryStatus(omnicrawl_ipc::bridge::MessagePayload {
                message: "请求失败，正在自动重试（第2次）".to_string(),
            }),
            now,
        );
        assert_eq!(
            state.status.as_deref(),
            Some("请求失败，正在自动重试（第2次）")
        );

        // 思考增量同理：模型重新动起来就不再是「等待」。
        state.apply(
            &HostEvent::ReasoningDelta(TextPayload {
                text: "再想想".into(),
            }),
            now,
        );
        assert_eq!(state.status, None, "思考增量到达后提示也应当让位");
        assert!(state.refresh_versions().composer >= after.composer);
    }

    /// 空闲时的状态行（慢命令的「正在评审」）不被子代理的流式增量顶掉。
    #[test]
    fn idle_status_survives_subagent_streaming() {
        let mut state = state();
        let now = Instant::now();
        state.set_status(Some("正在评审".to_string()));
        state.apply(
            &HostEvent::Delta(TextPayload {
                text: "评审正文".into(),
            }),
            now,
        );
        assert_eq!(
            state.status.as_deref(),
            Some("正在评审"),
            "回合没在跑时不能清掉慢命令的状态行"
        );
    }

    #[test]
    fn token_usage_and_finish_reset_status() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问".to_string(), Vec::new());
        state.apply(
            &HostEvent::Status(omnicrawl_ipc::bridge::MessagePayload {
                message: "正在分析".to_string(),
            }),
            now,
        );
        assert_eq!(state.status.as_deref(), Some("正在分析"));
        state.apply(
            &HostEvent::TokenUsage(TokenUsagePayload {
                input_tokens: 10,
                output_tokens: 20,
                cached_input_tokens: 5,
            }),
            now,
        );
        state.apply(
            &HostEvent::TurnFinished(TurnFinishedPayload {
                turn_id: "t1".to_string(),
                final_text: "完成".to_string(),
                reasoning: String::new(),
                model_turns: 1,
                tool_calls: 0,
                paused: false,
                post_compaction_context_tokens: None,
            }),
            now,
        );
        assert!(!state.turn.is_running());
        assert_eq!(state.status, None);
        assert_eq!(state.telemetry.input_tokens, 10);
        assert_eq!(state.telemetry.cached_input_tokens, 5);
    }

    /// 压缩后遥测要换成压缩后的真实上下文大小，↓/† 清零（它们属于那一次已经过去的请求）。
    #[test]
    fn context_compaction_refreshes_telemetry_to_the_post_compaction_size() {
        let mut state = state();
        let now = Instant::now();
        state.apply(
            &HostEvent::TokenUsage(TokenUsagePayload {
                input_tokens: 10,
                output_tokens: 20,
                cached_input_tokens: 5,
            }),
            now,
        );
        state.apply(
            &HostEvent::ContextCompaction(omnicrawl_ipc::bridge::ContextCompactionPayload {
                post_turn_context_tokens: 150_000,
                trigger_context_tokens: 100_000,
                turn_id: "t1".to_string(),
                post_compaction_context_tokens: Some(12_000),
            }),
            now,
        );
        assert_eq!(
            state.telemetry.input_tokens, 12_000,
            "CTX 与 ↑ 换成压缩后大小"
        );
        assert_eq!(
            state.telemetry.output_tokens, 0,
            "↓ 属于上一次请求，压缩后清零"
        );
        assert_eq!(state.telemetry.cached_input_tokens, 0, "† 同上");
    }

    /// 回合末工具调用压缩走 `turn.finished` 带同一字段。
    #[test]
    fn turn_finished_carries_the_tool_compaction_size() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问".to_string(), Vec::new());
        state.apply(
            &HostEvent::TokenUsage(TokenUsagePayload {
                input_tokens: 10,
                output_tokens: 20,
                cached_input_tokens: 5,
            }),
            now,
        );
        state.apply(
            &HostEvent::TurnFinished(TurnFinishedPayload {
                turn_id: "t1".to_string(),
                final_text: "完成".to_string(),
                reasoning: String::new(),
                model_turns: 1,
                tool_calls: 1,
                paused: false,
                post_compaction_context_tokens: Some(8_000),
            }),
            now,
        );
        assert_eq!(state.telemetry.input_tokens, 8_000);
        assert_eq!(state.telemetry.output_tokens, 0);
        assert_eq!(state.telemetry.cached_input_tokens, 0);
    }

    /// 没有压缩的回合不能动遥测：字段缺省时维持模型请求回报的用量。
    #[test]
    fn turn_finished_without_compaction_keeps_the_usage() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问".to_string(), Vec::new());
        state.apply(
            &HostEvent::TokenUsage(TokenUsagePayload {
                input_tokens: 10,
                output_tokens: 20,
                cached_input_tokens: 5,
            }),
            now,
        );
        state.apply(
            &HostEvent::TurnFinished(TurnFinishedPayload {
                turn_id: "t1".to_string(),
                final_text: "完成".to_string(),
                reasoning: String::new(),
                model_turns: 1,
                tool_calls: 0,
                paused: false,
                post_compaction_context_tokens: None,
            }),
            now,
        );
        assert_eq!(state.telemetry.input_tokens, 10);
        assert_eq!(state.telemetry.output_tokens, 20);
        assert_eq!(state.telemetry.cached_input_tokens, 5);
    }

    /// 会话区提示不能走状态行：状态行要留着显示「正在调用」等运行状态。
    #[test]
    fn notice_events_go_to_the_conversation_and_keep_the_status_line() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问".to_string(), Vec::new());
        state.apply(
            &HostEvent::Status(omnicrawl_ipc::bridge::MessagePayload {
                message: "正在调用".to_string(),
            }),
            now,
        );
        state.apply(
            &HostEvent::Notice(omnicrawl_ipc::bridge::MessagePayload {
                message: "检测到 1 处疑似畸形脱敏占位符，已按原样保留。".to_string(),
            }),
            now,
        );
        assert_eq!(
            state.status.as_deref(),
            Some("正在调用"),
            "会话区提示不得覆盖运行状态行"
        );
        assert!(
            state.records.iter().any(|record| matches!(
                record,
                Record::Notice(text) if text.contains("疑似畸形")
            )),
            "会话区应当出现这条提示：{:?}",
            state.records
        );
    }

    /// 提示插在流式正文中间时不能把一段回复拆成两条记录。
    #[test]
    fn notice_mid_stream_does_not_split_the_reply() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问".to_string(), Vec::new());
        state.apply(
            &HostEvent::Delta(TextPayload {
                text: "前半".into(),
            }),
            now,
        );
        state.apply(
            &HostEvent::Notice(omnicrawl_ipc::bridge::MessagePayload {
                message: "检测到 1 处疑似畸形脱敏占位符，已按原样保留。".to_string(),
            }),
            now,
        );
        state.apply(
            &HostEvent::Delta(TextPayload {
                text: "后半".into(),
            }),
            now,
        );

        let assistants: Vec<String> = state
            .records
            .iter()
            .filter_map(|record| match record {
                Record::Assistant(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            assistants,
            vec!["前半后半".to_string()],
            "提示不得把一段回复拆开：{:?}",
            state.records
        );
    }

    #[test]
    fn todos_arrive_from_both_tool_and_notification() {
        let mut state = state();
        let now = Instant::now();
        let step = state.start_batch(
            Id::Number(1),
            vec![ToolCall {
                name: host::TODO_TOOL.to_string(),
                arguments: json!({"todos": [{"id": "1", "step": "写骨架", "completed": false}]})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                id: "c1".to_string(),
                function_name: host::TODO_TOOL.to_string(),
            }],
        );
        assert_eq!(step, host::BatchStep::Complete, "自持工具不需要用户介入");
        assert_eq!(state.todos.len(), 1);
        assert_eq!(state.todos[0].step, "写骨架");

        state.apply(
            &HostEvent::TodoUpdate(TodoUpdatePayload {
                todos: json!([{"id": "2", "step": "接审批", "completed": true}]),
            }),
            now,
        );
        assert_eq!(state.todos.len(), 1);
        assert!(state.todos[0].completed);
    }

    #[test]
    fn batch_waiting_for_approval_can_be_decided_through_state() {
        let mut state = state();
        let step = state.start_batch(Id::Number(9), vec![call("c1", "bash")]);
        assert_eq!(step, host::BatchStep::Awaiting, "manual 模式应停在审批");
        assert!(matches!(state.waiting(), Some(Waiting::Approval(_))));

        let jobs = match state.decide_approval(true).expect("批准后应有下一步") {
            host::BatchStep::Execute(jobs) => jobs,
            other => panic!("批准后应派发执行，实际：{other:?}"),
        };
        assert_eq!(jobs.len(), 1);

        // 执行层回填结果：整批就绪后取走观察。
        let call = jobs[0].1.clone();
        assert!(state.record_tool_result(
            jobs[0].0,
            omnicrawl_core::ToolResult {
                ok: true,
                output: "文件内容".to_string(),
                full_output: "文件内容".to_string(),
                error_code: None,
                retryable: false,
            },
            None
        ));
        let observations = state.take_observations(false).expect("整批就绪应能取观察");
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].tool_call.id, call.id);
        assert_eq!(observations[0].result.output, "文件内容");
        assert!(state.batch_request_id().is_none(), "整批结束后不再挂起");
    }

    #[test]
    fn cancelling_a_batch_drops_it_without_observations() {
        let mut state = state();
        state.start_batch(Id::Number(11), vec![call("c1", "bash")]);
        assert!(state.waiting().is_some());
        state.cancel_batch();
        assert!(state.waiting().is_none());
        assert!(state.take_observations(false).is_none());
    }

    /// 子代理批次的调用照旧要执行，但卡片属于子代理窗口：主会话里一张都不该多出来。
    #[test]
    fn subagent_batches_never_reach_the_main_conversation() {
        let mut state = AppState::new(
            "demo".to_string(),
            "test-model".to_string(),
            ApprovalMode::Auto,
        );
        let tool_call = call("c1", "bash");
        let step = state.start_subagent_batch(Id::Number(31), vec![tool_call.clone()]);
        let jobs = match step {
            host::BatchStep::Execute(jobs) => jobs,
            other => panic!("auto 模式应派发执行，实际：{other:?}"),
        };
        state.begin_tool_run(&jobs[0].1, Instant::now());
        assert!(
            !state
                .records
                .iter()
                .any(|record| matches!(record, Record::Tool(_))),
            "子代理的工具卡不该进主会话：{:?}",
            state.records
        );

        // 结果照旧回填：批次仍要整批就绪，否则内核会一直等观察。
        assert!(state.record_tool_result(
            0,
            omnicrawl_core::ToolResult {
                ok: true,
                output: "输出".to_string(),
                full_output: "输出".to_string(),
                error_code: None,
                retryable: false,
            },
            None
        ));
        let observations = state.take_observations(false).expect("整批就绪");
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].result.output, "输出");
        assert!(
            !state
                .records
                .iter()
                .any(|record| matches!(record, Record::Tool(_))),
            "回填之后也不该冒出一张卡：{:?}",
            state.records
        );
    }

    /// 安静批次结束（卸下批次）之后，下一次普通批次照旧画卡。
    #[test]
    fn quiet_batches_do_not_poison_the_next_batch() {
        let mut state = AppState::new(
            "demo".to_string(),
            "test-model".to_string(),
            ApprovalMode::Auto,
        );
        let _ = state.start_subagent_batch(Id::Number(41), vec![call("c1", "bash")]);
        state.cancel_batch();
        let step = state.start_batch(Id::Number(42), vec![call("c2", "bash")]);
        let jobs = match step {
            host::BatchStep::Execute(jobs) => jobs,
            other => panic!("auto 模式应派发执行，实际：{other:?}"),
        };
        state.begin_tool_run(&jobs[0].1, Instant::now());
        assert!(
            state
                .records
                .iter()
                .any(|record| matches!(record, Record::Tool(card) if card.call_id == "c2")),
            "普通批次的卡片要照旧出现：{:?}",
            state.records
        );
    }

    #[test]
    fn tool_timeout_reaps_running_cards_and_notes_the_user() {
        let mut state = AppState::new(
            "demo".to_string(),
            "test-model".to_string(),
            ApprovalMode::Auto,
        );
        let tool_call = call("c1", "bash");
        let step = state.start_batch(Id::Number(21), vec![tool_call.clone()]);
        let jobs = match step {
            host::BatchStep::Execute(jobs) => jobs,
            other => panic!("auto 模式应派发执行，实际：{other:?}"),
        };
        assert_eq!(jobs.len(), 1);
        state.begin_tool_run(&jobs[0].1, Instant::now());

        assert!(state.fill_tool_timeout(600), "超时回填后整批就绪");

        match state
            .records
            .iter()
            .rev()
            .find(|record| matches!(record, Record::Tool(_)))
        {
            Some(Record::Tool(card)) => {
                assert_eq!(card.status, ToolStatus::Failed, "运行中的卡片要被收口");
                assert!(
                    card.body.lines().any(|line| line.contains("工具执行超时")),
                    "{:?}",
                    card.body
                );
            }
            other => panic!("应当有工具卡，实际：{other:?}"),
        }
        assert!(
            state.records.iter().any(
                |record| matches!(record, Record::Notice(text) if text.contains("已按超时继续"))
            ),
            "应当给用户一条超时提示：{:?}",
            state.records
        );
        let observations = state.take_observations(false).expect("整批就绪");
        assert!(observations[0].result.output.contains("工具执行超时"));
        assert_eq!(
            observations[0].result.error_code, None,
            "与 Python 一致：超时结果只有文案，不带错误码"
        );
    }

    #[test]
    fn tool_runs_are_rendered_as_cards() {
        let mut state = state();
        let now = Instant::now();
        let tool_call = call("c1", "bash");
        state.begin_tool_run(&tool_call, now);
        match state.records.last() {
            Some(Record::Tool(card)) => {
                assert_eq!(card.status, ToolStatus::Running);
                assert_eq!(card.name, "bash");
                assert!(card.elapsed.is_none());
            }
            other => panic!("应落一张运行中的工具卡，实际：{other:?}"),
        }
        state.finish_tool_run(
            &tool_call,
            &omnicrawl_core::ToolResult {
                ok: false,
                output: "出错了".to_string(),
                full_output: "出错了".to_string(),
                error_code: Some("FS_EDIT_NOT_FOUND".to_string()),
                retryable: false,
            },
            now + Duration::from_millis(30),
        );
        match state.records.last() {
            Some(Record::Tool(card)) => {
                assert_eq!(card.status, ToolStatus::Failed);
                assert_eq!(card.body, "出错了");
                assert_eq!(card.elapsed, Some(Duration::from_millis(30)));
            }
            other => panic!("应更新同一张工具卡，实际：{other:?}"),
        }
    }

    #[test]
    fn subagent_batch_waits_for_the_click_before_opening_a_window() {
        let mut state = state();
        let now = Instant::now();
        let call = call("c1", "subagent");
        state.begin_tool_run(&call, now);
        state.apply(
            &subagent_event_with_call("subagent.task.queued", "t1", "batch-1", "c1"),
            now,
        );
        state.apply(
            &subagent_event("subagent.task.completed", "t1", "batch-1"),
            now,
        );

        // 没有工具卡发起的批次（`/review` 那类）照旧直接开窗并留一行受理提示。
        assert!(
            state
                .records
                .iter()
                .all(|record| !matches!(record, Record::SubagentWindow(_))),
            "工具卡发起的批次不该自动开窗：{:?}",
            state.records
        );
        assert!(!state.subagent_window.is_opened("batch-1"));
        // 但按钮的归属已经绑定在那张卡上。
        assert_eq!(state.subagent_view_batch("c1"), Some("batch-1"));

        // 流水照旧按到达顺序记下两次状态。
        let stream = state.subagent_stream_root.join("batch-1.jsonl");
        let text = std::fs::read_to_string(&stream).expect("流水可读");
        let statuses: Vec<String> = text
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).expect("每行都是 JSON")["status"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        assert_eq!(statuses, vec!["queued", "completed"]);
        cleanup_state(&state);
    }

    #[test]
    fn a_host_driven_batch_still_opens_its_window_and_leaves_one_notice() {
        let mut state = state();
        let now = Instant::now();
        state.apply(
            &subagent_event("subagent.task.queued", "t1", "batch-1"),
            now,
        );
        state.apply(
            &subagent_event("subagent.task.running", "t2", "batch-2"),
            now,
        );

        let notices: Vec<&SubagentWindowNotice> = state
            .records
            .iter()
            .filter_map(|record| match record {
                Record::SubagentWindow(notice) => Some(notice),
                _ => None,
            })
            .collect();
        assert_eq!(
            notices.len(),
            2,
            "每个 batch_id 开一次窗口：{:?}",
            state.records
        );
        assert!(notices[0].stream.ends_with("batch-1.jsonl"));
        assert!(state.subagent_window.is_opened("batch-1"));
        cleanup_state(&state);
    }

    #[test]
    fn conversation_events_join_the_open_window_without_new_records() {
        let mut state = state();
        let now = Instant::now();
        state.apply(
            &subagent_event("subagent.task.started", "t1", "batch-1"),
            now,
        );
        let before = state.records.len();
        state.apply(
            &conversation_event(
                "subagent.turn.text",
                "t1",
                "batch-1",
                json!({"text": "正在看代码"}),
            ),
            now,
        );
        state.apply(
            &conversation_event(
                "subagent.tool.started",
                "t1",
                "batch-1",
                json!({"tool": "read", "arguments": {"path": "a.py"}}),
            ),
            now,
        );
        assert_eq!(
            state.records.len(),
            before,
            "过程事件只进窗口，不再往消息流里加记录"
        );
        let text = std::fs::read_to_string(state.subagent_stream_root.join("batch-1.jsonl"))
            .expect("流水可读");
        assert!(text.contains("正在看代码"), "{text}");
        assert!(text.contains("\"tool\":\"read\""), "{text}");
        cleanup_state(&state);
    }

    #[test]
    fn batch_events_alone_do_not_open_a_window() {
        let mut state = state();
        let now = Instant::now();
        state.apply(
            &HostEvent::SubagentEvent(omnicrawl_ipc::bridge::SubagentEventPayload {
                name: "subagent.batch.created".to_string(),
                payload: json!({"batch_id": "batch-9", "task_count": 2}),
            }),
            now,
        );
        assert!(state.records.is_empty(), "{:?}", state.records);
        assert!(!state.subagent_window.is_opened("batch-9"));
    }

    fn subagent_event(name: &str, task_id: &str, batch_id: &str) -> HostEvent {
        HostEvent::SubagentEvent(omnicrawl_ipc::bridge::SubagentEventPayload {
            name: name.to_string(),
            payload: json!({
                "task_id": task_id,
                "batch_id": batch_id,
                "agent_type": "reviewer",
                "description": format!("审查 {task_id}"),
            }),
        })
    }

    fn conversation_event(
        name: &str,
        task_id: &str,
        batch_id: &str,
        extra: serde_json::Value,
    ) -> HostEvent {
        let mut payload = json!({
            "task_id": task_id,
            "batch_id": batch_id,
            "agent_type": "reviewer",
        });
        if let (Some(target), Some(source)) = (payload.as_object_mut(), extra.as_object()) {
            for (key, value) in source {
                target.insert(key.clone(), value.clone());
            }
        }
        HostEvent::SubagentEvent(omnicrawl_ipc::bridge::SubagentEventPayload {
            name: name.to_string(),
            payload,
        })
    }

    /// 带发起调用 id 的子任务事件（模型自己调的 `subagent` / `computer_use`）。
    fn subagent_event_with_call(
        name: &str,
        task_id: &str,
        batch_id: &str,
        call_id: &str,
    ) -> HostEvent {
        HostEvent::SubagentEvent(omnicrawl_ipc::bridge::SubagentEventPayload {
            name: name.to_string(),
            payload: json!({
                "task_id": task_id,
                "batch_id": batch_id,
                "agent_type": "explore",
                "description": format!("审查 {task_id}"),
                "origin_call_id": call_id,
            }),
        })
    }

    /// 受理提示里带着流水路径：用例跑完把该目录删掉，别在临时目录留垃圾。
    fn cleanup_state(state: &AppState) {
        let _ = std::fs::remove_dir_all(&state.subagent_stream_root);
    }

    #[test]
    fn composer_edits_by_character_not_byte() {
        let mut composer = Composer::default();
        composer.insert("你好ab");
        composer.move_left();
        composer.insert("世界");
        assert_eq!(composer.text, "你好a世界b");
        assert_eq!(composer.cursor, 5);
        // 光标停在 'b' 之前，退格删掉它左边的 '界'。
        composer.backspace();
        assert_eq!(composer.text, "你好a世b");
        composer.move_home();
        composer.delete();
        assert_eq!(composer.text, "好a世b");
        composer.move_end();
        composer.insert("\n第二行");
        assert_eq!(composer.text, "好a世b\n第二行");
    }

    /// 鼠标坐标 → 字符下标：宽字符按显示列算，行尾之后算行尾，整个文本之后算末尾。
    #[test]
    fn click_maps_to_the_character_under_the_cursor() {
        let mut composer = Composer::default();
        composer.set_text("中文abc");
        // 「中」占列 0-1、「文」占 2-3、`a` 在列 4：点某一格就落在该格所属的字符之前。
        assert_eq!(composer.char_index_at(40, 0, 0), 0);
        assert_eq!(composer.char_index_at(40, 0, 2), 1);
        assert_eq!(composer.char_index_at(40, 0, 4), 2);
        assert_eq!(composer.char_index_at(40, 0, 5), 3);
        // 行尾之后：插入点落在末尾（这一行没有换行符）。
        assert_eq!(composer.char_index_at(40, 0, 99), 5);
    }

    /// 软折行与窗口滚动下也要对得上：行号是**可见窗口内**的行号，不是全文行号。
    #[test]
    fn click_maps_across_wrapped_and_scrolled_rows() {
        let mut composer = Composer::default();
        composer.set_text("abcd");
        // 宽 2：两行 `ab` / `cd`；点第二行第 1 格 → 第 3 个字符。
        assert_eq!(composer.char_index_at(2, 0, 1), 1);
        assert_eq!(composer.char_index_at(2, 1, 0), 2);
        assert_eq!(composer.char_index_at(2, 1, 9), 4);

        // 超过 5 行时窗口跟着光标滚：`visible_rows` 给的是窗口里的行，
        // 点窗口第一行应当落到被滚上去的那一行，而不是全文第一行。
        let mut composer = Composer::default();
        composer.set_text("1\n2\n3\n4\n5\n6\n7");
        let (rows, cursor, start, total) = composer.visible_rows(40);
        assert_eq!((rows.len(), cursor, start, total), (5, 4, 2, 7));
        assert_eq!(rows[0].text, "3");
        // 窗口第一行第 0 格 → 全文里的「3」（下标 4：`1\n2\n` 各占一格）。
        assert_eq!(composer.char_index_at(40, 0, 0), 4);
        // 每行的字符区间要跳过行与行之间的换行符。
        assert_eq!((rows[0].start, rows[0].end), (4, 5));
        assert_eq!((rows[1].start, rows[1].end), (6, 7));
    }

    /// 拖选：两个端点的字符下标取自全文，反向拖也照样归一化；空选区不算选区。
    #[test]
    fn mouse_selection_reads_back_the_selected_characters() {
        let mut composer = Composer::default();
        composer.set_text("中文 abc");
        composer.begin_selection(2);
        assert_eq!(composer.selection(), None, "只按下还没拖：不是选区");
        assert_eq!(composer.selected_text(), None);

        composer.extend_selection(6);
        assert_eq!(composer.selection(), Some((2, 6)));
        assert_eq!(composer.selected_text().as_deref(), Some(" abc"));
        // 锚点在心里是 2，反向拖到 0：归一化后是 (0, 2)。
        composer.extend_selection(0);
        assert_eq!(composer.selection(), Some((0, 2)));
        assert_eq!(composer.selected_text().as_deref(), Some("中文"));

        composer.clear_selection();
        assert_eq!(composer.selection(), None);
        assert_eq!(composer.selected_text(), None);
    }

    /// 文本一改、光标一动，旧选区就作废：下标会指向别的字符，高亮与复制就会不一致。
    #[test]
    fn editing_or_moving_the_caret_drops_the_mouse_selection() {
        let mut composer = Composer::default();
        composer.set_text("abcd");
        composer.begin_selection(1);
        composer.extend_selection(3);
        composer.insert("x");
        assert_eq!(composer.selection(), None, "插入后选区作废");

        composer.begin_selection(1);
        composer.extend_selection(3);
        composer.move_left();
        assert_eq!(composer.selection(), None, "左右键后选区作废");
    }

    /// 鼠标点在占位符块中间时取近端：块在输入框里是一整块，光标不停在块里。
    #[test]
    fn click_inside_a_placeholder_snaps_to_the_nearer_edge() {
        let mut composer = Composer::default();
        composer.insert_paste(&"a\n".repeat(7));
        let placeholder = "[粘贴 #1 +8 行]";
        assert_eq!(composer.text(), placeholder, "8 行会被折成占位符");
        let length = placeholder.chars().count();
        // 块内取近端：左半边回块首，右半边到块尾。
        assert_eq!(composer.snap_to_placeholder(1), 0);
        assert_eq!(composer.snap_to_placeholder(length - 1), length);
        // 块外照旧，一点不动。
        assert_eq!(composer.snap_to_placeholder(0), 0);
        assert_eq!(composer.snap_to_placeholder(length), length);
        // 点在块首那一格：光标就落在块首，不会跑进块里。
        assert_eq!(composer.char_index_at(40, 0, 0), 0);
    }

    #[test]
    fn composer_menu_follows_text_and_answers_selection_keys() {
        let mut composer = Composer::default();
        composer.set_commands(crate::commands::command_options());
        assert!(!composer.menu().is_open(), "空输入不弹菜单");

        composer.insert("/set");
        let names: Vec<&str> = composer
            .menu()
            .matches()
            .iter()
            .map(|option| option.command.as_str())
            .collect();
        assert_eq!(names, vec!["/settings"]);

        // Tab 补全：菜单把插入文本交回调用方，自己不写输入框。
        assert_eq!(
            composer.menu_handle_key(MenuKey::Tab),
            MenuAction::Complete {
                insert: "/settings".to_string()
            }
        );
        composer.set_text("/settings");
        // 完整命令名：候选后面附上参数提示，Enter 放行提交。
        let names: Vec<&str> = composer
            .menu()
            .matches()
            .iter()
            .map(|option| option.command.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["/settings"],
            "/settings 没有声明参数，不再附参数提示"
        );
        assert_eq!(
            composer.menu_handle_key(MenuKey::Enter),
            MenuAction::Passthrough
        );

        // 输入不再是命令前缀（普通提问）：菜单立即收起。
        composer.set_text("帮我看看这个报错");
        assert!(!composer.menu().is_open());
        assert_eq!(
            composer.menu_handle_key(MenuKey::Down),
            MenuAction::Passthrough,
            "菜单收起时不能吃掉选择键"
        );
    }

    /// 真实注册表里已没有声明可选参数的命令：输入空格后不再出现参数候选。
    ///
    /// 参数候选本身的筛选与补全由 `ui::fullscreen::input::menu` 的夹具用例覆盖。
    #[test]
    fn composer_menu_has_no_parameter_candidates_for_builtin_commands() {
        let mut composer = Composer::default();
        composer.set_commands(crate::commands::command_options());
        composer.insert("/settings --");
        assert!(
            composer.menu().matches().is_empty(),
            "没有声明参数的命令不该给出参数候选：{:?}",
            composer.menu().matches()
        );
    }

    #[test]
    fn composer_wraps_by_display_width() {
        let mut composer = Composer::default();
        // 全角字符各占 2 列：宽度 4 时每行两个。
        composer.insert("中文测试");
        assert_eq!(composer.wrapped_lines(4), vec!["中文", "测试"]);
        composer.move_home();
        assert_eq!(composer.cursor_position(4), (0, 0));
        assert_eq!(composer.wrapped_lines(1), vec!["中", "文", "测", "试"]);
    }

    #[test]
    fn composer_scrolls_to_cursor_after_five_lines() {
        let mut composer = Composer::default();
        for index in 1..=7 {
            if index > 1 {
                composer.newline();
            }
            composer.insert(&format!("第{index}行"));
        }
        let (lines, cursor_row) = composer.visible_lines(40);
        assert_eq!(lines.len(), COMPOSER_MAX_LINES);
        assert_eq!(lines[cursor_row], "第7行", "光标所在行必须在窗口内");
    }

    #[test]
    fn rate_estimator_ignores_long_idle_gaps() {
        let mut rate = RateEstimator::default();
        let start = Instant::now();
        rate.record("一二三四", start);
        assert!(rate.value().is_none(), "只有一次增量时没有可用的时长");
        rate.record("五六七八", start + Duration::from_millis(500));
        let first = rate.value().expect("累计时长超过 0.2 秒后应有速度");
        assert!((first - 8.0 / 0.5).abs() < 0.01, "实际：{first}");
        // 间隔超过 2 秒视为待机：token 计入，时长不计入。
        rate.record("九十", start + Duration::from_secs(3));
        let value = rate.value().expect("仍应保留平均值");
        assert!((value - 10.0 / 0.5).abs() < 0.01, "实际：{value}");
    }

    #[test]
    fn token_estimate_counts_cjk_as_one_token() {
        assert_eq!(estimated_tokens("你好世界"), 4);
        assert_eq!(estimated_tokens("abcdefgh"), 2);
        assert_eq!(estimated_tokens("你好abcd"), 3);
    }

    /// 用户消息可以带粘贴的图片：附件交给预览后，记录只留锚点。
    #[test]
    fn begin_turn_keeps_the_pasted_images_on_the_user_message() {
        let mut state = state();
        let image = TurnImageAttachment {
            media_type: "image/png".to_string(),
            data_base64: "AAA".to_string(),
            detail: "auto".to_string(),
        };
        state.begin_turn("t1".to_string(), "看图".to_string(), vec![image]);

        let Some(Record::User(message)) = state.records.first() else {
            panic!(
                "第一条记录应当是用｛Desensitized:718｝消息：{:?}",
                state.records
            );
        };
        assert_eq!(message.text, "看图");
        assert!(message.has_images(), "有附件就该出图片块");
        assert!(!message.image_key.is_empty(), "图片块要有锚点");
        assert!(
            state
                .image_previews()
                .borrow()
                .shows_image(&message.image_key),
            "附件应当已经交给预览存储"
        );
    }

    /// 纯文本消息不带锚点：不然界面上会多铺一块空的图片区。
    #[test]
    fn a_text_only_message_has_no_image_block() {
        let mut state = state();
        state.begin_turn("t1".to_string(), "只有字".to_string(), Vec::new());
        let Some(Record::User(message)) = state.records.first() else {
            panic!("第一条记录应当是用｛Desensitized:718｝消息");
        };
        assert!(!message.has_images());
        assert!(message.image_key.is_empty());
    }
}

#[cfg(test)]
mod replay_tests {
    use super::*;
    use serde_json::{json, Value};

    fn state() -> AppState {
        let mut state = AppState::new(
            "demo".to_string(),
            "test-model".to_string(),
            ApprovalMode::Manual,
        );
        // 用例只验流水：目录指到临时区，且不给可执行文件（否则真会弹窗跑起工作台）。
        state.subagent_stream_root =
            std::env::temp_dir().join(format!("oc-state-subwin-{}", std::process::id()));
        state
    }

    fn event(event_type: &str, payload: Value) -> Value {
        json!({
            "version": 1,
            "session_id": "20260101-000000-abcdef",
            "event_id": "e1",
            "created_at": "2026-01-01T00:00:00Z",
            "type": event_type,
            "payload": payload,
        })
    }

    fn tool_cards(state: &AppState) -> Vec<&ToolCard> {
        state
            .records
            .iter()
            .filter_map(|record| match record {
                Record::Tool(card) => Some(card),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn replay_events_rebuilds_messages_and_closes_unfinished_cards() {
        let mut state = state();
        state.replay_events(&[
            event("user_message", json!({"content": "读一下 a.py"})),
            event("assistant_message", json!({"content": "我看看"})),
            event(
                "tool_call_requested",
                json!({"tool": "read", "tool_call_id": "c1", "arguments": {"path": "a.py"}}),
            ),
            event(
                "tool_result",
                json!({"tool": "read", "tool_call_id": "c1", "ok": true, "output": "   1: print(1)"}),
            ),
            // 没有结果的事件：尾部要按「未收到结果」收口，而不是留一张永远在跑的工具卡。
            event(
                "tool_call_requested",
                json!({"tool": "bash", "tool_call_id": "c2", "arguments": {"command": "ls"}}),
            ),
        ]);

        assert!(
            matches!(state.records.first(), Some(Record::User(message)) if message.text == "读一下 a.py")
        );
        assert!(matches!(state.records.get(1), Some(Record::Assistant(text)) if text == "我看看"));
        let cards = tool_cards(&state);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].name, "read");
        assert_eq!(cards[0].status, ToolStatus::Ok);
        assert_eq!(cards[0].body, "   1: print(1)");
        assert_eq!(cards[1].name, "bash");
        assert_eq!(cards[1].status, ToolStatus::Failed);
        assert_eq!(cards[1].body, "工具调用在会话结束前未收到结果。");
    }

    #[test]
    fn replay_events_marks_denied_cards_and_skips_control_tools() {
        let mut state = state();
        state.replay_events(&[
            event(
                "tool_call_requested",
                json!({"tool": "bash", "tool_call_id": "c1", "arguments": {"command": "rm -rf /"}}),
            ),
            event(
                "tool_call_denied",
                json!({"tool": "bash", "reason": "用户拒绝执行。"}),
            ),
            // 计划清单与提问是展示层/入口状态，不生成工具卡。
            event(
                "tool_call_requested",
                json!({"tool": host::TODO_TOOL, "arguments": {"todos": [{"step": "改 bug"}]}}),
            ),
            event("tool_call_requested", json!({"tool": host::ASK_USER_TOOL})),
        ]);

        let cards = tool_cards(&state);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].status, ToolStatus::Denied);
        assert_eq!(cards[0].body, "用户拒绝执行。");
        assert_eq!(state.todos.len(), 1);
    }

    #[test]
    fn replay_events_restores_subagent_trees_and_compaction_boundary() {
        let mut state = state();
        state.replay_events(&[
            event(
                "subagent_task_started",
                json!({"task_id": "t1", "batch_id": "b1", "agent_type": "review"}),
            ),
            event(
                "subagent_task_completed",
                json!({"task_id": "t1", "batch_id": "b1"}),
            ),
            event("compact_summary", json!({"content": "前情提要"})),
            event("session_interrupted", json!({})),
            event("turn_cancelled", json!({})),
        ]);

        assert!(state
            .records
            .iter()
            .any(|record| matches!(record, Record::SubagentWindow(_))));
        assert!(state
            .records
            .iter()
            .any(|record| matches!(record, Record::Assistant(text)
                if text == "会话压缩摘要：\n前情提要")));
        assert!(state
            .records
            .iter()
            .any(|record| matches!(record, Record::Notice(text)
                if text == "上一回合在会话恢复前中断。")));
        assert!(state
            .records
            .iter()
            .any(|record| matches!(record, Record::Assistant(text)
                if text == "（上一回合被取消，未生成最终回复）")));
    }

    #[test]
    fn replay_events_clears_the_view_on_an_empty_stream() {
        let mut state = state();
        state.notice("旧消息".to_string());
        // 空事件流是权威结果（`/undo` 撤掉唯一一轮）：视图必须清空，不能保留旧记录。
        state.replay_events(&[]);
        assert!(state.records.is_empty());
    }

    /// 用户报的场景：`/new` 之后输入框上方仍挂着上一条会话的计划清单。
    ///
    /// 计划清单是这一屏的会话现场，整视图重建（清空 / 回放）必须一并复位；
    /// 事件流里真有 `update_todos` 时由重放再写回来。
    #[test]
    fn replaying_the_view_resets_the_todo_plan() {
        let mut state = state();
        state.todos.push(crate::host::TodoItem {
            id: "1".to_string(),
            step: "旧计划".to_string(),
            completed: false,
        });
        let composer_before = state.refresh_versions().composer;

        state.replay_events(&[]);
        assert!(state.todos.is_empty(), "回放后不该留着旧计划");
        assert!(
            state.refresh_versions().composer > composer_before,
            "计划清单画在输入块里，清空必须一并置脏（否则高度与内容都停在旧值）"
        );

        state.todos.push(crate::host::TodoItem {
            id: "2".to_string(),
            step: "又一条旧计划".to_string(),
            completed: false,
        });
        state.replay_history(&[]);
        assert!(state.todos.is_empty(), "消息投影回放同样要复位计划");

        state.todos.push(crate::host::TodoItem {
            id: "3".to_string(),
            step: "清空视图前的计划".to_string(),
            completed: false,
        });
        state.clear_conversation();
        assert!(state.todos.is_empty(), "清空会话视图同样要复位计划");

        // 事件流里带着清单时仍要重建出来（复位不是「一律丢掉」）。
        state.replay_events(&[event(
            "tool_call_requested",
            json!({"tool": "update_todos", "tool_call_id": "c1",
                   "arguments": {"todos": [{"id": "9", "step": "写骨架", "completed": false}]}}),
        )]);
        assert_eq!(state.todos.len(), 1);
        assert_eq!(state.todos[0].step, "写骨架");
    }

    /// 旧会话（没有 `turn_compaction` 合并事件）仍按分项事件重建计量行：
    /// 逐批工具调用压缩按字符数折算、淘汰按记账的实测值各出一行。
    #[test]
    fn replay_events_shows_the_tool_compaction_notice() {
        let mut state = state();
        state.replay_events(&[
            event("user_message", json!({"content": "看一下日志"})),
            event(
                "tool_call_requested",
                json!({"tool": "bash", "tool_call_id": "c1", "arguments": {}}),
            ),
            event(
                "tool_result",
                json!({"tool": "bash", "tool_call_id": "c1", "ok": true, "output": "很长"}),
            ),
            event(
                "tool_call_summary",
                json!({
                    "content": "本轮工具调用概括：\n看了日志。",
                    "covered_event_ids": ["e1"],
                    "raw_chars": 12_345,
                    "summary_chars": 1_234,
                }),
            ),
        ]);

        assert!(
            state
                .records
                .iter()
                .any(|record| matches!(record, Record::Notice(text)
                if text == "已压缩：2,778 Token")),
            "会话区应当出现压缩计量：{:?}",
            state.records
        );
        assert!(
            matches!(state.records.last(), Some(Record::Notice(text))
                if text == "已压缩：2,778 Token"),
            "计量画在工具调用之后（压缩本来就发生在整轮调用之后）：{:?}",
            state.records
        );
    }

    /// 新会话的回合末只画一行合并计量：同回合的分项事件（逐批压缩、淘汰记账）不再各自成行。
    #[test]
    fn replay_events_merges_the_turn_compaction_notices() {
        let mut state = state();
        state.replay_events(&[
            event("user_message", json!({"content": "看一下日志"})),
            event(
                "tool_call_summary",
                json!({
                    "content": "本轮工具调用概括：\n看了日志。",
                    "covered_event_ids": ["e1"],
                    "raw_chars": 12_345,
                    "summary_chars": 1_234,
                }),
            ),
            event(
                "tool_call_eviction",
                json!({"evicted_count": 1, "saved_tokens": 8_900}),
            ),
            event("turn_compaction", json!({"saved_tokens": 11_678})),
        ]);

        let notices: Vec<&str> = state
            .records
            .iter()
            .filter_map(|record| match record {
                Record::Notice(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            notices,
            vec!["已压缩：11,678 Token"],
            "一回合只留一行合并计量：{:?}",
            state.records
        );
    }

    /// 决策模型标记淘汰的记账同样在会话区留一行计量，且回放按载荷重建同一行。
    ///
    /// 淘汰的净省量是事件里记着的实测值（不是按字符数折算），回放照原样显示。
    #[test]
    fn replay_events_shows_the_tool_eviction_notice() {
        let mut state = state();
        state.replay_events(&[
            event("user_message", json!({"content": "看一下日志"})),
            event(
                "tool_call_evicted",
                json!({"evicted_call_ids": ["c0"], "evicted_tool_names": ["grep"]}),
            ),
            event(
                "tool_call_eviction",
                json!({"evicted_count": 1, "saved_tokens": 8_900}),
            ),
        ]);

        assert!(
            matches!(state.records.last(), Some(Record::Notice(text))
                if text == "已压缩：8,900 Token"),
            "淘汰记账应当落成一行计量：{:?}",
            state.records
        );
    }

    /// 缺计量字段的旧事件不假装有计量（宁可不显示，也不显示 0 → 0）。
    #[test]
    fn replay_events_skips_the_notice_without_char_counts() {
        let mut state = state();
        state.replay_events(&[event("tool_call_summary", json!({"content": "概括"}))]);
        assert!(
            state
                .records
                .iter()
                .all(|record| !matches!(record, Record::Notice(_))),
            "没有 char 计量就不显示提示：{:?}",
            state.records
        );
    }

    /// 重放用户消息时把随消息发出的图片也装回来：否则 `/resume` 之后图片就没了。
    #[test]
    fn replay_events_restores_the_user_message_images() {
        let mut state = state();
        state.replay_events(&[event(
            "user_message",
            json!({
                "content": "看一下这张图",
                "images": [
                    {"media_type": "image/png", "data_base64": "AAA", "detail": "auto"},
                ],
            }),
        )]);

        let Some(Record::User(message)) = state.records.first() else {
            panic!(
                "第一条记录应当是用｛Desensitized:719｝消息：{:?}",
                state.records
            );
        };
        assert_eq!(message.text, "看一下这张图");
        assert!(message.has_images(), "重放的图片也要出图片块");
        assert!(
            state
                .image_previews()
                .borrow()
                .shows_image(&message.image_key),
            "重放的附件应当交给预览存储，而不是留在记录里"
        );
    }

    /// 旧事件没有 `images` 字段：照旧是纯文本消息，不能凭空多出一块图片区。
    #[test]
    fn replay_events_without_images_stays_text_only() {
        let mut state = state();
        state.replay_events(&[event("user_message", json!({"content": "纯文本"}))]);
        let Some(Record::User(message)) = state.records.first() else {
            panic!("第一条记录应当是用｛Desensitized:719｝消息");
        };
        assert!(!message.has_images());
        assert!(message.image_key.is_empty());
    }
}

#[cfg(test)]
mod notice_line_tests {
    use super::{AppState, NOTICE_LINE_LINGER};
    use crate::args::ApprovalMode;
    use std::time::{Duration, Instant};

    #[test]
    fn notice_line_shows_then_expires() {
        let mut state = AppState::new("项目".to_string(), "模型".to_string(), ApprovalMode::Manual);
        let now = Instant::now();
        assert!(state.notice_line_text(now).is_none(), "默认没有提示行");

        state.show_notice_line("已复制 2 行到剪切板。", now);
        assert_eq!(state.notice_line_text(now), Some("已复制 2 行到剪切板。"));

        let later = now + NOTICE_LINE_LINGER + Duration::from_millis(10);
        assert!(state.notice_line_text(later).is_none(), "超时后不再显示");
        state.tick_notice_line(later);
        assert!(state.notice_line.is_none(), "tick 要把过期提示真的回收掉");
    }
}

/// 粘贴折叠：多行粘贴折成 `[粘贴 #n +N 行]`，提交时还原，删除时整块删
/// （对映 Python `editing.py` 的 `_compact_paste_if_needed` / `_expand_compact_paste_placeholders`）。
#[cfg(test)]
mod paste_tests {
    use super::*;

    fn pasted(lines: usize) -> String {
        (1..=lines)
            .map(|index| format!("第 {index} 行"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn multiline_paste_is_folded_and_expands_on_take() {
        let original = pasted(8);
        let mut composer = Composer::default();
        composer.insert_paste(&original);

        assert_eq!(composer.text(), "[粘贴 #1 +8 行]", "多行粘贴要折成占位符");
        assert_eq!(
            composer.expanded_text(),
            original,
            "展开后要拿回原文（提交路径读的就是它）"
        );
        assert_eq!(composer.take(), original, "take 必须还原原文");
        assert!(composer.is_empty(), "take 之后输入框要清空");
    }

    #[test]
    fn short_paste_stays_verbatim() {
        let mut composer = Composer::default();
        composer.insert_paste("第一行\n第二行\n第三行");
        assert_eq!(
            composer.text(),
            "第一行\n第二行\n第三行",
            "5 行及以内不折叠（阈值对映 Python）"
        );
    }

    #[test]
    fn crlf_paste_is_normalized_and_counted_like_python() {
        let mut composer = Composer::default();
        composer.insert_paste("a\r\nb\r\nc\r\nd\r\ne\r\nf");
        // 6 行 → 折叠；CRLF 已归一化，因此展开后是 LF。
        assert_eq!(composer.text(), "[粘贴 #1 +6 行]");
        assert_eq!(composer.take(), "a\nb\nc\nd\ne\nf");
    }

    #[test]
    fn backspace_and_delete_remove_the_whole_block() {
        let mut composer = Composer::default();
        composer.insert_paste(&pasted(9));
        composer.backspace();
        assert!(composer.is_empty(), "退格要整块删，而不是逐字符删");

        composer.insert_paste(&pasted(9));
        composer.move_home();
        composer.delete();
        assert!(composer.is_empty(), "块首的 Delete 也要整块删");
    }

    #[test]
    fn arrows_treat_the_block_as_a_single_cell() {
        let mut composer = Composer::default();
        composer.insert_paste(&pasted(9));
        composer.move_left();
        // 已在块首：再退格不会删到块里的字符（否则会变成残缺的占位符文本）。
        composer.backspace();
        assert_eq!(composer.text(), "[粘贴 #1 +9 行]");
        composer.move_right();
        composer.insert("尾巴");
        assert_eq!(composer.take(), format!("{}尾巴", pasted(9)));
    }

    #[test]
    fn typing_then_submitting_keeps_the_order() {
        let mut composer = Composer::default();
        composer.insert_paste(&pasted(7));
        composer.insert("（请按这个格式）");
        assert_eq!(composer.take(), format!("{}（请按这个格式）", pasted(7)));
    }
}

/// 粘贴图片：`Ctrl+V` 贴进来的位图在｛Desensitized:712｝以 `[ #n Image ]` 占位，
/// 与文本粘贴折叠同一套占位符语义（对映用户要求）。
#[cfg(test)]
mod image_paste_tests {
    use super::*;

    fn png() -> Vec<u8> {
        vec![0x89, b'P', b'N', b'G', 1, 2, 3]
    }

    #[test]
    fn pasted_images_get_numbered_placeholders() {
        let mut composer = Composer::default();
        composer.insert_image("image/png", png());
        composer.insert("看一下");
        composer.insert_image("image/png", png());
        assert_eq!(
            composer.text(),
            "[ #1 Image ]看一下[ #2 Image ]",
            "序号按粘贴顺序递增，占位符插在光标处"
        );
        assert_eq!(composer.image_count(), 2);
    }

    #[test]
    fn backspace_removes_the_whole_image_block() {
        let mut composer = Composer::default();
        composer.insert("前");
        composer.insert_image("image/png", png());
        composer.backspace();
        assert_eq!(composer.text(), "前", "退格要整块删掉占位符");
        assert_eq!(composer.image_count(), 0, "占位符没了附件也要跟着丢");
    }

    #[test]
    fn arrows_treat_the_image_block_as_a_single_cell() {
        let mut composer = Composer::default();
        composer.insert_image("image/png", png());
        composer.insert("尾巴");
        composer.move_home();
        composer.move_right();
        // 右移一次跨过整个块，不会停在块中间。
        composer.insert("这里");
        assert_eq!(composer.text(), "[ #1 Image ]这里尾巴");
    }

    #[test]
    fn take_images_hands_out_attachments_in_order() {
        let mut composer = Composer::default();
        composer.insert_image("image/png", png());
        composer.insert_image("image/png", png());
        let images = composer.take_images();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].placeholder, "[ #1 Image ]", "顺序与序号一致");
        assert_eq!(images[1].placeholder, "[ #2 Image ]");
        assert_eq!(composer.image_count(), 0, "取走后不残留");
    }

    #[test]
    fn take_clears_images_without_dropping_the_placeholder_text() {
        let mut composer = Composer::default();
        composer.insert_image("image/png", png());
        // 文本仍带占位符（提交时随 prompt 一起送），附件由 `take_images` 单独取。
        assert_eq!(composer.take(), "[ #1 Image ]");
        assert_eq!(composer.image_count(), 0, "take 也要清掉图片状态，避免残留");
    }

    #[test]
    fn clearing_the_composer_drops_pending_images() {
        let mut composer = Composer::default();
        composer.insert_image("image/png", png());
        composer.clear();
        assert!(composer.is_empty());
        assert_eq!(composer.image_count(), 0);
    }
}

/// 已发送消息的上下键回看（对映 Python `input/composer.py` 的历史浏览部分）。
#[cfg(test)]
mod history_tests {
    use super::*;

    fn composer_with(entries: &[&str]) -> Composer {
        let mut composer = Composer::default();
        for entry in entries {
            composer.history_record(entry);
        }
        composer
    }

    #[test]
    fn without_history_up_key_is_not_consumed() {
        let mut composer = Composer::default();
        assert!(!composer.navigate_history(-1), "没有历史要让位给会话滚动");
        assert!(!composer.navigate_history(1));
    }

    #[test]
    fn down_key_without_browsing_is_not_consumed() {
        let mut composer = composer_with(&["第一条", "第二条"]);
        assert!(
            !composer.navigate_history(1),
            "未在浏览时下键不该被消费（否则会话滚不下去）"
        );
    }

    #[test]
    fn repeated_submissions_are_recorded_once() {
        let mut composer = Composer::default();
        composer.history_record("同一句");
        composer.history_record("同一句");
        composer.history_record("");
        assert_eq!(composer.history.len(), 1, "重复与空提交不入列");
    }

    /// 历史必须封顶：无上限时任何一处反复提交（死循环、重试抖动）都会把内存顶到分配失败。
    #[test]
    fn history_is_capped_and_keeps_the_newest_entries() {
        let mut composer = Composer::default();
        for index in 0..COMPOSER_HISTORY_LIMIT + 50 {
            composer.history_record(&format!("第 {index} 条"));
        }
        assert_eq!(composer.history.len(), COMPOSER_HISTORY_LIMIT);
        let newest = format!("第 {} 条", COMPOSER_HISTORY_LIMIT + 49);
        assert_eq!(
            composer.history.last().map(String::as_str),
            Some(newest.as_str())
        );
        // 最早那条已被挤掉，上键翻到的第一条就是窗口内的第一条。
        assert!(composer.navigate_history(-1));
        assert_eq!(
            composer.history.first().map(String::as_str),
            Some("第 50 条")
        );
    }

    #[test]
    fn up_key_walks_backwards_and_down_key_restores_the_draft() {
        let mut composer = composer_with(&["第一条", "第二条"]);
        composer.insert("还没发出去的草稿");

        // 上键：从草稿进入最新一条。
        assert!(composer.navigate_history(-1));
        assert_eq!(composer.text(), "第二条");
        // 全角按显示宽度计列：三个全角字占 6 列。
        assert_eq!(composer.cursor_position(40), (0, 6), "光标要落在末尾");

        // 再上：向更早翻；到头后停在最早一条。
        assert!(composer.navigate_history(-1));
        assert_eq!(composer.text(), "第一条");
        assert!(composer.navigate_history(-1));
        assert_eq!(composer.text(), "第一条", "到头不循环");

        // 下键逐条回来，翻过最新一条后恢复草稿并退出浏览态。
        assert!(composer.navigate_history(1));
        assert_eq!(composer.text(), "第二条");
        assert!(composer.is_browsing_history());
        assert!(composer.navigate_history(1));
        assert_eq!(composer.text(), "还没发出去的草稿", "草稿要能切回来");
        assert!(!composer.is_browsing_history());
    }

    #[test]
    fn submit_exits_browse_so_next_up_starts_from_the_latest() {
        let mut composer = composer_with(&["第一条", "第二条"]);
        composer.navigate_history(-1);
        composer.navigate_history(-1);
        assert_eq!(composer.text(), "第一条");

        composer.history_record("第三条");
        assert!(!composer.is_browsing_history(), "提交即退出浏览态");
        assert!(composer.navigate_history(-1));
        assert_eq!(composer.text(), "第三条", "下一条上键从最新一条开始");
    }

    #[test]
    fn manual_edit_ends_browsing_and_keeps_the_edited_text() {
        let mut composer = composer_with(&["第一条", "第二条"]);
        composer.navigate_history(-1);
        composer.insert("补充");

        assert!(!composer.is_browsing_history(), "手动编辑结束浏览态");
        assert_eq!(composer.text(), "第二条补充");
        // 再上键从最新一条重新开始（草稿是编辑后的内容）。
        assert!(composer.navigate_history(-1));
        assert_eq!(composer.text(), "第二条");
        assert!(composer.navigate_history(1));
        assert_eq!(composer.text(), "第二条补充");
    }

    #[test]
    fn browsing_does_not_leave_the_command_menu_open() {
        let mut composer = Composer::default();
        composer.set_commands(crate::commands::command_options());
        composer.history_record("/settings");
        composer.insert("/ne");
        assert!(composer.menu().is_open(), "正常输入时菜单照常弹出");

        // 回看以 `/` 开头的历史时菜单必须收起，否则接下来的上下键会被菜单抢走。
        assert!(composer.navigate_history(-1));
        assert_eq!(composer.text(), "/settings");
        assert!(!composer.menu().is_open(), "浏览历史不该弹出候选菜单");

        // 手动编辑才让菜单回来：退回到 `/se` 前缀，候选立即重新出现。
        while composer.text() != "/se" {
            composer.backspace();
        }
        assert!(composer.menu().is_open(), "手动编辑后菜单要恢复刷新");
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use omnicrawl_ipc::bridge::{
        ToolCallArgumentsPayload, ToolCallStartedPayload, ToolOutputCompressionPayload,
    };
    use serde_json::json;

    #[test]
    fn partial_arguments_reads_what_has_arrived() {
        // 完整 JSON 直接解析。
        let full = partial_arguments(r#"{"path": "a.py", "line": 3}"#);
        assert_eq!(full["path"], json!("a.py"));
        assert_eq!(full["line"], json!(3));
        // 半截：字符串还没闭合、对象也没闭合——已到达的那一段要能取出来。
        let half = partial_arguments(HALF_JSON);
        assert_eq!(half["path"], json!("a.py"));
        assert_eq!(
            half["content"],
            json!(
                "第一
第二"
            )
        );
        // 连值都没开始的半截：只拿到前面的字段。
        let broken = partial_arguments(r#"{"path": "a.py", "cont"#);
        assert_eq!(broken["path"], json!("a.py"));
        // 完全不是 JSON：给空对象（渲染退回卡片自己的参数）。
        assert_eq!(
            partial_arguments("not json"),
            serde_json::Value::Object(serde_json::Map::new())
        );
    }

    #[test]
    fn streaming_card_grows_with_argument_deltas() {
        let mut state = AppState::new(
            "proj".to_string(),
            "model".to_string(),
            ApprovalMode::Manual,
        );
        let now = Instant::now();
        state.apply(
            &omnicrawl_ipc::HostEvent::ToolCallStarted(ToolCallStartedPayload {
                call_id: "call-1".to_string(),
                tool: "bash".to_string(),
            }),
            now,
        );
        for delta in [r#"{"command": "#, r#""ls -la""#, "}"] {
            state.apply(
                &omnicrawl_ipc::HostEvent::ToolCallArguments(ToolCallArgumentsPayload {
                    call_id: "call-1".to_string(),
                    delta: delta.to_string(),
                }),
                now,
            );
        }
        let entry = state.streaming_tool("call-1").expect("应当有侧信道状态");
        assert!(entry.streaming, "批次执行前仍处于流式阶段");
        assert_eq!(entry.arguments["command"], json!("ls -la"));
        assert_eq!(entry.text, r#"{"command": "ls -la"}"#);
        // 卡片在流式阶段就已经立起来了（标题走的是容错解析出来的参数）。
        let lines = crate::ui::conversation::display_lines(&state, 80);
        let title = lines
            .iter()
            .map(|line| {
                line.line
                    .spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect::<String>()
            })
            .find(|text| text.contains("bash"))
            .expect("流式阶段应当已经有工具卡");
        assert!(title.contains("ls -la"), "标题应带上已到达的命令：{title}");
    }

    #[test]
    fn compression_note_tracks_the_two_phases() {
        let mut state = AppState::new(
            "proj".to_string(),
            "model".to_string(),
            ApprovalMode::Manual,
        );
        let now = Instant::now();
        state.apply(
            &omnicrawl_ipc::HostEvent::ToolCallStarted(ToolCallStartedPayload {
                call_id: "call-1".to_string(),
                tool: "bash".to_string(),
            }),
            now,
        );
        let phase = |phase: &str, before: usize, after: usize| {
            omnicrawl_ipc::HostEvent::ToolOutputCompression(ToolOutputCompressionPayload {
                call_id: "call-1".to_string(),
                tool: "bash".to_string(),
                phase: phase.to_string(),
                before_chars: before,
                after_chars: after,
                output: if phase == "finished" {
                    "压缩后的正文".to_string()
                } else {
                    String::new()
                },
                error: if phase == "failed" {
                    "压缩超时（120 秒），保留原始输出：请求超时。".to_string()
                } else {
                    String::new()
                },
            })
        };
        state.apply(&phase("started", 12_345, 0), now);
        assert_eq!(
            state
                .streaming_tool("call-1")
                .unwrap()
                .compression
                .as_deref(),
            Some("正在压缩…")
        );
        state.apply(&phase("finished", 12_345, 1_234), now);
        assert_eq!(
            state
                .streaming_tool("call-1")
                .unwrap()
                .compression
                .as_deref(),
            None,
            "压缩完成后不在卡片上留计量行（省下的量由回合末的合并提示统一回答）"
        );
        // 压缩完成后用**压缩后的正文替换**卡片里的原始输出（用户要求）。
        let card = match state
            .records
            .iter()
            .find(|record| matches!(record, Record::Tool(_)))
        {
            Some(Record::Tool(card)) => card.clone(),
            other => panic!("应当有工具卡：{other:?}"),
        };
        assert_eq!(card.body, "压缩后的正文");
    }

    #[test]
    fn internal_calls_are_settled_after_the_question_is_answered() {
        // 回归（用户报）：ask_user 由批次推进在宿主内直接定调结果，从不经过执行层，
        // 卡片因此一直停在「调用中」并继续计时。update_todos 同理。
        let mut state = AppState::new(
            "proj".to_string(),
            "model".to_string(),
            ApprovalMode::Manual,
        );
        let call = |name: &str, arguments: serde_json::Value| omnicrawl_core::ToolCall {
            name: name.to_string(),
            arguments: arguments.as_object().cloned().unwrap_or_default(),
            id: format!("call-{name}"),
            function_name: name.to_string(),
        };
        let todos = call(
            "update_todos",
            json!({"todos": [{"step": "写骨架", "completed": false}]}),
        );
        let ask = call(
            "ask_user",
            json!({"question": "要不要继续？", "options": ["继续", "停下"]}),
        );
        let _ = state.start_batch(
            omnicrawl_ipc::Id::Number(1),
            vec![todos.clone(), ask.clone()],
        );
        // 推进到提问点：任务清单已在宿主内定调，提问还在等答案。
        state.settle_internal_batch_calls();
        assert_eq!(
            state.tool_card(&todos.id).map(|card| card.status),
            Some(ToolStatus::Ok),
            "update_todos 是内部定调的，应当立刻收口"
        );
        assert!(
            state.tool_card(&ask.id).is_none(),
            "提问在回答前不该有终态卡片"
        );

        state
            .answer_question("继续".to_string())
            .expect("应当在等提问");
        assert_eq!(
            state.settle_internal_batch_calls(),
            1,
            "回答之后提问卡片要收口"
        );
        let card = state.tool_card(&ask.id).expect("提问应当有卡片");
        assert_eq!(
            card.status,
            ToolStatus::Ok,
            "提问结束后不能再停在「调用中」"
        );
        assert!(
            card.body.lines().any(|line| line.contains("继续")),
            "卡片正文带回答：{:?}",
            card.body
        );
    }

    #[test]
    fn compression_failure_note_is_shown_on_the_card() {
        // 用户要求：压缩超时显示「压缩超时」，其他错误显示错误（以前只 eprintln，界面看不到）。
        let mut state = AppState::new(
            "proj".to_string(),
            "model".to_string(),
            ApprovalMode::Manual,
        );
        let now = Instant::now();
        state.apply(
            &omnicrawl_ipc::HostEvent::ToolCallStarted(ToolCallStartedPayload {
                call_id: "call-1".to_string(),
                tool: "bash".to_string(),
            }),
            now,
        );
        state.apply(
            &omnicrawl_ipc::HostEvent::ToolOutputCompression(ToolOutputCompressionPayload {
                call_id: "call-1".to_string(),
                tool: "bash".to_string(),
                phase: "failed".to_string(),
                before_chars: 100_000,
                after_chars: 0,
                output: String::new(),
                error: "压缩超时（120 秒），保留原始输出：请求超时。".to_string(),
            }),
            now,
        );
        let entry = state.streaming_tool("call-1").expect("应当有侧信道条目");
        assert!(
            entry
                .compression
                .as_deref()
                .unwrap_or_default()
                .contains("压缩超时"),
            "卡片上要显示压缩超时：{:?}",
            entry.compression
        );
        assert!(entry.compression_failed, "失败提示要走错误样式");
    }

    #[test]
    fn a_streamed_call_keeps_one_card_and_gets_finished() {
        // 回归：流式阶段立过卡片后，批次执行**不能**再推一张卡片，
        // 否则流式那张永远停在「调用中」（用户报「工具调用一直不结束」）。
        let mut state = AppState::new(
            "proj".to_string(),
            "model".to_string(),
            ApprovalMode::Manual,
        );
        let now = Instant::now();
        state.apply(
            &omnicrawl_ipc::HostEvent::ToolCallStarted(ToolCallStartedPayload {
                call_id: "call-1".to_string(),
                tool: "bash".to_string(),
            }),
            now,
        );
        let call = omnicrawl_core::ToolCall {
            name: "bash".to_string(),
            arguments: json!({"command": "ls"})
                .as_object()
                .cloned()
                .unwrap_or_default(),
            id: "call-1".to_string(),
            function_name: "bash".to_string(),
        };
        state.begin_tool_run(&call, now);
        state.begin_tool_run(&call, now);
        let cards: Vec<&ToolCard> = state
            .records
            .iter()
            .filter_map(|record| match record {
                Record::Tool(card) if card.call_id == "call-1" => Some(card),
                _ => None,
            })
            .collect();
        assert_eq!(cards.len(), 1, "同一 call_id 只应有一张卡片");
        assert!(
            !state.streaming_tool("call-1").unwrap().streaming,
            "批次执行后不再是流式态"
        );

        // 结果回填后卡片进入终态（不会一直是「调用中」）。
        state.finish_tool_run(
            &call,
            &omnicrawl_core::ToolResult {
                ok: true,
                output: "文件内容：42".to_string(),
                full_output: String::new(),
                error_code: None,
                retryable: false,
            },
            now,
        );
        let card = match state
            .records
            .iter()
            .find(|record| matches!(record, Record::Tool(_)))
        {
            Some(Record::Tool(card)) => card,
            other => panic!("应当有工具卡：{other:?}"),
        };
        assert_eq!(card.status, ToolStatus::Ok, "完成后状态必须是终态");
    }

    #[test]
    fn turn_end_closes_cards_still_marked_running() {
        // 回合结束仍停在「调用中」的卡片必须收口（用户报「工具调用一直不结束」）。
        let mut state = AppState::new(
            "proj".to_string(),
            "model".to_string(),
            ApprovalMode::Manual,
        );
        let now = Instant::now();
        state.apply(
            &omnicrawl_ipc::HostEvent::ToolCallStarted(ToolCallStartedPayload {
                call_id: "call-1".to_string(),
                tool: "bash".to_string(),
            }),
            now,
        );
        state.apply(
            &omnicrawl_ipc::HostEvent::TurnFinished(omnicrawl_ipc::bridge::TurnFinishedPayload {
                turn_id: "t1".to_string(),
                final_text: String::new(),
                reasoning: String::new(),
                model_turns: 1,
                tool_calls: 1,
                paused: false,
                post_compaction_context_tokens: None,
            }),
            now,
        );
        let card = match state
            .records
            .iter()
            .find(|record| matches!(record, Record::Tool(_)))
        {
            Some(Record::Tool(card)) => card,
            other => panic!("应当有工具卡：{other:?}"),
        };
        assert_eq!(card.status, ToolStatus::Failed, "回合结束不能再停在调用中");
        assert_eq!(card.body, "工具调用在回合结束前未收到结果。");
    }

    #[test]
    fn batch_waits_for_every_call_before_observations() {
        // 并发工具调用：整批**全部**回填后才打包成观察交给下一轮模型请求。
        let mut state = AppState::new(
            "proj".to_string(),
            "model".to_string(),
            ApprovalMode::Manual,
        );
        let calls: Vec<omnicrawl_core::ToolCall> = ["call-1", "call-2"]
            .iter()
            .map(|id| omnicrawl_core::ToolCall {
                name: "read_file".to_string(),
                arguments: json!({"path": "a.py"})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                id: (*id).to_string(),
                function_name: "read_file".to_string(),
            })
            .collect();
        let _ = state.start_batch(omnicrawl_ipc::Id::Number(1), calls.clone());
        let result = omnicrawl_core::ToolResult {
            ok: true,
            output: "文件内容：42".to_string(),
            full_output: String::new(),
            error_code: None,
            retryable: false,
        };
        assert!(
            !state.record_tool_result(0, result.clone(), None),
            "只回填一条时整批未就绪"
        );
        assert!(
            state.take_observations(false).is_none(),
            "未整批回填时不能打包观察"
        );
        assert!(
            state.record_tool_result(1, result, None),
            "两条都回填后整批就绪"
        );
        let observations = state
            .take_observations(false)
            .expect("整批就绪后应当能取观察");
        assert_eq!(observations.len(), 2, "观察按模型调用顺序整批产出");
        let _ = &calls;
    }

    /// 半截 JSON：字符串与对象都没闭合（模型流中断在这一刻的样子）。
    const HALF_JSON: &str = r#"{"path": "a.py", "content": "第一\n第二"#;

    // ---- 三块刷新（会话 / 输入框 / 底部） ------------------------------------------

    fn three_block_state() -> AppState {
        let mut state = AppState::new(
            "proj".to_string(),
            "model".to_string(),
            ApprovalMode::Manual,
        );
        state.begin_turn("t1".to_string(), "问题".to_string(), Vec::new());
        state
    }

    /// 只读操作（取行、取窗口、算滚动条）不能把任何一块置脏。
    ///
    /// 事件循环就是靠「三块都没变」来跳过整帧绘制；空转刷新一旦置脏，
    /// 2000 行的会话又会回到每帧重画。
    #[test]
    fn idle_reads_never_mark_any_block_dirty() {
        let state = three_block_state();
        let before = state.refresh_versions();
        for _ in 0..10 {
            let _ = crate::ui::conversation::display_lines(&state, 80);
            let _ = crate::ui::conversation::visible_window(&state, 80, 20, 0);
            let _ = &state.hud_text;
        }
        assert_eq!(state.refresh_versions(), before, "只读跟踪不得置脏");
    }

    /// 三块各管各的：改会话不会带脏底部，底部遥测刷新不会把会话整块失效。
    #[test]
    fn the_three_blocks_are_versioned_independently() {
        let mut state = three_block_state();
        let _ = crate::ui::conversation::display_lines(&state, 80);
        let base = state.refresh_versions();
        let rebuilds = state.conversation_cache().borrow().rebuilds;

        // 底部遥测刷新：只有底部版本动，会话的显示行缓存连一行都不重算。
        state.touch_bottom();
        let after_bottom = state.refresh_versions();
        assert_eq!(after_bottom.bottom, base.bottom + 1);
        assert_eq!(after_bottom.conversation, base.conversation);
        assert_eq!(after_bottom.composer, base.composer);

        // 会话区滚动：只换窗口，不是内容变化。
        state.scroll_by(-4);
        let after_scroll = state.refresh_versions();
        assert_eq!(after_scroll.conversation, after_bottom.conversation + 1);
        assert_eq!(after_scroll.bottom, after_bottom.bottom);
        let _ = crate::ui::conversation::display_lines(&state, 80);
        assert_eq!(
            state.conversation_cache().borrow().rebuilds,
            rebuilds,
            "滚动不得触发显示行重算"
        );

        // 拖选同理：只加高亮。
        state.begin_selection(1, 0);
        state.extend_selection(2, 3);
        state.clear_selection();
        let after_selection = state.refresh_versions();
        assert_eq!(after_selection.conversation, after_scroll.conversation + 3);
        assert_eq!(after_selection.composer, after_scroll.composer);

        // 新记录：会话与输入框都变（输入框上是排队/状态信息），底部不变。
        state.notice("提示".to_string());
        let after_notice = state.refresh_versions();
        assert_eq!(after_notice.conversation, after_selection.conversation + 1);
        assert_eq!(after_notice.bottom, after_selection.bottom);
    }

    /// 空闲时 `tick_activity` 什么也不做；回合在跑时它维持 spinner 与活动耗时。
    #[test]
    fn activity_tick_only_runs_while_a_turn_is_in_flight() {
        let mut state = AppState::new(
            "proj".to_string(),
            "model".to_string(),
            ApprovalMode::Manual,
        );
        // 空闲：一条版本都不动。
        let idle = state.refresh_versions();
        state.tick_activity();
        assert_eq!(state.refresh_versions(), idle, "空闲时不该有活动刷新");

        // 回合在跑：状态行 spinner 与运行中卡片耗时都要重画。
        state.begin_turn("t1".to_string(), "问题".to_string(), Vec::new());
        let running = state.refresh_versions();
        state.tick_activity();
        let ticked = state.refresh_versions();
        assert_eq!(
            ticked.composer,
            running.composer + 1,
            "spinner 在输入框上边框"
        );
        assert_eq!(
            ticked.conversation, running.conversation,
            "没有运行中的卡片就不动会话"
        );

        // 有一张运行中的卡片：它的耗时按帧重算。
        let mut state = three_block_state();
        let _ = crate::ui::conversation::display_lines(&state, 80);
        let rebuilds = state.conversation_cache().borrow().rebuilds;
        let call = omnicrawl_core::ToolCall {
            name: "bash".to_string(),
            arguments: serde_json::json!({"command": "pytest -q"})
                .as_object()
                .cloned()
                .unwrap_or_default(),
            id: "c1".to_string(),
            function_name: "bash".to_string(),
        };
        state.begin_tool_run(&call, Instant::now());
        state.tick_activity();
        let _ = crate::ui::conversation::display_lines(&state, 80);
        assert!(
            state.conversation_cache().borrow().rebuilds > rebuilds,
            "运行中卡片的耗时应当被重新算过"
        );
    }
}

