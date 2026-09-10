"""将持久化会话事件投影为模型上下文消息。"""

from __future__ import annotations

import json
from typing import Any, Mapping, Sequence

from .session_models import (
    COMPACT_SUMMARY_PREFIX,
    SessionEvent,
    clean_title,
    read_payload_non_negative_int,
)


TOOL_CALL_CONTEXT_PREFIX = "工具调用请求："
TOOL_RESULT_CONTEXT_PREFIX = "工具执行结果："
TURN_UNDONE_EVENT_TYPE = "turn_undone"
# 只参与投影、不落盘的临时事件 ID 前缀；用短横线不用冒号，避免被误认为
# 真实事件 ID 的命名空间（真实 ID 由 SessionStore 生成）。
PROJECTION_ONLY_EVENT_ID_PREFIX = "projection-only-"
RUN_GUARD_TODO_TOOL_NAME = "update_todos"
_RUN_GUARD_PENDING_EVENTS = frozenset(
    {
        "run_guard_continue",
        "run_guard_continue_exhausted",
        "run_guard_paused",
        "turn_cancelled",
        "session_interrupted",
    }
)


def active_session_events(events: list[SessionEvent]) -> list[SessionEvent]:
    """投影回退后的有效事件，保留 JSONL 的仅追加审计特性。

    `turn_undone` 只记录被回退轮次的事件 ID。读取时统一过滤这些事件，
    使模型恢复、会话列表和客户端转录共享同一套逻辑视图。
    """

    hidden_event_ids: set[str] = set()
    for event in events:
        if event.type != TURN_UNDONE_EVENT_TYPE:
            continue
        event_ids = event.payload.get("event_ids", [])
        if not isinstance(event_ids, list):
            continue
        hidden_event_ids.update(
            event_id.strip()
            for event_id in event_ids
            if isinstance(event_id, str) and event_id.strip()
        )
    return [
        event
        for event in events
        if event.type != TURN_UNDONE_EVENT_TYPE and event.event_id not in hidden_event_ids
    ]


def session_title_from_events(
    events: list[SessionEvent],
    *,
    fallback: str = "新会话",
) -> str:
    """按现有自动标题与显式重命名规则投影当前会话标题。"""

    title = fallback
    first_user_title_applied = False
    for event in events:
        if event.type == "session_started":
            started_title = event.payload.get("title", "")
            if isinstance(started_title, str) and started_title.strip():
                title = clean_title(started_title) or title
        if not first_user_title_applied and event.type == "user_message":
            content = event.payload.get("content", "")
            if isinstance(content, str) and content.strip():
                title = clean_title(content)
                first_user_title_applied = True
        elif event.type == "session_renamed":
            renamed_title = event.payload.get("title", "")
            if isinstance(renamed_title, str) and renamed_title.strip():
                title = clean_title(renamed_title)
    return title


CANCELLED_TURN_DEFAULT_SUMMARY = "（上一回合被取消，未生成最终回复）"


def recover_run_guard_state(
    events: list[SessionEvent],
) -> tuple[str, tuple[dict[str, Any], ...]]:
    """从有效事件流恢复运行护栏需要的 pending 任务和最后一份 Todo。

    运行中的临时字段不写入 ``index.json``，而是随关键事件做 last-write-wins
    投影。这样崩溃发生在 ``pause_work``、自动续跑或 Provider 中断之后时，
    新进程仍能知道“继续”应恢复哪个任务；普通 assistant_message 没有待续
    字段时会清除 pending，避免把已经完成的任务误认为未完成。
    """

    pending_user_text = ""
    todo_items: tuple[dict[str, Any], ...] = ()
    for event in events:
        pending_user_text, todo_items = apply_run_guard_event(
            pending_user_text,
            todo_items,
            event.type,
            event.payload if isinstance(event.payload, dict) else {},
        )
    return pending_user_text, todo_items


