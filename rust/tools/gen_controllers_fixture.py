#!/usr/bin/env python3
"""生成 `omnicrawl/agent/controllers/` 的对照数据集。

期望值全部来自 Python 真实现：能直接调的函数直接调；挂在 Mixin 上、依赖宿主对象的
方法用一个最小探针对象驱动（只补上方法真正读到的属性），不改写被测逻辑。

用法：``python rust/tools/gen_controllers_fixture.py``
输出：``rust/crates/omnicrawl-controllers/tests/fixtures/controllers_parity.json``
"""

from __future__ import annotations

import hashlib
import json
import os
import sys
import tempfile
from pathlib import Path, PurePosixPath
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = (
    ROOT / "rust/crates/omnicrawl-controllers/tests/fixtures/controllers_parity.json"
)

sys.path.insert(0, str(ROOT))

import omnicrawl.agent.controllers.tools.output as output_module  # noqa: E402
from omnicrawl.agent.controllers import shared as shared_module  # noqa: E402
from omnicrawl.agent.controllers.memory.stores import MemoryStoresMixin  # noqa: E402
from omnicrawl.agent.controllers.tools import compression as compression_module  # noqa: E402
from omnicrawl.agent.controllers.tools.building import ToolBuildingMixin  # noqa: E402
from omnicrawl.agent.controllers.tools.output import ToolOutputMixin as OutputMixin  # noqa: E402
from omnicrawl.agent.controllers.undo import UndoMixin  # noqa: E402
from omnicrawl.agent.controllers.workspace.switching import WorkspaceSwitchingMixin  # noqa: E402
from omnicrawl.agent.controllers.workspace.toolbox import WorkspaceToolboxMixin  # noqa: E402
from omnicrawl.agent.runtime.vision_proxy import VisionProxyError  # noqa: E402
from omnicrawl.agent.types import ToolCall, ToolResult, ToolImageAttachment  # noqa: E402
from omnicrawl.config.models.vision import VisionConfiguration  # noqa: E402
from omnicrawl.state.session_artifacts import preview_text  # noqa: E402

for module in (shared_module, output_module, compression_module):
    if not Path(module.__file__).resolve().is_relative_to(ROOT):
        raise SystemExit("加载到的不是仓库源码")


def outcome(callable_, *args, **kwargs):
    """记录一次调用的返回值或错误文案。"""

    try:
        return {"ok": True, "value": callable_(*args, **kwargs), "error": None}
    except Exception as exc:  # noqa: BLE001 - 对照数据集要原样记录失败文案
        return {"ok": False, "value": None, "error": str(exc)}


def posix(path) -> str:
    return str(path).replace("\\", "/")


