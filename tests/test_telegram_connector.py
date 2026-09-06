"""omnicrawl.connectors.telegram 回归测试。

不依赖网络与真实 Agent：mock requests.post（Telegram API）与 Agent 实例，
验证命令分发、任务执行/取消、单活动限制、工具确认与安全白名单。
"""

from __future__ import annotations

import os
import tempfile
import threading
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

import requests

from omnicrawl.connectors.telegram import (
    CLOSE_TASK_JOIN_TIMEOUT,
    TelegramAgentBot,
    load_telegram_config,
)





def _resp(payload, status=200):
    response = mock.Mock()
    response.status_code = status
    response.json.return_value = payload
    return response


def _fake_api(sent: list, file_path: str = "documents/test_1.docx"):
    """返回一个接受 getUpdates/sendMessage/editMessageText/getFile 的 API 替身。"""

    def fake_post(url, params=None, data=None, timeout=None):
        if url.endswith("/getUpdates"):
            return _resp({"ok": True, "result": []})
        if url.endswith("/sendMessage"):
            sent.append({
                "kind": "send",
                "chat_id": data["chat_id"],
                "text": data["text"],
            })
            return _resp({"ok": True, "result": {"message_id": len(sent)}})
        if url.endswith("/editMessageText"):
            sent.append({
                "kind": "edit",
                "chat_id": data["chat_id"],
                "message_id": data["message_id"],
                "text": data["text"],
            })
            return _resp({"ok": True, "result": {"message_id": data["message_id"]}})
        if url.endswith("/getFile"):
            return _resp({"ok": True, "result": {"file_path": file_path}})
        raise AssertionError(f"unexpected api call: {url}")

    return fake_post


def _fake_file_download(content: bytes = b"file-content-bytes"):
    """返回一个可注入的 requests.get 替身，模拟 Telegram CDN 文件下载。"""

    def fake_get(url, timeout=None):
        response = mock.Mock()
        response.content = content
        response.raise_for_status.return_value = None
        return response

    return fake_get


def requests_exc(message: str):
    """模拟 requests.get 抛网络异常。"""

    def fake_get(url, timeout=None):
        raise requests.exceptions.RequestException(message)

    return fake_get


class FakeAgent:
    """LocalToolAgent 的最小替身：记录调用并模拟流式输出。"""

    def __init__(
        self,
        task_delay: float = 0.2,
        chunk_delay: float = 0.0,
        workspace_root: str | None = None,
    ):
        self._session_id = "sess-123"
        self.workspace_root = workspace_root or "/fake/workspace"
        self.confirm_handler = None
        self.resets = 0
        self.runs: list[str] = []
        self.task_delay = task_delay
        self.chunk_delay = chunk_delay
        self.closed = False
        self.approval_mode = "manual"
        self.reasoning_effort = ""
        self.skill_manager = None
        self.sessions: list = []
        self.resumed_session: str | None = None
        self._temp_workspace = None

    @property
    def current_session_id(self) -> str:
        return self._session_id

    def set_confirm_handler(self, handler) -> None:
        self.confirm_handler = handler

    def close(self) -> None:
        self.closed = True

    def reset_conversation(self) -> None:
        self.resets += 1

    def set_approval_mode(self, mode) -> None:
        self.approval_mode = mode

    def set_reasoning_effort(self, effort) -> str:
        self.reasoning_effort = effort
        return effort

    def switch_workspace(self, new_path) -> str:
        self.workspace_root = str(new_path)
        return self.workspace_root

    def list_sessions(self, limit: int = 10) -> list:
        return self.sessions

    def resume_session(self, session_id: str):
        self.resumed_session = session_id
        return SimpleNamespace(
            session_id=session_id,
            title="测试会话",
            messages=[{"role": "user", "content": "x"}, {"role": "assistant", "content": "y"}],
        )

    def list_archived_sessions(self, limit: int = 10) -> list:
        return []

    def search_prompt_history(self, query: str = "", limit: int = 20) -> list:
        return []

    def list_subagent_tasks(self) -> list:
        return []

    def format_mcp_status(self) -> str:
        return "MCP 子系统：未启用"

    def format_plugins_status(self) -> str:
        return "插件子系统：未注入 PluginManager"

    def clean_memory(self) -> list:
        return []

    def run_stream(self, text, on_delta, **kwargs):
        self.runs.append(text)
        cancel_check = kwargs.get("cancel_check")
        reasoning_cb = kwargs.get("on_reasoning_delta")
        time.sleep(self.task_delay)
        if reasoning_cb is not None:
            reasoning_cb("分析中…")
            reasoning_cb("继续推理…")
        for chunk in ("第一步完成。", "最终结果：任务已执行。"):
            if cancel_check:
                cancel_check()
            on_delta(chunk)
            if self.chunk_delay:
                time.sleep(self.chunk_delay)
        return "模拟执行结果：" + text[:10]


def _update(chat_id: int, user_id: int, text: str) -> dict:
    return {
        "update_id": 1,
        "message": {
            "chat": {"id": chat_id, "type": "private"},
            "from": {"id": user_id},
            "text": text,
        },
    }


def _wait_task(bot: TelegramAgentBot, timeout: float = 8.0) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        with bot._lock:
            task = bot._active_task
        if task is None or task.thread is None or not task.thread.is_alive():
            return
        time.sleep(0.05)
    raise AssertionError("任务未在期限内结束")


def _wait_file_processed(
    bot: TelegramAgentBot,
    agent: FakeAgent,
    sent: list,
    timeout: float = 8.0,
) -> None:
    """等待文件下载线程完成：Agent 任务已启动，或出现下载/保存失败消息。

    文件处理已异步化，_handle_update 返回后下载线程仍在运行，
    必须等待文件落盘且任务启动（或失败回传）后再断言。
    """

    deadline = time.time() + timeout
    while time.time() < deadline:
        if agent.runs:
            return
        if any(
            "文件下载失败" in m["text"] or "文件保存失败" in m["text"]
            for m in sent
        ):
            return
        time.sleep(0.05)
    raise AssertionError("文件处理未在期限内完成")


