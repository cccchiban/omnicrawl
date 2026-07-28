"""定义式 SubAgent 的批量协调、权限收窄与安全事件出口。"""

from __future__ import annotations

import json
import logging
import threading
import time
import uuid
from concurrent.futures import FIRST_COMPLETED, Future, ThreadPoolExecutor, wait
from dataclasses import dataclass, field, replace
from typing import Any, Callable, Mapping, Sequence

from ..execution import AgentLoopBudgetExceeded
from ..types import ToolDefinition, ToolResult
from ...config.llm import LLMError
from ...config.subagents import SubAgentConfig
from ...llm.errors import ModelError, map_openai_exception
from ...state.session_artifacts import redact_sensitive_text
from .approval import (
    ApprovalBroker,
    SubAgentApprovalOrigin,
    SubAgentApprovalScope,
    activate_subagent_approval_scope,
)
from .definitions import AgentDefinition, AgentDefinitionRegistry
from .execution import SubAgentExecutionContext
from .read_only_commands import (
    READ_ONLY_COMMAND_TOOL_NAMES,
    wrap_read_only_command_tool,
)
from .tasks import SubAgentTaskManager, SubAgentTaskSpec
from .verify import VERIFY_COMMAND_TOOL_NAME


LOGGER = logging.getLogger(__name__)


def _failure_root_exception(exc: BaseException) -> BaseException:
    """沿异常因果链找到最具体的底层异常，避免公开包装器文本。"""

    current = exc
    seen: set[int] = set()
    for _ in range(8):
        if id(current) in seen:
            break
        seen.add(id(current))
        cause = current.__cause__ or current.__context__
        if cause is None:
            break
        current = cause
    return current


def _build_failure_diagnostics(
    exc: BaseException,
    *,
    model: str = "",
    wire_model: str = "",
) -> dict[str, Any]:
    """构建可持久化的 SubAgent 失败分类，不保存原始异常文本。"""

    root = _failure_root_exception(exc)
    diagnostic: dict[str, Any] = {
        "category": "UNKNOWN",
        "exception_type": type(root).__name__,
        "retryable": False,
    }
    if isinstance(root, ModelError):
        diagnostic["category"] = (
            root.code.value if hasattr(root.code, "value") else str(root.code)
        )
        diagnostic["retryable"] = bool(root.retryable)
        if root.provider:
            diagnostic["provider"] = redact_sensitive_text(root.provider)
        if root.status_code is not None:
            diagnostic["status_code"] = int(root.status_code)
    elif isinstance(root, LLMError):
        diagnostic["category"] = "CONFIGURATION_ERROR"
    elif isinstance(root, Exception):
        mapped = map_openai_exception(root)
        diagnostic["category"] = (
            mapped.code.value if hasattr(mapped.code, "value") else str(mapped.code)
        )
        diagnostic["retryable"] = bool(mapped.retryable)
        if mapped.code.value != "UNKNOWN" and mapped.provider:
            diagnostic["provider"] = redact_sensitive_text(mapped.provider)
        if mapped.code.value != "UNKNOWN" and mapped.status_code is not None:
            diagnostic["status_code"] = int(mapped.status_code)
    if model:
        safe_selection = redact_sensitive_text(model)
        diagnostic["model"] = safe_selection
        diagnostic["model_selection"] = safe_selection
    if wire_model:
        diagnostic["wire_model"] = redact_sensitive_text(wire_model)
    return diagnostic


READ_ONLY_TOOL_NAMES = frozenset(
    {
        "list_files",
        "find_files",
        "read_file",
        "search_text",
        "memory_search",
        "memory_read",
        "memory_expand_related",
        "project_memory_search",
        "project_memory_read",
        "project_memory_expand_related",
        "session_memory_search",
        "session_memory_read",
        "session_memory_expand_related",
        "user_memory_search",
        "user_memory_read",
        "user_memory_expand_related",
    }
)
# delegated-read-only 继承父 Host 已注册的 MCP、Skill、桌面、浏览器和其他外部
# 能力，只硬拒绝已知本地写入口与递归调度。命令工具另加运行前只读判定，不能
# 因为进入动态继承集合就绕过工作区文件保护。
READ_ONLY_BLOCKED_TOOL_NAMES = frozenset(
    {
        "write_file",
        "replace_text",
        "memory_write",
        "project_memory_write",
        "session_memory_write",
        "user_memory_write",
        "subagent",
    }
)
# verify profile 只能在既有只读能力上追加一个固定检查入口；绝不能把原始
# bash/powershell/monitor 或其他命令工具加入这里。
VERIFY_TOOL_NAMES = READ_ONLY_TOOL_NAMES | frozenset({VERIFY_COMMAND_TOOL_NAME})
# standard 写 Agent 允许的工具集合：只读 + 受限写入/命令；仍由 Host risk 策略逐次审批。
STANDARD_WRITE_TOOL_NAMES = frozenset({"write_file", "replace_text", "bash", "powershell"})
STANDARD_TOOL_NAMES = READ_ONLY_TOOL_NAMES | STANDARD_WRITE_TOOL_NAMES
_TASK_FIELDS = {"description", "prompt", "subagent_type", "context", "model"}
_TOP_LEVEL_FIELDS = {
    "action",
    "tasks",
    "max_concurrency",
    "fail_fast",
    "task_id",
    "batch_id",
    "branch",
    "strategy",
    "cleanup",
    "remove_branch",
}
_WORKTREE_CONTROL_ACTIONS = frozenset(
    {"apply_worktree", "discard_worktree", "list_worktrees"}
)


@dataclass(frozen=True)
class SubAgentExecutionResult:
    """Host 执行一个隔离子循环后返回给 Coordinator 的有界结果。"""

    final_text: str
    model_turns: int
    tool_calls: int
    input_tokens: int = 0
    output_tokens: int = 0
    cached_input_tokens: int = 0
    # worktree / 实现产物摘要，供 result_processor 提升为公开 artifacts。
    artifacts: tuple[str, ...] = ()


@dataclass(frozen=True)
class SubAgentPublicResult:
    """写入父上下文和公开事件前完成安全投影的子任务结果。"""

    summary: str
    artifacts: tuple[dict, ...] = ()


@dataclass(frozen=True)
class _PreparedTask:
    """一次批量请求中已经完成权限收窄的不可变任务快照。"""

    batch_id: str
    task_id: str
    description: str
    prompt: str
    agent_type: str
    definition: AgentDefinition
    tools: Mapping[str, ToolDefinition]
    execution_context: SubAgentExecutionContext


class SubAgentCancelled(RuntimeError):
    """Coordinator 主动取消同步子任务时使用的稳定内部异常。"""


