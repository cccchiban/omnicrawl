#!/usr/bin/env python3
"""生成 features 配置（approval / tools / context_compaction / run_guard）对照数据集。

期望值来自 Python 真实现：审批模式归一化与读写、工具开关表与默认值、上下文压缩段校验、
运行护栏段校验与写回。

用法：``python rust/tools/gen_config_features_fixture.py``
输出：``rust/crates/omnicrawl-config/tests/fixtures/config_features_parity.json``
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
from pathlib import Path
from unittest import mock

import tomli_w

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-config/tests/fixtures/config_features_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.config.core import runtime as R  # noqa: E402
from omnicrawl.config.features import approval as A  # noqa: E402
from omnicrawl.config.features import context_compaction as C  # noqa: E402
from omnicrawl.config.features import run_guard as G  # noqa: E402
from omnicrawl.config.features import tools as T  # noqa: E402

for module in (R, A, C, G, T):
    if not Path(module.__file__).resolve().is_relative_to(ROOT):
        raise SystemExit("加载到的不是仓库源码")

MANAGED_HOME = "C:\\oc-features\\home"
MANAGED_DIR = MANAGED_HOME + "\\" + R.USER_CONFIG_DIRNAME


class Patched:
    def __init__(self, managed_dir: str = MANAGED_DIR) -> None:
        self._env = {"USERPROFILE": MANAGED_HOME, "HOME": MANAGED_HOME}
        self._patch = mock.patch.object(
            R, "user_config_dir", lambda *args, **kwargs: Path(managed_dir)
        )

    def __enter__(self):
        self._stack = mock.patch.dict(os.environ, self._env, clear=True)
        self._stack.start()
        self._patch.start()
        return self

    def __exit__(self, *exc_info):
        self._patch.stop()
        self._stack.stop()
        return False


def write_text(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="\n") as handle:
        handle.write(content)


def build_approval_normalize() -> list[dict[str, object]]:
    cases = []
    for value in [
        "manual", " MANUAL ", "ask", "confirm", "off", "auto", "auto_approve", "approve",
        "always", "review", "auto-review", "reviewed", "AUTO", "bogus", "",
    ]:
        try:
            record = {"value": value, "expected": A.normalize_approval_mode(value)}
        except R.RuntimeConfigError as exc:
            record = {"value": value, "error": str(exc)}
        cases.append(record)
    return cases


def build_approval_labels() -> list[dict[str, object]]:
    return [
        {"mode": mode, "expected": A.approval_mode_label(mode)}
        for mode in ("manual", "auto", "review", "bogus")
    ]


APPROVAL_LOAD_CASES = [
    ("缺段", {}),
    ("显式模式", {"approval": {"mode": "auto"}}),
    ("模式需归一化", {"approval": {"mode": " Auto-Review "}}),
    ("模式非法", {"approval": {"mode": "bogus"}}),
    ("模式非字符串", {"approval": {"mode": 5}}),
    ("旧开关 auto_review", {"approval": {"auto_review": True}}),
    ("旧开关 auto_approve", {"approval": {"auto_approve": True}}),
    ("旧开关都关", {"approval": {"auto_review": False, "auto_approve": False}}),
    ("段不是对象", {"approval": 5}),
    ("审查模型", {"approval": {"review_model": " gpt-5.2 "}}),
    ("审查模型为空", {"approval": {"review_model": ""}}),
    ("审查模型非字符串", {"approval": {"review_model": 5}}),
]


def build_approval_load() -> list[dict[str, object]]:
    cases = []
    with tempfile.TemporaryDirectory(prefix="oc-features-approval-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, config) in enumerate(APPROVAL_LOAD_CASES):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            write_text(user_dir / "config.toml", tomli_w.dumps(config))
            with Patched(str(user_dir)):
                record: dict[str, object] = {}
                try:
                    record["mode"] = A.load_approval_mode()
                except R.RuntimeConfigError as exc:
                    record["mode_error"] = str(exc)
                try:
                    record["review_model"] = A.load_approval_review_model()
                except R.RuntimeConfigError as exc:
                    record["review_model_error"] = str(exc)
            record.update({"name": name, "config": config})
            cases.append(record)
    return cases


def build_approval_save() -> list[dict[str, object]]:
    cases = []
    with tempfile.TemporaryDirectory(prefix="oc-features-approval-save-") as tmp:
        root = Path(tmp).resolve()
        for index, mode in enumerate(["auto", "review", "bogus"]):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            target = user_dir / "config.toml"
            write_text(target, tomli_w.dumps({"approval": {"review_model": "m"}, "ui": {"show_thinking": True}}))
            with Patched(str(user_dir)):
                try:
                    A.save_approval_mode(mode)
                    record: dict[str, object] = {"text": target.read_text(encoding="utf-8")}
                except R.RuntimeConfigError as exc:
                    record = {"error": str(exc)}
            record.update({"name": "写回 %s" % mode, "mode": mode})
            cases.append(record)
    return cases


def build_tools_validate() -> list[dict[str, object]]:
    cases = []
    for name in [
        "list", " powershell ", "memory_search", "project_memory_search",
        "user_memory_write", "session_memory_expand_related", "bogus", "project_bogus", "",
    ]:
        try:
            record = {"name": name, "expected": T.validate_tool_switch_name(name)}
        except R.RuntimeConfigError as exc:
            record = {"name": name, "error": str(exc)}
        cases.append(record)
    return cases


TOOLS_LOAD_CASES = [
    ("缺段", {}),
    ("全部覆盖", {"tools": {"bash": False, "powershell": True, "list": False}}),
    ("旧记忆开关名", {"tools": {"project_memory_search": False, "user_memory_write": False}}),
    ("非法值", {"tools": {"bash": "yes"}}),
    ("非法工具名", {"tools": {"bogus": True}}),
    ("段不是对象", {"tools": []}),
]


def build_tools_load() -> list[dict[str, object]]:
    cases = []
    with tempfile.TemporaryDirectory(prefix="oc-features-tools-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, config) in enumerate(TOOLS_LOAD_CASES):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            write_text(user_dir / "config.toml", tomli_w.dumps(config))
            with Patched(str(user_dir)):
                try:
                    switches = T.load_tool_switches()
                    record: dict[str, object] = {
                        "switches": switches,
                        "disabled": sorted(T.load_disabled_tools()),
                    }
                except R.RuntimeConfigError as exc:
                    record = {"error": str(exc)}
            record.update({"name": name, "config": config})
            cases.append(record)
    return cases


def build_tools_save() -> list[dict[str, object]]:
    cases = []
    plans = [
        ("单个开关", {"bash": False}),
        ("批量开关", {"powershell": True, "list": False, "memory_read": False}),
        ("旧名归一化", {"project_memory_search": False}),
        ("非法名不写盘", {"bash": False, "bogus": True}),
        # 值类型非法（Python 侧 isinstance 校验）在 Rust 侧由 `bool` 类型保证，不进对照。
    ]
    with tempfile.TemporaryDirectory(prefix="oc-features-tools-save-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, switches) in enumerate(plans):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            target = user_dir / "config.toml"
            write_text(target, tomli_w.dumps({"tools": {"list": True}}))
            with Patched(str(user_dir)):
                try:
                    T.save_tool_switches(switches)
                    record: dict[str, object] = {"text": target.read_text(encoding="utf-8")}
                except R.RuntimeConfigError as exc:
                    record = {
                        "error": str(exc),
                        "text_after": target.read_text(encoding="utf-8"),
                    }
            record.update({"name": name, "switches": switches})
            cases.append(record)
    return cases


CONTEXT_CASES = [
    ("缺段", {}),
    ("空段", {"context_compaction": {}}),
    ("合法全量", {
        "context_compaction": {
            "trigger_context_percent": 70,
            "trigger_context_tokens": 50000,
            "next_user_reserve_tokens": 2048,
            "emergency_context_ratio": 0.5,
            "summary_profile": "ch1/gpt-5.2",
            "reasoning_effort": "medium",
            "recent_turns": 4,
            "target_summary_tokens": 0,
            "preserve_exact_evidence": False,
            "archive_compacted_events": False,
            "auto_memory_recall": False,
            "allow_cross_provider": True,
            "failure_fallback": "deterministic",
        }
    }),
    ("百分比为零", {"context_compaction": {"trigger_context_percent": 0}}),
    ("百分比非整数", {"context_compaction": {"trigger_context_percent": "70"}}),
    ("窗口为零", {"context_compaction": {"trigger_context_tokens": 0}}),
    ("窗口为负", {"context_compaction": {"trigger_context_tokens": -5}}),
    ("预留为零", {"context_compaction": {"next_user_reserve_tokens": 0}}),
    ("近回合为零", {"context_compaction": {"recent_turns": 0}}),
    ("摘要预算为负", {"context_compaction": {"target_summary_tokens": -1}}),
    ("比值为零", {"context_compaction": {"emergency_context_ratio": 0}}),
    ("比值为一", {"context_compaction": {"emergency_context_ratio": 1.0}}),
    ("比值过大", {"context_compaction": {"emergency_context_ratio": 1.5}}),
    ("比值非数字", {"context_compaction": {"emergency_context_ratio": "0.5"}}),
    ("摘要模型非字符串", {"context_compaction": {"summary_profile": 5}}),
    ("推理强度为空", {"context_compaction": {"reasoning_effort": "   "}}),
    ("推理强度非字符串", {"context_compaction": {"reasoning_effort": 5}}),
    ("证据开关非布尔", {"context_compaction": {"preserve_exact_evidence": 1}}),
    ("归档开关非布尔", {"context_compaction": {"archive_compacted_events": "yes"}}),
    ("召回开关非布尔", {"context_compaction": {"auto_memory_recall": 0}}),
    ("跨供应商非布尔", {"context_compaction": {"allow_cross_provider": "y"}}),
    ("回退策略非法", {"context_compaction": {"failure_fallback": "model"}}),
    ("回退策略非字符串", {"context_compaction": {"failure_fallback": 5}}),
    ("未知字段", {"context_compaction": {"bogus": 1}}),
    ("未知字段与废字段", {"context_compaction": {"minimum_turns_between_model_compactions": 3, "zzz": 1}}),
    ("已废弃字段被忽略", {"context_compaction": {"minimum_turns_between_model_compactions": 3}}),
    ("段不是对象", {"context_compaction": 5}),
]


def build_context_compaction() -> list[dict[str, object]]:
    cases = []
    with tempfile.TemporaryDirectory(prefix="oc-features-compaction-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, config) in enumerate(CONTEXT_CASES):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            write_text(user_dir / "config.toml", tomli_w.dumps(config))
            with Patched(str(user_dir)):
                try:
                    value = C.load_context_compaction_config()
                    record: dict[str, object] = {
                        "expected": {key: getattr(value, key) for key in value.__dataclass_fields__}
                    }
                except R.RuntimeConfigError as exc:
                    record = {"error": str(exc)}
            record.update({"name": name, "config": config})
            cases.append(record)
    return cases


RUN_GUARD_CASES = [
    ("缺段", {}),
    ("空段", {"run_guard": {}}),
    ("合法全量", {
        "run_guard": {
            "enabled": False,
            "guard": {
                "enabled": True,
                "window_chars": 5000,
                "substr_len": 16,
                "repeat_ratio": 0.5,
                "check_every": 10,
                "max_blocks": 2000,
                "max_chars": 20000,
                "max_guard_retries": 3,
                "auto_retry_errors": ["SERVICE_UNAVAILABLE", " RATE_LIMIT ", "SERVICE_UNAVAILABLE"],
            },
            "continue": {"enabled": False, "max_auto_followups": 5},
        }
    }),
    ("总开关非布尔", {"run_guard": {"enabled": 1}}),
    ("guard 段不是对象", {"run_guard": {"guard": 5}}),
    ("continue 段不是对象", {"run_guard": {"continue": []}}),
    ("guard 窗口过小", {"run_guard": {"guard": {"window_chars": 10}}}),
    ("guard 窗口过大", {"run_guard": {"guard": {"window_chars": 2_000_000}}}),
    ("substr 越界", {"run_guard": {"guard": {"substr_len": 4}}}),
    ("比值越界", {"run_guard": {"guard": {"repeat_ratio": 1.5}}}),
    ("比值非数字", {"run_guard": {"guard": {"repeat_ratio": "0.5"}}}),
    ("check_every 越界", {"run_guard": {"guard": {"check_every": 0}}}),
    ("max_blocks 越界", {"run_guard": {"guard": {"max_blocks": 10}}}),
    ("max_chars 越界", {"run_guard": {"guard": {"max_chars": 100}}}),
    ("重试次数越界", {"run_guard": {"guard": {"max_guard_retries": 11}}}),
    ("错误码非数组", {"run_guard": {"guard": {"auto_retry_errors": "x"}}}),
    ("错误码含空项", {"run_guard": {"guard": {"auto_retry_errors": ["a", " "]}}}),
    ("错误码含非字符串", {"run_guard": {"guard": {"auto_retry_errors": [1]}}}),
    ("错误码过多", {"run_guard": {"guard": {"auto_retry_errors": ["c%d" % i for i in range(33)]}}}),
    ("续跑开关非布尔", {"run_guard": {"continue": {"enabled": "y"}}}),
    ("续跑次数越界", {"run_guard": {"continue": {"max_auto_followups": 0}}}),
    ("未知字段", {"run_guard": {"bogus": 1}}),
    ("guard 未知字段", {"run_guard": {"guard": {"bogus": 1}}}),
    ("continue 未知字段", {"run_guard": {"continue": {"bogus": 1}}}),
]


def build_run_guard() -> list[dict[str, object]]:
    cases = []
    with tempfile.TemporaryDirectory(prefix="oc-features-runguard-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, config) in enumerate(RUN_GUARD_CASES):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            write_text(user_dir / "config.toml", tomli_w.dumps(config))
            with Patched(str(user_dir)):
                try:
                    value = G.load_run_guard_config()
                    record: dict[str, object] = {
                        "expected": {
                            "enabled": value.enabled,
                            "guard": {
                                "enabled": value.guard.enabled,
                                "window_chars": value.guard.window_chars,
                                "substr_len": value.guard.substr_len,
                                "repeat_ratio": value.guard.repeat_ratio,
                                "check_every": value.guard.check_every,
                                "max_blocks": value.guard.max_blocks,
                                "max_chars": value.guard.max_chars,
                                "max_guard_retries": value.guard.max_guard_retries,
                                "auto_retry_errors": list(value.guard.auto_retry_errors),
                            },
                            "continue": {
                                "enabled": value.continuation.enabled,
                                "max_auto_followups": value.continuation.max_auto_followups,
                            },
                        }
                    }
                except R.RuntimeConfigError as exc:
                    record = {"error": str(exc)}
            record.update({"name": name, "config": config})
            cases.append(record)
    return cases


def build_run_guard_save() -> list[dict[str, object]]:
    with tempfile.TemporaryDirectory(prefix="oc-features-runguard-save-") as tmp:
        user_dir = Path(tmp).resolve() / R.USER_CONFIG_DIRNAME
        user_dir.mkdir(parents=True)
        target = user_dir / "config.toml"
        write_text(target, tomli_w.dumps({"ui": {"show_thinking": True}}))
        with Patched(str(user_dir)):
            G.save_run_guard_config(
                G.RunGuardConfig(
                    enabled=False,
                    guard=G.ReasoningGuardConfig(
                        window_chars=3000,
                        auto_retry_errors=("RATE_LIMIT",),
                    ),
                    continuation=G.ContinueConfig(max_auto_followups=7),
                )
            )
            text = target.read_text(encoding="utf-8")
    return [{"name": "写回完整段", "text": text}]


def main() -> None:
    fixture = {
        "approval_normalize": build_approval_normalize(),
        "approval_labels": build_approval_labels(),
        "approval_load": build_approval_load(),
        "approval_save": build_approval_save(),
        "tools_validate": build_tools_validate(),
        "tools_keys": list(T.TOOL_SWITCH_KEYS),
        "tools_defaults": T.TOOL_SWITCH_DEFAULTS,
        "tools_labels": T.TOOL_SWITCH_LABELS,
        "tools_load": build_tools_load(),
        "tools_save": build_tools_save(),
        "context_compaction": build_context_compaction(),
        "run_guard": build_run_guard(),
        "run_guard_save": build_run_guard_save(),
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    with FIXTURE_PATH.open("w", encoding="utf-8", newline="\n") as handle:
        json.dump(fixture, handle, ensure_ascii=False, indent=1)
        handle.write("\n")
    print("wrote %s: %s" % (FIXTURE_PATH, {key: len(value) for key, value in fixture.items()}))


if __name__ == "__main__":
    main()