def apply_run_guard_event(
    pending_user_text: str,
    todo_items: tuple[dict[str, Any], ...],
    event_type: str,
    payload: dict[str, Any],
) -> tuple[str, tuple[dict[str, Any], ...]]:
    """把一条新事件应用到运行护栏恢复投影。

    该增量版本与 ``recover_run_guard_state`` 共用同一规则，供 Agent 在当前
    进程追加事件后立即更新 ``SessionState``，避免每个工具事件都重新扫描整份
    JSONL。只有显式保存的 pending 字段/用户任务会改变 pending；助手最终回复
    的 ``content`` 绝不能被误当成待续任务。
    """

    if event_type == "user_message":
        pending = _pending_text_from_payload(payload, include_content=True)
        raw_todos = payload.get("todo_items")
        return pending, _normalize_todo_items(raw_todos) if isinstance(raw_todos, list) else ()

    if event_type == "tool_call_requested":
        if str(payload.get("tool") or "").strip() == RUN_GUARD_TODO_TOOL_NAME:
            arguments = payload.get("arguments")
            if isinstance(arguments, dict):
                return pending_user_text, _normalize_todo_items(arguments.get("todos"))
        return pending_user_text, todo_items

    if event_type == "tool_result":
        if str(payload.get("tool") or "").strip() == RUN_GUARD_TODO_TOOL_NAME:
            recovered = _todo_items_from_tool_result(payload)
            if recovered is not None:
                return pending_user_text, recovered
        return pending_user_text, todo_items

    if event_type in _RUN_GUARD_PENDING_EVENTS:
        candidate = _pending_text_from_payload(payload)
        next_todos = todo_items
        if isinstance(payload.get("todo_items"), list):
            next_todos = _normalize_todo_items(payload.get("todo_items"))
        return candidate or pending_user_text, next_todos

    if event_type == "assistant_message":
        # 只有未来显式提供 pending_user_text 时才保留待续状态；当前正常
        # assistant_message 的 content 是回复正文，不是任务来源。
        candidate = _pending_text_from_payload(payload, include_content=False)
        return candidate, todo_items

    return pending_user_text, todo_items


def _pending_text_from_payload(
    payload: dict[str, Any],
    *,
    include_content: bool = False,
) -> str:
    keys = ("pending_user_text", "user_text", "content") if include_content else (
        "pending_user_text",
        "user_text",
    )
    for key in keys:
        value = payload.get(key)
        if isinstance(value, str) and value.strip():
            return value.strip()
    return ""


def _normalize_todo_items(raw_todos: Any) -> tuple[dict[str, Any], ...]:
    if not isinstance(raw_todos, list):
        return ()
    normalized: list[dict[str, Any]] = []
    for index, raw_item in enumerate(raw_todos[:20], start=1):
        if not isinstance(raw_item, dict):
            continue
        step = str(
            raw_item.get("step")
            or raw_item.get("description")
            or raw_item.get("title")
            or ""
        ).strip()
        if not step:
            continue
        status = str(raw_item.get("status") or "").strip().casefold()
        completed = bool(raw_item.get("completed")) or status in {
            "completed",
            "done",
            "complete",
        }
        item_id = str(raw_item.get("id") or index).strip()[:80] or str(index)
        normalized.append(
            {
                "id": item_id,
                "step": step[:240],
                "completed": completed,
            }
        )
    return tuple(normalized)


def _todo_items_from_tool_result(payload: dict[str, Any]) -> tuple[dict[str, Any], ...] | None:
    for key in ("model_output", "output"):
        raw_output = payload.get(key)
        if not isinstance(raw_output, str) or not raw_output.strip():
            continue
        try:
            decoded = json.loads(raw_output)
        except (TypeError, ValueError):
            continue
        if isinstance(decoded, dict) and "todos" in decoded:
            return _normalize_todo_items(decoded.get("todos"))
    return None


