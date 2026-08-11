"""ESC 取消后会话上下文继承问题的 Red-Green-Verify 回归测试。

覆盖 .agent_tmp/files/esc-context-loss-issue.md 记录的三个根因：

1. 模型流被主动关闭后正常耗尽，取消信号被空响应重试吞掉；
2. 通用异常收尾不把未完成回合加入内存历史（CANCELLED 错误语义缺失）；
3. 快速 ESC 竞态让用户输入没有进入会话记录。

以及取消后 /resume 的 Session 投影一致性。
"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

from omnicrawl.agent import AgentModelReply, LocalToolAgent, ToolDefinition, ToolResult
from omnicrawl.agent.llm_protocol import AgentLLMProtocol
from omnicrawl.llm.capabilities import ModelCapabilities
from omnicrawl.llm.errors import ModelError, ModelErrorCode
from omnicrawl.llm.protocol import (
    GenerationOptions,
    ModelIdentity,
    ModelTurnRequest,
)
from omnicrawl.llm.providers.openai_chat import OpenAIChatCompletionsRuntime
from omnicrawl.llm.providers.openai_responses import OpenAIResponsesRuntime
from omnicrawl.llm.registry import ModelDescriptor, ProviderProfile
from omnicrawl.llm.runtime import ModelRuntimeManager
from omnicrawl.session import SessionStore


class _ClosedStreamRuntime:
    """模拟底层流被 close() 后迭代器正常耗尽、无任何事件的 Runtime。"""

    def __init__(self, identity: ModelIdentity) -> None:
        self.identity = identity
        self.capabilities = ModelCapabilities(streaming=True, tools=True)
        self.turn_count = 0

    def stream_turn(self, request, *, cancel_check=None):
        self.turn_count += 1
        yield from ()  # 正常耗尽，不产生任何事件
        return


class _EmptyWithRuntimeError(_ClosedStreamRuntime):
    """每次请求都抛 EMPTY_RESPONSE 的 Runtime，用于空响应重试路径。"""

    def stream_turn(self, request, *, cancel_check=None):
        self.turn_count += 1
        raise ModelError(
            code=ModelErrorCode.EMPTY_RESPONSE,
            message="空响应",
            retryable=False,
        )
        yield  # pragma: no cover - 永不执行
        return


class _CancelledRuntime(_ClosedStreamRuntime):
    """直接抛出 CANCELLED 的 Runtime，用于验证取消错误语义。"""

    def stream_turn(self, request, *, cancel_check=None):
        self.turn_count += 1
        raise ModelError(
            code=ModelErrorCode.CANCELLED,
            message="用户取消当前任务",
        )
        yield  # pragma: no cover - 永不执行
        return


class ProtocolCancelCheckpointTests(unittest.TestCase):
    """协议层：流关闭/空响应/重试路径必须尊重取消状态。"""

    def _protocol(
        self,
        runtime,
        *,
        request_retry_count: int = 3,
    ) -> AgentLLMProtocol:
        identity = runtime.identity
        profile = ProviderProfile(
            id="p1",
            provider="openai",
            base_url="https://example.test/v1",
            api_key="test-key",
            default_protocol="openai_chat_completions",
        )
        descriptor = ModelDescriptor(
            identity=identity,
            display_name=identity.model_id,
            capabilities=ModelCapabilities(streaming=True, tools=True),
            context_window_tokens=128000,
        )
        manager = ModelRuntimeManager()
        manager.bootstrap(profile, descriptor, runtime=runtime)
        self.addCleanup(manager.close)
        return AgentLLMProtocol(
            client=None,
            model=identity.model_id,
            request_timeout_seconds=30,
            request_retry_count=request_retry_count,
            workspace_root=Path("."),
            system_prompt_provider=lambda: "sys",
            prompt_cache_identity_provider=lambda: {"workspace": "w"},
            tools_provider=lambda: [],
            extra_body_provider=lambda: {},
            tool_name_from_function_name=lambda name: name,
            function_name_for_tool=lambda name: name,
            runtime_manager=manager,
        )

    def _identity(self, model_id: str = "demo-model") -> ModelIdentity:
        return ModelIdentity(
            profile_id="p1",
            provider="openai",
            protocol="openai_chat_completions",
            model_id=model_id,
        )

    def test_closed_stream_exhaustion_checks_cancel_instead_of_retrying_empty(self) -> None:
        """流被关闭后正常耗尽且无响应时，必须调用取消检查并立即结束。

        修复前：取消检查次数为 0，协议层把空响应重试 3 次后抛出
        ``AgentProtocolError``；修复后：取消检查抛出 KeyboardInterrupt，
        不进入空响应重试。
        """

        runtime = _ClosedStreamRuntime(self._identity())
        protocol = self._protocol(runtime)
        cancel_calls: list[int] = []

        def cancel_check() -> None:
            cancel_calls.append(len(cancel_calls) + 1)
            if len(cancel_calls) == 2:
                raise KeyboardInterrupt("用户取消当前任务")

        with self.assertRaises(KeyboardInterrupt):
            protocol.request_reply(
                [{"role": "user", "content": "hi"}],
                on_delta=lambda _t: None,
                on_token_usage=lambda _i, _o, _c: None,
                on_protocol_wait=lambda: None,
                on_retry_status=lambda _msg: None,
                cancel_check=cancel_check,
            )

        # 第 1 次：请求开始前；第 2 次：流正常耗尽后。两者都发生才算达标。
        self.assertEqual(len(cancel_calls), 2)
        # 取消必须在第一次请求内结束，不能继续空响应重试。
        self.assertEqual(runtime.turn_count, 1)

    def test_cancel_before_request_start_skips_model_call(self) -> None:
        """请求开始前的取消检查必须阻止任何模型调用。"""

        runtime = _ClosedStreamRuntime(self._identity())
        protocol = self._protocol(runtime)
        cancel_calls: list[int] = []

        def cancel_check() -> None:
            cancel_calls.append(len(cancel_calls) + 1)
            raise KeyboardInterrupt("用户取消当前任务")

        with self.assertRaises(KeyboardInterrupt):
            protocol.request_reply(
                [{"role": "user", "content": "hi"}],
                on_delta=lambda _t: None,
                on_token_usage=lambda _i, _o, _c: None,
                on_protocol_wait=lambda: None,
                on_retry_status=lambda _msg: None,
                cancel_check=cancel_check,
            )

        self.assertEqual(len(cancel_calls), 1)
        self.assertEqual(runtime.turn_count, 0)

    def test_empty_reply_retry_checks_cancel_before_next_attempt(self) -> None:
        """空响应异常捕获后、下一次重试前必须检查取消状态。"""

        runtime = _EmptyWithRuntimeError(self._identity())
        protocol = self._protocol(runtime, request_retry_count=3)
        cancel_calls: list[int] = []

        def cancel_check() -> None:
            cancel_calls.append(len(cancel_calls) + 1)
            # 第 1 次：请求开始前放行；第 2 次：空响应捕获后、重试前取消。
            if len(cancel_calls) == 2:
                raise KeyboardInterrupt("用户取消当前任务")

        with self.assertRaises(KeyboardInterrupt):
            protocol.request_reply(
                [{"role": "user", "content": "hi"}],
                on_delta=lambda _t: None,
                on_token_usage=lambda _i, _o, _c: None,
                on_protocol_wait=lambda: None,
                on_retry_status=lambda _msg: None,
                cancel_check=cancel_check,
            )

        self.assertEqual(len(cancel_calls), 2)
        # 不能继续执行第 2、3 次重试。
        self.assertEqual(runtime.turn_count, 1)

    def test_runtime_cancelled_error_propagates_without_retry(self) -> None:
        """Provider 抛出 CANCELLED 时，协议层必须原样传播且不重试。"""

        runtime = _CancelledRuntime(self._identity())
        protocol = self._protocol(runtime, request_retry_count=3)
        cancel_calls: list[int] = []

        def cancel_check() -> None:
            cancel_calls.append(len(cancel_calls) + 1)

        with self.assertRaises(ModelError) as raised:
            protocol.request_reply(
                [{"role": "user", "content": "hi"}],
                on_delta=lambda _t: None,
                on_token_usage=lambda _i, _o, _c: None,
                on_protocol_wait=lambda: None,
                on_retry_status=lambda _msg: None,
                cancel_check=cancel_check,
            )

        self.assertEqual(raised.exception.code, ModelErrorCode.CANCELLED)
        self.assertEqual(runtime.turn_count, 1)
        self.assertEqual(len(cancel_calls), 1)


class ProviderStreamExhaustionTests(unittest.TestCase):
    """Provider 层：流正常耗尽后必须执行结束后取消检查。"""

    def _identity(self, protocol: str, model_id: str = "demo-model") -> ModelIdentity:
        return ModelIdentity(
            profile_id="p1",
            provider="openai",
            protocol=protocol,
            model_id=model_id,
        )

    def test_openai_chat_stream_exhaustion_runs_cancel_check(self) -> None:
        """Chat Completions 流正常耗尽后，取消检查必须被调用。"""

        calls: list[int] = []

        def create(**kwargs):
            return iter([])  # 模拟被关闭的流：迭代器正常结束，无内容

        client = SimpleNamespace(
            chat=SimpleNamespace(
                completions=SimpleNamespace(create=create),
            ),
        )
        identity = self._identity("openai_chat_completions")
        profile = ProviderProfile(
            id="p1",
            provider="openai",
            base_url="https://example.test/v1",
            api_key="test-key",
            default_protocol="openai_chat_completions",
        )
        descriptor = ModelDescriptor(
            identity=identity,
            display_name=identity.model_id,
            capabilities=ModelCapabilities(streaming=True, tools=True),
            context_window_tokens=128000,
        )
        runtime = OpenAIChatCompletionsRuntime(
            identity=identity,
            capabilities=ModelCapabilities(streaming=True, tools=True),
            client=client,
            profile=profile,
            descriptor=descriptor,
            _owns_client=False,
        )
        request = ModelTurnRequest(
            identity=identity,
            system_prompt="sys",
            messages=(),
            generation_options=GenerationOptions(request_timeout_seconds=30),
            prompt_cache_identity={"workspace": "w"},
        )

        def cancel_check() -> None:
            calls.append(len(calls) + 1)
            raise KeyboardInterrupt("用户取消当前任务")

        with self.assertRaises(KeyboardInterrupt):
            list(runtime.stream_turn(request, cancel_check=cancel_check))

        self.assertEqual(calls, [1])

    def test_openai_responses_stream_exhaustion_runs_cancel_check(self) -> None:
        """Responses 流正常耗尽后，取消检查必须被调用。"""

        calls: list[int] = []

        def create(**kwargs):
            return iter([])

        client = SimpleNamespace(
            responses=SimpleNamespace(create=create),
        )
        identity = self._identity("openai_responses")
        profile = ProviderProfile(
            id="p1",
            provider="openai",
            base_url="https://example.test/v1",
            api_key="test-key",
            default_protocol="openai_responses",
        )
        descriptor = ModelDescriptor(
            identity=identity,
            display_name=identity.model_id,
            capabilities=ModelCapabilities(streaming=True, tools=True),
            context_window_tokens=128000,
        )
        runtime = OpenAIResponsesRuntime(
            identity=identity,
            capabilities=ModelCapabilities(streaming=True, tools=True),
            client=client,
            profile=profile,
            descriptor=descriptor,
            _owns_client=False,
        )
        request = ModelTurnRequest(
            identity=identity,
            system_prompt="sys",
            messages=(),
            generation_options=GenerationOptions(request_timeout_seconds=30),
            prompt_cache_identity={"workspace": "w"},
        )

        def cancel_check() -> None:
            calls.append(len(calls) + 1)
            raise KeyboardInterrupt("用户取消当前任务")

        with self.assertRaises(KeyboardInterrupt):
            list(runtime.stream_turn(request, cancel_check=cancel_check))

        self.assertEqual(calls, [1])


class AgentCancelContextTests(unittest.TestCase):
    """Agent 层：取消回合的内存历史与 Session 持久化一致性。"""

    def _build_agent(self, temp_dir: str) -> LocalToolAgent:
        workspace = Path(temp_dir)
        agent = object.__new__(LocalToolAgent)
        agent.workspace_root = workspace
        agent.config = SimpleNamespace(
            max_history_turns=6,
            max_tool_output_chars=6000,
        )
        agent._history = []
        agent._skill_manager = None
        agent._active_skills = []
        agent._tools = {
            "read": ToolDefinition(
                "read",
                "read",
                "{}",
                False,
                lambda _args: ToolResult(ok=True, output="文件内容"),
            ),
        }
        store = SessionStore(workspace / ".agent_sessions")
        state = store.start_session(workspace)
        agent._session_store = store
        agent._session_state = state
        return agent

    def test_immediate_cancel_before_persistence_records_user_message(self) -> None:
        """取消发生在首个检查点时，用户任务仍必须进入可恢复记录。

        修复前：``_history`` 为空、SessionStore 只有 ``session_started``；
        修复后：用户消息补写进 Session，``_history`` 保留任务与取消摘要。
        """

        import json

        with tempfile.TemporaryDirectory() as temp_dir:
            agent = self._build_agent(temp_dir)
            store = agent._session_store

            def cancel_check() -> None:
                raise KeyboardInterrupt("用户取消当前任务")

            with self.assertRaises(KeyboardInterrupt):
                LocalToolAgent.run_stream(
                    agent,
                    "快速取消",
                    lambda _delta: None,
                    cancel_check=cancel_check,
                )

            events = [
                json.loads(line)
                for line in agent._session_state.path.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]

        self.assertEqual(
            [message["content"] for message in agent._history],
            ["快速取消", "（上一回合被取消，未生成最终回复）"],
        )
        self.assertEqual(
            [event["type"] for event in events],
            ["session_started", "user_message", "turn_cancelled"],
        )
        self.assertEqual(events[-1]["payload"]["user_text"], "快速取消")

    def test_provider_cancelled_error_enters_turn_cancelled_history(self) -> None:
        """Provider 包装为 CANCELLED 的取消必须进入取消历史收尾。

        修复前：``_is_turn_cancel_exception`` 无法识别 CANCELLED，回合并入
        ``session_interrupted``，``_history`` 为空；修复后：走 ``turn_cancelled``。
        """

        import json

        with tempfile.TemporaryDirectory() as temp_dir:
            agent = self._build_agent(temp_dir)
            store = agent._session_store

            def fake_request(
                _messages,
                _on_delta,
                _on_token_usage,
                _on_protocol_wait,
                _on_retry_status,
                on_stream_rollback=None,
            ):
                raise ModelError(
                    code=ModelErrorCode.CANCELLED,
                    message="用户取消当前任务",
                )

            agent._request_agent_reply = fake_request  # type: ignore[method-assign]

            with self.assertRaises(ModelError):
                LocalToolAgent.run_stream(agent, "取消这一轮", lambda _delta: None)

            events = [
                json.loads(line)
                for line in agent._session_state.path.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]

        self.assertEqual(events[-1]["type"], "turn_cancelled")
        self.assertEqual(events[-1]["payload"]["user_text"], "取消这一轮")
        self.assertEqual(
            [message["content"] for message in agent._history],
            ["取消这一轮", "（上一回合被取消，未生成最终回复，未执行任何工具）"],
        )

    def test_followup_after_cancel_includes_previous_task(self) -> None:
        """取消后同一进程继续提问，新请求必须包含上一轮任务与取消摘要。"""

        seen_messages: list[list[dict]] = []
        request_count = 0

        def fake_request(
            messages,
            _on_delta,
            _on_token_usage,
            _on_protocol_wait,
            _on_retry_status,
            on_stream_rollback=None,
        ):
            nonlocal request_count
            request_count += 1
            if request_count == 1:
                raise KeyboardInterrupt("用户取消当前任务")
            seen_messages.append(messages)
            return AgentModelReply(
                message={"role": "assistant", "content": "继续完成"},
                content="继续完成",
            )

        with tempfile.TemporaryDirectory() as temp_dir:
            agent = self._build_agent(temp_dir)
            agent._request_agent_reply = fake_request  # type: ignore[method-assign]

            with self.assertRaises(KeyboardInterrupt):
                LocalToolAgent.run_stream(agent, "第一轮任务", lambda _delta: None)
            LocalToolAgent.run_stream(agent, "继续", lambda _delta: None)

        self.assertEqual(request_count, 2)
        all_contents = [
            str(message.get("content") or "")
            for message in seen_messages[0]
        ]
        self.assertTrue(
            any("第一轮任务" in content for content in all_contents),
            "后续请求未包含上一轮任务文本",
        )
        self.assertTrue(
            any("上一回合被取消" in content for content in all_contents),
            "后续请求未包含取消摘要",
        )

    def test_resume_after_cancel_matches_in_memory_history(self) -> None:
        """取消后通过 Session 恢复，投影出的历史必须与当前进程一致。"""

        import json

        with tempfile.TemporaryDirectory() as temp_dir:
            agent = self._build_agent(temp_dir)
            store = agent._session_store
            state = agent._session_state

            def fake_request(
                _messages,
                _on_delta,
                _on_token_usage,
                _on_protocol_wait,
                _on_retry_status,
                on_stream_rollback=None,
            ):
                raise KeyboardInterrupt("用户取消当前任务")

            agent._request_agent_reply = fake_request  # type: ignore[method-assign]

            with self.assertRaises(KeyboardInterrupt):
                LocalToolAgent.run_stream(agent, "取消的任务", lambda _delta: None)

            restored = store.load_session(state.session_id)

            # 事件流必须包含取消摘要，恢复投影才能复现进程内历史。
            events = [
                json.loads(line)
                for line in agent._session_state.path.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]
            cancelled = next(
                event for event in events if event["type"] == "turn_cancelled"
            )

        self.assertIn("summary", cancelled["payload"])
        self.assertEqual(
            [message["content"] for message in restored.messages],
            ["取消的任务", "（上一回合被取消，未生成最终回复，未执行任何工具）"],
        )
        self.assertEqual(
            [message["content"] for message in agent._history],
            [message["content"] for message in restored.messages],
        )


if __name__ == "__main__":
    unittest.main()
