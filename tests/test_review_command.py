"""`/review` 斜杠命令：主线程转发到评审子 Agent，并渲染结构化 JSON 结果。"""

from __future__ import annotations

import unittest
from pathlib import Path

from omnicrawl.commands.slash import (
    _build_review_task_prompt,
    format_review_report,
    handle_review_command,
)


class ReviewPromptTests(unittest.TestCase):
    def test_default_prompt_mentions_working_tree_scope(self) -> None:
        prompt = _build_review_task_prompt("")
        self.assertIn("工作区未提交改动", prompt)
        self.assertIn("git status", prompt)
        self.assertIn("git diff", prompt)
        self.assertIn("OUTPUT FORMAT", prompt)

    def test_scoped_prompt_forwards_given_range(self) -> None:
        prompt = _build_review_task_prompt("HEAD~3")
        self.assertIn("`HEAD~3`", prompt)
        self.assertIn("git diff <范围>", prompt)


class ReviewReportTests(unittest.TestCase):
    def test_renders_findings_and_verdict(self) -> None:
        report = format_review_report(
            '{"findings": ['
            '{"title": "Off-by-one in loop", "body": "Loop exits one item early.", '
            '"confidence_score": 0.9, "priority": 1, '
            '"code_location": {"absolute_file_path": "C:/app/main.py", '
            '"line_range": {"start": 12, "end": 14}}}], '
            '"overall_correctness": "patch is incorrect", '
            '"overall_explanation": "Off-by-one breaks the final element.", '
            '"overall_confidence_score": 0.85}'
        )

        self.assertIn("patch is incorrect ❌", report)
        self.assertIn("[P1]", report)
        self.assertIn("Off-by-one in loop", report)
        self.assertIn("`C:/app/main.py` 行 12-14", report)
        self.assertIn("Loop exits one item early.", report)
        self.assertIn("Off-by-one breaks the final element.", report)

    def test_renders_correct_verdict_without_findings(self) -> None:
        report = format_review_report(
            '{"findings": [], "overall_correctness": "patch is correct", '
            '"overall_explanation": "No issues found.", '
            '"overall_confidence_score": 0.95}'
        )

        self.assertIn("patch is correct ✅", report)
        self.assertIn("未发现问题", report)

    def test_falls_back_to_raw_text_when_not_json(self) -> None:
        report = format_review_report("评审子 Agent 没有返回 JSON，只返回了一段文字。")

        self.assertEqual(report, "评审子 Agent 没有返回 JSON，只返回了一段文字。")

    def test_missing_or_invalid_correctness_shows_undetermined(self) -> None:
        """#18：overall_correctness 缺失/非法时不得默认判定为通过。"""

        missing = format_review_report('{"findings": []}')
        self.assertIn("无法判定", missing)
        self.assertNotIn("patch is correct ✅", missing)

        invalid = format_review_report(
            '{"findings": [], "overall_correctness": "maybe"}'
        )
        self.assertIn("无法判定", invalid)
        self.assertNotIn("patch is correct ✅", invalid)

    def test_non_list_findings_shows_warning_not_clean_bill(self) -> None:
        """#19：findings 非数组时不得显示"未发现问题"（避免漏报）。"""

        report = format_review_report(
            '{"findings": "部分问题：\\n- 竞态\\n- 越界", '
            '"overall_correctness": "patch is incorrect"}'
        )
        self.assertIn("缺少有效的 findings 列表", report)
        self.assertNotIn("未发现问题", report)


