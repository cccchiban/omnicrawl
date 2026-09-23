#!/usr/bin/env python3
"""生成连接器自动启动监督器的对照数据集，供 `omnicrawl-connectors` 的 parity 测试使用。

单一真相是 `omnicrawl/connectors/autostart.py` 的真实现：本脚本把同一批「配置文件 + 环境
变量 + 采集日志开关」喂给真监督器（`Popen` 换成记录用的假实现），把「哪些平台被拉起」、
告警文案、以及传给子进程的实参形状（stdin/stdout/stderr、独立进程组、启动目录变量是否
被清掉）原样记下来。

三处刻意留白（`README.md` 有记录）：

- **子进程命令**：Python 固定 `[sys.executable, "-m", "omnicrawl.connectors.<平台>"]`；Rust
  侧内核没有 Python 模块进程，缺省是「当前可执行文件 + `connector <平台>`」，宿主可注入。
  数据集记录 Python 的命令用于对照，但 `started` / 实参形状才是断言目标。
- **PYTHONPATH**：Python 会把 `omnicrawl` 包的父目录塞进子进程环境；Rust 侧由宿主注入，
  缺省不动。因此这里只记「启动目录变量被清掉」，PYTHONPATH 的形状单独由 `child_env_cases`
  覆盖（那一节把 `os.environ` 换成受控字典，Python 与 Rust 都按同一批输入算一遍）。
- **close()**：假进程不是真进程，`close()` 里的进程树回收在两边都退化成「先请求终止、再
  等待」，只用于验证「关闭后 `started_connectors` 是否仍然报告本次会话拉起过的平台」。

假进程的 PID 取一个不可能存在的极大值，避免 Windows 上 `taskkill /PID` 打歪到真实进程。

用法：``python rust/tools/gen_connectors_autostart_fixture.py``
输出：``rust/crates/omnicrawl-connectors/tests/fixtures/connectors_autostart_parity.json``
"""

from __future__ import annotations

import importlib
import json
import logging
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import types
from pathlib import Path
from typing import Any
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
OUTPUT_PATH = (
    ROOT
    / "rust"
    / "crates"
    / "omnicrawl-connectors"
    / "tests"
    / "fixtures"
    / "connectors_autostart_parity.json"
)

sys.path.insert(0, str(ROOT))

# 不可能的 PID：`taskkill` / `killpg` 一定落空，不会波及真实进程。
FAKE_PID = 4_294_967_280

# 会被本脚本接管的连接器相关环境变量。
MANAGED_ENV = (
    "AI_CONFIG_FILE",
    "OMNICRAWL_AUTO_START_CONNECTORS",
    "TELEGRAM_BOT_TOKEN",
    "TELEGRAM_ALLOWED_USER_IDS",
    "TELEGRAM_CONFIRM_TIMEOUT",
    "FEISHU_APP_ID",
    "FEISHU_APP_SECRET",
    "FEISHU_ALLOWED_USER_IDS",
    "FEISHU_CONFIRM_TIMEOUT",
)


def import_autostart_module():
    """导入 `omnicrawl.connectors.autostart`（真实现）。

    仓库工作区里 `omnicrawl/llm/desensitization/engine.py` 带着未解决的合并冲突标记
    （`<<<<<<< ours`），语法就不合法；而 `omnicrawl/connectors/fsapp.py` 会经
    `omnicrawl.agent.toolkit.tools` 拉起 `agent` 的 `__init__` → `agent.controllers` →
    `llm.desensitization`，于是任何走这条链的导入都会连带失败（autostart 的两个目标模块与
    这条链无关）。这里把 `omnicrawl.agent` 换成一个「只有 `__path__` 的包壳」：它的
    `__init__` 不执行，子模块仍按真实路径加载；再把 `omnicrawl.commands.slash` 挡成空模块，
    断掉 `omnicrawl/__init__.py` 的同一条链。导入后断言模块文件确实位于仓库内，避免
    「挡住了冲突」变成换了真相源。
    """

    sys.modules.setdefault("omnicrawl.commands.slash", types.ModuleType("omnicrawl.commands.slash"))
    shell = types.ModuleType("omnicrawl.agent")
    shell.__path__ = [str(ROOT / "omnicrawl" / "agent")]
    sys.modules.setdefault("omnicrawl.agent", shell)

    module = importlib.import_module("omnicrawl.connectors.autostart")
    runtime = importlib.import_module("omnicrawl.config.core.runtime")
    for item in (module, runtime):
        if not Path(item.__file__).resolve().is_relative_to(ROOT):
            raise SystemExit(f"加载到的不是仓库源码：{item.__file__}")
    return module, runtime