def event_to_model_message(event: SessionEvent) -> dict[str, Any] | None:
    if event.type == "user_message":
        content = event.payload.get("content", "")
        return {"role": "user", "content": content} if isinstance(content, str) and content.strip() else None
    if event.type == "assistant_message":
        content = event.payload.get("content", "")
        if not (isinstance(content, str) and content.strip()):
            return None
        message: dict[str, Any] = {"role": "assistant", "content": content}
        # 思考模式下网关要求历史 assistant 消息回传 reasoning_content；
        # 运行期 _assistant_message 会带上该字段，投影必须同样恢复。
        reasoning = event.payload.get("reasoning_content")
        if isinstance(reasoning, str) and reasoning:
            message["reasoning_content"] = reasoning
        return message
    if event.type == "turn_cancelled":
        # 取消回合必须投影为可恢复的 assistant 摘要：仅靠 user_message
        # 无法让 /resume 复现进程内历史，后续提问会丢失取消上下文。
        summary = event.payload.get("summary", "")
        if not isinstance(summary, str) or not summary.strip():
            summary = CANCELLED_TURN_DEFAULT_SUMMARY
        return {"role": "assistant", "content": summary}
    if event.type == "run_guard_paused":
        # pause_work 不产生正常最终回答，但仍需要向恢复后的模型说明上一轮
        # 在哪里停下；工具调用/结果事件会在此前分别投影，避免留下未配对协议。
        message = event.payload.get("message", "")
        if not isinstance(message, str) or not message.strip():
            message = "（上一任务已暂停，用户发送“继续”后恢复。）"
        return {"role": "assistant", "content": message.strip()}
    if event.type == "compact_summary":
        content = event.payload.get("content", "")
        if isinstance(content, str) and content.strip():
            return {"role": "assistant", "content": f"{COMPACT_SUMMARY_PREFIX}{content}"}
    if event.type == "tool_call_requested":
        content = _tool_call_context(event.payload)
        return {"role": "assistant", "content": content} if content else None
    if event.type == "tool_call_denied":
        content = _tool_denied_context(event.payload)
        return {"role": "assistant", "content": content} if content else None
    if event.type == "tool_result":
        content = _tool_result_context(event.payload)
        return {"role": "assistant", "content": content} if content else None
    return None


def _tool_call_context(payload: dict[str, Any]) -> str:
    tool = payload.get("tool", "")
    if not isinstance(tool, str) or not tool.strip():
        return ""
    arguments = payload.get("arguments", {})
    safe_arguments = arguments if isinstance(arguments, dict) else {}
    arguments_text = json.dumps(safe_arguments, ensure_ascii=False, sort_keys=True)
    return f"{TOOL_CALL_CONTEXT_PREFIX}{tool.strip()} 参数：{arguments_text}"


def _tool_denied_context(payload: dict[str, Any]) -> str:
    tool = payload.get("tool", "")
    reason = payload.get("reason", "")
    if not isinstance(tool, str) or not tool.strip():
        return ""
    reason_text = reason.strip() if isinstance(reason, str) and reason.strip() else "未批准。"
    return f"{TOOL_RESULT_CONTEXT_PREFIX}{tool.strip()} 失败，原因：{reason_text}"


def _tool_result_context(payload: dict[str, Any]) -> str:
    tool = payload.get("tool", "")
    if not isinstance(tool, str) or not tool.strip():
        return ""
    status = "成功" if bool(payload.get("ok", False)) else "失败"
    output = payload.get("model_output")
    if not isinstance(output, str) or not output.strip():
        output = payload.get("output_preview")
    if not isinstance(output, str) or not output.strip():
        output = payload.get("output", "")
    if not isinstance(output, str):
        output = ""

    artifact_path = payload.get("artifact_path", "")
    artifact_hint = ""
    if isinstance(artifact_path, str) and artifact_path.strip():
        artifact_hint = f"\n完整输出 artifact：{artifact_path.strip()}"
    return f"{TOOL_RESULT_CONTEXT_PREFIX}{tool.strip()} {status}\n{output.strip()}{artifact_hint}".strip()


INTERRUPTED_TOOL_RESULT_TEXT = "（已中断：回合结束前未返回结果，执行状态未知。）"
INTERRUPTED_TURN_DEFAULT_SUMMARY = "（上一回合因异常中断，未生成最终回复）"


def format_tool_result_content(tool: str, ok: bool, output: str) -> str:
    """工具结果的模型可见正文；运行时代理与恢复投影共用，保证逐字一致。"""

    return f"状态：{'成功' if ok else '失败'}\n工具：{tool}\n结果：\n{output}"


def tool_result_message(
    tool: str,
    ok: bool,
    output: str,
    tool_call_id: str,
) -> dict[str, Any]:
    """构造一条 tool 结果协议消息（与 assistant tool_calls 配对的最小单位）。"""

    return {
        "role": "tool",
        "tool_call_id": tool_call_id,
        "content": format_tool_result_content(tool, ok, output),
    }