@dataclass
class _ActiveBatch:
    """同步批次的取消令牌与真实 worker 存活状态。"""

    batch_id: str
    tasks: tuple[_PreparedTask, ...]
    cancel_event: threading.Event = field(default_factory=threading.Event)
    done_event: threading.Event = field(default_factory=threading.Event)
    cancel_reason: str = ""
    cancel_dispatch_started: bool = False
    futures: dict[Future[dict], int] = field(default_factory=dict)
    finished_indexes: set[int] = field(default_factory=set)
    lock: threading.RLock = field(default_factory=threading.RLock)

    def register_future(self, future: Future[dict], index: int) -> bool:
        """登记 Future，并返回登记时批次是否已经进入取消态。"""

        with self.lock:
            self.futures[future] = index
            return self.cancel_event.is_set()

    def begin_cancel(self, reason: str) -> bool:
        """立即设置取消令牌，并告知调用方是否需启动一次异步收尾。"""

        with self.lock:
            if not self.cancel_reason:
                self.cancel_reason = reason
            self.cancel_event.set()
            if self.cancel_dispatch_started:
                return False
            self.cancel_dispatch_started = True
            return True

    def mark_finished(self, index: int) -> bool:
        """记录一个任务不再占用 worker；全部结束时置位 done_event。"""

        with self.lock:
            self.finished_indexes.add(index)
            finished = len(self.finished_indexes) == len(self.tasks)
            if finished:
                self.done_event.set()
            return finished

    def untracked_indexes(self) -> tuple[int, ...]:
        """返回尚未提交为 Future 的任务索引，用于异常/取消收尾。"""

        with self.lock:
            tracked = set(self.futures.values()) | self.finished_indexes
            return tuple(index for index in range(len(self.tasks)) if index not in tracked)


