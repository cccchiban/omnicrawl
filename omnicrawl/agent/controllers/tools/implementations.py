"""具体 ``_tool_*`` 实现：内置工具与记忆/KB/MCP 工具的薄包装。"""
from __future__ import annotations

import logging
import json
from pathlib import Path
from typing import Any
from ...toolkit.tools import (
    mcp_prompt_result,
    mcp_resource_result,
    mcp_tool_result,
    workspace_command_tool_result,
    workspace_tool_result,
)
from ...context_compaction import (
    SessionEvidenceRecallService,
    SourceEvent,
)
from ...toolkit.image_tools import read_image_file
from ....workspace.tools import WorkspaceToolError
from ...toolkit.memory_tools import (
    memory_expand_related_result,
    memory_read_result,
    memory_search_result,
    memory_write_result,
    project_memory_expand_related_result,
    project_memory_read_result,
    project_memory_search_result,
    project_memory_write_result,
    session_memory_expand_related_result,
    session_memory_read_result,
    session_memory_search_result,
    session_memory_write_result,
    user_memory_expand_related_result,
    user_memory_read_result,
    user_memory_search_result,
    user_memory_write_result,
)
from ...toolkit.knowledge_tools import (
    kb_append_result,
    kb_list_result,
    kb_read_result,
    kb_search_result,
    kb_write_result,
)
from ...toolkit.git_tools import git_result
from ...types import AskUserRequest, ToolResult
from ....config.core.runtime import resolve_config_path
from ....memory import (
    MemoryStore,
)
from ....mcp import MCPToolMeta
from ...runtime.run_guard import mark_pause_requested

from ..shared import (
    AgentError,
    ask_user_advisor_hint,
)

LOGGER = logging.getLogger(__name__)