def interrupted_tool_result_message(tool: str, tool_call_id: str) -> dict[str, Any]:
    """未返回结果的工具调用占位：只声明「已中断」，不伪装成功或失败结论。"""

    return tool_result_message(tool, False, INTERRUPTED_TOOL_RESULT_TEXT, tool_call_id)


def complete_tool_pairing(messages: Sequence[dict[str, Any]]) -> list[dict[str, Any]]:
    """为缺失结果的 assistant tool_calls 补齐「已中断」结果，保证协议配对。

    取消/异常中断回合可能以未配对的工具调用收尾；本函数只在该工具调用对应
    的结果段末尾插入占位消息，不改写任何既有消息，供连续对话与恢复共用。
    """

    completed: list[dict[str, Any]] = []
    index = 0
    total = len(messages)
    while index < total:
        message = messages[index]
        if message.get("role") == "tool":
            # 循环顶部的 tool 消息一定没有紧邻的前置 assistant tool_calls
            # （配对正常的 ones 已在下方内层循环消费）：丢弃，避免非法协议。
            index += 1
            continue
        completed.append(message)
        tool_calls = message.get("tool_calls")
        if (
            message.get("role") != "assistant"
            or not isinstance(tool_calls, list)
            or not tool_calls
        ):
            index += 1
            continue
        expected: list[tuple[str, str]] = []
        for call in tool_calls:
            if not isinstance(call, dict):
                continue
            function = call.get("function")
            name = str(function.get("name") or "") if isinstance(function, dict) else ""
            call_id = str(call.get("id") or "") or name
            if call_id:
                expected.append((call_id, name or call_id))
        expected_ids = {call_id for call_id, _name in expected}
        seen: set[str] = set()
        cursor = index + 1
        while cursor < total and messages[cursor].get("role") == "tool":
            call_id = str(messages[cursor].get("tool_call_id") or "")
            if call_id and call_id not in expected_ids:
                # 多余或错配的结果：不能进入协议，否则 Provider 会拒收。
                cursor += 1
                continue
            completed.append(messages[cursor])
            if call_id:
                seen.add(call_id)
            cursor += 1
        for call_id, name in expected:
            if call_id in seen:
                continue
            completed.append(interrupted_tool_result_message(name, call_id))
        index = cursor
    return completed


def tool_result_output_text(payload: Mapping[str, Any]) -> str:
    """按模型可见优先级提取工具结果输出（model_output → output_preview → output）。"""

    for key in ("model_output", "output_preview", "output"):
        value = payload.get(key)
        if isinstance(value, str) and value.strip():
            return value
    return ""


def function_tool_call(
    call_id: str,
    function_name: str,
    arguments: str,
) -> dict[str, Any]:
    """构造单条 OpenAI 形状的 tool_call（arguments 为已序列化的 JSON 字符串）。

    ``arguments`` 必须由调用方用与 Session 事件完全相同的公开参数投影序列化，
    这样「发给模型的消息」「落盘事件」「恢复重建的消息」三者逐字一致。
    """

    return {
        "id": call_id,
        "type": "function",
        "function": {
            "name": function_name,
            "arguments": arguments,
        },
    }