class SubAgentCoordinator:
    """验证模型请求、收窄工具权限并执行受限任务批次。

    支持每批至多 4 个 ``fresh`` 任务，以及显式配置后的 ``fork`` 任务。默认 profile
    只读；显式启用的 verify profile 仅可追加 Host 固定的 ``verify_command``。
    Fork 和模型快照均在任务进入线程池前由 Host 冻结。批次任务数与模型请求并发分别受 Host
    配置控制；Coordinator 只负责前者，后者由 ``LocalToolAgent`` 在实际请求边界
    使用信号量限制。批次结果始终按输入顺序返回，避免完成顺序改变模型语义。
    """

    def __init__(
        self,
        *,
        config: SubAgentConfig,
        registry: AgentDefinitionRegistry,
        tools_provider: Callable[[], Mapping[str, ToolDefinition]],
        execute_task: Callable[
            [
                AgentDefinition,
                Mapping[str, ToolDefinition],
                str,
                str,
                Callable[[], None],
                SubAgentExecutionContext,
            ],
            SubAgentExecutionResult,
        ],
        prepare_execution: (
            Callable[[AgentDefinition, str, str], SubAgentExecutionContext] | None
        ) = None,
        event_sink: Callable[[str, dict], None] | None = None,
        result_processor: (
            Callable[[str, str, str, str], SubAgentPublicResult] | None
        ) = None,
        task_manager: SubAgentTaskManager | None = None,
        owner_id: str = "local-agent",
        session_id_provider: Callable[[], str] | None = None,
        observer_provider: Callable[[], Callable[[str, dict], None] | None] | None = None,
        approval_broker: ApprovalBroker | None = None,
        verify_tools_provider: Callable[[], Mapping[str, ToolDefinition]] | None = None,
        apply_worktree: Callable[..., str] | None = None,
        discard_worktree: Callable[..., str] | None = None,
        list_worktrees: Callable[[], list[dict[str, Any]]] | None = None,
    ) -> None:
        self.config = config
        self.registry = registry
        self._tools_provider = tools_provider
        self._execute_task = execute_task
        # Host 在任务进入线程池前冻结 Fork 消息和模型选择。Coordinator 只保存
        # 私有执行上下文，不把它写入事件、Session、artifact 或公开结果。
        self._prepare_execution = prepare_execution or (
            lambda _definition, context, _model: SubAgentExecutionContext(context=context)
        )
        # ``execute_task`` 是此前已存在的内部注入点。没有显式 Host 准备器的
        # 旧测试/嵌入方仍接收五个参数；真正 Host 传入准备器后才接收第六个私有
        # 执行上下文。这避免为了新增 Fork 破坏既有 Phase 1/2 契约夹具。
        self._execute_task_uses_execution_context = prepare_execution is not None
        self._event_sink = event_sink or (lambda _event, _payload: None)
        self._result_processor = result_processor or self._default_result_processor
        self._terminal_lock = threading.Lock()
        self._terminal_task_ids: set[str] = set()
        self._lifecycle_lock = threading.RLock()
        self._active_batches: dict[str, _ActiveBatch] = {}
        self._accepting = True
        self._closed = False
        self._resume_when_idle = False
        self._idle_callbacks: list[Callable[[], None]] = []
        self._cancel_check_provider: Callable[[], Callable[[], None] | None] = (
            lambda: None
        )
        self._task_manager = task_manager or SubAgentTaskManager(
            retention_seconds=max(60.0, config.task_retention_minutes * 60),
            max_workers=config.max_concurrency,
        )
        self._owner_id = owner_id
        self._session_id_provider = session_id_provider or (lambda: "")
        self._observer_provider = observer_provider or (lambda: None)
        # Broker 由父 Agent 创建并跨定义刷新复用；Coordinator 只在任务执行和
        # 生命周期取消边界绑定/撤销任务来源，避免并发 worker 共享可变上下文。
        self._approval_broker = approval_broker
        # verify_command 不进入父 Agent 的普通工具表。由 Host 在准备任务时提供
        # 固定检查工具，Coordinator 仍会按定义白名单与 profile 上限再次收窄。
        self._verify_tools_provider = verify_tools_provider or (lambda: {})
        # worktree 控制面由 Host 注入；Coordinator 只做参数校验与 JSON 投影。
        self._apply_worktree = apply_worktree
        self._discard_worktree = discard_worktree
        self._list_worktrees = list_worktrees
        # shared 写任务单写锁：同一时刻只允许一个 shared 写 Agent 占用主工作区。
        self._shared_writer_lock = threading.Lock()
        self._shared_writer_holder: str | None = None

    def set_cancel_check_provider(
        self,
        provider: Callable[[], Callable[[], None] | None],
    ) -> None:
        """注入父 Run 取消检查提供器；每个批次创建时冻结一次引用。"""

        self._cancel_check_provider = provider

    def cancel_and_wait(
        self,
        *,
        reason: str,
        timeout_seconds: float,
        permanent: bool,
    ) -> bool:
        """停止接收新批次、取消活动任务并按单一 deadline 有界等待。"""

        with self._lifecycle_lock:
            self._accepting = False
            if permanent:
                self._closed = True
                self._resume_when_idle = False
            batches = tuple(self._active_batches.values())
        deadline = time.monotonic() + max(0.0, float(timeout_seconds))
        for batch in batches:
            self._request_batch_cancel(batch, reason)
        if self._approval_broker is not None:
            self._approval_broker.cancel_all()
        # close/workspace switch 是 Agent 级生命周期边界，必须覆盖该实例下
        # 所有 Session 的后台任务，不能只取消当前恢复会话。
        self._task_manager.cancel_all(owner_id=self._owner_id, session_id=None)

        for batch in batches:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not batch.done_event.wait(timeout=remaining):
                return False
        remaining = deadline - time.monotonic()
        if not self._task_manager.wait_for_idle(
            owner_id=self._owner_id,
            session_id=None,
            timeout=max(0.0, remaining),
        ):
            return False
        if permanent:
            self._task_manager.close(owner_id=self._owner_id)
            if self._approval_broker is not None:
                self._approval_broker.close()
        return True

    def resume_accepting_when_idle(self) -> None:
        """工作区切换失败后，待同步与后台任务真实退出再恢复接单。"""

        def resume_after_background_idle() -> None:
            with self._lifecycle_lock:
                if self._closed:
                    return
                if self._active_batches:
                    self._resume_when_idle = True
                else:
                    self._accepting = True
                    self._resume_when_idle = False

        self._task_manager.call_when_idle(
            owner_id=self._owner_id,
            session_id=None,
            callback=resume_after_background_idle,
        )

    def call_when_idle(self, callback: Callable[[], None]) -> None:
        """同步批次和后台 worker 都退出后调用一次资源关闭回调。"""

        def wait_for_background() -> None:
            self._task_manager.call_when_idle(
                owner_id=self._owner_id,
                session_id=None,
                callback=callback,
            )

        with self._lifecycle_lock:
            if self._active_batches:
                self._idle_callbacks.append(wait_for_background)
                return
        wait_for_background()

    def drain_notifications(self, *, session_id: str) -> list[dict]:
        """消费当前 owner/session 的一次性终态通知。"""

        return self._task_manager.drain_notifications(
            owner_id=self._owner_id,
            session_id=session_id,
        )

    def available_agent_types(self) -> tuple[str, ...]:
        """返回当前配置下可以实际创建任务的角色名称。"""

        return tuple(
            definition.name
            for definition in self.registry.list_all()
            if not self._unsupported_definition_reason(definition)
        )

    def list_tasks(self) -> list[dict[str, Any]]:
        """返回当前 Session 的后台任务安全快照。

        控制面不接受调用方传入的 owner 或 session，避免 HTTP/TUI 入口绕过
        Coordinator 创建时冻结的隔离范围。
        """

        return self._task_manager.list(
            owner_id=self._owner_id,
            session_id=self._session_id_provider() or "",
        )

    def import_recovered_snapshots(
        self,
        snapshots: Sequence[Mapping[str, Any]],
    ) -> int:
        """导入当前 Session 的跨进程恢复快照；owner/session 由 Coordinator 固定。"""

        return self._task_manager.import_recovered_snapshots(
            owner_id=self._owner_id,
            session_id=self._session_id_provider() or "",
            snapshots=snapshots,
        )

    def get_task(self, task_id: str) -> dict[str, Any] | None:
        """读取当前 Session 内单个后台任务；其他 Session 统一不可见。"""

        return self._task_manager.get(
            task_id,
            owner_id=self._owner_id,
            session_id=self._session_id_provider() or "",
        )

    def cancel_task(
        self,
        *,
        task_id: str | None = None,
        batch_id: str | None = None,
    ) -> dict[str, Any]:
        """取消当前 Session 内的任务或批次，不跨越 owner/session 边界。"""

        result = self._task_manager.cancel(
            owner_id=self._owner_id,
            session_id=self._session_id_provider() or "",
            task_id=task_id,
            batch_id=batch_id,
        )
        if bool(result.get("ok")) and self._approval_broker is not None:
            if isinstance(task_id, str) and task_id:
                self._approval_broker.cancel_task(task_id)
            if isinstance(batch_id, str) and batch_id:
                self._approval_broker.cancel_batch(batch_id)
        return result

    def run(self, arguments: dict) -> ToolResult:
        if not self.config.enabled:
            return self._top_level_error(
                "SUBAGENT_DISABLED",
                "SubAgent 功能未启用。请在配置中显式设置 subagents.enabled=true。",
            )

        action = arguments.get("action") if isinstance(arguments, dict) else None
        if action in {"list", "get", "cancel"}:
            return self._query_action(arguments)
        if action in _WORKTREE_CONTROL_ACTIONS:
            return self._worktree_control_action(arguments if isinstance(arguments, dict) else {})
        validation_error = self._validate_arguments(arguments)
        if validation_error is not None:
            code, message = validation_error
            return self._top_level_error(code, message)

        batch_id = f"batch-{uuid.uuid4().hex[:12]}"
        prepared_or_error = self._prepare_tasks(arguments["tasks"], batch_id=batch_id)
        if isinstance(prepared_or_error, ToolResult):
            return prepared_or_error
        prepared_tasks = prepared_or_error
        if arguments.get("action") == "spawn":
            return self._spawn_background(
                prepared_tasks,
                batch_id=batch_id,
                arguments=arguments,
                parent_cancel_check=self._cancel_check_provider(),
            )
        batch = _ActiveBatch(batch_id=batch_id, tasks=tuple(prepared_tasks))
        with self._lifecycle_lock:
            if self._closed or not self._accepting:
                return self._top_level_error(
                    "SUBAGENT_CANCELLED",
                    "SubAgent 正在关闭或切换工作区，暂不接受新任务。",
                )
            self._active_batches[batch_id] = batch

        requested_concurrency = arguments.get(
            "max_concurrency",
            self.config.max_concurrency,
        )
        max_concurrency = min(
            requested_concurrency,
            self.config.max_concurrency,
            len(prepared_tasks),
        )
        fail_fast = arguments.get("fail_fast", False)
        parent_cancel_check = self._cancel_check_provider()
        cancel_check = self._combined_cancel_check(batch, parent_cancel_check)

        try:
            self._emit(
                "subagent.batch.created",
                {
                    "batch_id": batch_id,
                    "status": "queued",
                    "task_count": len(prepared_tasks),
                    "max_concurrency": max_concurrency,
                    "fail_fast": fail_fast,
                },
            )
            for task in prepared_tasks:
                self._emit_task_event(task, "queued")

            if fail_fast:
                results = self._run_fail_fast(
                    prepared_tasks,
                    batch=batch,
                    cancel_check=cancel_check,
                    max_concurrency=max_concurrency,
                )
            else:
                results = self._run_all(
                    prepared_tasks,
                    batch=batch,
                    cancel_check=cancel_check,
                    max_concurrency=max_concurrency,
                )

            statuses = [item["status"] for item in results]
            completed_count = statuses.count("completed")
            if completed_count == len(results):
                batch_status = "completed"
            elif completed_count:
                batch_status = "partial"
            else:
                batch_status = "failed"
            return self._json_result(
                batch_status == "completed",
                {
                    "batch_id": batch_id,
                    "status": batch_status,
                    "results": results,
                },
            )
        except BaseException:
            self._request_batch_cancel(batch, "父任务已取消子任务。")
            self._cancel_unsubmitted_tasks(batch)
            raise


    def _worktree_control_action(self, arguments: dict) -> ToolResult:
        """父 Agent 显式 apply / discard / list worktree 会话。"""

        action = arguments.get("action")
        allowed = {
            "action",
            "task_id",
            "batch_id",
            "branch",
            "strategy",
            "cleanup",
            "remove_branch",
        }
        unknown = set(arguments) - allowed
        if unknown:
            return self._top_level_error(
                "SUBAGENT_PERMISSION_DENIED",
                f"worktree 控制动作包含不允许的字段：{', '.join(sorted(unknown))}",
            )

        if action == "list_worktrees":
            if self._list_worktrees is None:
                return self._top_level_error(
                    "SUBAGENT_PERMISSION_DENIED",
                    "当前 Host 未注入 list_worktrees 回调。",
                )
            try:
                sessions = list(self._list_worktrees() or [])
            except Exception as exc:  # noqa: BLE001 - 投影为稳定工具错误
                return self._top_level_error(
                    "SUBAGENT_MODEL_ERROR",
                    f"列出 worktree 会话失败：{exc}",
                )
            return self._json_result(True, {"worktrees": sessions})

        key = arguments.get("task_id") or arguments.get("branch")
        if not isinstance(key, str) or not key.strip():
            return self._top_level_error(
                "AGENT_DEFINITION_INVALID",
                f"{action} 需要 task_id 或 branch。",
            )
        key = key.strip()

        if action == "apply_worktree":
            if self._apply_worktree is None:
                return self._top_level_error(
                    "SUBAGENT_PERMISSION_DENIED",
                    "当前 Host 未注入 apply_worktree 回调。",
                )
            strategy = arguments.get("strategy", "checkout")
            if strategy not in {"checkout", "merge"}:
                return self._top_level_error(
                    "AGENT_DEFINITION_INVALID",
                    "strategy 仅支持 checkout 或 merge。",
                )
            cleanup = bool(arguments.get("cleanup", False))
            try:
                message = self._apply_worktree(key, strategy=strategy, cleanup=cleanup)
            except Exception as exc:  # noqa: BLE001
                return self._top_level_error(
                    "SUBAGENT_MODEL_ERROR",
                    f"应用 worktree 失败：{exc}",
                )
            return self._json_result(
                True,
                {
                    "action": "apply_worktree",
                    "key": key,
                    "strategy": strategy,
                    "cleanup": cleanup,
                    "message": str(message),
                },
            )

        # discard_worktree
        if self._discard_worktree is None:
            return self._top_level_error(
                "SUBAGENT_PERMISSION_DENIED",
                "当前 Host 未注入 discard_worktree 回调。",
            )
        remove_branch = bool(arguments.get("remove_branch", True))
        try:
            message = self._discard_worktree(key, remove_branch=remove_branch)
        except Exception as exc:  # noqa: BLE001
            return self._top_level_error(
                "SUBAGENT_MODEL_ERROR",
                f"清理 worktree 失败：{exc}",
            )
        return self._json_result(
            True,
            {
                "action": "discard_worktree",
                "key": key,
                "remove_branch": remove_branch,
                "message": str(message),
            },
        )

    def _requires_shared_writer_lock(self, task: _PreparedTask) -> bool:
        """shared isolation + standard 写权限才需要主工作区单写锁。"""

        isolation = getattr(task.execution_context, "isolation", None) or getattr(
            task.definition, "isolation", "shared"
        )
        if isolation != "shared":
            return False
        if task.definition.permission_mode != "standard":
            return False
        # tools 表中只要包含任一写工具，就视为写任务。
        return bool(STANDARD_WRITE_TOOL_NAMES.intersection(task.tools))

    def _acquire_shared_writer(self, task_id: str) -> None:
        """阻塞获取 shared 写锁；同一 task 重入安全。"""

        with self._lifecycle_lock:
            if self._shared_writer_holder == task_id:
                return
        # 阻塞等待；取消检查由执行路径 cancel_check 负责。
        self._shared_writer_lock.acquire()
        with self._lifecycle_lock:
            self._shared_writer_holder = task_id

    def _release_shared_writer(self, task_id: str) -> None:
        """释放当前 task 持有的 shared 写锁。"""

        with self._lifecycle_lock:
            if self._shared_writer_holder != task_id:
                return
            self._shared_writer_holder = None
        try:
            self._shared_writer_lock.release()
        except RuntimeError:
            # 未持有时忽略，避免 teardown 二次释放放大故障。
            pass

    def _query_action(self, arguments: dict) -> ToolResult:
        """查询/取消仅在当前 Coordinator owner/session 范围内生效。"""

        action = arguments.get("action")
        allowed = {"action", "task_id", "batch_id"}
        if set(arguments) - allowed:
            return self._top_level_error("SUBAGENT_PERMISSION_DENIED", "查询动作包含不允许的字段。")
        if action == "list":
            return self._json_result(True, {"tasks": self.list_tasks()})
        task_id = arguments.get("task_id")
        batch_id = arguments.get("batch_id")
        if action == "get":
            if not isinstance(task_id, str) or not task_id:
                return self._top_level_error("AGENT_DEFINITION_INVALID", "get 需要 task_id。")
            task = self.get_task(task_id)
            if task is None:
                return self._top_level_error("SUBAGENT_NOT_FOUND", "未找到当前 Agent 的任务。")
            return self._json_result(True, {"task": task})
        if not isinstance(task_id, str) and not isinstance(batch_id, str):
            return self._top_level_error(
                "AGENT_DEFINITION_INVALID",
                "cancel 需要 task_id 或 batch_id。",
            )
        result = self.cancel_task(task_id=task_id, batch_id=batch_id)
        return self._json_result(bool(result.get("ok")), result)

    def _spawn_background(
        self,
        tasks: list[_PreparedTask],
        *,
        batch_id: str,
        arguments: dict,
        parent_cancel_check: Callable[[], None] | None,
    ) -> ToolResult:
        if not self.config.allow_background:
            return self._top_level_error(
                "SUBAGENT_BACKGROUND_DISABLED",
                "后台 SubAgent 默认关闭，请在配置中显式设置 allow_background=true。",
            )
        if arguments.get("fail_fast", False):
            return self._top_level_error(
                "SUBAGENT_PERMISSION_DENIED",
                "后台批次暂不支持 fail_fast=true。",
            )

        session_id = self._session_id_provider() or ""
        specs = tuple(
            SubAgentTaskSpec(
                task.task_id,
                task.description,
                task.agent_type,
                batch_id,
            )
            for task in tasks
        )
        frozen_observer = self._observer_provider()
        requested_concurrency = arguments.get(
            "max_concurrency",
            self.config.max_concurrency,
        )
        batch_semaphore = threading.BoundedSemaphore(
            min(requested_concurrency, self.config.max_concurrency, len(tasks))
        )
        try:
            # 与 cancel_and_wait 共用生命周期锁，避免工作区切换/关闭已开始后
            # 仍有新后台任务穿过取消快照并绑定即将拆除的共享资源。
            with self._lifecycle_lock:
                if self._closed or not self._accepting:
                    return self._top_level_error(
                        "SUBAGENT_CANCELLED",
                        "SubAgent 正在关闭或切换工作区，暂不接受新任务。",
                    )
                self._emit_background_event(
                    "subagent.batch.created",
                    {
                        "batch_id": batch_id,
                        "status": "queued",
                        "task_count": len(tasks),
                        "max_concurrency": min(
                            requested_concurrency,
                            self.config.max_concurrency,
                            len(tasks),
                        ),
                    },
                    frozen_observer=frozen_observer,
                )
                accepted = self._task_manager.spawn(
                    owner_id=self._owner_id,
                    session_id=session_id,
                    specs=specs,
                    runner=lambda spec, cancel_event: self._run_background_task(
                        tasks,
                        spec,
                        cancel_event,
                        parent_cancel_check=parent_cancel_check,
                        batch_semaphore=batch_semaphore,
                    ),
                    observer=lambda event, payload: self._emit_background_event(
                        event,
                        payload,
                        frozen_observer=frozen_observer,
                    ),
                )
        except RuntimeError as exc:
            return self._top_level_error("SUBAGENT_CANCELLED", str(exc))
        return self._json_result(True, accepted)

    def _run_background_task(
        self,
        tasks: list[_PreparedTask],
        spec: SubAgentTaskSpec,
        cancel_event: threading.Event,
        *,
        parent_cancel_check: Callable[[], None] | None,
        batch_semaphore: threading.BoundedSemaphore,
    ) -> Mapping[str, object]:
        task = next(item for item in tasks if item.task_id == spec.task_id)

        def cancel_check() -> None:
            if cancel_event.is_set():
                raise SubAgentCancelled("后台任务已取消。")
            # 父 Run 的取消检查必须在 spawn 时冻结。若动态读取 Agent 当前回合，
            # 后续无关用户回合的取消会错误级联到旧后台任务。
            if parent_cancel_check is not None:
                parent_cancel_check()

        while not batch_semaphore.acquire(timeout=0.1):
            cancel_check()
        try:
            cancel_check()
            return self._execute_prepared_task(
                task,
                cancel_check,
                emit_events=False,
            )
        finally:
            batch_semaphore.release()

    def _emit_background_event(
        self,
        event_name: str,
        payload: dict,
        *,
        frozen_observer: Callable[[str, dict], None] | None = None,
    ) -> None:
        # Manager 已冻结 observer；观察者失败只记录日志，不能改写任务状态。
        normalized = "subagent.task.started" if event_name == "subagent.task.running" else event_name
        try:
            self._emit(normalized, payload)
        except Exception as exc:
            LOGGER.warning("SubAgent background event sink failed: %s", type(exc).__name__)
        current_observer = self._observer_provider()
        if frozen_observer is not None and frozen_observer is not current_observer:
            try:
                frozen_observer(normalized, dict(payload))
            except Exception as exc:
                LOGGER.warning("SubAgent frozen observer failed: %s", type(exc).__name__)

    def _prepare_tasks(
        self,
        tasks: list[dict],
        *,
        batch_id: str,
    ) -> list[_PreparedTask] | ToolResult:
        prepared: list[_PreparedTask] = []
        for task in tasks:
            description = task["description"].strip()
            prompt = task["prompt"].strip()
            agent_type = task["subagent_type"].strip().casefold()
            definition = self.registry.get(agent_type)
            if definition is None:
                available = ", ".join(item.name for item in self.registry.list_all()) or "无"
                return self._top_level_error(
                    "AGENT_TYPE_NOT_FOUND",
                    f"未找到 Agent 定义：{agent_type}。当前可用：{available}。",
                )

            unsupported = self._unsupported_definition_reason(definition)
            if unsupported:
                return self._top_level_error("AGENT_DEFINITION_INVALID", unsupported)

            context = task.get("context", "fresh")
            model = task.get("model", "")
            try:
                # Fork 上下文和模型均必须在排队前冻结。特别是后台任务不能等到
                # worker 启动后再读取父历史或当前模型，否则 /model、下一回合或
                # 工作区状态变化会导致同一 task_id 使用不同输入。
                execution_context = self._prepare_execution(
                    definition,
                    context,
                    model,
                )
            except Exception as exc:  # noqa: BLE001 - Host 准备错误需收敛为安全公开结果
                LOGGER.warning(
                    "SubAgent execution context preparation failed: %s",
                    type(exc).__name__,
                )
                return self._top_level_error(
                    "SUBAGENT_MODEL_ERROR",
                    "子任务模型或 Fork 上下文无法准备。",
                )
            prepared.append(
                _PreparedTask(
                    batch_id=batch_id,
                    task_id=f"task-{uuid.uuid4().hex[:12]}",
                    description=description,
                    prompt=prompt,
                    agent_type=agent_type,
                    definition=definition,
                    tools=self._tools_for_definition(definition),
                    execution_context=execution_context,
                )
            )
        return prepared

    def _run_all(
        self,
        tasks: list[_PreparedTask],
        *,
        batch: _ActiveBatch,
        cancel_check: Callable[[], None],
        max_concurrency: int,
    ) -> list[dict]:
        results: list[dict | None] = [None] * len(tasks)
        executor = ThreadPoolExecutor(
            max_workers=max_concurrency,
            thread_name_prefix="omnicrawl-subagent",
        )
        pending: set[Future[dict]] = set()
        try:
            for index, task in enumerate(tasks):
                cancel_check()
                future = executor.submit(self._execute_prepared_task, task, cancel_check)
                self._track_future(batch, future, index)
                pending.add(future)

            while pending:
                cancel_check()
                completed, pending = wait(
                    pending,
                    timeout=0.05,
                    return_when=FIRST_COMPLETED,
                )
                for future in completed:
                    index = batch.futures[future]
                    results[index] = future.result()
        except BaseException:
            self._request_batch_cancel(batch, "父任务已取消子任务。")
            raise
        finally:
            # 外部取消后已运行线程只能合作式退出；这里禁止无界等待，真实存活
            # 状态由 Future done callback 继续维护，资源层据此决定是否可安全拆除。
            executor.shutdown(wait=False, cancel_futures=True)
        return [item for item in results if item is not None]

    def _run_fail_fast(
        self,
        tasks: list[_PreparedTask],
        *,
        batch: _ActiveBatch,
        cancel_check: Callable[[], None],
        max_concurrency: int,
    ) -> list[dict]:
        results: list[dict | None] = [None] * len(tasks)
        next_index = 0
        stopped = False
        executor = ThreadPoolExecutor(
            max_workers=max_concurrency,
            thread_name_prefix="omnicrawl-subagent",
        )
        running: dict[Future[dict], int] = {}

        def submit_available() -> None:
            nonlocal next_index
            while not stopped and next_index < len(tasks) and len(running) < max_concurrency:
                cancel_check()
                index = next_index
                future = executor.submit(
                    self._execute_prepared_task,
                    tasks[index],
                    cancel_check,
                )
                self._track_future(batch, future, index)
                running[future] = index
                next_index += 1

        try:
            submit_available()
            while running:
                cancel_check()
                completed, _pending = wait(
                    tuple(running),
                    timeout=0.05,
                    return_when=FIRST_COMPLETED,
                )
                for future in completed:
                    index = running.pop(future)
                    result = future.result()
                    results[index] = result
                    if result["status"] != "completed":
                        stopped = True
                submit_available()

            if stopped:
                for index in range(next_index, len(tasks)):
                    results[index] = self._cancelled_payload(
                        tasks[index],
                        "fail_fast 已在前序任务失败后停止调度该任务。",
                    )
                    try:
                        self._emit_terminal_event(tasks[index], results[index])
                    except BaseException as exc:
                        LOGGER.warning(
                            "SubAgent fail-fast cancellation event failed: %s",
                            type(exc).__name__,
                        )
                    finally:
                        batch.mark_finished(index)
                        self._finalize_batch_if_done(batch)
        except BaseException:
            self._request_batch_cancel(batch, "父任务已取消子任务。")
            self._cancel_unsubmitted_tasks(batch)
            raise
        finally:
            executor.shutdown(wait=False, cancel_futures=True)
        return [item for item in results if item is not None]

    def _call_execute_task(
        self,
        task: _PreparedTask,
        cancel_check: Callable[[], None],
    ) -> SubAgentExecutionResult:
        """在兼容旧夹具的同时把 Phase 3 私有快照交给 Host。"""

        # shared 写任务必须串行占用主工作区；worktree 任务互不抢锁。
        needs_writer_lock = self._requires_shared_writer_lock(task)
        if needs_writer_lock:
            self._acquire_shared_writer(task.task_id)
        try:
            if self._execute_task_uses_execution_context:
                return self._execute_task(
                    task.definition,
                    task.tools,
                    task.description,
                    task.prompt,
                    cancel_check,
                    task.execution_context,
                )
            return self._execute_task(
                task.definition,
                task.tools,
                task.description,
                task.prompt,
                cancel_check,
            )
        finally:
            if needs_writer_lock:
                self._release_shared_writer(task.task_id)

    def _execute_prepared_task(
        self,
        task: _PreparedTask,
        cancel_check: Callable[[], None],
        *,
        emit_events: bool = True,
    ) -> dict:
        try:
            cancel_check()
            if emit_events:
                self._emit_task_event(task, "started")
            approval_broker = self._approval_broker
            if approval_broker is None:
                execution = self._call_execute_task(task, cancel_check)
            else:
                approval_scope = SubAgentApprovalScope(
                    broker=approval_broker,
                    origin=SubAgentApprovalOrigin(
                        batch_id=task.batch_id,
                        task_id=task.task_id,
                        agent_label=task.agent_type,
                        description=task.description,
                        permission_mode=task.definition.permission_mode,
                    ),
                )
                try:
                    # ContextVar 只在当前 worker 有效；子执行中的并行任务不会
                    # 覆盖其他子任务的 task_id/batch_id，且定义刷新不影响旧任务。
                    with activate_subagent_approval_scope(approval_scope):
                        execution = self._call_execute_task(task, cancel_check)
                finally:
                    # 若确认处理器在任务取消后迟到返回，其批准不能让已终态任务
                    # 继续；活跃 UI 槽位会保留到处理器自然返回，维持单确认不变量。
                    approval_broker.cancel_task(task.task_id)
            cancel_check()
        except KeyboardInterrupt as exc:
            result = self._cancelled_payload(
                task,
                str(exc) or "父任务已取消子任务。",
            )
            if emit_events:
                self._emit_cancellation_event_safely(task, result)
            raise
        except AgentLoopBudgetExceeded as exc:
            result = self._failure_payload(
                task,
                code="SUBAGENT_LIMIT_EXCEEDED",
                message=str(exc),
            )
            if emit_events:
                self._emit_terminal_event(task, result)
            return result
        except Exception as exc:  # noqa: BLE001 - 转成公开、可序列化的子任务错误边界
            if self._is_cancellation(exc):
                result = self._cancelled_payload(
                    task,
                    str(exc) or "父任务已取消子任务。",
                )
                if emit_events:
                    self._emit_cancellation_event_safely(task, result)
                raise
            model = ""
            wire_model = ""
            model_snapshot = task.execution_context.model_snapshot
            if model_snapshot is not None:
                model = str(model_snapshot.selection or "").strip()
                wire_model = str(model_snapshot.descriptor.model_id or "").strip()
            diagnostic = _build_failure_diagnostics(
                exc,
                model=model,
                wire_model=wire_model,
            )
            LOGGER.warning(
                "SubAgent task model execution failed: category=%s exception=%s provider=%s "
                "status=%s retryable=%s",
                diagnostic["category"],
                diagnostic["exception_type"],
                diagnostic.get("provider", ""),
                diagnostic.get("status_code", ""),
                diagnostic["retryable"],
            )
            result = self._failure_payload(
                task,
                code="SUBAGENT_MODEL_ERROR",
                message="子任务模型请求失败。",
                diagnostic=diagnostic,
            )
            if emit_events:
                self._emit_terminal_event(task, result)
            return result

        try:
            public_result = self._result_processor(
                task.task_id,
                task.agent_type,
                task.description,
                execution.final_text,
            )
            cancel_check()
        except Exception as exc:  # noqa: BLE001 - artifact 写入失败必须收敛为稳定公开错误
            if self._is_cancellation(exc):
                result = self._cancelled_payload(task, str(exc) or "父任务已取消子任务。")
                if emit_events:
                    self._emit_cancellation_event_safely(task, result)
                raise
            result = self._failure_payload(
                task,
                code="SUBAGENT_RESULT_TOO_LARGE",
                message="子任务结果安全处理或 artifact 写入失败。",
            )
            if emit_events:
                self._emit_terminal_event(task, result)
            return result

        result = {
            "task_id": task.task_id,
            "description": task.description,
            "agent_type": task.agent_type,
            "definition_source": task.definition.source,
            "status": "completed",
            "summary": public_result.summary,
            "evidence": [],
            "artifacts": list(public_result.artifacts),
            "usage": {
                "input_tokens": execution.input_tokens,
                "output_tokens": execution.output_tokens,
                "cached_input_tokens": execution.cached_input_tokens,
                "model_turns": execution.model_turns,
                "tool_calls": execution.tool_calls,
            },
            "error": None,
        }
        if emit_events:
            self._emit_terminal_event(task, result)
        return result

    def _default_result_processor(
        self,
        _task_id: str,
        _agent_type: str,
        _description: str,
        final_text: str,
    ) -> SubAgentPublicResult:
        summary = redact_sensitive_text(final_text.strip())
        if len(summary) > self.config.result_summary_chars:
            summary = summary[: self.config.result_summary_chars] + "\n... 子任务结果已截断。"
        return SubAgentPublicResult(summary=summary)

    def _emit_task_event(self, task: _PreparedTask, status: str) -> None:
        public_status = "running" if status == "started" else status
        self._emit(
            f"subagent.task.{status}",
            {
                "batch_id": task.batch_id,
                "task_id": task.task_id,
                "agent_type": task.agent_type,
                "description": task.description,
                "definition_source": task.definition.source,
                "status": public_status,
            },
        )

    def _combined_cancel_check(
        self,
        batch: _ActiveBatch,
        parent_cancel_check: Callable[[], None] | None,
    ) -> Callable[[], None]:
        """组合父 Run 与 Coordinator 生命周期取消，并向整批级联。"""

        def check_cancelled() -> None:
            if batch.cancel_event.is_set():
                raise SubAgentCancelled(batch.cancel_reason or "子任务批次已取消。")
            if parent_cancel_check is not None:
                try:
                    parent_cancel_check()
                except BaseException:
                    self._request_batch_cancel(batch, "父任务已取消子任务。")
                    raise
            if batch.cancel_event.is_set():
                raise SubAgentCancelled(batch.cancel_reason or "子任务批次已取消。")

        return check_cancelled

    def _track_future(
        self,
        batch: _ActiveBatch,
        future: Future[dict],
        index: int,
    ) -> None:
        """登记 worker，并确保取消竞态与完成回调都只收尾一次。"""

        was_cancelled = batch.register_future(future, index)
        future.add_done_callback(
            lambda completed, batch=batch, index=index: self._future_finished(
                batch,
                index,
                completed,
            )
        )
        if was_cancelled:
            self._start_cancel_cleanup(
                batch,
                futures=((future, index),),
            )

    def _future_finished(
        self,
        batch: _ActiveBatch,
        index: int,
        future: Future[dict],
    ) -> None:
        # 被 cancel() 的 Future 由异步取消收尾线程在终态事件发布后标记完成，
        # 避免 Future.cancel() 同步执行 callback 时突破 close/switch deadline。
        if future.cancelled():
            return
        batch.mark_finished(index)
        self._finalize_batch_if_done(batch)

    def _request_batch_cancel(self, batch: _ActiveBatch, reason: str) -> None:
        if not batch.begin_cancel(reason):
            return
        if self._approval_broker is not None:
            self._approval_broker.cancel_batch(batch.batch_id)
        with batch.lock:
            futures = tuple(batch.futures.items())
        self._start_cancel_cleanup(batch, futures=futures)

    def _start_cancel_cleanup(
        self,
        batch: _ActiveBatch,
        *,
        futures: tuple[tuple[Future[dict], int], ...],
    ) -> None:
        """异步取消排队 Future；慢事件出口计入 batch done，但不阻塞调用方。"""

        def cleanup() -> None:
            for future, index in futures:
                if not future.cancel():
                    continue
                task = batch.tasks[index]
                cancelled = self._cancelled_payload(
                    task,
                    batch.cancel_reason or "子任务批次已取消。",
                )
                try:
                    self._emit_terminal_event(task, cancelled)
                except BaseException as exc:  # 取消收尾必须继续处理同批其他 Future
                    LOGGER.warning(
                        "SubAgent cancellation event failed: %s",
                        type(exc).__name__,
                    )
                finally:
                    batch.mark_finished(index)
                    self._finalize_batch_if_done(batch)

        threading.Thread(
            target=cleanup,
            name=f"omnicrawl-subagent-cancel-{batch.batch_id}",
            daemon=True,
        ).start()

    def _cancel_unsubmitted_tasks(self, batch: _ActiveBatch) -> None:
        for index in batch.untracked_indexes():
            task = batch.tasks[index]
            cancelled = self._cancelled_payload(
                task,
                batch.cancel_reason or "父任务已取消尚未调度的子任务。",
            )
            try:
                self._emit_terminal_event(task, cancelled)
            except BaseException as exc:
                # 未提交任务没有 Future done callback 兜底；单个 Session/observer
                # 出口失败不能阻止同批其余索引进入 finished 状态。
                LOGGER.warning(
                    "SubAgent unsubmitted cancellation event failed: %s",
                    type(exc).__name__,
                )
            finally:
                batch.mark_finished(index)
        self._finalize_batch_if_done(batch)

    def _finalize_batch_if_done(self, batch: _ActiveBatch) -> None:
        if not batch.done_event.is_set():
            return
        callbacks: tuple[Callable[[], None], ...] = ()
        with self._lifecycle_lock:
            current = self._active_batches.get(batch.batch_id)
            if current is batch:
                self._active_batches.pop(batch.batch_id, None)
            if not self._active_batches:
                if self._resume_when_idle and not self._closed:
                    self._accepting = True
                    self._resume_when_idle = False
                callbacks = tuple(self._idle_callbacks)
                self._idle_callbacks.clear()
        with self._terminal_lock:
            self._terminal_task_ids.difference_update(
                task.task_id for task in batch.tasks
            )
        for callback in callbacks:
            try:
                callback()
            except Exception as exc:  # noqa: BLE001 - 清理回调不能破坏 Future 收尾
                LOGGER.warning(
                    "SubAgent idle callback failed: %s",
                    type(exc).__name__,
                )

    def _emit_cancellation_event_safely(
        self,
        task: _PreparedTask,
        result: dict,
    ) -> None:
        """取消事件出口失败时保留原始取消异常，避免父 Run 被误判为 failed。"""

        try:
            self._emit_terminal_event(task, result)
        except BaseException as exc:
            LOGGER.warning(
                "SubAgent cancellation event failed: %s",
                type(exc).__name__,
            )

    def _emit_terminal_event(self, task: _PreparedTask, result: dict) -> None:
        with self._terminal_lock:
            if task.task_id in self._terminal_task_ids:
                return
            self._terminal_task_ids.add(task.task_id)
        payload = {
            "batch_id": task.batch_id,
            "task_id": task.task_id,
            "agent_type": task.agent_type,
            "description": task.description,
            "definition_source": task.definition.source,
            "status": result["status"],
            "summary": result.get("summary", ""),
            "artifacts": result.get("artifacts", []),
            "usage": result.get("usage", {}),
            "error": result.get("error"),
        }
        self._emit(f"subagent.task.{result['status']}", payload)

    def _emit(self, event_name: str, payload: dict) -> None:
        self._event_sink(event_name, dict(payload))

    def _validate_arguments(self, arguments: dict) -> tuple[str, str] | None:
        if not isinstance(arguments, dict):
            return "AGENT_DEFINITION_INVALID", "subagent 参数必须是对象。"
        unknown_top_level = sorted(set(arguments) - _TOP_LEVEL_FIELDS)
        if unknown_top_level:
            return (
                "SUBAGENT_PERMISSION_DENIED",
                f"当前 SubAgent 阶段不允许参数：{', '.join(unknown_top_level)}。",
            )
        if arguments.get("action") not in {"run", "spawn"}:
            return (
                "SUBAGENT_PERMISSION_DENIED",
                "仅支持 action=run 或 action=spawn。",
            )
        tasks = arguments.get("tasks")
        if not isinstance(tasks, list):
            return "SUBAGENT_LIMIT_EXCEEDED", "tasks 必须是数组。"
        task_limit = self.config.max_tasks_per_batch
        if not 1 <= len(tasks) <= task_limit:
            return (
                "SUBAGENT_LIMIT_EXCEEDED",
                f"当前 SubAgent 阶段每批必须包含 1 到 {task_limit} 个任务。",
            )

        max_concurrency = arguments.get("max_concurrency", self.config.max_concurrency)
        if (
            isinstance(max_concurrency, bool)
            or not isinstance(max_concurrency, int)
            or not 1 <= max_concurrency <= 4
        ):
            return "SUBAGENT_LIMIT_EXCEEDED", "max_concurrency 必须是 1 到 4 的整数。"
        fail_fast = arguments.get("fail_fast", False)
        if not isinstance(fail_fast, bool):
            return "AGENT_DEFINITION_INVALID", "fail_fast 必须是布尔值。"

        for index, task in enumerate(tasks):
            if not isinstance(task, dict):
                return "AGENT_DEFINITION_INVALID", f"tasks[{index}] 必须是对象。"
            unknown_task_fields = sorted(set(task) - _TASK_FIELDS)
            if unknown_task_fields:
                return (
                    "SUBAGENT_PERMISSION_DENIED",
                    f"当前 SubAgent 阶段不允许任务字段：{', '.join(unknown_task_fields)}。",
                )
            description = task.get("description")
            prompt = task.get("prompt")
            agent_type = task.get("subagent_type")
            context = task.get("context", "fresh")
            model = task.get("model", "")
            if not isinstance(description, str) or not description.strip():
                return (
                    "AGENT_DEFINITION_INVALID",
                    f"tasks[{index}].description 必须是非空字符串。",
                )
            if not isinstance(prompt, str) or not prompt.strip():
                return (
                    "AGENT_DEFINITION_INVALID",
                    f"tasks[{index}].prompt 必须是非空字符串。",
                )
            if not isinstance(agent_type, str) or not agent_type.strip():
                return (
                    "AGENT_DEFINITION_INVALID",
                    f"tasks[{index}].subagent_type 必须是非空字符串。",
                )
            normalized_agent_type = agent_type.strip().casefold()
            definition = self.registry.get(normalized_agent_type)
            if definition is None:
                available = ", ".join(self.available_agent_types()) or "无"
                return (
                    "AGENT_TYPE_NOT_FOUND",
                    f"未找到 Agent 定义：{normalized_agent_type}。当前可用：{available}。",
                )
            unsupported = self._unsupported_definition_reason(definition)
            if unsupported:
                return "AGENT_DEFINITION_INVALID", unsupported
            if not isinstance(context, str) or context not in {"fresh", "fork"}:
                return "AGENT_DEFINITION_INVALID", "context 仅支持 fresh 或 fork。"
            if context == "fork" and not self.config.allow_fork:
                return (
                    "SUBAGENT_PERMISSION_DENIED",
                    "Fork SubAgent 默认关闭，请在配置中显式设置 allow_fork=true。",
                )
            if (
                not isinstance(model, str)
                or ("model" in task and not model.strip())
                or (isinstance(model, str) and len(model.strip()) > 200)
            ):
                return (
                    "AGENT_DEFINITION_INVALID",
                    f"tasks[{index}].model 必须是 1 到 200 字符的字符串。",
                )
        return None

    def _unsupported_definition_reason(self, definition: AgentDefinition) -> str:
        """验证定义只能选择 Host 已实现且已显式开启的权限 profile。"""

        unsupported: list[str] = []
        if definition.permission_mode == "delegated-read-only":
            if definition.background:
                unsupported.append("read_only profile 的 background 必须为 false")
        elif definition.permission_mode == "explicit-command-allowlist":
            if not self.config.enable_verify_agent:
                unsupported.append(
                    "explicit-command-allowlist 需要 subagents.enable_verify_agent=true"
                )
        elif definition.permission_mode == "standard":
            if not self.config.allow_standard_agent:
                unsupported.append(
                    "standard profile 需要 subagents.allow_standard_agent=true"
                )
            if (
                definition.isolation == "shared"
                and not self.config.allow_shared_workspace_writes
            ):
                unsupported.append(
                    "standard + isolation=shared 需要 "
                    "subagents.allow_shared_workspace_writes=true；"
                    "推荐改用 isolation=worktree"
                )
        else:
            unsupported.append("permissionMode 不受当前 SubAgent 阶段支持")
        if definition.isolation not in {"shared", "worktree"}:
            unsupported.append(f"isolation 不受支持：{definition.isolation}")
        elif definition.isolation == "worktree" and not self.config.allow_worktree:
            unsupported.append("isolation=worktree 需要 subagents.allow_worktree=true")
        if definition.skills:
            unsupported.append("当前阶段尚不注入 skills")
        if definition.mcp_servers:
            unsupported.append("当前阶段尚不开放 mcpServers")
        if not unsupported:
            return ""
        source = str(definition.source_path or definition.source)
        return f"Agent 定义超出当前 SubAgent 能力：{'; '.join(unsupported)}（{source}）。"

    def _tools_for_definition(self, definition: AgentDefinition) -> dict[str, ToolDefinition]:
        """按定义 profile 选择工具，再应用定义自身的白名单和黑名单。"""

        if definition.permission_mode == "explicit-command-allowlist":
            return self._verify_tools(definition)
        if definition.permission_mode == "standard":
            return self._standard_tools(definition)
        return self._read_only_tools(definition)

    def _standard_tools(self, definition: AgentDefinition) -> dict[str, ToolDefinition]:
        """standard 写 Agent：只读 + 写入/命令工具。

        写入与命令默认由 Host 的 subagent risk 策略逐次审批，不在这里改写
        requires_confirmation，避免 profile 过滤层吞掉审批语义。
        """

        parent_tools = self._tools_provider()
        return self._filter_profile_tools(
            definition,
            available_tools=parent_tools,
            profile_tools=STANDARD_TOOL_NAMES,
        )

    def _read_only_tools(self, definition: AgentDefinition) -> dict[str, ToolDefinition]:
        """继承父工具，仅移除本地写入口并包装 Shell/Monitor 命令。"""

        parent_tools = self._tools_provider()
        profile_tools = frozenset(parent_tools) - READ_ONLY_BLOCKED_TOOL_NAMES
        filtered = self._filter_profile_tools(
            definition,
            available_tools=parent_tools,
            profile_tools=profile_tools,
        )
        for name in READ_ONLY_TOOL_NAMES.intersection(filtered):
            filtered[name] = replace(filtered[name], requires_confirmation=False)
        for name in READ_ONLY_COMMAND_TOOL_NAMES.intersection(filtered):
            filtered[name] = wrap_read_only_command_tool(filtered[name])
        return filtered

    def _verify_tools(self, definition: AgentDefinition) -> dict[str, ToolDefinition]:
        # 只读工具来自父 Host；verify_command 由 Host 单独提供，确保它不会出现在
        # 主 Agent 的工具 schema 中，更不能被自定义 Markdown 定义替换为原始 Shell。
        available_tools = dict(self._tools_provider())
        available_tools.update(self._verify_tools_provider())
        return self._filter_profile_tools(
            definition,
            available_tools=available_tools,
            profile_tools=VERIFY_TOOL_NAMES,
            clear_confirmation=True,
        )

    @staticmethod
    def _filter_profile_tools(
        definition: AgentDefinition,
        *,
        available_tools: Mapping[str, ToolDefinition],
        profile_tools: frozenset[str],
        clear_confirmation: bool = False,
    ) -> dict[str, ToolDefinition]:
        allowlist = set(definition.tools) if definition.tools else set(profile_tools)
        allowlist &= profile_tools
        allowlist -= set(definition.disallowed_tools)
        result = {
            name: available_tools[name]
            for name in sorted(allowlist)
            if name in available_tools
        }
        if clear_confirmation:
            result = {
                name: replace(tool, requires_confirmation=False)
                for name, tool in result.items()
            }
        return result

    @staticmethod
    def _failure_payload(
        task: _PreparedTask,
        *,
        code: str,
        message: str,
        diagnostic: Mapping[str, Any] | None = None,
    ) -> dict:
        return {
            "task_id": task.task_id,
            "description": task.description,
            "agent_type": task.agent_type,
            "definition_source": task.definition.source,
            "status": "failed",
            "summary": "",
            "evidence": [],
            "artifacts": [],
            "usage": {
                "input_tokens": 0,
                "output_tokens": 0,
                "cached_input_tokens": 0,
                "model_turns": 0,
                "tool_calls": 0,
            },
            "error": {
                "code": code,
                "message": redact_sensitive_text(message),
                **({"diagnostic": dict(diagnostic)} if diagnostic else {}),
            },
        }

    @classmethod
    def _cancelled_payload(cls, task: _PreparedTask, message: str) -> dict:
        payload = cls._failure_payload(
            task,
            code="SUBAGENT_CANCELLED",
            message=message,
        )
        payload["status"] = "cancelled"
        return payload

    @staticmethod
    def _is_cancellation(exc: BaseException) -> bool:
        return "cancel" in exc.__class__.__name__.casefold()

    def _top_level_error(self, code: str, message: str) -> ToolResult:
        return self._json_result(
            False,
            {
                "batch_id": None,
                "status": "failed",
                "results": [],
                "error": {"code": code, "message": message},
            },
        )

    @staticmethod
    def _json_result(ok: bool, payload: dict) -> ToolResult:
        text = json.dumps(payload, ensure_ascii=False, indent=2)
        return ToolResult(ok=ok, output=text, full_output=text)