A, RUNTIME = import_autostart_module()


class FakeProcess:
    """够用的假子进程：`wait` / `poll` / `terminate` / `kill` 与 `Popen` 同形。"""

    def __init__(self, pid: int = FAKE_PID) -> None:
        self.pid = pid
        self._stopped = threading.Event()

    def poll(self) -> int | None:
        return 0 if self._stopped.is_set() else None

    def wait(self, timeout: float | None = None) -> int | None:
        self._stopped.wait(timeout)
        return self.poll()

    def terminate(self) -> None:
        self._stopped.set()

    def kill(self) -> None:
        self._stopped.set()


class RecordingHandler(logging.Handler):
    """收集监督器自己的日志文案（Python 用 logging，Rust 侧对应 diagnostics）。"""

    def __init__(self) -> None:
        super().__init__(level=logging.INFO)
        self.messages: list[str] = []

    def emit(self, record: logging.LogRecord) -> None:
        self.messages.append(record.getMessage())


def toml_text(sections: dict[str, Any]) -> str:
    """把极简的「段 → 键值」结构写成 TOML（够这批用例用）。

    非表值是**根级**键，必须先写：TOML 里写在 `[段]` 之后的裸键属于该段，先写段会把
    `feishu = 5` 悄悄归到 `[telegram]` 下，用例就测不到「段不是对象」那条路径了。
    """

    lines: list[str] = []
    for name, body in sections.items():
        if not isinstance(body, dict):
            lines.append(f"{name} = {json.dumps(body) if isinstance(body, str) else body}\n")
    for name, body in sections.items():
        if not isinstance(body, dict):
            continue
        lines.append(f"[{name}]\n")
        for key, value in body.items():
            if isinstance(value, list):
                rendered = ", ".join(json.dumps(item) for item in value)
                lines.append(f"{key} = [{rendered}]\n")
            elif isinstance(value, str):
                lines.append(f'{key} = "{value}"\n')
            else:
                lines.append(f"{key} = {value}\n")
    return "".join(lines)


TELEGRAM_SECTION = {"telegram": {"bot_token": "123:abc", "allowed_user_ids": [1, 2]}}
FEISHU_SECTION = {"feishu": {"app_id": "cli_app", "app_secret": "secret"}}
BOTH_SECTIONS = {**TELEGRAM_SECTION, **FEISHU_SECTION}

# (用例名, 配置文件内容（None 表示指向一个不存在的路径）, 环境变量覆盖, 是否采集日志)
SCENARIOS: list[tuple[str, str | None, dict[str, str], bool]] = [
    ("none_configured", None, {}, False),
    ("empty_config", "", {}, False),
    ("telegram_only", toml_text(TELEGRAM_SECTION), {}, False),
    ("feishu_only", toml_text(FEISHU_SECTION), {}, False),
    ("both_configured", toml_text(BOTH_SECTIONS), {}, False),
    (
        "feishu_not_table",
        toml_text({**TELEGRAM_SECTION, "feishu": 5}),
        {},
        False,
    ),
    (
        "feishu_only_telegram_env",
        toml_text(FEISHU_SECTION),
        {"TELEGRAM_BOT_TOKEN": "9:zzz", "TELEGRAM_ALLOWED_USER_IDS": "7,8"},
        False,
    ),
    (
        "auto_start_disabled",
        toml_text(BOTH_SECTIONS),
        {"OMNICRAWL_AUTO_START_CONNECTORS": "0"},
        False,
    ),
    (
        "auto_start_unknown_value",
        toml_text(TELEGRAM_SECTION),
        {"OMNICRAWL_AUTO_START_CONNECTORS": "maybe"},
        False,
    ),
    ("log_capture", toml_text(BOTH_SECTIONS), {}, True),
]