class TelegramConnectorTests(unittest.TestCase):
    def test_unauthorized_user_is_ignored(self) -> None:
        sent: list = []
        with mock.patch("requests.post", side_effect=_fake_api(sent)):
            bot = TelegramAgentBot("test-token", [1001], agent=FakeAgent())
            bot._handle_update(_update(999, 2002, "任意消息"))
            self.assertEqual(sent, [])
            bot.close()

    def test_commands_start_status_session_reset(self) -> None:
        sent: list = []
        with mock.patch("requests.post", side_effect=_fake_api(sent)):
            bot = TelegramAgentBot("test-token", [1001], agent=FakeAgent())
            bot._handle_update(_update(1, 1001, "/start"))
            self.assertTrue(any("OmniCrawl 远程控制" in m["text"] for m in sent))
            bot._handle_update(_update(1, 1001, "/status"))
            status = [m["text"] for m in sent if "OmniCrawl 状态" in m["text"]]
            self.assertEqual(len(status), 1)
            self.assertIn("/fake/workspace", status[0])
            bot._handle_update(_update(1, 1001, "/session"))
            self.assertTrue(any("sess-123" in m["text"] for m in sent))
            bot._handle_update(_update(1, 1001, "/reset"))
            self.assertEqual(bot._agent.resets, 1)
            bot.close()

    def test_task_execution_and_single_active_limit(self) -> None:
        sent: list = []
        with mock.patch("requests.post", side_effect=_fake_api(sent)):
            bot = TelegramAgentBot("test-token", [1001], agent=FakeAgent(task_delay=0.5))
            bot._handle_update(_update(1, 1001, "帮我看看项目"))
            self.assertTrue(any("已收到任务" in m["text"] for m in sent))
            # 任务仍在执行时再次提交应被拒绝
            bot._handle_update(_update(1, 1001, "第二个任务"))
            self.assertTrue(any("已有任务正在执行" in m["text"] for m in sent))
            _wait_task(bot)
            self.assertTrue(any("模拟执行结果" in m["text"] for m in sent))
            self.assertEqual(bot._agent.runs, ["帮我看看项目"])
            self.assertIn("空闲", bot._status_text())
            bot.close()

    def test_cancel_interrupts_task(self) -> None:
        sent: list = []
        with mock.patch("requests.post", side_effect=_fake_api(sent)):
            bot = TelegramAgentBot("test-token", [1001], agent=FakeAgent(task_delay=5.0))
            agent = bot._agent

            def slow_run(text, on_delta, **kwargs):
                cancel_check = kwargs.get("cancel_check")
                for _ in range(500):
                    if cancel_check:
                        cancel_check()
                    time.sleep(0.02)
                return "done"

            agent.run_stream = slow_run
            bot._handle_update(_update(1, 1001, "长任务"))
            time.sleep(0.2)
            bot._handle_update(_update(1, 1001, "/cancel"))
            self.assertTrue(any("已请求取消" in m["text"] for m in sent))
            _wait_task(bot)
            self.assertTrue(any("任务已取消" in m["text"] for m in sent))
            bot.close()

    def test_confirm_approve_and_reject(self) -> None:
        sent: list = []
        with mock.patch("requests.post", side_effect=_fake_api(sent)):
            bot = TelegramAgentBot(
                "test-token",
                [1001],
                agent=FakeAgent(),
                confirm_timeout_seconds=2,
            )
            agent = bot._agent
            decision: dict = {}

            def run_approve(text, on_delta, **kwargs):
                decision["value"] = agent.confirm_handler("bash", {"command": "rm -rf /"})
                return "done"

            agent.run_stream = run_approve
            bot._handle_update(_update(1, 1001, "危险命令"))
            time.sleep(0.4)
            self.assertTrue(any("需要确认" in m["text"] for m in sent))
            bot._handle_update(_update(1, 1001, "/approve"))
            _wait_task(bot)
            self.assertIs(decision.get("value"), True)
            bot.close()

        sent = []
        with mock.patch("requests.post", side_effect=_fake_api(sent)):
            bot = TelegramAgentBot(
                "test-token",
                [1001],
                agent=FakeAgent(),
                confirm_timeout_seconds=2,
            )
            agent = bot._agent
            decision: dict = {}

            def run_reject(text, on_delta, **kwargs):
                decision["value"] = agent.confirm_handler("bash", {"command": "rm -rf /"})
                return "done"

            agent.run_stream = run_reject
            bot._handle_update(_update(1, 1001, "危险命令2"))
            time.sleep(0.4)
            bot._handle_update(_update(1, 1001, "/reject"))
            _wait_task(bot)
            self.assertIs(decision.get("value"), False)
            bot.close()

    def test_confirm_timeout_auto_rejects(self) -> None:
        sent: list = []
        with mock.patch("requests.post", side_effect=_fake_api(sent)):
            bot = TelegramAgentBot(
                "test-token",
                [1001],
                agent=FakeAgent(),
                confirm_timeout_seconds=0.5,
            )
            agent = bot._agent
            decision: dict = {}

            def run_timeout(text, on_delta, **kwargs):
                decision["value"] = agent.confirm_handler("bash", {"command": "echo hi"})
                return "done"

            agent.run_stream = run_timeout
            bot._handle_update(_update(1, 1001, "危险命令3"))
            _wait_task(bot, timeout=10)
            self.assertIs(decision.get("value"), False)
            self.assertTrue(any("确认超时" in m["text"] for m in sent))
            bot.close()

    def test_message_split_obeys_length_limit(self) -> None:
        parts = TelegramAgentBot._split_message("a" * 9000)
        self.assertLessEqual(len(parts), 3)
        self.assertTrue(all(len(part) <= 4000 for part in parts))

    def test_message_split_preserves_newlines_and_separators(self) -> None:
        """分段保留行间换行；超长单行硬切时插入分隔符（P3）。"""

        # 行边界切分：段尾保留换行，直接拼接即可还原原文。
        text = "\n".join(f"line{i}" for i in range(1000))
        parts = TelegramAgentBot._split_message(text, limit=50)
        self.assertGreater(len(parts), 1)
        self.assertTrue(all(len(p) <= 50 for p in parts))
        self.assertEqual("".join(parts), text)

        # 超长单行硬切：切点用换行分隔，且每段不超限。
        long_line = "x" * 9000
        parts = TelegramAgentBot._split_message(long_line, limit=4000)
        self.assertLessEqual(len(parts), 3)
        self.assertTrue(all(len(p) <= 4000 for p in parts))
        self.assertTrue(any(p.startswith("\n") for p in parts), "硬切段应有换行分隔符")

    def test_abort_stream_preserves_partial_output(self) -> None:
        """任务失败/取消时保留已流式输出的内容，错误单独发送（P3）。"""

        sent: list = []
        with mock.patch("requests.post", side_effect=_fake_api(sent)):
            bot = TelegramAgentBot("test-token", [1001], agent=FakeAgent())
            agent = bot._agent

            def failing_run(text, on_delta, **kwargs):
                on_delta("部分输出内容")
                raise RuntimeError("中途爆炸")

            agent.run_stream = failing_run
            bot._handle_update(_update(1, 1001, "触发失败任务"))
            _wait_task(bot)
            # 部分输出仍在流式消息中（未被错误覆盖），且错误单独一条。
            texts = [m["text"] for m in sent if m.get("kind") == "send"]
            self.assertTrue(any("任务执行失败" in t for t in texts))
            self.assertTrue(any("部分输出内容" in t for t in texts))
            # 流式编辑消息未被覆盖成纯错误。
            edits = [m["text"] for m in sent if m.get("kind") == "edit"]
            self.assertTrue(any("部分输出内容" in t for t in edits))
            bot.close()

    def test_close_releases_pending_and_joins_task(self) -> None:
        """close() 释放挂起确认并等待任务线程退出后再关 Agent（P3）。"""

        sent: list = []
        closed = threading.Event()
        with mock.patch("requests.post", side_effect=_fake_api(sent)):
            bot = TelegramAgentBot(
                "test-token", [1001], agent=FakeAgent(task_delay=5.0), confirm_timeout_seconds=300
            )
            agent = bot._agent
            original_close = agent.close

            def tracking_close():
                closed.set()
                original_close()

            agent.close = tracking_close
            confirm_returned = threading.Event()

            def run_blocked(text, on_delta, **kwargs):
                # 任务线程阻塞在确认回调上；close 应释放它。
                agent.confirm_handler("bash", {"command": "sleep 999"})
                confirm_returned.set()
                return "done"

            agent.run_stream = run_blocked
            bot._handle_update(_update(1, 1001, "阻塞任务"))
            deadline = time.time() + 5
            while time.time() < deadline and not any("需要确认" in m["text"] for m in sent):
                time.sleep(0.05)
            self.assertTrue(any("需要确认" in m["text"] for m in sent))
            # close：确认立即释放，任务线程退出后才关闭 Agent。
            start = time.time()
            bot.close()
            elapsed = time.time() - start
            self.assertTrue(confirm_returned.is_set(), "close 应释放挂起的确认")
            self.assertLess(elapsed, CLOSE_TASK_JOIN_TIMEOUT)
            self.assertTrue(closed.is_set(), "Agent 应已在任务线程退出后关闭")

    def test_help_includes_start_command(self) -> None:
        """帮助文本包含 /start 自身与全部基础命令（P3 防漂移）。"""

        help_text = TelegramAgentBot("test-token", [1001], agent=FakeAgent())._help_text()
        for cmd in (
            "/start",
            "/cancel",
            "/approve",
            "/reject",
            "/thinking",
            "/status",
            "/session",
            "/reset",
            "/workspace",
            "/resume",
            "/memory:clean",
            "/reasoning",
            "/approval",
        ):
            self.assertIn(cmd, help_text)

    def test_harness_slash_commands(self) -> None:
        """harness 管理命令复用 slash.py，返回格式化结果。"""

        sent: list = []
        with mock.patch("requests.post", side_effect=_fake_api(sent)):
            bot = TelegramAgentBot("test-token", [1001], agent=FakeAgent())
            # 会话列表
            bot._handle_update(_update(1, 1001, "/sessions"))
            self.assertTrue(any("可恢复会话" in m["text"] for m in sent))
            # 子系统状态
            bot._handle_update(_update(1, 1001, "/mcp"))
            self.assertTrue(any("MCP" in m["text"] for m in sent))
            bot._handle_update(_update(1, 1001, "/plugins"))
            self.assertTrue(any("插件" in m["text"] for m in sent))
            bot._handle_update(_update(1, 1001, "/skills"))
            self.assertTrue(any("Skill" in m["text"] for m in sent))
            bot._handle_update(_update(1, 1001, "/memory:clean"))
            self.assertTrue(any("过期记忆" in m["text"] for m in sent))
            # 子任务与推理强度
            bot._handle_update(_update(1, 1001, "/tasks"))
            self.assertTrue(any("后台 SubAgent" in m["text"] for m in sent))
            bot._handle_update(_update(1, 1001, "/reasoning"))
            self.assertTrue(any("推理强度" in m["text"] for m in sent))
            bot.close()

    def test_approval_mode_security_boundary(self) -> None:
        """远程仅支持 manual/review，禁止 auto（安全边界），默认 review。"""

        sent: list = []
        with mock.patch("requests.post", side_effect=_fake_api(sent)), mock.patch(
            "omnicrawl.config.features.approval.save_approval_mode",
            return_value=Path("/fake/config.toml"),
        ):
            bot = TelegramAgentBot("test-token", [1001], agent=FakeAgent())
            bot._handle_update(_update(1, 1001, "/approval"))
            self.assertTrue(any("审批模式" in m["text"] for m in sent))
            # 远程拒绝完全自动
            bot._handle_update(_update(1, 1001, "/approval:auto"))
            self.assertTrue(any("不支持" in m["text"] or "仅限本地" in m["text"] for m in sent))
            self.assertNotEqual(bot._agent.approval_mode, "auto")
            bot._handle_update(_update(1, 1001, "/auto-approve:on"))
            self.assertTrue(any("不支持" in m["text"] or "仅限本地" in m["text"] for m in sent))
            self.assertNotEqual(bot._agent.approval_mode, "auto")
            # 允许的 两档
            bot._handle_update(_update(1, 1001, "/approval:review"))
            self.assertTrue(any("自动审查" in m["text"] for m in sent))
            self.assertEqual(bot._agent.approval_mode, "review")
            bot._handle_update(_update(1, 1001, "/approval:manual"))
            self.assertTrue(any("手动确认" in m["text"] for m in sent))
            self.assertEqual(bot._agent.approval_mode, "manual")
            # 兼容别名
            bot._handle_update(_update(1, 1001, "/auto-approve:off"))
            self.assertEqual(bot._agent.approval_mode, "manual")
            bot._handle_update(_update(1, 1001, "/auto-review:on"))
            self.assertEqual(bot._agent.approval_mode, "review")
            bot.close()

    def test_approval_sync_clamps_auto_to_review(self) -> None:
        """磁盘上为 auto（来自本地 TUI）时，Telegram 同步按 review 降级生效。"""

        agent = FakeAgent()
        agent.approval_mode = "manual"
        sent: list = []
        with mock.patch("requests.post", side_effect=_fake_api(sent)), mock.patch(
            "omnicrawl.config.features.approval.load_approval_mode", return_value="auto"
        ):
            bot = TelegramAgentBot("test-token", [1001], agent=agent)
            # 强制触发同步（清空 mtime 缓存）
            bot._config_mtime = 0
            bot._sync_runtime_config()
            self.assertEqual(agent.approval_mode, "review")
            bot.close()

    def test_unknown_slash_command_gets_hint(self) -> None:
        sent: list = []
        with mock.patch("requests.post", side_effect=_fake_api(sent)):
            bot = TelegramAgentBot("test-token", [1001], agent=FakeAgent())
            bot._handle_update(_update(1, 1001, "/foobar"))
            self.assertTrue(any("未知命令" in m["text"] for m in sent))
            # 未知命令不会启动 Agent 任务
            self.assertEqual(bot._agent.runs, [])
            bot.close()

    def test_sync_runtime_config_from_disk(self) -> None:
        """任务前同步：把 TUI 持久化的推理强度与工作区应用到 Agent。"""

        fake_llm_config = SimpleNamespace(reasoning_effort="high")
        with mock.patch(
            "omnicrawl.config.models.llm.load_llm_config", return_value=fake_llm_config
        ), mock.patch(
            "omnicrawl.config.core.workspace.load_workspace_root",
            return_value="/new/workspace",
        ):
            bot = TelegramAgentBot("test-token", [1001], agent=FakeAgent())
            bot._sync_runtime_config()
            self.assertEqual(bot._agent.reasoning_effort, "high")
            self.assertEqual(bot._agent.workspace_root, "/new/workspace")
            bot.close()

    def test_sync_runtime_config_mtime_shortcircuit(self) -> None:
        """config.toml 未变化时跳过重读（mtime 短路，P3 开销优化）。"""

        fake_llm_config = SimpleNamespace(reasoning_effort="medium")
        with tempfile.TemporaryDirectory() as td:
            cfg = Path(td) / "config.toml"
            cfg.write_text('[workspace]\nroot = "%s"\n' % td.replace(os.sep, "/"), encoding="utf-8")
            reads = {"count": 0}

            def counting_load():
                reads["count"] += 1
                return td.replace(os.sep, "/")

            with mock.patch(
                "omnicrawl.config.core.runtime.resolve_config_path", return_value=cfg
            ), mock.patch(
                "omnicrawl.config.models.llm.load_llm_config", return_value=fake_llm_config
            ), mock.patch(
                "omnicrawl.config.core.workspace.load_workspace_root", side_effect=counting_load
            ):
                bot = TelegramAgentBot("test-token", [1001], agent=FakeAgent())
                bot._sync_runtime_config()  # 首次：读取并应用
                self.assertEqual(reads["count"], 1)
                bot._sync_runtime_config()  # mtime 未变：应跳过重读
                bot._sync_runtime_config()
                self.assertEqual(reads["count"], 1, "mtime 未变时应跳过重读")
                bot.close()

    def test_workspace_command_view_switch_persist(self) -> None:
        """/workspace 查看当前、切换并持久化到 config.toml。"""

        sent: list = []
        with mock.patch("requests.post", side_effect=_fake_api(sent)), mock.patch(
            "omnicrawl.config.core.workspace.save_workspace_root",
            return_value="/cfg/config.toml",
        ) as save_mock:
            bot = TelegramAgentBot("test-token", [1001], agent=FakeAgent())
            # 查看当前工作区
            bot._handle_update(_update(1, 1001, "/workspace"))
            self.assertTrue(any("当前工作区" in m["text"] for m in sent))
            # 切换到新工作区并持久化
            bot._handle_update(_update(1, 1001, "/workspace D:/other/ws"))
            self.assertEqual(bot._agent.workspace_root, "D:/other/ws")
            save_mock.assert_called_once()
            self.assertTrue(any("已切换工作区" in m["text"] for m in sent))
            bot.close()

    def test_resume_latest_session(self) -> None:
        """/resume latest 恢复最近活动会话（一步接力，无需先查 ID）。"""

        sent: list = []
        agent = FakeAgent()
        agent.sessions = [SimpleNamespace(session_id="sess-recent", title="电脑上的会话")]
        with mock.patch("requests.post", side_effect=_fake_api(sent)):
            bot = TelegramAgentBot("test-token", [1001], agent=agent)
            bot._handle_update(_update(1, 1001, "/resume latest"))
            self.assertEqual(agent.resumed_session, "sess-recent")
            reply = sent[-1]["text"]
            self.assertIn("已恢复最近会话", reply)
            self.assertIn("电脑上的会话", reply)
            self.assertIn("2 条上下文消息", reply)
            bot.close()

    def test_resume_latest_no_sessions(self) -> None:
        """/resume latest 无会话时给出可操作提示，不报错。"""

        sent: list = []
        agent = FakeAgent()
        agent.sessions = []
        with mock.patch("requests.post", side_effect=_fake_api(sent)):
            bot = TelegramAgentBot("test-token", [1001], agent=agent)
            bot._handle_update(_update(1, 1001, "/resume latest"))
            self.assertIsNone(agent.resumed_session)
            self.assertTrue(any("还没有可恢复的会话" in m["text"] for m in sent))
            bot.close()

    def test_streaming_output_edits_same_message(self) -> None:
        """流式输出：最终回答打字机效果，编辑同一条消息而非全部塞进一条。"""

        sent: list = []
        with mock.patch("requests.post", side_effect=_fake_api(sent)):
            # chunk_delay 拉长输出间隔，越过 STREAM_EDIT_INTERVAL 触发多次编辑
            bot = TelegramAgentBot(
                "test-token",
                [1001],
                agent=FakeAgent(task_delay=0.1, chunk_delay=1.2),
            )
            bot._handle_update(_update(1, 1001, "流式任务"))
            _wait_task(bot, timeout=10)
            edits = [m for m in sent if m["kind"] == "edit"]
            sends = [m for m in sent if m["kind"] == "send"]
            # 回答流式消息：先 send 创建，再 edit 更新
            self.assertGreaterEqual(len(edits), 1)
            self.assertTrue(any("第一步完成" in m["text"] for m in sends))
            # 定型后的最终文本包含结果且不带截断标记
            final_edit = edits[-1]["text"]
            self.assertIn("模拟执行结果", final_edit)
            self.assertNotIn("…", final_edit)
            bot.close()

    def test_thinking_default_off_and_toggle(self) -> None:
        """思考内容默认不显示；/thinking on 后以独立消息流式显示。"""

        # 默认关闭：不产生 🧠 消息
        sent: list = []
        with mock.patch("requests.post", side_effect=_fake_api(sent)):
            bot = TelegramAgentBot("test-token", [1001], agent=FakeAgent(task_delay=0.1))
            bot._handle_update(_update(1, 1001, "普通任务"))
            _wait_task(bot)
            self.assertFalse(any("🧠" in m["text"] for m in sent))
            bot.close()

        # 开启后：产生独立的 🧠 思考消息
        sent = []
        with mock.patch("requests.post", side_effect=_fake_api(sent)):
            bot = TelegramAgentBot("test-token", [1001], agent=FakeAgent(task_delay=0.1))
            bot._handle_update(_update(1, 1001, "/thinking on"))
            self.assertTrue(any("已开启思考内容显示" in m["text"] for m in sent))
            bot._handle_update(_update(1, 1001, "带思考的任务"))
            _wait_task(bot)
            thinking = [m for m in sent if m["text"].startswith("🧠")]
            self.assertTrue(thinking)
            # 思考消息与回答消息相互独立（不是同一条）
            answer = [m for m in sent if "模拟执行结果" in m["text"]]
            self.assertTrue(answer)
            self.assertNotEqual(thinking[0]["text"], answer[-1]["text"])
            # 再次关闭
            bot._handle_update(_update(1, 1001, "/thinking off"))
            self.assertTrue(any("已关闭思考内容显示" in m["text"] for m in sent))
            bot.close()

    def test_file_document_download_and_classify(self) -> None:
        """文档文件：下载 → 存入 files/ → Agent 收到文件位置消息。"""

        sent: list = []
        with tempfile.TemporaryDirectory() as ws:
            agent = FakeAgent(workspace_root=ws)
            agent._temp_workspace = SimpleNamespace(
                root=str(Path(ws) / ".omnicrawl" / ".agent_tmp")
            )
            with mock.patch(
                "requests.post", side_effect=_fake_api(sent, file_path="documents/report.docx")
            ), mock.patch(
                "requests.get", side_effect=_fake_file_download(b"docx-content")
            ):
                bot = TelegramAgentBot("test-token", [1001], agent=agent)
                update = {
                    "update_id": 1,
                    "message": {
                        "chat": {"id": 1},
                        "from": {"id": 1001},
                        "document": {
                            "file_id": "FILE_DOC_ID",
                            "file_name": "报告.docx",
                        },
                    },
                }
                bot._handle_update(update)
                _wait_file_processed(bot, agent, sent)
                _wait_task(bot)
                # 文件落盘在 files/ 分类目录
                dest = Path(ws) / ".omnicrawl" / ".agent_tmp" / "files" / "报告.docx"
                self.assertTrue(dest.exists())
                self.assertEqual(dest.read_bytes(), b"docx-content")
                # 用户收到确认消息
                self.assertTrue(any("已收到文件" in m["text"] for m in sent))
                # Agent 收到的任务文本含文件位置
                self.assertEqual(agent.runs, ["已收到文件：位于 .omnicrawl/.agent_tmp/files/报告.docx"])
                bot.close()

    def test_file_photo_goes_to_images(self) -> None:
        """图片消息：存入 images/，无 caption 时任务文本只有位置。"""

        sent: list = []
        with tempfile.TemporaryDirectory() as ws:
            agent = FakeAgent(workspace_root=ws)
            agent._temp_workspace = SimpleNamespace(
                root=str(Path(ws) / ".omnicrawl" / ".agent_tmp")
            )
            with mock.patch(
                "requests.post", side_effect=_fake_api(sent, file_path="photos/photo_1.jpg")
            ), mock.patch(
                "requests.get", side_effect=_fake_file_download(b"jpg-bytes")
            ):
                bot = TelegramAgentBot("test-token", [1001], agent=agent)
                update = {
                    "update_id": 1,
                    "message": {
                        "chat": {"id": 1},
                        "from": {"id": 1001},
                        "photo": [
                            {"file_id": "SMALL", "file_size": 100},
                            {"file_id": "BIG", "file_size": 5000},
                        ],
                    },
                }
                bot._handle_update(update)
                _wait_file_processed(bot, agent, sent)
                _wait_task(bot)
                # 落在 images/，文件名按时间戳生成（.jpg 扩展名来自远程路径）
                images_dir = Path(ws) / ".omnicrawl" / ".agent_tmp" / "images"
                saved = list(images_dir.iterdir())
                self.assertEqual(len(saved), 1)
                self.assertTrue(saved[0].suffix.casefold() in {".jpg"})
                # Agent 收到位置消息
                self.assertTrue(agent.runs[0].startswith("已收到文件：位于 "))
                self.assertIn("images/", agent.runs[0])
                bot.close()

    def test_file_with_caption_appends_note(self) -> None:
        """文件带 caption：任务文本附加用户补充说明。"""

        sent: list = []
        with tempfile.TemporaryDirectory() as ws:
            agent = FakeAgent(workspace_root=ws)
            agent._temp_workspace = SimpleNamespace(
                root=str(Path(ws) / ".omnicrawl" / ".agent_tmp")
            )
            with mock.patch(
                "requests.post", side_effect=_fake_api(sent, file_path="documents/spec.pdf")
            ), mock.patch(
                "requests.get", side_effect=_fake_file_download(b"pdf-bytes")
            ):
                bot = TelegramAgentBot("test-token", [1001], agent=agent)
                update = {
                    "update_id": 1,
                    "message": {
                        "chat": {"id": 1},
                        "from": {"id": 1001},
                        "caption": "帮我提取里面的表格",
                        "document": {
                            "file_id": "FILE_PDF",
                            "file_name": "spec.pdf",
                        },
                    },
                }
                bot._handle_update(update)
                _wait_file_processed(bot, agent, sent)
                _wait_task(bot)
                self.assertTrue(
                    agent.runs[0].startswith("已收到文件：位于 ")
                    and "用户补充说明：帮我提取里面的表格" in agent.runs[0]
                )
                bot.close()

    def test_file_download_failure_reports_error(self) -> None:
        """下载失败：回传脱敏错误，不启动任务。"""

        sent: list = []
        with tempfile.TemporaryDirectory() as ws:
            agent = FakeAgent(workspace_root=ws)
            agent._temp_workspace = SimpleNamespace(
                root=str(Path(ws) / ".omnicrawl" / ".agent_tmp")
            )
            with mock.patch(
                "requests.post", side_effect=_fake_api(sent, file_path="documents/bad.pdf")
            ), mock.patch(
                "omnicrawl.connectors.telegram.requests.get",
                side_effect=requests_exc("download boom"),
            ):
                bot = TelegramAgentBot("test-token", [1001], agent=agent)
                update = {
                    "update_id": 1,
                    "message": {
                        "chat": {"id": 1},
                        "from": {"id": 1001},
                        "document": {"file_id": "FILE_BAD", "file_name": "bad.pdf"},
                    },
                }
                bot._handle_update(update)
                _wait_file_processed(bot, agent, sent)
                # 任务未启动
                self.assertEqual(agent.runs, [])
                self.assertTrue(any("文件下载失败" in m["text"] for m in sent))
                bot.close()

    def test_file_voice_goes_to_audio(self) -> None:
        """语音消息：无文件名，按远程路径扩展名归 audio/。"""

        sent: list = []
        with tempfile.TemporaryDirectory() as ws:
            agent = FakeAgent(workspace_root=ws)
            agent._temp_workspace = SimpleNamespace(
                root=str(Path(ws) / ".omnicrawl" / ".agent_tmp")
            )
            with mock.patch(
                "requests.post", side_effect=_fake_api(sent, file_path="voice/file_3.oga")
            ), mock.patch(
                "requests.get", side_effect=_fake_file_download(b"ogg-bytes")
            ):
                bot = TelegramAgentBot("test-token", [1001], agent=agent)
                update = {
                    "update_id": 1,
                    "message": {
                        "chat": {"id": 1},
                        "from": {"id": 1001},
                        "voice": {"file_id": "FILE_VOICE", "duration": 3},
                    },
                }
                bot._handle_update(update)
                _wait_file_processed(bot, agent, sent)
                _wait_task(bot)
                audio_dir = Path(ws) / ".omnicrawl" / ".agent_tmp" / "audio"
                saved = list(audio_dir.iterdir())
                self.assertEqual(len(saved), 1)
                self.assertEqual(saved[0].suffix.casefold(), ".oga")
                self.assertIn("audio/", agent.runs[0])
                bot.close()

    def test_file_audio_with_name_goes_to_audio(self) -> None:
        """音频文件：按 file_name 扩展名归 audio/。"""

        sent: list = []
        with tempfile.TemporaryDirectory() as ws:
            agent = FakeAgent(workspace_root=ws)
            agent._temp_workspace = SimpleNamespace(
                root=str(Path(ws) / ".omnicrawl" / ".agent_tmp")
            )
            with mock.patch(
                "requests.post", side_effect=_fake_api(sent, file_path="documents/music.mp3")
            ), mock.patch(
                "requests.get", side_effect=_fake_file_download(b"mp3-bytes")
            ):
                bot = TelegramAgentBot("test-token", [1001], agent=agent)
                update = {
                    "update_id": 1,
                    "message": {
                        "chat": {"id": 1},
                        "from": {"id": 1001},
                        "audio": {"file_id": "FILE_AUDIO", "file_name": "demo.mp3"},
                    },
                }
                bot._handle_update(update)
                _wait_file_processed(bot, agent, sent)
                _wait_task(bot)
                dest = Path(ws) / ".omnicrawl" / ".agent_tmp" / "audio" / "demo.mp3"
                self.assertTrue(dest.exists())
                self.assertEqual(agent.runs[0], "已收到文件：位于 .omnicrawl/.agent_tmp/audio/demo.mp3")
                bot.close()

    def test_file_video_goes_to_videos(self) -> None:
        """视频消息：归 videos/，Agent 收到位置消息。"""

        sent: list = []
        with tempfile.TemporaryDirectory() as ws:
            agent = FakeAgent(workspace_root=ws)
            agent._temp_workspace = SimpleNamespace(
                root=str(Path(ws) / ".omnicrawl" / ".agent_tmp")
            )
            with mock.patch(
                "requests.post", side_effect=_fake_api(sent, file_path="videos/video_1.mp4")
            ), mock.patch(
                "requests.get", side_effect=_fake_file_download(b"mp4-bytes")
            ):
                bot = TelegramAgentBot("test-token", [1001], agent=agent)
                update = {
                    "update_id": 1,
                    "message": {
                        "chat": {"id": 1},
                        "from": {"id": 1001},
                        "video": {"file_id": "FILE_VIDEO", "duration": 5},
                    },
                }
                bot._handle_update(update)
                _wait_file_processed(bot, agent, sent)
                _wait_task(bot)
                videos_dir = Path(ws) / ".omnicrawl" / ".agent_tmp" / "videos"
                saved = list(videos_dir.iterdir())
                self.assertEqual(len(saved), 1)
                self.assertEqual(saved[0].suffix.casefold(), ".mp4")
                self.assertIn("videos/", agent.runs[0])
                bot.close()

    def test_approval_requires_initiator(self) -> None:
        """群聊场景：只有发起任务的白名单用户能批准/拒绝确认。"""

        sent: list = []
        with mock.patch("requests.post", side_effect=_fake_api(sent)):
            bot = TelegramAgentBot("test-token", [1001, 2002], agent=FakeAgent(task_delay=0.4))
            agent = bot._agent
            # 1001 发起任务，确认回调触发后：2002 无权批准，1001 本人可批准
            def run_with_confirm(text, on_delta, **kwargs):
                time.sleep(0.1)
                assert agent.confirm_handler is not None
                confirmed = agent.confirm_handler("bash", {"command": "rm -rf /tmp/x"})
                return "已执行" if confirmed else "已拒绝"

            agent.run_stream = run_with_confirm
            bot._handle_update(_update(1, 1001, "执行敏感操作"))
            # 等待确认提示发出
            deadline = time.time() + 5
            while time.time() < deadline and not any("需要确认执行敏感操作" in m["text"] for m in sent):
                time.sleep(0.05)
            self.assertTrue(any("需要确认执行敏感操作" in m["text"] for m in sent))
            # 2002（同一群）尝试批准：确认应仍悬挂（未被释放）
            bot._handle_update(_update(1, 2002, "/approve"))
            with bot._lock:
                still_pending = bot._pending_confirm is not None
            self.assertTrue(still_pending, "非发起人的 /approve 不应释放确认")
            # 1001 本人批准：应生效并返回 True
            bot._handle_update(_update(1, 1001, "/approve"))
            _wait_task(bot)
            self.assertTrue(any("已执行" in m["text"] for m in sent))
            bot.close()

    def test_cancel_releases_pending_confirm(self) -> None:
        """取消任务时挂起的确认请求立即释放（按拒绝），不等超时。"""

        sent: list = []
        with mock.patch("requests.post", side_effect=_fake_api(sent)):
            bot = TelegramAgentBot(
                "test-token", [1001], agent=FakeAgent(task_delay=5.0), confirm_timeout_seconds=300
            )
            agent = bot._agent
            confirm_returned = threading.Event()

            def run_with_confirm(text, on_delta, **kwargs):
                time.sleep(0.1)
                agent.confirm_handler("bash", {"command": "sleep 999"})
                confirm_returned.set()
                return "done"

            agent.run_stream = run_with_confirm
            bot._handle_update(_update(1, 1001, "执行任务"))
            deadline = time.time() + 5
            while time.time() < deadline and not any("需要确认执行敏感操作" in m["text"] for m in sent):
                time.sleep(0.05)
            self.assertTrue(any("需要确认执行敏感操作" in m["text"] for m in sent))
            # 取消任务：确认应立即释放（< 300s 超时）
            bot._handle_update(_update(1, 1001, "/cancel"))
            self.assertTrue(confirm_returned.wait(3.0), "确认回调应在取消后及时返回")
            _wait_task(bot)
            self.assertTrue(any("任务已取消" in m["text"] for m in sent))
            bot.close()

    def test_file_download_does_not_block_polling(self) -> None:
        """文件下载在后台线程：下载期间轮询线程仍能响应 /cancel 等命令。"""

        sent: list = []
        with tempfile.TemporaryDirectory() as ws:
            agent = FakeAgent(workspace_root=ws)
            agent._temp_workspace = SimpleNamespace(
                root=str(Path(ws) / ".omnicrawl" / ".agent_tmp")
            )
            # 下载阻塞 3 秒（模拟慢 CDN）
            def slow_download(url, timeout=None):
                time.sleep(3.0)
                response = mock.Mock()
                response.content = b"slow-bytes"
                response.raise_for_status.return_value = None
                return response

            with mock.patch(
                "requests.post", side_effect=_fake_api(sent, file_path="documents/big.pdf")
            ), mock.patch(
                "omnicrawl.connectors.telegram.requests.get", side_effect=slow_download
            ):
                bot = TelegramAgentBot("test-token", [1001], agent=agent)
                update = {
                    "update_id": 1,
                    "message": {
                        "chat": {"id": 1},
                        "from": {"id": 1001},
                        "document": {"file_id": "FILE_BIG", "file_name": "big.pdf"},
                    },
                }
                start = time.time()
                bot._handle_update(update)
                # 下载期间 /status 应立即响应（不等待 3s 下载）
                bot._handle_update(_update(1, 1001, "/status"))
                status_elapsed = time.time() - start
                self.assertLess(status_elapsed, 1.0, "轮询线程不应被下载阻塞")
                self.assertTrue(any("OmniCrawl 状态" in m["text"] for m in sent))
                # 下载完成后任务正常启动
                _wait_file_processed(bot, agent, sent, timeout=8.0)
                _wait_task(bot)
                dest = Path(ws) / ".omnicrawl" / ".agent_tmp" / "files" / "big.pdf"
                self.assertTrue(dest.exists())
                bot.close()

    def test_non_text_no_file_silently_ignored(self) -> None:
        """既无文本也无文件的非文本消息静默忽略（保持原有行为）。"""

        sent: list = []
        with mock.patch("requests.post", side_effect=_fake_api(sent)):
            bot = TelegramAgentBot("test-token", [1001], agent=FakeAgent())
            update = {
                "update_id": 1,
                "message": {"chat": {"id": 1}, "from": {"id": 1001}, "location": {"lat": 1.0, "lng": 2.0}},
            }
            bot._handle_update(update)
            self.assertEqual(sent, [])
            self.assertEqual(bot._agent.runs, [])
            bot.close()