class ToolImplementationsMixin:
    """具体 ``_tool_*`` 实现：内置工具与记忆/KB/MCP 工具的薄包装。"""

    def _tool_update_todos(self, arguments: dict[str, Any]) -> ToolResult:
        """接收模型的执行清单，并把安全投影转发给 UI。"""

        raw_todos = arguments.get("todos")
        if not isinstance(raw_todos, list):
            return ToolResult(ok=False, output="todos 必须是数组。")
        todos: list[dict[str, Any]] = []
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
            item_id = str(raw_item.get("id") or index).strip()[:80]
            todos.append(
                {
                    "id": item_id or str(index),
                    "step": step[:240],
                    "completed": completed,
                }
            )
        payload = {"todos": todos}
        active_todos = getattr(self, "_active_todo_items", None)
        if isinstance(active_todos, list):
            active_todos.clear()
            active_todos.extend(todos)
        callback = getattr(self, "_todo_update_callback", None)
        if callable(callback):
            try:
                callback(payload)
            except Exception:  # noqa: BLE001 - UI observer 不得破坏 Agent 回合
                LOGGER.warning("Todo UI observer failed", exc_info=True)
        return ToolResult(
            ok=True,
            output=json.dumps(
                {"updated": len(todos), "todos": todos},
                ensure_ascii=False,
            ),
        )

    def _tool_pause_work(self, arguments: dict[str, Any]) -> ToolResult:
        """主动停止当前回合的自动路径，保留任务供用户稍后继续。"""

        _ = arguments
        if not mark_pause_requested():
            return ToolResult(ok=False, output="当前没有可暂停的 Agent 回合。")
        return ToolResult(
            ok=True,
            output=json.dumps(
                {
                    "paused": True,
                    "message": "当前回合已暂停；用户发送‘继续’后可恢复未完成任务。",
                },
                ensure_ascii=False,
            ),
        )

    def _tool_ask_user(self, arguments: dict[str, Any]) -> ToolResult:
        """向用户提出一个结构化问题并阻塞等待入口返回答案。"""

        kind = str(arguments.get("kind") or "question").strip().casefold()
        if kind not in {"question", "select", "confirm"}:
            return ToolResult(
                ok=False,
                output="kind 必须是 question、select 或 confirm。",
            )
        question = arguments.get("question")
        if not isinstance(question, str) or not question.strip():
            return ToolResult(ok=False, output="question 不能为空。")
        question = question.strip()
        raw_options = arguments.get("options", [])
        if not isinstance(raw_options, list):
            return ToolResult(ok=False, output="options 必须是数组。")
        options = tuple(
            option.strip()
            for option in raw_options
            if isinstance(option, str) and option.strip()
        )
        if not options:
            return ToolResult(ok=False, output="ask_user 必须提供至少一个非空 options 选项。")

        request = AskUserRequest(
            question=question,
            kind=kind,
            options=options,
            request_id=str(arguments.get("request_id") or "").strip(),
            # 与工具批次超时保持一致；入口据此在超时后自行关闭提问面板。
            timeout_seconds=getattr(
                getattr(self, "config", None),
                "tool_timeout_seconds",
                None,
            ),
        )
        handler = getattr(self, "_ask_user_handler", None)
        try:
            answer = handler(request) if callable(handler) else self._ask_user_in_terminal(request)
        except KeyboardInterrupt:
            raise
        except Exception as exc:  # noqa: BLE001 - 提问失败按工具错误返回
            if "cancel" in type(exc).__name__.casefold():
                raise
            LOGGER.warning("ask_user handler failed", exc_info=True)
            return ToolResult(ok=False, output=f"向用户提问失败：{exc}")
        if answer is None or not str(answer).strip():
            output = "用户未回答该问题。"
            # 仅当顾问策略真正可用时提示可调用 advisor 托管（启用 + 已选模型 +
            # 当前模型不在黑名单；与工具表注册/提示词注入同一判定）。
            hint = ask_user_advisor_hint(self)
            if hint:
                output = f"{output}{hint}"
            return ToolResult(ok=False, output=output)
        answer_text = str(answer).strip()
        return ToolResult(
            ok=True,
            output=json.dumps(
                {
                    "kind": request.kind,
                    "question": request.question,
                    "options": list(request.options),
                    "answer": answer_text,
                },
                ensure_ascii=False,
            ),
        )

    def _ask_user_in_terminal(self, request: AskUserRequest) -> str | None:
        """无入口回调时回退到普通终端输入。"""

        print(f"\nAgent 提问：{request.question}")
        for index, option in enumerate(request.options, start=1):
            print(f"  {index}. {option}")
        if request.kind == "confirm":
            answer = input("请输入选项序号，或直接输入 yes/no 或自定义回答：").strip()
            if answer.isdigit() and 1 <= int(answer) <= len(request.options):
                return request.options[int(answer) - 1]
            if answer.casefold() in {"yes", "y", "是", "确认", "true", "1"}:
                return "yes"
            if answer.casefold() in {"no", "n", "否", "拒绝", "false", "0"}:
                return "no"
            return answer or None
        if request.kind == "select":
            answer = input("请输入选项序号：").strip()
            if answer.isdigit() and 1 <= int(answer) <= len(request.options):
                return request.options[int(answer) - 1]
            return answer or None
        answer = input("请输入选项序号或直接输入回答：").strip()
        if answer.isdigit() and 1 <= int(answer) <= len(request.options):
            return request.options[int(answer) - 1]
        return answer or None

    def _tool_list(self, arguments: dict[str, Any]) -> ToolResult:
        return workspace_tool_result(self._workspace_toolbox().list_files, arguments)

    def _tool_find(self, arguments: dict[str, Any]) -> ToolResult:
        return workspace_tool_result(self._workspace_toolbox().find_files, arguments)

    def _tool_read(self, arguments: dict[str, Any]) -> ToolResult:
        """读取文本文件：模型可见输出带行号与续读 footer，ui_artifact 携带
        结构化行窗口（行号/语言/总数）供 UI 渲染。"""

        try:
            text, artifact = self._workspace_toolbox().read_file_result(arguments)
            return ToolResult(
                ok=True,
                output=text,
                ui_artifact=artifact,
            )
        except WorkspaceToolError as exc:
            return ToolResult(
                ok=False,
                output=exc.formatted_message(),
                error_code=exc.code,
                retryable=exc.retryable,
            )

    def _tool_read_image(self, arguments: dict[str, Any]) -> ToolResult:
        return read_image_file(
            arguments,
            workspace_root=Path(self._workspace_toolbox().workspace_root),
        )

    def _tool_grep(self, arguments: dict[str, Any]) -> ToolResult:
        return workspace_tool_result(self._workspace_toolbox().grep, arguments)

    def _tool_web_search(self, arguments: dict[str, Any]) -> ToolResult:
        """使用 Bing/DuckDuckGo/雅虎搜索公开网页（见 omnicrawl/web_search.py）。"""

        try:
            from omnicrawl.web_search import WebSearch

            return ToolResult(ok=True, output=WebSearch().search(arguments))
        except RuntimeError as exc:
            return ToolResult(ok=False, output=str(exc))

    def _tool_fetcher(self, arguments: dict[str, Any]) -> ToolResult:
        """模拟浏览器指纹抓取网页（见 omnicrawl/fetcher.py）。"""

        try:
            from omnicrawl.fetcher import Fetcher

            return ToolResult(ok=True, output=Fetcher().fetch(arguments))
        except RuntimeError as exc:
            return ToolResult(ok=False, output=str(exc))

    def _tool_image_gen(self, arguments: dict[str, Any]) -> ToolResult:
        """生成/编辑图片（见 omnicrawl/image_gen.py，配置见 config/image_gen.py）。"""

        try:
            from omnicrawl.image_gen import ImageGenerator

            configuration = getattr(getattr(self, "config", None), "image_gen", None)
            if configuration is not None:
                generator = ImageGenerator(configuration=configuration)
            else:
                generator = ImageGenerator(config_path=resolve_config_path())
            return ToolResult(ok=True, output=generator.run(arguments))
        except RuntimeError as exc:
            return ToolResult(ok=False, output=str(exc))

    def _tool_tts_synthesize(self, arguments: dict[str, Any]) -> ToolResult:
        """把文本合成为语音（见 omnicrawl/tts，配置见 config/features/tts.py）。

        引擎按 model_dir/thread_count 缓存复用；合成完成后按配置自动播放。
        """

        def _error_result(message: str) -> ToolResult:
            """失败结果统一为 JSON，明确 ok=false，让模型不再重复调用。"""
            return ToolResult(
                ok=False,
                output=json.dumps({"ok": False, "error": message}, ensure_ascii=False),
            )

        try:
            from omnicrawl.tts import TtsEngine, TTSConfig  # noqa: F401 - 仅用于探测可选依赖是否安装
        except ModuleNotFoundError as exc:
            dependency = str(getattr(exc, "name", "") or "") or "可选依赖"
            return _error_result(
                f"TTS 依赖缺失（{dependency}）：语音合成不可用。"
                "请执行 pip install -r requirements.txt 安装 TTS 依赖后重试。"
            )

        config = getattr(getattr(self, "config", None), "tts", None)
        if config is None or not getattr(config, "enabled", False):
            return _error_result(
                "TTS 未启用：请在 TUI 设置面板（/settings → TTS）中启用并等待模型就绪。"
            )
        text = str(arguments.get("text") or "").strip()
        if not text:
            return _error_result("text 不能为空。")
        # 音色固定使用设置中的配置（/settings → TTS），不接受模型传入：模型可能
        # 猜一个不存在的音色名（如 default），导致合成失败。
        voice = str(getattr(config, "voice", "Junhao") or "Junhao").strip()
        prompt_audio = arguments.get("prompt_audio") or None
        output_path = arguments.get("path") or None
        if output_path is None:
            # 无 path 时写入配置的 output_dir（相对工作区），时间戳命名避免覆盖。
            from datetime import datetime

            output_dir = Path(
                getattr(config, "output_dir", ".omnicrawl/.agent_tmp/tts") or ".omnicrawl/.agent_tmp/tts"
            )
            if not output_dir.is_absolute():
                output_dir = Path(self.workspace_root) / output_dir
            stamp = datetime.now().strftime("%Y%m%d_%H%M%S")
            output_path = output_dir / f"tts_{stamp}.wav"

        try:
            engine = self._get_tts_engine()
            if prompt_audio:
                voice_arg = None
            else:
                voice_arg = voice
            result = engine.synthesize(
                text,
                voice=voice_arg,
                prompt_audio_path=prompt_audio,
                output_path=output_path,
            )
        except Exception as exc:  # noqa: BLE001 - 统一包装为工具错误
            return _error_result(str(exc))

        # 生成后默认自动播放（配置可控，播放失败不影响结果）。
        if getattr(config, "auto_play", True):
            try:
                from omnicrawl.tts.player import play_wav

                play_wav(result.audio_path)
            except Exception:  # noqa: BLE001
                pass

        summary = {
            "ok": True,
            "audio_path": str(result.audio_path),
            "sample_rate": result.sample_rate,
            "duration_seconds": round(result.duration_seconds, 2),
            "voice": voice if voice_arg else (str(prompt_audio) if prompt_audio else ""),
            "text_chunks": len(result.text_chunks),
        }
        return ToolResult(ok=True, output=json.dumps(summary, ensure_ascii=False))

    def _get_tts_engine(self):
        """按当前 TTS 配置惰性构建并缓存引擎；配置变化时自动重建。"""

        from omnicrawl.tts import TTSConfig, TtsEngine

        config = getattr(getattr(self, "config", None), "tts", None)
        cached = getattr(self, "_tts_engine", None)
        cached_sig = getattr(self, "_tts_engine_sig", None)
        sig = (
            (str(config.resolved_model_dir()), int(config.thread_count), str(getattr(config, "device", "auto")))
            if config is not None
            else None
        )
        if cached is not None and cached_sig == sig:
            return cached
        if cached is not None:
            try:
                cached.close()
            except Exception:  # noqa: BLE001
                pass
        output_dir = getattr(config, "output_dir", ".omnicrawl/.agent_tmp/tts") or ".omnicrawl/.agent_tmp/tts"
        resolved_output_dir = Path(output_dir)
        if not resolved_output_dir.is_absolute():
            resolved_output_dir = Path(self.workspace_root) / resolved_output_dir
        engine = TtsEngine(
            TTSConfig(
                model_dir=config.model_dir or None,
                thread_count=config.thread_count,
                device=getattr(config, "device", "auto"),
                output_dir=resolved_output_dir,
            )
        )
        self._tts_engine = engine
        self._tts_engine_sig = sig
        return engine

    def _tool_edit_file(self, arguments: dict[str, Any]) -> ToolResult:
        return workspace_tool_result(self._workspace_toolbox().edit_file, arguments)

    def _tool_write_file(self, arguments: dict[str, Any]) -> ToolResult:
        return workspace_tool_result(self._workspace_toolbox().write_file, arguments)

    def _tool_bash(self, arguments: dict[str, Any]) -> ToolResult:
        """用显式 Bash 解释器执行命令，不能被模型参数覆盖解释器。"""

        return workspace_command_tool_result(
            lambda command_arguments: self._workspace_toolbox().run_shell_command(
                command_arguments,
                shell="bash",
            ),
            arguments,
        )

    def _tool_powershell(self, arguments: dict[str, Any]) -> ToolResult:
        """用显式 PowerShell 解释器执行命令，不能被模型参数覆盖解释器。"""

        return workspace_command_tool_result(
            lambda command_arguments: self._workspace_toolbox().run_shell_command(
                command_arguments,
                shell="powershell",
            ),
            arguments,
        )

    def _tool_monitor(self, arguments: dict[str, Any]) -> ToolResult:
        return workspace_command_tool_result(self._monitor_toolbox().run, arguments)

    def _tool_git(self, arguments: dict[str, Any]) -> ToolResult:
        """结构化 git 操作：argv 直调不经 shell，风险分级见 approval_policy。"""

        return git_result(self.workspace_root, arguments)

    def _tool_recall_session_evidence(self, arguments: dict[str, Any]) -> ToolResult:
        """恢复当前有效摘要授权的事件，不接受 Session ID 或 artifact 路径。"""

        store = getattr(self, "_session_store", None)
        state = getattr(self, "_session_state", None)
        if store is None or state is None:
            output = {
                "schema_version": 1,
                "ok": False,
                "items": [],
                "diagnostics": [
                    {
                        "code": "session_unavailable",
                        "message": "当前没有可读取的活动 Session。",
                    }
                ],
                "truncated": False,
            }
            return ToolResult(ok=False, output=json.dumps(output, ensure_ascii=False))

        service = getattr(self, "_session_evidence_recall_service", None)
        if not isinstance(service, SessionEvidenceRecallService):
            service = SessionEvidenceRecallService()
            self._session_evidence_recall_service = service
        try:
            events = tuple(
                SourceEvent(event.event_id, event.type, dict(event.payload))
                for event in store.read_session_events(state.session_id)
            )
            result = service.recall(
                events=events,
                event_ids=arguments.get("event_ids"),
                artifact_reader=lambda artifact_path: store.read_artifact_text(
                    state.session_id,
                    artifact_path,
                ),
            )
        except Exception:
            result = {
                "schema_version": 1,
                "ok": False,
                "items": [],
                "diagnostics": [
                    {
                        "code": "evidence_unavailable",
                        "message": "当前 Session 证据暂时不可读取。",
                    }
                ],
                "truncated": False,
            }
        return ToolResult(
            ok=bool(result.get("ok", False)),
            output=json.dumps(result, ensure_ascii=False, separators=(",", ":")),
        )

    def _tool_memory_search(self, arguments: dict[str, Any]) -> ToolResult:
        return memory_search_result(self._require_memory_store("project"), arguments)

    def _tool_memory_read(self, arguments: dict[str, Any]) -> ToolResult:
        return memory_read_result(self._require_memory_store("project"), arguments)

    def _tool_memory_expand_related(self, arguments: dict[str, Any]) -> ToolResult:
        return memory_expand_related_result(self._require_memory_store("project"), arguments)

    def _tool_memory_write(self, arguments: dict[str, Any]) -> ToolResult:
        return memory_write_result(self._require_memory_store("project"), arguments)

    def _tool_project_memory_search(self, arguments: dict[str, Any]) -> ToolResult:
        return project_memory_search_result(self._require_memory_store("project"), arguments)

    def _tool_project_memory_read(self, arguments: dict[str, Any]) -> ToolResult:
        return project_memory_read_result(self._require_memory_store("project"), arguments)

    def _tool_project_memory_expand_related(self, arguments: dict[str, Any]) -> ToolResult:
        return project_memory_expand_related_result(self._require_memory_store("project"), arguments)

    def _tool_project_memory_write(self, arguments: dict[str, Any]) -> ToolResult:
        return project_memory_write_result(self._require_memory_store("project"), arguments)

    def _tool_session_memory_search(self, arguments: dict[str, Any]) -> ToolResult:
        return session_memory_search_result(self._require_memory_store("session"), arguments)

    def _tool_session_memory_read(self, arguments: dict[str, Any]) -> ToolResult:
        return session_memory_read_result(self._require_memory_store("session"), arguments)

    def _tool_session_memory_expand_related(self, arguments: dict[str, Any]) -> ToolResult:
        return session_memory_expand_related_result(self._require_memory_store("session"), arguments)

    def _tool_session_memory_write(self, arguments: dict[str, Any]) -> ToolResult:
        return session_memory_write_result(self._require_memory_store("session"), arguments)

    def _tool_user_memory_search(self, arguments: dict[str, Any]) -> ToolResult:
        return user_memory_search_result(self._require_memory_store("user"), arguments)

    def _tool_user_memory_read(self, arguments: dict[str, Any]) -> ToolResult:
        return user_memory_read_result(self._require_memory_store("user"), arguments)

    def _tool_user_memory_expand_related(self, arguments: dict[str, Any]) -> ToolResult:
        return user_memory_expand_related_result(self._require_memory_store("user"), arguments)

    def _tool_user_memory_write(self, arguments: dict[str, Any]) -> ToolResult:
        return user_memory_write_result(self._require_memory_store("user"), arguments)

    def _tool_kb_search(self, arguments: dict[str, Any]) -> ToolResult:
        return kb_search_result(self._get_knowledge_base(), arguments)

    def _tool_kb_read(self, arguments: dict[str, Any]) -> ToolResult:
        return kb_read_result(self._get_knowledge_base(), arguments)

    def _tool_kb_write(self, arguments: dict[str, Any]) -> ToolResult:
        return kb_write_result(self._get_knowledge_base(), arguments)

    def _tool_kb_append(self, arguments: dict[str, Any]) -> ToolResult:
        return kb_append_result(self._get_knowledge_base(), arguments)

    def _tool_kb_list(self, arguments: dict[str, Any]) -> ToolResult:
        return kb_list_result(self._get_knowledge_base(), arguments)

    def _require_memory_store(self, scope: str = "project") -> MemoryStore:
        stores = {
            "project": getattr(self, "_project_memory_store", getattr(self, "_memory_store", None)),
            "session": getattr(self, "_session_memory_store", None),
            "user": getattr(self, "_user_memory_store", None),
        }
        store = stores.get(scope)
        if store is None:
            raise AgentError(f"{scope} 级记忆系统未启用。")
        return store

    def _tool_mcp_call(self, meta: MCPToolMeta, arguments: dict[str, Any]) -> ToolResult:
        return mcp_tool_result(self._mcp_manager, meta, arguments)

    def _tool_mcp_read_resource(self, logical_uri: str) -> ToolResult:
        return mcp_resource_result(self._mcp_manager, logical_uri)

    def _tool_mcp_get_prompt(self, logical_name: str, arguments: dict[str, Any]) -> ToolResult:
        return mcp_prompt_result(self._mcp_manager, logical_name, arguments)
