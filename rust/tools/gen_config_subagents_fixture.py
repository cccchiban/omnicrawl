#!/usr/bin/env python3
"""生成 `omnicrawl/config/features/subagents.py` 的对照数据集。

期望值全部来自 Python 真实现：设置面板校验直接调用，读盘用例在临时目录里写
`subagents.toml` 后驱动真函数，环境变量按用例临时设置。

用法：``python rust/tools/gen_config_subagents_fixture.py``
输出：``rust/crates/omnicrawl-config/tests/fixtures/config_subagents_parity.json``
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = (
    ROOT / "rust/crates/omnicrawl-config/tests/fixtures/config_subagents_parity.json"
)

sys.path.insert(0, str(ROOT))

from omnicrawl.config.features import subagents as module  # noqa: E402

_source = Path(sys.modules[module.__name__].__file__).resolve()
if not _source.is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")

ENV_NAMES = [
    "OMNICRAWL_SUBAGENTS_ENABLED",
    "OMNICRAWL_SUBAGENT_VERIFY_AGENT_ENABLED",
    "OMNICRAWL_SUBAGENT_MAX_CONCURRENCY",
    "OMNICRAWL_SUBAGENT_TIMEOUT_SECONDS",
    "OMNICRAWL_SUBAGENT_VERIFY_TIMEOUT_SECONDS",
]


def outcome(callable_, *args, **kwargs):
    try:
        return {"ok": True, "value": callable_(*args, **kwargs), "error": None}
    except Exception as exc:  # noqa: BLE001 - 对照数据集要原样记录失败文案
        return {"ok": False, "value": None, "error": str(exc)}


ADVANCED_CASES = [
    ("并发下限内", "max_concurrency", 2),
    ("并发等于下限", "max_concurrency", 1),
    ("并发等于上限", "max_concurrency", 4),
    ("并发低于下限", "max_concurrency", 0),
    ("并发高于上限", "max_concurrency", 5),
    ("并发布尔值", "max_concurrency", True),
    ("并发浮点", "max_concurrency", 2.5),
    ("并发字符串", "max_concurrency", "2"),
    ("批次上限内", "max_tasks_per_batch", 3),
    ("超时整数写法", "default_timeout_seconds", 3600),
    ("超时浮点写法", "default_timeout_seconds", 12.5),
    ("超时等于下限", "default_timeout_seconds", 1),
    ("超时低于下限", "default_timeout_seconds", 0.5),
    ("超时高于上限", "default_timeout_seconds", 3601),
    ("超时布尔值", "default_timeout_seconds", False),
    ("模型并发内", "model_request_concurrency", 4),
    ("校验超时内", "verify_command_timeout_seconds", 360),
    ("保留分钟上限", "task_retention_minutes", 10080),
    ("保留分钟越界", "task_retention_minutes", 10081),
    ("未知配置项", "unknown_setting", 1),
]


LOAD_CASES = [
    ("空配置取默认值", "", {}),
    (
        "全字段生效",
        """
[subagents]
enabled = true
max_depth = 1
max_concurrency = 3
max_tasks_per_batch = 2
default_timeout_seconds = 120.5
model_request_concurrency = 4
allow_background = true
allow_fork = true
allow_shared_workspace_writes = true
allow_worktree = true
allow_standard_agent = true
enable_verify_agent = true
verify_command_timeout_seconds = 300
task_retention_minutes = 120
result_summary_chars = 1234
""",
        {},
    ),
    ("布尔类型错误", '[subagents]\nenabled = "yes"\n', {}),
    ("整数类型错误", '[subagents]\nmax_concurrency = "2"\n', {}),
    ("整数越界", "[subagents]\nmax_concurrency = 9\n", {}),
    ("数字类型错误", '[subagents]\ndefault_timeout_seconds = "600"\n', {}),
    ("数字越界", "[subagents]\ndefault_timeout_seconds = 0.5\n", {}),
    ("结果摘要越界", "[subagents]\nresult_summary_chars = 50\n", {}),
    ("max_depth 越界", "[subagents]\nmax_depth = 2\n", {}),
    (
        "模型覆盖解析",
        """
[subagents]
enabled = true

[subagents.models.review]
model = "deepseek-v4"

[subagents.models."  Explore  "]
model = "inherit"

[subagents.models.blank]
model = ""
""",
        {},
    ),
    ("模型段非对象", '[subagents]\nmodels = "x"\n', {}),
    ("模型条目非对象", "[subagents]\n[subagents.models.review]\nmodel = 5\n", {}),
    (
        "中文布尔取值",
        "[subagents]\nenabled = false\n",
        {},
    ),
]


def config_view(config) -> dict:
    return {
        "enabled": config.enabled,
        "max_depth": config.max_depth,
        "max_concurrency": config.max_concurrency,
        "max_tasks_per_batch": config.max_tasks_per_batch,
        "default_timeout_seconds": config.default_timeout_seconds,
        "model_request_concurrency": config.model_request_concurrency,
        "allow_background": config.allow_background,
        "allow_fork": config.allow_fork,
        "allow_shared_workspace_writes": config.allow_shared_workspace_writes,
        "allow_worktree": config.allow_worktree,
        "allow_standard_agent": config.allow_standard_agent,
        "enable_verify_agent": config.enable_verify_agent,
        "verify_command_timeout_seconds": config.verify_command_timeout_seconds,
        "task_retention_minutes": config.task_retention_minutes,
        "result_summary_chars": config.result_summary_chars,
        "model_overrides": dict(config.model_overrides),
    }


def advanced_cases() -> list[dict]:
    cases = []
    for label, name, value in ADVANCED_CASES:
        observed = outcome(module.validate_subagent_advanced_setting, name, value)
        entry = {"label": label, "name": name, "value": value, "ok": observed["ok"]}
        if observed["ok"]:
            result = observed["value"]
            entry["kind"] = "int" if isinstance(result, int) else "number"
            entry["result"] = result
            entry["error"] = None
        else:
            entry["kind"] = None
            entry["result"] = None
            entry["error"] = observed["error"]
        cases.append(entry)
    return cases


def load_cases(case_dir: Path) -> list[dict]:
    """环境变量通道已从 Rust 侧移除：期望值在**清空环境**的前提下录制。"""

    cases = []
    for index, (label, toml_text, env_values) in enumerate(LOAD_CASES):
        path = case_dir / f"case-{index}.toml"
        path.write_text(toml_text, encoding="utf-8")
        saved = {name: os.environ.pop(name, None) for name in ENV_NAMES}
        try:
            observed = outcome(module.load_subagent_config, path)
        finally:
            for name, value in saved.items():
                if value is not None:
                    os.environ[name] = value
        entry = {
            "label": label,
            "toml": toml_text,
            "env": env_values,
            "ok": observed["ok"],
        }
        if observed["ok"]:
            entry["config"] = config_view(observed["value"])
            entry["error"] = None
        else:
            entry["config"] = None
            entry["error"] = observed["error"]
        cases.append(entry)
    return cases


def main() -> None:
    root = Path(tempfile.mkdtemp(prefix="omnicrawl-subagents-config-"))
    try:
        payload = {
            "advanced_keys": list(module.SUBAGENT_ADVANCED_SETTING_KEYS),
            "advanced": advanced_cases(),
            "load": load_cases(root),
        }
    finally:
        import shutil

        shutil.rmtree(root, ignore_errors=True)

    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(payload, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    print("已写入 %s" % FIXTURE_PATH)


if __name__ == "__main__":
    main()