class TelegramConfigTests(unittest.TestCase):
    """load_telegram_config 的配置源优先级与格式兼容。"""

    def _write_config(self, body: str) -> str:
        """写临时 config.toml，返回文件路径（调用方负责清理）。"""

        fd, path = tempfile.mkstemp(suffix=".toml", text=True)
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            fh.write(body)
        return path

    def tearDown(self) -> None:
        for key in (
            "AI_CONFIG_FILE",
            "TELEGRAM_BOT_TOKEN",
            "TELEGRAM_ALLOWED_USER_IDS",
            "TELEGRAM_CONFIRM_TIMEOUT",
        ):
            os.environ.pop(key, None)

    def test_config_toml_section_read(self) -> None:
        """config.toml [telegram] 段的全部字段正确读取（含数组白名单）。"""

        path = self._write_config(
            "[telegram]\n"
            'bot_token = "abc123:TESTTOKEN"\n'
            "allowed_user_ids = [123456789, 987654321]\n"
            "confirmation_timeout_seconds = 600\n"
        )
        try:
            os.environ["AI_CONFIG_FILE"] = path
            config = load_telegram_config()
            self.assertEqual(config["bot_token"], "abc123:TESTTOKEN")
            self.assertEqual(config["allowed_user_ids"], [123456789, 987654321])
            self.assertEqual(config["confirm_timeout_seconds"], 600.0)
        finally:
            os.unlink(path)

    def test_env_vars_take_priority(self) -> None:
        """环境变量存在时覆盖 config.toml 段。"""

        path = self._write_config(
            "[telegram]\n"
            'bot_token = "cfg-token"\n'
            "allowed_user_ids = [1]\n"
            "confirmation_timeout_seconds = 600\n"
        )
        try:
            os.environ["AI_CONFIG_FILE"] = path
            os.environ["TELEGRAM_BOT_TOKEN"] = "env-token"
            os.environ["TELEGRAM_ALLOWED_USER_IDS"] = "10, 20, 30"
            os.environ["TELEGRAM_CONFIRM_TIMEOUT"] = "120"
            config = load_telegram_config()
            self.assertEqual(config["bot_token"], "env-token")
            self.assertEqual(config["allowed_user_ids"], [10, 20, 30])
            self.assertEqual(config["confirm_timeout_seconds"], 120.0)
        finally:
            os.unlink(path)

    def test_no_config_returns_empty(self) -> None:
        """无环境变量且无 [telegram] 段时返回空值，由 main() 提示。"""

        config = load_telegram_config()
        self.assertEqual(config["bot_token"], "")
        self.assertEqual(config["allowed_user_ids"], [])
        self.assertEqual(config["confirm_timeout_seconds"], 300.0)

    def test_invalid_user_id_raises(self) -> None:
        """白名单含非整数时抛 ValueError。"""

        path = self._write_config(
            "[telegram]\n"
            'bot_token = "t"\n'
            "allowed_user_ids = [\"not-a-number\"]\n"
        )
        try:
            os.environ["AI_CONFIG_FILE"] = path
            with self.assertRaises(ValueError):
                load_telegram_config()
        finally:
            os.unlink(path)


if __name__ == "__main__":
    unittest.main()