def digest(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def shorten(text: str, limit: int = 120) -> str:
    if len(text) <= limit:
        return text
    return text[:limit] + "…(共%d字符)" % len(text)


def result_view(result) -> dict:
    return {
        "ok": result.ok,
        "output": shorten(result.output),
        "output_len": len(result.output),
        "output_sha256": digest(result.output),
        "full_output": shorten(result.full_output),
        "full_output_len": len(result.full_output),
        "full_output_sha256": digest(result.full_output),
        "model_images": len(result.model_images),
        "error_code": result.error_code,
        "completed_at_is_none": result.completed_at is None,
    }


# --------------------------------------------------------------------------- shared


def shared_cases() -> dict:
    int_env_cases = []
    for label, name, raw, default, min_value, max_value in [
        ("未设置用默认值", "OC_PROBE_INT", None, 600, 1, 3600),
        ("空串用默认值", "OC_PROBE_INT", "   ", 600, 1, 3600),
        ("正常取值", "OC_PROBE_INT", " 120 ", 600, 1, 3600),
        ("带正号", "OC_PROBE_INT", "+120", 600, 1, 3600),
        ("下划线分隔", "OC_PROBE_INT", "1_200", 600, 1, 3600),
        ("非法文本", "OC_PROBE_INT", "abc", 600, 1, 3600),
        ("浮点文本", "OC_PROBE_INT", "12.5", 600, 1, 3600),
        ("低于下限", "OC_PROBE_INT", "0", 600, 1, 3600),
        ("高于上限", "OC_PROBE_INT", "99999", 600, 1, 3600),
        ("十六进制文本", "OC_PROBE_INT", "0x10", 600, 1, 3600),
    ]:
        previous = os.environ.get(name)
        if raw is None:
            os.environ.pop(name, None)
        else:
            os.environ[name] = raw
        try:
            observed = outcome(
                shared_module._read_int_env,
                name,
                default,
                min_value=min_value,
                max_value=max_value,
            )
        finally:
            if previous is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = previous
        int_env_cases.append(
            {
                "label": label,
                "raw": raw,
                "default": default,
                "min": min_value,
                "max": max_value,
                **observed,
            }
        )

    range_cases = []
    for label, value, min_value, max_value in [
        ("区间内", 600, 1, 3600),
        ("等于下限", 1, 1, 3600),
        ("等于上限", 3600, 1, 3600),
        ("低于下限", 0, 1, 3600),
        ("高于上限", 3601, 1, 3600),
    ]:
        range_cases.append(
            {
                "label": label,
                "value_in": value,
                "min": min_value,
                "max": max_value,
                **outcome(
                    shared_module._validate_int_range,
                    "tool_timeout",
                    value,
                    min_value=min_value,
                    max_value=max_value,
                ),
            }
        )

    unknown_cases = []
    search_digest = hashlib.sha1(b"search_tools").hexdigest()[:10]
    for label, requested, active in [
        ("完整哈希名变体", "tool_search_tools_%s" % search_digest, ["search_tools", "read"]),
        ("截断的哈希名", "tool_search_%s" % search_digest, ["search_tools", "read"]),
        ("普通未知名", "nope", ["read", "write"]),
        ("哈希形状但无命中", "tool_x_0123456789", ["read", "write"]),
        ("大写 digest", "tool_search_%s" % search_digest.upper(), ["search_tools"]),
    ]:
        observed = outcome(
            shared_module._unknown_tool_result, requested, dict.fromkeys(active)
        )
        unknown_cases.append(
            {
                "label": label,
                "requested_name": requested,
                "active_tools": active,
                "ok": observed["ok"],
                "result": result_view(observed["value"]) if observed["ok"] else None,
                "error": observed["error"],
            }
        )

    timeout_cases = []
    for label, seconds, hint in [
        ("无附加提示", 600, ""),
        ("带顾问提示", 5, shared_module.ASK_USER_ADVISOR_HINT),
    ]:
        observed = outcome(shared_module._tool_timeout_result, seconds, hint)
        timeout_cases.append(
            {
                "label": label,
                "seconds": seconds,
                "hint": hint,
                "result": result_view(observed["value"]),
            }
        )

    def quick_call(index):
        return ToolResult(ok=True, output="完成 %d" % index)

    def slow_call(index):
        import time

        time.sleep(2)
        return ToolResult(ok=True, output="太晚 %d" % index)

    execute_cases = []
    for label, callable_, index, timeout_seconds in [
        ("限时内完成", quick_call, 3, 5),
        ("超时返回超时结果", slow_call, 1, 1),
    ]:
        observed = outcome(
            shared_module._execute_call_with_timeout, callable_, index, timeout_seconds
        )
        execute_cases.append(
            {
                "label": label,
                "index": index,
                "timeout_seconds": timeout_seconds,
                "ok": observed["ok"],
                "result": result_view(observed["value"]) if observed["ok"] else None,
                "error": observed["error"],
            }
        )

    return {
        "int_env": int_env_cases,
        "int_range": range_cases,
        "unknown_tool": unknown_cases,
        "timeout_result": timeout_cases,
        "execute_with_timeout": execute_cases,
        "constants": {
            "default_tool_timeout_seconds": shared_module.DEFAULT_TOOL_TIMEOUT_SECONDS,
            "max_tool_timeout_seconds": shared_module.MAX_TOOL_TIMEOUT_SECONDS,
            "tool_output_inline_limit_chars": shared_module.TOOL_OUTPUT_INLINE_LIMIT_CHARS,
            "tool_output_batch_budget_chars": shared_module.TOOL_OUTPUT_BATCH_BUDGET_CHARS,
            "tool_output_archived_preview_chars": shared_module.TOOL_OUTPUT_ARCHIVED_PREVIEW_CHARS,
            "subagent_lifecycle_wait_seconds": shared_module.SUBAGENT_LIFECYCLE_WAIT_SECONDS,
            "system_prompt_file": shared_module.SYSTEM_PROMPT_FILE,
            "agents_instructions_file": shared_module.AGENTS_INSTRUCTIONS_FILE,
            "context_overflow_recovery_prompt": shared_module._CONTEXT_OVERFLOW_RECOVERY_PROMPT,
            "context_overflow_error_markers": list(
                shared_module._CONTEXT_OVERFLOW_ERROR_MARKERS
            ),
            "rate_limit_error_markers": list(shared_module._RATE_LIMIT_ERROR_MARKERS),
            "continue_last_task_texts": sorted(shared_module._CONTINUE_LAST_TASK_TEXTS),
            "read_only_undo_tools": sorted(shared_module._READ_ONLY_UNDO_TOOLS),
            "reversible_undo_tools": sorted(shared_module._REVERSIBLE_UNDO_TOOLS),
            "memory_undo_exempt_tools": sorted(shared_module._MEMORY_UNDO_EXEMPT_TOOLS),
            "ask_user_advisor_hint": shared_module.ASK_USER_ADVISOR_HINT,
        },
    }


# ----------------------------------------------------------------------------- undo


class UndoProbe(UndoMixin):
    """只补 `_record_turn_tool_execution` 真正调用的宿主方法。"""

    def __init__(self):
        self.captures = 0

    def _ensure_turn_captured(self, snapshot):
        self.captures += 1
        snapshot.capture_attempted = True


class EventProbe:
    def __init__(self, event_type, payload):
        self.type = event_type
        self.payload = payload


class PlanProbe:
    def __init__(self, events, session_id="session-1"):
        self.events = events
        self.session_id = session_id


class RestoreProbe(UndoMixin):
    def __init__(self, workspace_root):
        self.workspace_root = Path(workspace_root)


SAFE_TOOL_CASES = [
    ("只读工具", "read", {}),
    ("可回退写工具", "Edit_file", {"path": "a.py"}),
    ("记忆豁免工具", "memory_write", {}),
    ("bash 不可逆", "bash", {"command": "echo hi"}),
    ("subagent 只读 action", "subagent", {"action": " list "}),
    ("subagent 默认 action", "subagent", {}),
    ("subagent 执行 action", "subagent", {"action": "run"}),
    ("subagent 非字符串 action", "subagent", {"action": 7}),
    ("monitor 轮询", "monitor", {"action": "poll"}),
    ("monitor 停止", "monitor", {"action": "stop"}),
    ("windows_window 列表", "windows_window", {"action": "list"}),
    ("windows_clipboard 读取", "windows_clipboard", {}),
    ("windows_clipboard 写入", "windows_clipboard", {"action": "write_text"}),
    ("windows_screenshot", "windows_screenshot", {}),
    ("未知工具", "whatever", {}),
]

LEDGER_SEQUENCES = [
    ("只读序列", [("read", {}), ("grep", {"pattern": "x"})]),
    ("可回退写工具补捕获", [("read", {}), ("write_file", {"path": "a"})]),
    ("不可逆工具进账本", [("bash", {"command": "rm -rf x"}), ("bash", {"command": "ls"})]),
    ("混合序列", [("memory_write", {"content": "x"}), ("Edit_file", {}), ("monitor", {})]),
]


def undo_cases(tmp_root: Path) -> dict:
    safe_cases = [
        {
            "label": label,
            "name": name,
            "arguments": arguments,
            "expected": outcome(UndoMixin._tool_is_undo_safe, name, dict(arguments)),
        }
        for label, name, arguments in SAFE_TOOL_CASES
    ]

    ledger_cases = []
    for label, sequence in LEDGER_SEQUENCES:
        probe = UndoProbe()
        snapshot = shared_module._ActiveTurnSnapshot(
            snapshot_id="snap-1", store=None, workspace=Path("/ws")
        )
        for name, arguments in sequence:
            probe._record_turn_tool_execution(
                snapshot, ToolCall(name=name, arguments=dict(arguments))
            )
        ledger_cases.append(
            {
                "label": label,
                "sequence": [
                    {"name": name, "arguments": arguments} for name, arguments in sequence
                ],
                "executed_tools": list(snapshot.executed_tools),
                "irreversible_tools": list(snapshot.irreversible_tools),
                "captures": probe.captures,
                "capture_attempted": snapshot.capture_attempted,
            }
        )

    workspace = tmp_root / "workspace"
    workspace.mkdir(parents=True, exist_ok=True)
    snapshot_payload = {
        "version": 2,
        "snapshot_id": "snap-2",
        "workspace": str(workspace),
        "begin_patch": "undo/begin.patch",
        "begin_untracked": "undo/begin.untracked.txt",
        "end_patch": "undo/end.patch",
        "end_untracked": "undo/end.untracked.txt",
        "executed_tools": ["read"],
        "irreversible_tools": [],
    }
    scenarios = [
        ("无快照无副作用", []),
        ("无快照但压缩过上下文", [EventProbe("compact_summary", {"summary": "x"})]),
        (
            "无快照且工具失败",
            [EventProbe("tool_result", {"tool": "bash", "ok": False, "tool_call_id": "c1"})],
        ),
        (
            "无快照且只读工具",
            [
                EventProbe("tool_call_requested", {"tool_call_id": "c2", "arguments": {}}),
                EventProbe("tool_result", {"tool": "read", "ok": True, "tool_call_id": "c2"}),
            ],
        ),
        (
            "无快照且有不可逆工具",
            [
                EventProbe(
                    "tool_call_requested",
                    {"tool_call_id": "c3", "arguments": {"command": "rm -rf x"}},
                ),
                EventProbe("tool_result", {"tool": "bash", "ok": True, "tool_call_id": "c3"}),
            ],
        ),
        (
            "无快照且可回退写工具",
            [
                EventProbe("tool_call_requested", {"tool_call_id": "c4", "arguments": {}}),
                EventProbe("tool_result", {"tool": "Edit_file", "ok": True, "tool_call_id": "c4"}),
            ],
        ),
        (
            "无快照且缺少请求配对",
            [EventProbe("tool_result", {"tool": "bash", "ok": True, "tool_call_id": "c9"})],
        ),
        (
            "两个快照事件",
            [
                EventProbe("turn_snapshot", snapshot_payload),
                EventProbe("turn_snapshot", snapshot_payload),
            ],
        ),
        ("旧版快照", [EventProbe("turn_snapshot", {**snapshot_payload, "version": 1})]),
        (
            "不可逆账本格式非法",
            [EventProbe("turn_snapshot", {**snapshot_payload, "irreversible_tools": "bash"})],
        ),
        (
            "不可逆账本非空",
            [
                EventProbe(
                    "turn_snapshot",
                    {**snapshot_payload, "irreversible_tools": ["bash", " bash ", ""]},
                )
            ],
        ),
        (
            "工作区不一致",
            [
                EventProbe(
                    "turn_snapshot",
                    {**snapshot_payload, "workspace": str(tmp_root / "other")},
                )
            ],
        ),
    ]
    restore_cases = []
    for label, events in scenarios:
        observed = outcome(
            RestoreProbe(workspace)._restore_turn_side_effects, PlanProbe(list(events))
        )
        restore_cases.append(
            {
                "label": label,
                "events": [
                    {"type": event.type, "payload": event.payload} for event in events
                ],
                "workspace": posix(workspace),
                "ok": observed["ok"],
                "decision": "none" if observed["ok"] else None,
                "error": observed["error"],
            }
        )

    path_root = tmp_root / "artifacts"
    path_root.mkdir(parents=True, exist_ok=True)
    path_cases = []
    for label, relative in [
        ("正常相对路径", "undo/begin.patch"),
        ("前后多余斜杠", "/undo/end.patch/"),
        ("反斜杠分隔", "undo\\begin.patch"),
        ("空路径", ""),
        ("仅斜杠", "///"),
        ("穿越到上一级", "../secret.txt"),
        ("中间穿越", "undo/../../secret.txt"),
        ("绝对路径", "/etc/passwd"),
        ("当前目录段", "./undo/begin.patch"),
    ]:
        observed = outcome(UndoMixin._resolve_artifact_path, path_root, relative)
        path_cases.append(
            {
                "label": label,
                "relative": relative,
                "root": posix(path_root),
                "ok": observed["ok"],
                "value": posix(observed["value"]) if observed["ok"] else None,
                "error": observed["error"],
            }
        )

    relative_cases = [
        {
            "label": label,
            "path": posix(path),
            "parent": posix(parent),
            "expected": UndoMixin._is_relative_to(Path(path), Path(parent)),
        }
        for label, path, parent in [
            ("子路径", path_root / "a" / "b", path_root / "a"),
            ("等值", path_root / "a", path_root / "a"),
            ("父路径", path_root / "a", path_root / "a" / "b"),
            ("根路径", path_root / "a", path_root),
        ]
    ]

    return {
        "tool_is_undo_safe": safe_cases,
        "ledger": ledger_cases,
        "restore_precheck": restore_cases,
        "artifact_path": path_cases,
        "is_relative_to": relative_cases,
    }


# ------------------------------------------------------------------------ workspace


class SwitchProbe(WorkspaceSwitchingMixin, UndoMixin):
    def __init__(self, workspace_root, worktrees=(), coordinator=None):
        self.workspace_root = Path(workspace_root)
        self._worktrees = list(worktrees)
        self._subagent_coordinator = coordinator

    def list_subagent_worktrees(self):
        return self._worktrees

    def _dispatch_plugin_hook(self, name, payload):
        return {}

    def _plugin_denial_error(self, name):
        return RuntimeError("插件拒绝：%s" % name)


class ToolboxProbe(WorkspaceToolboxMixin):
    def __init__(self, is_memory=False, is_session=False):
        self._memory = is_memory
        self._session = is_session

    def _is_memory_path(self, path):
        return self._memory

    def _is_session_path(self, path):
        return self._session

    def _relative_path(self, path):
        return posix(path)


class DrainCoordinator:
    """只回答「子任务还没退干净」的协调器替身。"""

    def __init__(self):
        self.resumed = False

    def cancel_and_wait(self, reason, timeout_seconds, permanent):
        return False

    def resume_accepting_when_idle(self):
        self.resumed = True


def workspace_cases(tmp_root: Path) -> dict:
    workspace = tmp_root / "ws"
    workspace.mkdir(parents=True, exist_ok=True)
    other = tmp_root / "other"
    other.mkdir(parents=True, exist_ok=True)
    a_file = tmp_root / "plain.txt"
    a_file.write_text("x", encoding="utf-8")

    switch_cases = []
    for label, target, worktrees, coordinator in [
        ("目标就是当前工作区", posix(workspace), [], None),
        ("目标不是目录", posix(a_file), [], None),
        ("路径不存在", posix(tmp_root / "nope"), [], None),
        ("子任务未在期限内退出", posix(other), [], DrainCoordinator()),
        (
            "有待处理 worktree",
            posix(other),
            [
                {"branch": "feat/a", "task_id": "t1"},
                {"branch": "", "task_id": "t2"},
                {"branch": "feat/c", "task_id": "t3"},
                {"branch": "feat/d", "task_id": "t4"},
            ],
            None,
        ),
    ]:
        observed = outcome(
            SwitchProbe(workspace, worktrees, coordinator).switch_workspace, target
        )
        if label == "路径不存在":
            compare = "prefix"
            error_prefix = "工作区切换失败："
            error_suffix = "无法解析，"
        elif label == "目标不是目录":
            compare = "suffix"
            error_prefix = None
            error_suffix = " 不是目录。"
        else:
            compare = "exact"
            error_prefix = None
            error_suffix = None
        switch_cases.append(
            {
                "label": label,
                "target": target,
                "worktrees": worktrees,
                "draining": coordinator is not None,
                "workspace": posix(workspace),
                "ok": observed["ok"],
                "value": posix(observed["value"]) if observed["ok"] else None,
                "compare": compare,
                "error_prefix": error_prefix,
                "error_suffix": error_suffix,
                "error": observed["error"],
            }
        )

    protection_cases = []
    for label, is_memory, is_session in [
        ("记忆目录", True, False),
        ("会话目录", False, True),
        ("普通路径", False, False),
    ]:
        protection_cases.append(
            {
                "label": label,
                "is_memory": is_memory,
                "is_session": is_session,
                "relative": "a/b.txt",
                "expected": ToolboxProbe(
                    is_memory, is_session
                )._workspace_extra_protection_message(Path("a/b.txt")),
            }
        )

    memory_root = tmp_root / "memory-root"
    (memory_root / "sub").mkdir(parents=True, exist_ok=True)
    elsewhere = tmp_root / "elsewhere.md"
    elsewhere.write_text("x", encoding="utf-8")
    path_membership = []
    for label, path, root, expected in [
        ("根自身", memory_root, memory_root, True),
        ("子路径", memory_root / "sub" / "m.md", memory_root, True),
        ("外部路径", elsewhere, memory_root, False),
    ]:
        observed = UndoMixin._is_relative_to(Path(path).resolve(), Path(root).resolve())
        if observed != expected:
            raise SystemExit("路径归属判定与预期不符：%s" % label)
        path_membership.append(
            {
                "label": label,
                "path": posix(path),
                "root": posix(root),
                "expected": observed,
            }
        )

    return {
        "switch_workspace": switch_cases,
        "protect_message": protection_cases,
        "path_membership": path_membership,
    }


# --------------------------------------------------------------------------- memory


class FakeMemoryStore:
    def __init__(self, root, expired):
        self.root = Path(root)
        self._expired = list(expired)

    def clean_expired_memories(self):
        return list(self._expired)


class MemoryProbe(MemoryStoresMixin, UndoMixin):
    def __init__(
        self,
        workspace_root,
        directory,
        *,
        memory_enabled=True,
        session_id=None,
        session_state=True,
        user_root=None,
        stores=None,
    ):
        self.workspace_root = Path(workspace_root)
        self.config = SimpleNamespace(
            memory_directory=directory, memory_enabled=memory_enabled
        )
        if session_state and session_id is not None:
            self._session_state = SimpleNamespace(session_id=session_id)
        else:
            self._session_state = None
        self._user_root = Path(user_root) if user_root is not None else None
        self._project_memory_store = None
        self._session_memory_store = None
        self._user_memory_store = None
        if stores is not None:
            (
                self._project_memory_store,
                self._session_memory_store,
                self._user_memory_store,
            ) = stores

    def _memory_user_data_root(self):
        return self._user_root


def memory_cases(tmp_root: Path) -> dict:
    workspace = tmp_root / "ws-mem"
    workspace.mkdir(parents=True, exist_ok=True)
    user_root = tmp_root / "userdata"
    user_root.mkdir(parents=True, exist_ok=True)

    project_cases = []
    for label, directory in [
        ("工作区内相对目录", ".omnicrawl/.oclmemory"),
        ("工作区内绝对目录", posix(workspace / "inside")),
        ("工作区外相对目录", "../outside"),
        ("工作区外绝对目录", posix(tmp_root / "outside")),
    ]:
        probe = MemoryProbe(workspace, directory, user_root=user_root)
        observed = outcome(probe._create_memory_stores)
        project_cases.append(
            {
                "label": label,
                "directory": directory,
                "workspace": posix(workspace),
                "ok": observed["ok"],
                "root": posix(observed["value"][0].root) if observed["ok"] else None,
                "session_store_is_none": (
                    observed["value"][1] is None if observed["ok"] else None
                ),
                "user_root": posix(observed["value"][2].root) if observed["ok"] else None,
                "error": observed["error"],
            }
        )

    session_cases = []
    for label, session_id, session_state in [
        ("合法 ID", "abc-1_2", True),
        ("带空格", " abc ", True),
        ("含非法字符", "bad id!", True),
        ("以连字符开头", "-abc", True),
        ("空 ID", "", True),
        ("未启用会话", "abc", False),
    ]:
        probe = MemoryProbe(
            workspace,
            ".omnicrawl/.oclmemory",
            session_id=session_id,
            session_state=session_state,
            user_root=user_root,
        )
        observed = outcome(probe._create_current_session_memory_store)
        value = observed["value"] if observed["ok"] else None
        session_cases.append(
            {
                "label": label,
                "session_id": session_id,
                "session_state": session_state,
                "user_root": posix(user_root),
                "ok": observed["ok"],
                "root": posix(value.root) if value is not None else None,
                "error": observed["error"],
            }
        )

    delete_cases = []
    for label, session_id, enabled in [
        ("删除已存在目录", "session-a", True),
        ("目录不存在", "session-missing", True),
        ("记忆未启用", "session-b", False),
        ("ID 非法", "bad id", True),
    ]:
        target = user_root / "Session_memory" / session_id
        existed_before = session_id in {"session-a", "session-b"}
        if existed_before:
            target.mkdir(parents=True, exist_ok=True)
        probe = MemoryProbe(
            workspace,
            ".omnicrawl/.oclmemory",
            memory_enabled=enabled,
            user_root=user_root,
        )
        observed = outcome(probe._delete_session_memory, session_id)
        delete_cases.append(
            {
                "label": label,
                "session_id": session_id,
                "memory_enabled": enabled,
                "user_root": posix(user_root),
                "existed_before": existed_before,
                "exists_after": target.is_dir(),
                "ok": observed["error"] is None,
                "error": observed["error"],
            }
        )

    clean_cases = []
    stores = (
        FakeMemoryStore(workspace / "proj", [PurePosixPath("/proj/old.md")]),
        FakeMemoryStore(user_root / "sess", [PurePosixPath("/sess/old.md")]),
        FakeMemoryStore(user_root / "user", [PurePosixPath("/user/old.md")]),
    )
    enabled_probe = MemoryProbe(
        workspace, ".omnicrawl/.oclmemory", user_root=user_root, stores=stores
    )
    clean_cases.append(
        {
            "label": "三类作用域",
            "stores": True,
            "expected": enabled_probe.clean_memory(),
            "error": None,
        }
    )
    disabled_probe = MemoryProbe(
        workspace, ".omnicrawl/.oclmemory", user_root=user_root
    )
    clean_cases.append(
        {
            "label": "未启用",
            "stores": False,
            "expected": None,
            "error": outcome(disabled_probe.clean_memory)["error"],
        }
    )

    return {
        "project_root": project_cases,
        "session_store": session_cases,
        "delete_session": delete_cases,
        "clean_memory": clean_cases,
        "user_memory_root": posix(
            MemoryProbe(workspace, ".m", user_root=user_root)._memory_user_data_root()
            / "User_memory"
        ),
    }


# --------------------------------------------------------------------------- output


class OutputProbe(OutputMixin):
    def __init__(self, archive_paths=None):
        self.archive_paths = dict(archive_paths or {})
        self.archived_indices = []

    def _archive_batch_tool_outputs(self, items):
        self.archived_indices = [index for index, _result in items]
        return dict(self.archive_paths)


BUDGET_SCENARIOS = [
    ("全部较小", [10, 200, 3_000]),
    ("单个超限", [10, 50_001, 30]),
    ("空输出与缺失", [0, None, 12]),
    ("单个超限且落盘失败", [60_000]),
    ("批次预算从最大者开始", [60_000, 70_000, 80_000]),
    ("恰好等于批次预算", [100_000, 100_000]),
    ("超过批次预算一点", [100_000, 100_001]),
    ("混合", [None, 50_000, 150_000, 1_000]),
]


ARCHIVE_PATHS = {0: "/tmp/arch-0", 1: "/tmp/arch-1", 2: "/tmp/arch-2"}


def output_cases() -> dict:
    budget_cases = []
    for label, sizes in BUDGET_SCENARIOS:
        results = [
            None if size is None else ToolResult(ok=True, output="x" * size)
            for size in sizes
        ]
        probe = OutputProbe(
            archive_paths=ARCHIVE_PATHS
        )
        observed = outcome(probe._apply_batch_output_budget, results)
        view = None
        if observed["ok"]:
            view = [
                None if result is None else result_view(result)
                for result in observed["value"]
            ]
        budget_cases.append(
            {
                "label": label,
                "sizes": sizes,
                "archive_paths": {str(key): value for key, value in ARCHIVE_PATHS.items()},
                "archived_indices": probe.archived_indices,
                "ok": observed["ok"],
                "value": view,
                "error": observed["error"],
            }
        )

    preview_cases = []
    for label, output, path in [
        ("有落盘路径", "x" * 12_345, "/tmp/a.txt"),
        ("落盘失败", "y" * 3, ""),
        ("短输出", "abc", "/tmp/b.txt"),
        ("恰好 4000", "z" * 4_000, "/tmp/c.txt"),
        ("4001 字符", "z" * 4_001, "/tmp/d.txt"),
    ]:
        preview_cases.append(
            {
                "label": label,
                "output": output if len(output) <= 64 else None,
                "fill_char": output[0] if len(output) > 64 else None,
                "chars": len(output),
                "path": path,
                "expected": OutputMixin._format_archived_output_preview(output, path),
            }
        )

    preview_text_cases = []
    for label, text, limit in [
        ("未超限", "abcdef", 10),
        ("正好等于", "abcdefghij", 10),
        ("奇数上限", "abcdefghijkl", 11),
        ("偶数上限", "abcdefghijkl", 12),
        ("空文本", "", 4),
    ]:
        preview_text_cases.append(
            {
                "label": label,
                "text": text,
                "limit": limit,
                "expected": preview_text(text, limit),
            }
        )

    message_cases = []
    for label, name, ok, output, call_id in [
        ("成功且有 ID", "read", True, "内容", "call-1"),
        ("失败且无 ID", "bash", False, "报错", ""),
    ]:
        tool_call = ToolCall(name=name, id=call_id)
        message_cases.append(
            {
                "label": label,
                "tool": name,
                "ok": ok,
                "output": output,
                "tool_call_id": call_id or name,
                "expected": OutputMixin._tool_result_message(
                    tool_call, ToolResult(ok=ok, output=output)
                ),
            }
        )

    return {
        "batch_budget": budget_cases,
        "archived_preview": preview_cases,
        "preview_text": preview_text_cases,
        "tool_result_message": message_cases,
        "vision": vision_cases(),
    }


class FakeVisionProxy:
    def __init__(self, **kwargs):
        self.kwargs = kwargs

    def analyze(self, images, prompt, cancel_check=None, on_token_usage=None):
        return SimpleNamespace(model="vision-x", text="图片里有猫。")


class FailingVisionProxy:
    def __init__(self, **kwargs):
        pass

    def analyze(self, images, prompt, cancel_check=None, on_token_usage=None):
        raise VisionProxyError("上游 500")


class VisionProbe(OutputMixin):
    def __init__(self, native, config):
        self._native = native
        self.config = config
        self.workspace_root = Path("/ws")

    def _native_vision_enabled(self, snapshot):
        return self._native


def vision_cases() -> list:
    images = (ToolImageAttachment(media_type="image/png", data_base64="QUJD", detail="high"),)
    native_config = SimpleNamespace()
    proxy_config = SimpleNamespace(
        vision=VisionConfiguration(enabled=True), llm=SimpleNamespace(model="m")
    )
    no_llm_config = SimpleNamespace(vision=VisionConfiguration(enabled=True), llm=None)
    off_config = SimpleNamespace(vision=VisionConfiguration(enabled=False))

    scenarios = [
        ("原生视觉", True, native_config, "read_image", {"prompt": "看这张图"}, FakeVisionProxy),
        ("代理视觉", False, proxy_config, "read_image", {"prompt": "看这张图"}, FakeVisionProxy),
        ("read_image 缺少 prompt", True, native_config, "read_image", {}, FakeVisionProxy),
        ("缺少模型配置", False, no_llm_config, "read_image", {"prompt": "看这张图"}, FakeVisionProxy),
        ("视觉代理失败", False, proxy_config, "read_image", {"prompt": "看这张图"}, FailingVisionProxy),
        ("未启用视觉且非原生", False, off_config, "screenshot", {}, FakeVisionProxy),
    ]

    original_proxy = output_module.VisionModelProxy
    cases = []
    try:
        for label, native, config, tool_name, arguments, proxy in scenarios:
            output_module.VisionModelProxy = proxy
            probe = VisionProbe(native, config)
            result = ToolResult(ok=True, output="原始输出", model_images=images)
            observed = outcome(
                probe._prepare_tool_result_for_model,
                ToolCall(name=tool_name, arguments=dict(arguments)),
                result,
                prompt="任务提示",
            )
            cases.append(
                {
                    "label": label,
                    "native_vision": native,
                    "vision_enabled": bool(
                        getattr(getattr(config, "vision", None), "enabled", False)
                    ),
                    "has_llm": getattr(config, "llm", None) is not None,
                    "tool": tool_name,
                    "arguments": arguments,
                    "ok": observed["ok"],
                    "result": result_view(observed["value"][0]) if observed["ok"] else None,
                    "messages": observed["value"][1] if observed["ok"] else None,
                    "error": observed["error"],
                }
            )
    finally:
        output_module.VisionModelProxy = original_proxy
    return cases


# ---------------------------------------------------------------------- compression


def compression_cases() -> dict:
    compact_cases = []
    for label, name, output, min_chars in [
        ("bash 大输出", "bash", "x" * 2_000, 1_000),
        ("bash 小输出", "bash", "x" * 10, 1_000),
        ("空白输出", "bash", "   \n ", 1),
        ("非参与工具", "read", "x" * 2_000, 1_000),
        ("恰好达标", "grep", "x" * 1_000, 1_000),
    ]:
        configuration = SimpleNamespace(min_chars=min_chars)
        compact_cases.append(
            {
                "label": label,
                "name": name,
                "output": output,
                "chars": len(output),
                "min_chars": min_chars,
                "expected": compression_module._should_compact(
                    ToolCall(name=name),
                    ToolResult(ok=True, output=output),
                    configuration,
                ),
            }
        )

    argument_cases = []
    for label, arguments in [
        ("空参数", {}),
        ("普通参数", {"path": "a.py", "count": 2}),
        ("嵌套参数", {"a": {"b": [1, 2, "中文"]}, "flag": True}),
        ("超长参数", {"content": "x" * 800}),
        ("非字典参数", "not-a-dict"),
        ("None 参数", None),
    ]:
        argument_cases.append(
            {
                "label": label,
                "arguments": arguments,
                "expected": compression_module._arguments_summary(arguments),
            }
        )

    display_cases = []
    for label, compressed, compressed_chars, raw_chars, model in [
        ("普通压缩", "摘要", 2, 100, "small-model"),
        ("空摘要", "", 0, 10, "m"),
    ]:
        display_cases.append(
            {
                "label": label,
                "compressed": compressed,
                "compressed_chars": compressed_chars,
                "raw_chars": raw_chars,
                "model": model,
                "expected": compression_module._compacted_display(
                    compressed,
                    compressed_chars=compressed_chars,
                    raw_chars=raw_chars,
                    model=model,
                ),
            }
        )

    return {
        "should_compact": compact_cases,
        "arguments_summary": argument_cases,
        "compacted_display": display_cases,
        "constants": {
            "arguments_preview_chars": compression_module.ARGUMENTS_PREVIEW_CHARS,
            "max_parallel_compressions": compression_module.MAX_PARALLEL_COMPRESSIONS,
            "compaction_grace_seconds": compression_module.COMPACTION_GRACE_SECONDS,
            "compactable_tools": sorted(compression_module.COMPACTABLE_TOOLS),
        },
    }


# ------------------------------------------------------------------------- building


class BuildProbe(ToolBuildingMixin):
    def __init__(
        self,
        *,
        advisor_active=True,
        blacklisted=False,
        template="基础提示词。",
        mode_name="",
        mode_prompt="",
        temp_display=None,
    ):
        self.config = SimpleNamespace(advisor=SimpleNamespace(active=advisor_active))
        self._blacklisted = blacklisted
        self._system_prompt_template = template
        self._active_mode_name = mode_name
        self._active_mode_prompt = mode_prompt
        self._temp_workspace = (
            SimpleNamespace(display_path=temp_display) if temp_display else None
        )

    def _advisor_blacklisted_for_current_model(self, advisor):
        return self._blacklisted


def building_cases() -> dict:
    guidelines_cases = []
    for label, active, blacklisted in [
        ("启用且未拉黑", True, False),
        ("未启用", False, False),
        ("命中黑名单", True, True),
    ]:
        guidelines_cases.append(
            {
                "label": label,
                "active": active,
                "blacklisted": blacklisted,
                "expected": BuildProbe(
                    advisor_active=active, blacklisted=blacklisted
                )._advisor_guidelines_block(),
            }
        )

    mode_cases = []
    for label, mode in [
        ("合法名称", "Plan"),
        ("大写连字符", "  SUB-AGENT  "),
        ("带下划线", "bad_name"),
        ("带空格", "bad name"),
        ("开头连字符", "-bad"),
        ("结尾连字符", "bad-"),
        ("双连字符", "ba--d"),
        ("空名称", "   "),
        ("不存在模板", "missing-mode-xyz"),
    ]:
        probe = BuildProbe()
        observed = outcome(probe.activate_mode, mode)
        template_error = bool(
            observed["error"] and observed["error"].startswith("读取模式模板")
        )
        mode_cases.append(
            {
                "label": label,
                "mode": mode,
                "ok": observed["ok"],
                "normalized": observed["value"] if observed["ok"] else None,
                "prompt_len": len(probe._active_mode_prompt) if observed["ok"] else None,
                "prompt_sha256": digest(probe._active_mode_prompt)
                if observed["ok"]
                else None,
                "compare": "prefix" if template_error else "exact",
                "error_prefix": (
                    observed["error"].split("失败：")[0] + "失败："
                    if template_error
                    else None
                ),
                "error": observed["error"],
            }
        )

    system_prompt_cases = []
    for label, advisor_active, blacklisted, mode_name, mode_prompt in [
        ("仅基础模板", False, False, "", ""),
        ("带顾问准则", True, False, "", ""),
        ("带模式块", False, False, "plan", "模式指令。"),
        ("顾问与模式块", True, False, "plan", "  模式指令。  "),
        ("拉黑时不加顾问准则", True, True, "", ""),
    ]:
        probe = BuildProbe(
            advisor_active=advisor_active,
            blacklisted=blacklisted,
            mode_name=mode_name,
            mode_prompt=mode_prompt,
        )
        system_prompt_cases.append(
            {
                "label": label,
                "advisor_active": advisor_active,
                "blacklisted": blacklisted,
                "mode_name": mode_name,
                "mode_prompt": mode_prompt,
                "expected": probe._system_prompt(),
            }
        )

    temp_dir_cases = []
    for label, display in [("未启用临时目录", None), ("启用临时目录", ".tmp/x")]:
        probe = BuildProbe(temp_display=display)
        temp_dir_cases.append(
            {
                "label": label,
                "display": display,
                "expected": probe._agent_temp_dir_display(),
            }
        )

    return {
        "advisor_guidelines": guidelines_cases,
        "activate_mode": mode_cases,
        "system_prompt": system_prompt_cases,
        "temp_dir_display": temp_dir_cases,
    }


def main() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        tmp_root = Path(tmp).resolve()
        fixture = {
            "source": "omnicrawl/agent/controllers/",
            "shared": shared_cases(),
            "undo": undo_cases(tmp_root),
            "workspace": workspace_cases(tmp_root),
            "memory": memory_cases(tmp_root),
            "output": output_cases(),
            "compression": compression_cases(),
            "building": building_cases(),
        }

    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + chr(10), encoding="utf-8"
    )
    counts = []
    for section, value in fixture.items():
        if not isinstance(value, dict):
            continue
        total = sum(len(item) for item in value.values() if isinstance(item, list))
        counts.append("%s=%d" % (section, total))
    print("已写入 %s：%s" % (FIXTURE_PATH.relative_to(ROOT), ", ".join(counts)))


if __name__ == "__main__":
    main()
