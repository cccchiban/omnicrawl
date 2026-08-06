from __future__ import annotations

import io
import json
import logging
import tarfile
import tempfile
import threading
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

from omnicrawl.agent.core import AgentError, LocalToolAgent
from omnicrawl.agent.llm_protocol import AgentLLMProtocol, AgentProtocolError
from omnicrawl.api.models import APIServiceError
from omnicrawl.api.service import AgentAPIService
from omnicrawl.config.llm import LLMConfig
from omnicrawl.config.llm_multi import apply_model_selection, load_multi_model_llm_config
from omnicrawl.extensions.plugin_install import (
    extract_tarball,
    install_from_local_package,
    install_from_npm,
    uninstall_plugin,
)
from omnicrawl.extensions.plugin_models import (
    PluginRecord,
    PluginsConfig,
    PluginRegistryDocument,
    PluginVersionRef,
    parse_plugins_config,
)
from omnicrawl.extensions.plugin_manager import PluginManager
from omnicrawl.extensions.plugin_registry import save_registry_document
from omnicrawl.llm.capabilities import ModelCapabilities
from omnicrawl.llm.errors import ModelError, ModelErrorCode
from omnicrawl.llm.protocol import (
    ConversationMessage,
    GenerationOptions,
    ModelIdentity,
    ModelTurnRequest,
    ResponseCompleted,
    TextBlock,
    TextDelta,
    ToolCallBlock,
    ToolResultBlock,
)
from omnicrawl.llm.providers.gemini import _to_gemini_contents
from omnicrawl.llm.providers.openai_responses import OpenAIResponsesRuntime
from omnicrawl.llm.registry import ModelDescriptor, ProviderProfile
from omnicrawl.llm.runtime import ModelRuntimeManager


class _InterruptingRuntime:
    def __init__(self, identity: ModelIdentity) -> None:
        self.identity = identity
        self.capabilities = ModelCapabilities(streaming=True, tools=True)
        self.calls = 0

    def stream_turn(self, request, *, cancel_check=None):
        self.calls += 1
        yield TextDelta(text="partial")
        raise ModelError(
            code=ModelErrorCode.STREAM_INTERRUPTED,
            message="connection reset",
            retryable=True,
        )

    def close(self) -> None:
        pass


class _ResponsesAPI:
    def __init__(self, events):
        self._events = events

    def create(self, **_kwargs):
        return iter(self._events)


class _ResponsesClient:
    def __init__(self, events):
        self.responses = _ResponsesAPI(events)


class _BlockingAgent:
    def __init__(self) -> None:
        self.release = threading.Event()
        self.closed = False
        self.running_when_closed = False
        self._confirm = None

    def set_confirm_handler(self, callback) -> None:
        self._confirm = callback

    def run_stream(self, _message, _on_delta, **callbacks):
        while not self.release.wait(0.01):
            try:
                callbacks["cancel_check"]()
            except Exception:
                break
        self.running_when_closed = self.closed
        return "done"

    def close(self) -> None:
        self.closed = True
        self.release.set()


class _InterruptThenSucceedRuntime:
    """第一次流式输出部分内容后中断，第二次完整成功。"""

    def __init__(self, identity: ModelIdentity) -> None:
        self.identity = identity
        self.capabilities = ModelCapabilities(streaming=True, tools=True)
        self.calls = 0

    def stream_turn(self, request, *, cancel_check=None):
        self.calls += 1
        if self.calls == 1:
            yield TextDelta(text="partial")
            raise ModelError(
                code=ModelErrorCode.STREAM_INTERRUPTED,
                message="connection reset",
                retryable=True,
            )
        yield TextDelta(text="完整回复")
        yield ResponseCompleted(finish_reason="stop")

    def close(self) -> None:
        pass


