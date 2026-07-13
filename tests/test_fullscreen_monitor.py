"""全屏 Monitor 状态适配器的非 Textual 回归测试。"""

from __future__ import annotations

import unittest
from types import SimpleNamespace

from omnicrawl.agent import AgentError
from omnicrawl.ui.fullscreen.monitor import (
    MonitorDisplayBatch,
    MonitorStateAdapter,
    format_monitor_display_batch,
)


class MonitorStateAdapterTests(unittest.TestCase):
    """锁定全屏 UI 私有 cursor 与轮询暂停状态，不依赖 Textual 事件循环。"""

    @staticmethod
    def _task(monitor_id: str) -> SimpleNamespace:
        return SimpleNamespace(monitor_id=monitor_id)

    @staticmethod
    def _event(stream: str, text: str) -> SimpleNamespace:
        return SimpleNamespace(stream=stream, text=text)

    @staticmethod
    def _result(
        *,
        status: str = "running",
        events: tuple[SimpleNamespace, ...] = (),
        next_cursor: int = 0,
    ) -> SimpleNamespace:
        return SimpleNamespace(
            snapshot=SimpleNamespace(status=status),
            events=events,
            next_cursor=next_cursor,
        )

    def test_refresh_tracks_each_monitor_cursor_independently(self) -> None:
        """每个后台任务必须从自己的上次 cursor 继续增量读取。"""

        class FakeAgent:
            def __init__(self) -> None:
                self.calls: list[tuple[str, int, int]] = []

            def list_monitor_tasks(self) -> list[SimpleNamespace]:
                return [
                    MonitorStateAdapterTests._task("build"),
                    MonitorStateAdapterTests._task("server"),
                ]

            def poll_monitor_events(
                self,
                monitor_id: str,
                *,
                cursor: int,
                max_events: int,
            ) -> SimpleNamespace:
                self.calls.append((monitor_id, cursor, max_events))
                if monitor_id == "build":
                    return MonitorStateAdapterTests._result(
                        events=(
                            MonitorStateAdapterTests._event("stdout", "build complete"),
                        ),
                        next_cursor=4 if cursor == 0 else 5,
                    )
                return MonitorStateAdapterTests._result(
                    events=(
                        MonitorStateAdapterTests._event("stderr", "server warning"),
                    ),
                    next_cursor=8 if cursor == 0 else 9,
                )

        agent = FakeAgent()
        adapter = MonitorStateAdapter(agent)

        first_batches = adapter.refresh()
        second_batches = adapter.refresh()

        self.assertEqual(
            [(batch.monitor_id, batch.status) for batch in first_batches],
            [("build", "running"), ("server", "running")],
        )
        self.assertEqual(
            [batch.events[0].text for batch in second_batches],
            ["build complete", "server warning"],
        )
        self.assertEqual(
            agent.calls,
            [
                ("build", 0, 50),
                ("server", 0, 50),
                ("build", 4, 50),
                ("server", 8, 50),
            ],
        )
        self.assertEqual(dict(adapter.cursors), {"build": 5, "server": 9})

    def test_successful_empty_poll_still_advances_cursor(self) -> None:
        """无新展示事件时也必须记录服务端返回的下一个 cursor。"""

        observed: list[tuple[int, int]] = []

        class FakeAgent:
            def list_monitor_tasks(self) -> list[SimpleNamespace]:
                return [MonitorStateAdapterTests._task("idle-task")]

            def poll_monitor_events(
                self,
                _monitor_id: str,
                *,
                cursor: int,
                max_events: int,
            ) -> SimpleNamespace:
                observed.append((cursor, max_events))
                return MonitorStateAdapterTests._result(next_cursor=12)

        adapter = MonitorStateAdapter(FakeAgent())

        self.assertEqual(adapter.refresh(), ())
        self.assertEqual(observed, [(0, 50)])
        self.assertEqual(dict(adapter.cursors), {"idle-task": 12})

    def test_list_failure_leaves_cursors_and_display_batches_unchanged(self) -> None:
        """任务列表读取失败时，本轮应静默跳过且不能破坏已知消费位置。"""

        class FakeAgent:
            def __init__(self) -> None:
                self.list_should_fail = False
                self.poll_calls = 0

            def list_monitor_tasks(self) -> list[SimpleNamespace]:
                if self.list_should_fail:
                    raise AgentError("Monitor 列表暂不可用")
                return [MonitorStateAdapterTests._task("job")]

            def poll_monitor_events(self, *_args, **_kwargs) -> SimpleNamespace:
                self.poll_calls += 1
                return MonitorStateAdapterTests._result(next_cursor=7)

        agent = FakeAgent()
        adapter = MonitorStateAdapter(agent)
        adapter.refresh()
        agent.list_should_fail = True

        self.assertEqual(adapter.refresh(), ())
        self.assertEqual(dict(adapter.cursors), {"job": 7})
        self.assertEqual(agent.poll_calls, 1)

    def test_individual_poll_failure_retries_original_cursor_and_keeps_other_tasks_running(
        self,
    ) -> None:
        """单任务失败不得推进该任务 cursor，也不能阻断同轮其他任务。"""

        case = self

        class FakeAgent:
            def __init__(self) -> None:
                self.failing_attempts = 0
                self.calls: list[tuple[str, int]] = []

            def list_monitor_tasks(self) -> list[SimpleNamespace]:
                return [
                    MonitorStateAdapterTests._task("failing"),
                    MonitorStateAdapterTests._task("healthy"),
                ]

            def poll_monitor_events(
                self,
                monitor_id: str,
                *,
                cursor: int,
                max_events: int,
            ) -> SimpleNamespace:
                case.assertEqual(max_events, 50)
                self.calls.append((monitor_id, cursor))
                if monitor_id == "failing":
                    self.failing_attempts += 1
                    if self.failing_attempts == 1:
                        raise AgentError("任务暂不可读")
                    return MonitorStateAdapterTests._result(
                        events=(MonitorStateAdapterTests._event("stdout", "retry worked"),),
                        next_cursor=3,
                    )
                return MonitorStateAdapterTests._result(next_cursor=6)

        agent = FakeAgent()
        adapter = MonitorStateAdapter(agent)

        first_batches = adapter.refresh()
        second_batches = adapter.refresh()

        self.assertEqual(first_batches, ())
        self.assertEqual([batch.monitor_id for batch in second_batches], ["failing"])
        self.assertEqual(
            agent.calls,
            [
                ("failing", 0),
                ("healthy", 0),
                ("failing", 0),
                ("healthy", 6),
            ],
        )
        self.assertEqual(dict(adapter.cursors), {"healthy": 6, "failing": 3})

    def test_workspace_switch_suspends_refresh_clears_cursors_and_resumes_from_zero(
        self,
    ) -> None:
        """切换开始即丢弃旧工作区 cursor，恢复后新任务从零开始读取。"""

        case = self

        class FakeAgent:
            def __init__(self) -> None:
                self.calls: list[int] = []

            def list_monitor_tasks(self) -> list[SimpleNamespace]:
                return [MonitorStateAdapterTests._task("workspace-task")]

            def poll_monitor_events(
                self,
                _monitor_id: str,
                *,
                cursor: int,
                max_events: int,
            ) -> SimpleNamespace:
                case.assertEqual(max_events, 50)
                self.calls.append(cursor)
                return MonitorStateAdapterTests._result(next_cursor=9)

        agent = FakeAgent()
        adapter = MonitorStateAdapter(agent)
        adapter.refresh()

        adapter.suspend_for_workspace_switch()
        self.assertTrue(adapter.polling_suspended)
        self.assertEqual(dict(adapter.cursors), {})
        self.assertEqual(adapter.refresh(), ())
        self.assertEqual(agent.calls, [0])

        adapter.resume_polling()
        adapter.refresh()
        self.assertFalse(adapter.polling_suspended)
        self.assertEqual(agent.calls, [0, 0])

    def test_missing_poll_method_degrades_without_throwing(self) -> None:
        """旧 Agent 仅暴露列表能力时，定时回调仍应保持无副作用。"""

        class ListOnlyAgent:
            def list_monitor_tasks(self) -> list[SimpleNamespace]:
                return [MonitorStateAdapterTests._task("legacy-task")]

        adapter = MonitorStateAdapter(ListOnlyAgent())

        self.assertTrue(adapter.can_schedule_refresh)
        self.assertEqual(adapter.refresh(), ())
        self.assertEqual(dict(adapter.cursors), {})

    def test_format_monitor_display_batch_keeps_existing_tool_message_text(self) -> None:
        """抽离状态后，UI 仍应复用既有 Monitor 消息文案。"""

        batch = MonitorDisplayBatch(
            monitor_id="job-1",
            status="running",
            events=(
                self._event("stdout", "ready"),
                self._event("stderr", ""),
            ),
        )

        self.assertEqual(
            format_monitor_display_batch(batch),
            "Monitor · job-1 · running\n[stdout] ready\n[stderr] (空行)",
        )