# 子进程环境的受控输入：(基础环境, 说明)。`<ROOT>` 会被换成 Python 侧算出的包父目录。
CHILD_ENV_CASES: list[dict[str, Any]] = [
    {"name": "no_pythonpath", "base": {"PATH": "/usr/bin"}},
    {"name": "existing_pythonpath", "base": {"PATH": "/usr/bin", "PYTHONPATH": "  /existing  "}},
    {
        "name": "blank_pythonpath_and_launch_cwd",
        "base": {"PYTHONPATH": "   ", "AI_VOICE_CHAT_LAUNCH_CWD": "/somewhere"},
    },
    {
        "name": "pythonpath_list_and_launch_cwd",
        "base": {
            "PYTHONPATH": "/a:/b",
            "AI_VOICE_CHAT_LAUNCH_CWD": "/x",
            "KEEP": "1",
        },
    },
]


def normalize_spawn(record: dict[str, Any], scratch: Path) -> dict[str, Any]:
    """把一次 `Popen` 调用的实参压成与平台无关的形状。"""

    command = list(record["command"])
    kwargs = record["kwargs"]
    stdout = kwargs.get("stdout")
    stderr = kwargs.get("stderr")
    log_name = None
    if stdout is not None and hasattr(stdout, "name"):
        log_name = Path(stdout.name).name
    return {
        "module": command[2] if len(command) > 2 else command[-1],
        "cwd": normalize_path(kwargs.get("cwd"), scratch),
        "stdin_is_devnull": kwargs.get("stdin") is subprocess.DEVNULL,
        "stdout_is_devnull": stdout is subprocess.DEVNULL,
        "log_name": log_name,
        "stderr_merged_into_stdout": stderr is subprocess.STDOUT,
        "stderr_is_devnull": stderr is subprocess.DEVNULL,
        "new_process_group": bool(
            kwargs.get("creationflags", 0) if os.name == "nt" else kwargs.get("start_new_session")
        ),
        "launch_cwd_env_present": "AI_VOICE_CHAT_LAUNCH_CWD" in (kwargs.get("env") or {}),
        "pythonpath_present": "PYTHONPATH" in (kwargs.get("env") or {}),
    }


def normalize_path(value: Any, scratch: Path) -> str:
    if value is None:
        return ""
    path = Path(str(value)).resolve()
    try:
        relative = path.relative_to(scratch.resolve())
    except ValueError:
        return "<OUTSIDE>"
    return f"<WORKSPACE>/{relative.as_posix()}"


def run_scenario(
    name: str,
    config_text: str | None,
    env_overrides: dict[str, str],
    capture_logs: bool,
    scratch: Path,
) -> dict[str, Any]:
    """跑一次监督器：拉起 → 记录 → 关闭。"""

    workspace = scratch / "workspaces" / name
    workspace.mkdir(parents=True, exist_ok=True)
    config_path = scratch / "configs" / f"{name}.toml"
    config_path.parent.mkdir(parents=True, exist_ok=True)
    if config_text is None:
        config_path.unlink(missing_ok=True)
    else:
        config_path.write_bytes(config_text.encode("utf-8"))

    saved = {key: os.environ.get(key) for key in MANAGED_ENV}
    try:
        for key in MANAGED_ENV:
            os.environ.pop(key, None)
        os.environ["AI_CONFIG_FILE"] = str(config_path)
        os.environ.update(env_overrides)

        spawns: list[dict[str, Any]] = []

        def popen(command, **kwargs):  # noqa: ANN001 - 与 subprocess.Popen 同形
            spawns.append({"command": list(command), "kwargs": kwargs})
            return FakeProcess()

        handler = RecordingHandler()
        A.LOGGER.addHandler(handler)
        A.LOGGER.setLevel(logging.INFO)
        try:
            manager = A.ConnectorProcessManager(
                workspace,
                popen_factory=popen,
                capture_logs=capture_logs,
            )
            started = list(manager.start())
            diagnostics = list(handler.messages)
            manager.close()
            started_after_close = list(manager.started_connectors)
            diagnostics_after_close = list(handler.messages)
        finally:
            A.LOGGER.removeHandler(handler)

        return {
            "name": name,
            "config": config_text,
            "env_overrides": env_overrides,
            "capture_logs": capture_logs,
            "capture_logs_flag": manager.capture_logs,
            "started": started,
            "started_after_close": started_after_close,
            "diagnostics": diagnostics,
            "diagnostics_after_close": diagnostics_after_close,
            "spawns": [normalize_spawn(item, scratch) for item in spawns],
        }
    finally:
        for key, value in saved.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value