class ReviewRegressionTests(unittest.TestCase):
    def _profile_descriptor(self):
        identity = ModelIdentity(
            profile_id="p1",
            provider="openai",
            protocol="openai_responses",
            model_id="demo",
        )
        profile = ProviderProfile(
            id="p1",
            provider="openai",
            api_key="test",
            default_protocol="openai_responses",
        )
        descriptor = ModelDescriptor(
            identity=identity,
            display_name="demo",
            capabilities=ModelCapabilities(streaming=True, tools=True),
        )
        return identity, profile, descriptor

    def test_runtime_does_not_retry_after_visible_text(self) -> None:
        identity, profile, descriptor = self._profile_descriptor()
        runtime = _InterruptingRuntime(identity)
        manager = ModelRuntimeManager()
        manager.bootstrap(profile, descriptor, runtime=runtime)
        retries: list[str] = []
        protocol = AgentLLMProtocol(
            client=None,
            model="demo",
            request_timeout_seconds=10,
            request_retry_count=2,
            workspace_root=Path.cwd(),
            system_prompt_provider=lambda: "system",
            prompt_cache_identity_provider=lambda: {},
            tools_provider=lambda: [],
            extra_body_provider=lambda: {},
            tool_name_from_function_name=lambda name: name,
            function_name_for_tool=lambda name: name,
            runtime_manager=manager,
        )

        with self.assertRaises(AgentProtocolError):
            protocol.request_reply(
                [{"role": "user", "content": "hello"}],
                on_delta=lambda _text: None,
                on_token_usage=lambda *_args: None,
                on_protocol_wait=lambda: None,
                on_retry_status=retries.append,
            )

        self.assertEqual(runtime.calls, 1)
        self.assertEqual(retries, [])

    def test_stream_interrupt_retries_when_rollback_provided(self) -> None:
        """提供 on_stream_rollback 时，已输出内容后的流式中断应回滚并自动重试。"""
        identity, profile, descriptor = self._profile_descriptor()
        runtime = _InterruptThenSucceedRuntime(identity)
        manager = ModelRuntimeManager()
        manager.bootstrap(profile, descriptor, runtime=runtime)
        retries: list[str] = []
        rollbacks: list[str] = []
        protocol = AgentLLMProtocol(
            client=None,
            model="demo",
            request_timeout_seconds=10,
            request_retry_count=2,
            workspace_root=Path.cwd(),
            system_prompt_provider=lambda: "system",
            prompt_cache_identity_provider=lambda: {},
            tools_provider=lambda: [],
            extra_body_provider=lambda: {},
            tool_name_from_function_name=lambda name: name,
            function_name_for_tool=lambda name: name,
            runtime_manager=manager,
        )

        deltas: list[str] = []
        reply = protocol.request_reply(
            [{"role": "user", "content": "hello"}],
            on_delta=lambda text: deltas.append(text),
            on_token_usage=lambda *_args: None,
            on_protocol_wait=lambda: None,
            on_retry_status=retries.append,
            on_stream_rollback=lambda: rollbacks.append("rollback"),
        )

        self.assertEqual(runtime.calls, 2)
        self.assertEqual(rollbacks, ["rollback"])
        self.assertEqual(len(retries), 1)
        self.assertIn("撤销已显示内容", retries[0])
        self.assertEqual(reply.content, "完整回复")
        # 重试成功后只保留新回复的增量，半截旧内容不得拼接进历史。
        self.assertEqual(deltas, ["partial", "完整回复"])
        manager.close()

    def test_stream_interrupt_no_rollback_no_retry_keeps_error(self) -> None:
        """未提供 on_stream_rollback 时保持既有语义：可见输出后中断直接失败。"""
        identity, profile, descriptor = self._profile_descriptor()
        runtime = _InterruptingRuntime(identity)
        manager = ModelRuntimeManager()
        manager.bootstrap(profile, descriptor, runtime=runtime)
        protocol = AgentLLMProtocol(
            client=None,
            model="demo",
            request_timeout_seconds=10,
            request_retry_count=3,
            workspace_root=Path.cwd(),
            system_prompt_provider=lambda: "system",
            prompt_cache_identity_provider=lambda: {},
            tools_provider=lambda: [],
            extra_body_provider=lambda: {},
            tool_name_from_function_name=lambda name: name,
            function_name_for_tool=lambda name: name,
            runtime_manager=manager,
        )
        with self.assertRaises(AgentProtocolError) as ctx:
            protocol.request_reply(
                [{"role": "user", "content": "hello"}],
                on_delta=lambda _text: None,
                on_token_usage=lambda *_args: None,
                on_protocol_wait=lambda: None,
                on_retry_status=lambda _message: None,
            )
        self.assertIn("模型流中断", str(ctx.exception))
        self.assertEqual(runtime.calls, 1)
        manager.close()

    def test_openai_responses_deduplicates_done_and_completed_function_call(self) -> None:
        identity, profile, descriptor = self._profile_descriptor()
        item = {
            "type": "function_call",
            "call_id": "call-1",
            "name": "read_file",
            "arguments": '{"path":"a.txt"}',
        }
        events = [
            {"type": "response.output_item.done", "item": item},
            {
                "type": "response.completed",
                "response": {"status": "completed", "output": [item]},
            },
        ]
        runtime = OpenAIResponsesRuntime(
            identity=identity,
            capabilities=descriptor.capabilities,
            client=_ResponsesClient(events),
            profile=profile,
            descriptor=descriptor,
            _owns_client=False,
        )
        request = ModelTurnRequest(
            identity=identity,
            system_prompt="system",
            messages=(ConversationMessage(role="user", blocks=(TextBlock("hi"),)),),
            generation_options=GenerationOptions(),
        )

        emitted = list(runtime.stream_turn(request))
        completed = [event for event in emitted if event.__class__.__name__ == "ToolCallCompleted"]
        self.assertEqual(len(completed), 1)

    def test_gemini_function_response_uses_original_function_name(self) -> None:
        contents = _to_gemini_contents(
            (
                ConversationMessage(
                    role="assistant",
                    blocks=(
                        ToolCallBlock(
                            call_id="gemini-1",
                            name="read_file",
                            arguments={"path": "a.txt"},
                        ),
                    ),
                ),
                ConversationMessage(
                    role="tool",
                    blocks=(ToolResultBlock(call_id="gemini-1", ok=True, content="ok"),),
                ),
            )
        )
        response = contents[1]["parts"][0]["function_response"]
        self.assertEqual(response["name"], "read_file")

    def test_multi_model_config_rejects_protocol_provider_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            models = Path(temp_dir) / "models.yaml"
            models.write_text(
                """version: 1
models:
  bad:
    profile: openai-main
    model_id: gemini-demo
    protocol: gemini_generate_content
""",
                encoding="utf-8",
            )
            section = {
                "profiles": {
                    "openai-main": {
                        "provider": "openai",
                        "api_key": "test",
                    }
                },
                "active_model": {"source": "custom", "key": "bad"},
            }
            with mock.patch.dict("os.environ", {"AI_MODELS_FILE": str(models)}, clear=False):
                with self.assertRaisesRegex(Exception, "Provider|provider|协议"):
                    load_multi_model_llm_config(section)

    def test_extract_tarball_rejects_symlink_member(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            tar_path = root / "plugin.tgz"
            with tarfile.open(tar_path, "w:gz") as archive:
                package = tarfile.TarInfo("package")
                package.type = tarfile.DIRTYPE
                archive.addfile(package)
                link = tarfile.TarInfo("package/link")
                link.type = tarfile.SYMTYPE
                link.linkname = "../../outside"
                archive.addfile(link)
            with self.assertRaises(Exception):
                extract_tarball(tar_path, root / "extract")

    def test_bootstrap_rejects_tampered_plugin_store(self) -> None:
        fixture = Path(__file__).parent / "fixtures" / "npm_plugins" / "sample-observe"
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            store = root / "store"
            registry = root / "plugins.json"
            with mock.patch(
                "omnicrawl.extensions.plugin_install.project_registry_path",
                return_value=registry,
            ), mock.patch(
                "omnicrawl.extensions.plugin_install.detect_node_npm",
                return_value=("node", "npm"),
            ), mock.patch(
                "omnicrawl.extensions.plugin_install.smoke_test_worker"
            ):
                result = install_from_local_package(
                    fixture,
                    scope="project",
                    workspace_root=root,
                    enable=True,
                    store_root=store,
                )
            entry = Path(result.store_path) / "dist" / "plugin.js"
            entry.write_text(entry.read_text(encoding="utf-8") + "\n// tampered\n", encoding="utf-8")
            manager = PluginManager(
                workspace_root=root,
                config=PluginsConfig(enabled=True),
                project_registry=registry,
                user_registry=root / "user.json",
                store_root=store,
            )
            diagnostics = manager.bootstrap()
            self.assertTrue(any("内容哈希不匹配" in item for item in diagnostics), diagnostics)
            self.assertEqual(manager.list_status(), [])

    def test_workspace_metadata_is_isolated_per_plugin(self) -> None:
        from omnicrawl.extensions.plugin_manager import HookDispatcher, PluginWorkerState
        from omnicrawl.extensions.plugin_models import HandlerRegistration, PluginManifest, ResolvedHandler

        class Client:
            def __init__(self) -> None:
                self.events = []

            def invoke_handler(self, *, handler_id, event, timeout_ms):
                self.events.append(event)
                return {"action": "continue"}

        dispatcher = HookDispatcher(config=PluginsConfig(enabled=True))
        handlers = []
        workers = {}
        for index, permissions in enumerate((("hook:turn.end", "workspace:metadata"), ("hook:turn.end",))):
            name = f"@demo/p{index}"
            client = Client()
            manifest = PluginManifest(
                name=name,
                version="1.0.0",
                api_version="1",
                entry="dist/plugin.js",
                permissions=permissions,
                hooks=(HandlerRegistration(id="h", hook="turn.end", mode="notify"),),
                engines_omnicrawl=">=0.1",
                engines_node=">=20",
            )
            workers[name] = PluginWorkerState(
                name=name,
                manifest=manifest,
                scope="user",
                record=PluginRecord(name=name, enabled=True, approved_permissions=list(permissions)),
                root=Path.cwd(),
                client=client,
                active=True,
            )
            handlers.append(
                ResolvedHandler(
                    key=f"{name}/h",
                    plugin_name=name,
                    plugin_version="1.0.0",
                    handler_id="h",
                    hook="turn.end",
                    mode="notify",
                    priority=0,
                    scope="user",
                    timeout_ms=100,
                )
            )
        dispatcher.set_execution_plan(handlers, workers)
        dispatcher.dispatch(
            "turn.end",
            {"userText": "x", "assistantText": "y"},
            workspace={"id": "w", "root": "D:/secret-workspace"},
        )
        self.assertIn("root", workers["@demo/p0"].client.events[0]["workspace"])
        self.assertNotIn("root", workers["@demo/p1"].client.events[0]["workspace"])

    def test_purge_rejects_store_path_outside_store_root(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            outside = root / "outside"
            outside.mkdir()
            (outside / "sentinel.txt").write_text("keep", encoding="utf-8")
            registry = root / "registry.json"
            record = PluginRecord(
                name="@demo/plugin",
                enabled=False,
                active=PluginVersionRef(
                    version="1.0.0",
                    integrity="sha512-x",
                    lockfile_hash="",
                    source="test",
                    store_path=str(outside),
                ),
            )
            document = PluginRegistryDocument(plugins={record.name: record})
            save_registry_document(registry, document)
            with mock.patch(
                "omnicrawl.extensions.plugin_install._load_scope_registry",
                return_value=(registry, document),
            ), mock.patch(
                "omnicrawl.extensions.plugin_install.user_store_root",
                return_value=root / "store",
            ):
                with self.assertRaises(Exception):
                    uninstall_plugin(record.name, purge=True)
            self.assertTrue((outside / "sentinel.txt").exists())

    def test_api_close_timeout_defers_agent_close_until_worker_exits(self) -> None:
        class IgnoringAgent(_BlockingAgent):
            def run_stream(self, _message, _on_delta, **_callbacks):
                self.release.wait(timeout=1)
                self.running_when_closed = self.closed
                return "done"

        agent = IgnoringAgent()
        service = AgentAPIService(agent, close_timeout_seconds=0.05)
        run = service.start_run("slow")
        for _ in range(100):
            if run.status == "running":
                break
            time.sleep(0.01)
        service.close()
        self.assertFalse(agent.closed)
        agent.release.set()
        for _ in range(100):
            if agent.closed:
                break
            time.sleep(0.01)
        self.assertTrue(agent.closed)
        self.assertFalse(agent.running_when_closed)

    def test_api_close_waits_for_worker_and_rejects_new_run(self) -> None:
        agent = _BlockingAgent()
        service = AgentAPIService(agent)
        run = service.start_run("hello")
        for _ in range(100):
            if run.status == "running":
                break
            time.sleep(0.01)

        service.close()

        self.assertFalse(agent.running_when_closed)
        with self.assertRaises(APIServiceError) as ctx:
            service.start_run("after close")
        self.assertEqual(ctx.exception.code, "SERVICE_CLOSED")

    def test_plugin_update_requires_explicit_new_permission_confirmation(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            registry = root / "plugins.json"
            existing = PluginRecord(
                name="@demo/plugin",
                enabled=True,
                approved_permissions=["hook:turn.end"],
            )
            document = PluginRegistryDocument(plugins={existing.name: existing})
            package_json = {
                "name": existing.name,
                "version": "1.0.0",
                "type": "module",
                "main": "dist/plugin.js",
                "engines": {"node": ">=20"},
                "omnicrawl": {
                    "apiVersion": "1",
                    "entry": "dist/plugin.js",
                    "permissions": ["hook:turn.end", "workspace:metadata"],
                    "hooks": [{"id": "h", "hook": "turn.end", "mode": "notify"}],
                    "engines": {"omnicrawl": ">=0.1", "node": ">=20"},
                },
            }
            with mock.patch(
                "omnicrawl.extensions.plugin_install.resolve_exact_version",
                return_value=("1.0.0", {"dist": {"tarball": "https://example/p.tgz", "integrity": "sha512-x"}}),
            ), mock.patch(
                "omnicrawl.extensions.plugin_install.download_tarball",
                return_value=b"data",
            ), mock.patch(
                "omnicrawl.extensions.plugin_install._sha512_integrity",
                return_value="sha512-x",
            ), mock.patch(
                "omnicrawl.extensions.plugin_install.extract_tarball",
                return_value=root / "package",
            ), mock.patch(
                "omnicrawl.extensions.plugin_install.parse_plugin_manifest"
            ) as parse_manifest, mock.patch(
                "omnicrawl.extensions.plugin_install.npm_install_production"
            ), mock.patch(
                "omnicrawl.extensions.plugin_install._load_scope_registry",
                return_value=(registry, document),
            ), mock.patch(
                "omnicrawl.extensions.plugin_install.detect_node_npm",
                return_value=("node", "npm"),
            ):
                package = root / "package"
                package.mkdir()
                (package / "package-lock.json").write_text("{}", encoding="utf-8")
                (package / "package.json").write_text(json.dumps(package_json), encoding="utf-8")
                (package / "dist").mkdir()
                (package / "dist" / "plugin.js").write_text("export default {}", encoding="utf-8")
                from omnicrawl.extensions.plugin_models import parse_plugin_manifest as real_parse
                parse_manifest.return_value = real_parse(package_json)
                with self.assertRaises(Exception):
                    install_from_npm(
                        existing.name,
                        scope="user",
                        enable=True,
                        yes=True,
                        confirm=None,
                    )

    def test_api_event_cursor_reports_expired_window(self) -> None:
        class FastAgent(_BlockingAgent):
            def run_stream(self, _message, on_delta, **_callbacks):
                for index in range(20):
                    on_delta(str(index))
                return "ok"

        service = AgentAPIService(FastAgent(), max_events_per_run=10)
        run = service.start_run("events")
        for _ in range(100):
            if run.status == "completed":
                break
            time.sleep(0.01)
        with self.assertRaises(APIServiceError) as ctx:
            service.events_after(run, 1)
        self.assertEqual(ctx.exception.code, "EVENT_CURSOR_EXPIRED")
        service.close()

    def test_api_run_retention_is_bounded(self) -> None:
        class FastAgent(_BlockingAgent):
            def run_stream(self, _message, _on_delta, **_callbacks):
                return "ok"

        service = AgentAPIService(FastAgent(), max_retained_runs=3)
        run_ids = []
        for index in range(5):
            run = service.start_run(str(index))
            run_ids.append(run.run_id)
            for _ in range(100):
                if run.status in {"completed", "failed", "cancelled"}:
                    break
                time.sleep(0.01)
        self.assertLessEqual(len(service._runs), 3)
        service.close()

    def test_guard_hook_infra_exception_is_fail_closed(self) -> None:
        agent = object.__new__(LocalToolAgent)

        class BrokenManager:
            def dispatch(self, *_args, **_kwargs):
                raise RuntimeError("worker crashed")

        agent._plugin_manager = BrokenManager()
        denied = LocalToolAgent._dispatch_plugin_hook(
            agent,
            "tool.execute.before",
            {"tool": "bash", "arguments": {}},
            session_id="s1",
        )
        self.assertIsNone(denied)

        allowed = LocalToolAgent._dispatch_plugin_hook(
            agent,
            "turn.end",
            {"userText": "x", "assistantText": "y"},
            session_id="s1",
        )
        self.assertEqual(allowed["userText"], "x")

    def test_set_model_does_not_swallow_llm_error(self) -> None:
        from omnicrawl.config.llm import LLMError as ConfigLLMError

        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(
            llm=LLMConfig(
                api_key="k",
                base_url="https://example.test/v1",
                model="demo",
                provider="openai",
                protocol="openai_chat_completions",
                profile_id="p1",
            )
        )
        agent._runtime_manager = None
        with mock.patch(
            "omnicrawl.agent.core.apply_model_selection",
            side_effect=ConfigLLMError("models.yaml 损坏"),
        ):
            with self.assertRaisesRegex(AgentError, "models.yaml 损坏"):
                LocalToolAgent.set_model(agent, "broken-key")

    def test_apply_model_selection_rejects_unknown_profile_when_profiles_exist(self) -> None:
        config = LLMConfig(
            api_key="k",
            base_url="https://example.test/v1",
            model="demo",
            provider="openai",
            protocol="openai_chat_completions",
            profile_id="openai-main",
        )
        fake_profile = object()
        with mock.patch(
            "omnicrawl.config.llm_multi.load_model_store"
        ) as load_store, mock.patch(
            "omnicrawl.config.llm_multi._profiles_from_disk",
            return_value={"openai-main": fake_profile},
        ):
            store = mock.Mock()
            store.resolve_alias.return_value = None
            load_store.return_value = store
            with self.assertRaisesRegex(Exception, "Profile 不存在|未启用"):
                apply_model_selection(config, "missing-profile/demo-model")

    def test_network_install_defaults_to_false(self) -> None:
        self.assertFalse(PluginsConfig().allow_network_install)
        self.assertFalse(parse_plugins_config({}).allow_network_install)
        self.assertFalse(parse_plugins_config({"enabled": True}).allow_network_install)
        self.assertTrue(
            parse_plugins_config({"allow_network_install": True}).allow_network_install
        )

    def test_prepare_workspace_switch_failure_keeps_old_root(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            old_root = root / "old"
            new_root = root / "new"
            old_root.mkdir()
            new_root.mkdir()

            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = old_root
            agent.config = SimpleNamespace(
                temp_workspace=SimpleNamespace(),
                session_enabled=False,
                memory_enabled=False,
                skills_enabled=False,
                skill_paths=(),
            )
            agent._session_store = None
            agent._session_state = None
            agent._project_store = None
            agent._memory_store = None
            agent._mcp_manager = object()
            agent._tools = {"keep": True}
            agent._plugin_manager = None
            agent._on_workspace_switched = None
            agent._dispatch_plugin_hook = lambda *args, **kwargs: {}

            with mock.patch(
                "omnicrawl.agent.core.AgentTempWorkspace",
                side_effect=RuntimeError("temp init failed"),
            ):
                with self.assertRaises(AgentError):
                    LocalToolAgent._prepare_workspace_switch(agent, new_root)

            self.assertEqual(agent.workspace_root, old_root)
            self.assertEqual(agent._tools, {"keep": True})


if __name__ == "__main__":
    unittest.main()