class ReviewCommandTests(unittest.TestCase):
    def setUp(self) -> None:
        class FakeAgent:
            def __init__(self) -> None:
                self.calls: list[tuple[str, str, str]] = []
                self.result = '{"findings": [], "overall_correctness": "patch is correct"}'

            def run_subagent_task(
                self,
                *,
                agent_type: str,
                description: str,
                prompt: str,
                on_subagent_event=None,
            ) -> str:
                self.calls.append((agent_type, description, prompt, on_subagent_event))
                return self.result

        self.agent = FakeAgent()

    def test_ignores_non_review_commands(self) -> None:
        self.assertIsNone(handle_review_command(self.agent, "/reasoning"))
        self.assertIsNone(handle_review_command(self.agent, "普通消息"))
        self.assertEqual(self.agent.calls, [])

    def test_dispatches_review_subagent_and_renders(self) -> None:
        reply = handle_review_command(self.agent, "/review")

        self.assertEqual(len(self.agent.calls), 1)
        agent_type, description, prompt, on_subagent_event = self.agent.calls[0]
        self.assertEqual(agent_type, "review")
        self.assertIn("git diff", prompt)
        self.assertIn("patch is correct ✅", reply)
        self.assertIsNone(on_subagent_event)

    def test_forwards_progress_callback(self) -> None:
        """/review 把 UI 提供的子任务进度回调透传给 run_subagent_task。"""

        captured = []

        def progress(event_name: str, payload: dict) -> None:
            captured.append((event_name, payload))

        reply = handle_review_command(
            self.agent,
            "/review",
            on_subagent_event=progress,
        )

        self.assertIn("patch is correct ✅", reply)
        _agent_type, _description, _prompt, on_subagent_event = self.agent.calls[0]
        self.assertIs(on_subagent_event, progress)

    def test_forwards_scope_argument(self) -> None:
        handle_review_command(self.agent, "/review main...feature")

        agent_type, _description, prompt, _progress = self.agent.calls[0]
        self.assertEqual(agent_type, "review")
        self.assertIn("`main...feature`", prompt)

    def test_surfaces_subagent_error(self) -> None:
        class FailingAgent:
            def run_subagent_task(self, **kwargs) -> str:
                from omnicrawl.agent import AgentError

                raise AgentError("评审子任务执行失败。")

        reply = handle_review_command(FailingAgent(), "/review")

        self.assertIn("评审失败", reply)
        self.assertIn("评审子任务执行失败", reply)


