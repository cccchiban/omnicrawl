from __future__ import annotations

import os
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import patch

from omnicrawl.workspace import connector_singleton
from omnicrawl.workspace.connector_singleton import ConnectorInstanceLock


class ConnectorSingletonTests(unittest.TestCase):
    def _lock_path(self) -> Path:
        # 测试用例之间隔离：使用独立目录，避免用户级真实锁文件被污染。
        base = Path(self._temp_dir.name)
        return base / "connector-test.lock"

    def setUp(self) -> None:
        import tempfile

        self._temp_dir = tempfile.TemporaryDirectory()
        self._lock_path_cache: Path | None = None
        # 避免测试写入真实用户配置目录。
        self._user_config_patch = patch.object(
            connector_singleton,
            "user_config_dir",
            return_value=Path(self._temp_dir.name),
        )
        self._user_config_patch.start()

    def tearDown(self) -> None:
        self._user_config_patch.stop()
        self._temp_dir.cleanup()

    def test_acquire_then_release_roundtrip(self) -> None:
        lock = ConnectorInstanceLock("飞书", lock_path=self._lock_path())
        self.assertTrue(lock.try_acquire())
        self.assertTrue(lock.lock_path.exists())
        # Windows 上 msvcrt 锁定期间文件不可读，只在 POSIX 校验内容。
        if os.name != "nt":
            self.assertTrue(
                lock.lock_path.read_text(encoding="utf-8").startswith("pid=")
            )
        lock.release()
        self.assertFalse(lock.lock_path.exists())

    def test_second_instance_sees_active_lock(self) -> None:
        first = ConnectorInstanceLock("飞书", lock_path=self._lock_path())
        second = ConnectorInstanceLock("飞书", lock_path=self._lock_path())
        self.assertTrue(first.try_acquire())
        # 同进程内竞争：Windows msvcrt 锁在同一进程内同样互斥（不因可重入
        # 而放行），因此 second 应被拒；POSIX flock 按文件描述符互斥，同一
        # 进程不同 fd 也会竞争失败。两者都符合预期。
        self.assertFalse(second.try_acquire())
        # 持有者释放后，第三方可以接管。
        first.release()
        self.assertTrue(second.try_acquire())
        second.release()

    def test_stale_lock_is_reclaimed(self) -> None:
        # 模拟崩溃残留：锁文件里有 pid，但进程已不存在。
        lock = ConnectorInstanceLock("飞书", lock_path=self._lock_path())
        self._lock_path().write_text(f"pid={999999999}\n", encoding="utf-8")
        self.assertTrue(lock.try_acquire())
        self.assertTrue(lock.lock_path.exists())
        lock.release()

    def test_live_pid_prevents_acquire(self) -> None:
        # 真实互斥场景：进程 A 持有文件锁（单例在跑），进程 B 尝试应被拒。
        # 注意：文件里写 pid 并不等于“有人持锁”——锁的互斥语义来自文件锁
        # 本身，而不是文件内容。因此用子进程 A 实际持有锁来模拟。
        import subprocess
        import sys as _sys
        import time as _time

        holder_code = (
            "import sys,time;"
            "from pathlib import Path;"
            "from omnicrawl.workspace.connector_singleton import ConnectorInstanceLock;"
            "lock = ConnectorInstanceLock('飞书', lock_path=Path(sys.argv[1]));"
            "print('ACQ:', lock.try_acquire(), flush=True);"
            "time.sleep(3)"
        )
        holder = subprocess.Popen(
            [_sys.executable, "-u", "-c", holder_code, str(self._lock_path())],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        try:
            # 等 A 拿到锁（跳过可能的 PID 写入警告行）
            import time as _time
            deadline = _time.time() + 5
            line = ""
            while _time.time() < deadline:
                if holder.stdout is None:
                    break
                line = holder.stdout.readline().strip()
                if line.startswith("ACQ:"):
                    break
                _time.sleep(0.05)
            else:
                holder.kill()
                holder.wait()
                self.fail(f"持锁子进程未在超时内获得单例锁，最后输出：{line}")
            self.assertTrue(line == "ACQ: True", f"子进程应拿到锁，实际：{line}")
            lock = ConnectorInstanceLock("飞书", lock_path=self._lock_path())
            self.assertFalse(lock.try_acquire())
        finally:
            holder.kill()
            holder.wait()

    def test_acquire_failure_does_not_leave_stale_file(self) -> None:
        first = ConnectorInstanceLock("飞书", lock_path=self._lock_path())
        second = ConnectorInstanceLock("飞书", lock_path=self._lock_path())
        first.try_acquire()
        self.assertFalse(second.try_acquire())
        # 锁文件仍属于 first（pid 存在），不能被 second 删除。
        self.assertTrue(first.lock_path.exists())
        first.release()
        self.assertFalse(first.lock_path.exists())

    def test_shared_name_uses_user_config_dir(self) -> None:
        path = connector_singleton.connector_lock_path("飞书")
        self.assertTrue(str(path).startswith(str(Path(self._temp_dir.name))))
        self.assertTrue(path.name.startswith("connector-"))
        self.assertTrue(path.name.endswith(".lock"))

    def test_pid_is_running(self) -> None:
        self.assertTrue(connector_singleton._pid_is_running(os.getpid()))
        self.assertFalse(connector_singleton._pid_is_running(999999999))
        self.assertFalse(connector_singleton._pid_is_running(0))
        self.assertFalse(connector_singleton._pid_is_running(-1))

    def test_release_is_idempotent(self) -> None:
        lock = ConnectorInstanceLock("飞书", lock_path=self._lock_path())
        self.assertTrue(lock.try_acquire())
        lock.release()
        lock.release()  # 不应抛异常

    def test_context_manager_releases_after_body(self) -> None:
        path = self._lock_path()
        with ConnectorInstanceLock("飞书", lock_path=path):
            self.assertTrue(path.exists())
        self.assertFalse(path.exists())

    def test_connector_lock_path_preserves_unicode_platform_names(self) -> None:
        # 飞书/Telegram 是中文名，锁文件名必须保留原字符，避免 sanitize
        # 把不同平台折叠成同一个锁文件（否则“飞书”和“Telegram”会互相
        # 误判为同一实例）。
        feishu = connector_singleton.connector_lock_path("飞书")
        telegram = connector_singleton.connector_lock_path("Telegram")
        self.assertNotEqual(feishu.name, telegram.name)
        self.assertIn("飞书", feishu.name)
        self.assertIn("Telegram", telegram.name)
        self.assertTrue(feishu.name.endswith(".lock"))
        self.assertTrue(feishu.name.startswith("connector-"))

    def test_real_subprocess_holds_lock_and_blocks_manager_start(self) -> None:
        """端到端：真实子进程持有飞书锁时，自动启动应跳过该平台。"""

        import subprocess
        import sys as _sys
        import time as _time

        from omnicrawl.connectors import autostart
        from omnicrawl.workspace import connector_singleton as cs

        lock_path = cs.connector_lock_path("飞书")
        holder_code = (
            "import sys,time;"
            "from pathlib import Path;"
            "from omnicrawl.workspace.connector_singleton import ConnectorInstanceLock;"
            "lock = ConnectorInstanceLock('飞书', lock_path=Path(sys.argv[1]));"
            "print('HOLDER_ACQUIRE:', lock.try_acquire(), flush=True);"
            "time.sleep(3)"
        )
        holder = subprocess.Popen(
            [_sys.executable, "-u", "-c", holder_code, str(lock_path)],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        try:
            deadline = _time.time() + 5
            acquired = False
            while _time.time() < deadline and holder.stdout is not None:
                line = holder.stdout.readline().strip()
                if "HOLDER_ACQUIRE: True" in line:
                    acquired = True
                    break
            self.assertTrue(acquired, "持锁子进程应成功获取单例锁")

            calls: list = []

            def fake_popen(cmd, **kwargs):
                calls.append(cmd[:])
                return subprocess.Popen(
                    [_sys.executable, "-c", "import time; time.sleep(0.5)"]
                )

            with patch.object(
                cs,
                "user_config_dir",
                return_value=lock_path.parent,
            ):
                with patch.object(
                    autostart,
                    "_configured_connectors",
                    return_value=(
                        (autostart._CONNECTOR_SPECS[0], False),
                        (autostart._CONNECTOR_SPECS[1], True),
                    ),
                ):
                    with patch.object(
                        autostart,
                        "assign_process_to_kill_on_close_job",
                        return_value=None,
                    ):
                        manager = autostart.ConnectorProcessManager(
                            Path.cwd(),
                            popen_factory=fake_popen,
                        )
                        started = manager.start()
                        manager.close()
            self.assertEqual(started, ())
            self.assertEqual(calls, [])
        finally:
            holder.kill()
            holder.wait()
            # 清理可能残留的锁文件
            if lock_path.exists():
                try:
                    lock_path.unlink()
                except OSError:
                    pass


if __name__ == "__main__":
    unittest.main()