class TurnHistoryProjector:
    """把会话事件增量投影为模型协议消息。

    运行期把每条新落盘事件 ``feed`` 进来，轮次收尾时 ``take`` 得到本轮新增的
    完整协议消息；``project_history_messages`` 用同一状态机投影整份事件流。
    两条路径共用唯一实现，保证「同一会话内多轮对话的完整继承」与「重启恢复
    后的历史」逐字一致，从而保持已发送消息前缀不变、不破坏前缀缓存。
    """

    def __init__(self, raw_arguments_provider: Any | None = None) -> None:
        """``raw_arguments_provider(call_id, tool_name) -> str`` 可提供协议原文。

        运行期把「真正发往 Provider 的 arguments 原文」留在内存里投影，使同一
        会话内多轮对话与已发送内容逐字一致（前缀缓存完全命中）；协议原文绝不
        落盘：Session 事件只保存脱敏后的公开参数，重启恢复时回退到该投影。
        """

        self._raw_arguments_provider = raw_arguments_provider
        self._entries: list[tuple[str, dict[str, Any]]] = []
        self._reset_tool_group()

    def _reset_tool_group(self) -> None:
        """清空挂起的工具调用组（不动已完成的 messages）。"""

        self._tool_calls: list[dict[str, Any]] = []
        self._tool_calls_content: Any = None
        self._tool_calls_reasoning = ""
        self._tool_calls_anchor = ""
        self._tool_calls_flushed = False
        self._unresolved: dict[str, tuple[str, str]] = {}

    def reset(self) -> None:
        """丢弃全部状态；压缩边界替换历史后由调用方重新累积后续事件。"""

        self._entries = []
        self._reset_tool_group()

    def _flush_tool_calls(self) -> None:
        """把挂起的工具调用落成一条 assistant tool_calls 消息。"""

        if self._tool_calls and not self._tool_calls_flushed:
            # content 原样保留（可能是 None 或空串）：任何归一化都会让恢复
            # 重建与运行时发送的消息出现字段差异，从而让前缀缓存无法命中。
            message: dict[str, Any] = {
                "role": "assistant",
                "content": self._tool_calls_content,
            }
            # 思考模式下上游（DeepSeek 等）要求带 tool_calls 的历史 assistant
            # 消息回传 reasoning_content，否则二次请求直接 400 拒绝：
            # "The `reasoning_content` in the thinking mode must be passed back"。
            # 与运行期 assistant_tool_call_message 保持同一规则（仅在非空时写入）。
            if self._tool_calls_reasoning:
                message["reasoning_content"] = self._tool_calls_reasoning
            message["tool_calls"] = list(self._tool_calls)
            self._entries.append((self._tool_calls_anchor, message))
            self._tool_calls_flushed = True

    def _flush_tool_group(self) -> None:
        """结束当前工具组：先落调用消息，再为未返回结果的调用补中断占位。"""

        self._flush_tool_calls()
        for call_id, (tool, anchor_id) in self._unresolved.items():
            self._entries.append(
                (anchor_id, interrupted_tool_result_message(tool, call_id))
            )
        self._reset_tool_group()

    def feed(self, event: SessionEvent) -> None:
        """按事件类型推进状态机；无关事件类型直接忽略。"""

        event_type = event.type
        payload = event.payload if isinstance(event.payload, dict) else {}
        if event_type == "tool_call_requested":
            self._feed_tool_call(event.event_id, payload)
            return
        if event_type == "tool_result":
            self._feed_tool_result(event.event_id, payload)
            return
        if event_type == "tool_call_denied":
            # 拒绝结果由随后的 tool_result 事件补全；不单独投影协议消息。
            return
        if event_type == "user_message":
            self._flush_tool_group()
            self._append_event_message(event)
            return
        if event_type == "session_interrupted":
            self._flush_tool_group()
            self._entries.append(
                (
                    event.event_id,
                    {"role": "assistant", "content": INTERRUPTED_TURN_DEFAULT_SUMMARY},
                )
            )
            return
        if event_type == "compact_summary":
            self._feed_compact_summary(event)
            return
        if event_type in {"assistant_message", "turn_cancelled", "run_guard_paused"}:
            self._flush_tool_group()
        self._append_event_message(event)

    def take(self) -> list[tuple[str, dict[str, Any]]]:
        """结束投影并返回全部（锚点事件 ID，消息）条目，同时清空状态。"""

        self._flush_tool_group()
        entries = self._entries
        self._entries = []
        return entries

    def drain(self) -> list[tuple[str, dict[str, Any]]]:
        """取走当前已完成的消息，但保留挂起的工具调用组。

        用于压缩重建历史后立即把后续事件（如溢出恢复提示）落进历史：此时
        不能为尚未执行完的工具批补占位，否则后续真实结果会与之重复。
        """

        entries = self._entries
        self._entries = []
        return entries

    # ---- 各事件类型的状态转移 ----

    def _append_event_message(self, event: SessionEvent) -> None:
        message = event_to_model_message(event)
        if message is not None:
            self._entries.append((event.event_id, message))

    def _feed_tool_call(self, event_id: str, payload: Mapping[str, Any]) -> None:
        tool = str(payload.get("tool") or "").strip()
        if not tool:
            return
        if self._tool_calls_flushed:
            # 上一条 assistant 工具消息已经结束；新到调用属于下一条消息。
            self._flush_tool_group()
        call_id = str(payload.get("tool_call_id") or "").strip() or tool
        function_name = str(payload.get("function_name") or "").strip() or tool
        arguments = payload.get("arguments")
        arguments = arguments if isinstance(arguments, dict) else {}
        if not self._tool_calls:
            raw_content = payload.get("assistant_content")
            self._tool_calls_content = (
                raw_content
                if isinstance(raw_content, str) or raw_content is None
                else None
            )
            # reasoning_content 只在该工具批次的首次调用事件里取一次：同一批
            # 调用共享同一条 assistant 消息，逐条覆盖会让消息内容抖动。
            raw_reasoning = payload.get("assistant_reasoning_content")
            self._tool_calls_reasoning = (
                raw_reasoning.strip()
                if isinstance(raw_reasoning, str) and raw_reasoning.strip()
                else ""
            )
        # 优先使用运行期发给 Provider 的 arguments 原文（仅内存投影可用）：公开
        # 参数投影只服务 UI/审计与脱敏落盘，回退到它会让同一段历史出现两种写法。
        raw_arguments = ""
        if self._raw_arguments_provider is not None:
            try:
                raw_arguments = str(
                    self._raw_arguments_provider(call_id, tool) or ""
                )
            except Exception:  # noqa: BLE001 - 取原文失败不得中断回合
                raw_arguments = ""
        if not raw_arguments.strip():
            persisted = payload.get("arguments_json")
            raw_arguments = (
                persisted.strip()
                if isinstance(persisted, str) and persisted.strip()
                else json.dumps(arguments, ensure_ascii=False)
            )
        self._tool_calls.append(
            function_tool_call(
                call_id,
                function_name,
                raw_arguments,
            )
        )
        self._tool_calls_anchor = event_id
        self._unresolved[call_id] = (tool, event_id)

    def _feed_tool_result(self, event_id: str, payload: Mapping[str, Any]) -> None:
        tool = str(payload.get("tool") or "").strip()
        call_id = str(payload.get("tool_call_id") or "").strip() or tool
        if not self._tool_calls and not self._unresolved:
            # 没有配对调用来源的孤立结果无法构成合法协议消息，跳过。
            return
        self._flush_tool_calls()
        self._entries.append(
            (
                event_id,
                tool_result_message(
                    tool,
                    bool(payload.get("ok", False)),
                    tool_result_output_text(payload),
                    call_id,
                ),
            )
        )
        if call_id in self._unresolved:
            del self._unresolved[call_id]
        else:
            _drop_pending_tool_name(self._unresolved, tool)

    def _feed_compact_summary(self, event: SessionEvent) -> None:
        """压缩摘要边界：历史被替换为「摘要 + 保留窗口」。

        旧摘要只带 ``remaining_message_count``，只能按数量兼容恢复；带
        ``remaining_event_ids`` 的新摘要本应由 ``project_session_history``
        先裁掉被压缩事件再投影，此时 ``entries`` 必为空，数量/锚点筛选都是
        空操作。这里保留兼容分支，保证直接喂入全量事件流的旧调用方行为不变。
        """

        self._flush_tool_group()
        summary_message = event_to_model_message(event)
        remaining_ids = event.payload.get("remaining_event_ids")
        if isinstance(remaining_ids, list) and all(
            isinstance(item, str) and item for item in remaining_ids
        ):
            wanted = set(remaining_ids)
            recent_entries = [entry for entry in self._entries if entry[0] in wanted]
        else:
            remaining_count = read_payload_non_negative_int(
                event.payload.get("remaining_message_count", 0)
            )
            recent_entries = self._entries[-remaining_count:] if remaining_count else []
        self._entries = (
            ([(event.event_id, summary_message)] if summary_message is not None else [])
            + recent_entries
        )