class ReviewPreconditionTests(unittest.TestCase):
    """`/review` 前置检查（#9/#10）：非 git 仓库与无改动场景。"""

    def setUp(self) -> None:
        import shutil
        import tempfile

        self._tmp = tempfile.mkdtemp(prefix="review-precheck-")
        self.root = Path(self._tmp)

    def tearDown(self) -> None:
        import shutil

        shutil.rmtree(self._tmp, ignore_errors=True)

    def _git(self, *args: str) -> None:
        import subprocess

        subprocess.run(
            ["git", "-c", "user.name=test", "-c", "user.email=test@example.com", *args],
            cwd=str(self.root),
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

    def _init_repo(self) -> None:
        self._git("init", "-q")
        self._git("config", "user.name", "test")
        self._git("config", "user.email", "test@example.com")

    def test_rejects_non_git_workspace(self) -> None:
        from omnicrawl.commands.slash import _check_review_preconditions

        error = _check_review_preconditions(self.root, "")
        self.assertIn("不是 git 仓库", error)

    def test_rejects_clean_working_tree_for_default_scope(self) -> None:
        from omnicrawl.commands.slash import _check_review_preconditions

        self._init_repo()
        (self.root / "a.txt").write_text("content")
        self._git("add", "a.txt")
        self._git("commit", "-qm", "init")

        error = _check_review_preconditions(self.root, "")
        self.assertIn("没有未提交改动", error)

    def test_accepts_dirty_working_tree_for_default_scope(self) -> None:
        from omnicrawl.commands.slash import _check_review_preconditions

        self._init_repo()
        (self.root / "a.txt").write_text("content")
        self._git("add", "a.txt")
        self._git("commit", "-qm", "init")
        (self.root / "a.txt").write_text("changed")

        self.assertIsNone(_check_review_preconditions(self.root, ""))

    def test_rejects_scoped_review_without_commits(self) -> None:
        from omnicrawl.commands.slash import _check_review_preconditions

        self._init_repo()

        error = _check_review_preconditions(self.root, "HEAD~3")
        self.assertIn("还没有任何提交", error)

    def test_accepts_scoped_review_with_commits(self) -> None:
        from omnicrawl.commands.slash import _check_review_preconditions

        self._init_repo()
        (self.root / "a.txt").write_text("content")
        self._git("add", "a.txt")
        self._git("commit", "-qm", "init")

        self.assertIsNone(_check_review_preconditions(self.root, "HEAD"))

    def test_missing_git_binary_returns_install_message(self) -> None:
        import os
        import sys

        from omnicrawl.commands.slash import _check_review_preconditions

        if sys.platform == "win32":
            self.skipTest("Windows 下 PATH 替换不可靠，跳过。")
        old_path = os.environ.get("PATH", "")
        os.environ["PATH"] = "/nonexistent"
        try:
            error = _check_review_preconditions(self.root, "")
            self.assertIn("git 可执行文件", error)
        finally:
            os.environ["PATH"] = old_path

    def test_handler_returns_precheck_error_without_dispatching(self) -> None:
        class FakeAgent:
            workspace_root = None

            def run_subagent_task(self, **kwargs) -> str:
                raise AssertionError("前置检查失败时不应派生子 Agent")

        # workspace_root 为 None 时跳过前置检查（兼容无路径的测试夹具）。
        agent = FakeAgent()
        agent.workspace_root = self.root
        from omnicrawl.commands.slash import handle_review_command

        reply = handle_review_command(agent, "/review")
        self.assertIn("不是 git 仓库", reply)


class ReviewIntoParentContextTests(unittest.TestCase):
    """评审报告进父模型上下文：父模型可自主调用评审并收到完整报告。"""

    def _coordinator(self, run_impl):
        class _Registry:
            def get(self, name):
                return type("D", (), {"name": name})() if name == "review" else None

        class _Coordinator:
            registry = _Registry()

            def run(self, arguments, **kwargs):
                return run_impl(arguments, kwargs)

        return _Coordinator()

    def test_tool_subagent_renders_review_report_for_parent_model(self) -> None:
        """父模型调用 subagent 工具（review 角色）时返回渲染后的完整报告。"""

        import json

        from omnicrawl.agent import LocalToolAgent

        agent = object.__new__(LocalToolAgent)
        review_json = (
            '{"findings": [{"title": "竞态条件", "priority": 1, '
            '"body": "共享变量无锁访问。", "code_location": {'
            '"absolute_file_path": "C:/app/main.py", '
            '"line_range": {"start": 1, "end": 3}}}], '
            '"overall_correctness": "patch is incorrect", '
            '"overall_explanation": "存在并发问题。", '
            '"overall_confidence_score": 0.9}'
        )

        def run_impl(arguments, kwargs):
            self.assertTrue(kwargs.get("keep_full_text"))
            payload = {
                "status": "completed",
                "results": [
                    {
                        "status": "completed",
                        "summary": "截断摘要",
                        "full_text": review_json,
                        "error": None,
                    }
                ],
            }
            return type(
                "R",
                (),
                {"ok": True, "output": json.dumps(payload, ensure_ascii=False)},
            )()

        agent._subagent_coordinator = self._coordinator(run_impl)
        result = agent._tool_subagent(
            {
                "action": "run",
                "tasks": [
                    {
                        "description": "评审",
                        "prompt": "p",
                        "subagent_type": "review",
                    }
                ],
            }
        )

        self.assertTrue(result.ok)
        self.assertIn("patch is incorrect ❌", result.output)
        self.assertIn("竞态条件", result.output)
        self.assertIn("`C:/app/main.py` 行 1-3", result.output)
        self.assertNotIn("截断摘要", result.output)

    def test_tool_subagent_passes_through_non_review_tasks(self) -> None:
        """非 review 角色保持原行为（不透传 full_text，不渲染报告）。"""

        import json

        from omnicrawl.agent import LocalToolAgent

        agent = object.__new__(LocalToolAgent)

        def run_impl(arguments, kwargs):
            self.assertFalse(kwargs.get("keep_full_text"))
            payload = {
                "status": "completed",
                "results": [{"status": "completed", "summary": "explore 摘要"}],
            }
            return type(
                "R",
                (),
                {"ok": True, "output": json.dumps(payload, ensure_ascii=False)},
            )()

        agent._subagent_coordinator = self._coordinator(run_impl)
        result = agent._tool_subagent(
            {
                "action": "run",
                "tasks": [
                    {
                        "description": "探索",
                        "prompt": "p",
                        "subagent_type": "explore",
                    }
                ],
            }
        )

        self.assertTrue(result.ok)
        self.assertIn("explore 摘要", result.output)

    def test_handle_review_command_injects_report_into_context(self) -> None:
        """/review 成功后报告以 assistant 消息注入 _history 与会话事件。"""

        from omnicrawl.agent import LocalToolAgent

        class FakeAgent:
            workspace_root = None
            remembered: list[str] = []

            def run_subagent_task(self, **kwargs) -> str:
                return '{"findings": [], "overall_correctness": "patch is correct"}'

            def remember_review_report(self, report: str) -> None:
                self.remembered.append(report)

        agent = FakeAgent()
        from omnicrawl.commands.slash import handle_review_command

        reply = handle_review_command(agent, "/review")

        self.assertIn("patch is correct ✅", reply)
        self.assertEqual(len(agent.remembered), 1)
        self.assertIn("patch is correct ✅", agent.remembered[0])

    def test_remember_review_report_appends_history_and_session_event(self) -> None:
        """remember_review_report 追加 assistant 历史并写入会话事件。"""

        from omnicrawl.agent import LocalToolAgent

        agent = object.__new__(LocalToolAgent)
        agent._history = []
        events = []
        agent._append_session_event = lambda event_type, payload: events.append(
            (event_type, dict(payload))
        )

        agent.remember_review_report("报告内容")

        self.assertEqual(len(agent._history), 1)
        self.assertEqual(agent._history[0]["role"], "assistant")
        self.assertIn("[评审报告]", agent._history[0]["content"])
        self.assertIn("报告内容", agent._history[0]["content"])
        self.assertEqual(events, [("assistant_message", {"content": agent._history[0]["content"]})])

    def test_remember_review_report_skips_empty(self) -> None:
        from omnicrawl.agent import LocalToolAgent

        agent = object.__new__(LocalToolAgent)
        agent._history = []
        agent._append_session_event = lambda *_args: self.fail("空报告不应写会话事件")

        agent.remember_review_report("   ")
        self.assertEqual(agent._history, [])


class ReviewConversationStreamTests(unittest.TestCase):
    """run_subagent_task 的对话流开关与实时事件转发。"""

    def _fake_agent(self, coordinator_run=None):
        from omnicrawl.agent import LocalToolAgent

        agent = object.__new__(LocalToolAgent)

        class _Registry:
            def get(self, name):
                return type("D", (), {"name": name})() if name == "review" else None

        class _Coordinator:
            registry = _Registry()

            def run(self, arguments, **kwargs):
                self.last_kwargs = kwargs
                if coordinator_run is not None:
                    return coordinator_run(arguments, **kwargs)
                return type(
                    "R",
                    (),
                    {
                        "ok": True,
                        "output": '{"status": "completed", "results": '
                        '[{"status": "completed", "summary": "{}", "error": null}]}',
                    },
                )()

        agent._subagent_coordinator = _Coordinator()
        agent._subagent_event_callback = None
        return agent

    def test_run_requests_keep_full_text_and_prefers_full_text_over_summary(self) -> None:
        """#15：run_subagent_task 请求 keep_full_text，且优先返回未截断全文。"""

        import json

        observed = {}

        def capturing_run(_arguments, **_kwargs):
            observed["keep_full_text"] = _kwargs.get("keep_full_text")
            payload = {
                "status": "completed",
                "results": [
                    {
                        "status": "completed",
                        "summary": "截断摘要",
                        "full_text": '{"findings": [], "overall_correctness": '
                        '"patch is correct"}',
                        "error": None,
                    }
                ],
            }
            return type(
                "R",
                (),
                {"ok": True, "output": json.dumps(payload, ensure_ascii=False)},
            )()

        agent = self._fake_agent(coordinator_run=capturing_run)
        result = agent.run_subagent_task(
            agent_type="review",
            description="评审",
            prompt="p",
        )

        self.assertTrue(observed["keep_full_text"])
        self.assertIn("overall_correctness", result)
        self.assertNotIn("截断摘要", result)

    def test_stream_flag_and_callback_are_restored_after_run(self) -> None:
        observed = {}

        def capturing_run(_arguments, **_kwargs):
            observed["streaming"] = bool(
                getattr(agent, "_stream_subagent_conversation", False)
            )
            observed["callback"] = agent._subagent_event_callback is not None
            return type(
                "R",
                (),
                {
                    "ok": True,
                    "output": '{"status": "completed", "results": '
                    '[{"status": "completed", "summary": "{}", "error": null}]}',
                },
            )()

        agent = self._fake_agent(coordinator_run=capturing_run)
        agent.run_subagent_task(
            agent_type="review",
            description="评审",
            prompt="p",
            on_subagent_event=lambda _name, _payload: None,
        )

        self.assertTrue(observed["streaming"])
        self.assertTrue(observed["callback"])
        self.assertIsNone(agent._subagent_event_callback)
        self.assertFalse(getattr(agent, "_stream_subagent_conversation", False))

    def test_stream_flag_restored_even_when_task_fails(self) -> None:
        from omnicrawl.agent import AgentError

        def failing_run(_arguments, **_kwargs):
            raise AgentError("boom")

        agent = self._fake_agent(coordinator_run=failing_run)
        with self.assertRaises(AgentError):
            agent.run_subagent_task(
                agent_type="review",
                description="评审",
                prompt="p",
                on_subagent_event=lambda _name, _payload: None,
            )

        self.assertIsNone(agent._subagent_event_callback)
        self.assertFalse(getattr(agent, "_stream_subagent_conversation", False))

    def test_failure_surfaces_per_task_error_with_code_and_diagnostic(self) -> None:
        """批失败时读取 per-task error，带 code 与 diagnostic category，而非通用回退。"""

        import json

        def failing_run(_arguments, **_kwargs):
            payload = {
                "status": "failed",
                "results": [
                    {
                        "status": "failed",
                        "error": {
                            "code": "SUBAGENT_MODEL_ERROR",
                            "message": "子任务模型请求失败。",
                            "diagnostic": {"category": "AUTHENTICATION_FAILED"},
                        },
                    }
                ],
            }
            return type(
                "R",
                (),
                {"ok": False, "output": json.dumps(payload, ensure_ascii=False)},
            )()

        from omnicrawl.agent import AgentError

        agent = self._fake_agent(coordinator_run=failing_run)
        with self.assertRaises(AgentError) as ctx:
            agent.run_subagent_task(
                agent_type="review",
                description="评审",
                prompt="p",
            )

        self.assertIn("子任务模型请求失败", str(ctx.exception))
        self.assertIn("SUBAGENT_MODEL_ERROR", str(ctx.exception))
        self.assertIn("AUTHENTICATION_FAILED", str(ctx.exception))
        self.assertNotIn("子任务执行失败。", str(ctx.exception))

    def test_failure_falls_back_to_generic_when_no_detail(self) -> None:
        """完全无错误信息时回退通用文案。"""

        import json

        def failing_run(_arguments, **_kwargs):
            payload = {"status": "failed", "results": [{"status": "failed"}]}
            return type(
                "R",
                (),
                {"ok": False, "output": json.dumps(payload, ensure_ascii=False)},
            )()

        from omnicrawl.agent import AgentError

        agent = self._fake_agent(coordinator_run=failing_run)
        with self.assertRaises(AgentError) as ctx:
            agent.run_subagent_task(
                agent_type="review",
                description="评审",
                prompt="p",
            )
        self.assertEqual(str(ctx.exception), "子任务执行失败。")

    def test_live_event_emitter_forwards_without_session_persistence(self) -> None:
        agent = self._fake_agent()
        received = []
        agent._subagent_event_callback = lambda name, payload: received.append(
            (name, dict(payload))
        )
        session_events_before = getattr(agent, "_session_events", None)

        agent._emit_subagent_live_event(
            "subagent.tool.started",
            {"tool": "git", "arguments": {"action": "diff"}},
        )

        self.assertEqual(received, [("subagent.tool.started", {"tool": "git", "arguments": {"action": "diff"}})])
        self.assertIs(getattr(agent, "_session_events", None), session_events_before)
        agent._subagent_event_callback = None
        agent._emit_subagent_live_event("subagent.tool.started", {"tool": "git"})
        self.assertEqual(len(received), 1)


if __name__ == "__main__":
    unittest.main()