def build_child_env_cases() -> list[dict[str, Any]]:
    """`_child_environment` 的受控用例：把 `os.environ` 换成小字典后算一遍。"""

    cases: list[dict[str, Any]] = []
    for case in CHILD_ENV_CASES:
        base = case["base"]
        with mock.patch.object(os, "environ", dict(base)):
            environment = A._child_environment()
        package_parent = str(ROOT)
        pairs = [
            [key, value.replace(package_parent, "<ROOT>")] for key, value in environment.items()
        ]
        cases.append(
            {
                "name": case["name"],
                "base": base,
                "pairs": pairs,
                "launch_cwd_env_present": "AI_VOICE_CHAT_LAUNCH_CWD" in dict(pairs),
                "pythonpath": dict(pairs).get("PYTHONPATH", ""),
            }
        )
    return cases


def build_auto_start_decisions() -> list[dict[str, Any]]:
    """自动启动开关的判定表：未设置 / 关闭取值 / 开启取值 / 无效取值。"""

    cases: list[dict[str, Any]] = []
    for raw in (None, "0", "  OFF  ", "off", "disabled", "1", "TRUE", "on", "maybe", ""):
        if raw is None:
            os.environ.pop(A.AUTO_START_ENV, None)
        else:
            os.environ[A.AUTO_START_ENV] = raw
        cases.append(
            {
                "raw": raw,
                "enabled": A._auto_start_enabled(),
            }
        )
    os.environ.pop(A.AUTO_START_ENV, None)
    return cases


def main() -> None:
    scratch = Path(tempfile.mkdtemp(prefix="omnicrawl-autostart-fixture-"))
    try:
        # 锁文件与日志都落在临时目录里，别在真实用户配置目录留下痕迹。
        config_root = scratch / "user-config"
        config_root.mkdir(parents=True, exist_ok=True)
        A.user_config_dir = lambda: config_root  # type: ignore[assignment]
        singleton = importlib.import_module("omnicrawl.workspace.connector_singleton")
        singleton.user_config_dir = lambda: config_root  # type: ignore[assignment]

        scenarios = [
            run_scenario(name, config_text, env, capture, scratch)
            for name, config_text, env, capture in SCENARIOS
        ]
        child_env_cases = build_child_env_cases()
        log_dir = A._connector_log_path("飞书").parent

        data: dict[str, Any] = {
            "constants": {
                "auto_start_env": A.AUTO_START_ENV,
                "disabled_values": sorted(A._DISABLED_VALUES),
                "enabled_values": sorted(A._ENABLED_VALUES),
                "connector_log_dirname": A.CONNECTOR_LOG_DIRNAME,
                "platform_names": [spec.name for spec in A._CONNECTOR_SPECS],
                "platform_modules": [spec.module for spec in A._CONNECTOR_SPECS],
                "watch_poll_hint": "Rust 侧监督线程按 0.05s 轮询子进程状态（Python 直接用 wait()）。",
            },
            "auto_start_decisions": build_auto_start_decisions(),
            "log_paths": [
                {
                    "platform": spec.name,
                    "filename": A._connector_log_path(spec.name).name,
                }
                for spec in A._CONNECTOR_SPECS
            ],
            "log_dir_name": log_dir.name,
            "child_env_cases": child_env_cases,
            "scenarios": scenarios,
        }
        OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
        OUTPUT_PATH.write_bytes(
            (json.dumps(data, ensure_ascii=False, indent=2, sort_keys=False) + "\n").encode(
                "utf-8"
            )
        )
        print(f"已写出 {OUTPUT_PATH.relative_to(ROOT)}")
        print(
            f"用例数：场景 {len(scenarios)}、"
            f"子进程环境 {len(child_env_cases)}、"
            f"日志路径 {len(data['log_paths'])}"
        )
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


if __name__ == "__main__":
    main()