def project_history_messages(
    events: Sequence[SessionEvent],
) -> list[tuple[str, dict[str, Any]]]:
    """把有效事件流投影为（锚点事件 ID，消息）序列，供会话恢复构建模型历史。

    工具事件按运行时协议重建为完整的 assistant tool_calls 与 tool 结果消息，
    与连续对话的完整继承保持一致（不再折叠为「本轮工作记录」摘要）：一批
    连续的工具调用合并为一条 assistant 消息（锚定该批最后一次调用事件），
    每个结果各为一条 tool 消息（锚定自身事件）；未返回结果的调用补齐
    「已中断」占位，取消/异常中断回合追加说明消息。新用户消息或压缩摘要
    出现前若仍有未收尾的工具，先落完整工具消息再继续投影。
    """

    projector = TurnHistoryProjector()
    for event in events:
        projector.feed(event)
    return projector.take()


def project_session_history(
    events: Sequence[SessionEvent],
) -> list[tuple[str, dict[str, Any]]]:
    """投影整份会话事件的有效历史，并正确处理压缩边界。

    最后一个带 ``remaining_event_ids`` 的压缩摘要之前的事件已被摘要取代；
    这里先按事件 ID 裁掉被压缩窗口再投影，使「重启恢复」与「压缩后的运行期
    历史」得到逐字相同的消息序列。缺少 ``remaining_event_ids`` 的旧摘要继续
    交给 ``project_history_messages`` 的兼容分支按锚点/数量处理。
    """

    boundary_index = -1
    boundary_ids: set[str] = set()
    for index in range(len(events) - 1, -1, -1):
        event = events[index]
        if event.type != "compact_summary":
            continue
        remaining_ids = event.payload.get("remaining_event_ids")
        if isinstance(remaining_ids, list) and all(
            isinstance(item, str) and item for item in remaining_ids
        ):
            boundary_index = index
            boundary_ids = set(remaining_ids)
        break
    if boundary_index < 0:
        return project_history_messages(events)
    boundary = events[boundary_index]
    # 压缩边界之后的全部事件都是压缩后新增的（恢复指令、后续工具调用、
    # 后续最终回复），必须完整保留；边界之前只保留摘要声明的保留窗口。
    # 只按 remaining_event_ids 筛选会把压缩后新增的事件也当旧窗口丢掉，
    # 使恢复历史短于运行期历史（本次修复的前身就是这么错的）。
    selected: list[SessionEvent] = [boundary]
    selected.extend(
        event
        for index, event in enumerate(events)
        if index != boundary_index
        and event.type != "compact_summary"
        and (index > boundary_index or event.event_id in boundary_ids)
    )
    return project_history_messages(selected)


def project_compaction_boundary_history(
    summary_payload: Mapping[str, Any],
    events: Sequence[SessionEvent],
) -> list[dict[str, Any]]:
    """按压缩摘要的保留窗口重建运行期历史消息。

    压缩完成时运行期 ``_history`` 必须与「重启后由 ``project_session_history``
    重建」一致，否则同一会话在压缩前后会出现两份不同的上下文并让前缀缓存失效。
    这里用同一个投影器重建：摘要消息 + ``remaining_event_ids`` 命中的事件。
    """

    summary_event = _projection_only_event("compact_summary", summary_payload)
    remaining_ids = summary_payload.get("remaining_event_ids")
    if not isinstance(remaining_ids, list) or not all(
        isinstance(item, str) and item for item in remaining_ids
    ):
        return [
            message
            for _anchor, message in project_history_messages([summary_event, *events])
        ]
    wanted = set(remaining_ids)
    selected: list[SessionEvent] = [summary_event]
    selected.extend(
        event
        for event in events
        if event.type != "compact_summary" and event.event_id in wanted
    )
    return [message for _anchor, message in project_history_messages(selected)]


def _projection_only_event(event_type: str, payload: Mapping[str, Any]) -> SessionEvent:
    """构造只参与投影、不落盘的临时事件（锚点 ID 固定且不会与真实事件冲突）。"""

    return SessionEvent.from_dict(
        {
            "version": 1,
            # session_id 必须满足 SessionEvent 的格式校验（全零形状），否则事件
            # 构造会直接报错，压缩重建整个流程都会失败。
            "session_id": "00000000-000000-000000",
            "event_id": f"{PROJECTION_ONLY_EVENT_ID_PREFIX}{event_type}",
            "type": event_type,
            "created_at": "1970-01-01T00:00:00+00:00",
            "payload": dict(payload),
        }
    )


def _drop_pending_tool_name(
    unresolved: dict[str, tuple[str, str]],
    tool: str,
) -> None:
    """缺少可匹配 tool_call_id 时，按最近的同名未完成调用解除配对。"""

    for call_id in reversed(list(unresolved)):
        if unresolved[call_id][0] == tool:
            del unresolved[call_id]
            return
