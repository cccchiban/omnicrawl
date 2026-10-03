#!/usr/bin/env python3
"""生成 core 配置（workspace / settings / bootstrap）的对照数据集。

期望值来自 Python 真实现：工作区根读写、设置面板各类写回、首次启动编排与诊断判定。

用法：``python rust/tools/gen_config_core_fixture.py``
输出：``rust/crates/omnicrawl-config/tests/fixtures/config_core_parity.json``
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
import types
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

import tomli_w

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-config/tests/fixtures/config_core_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.config.core import bootstrap as B  # noqa: E402
from omnicrawl.config.core import runtime as R  # noqa: E402
from omnicrawl.config.core import settings as S  # noqa: E402
from omnicrawl.config.core import workspace as W  # noqa: E402

for module in (R, S, W, B):
    if not Path(module.__file__).resolve().is_relative_to(ROOT):
        raise SystemExit("加载到的不是仓库源码")

MANAGED_HOME = "C:\\oc-core\\home"
MANAGED_DIR = MANAGED_HOME + "\\" + R.USER_CONFIG_DIRNAME

TEMPLATE_TEXT = {
    "config.example.toml": "# 运行配置模板\nllm = {}\n",
    "models.example.toml": "# 模型配置模板\nversion = 1\n",
    "subagents.example.toml": "# 子代理设置模板\n",
}


class Patched:
    def __init__(self, managed_dir: str = MANAGED_DIR, extra_env=None) -> None:
        self._env = {"USERPROFILE": MANAGED_HOME, "HOME": MANAGED_HOME}
        self._env.update(extra_env or {})
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


class _Resource:
    def __init__(self, text: str) -> None:
        self._text = text

    def read_text(self, encoding: str = "utf-8") -> str:
        return self._text


class _Resources:
    def joinpath(self, *parts: str) -> _Resource:
        return _Resource(TEMPLATE_TEXT[parts[-1]])


def write_text(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="\n") as handle:
        handle.write(content)


def read_text(path: Path) -> str:
    return path.read_text(encoding="utf-8")


# ── workspace ──────────────────────────────────────────────────────────────

WORKSPACE_LOAD_CASES = [
    ("缺段", {}),
    ("根目录", {"workspace": {"root": " D:\\proj "}}),
    ("根目录为空", {"workspace": {"root": ""}}),
    ("根目录非字符串", {"workspace": {"root": 5}}),
    ("段不是对象", {"workspace": 5}),
]


def build_workspace_load() -> list[dict[str, object]]:
    records = []
    with tempfile.TemporaryDirectory(prefix="oc-core-ws-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, config) in enumerate(WORKSPACE_LOAD_CASES):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            write_text(user_dir / "config.toml", tomli_w.dumps(config))
            record: dict[str, object] = {"name": name, "config": config}
            with Patched(str(user_dir)):
                try:
                    record["result"] = W.load_workspace_root()
                except Exception as exc:  # noqa: BLE001
                    record["error"] = str(exc)
            records.append(record)
    return records


def build_workspace_save() -> list[dict[str, object]]:
    records = []
    with tempfile.TemporaryDirectory(prefix="oc-core-ws-save-") as tmp:
        user_dir = Path(tmp).resolve() / R.USER_CONFIG_DIRNAME
        user_dir.mkdir(parents=True)
        target = user_dir / "config.toml"
        write_text(target, tomli_w.dumps({"llm": {"model": "keep"}, "workspace": {"other": 1}}))
        with Patched(str(user_dir)):
            W.save_workspace_root("D:\\proj\\sub\\..\\proj")
            records.append({"text": read_text(target)})
    return records


# ── settings ───────────────────────────────────────────────────────────────

SETTINGS_ENABLED_CASES = [
    ("缺段用默认真", {"section": "tts", "default": True, "config": {}}),
    ("缺段用默认假", {"section": "tts", "default": False, "config": {}}),
    ("显式开启", {"section": "tts", "default": False, "config": {"tts": {"enabled": True}}}),
    ("显式关闭", {"section": "advisor", "default": True, "config": {"advisor": {"enabled": False}}}),
    ("非布尔", {"section": "tts", "default": True, "config": {"tts": {"enabled": "yes"}}}),
    ("段不是对象", {"section": "tts", "default": True, "config": {"tts": 5}}),
]


def build_settings_enabled() -> list[dict[str, object]]:
    records = []
    with tempfile.TemporaryDirectory(prefix="oc-core-set-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, case) in enumerate(SETTINGS_ENABLED_CASES):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            write_text(user_dir / "config.toml", tomli_w.dumps(case["config"]))
            record: dict[str, object] = {
                "name": name,
                "section": case["section"],
                "default": case["default"],
                "config": case["config"],
            }
            with Patched(str(user_dir)):
                try:
                    record["result"] = S.load_feature_enabled(
                        case["section"], default=case["default"]
                    )
                except Exception as exc:  # noqa: BLE001
                    record["error"] = str(exc)
            records.append(record)
    return records


SETTINGS_WINDOW_CASES = [
    (
        "legacy 单模型段",
        {"llm": {"model": "gpt-5", "context_window_tokens": 1000}},
        "legacy",
        "",
    ),
    (
        "多模型段写 defaults",
        {"llm": {"active_model": {"source": "detected"}, "defaults": {"context_window_tokens": 1}}},
        "detected",
        "",
    ),
    (
        "多模型段缺 defaults",
        {"llm": {"active_model": {"source": "detected"}}},
        "custom",
        "gpt-5",
    ),
]


def build_settings_window_config() -> list[dict[str, object]]:
    records = []
    with tempfile.TemporaryDirectory(prefix="oc-core-win-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, config, source, key) in enumerate(SETTINGS_WINDOW_CASES):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            target = user_dir / "config.toml"
            write_text(target, tomli_w.dumps(config))
            record: dict[str, object] = {
                "name": name,
                "config": config,
                "model_source": source,
                "catalog_key": key,
            }
            with Patched(str(user_dir)):
                try:
                    S.save_context_window_tokens(200_000, model_source=source, catalog_key=key)
                    record["text"] = read_text(target)
                except Exception as exc:  # noqa: BLE001
                    record["error"] = str(exc)
            records.append(record)
    return records


MODELS_TOML = {
    "version": 1,
    "models": {
        "gpt-5": {
            "display_name": "GPT-5",
            "profile": "openai-main",
            "model_id": "gpt-5",
            "protocol": "openai_chat_completions",
            "context_window_tokens": 1000,
        },
        "claude": {
            "display_name": "Claude",
            "profile": "anthropic-main",
            "model_id": "claude-sonnet",
            "protocol": "anthropic_messages",
        },
    },
}


def build_settings_window_store() -> list[dict[str, object]]:
    records = []
    with tempfile.TemporaryDirectory(prefix="oc-core-win-store-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, key, tokens) in enumerate(
            [("命中条目", "gpt-5", 200_000), ("未命中条目", "missing", 200_000), ("非法 Token", "gpt-5", 0)]
        ):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            models_path = user_dir / "models.toml"
            write_text(models_path, tomli_w.dumps(MODELS_TOML))
            write_text(user_dir / "config.toml", tomli_w.dumps({}))
            record: dict[str, object] = {
                "name": name,
                "key": key,
                "tokens": tokens,
                "models": MODELS_TOML,
            }
            with Patched(str(user_dir)):
                try:
                    S.save_context_window_tokens(tokens, model_source="custom", catalog_key=key)
                    record["text"] = read_text(models_path)
                except Exception as exc:  # noqa: BLE001
                    record["error"] = str(exc)
            records.append(record)
    return records


COMPACTION_PERCENT_CASES = [
    ("整数换算", 80, 128_000),
    ("向下取整", 33, 1000),
    ("上限截断到 1", 1, 10),
    ("百分比为零", 0, 1000),
    ("窗口为空", 80, 0),
]


def build_settings_compaction_percent() -> list[dict[str, object]]:
    records = []
    with tempfile.TemporaryDirectory(prefix="oc-core-cmp-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, percent, window) in enumerate(COMPACTION_PERCENT_CASES):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            target = user_dir / "config.toml"
            write_text(target, tomli_w.dumps({"context_compaction": {"recent_turns": 6}}))
            record: dict[str, object] = {"name": name, "percent": percent, "window": window}
            with Patched(str(user_dir)):
                try:
                    S.save_context_compaction_trigger_percent(
                        percent, context_window_tokens=window
                    )
                    record["text"] = read_text(target)
                except Exception as exc:  # noqa: BLE001
                    record["error"] = str(exc)
            records.append(record)
    return records


SHOW_THINKING_CASES = [
    ("缺段", {}),
    ("显式关闭", {"ui": {"show_thinking": False}}),
    ("非布尔", {"ui": {"show_thinking": "no"}}),
]


def build_settings_show_thinking() -> list[dict[str, object]]:
    records = []
    with tempfile.TemporaryDirectory(prefix="oc-core-think-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, config) in enumerate(SHOW_THINKING_CASES):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            target = user_dir / "config.toml"
            write_text(target, tomli_w.dumps(config))
            record: dict[str, object] = {"name": name, "config": config}
            with Patched(str(user_dir)):
                try:
                    record["result"] = S.load_show_thinking()
                    S.save_show_thinking(False)
                    record["text"] = read_text(target)
                except Exception as exc:  # noqa: BLE001
                    record["error"] = str(exc)
            records.append(record)
    return records


def build_settings_subagent() -> list[dict[str, object]]:
    records = []
    with tempfile.TemporaryDirectory(prefix="oc-core-subagent-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, key, value) in enumerate(
            [
                ("整数项", "max_concurrency", 3),
                ("数字项", "default_timeout_seconds", 120.5),
                ("越界", "max_concurrency", 9),
                ("不支持项", "max_depth", 1),
            ]
        ):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            target = user_dir / "subagents.toml"
            write_text(target, tomli_w.dumps({"subagents": {"enabled": True, "max_depth": 1}}))
            record: dict[str, object] = {"name": name, "key": key, "value": value}
            with Patched(str(user_dir)):
                try:
                    S.save_subagent_setting(key, value)
                    record["text"] = read_text(target)
                except Exception as exc:  # noqa: BLE001
                    record["error"] = str(exc)
            records.append(record)
    return records


def mcp_config() -> SimpleNamespace:
    return SimpleNamespace(
        enabled=True,
        default_timeout_seconds=30,
        servers={
            "files": SimpleNamespace(
                enabled=True,
                transport="stdio",
                command="node",
                args=["server.js"],
                url="",
                env={"A": "1"},
                headers={"X": "y"},
                timeout_seconds=15,
                risk_level="read",
            )
        },
        policy=SimpleNamespace(
            require_confirmation_for_write=True,
            require_confirmation_for_command=False,
            allow_external_network_tools=False,
            audit_log_enabled=True,
        ),
    )


def build_settings_mcp() -> list[dict[str, object]]:
    records = []
    with tempfile.TemporaryDirectory(prefix="oc-core-mcp-") as tmp:
        user_dir = Path(tmp).resolve() / R.USER_CONFIG_DIRNAME
        user_dir.mkdir(parents=True)
        target = user_dir / "config.toml"
        write_text(target, tomli_w.dumps({"llm": {"model": "keep"}}))
        with Patched(str(user_dir)):
            S.save_mcp_config(mcp_config())
            records.append({"text": read_text(target)})
    return records


# ── bootstrap ──────────────────────────────────────────────────────────────

NODE_CASES = [
    ("未检测到 node", {"node": None, "npm": None}),
    ("版本过低", {"node": "C:\\node.exe", "npm": "C:\\npm.cmd", "output": "v18.20.4\n"}),
    ("版本可用", {"node": "C:\\node.exe", "npm": "C:\\npm.cmd", "output": "v20.11.0\n"}),
    ("缺少 npm", {"node": "C:\\node.exe", "npm": None, "output": "v20.11.0\n"}),
    ("版本输出异常", {"node": "C:\\node.exe", "npm": "C:\\npm.cmd", "output": "not-a-version\n"}),
    ("执行失败", {"node": "C:\\node.exe", "npm": "C:\\npm.cmd", "raise": "OSError: 拒绝访问"}),
]


def build_bootstrap_node() -> list[dict[str, object]]:
    records = []
    for name, case in NODE_CASES:
        record: dict[str, object] = {"name": name, "case": case}
        with mock.patch.object(B.shutil, "which", lambda command: case.get(command)):
            def fake_run(*args, **kwargs):
                if case.get("raise"):
                    raise OSError(case["raise"])
                return SimpleNamespace(stdout=case["output"], stderr="")

            with mock.patch.object(B.subprocess, "run", fake_run):
                check = B._check_node()
        record["check"] = [check.name, check.status, check.message]
        records.append(record)
    return records


PLUGIN_CASES = [
    ("插件禁用无注册项", {}, []),
    ("插件启用", {"plugins": {"enabled": True}}, [{"enabled": True}, {"enabled": False}, {"enabled": True}]),
    ("注册项报错", {}, [{"enabled": True, "error": "清单损坏"}, {"error": "缺少入口"}]),
    ("读取失败", {}, "raise"),
    ("启用开关非布尔", {"plugins": {"enabled": "yes"}}, []),
]


def build_bootstrap_plugin() -> list[dict[str, object]]:
    records = []
    for name, config_data, rows in PLUGIN_CASES:
        module = types.ModuleType("omnicrawl.extensions.plugin_install")
        if rows == "raise":
            def failing(**kwargs):
                raise RuntimeError("注册表不可读")

            module.list_plugins = failing
        else:
            module.list_plugins = lambda **kwargs: list(rows)
        record: dict[str, object] = {"name": name, "config": config_data}
        record["rows"] = [] if rows == "raise" else rows
        record["raise"] = rows == "raise"
        with mock.patch.dict(sys.modules, {"omnicrawl.extensions.plugin_install": module}):
            check = B._check_plugin_state(config_data)
        record["check"] = [check.name, check.status, check.message]
        records.append(record)
    return records


def startup_setup(**overrides) -> B.StartupSetup:
    base = dict(
        config_dir=Path("C:/cfg"),
        config_path=Path("C:/cfg/config.toml"),
        models_path=Path("C:/cfg/models.toml"),
        subagents_path=Path("C:/cfg/subagents.toml"),
        config_created=False,
        models_created=False,
        subagents_created=False,
        api_key_prompted=False,
        api_key_configured=True,
        checks=(),
        errors=(),
    )
    base.update(overrides)
    return B.StartupSetup(**base)


def build_bootstrap_report() -> list[dict[str, object]]:
    cases = [
        ("一切正常", startup_setup()),
        (
            "首次创建配置",
            startup_setup(config_created=True, models_created=True, subagents_created=True),
        ),
        ("未配置 Key", startup_setup(api_key_configured=False)),
        (
            "有错误",
            startup_setup(
                errors=("运行配置读取失败：坏文件",),
                checks=(B.StartupCheck("模型配置", "warning", "配置文件存在错误，请修复后重试。"),),
            ),
        ),
        (
            "检查项齐全",
            startup_setup(
                api_key_prompted=True,
                checks=(
                    B.StartupCheck("模型配置", "ok", "默认模型：GPT-5 (gpt-5)"),
                    B.StartupCheck("Node.js", "warning", "未检测到 Node.js；插件功能暂不可用。"),
                    B.StartupCheck("插件状态", "ok", "插件系统已禁用，已注册 0 个插件，当前启用 0 个。"),
                ),
            ),
        ),
    ]
    records = []
    for name, setup in cases:
        records.append({"name": name, "lines": list(B.format_startup_report(setup))})
    return records


INIT_CASES = [
    {
        "name": "空目录首次启动",
        "files": {},
        "api_key": None,
        "channel_setup": False,
        "prompt": None,
    },
    {
        "name": "已有 Profile 与直连 Key",
        "files": {
            "config.toml": tomli_w.dumps(
                {
                    "llm": {
                        "profiles": {
                            "openai-main": {"provider": "openai", "api_key": "sk-x"}
                        },
                        "active_model": {"profile": "openai-main"},
                    }
                }
            )
        },
        "api_key": None,
        "channel_setup": False,
        "prompt": None,
    },
    {
        "name": "已有可用模型",
        "files": {
            "config.toml": tomli_w.dumps(
                {
                    "llm": {
                        "profiles": {
                            "openai-main": {"provider": "openai", "api_key": "sk-x"}
                        },
                        "active_model": {"profile": "openai-main", "key": "gpt-5"},
                    }
                }
            ),
            "models.toml": tomli_w.dumps(MODELS_TOML),
        },
        "api_key": None,
        "channel_setup": False,
        "prompt": None,
    },
    {
        # 环境变量通道已移除：Profile 只写了 api_key_env、没有明文 api_key 时，
        # 判定为「未配置」并进入索取流程（期望值按 Rust 语义覆盖，见下方 override）。
        "name": "Key 只写了环境变量名",
        "files": {
            "config.toml": tomli_w.dumps(
                {
                    "llm": {
                        "profiles": {
                            "gem-main": {"provider": "gemini", "api_key_env": "GEMINI_API_KEY"}
                        },
                        "active_model": {"profile": "gem-main"},
                    }
                }
            )
        },
        "api_key": {"GEMINI_API_KEY": "env-key"},
        "channel_setup": False,
        "prompt": None,
        "summary_override": {"api_key_prompted": True, "api_key_configured": False},
    },
    {
        "name": "配置读取失败",
        "files": {"config.toml": "llm = { broken\n"},
        "api_key": None,
        "channel_setup": False,
        "prompt": None,
    },
    {
        "name": "渠道向导完成",
        "files": {},
        "api_key": None,
        "channel_setup": True,
        "prompt": None,
    },
    {
        "name": "交互输入 Key",
        "files": {
            "config.toml": tomli_w.dumps(
                {"llm": {"profiles": {"main": {"provider": "openai"}}, "active_model": {"profile": "main"}}}
            )
        },
        "api_key": None,
        "channel_setup": False,
        "prompt": "sk-from-prompt",
    },
]


def summarize(setup: B.StartupSetup, case_dir: Path) -> dict[str, object]:
    return {
        "config_created": setup.config_created,
        "models_created": setup.models_created,
        "subagents_created": setup.subagents_created,
        "api_key_prompted": setup.api_key_prompted,
        "api_key_configured": setup.api_key_configured,
        "first_run": setup.first_run,
        "checks": [[check.name, check.status, check.message] for check in setup.checks],
        "errors": [redact(error, case_dir) for error in setup.errors],
    }


def redact(message: object, case_dir: Path) -> str:
    """把用例目录从文案里抹掉：两侧的解析库错误尾巴本就不同。"""

    text = str(message)
    index = text.find(str(case_dir))
    if index >= 0:
        return text[:index] + "<case_dir>"
    return text


def build_bootstrap_initialize() -> list[dict[str, object]]:
    records = []
    with tempfile.TemporaryDirectory(prefix="oc-core-init-") as tmp:
        root = Path(tmp).resolve()
        for index, case in enumerate(INIT_CASES):
            config_dir = root / ("case-%d" % index)
            config_dir.mkdir(parents=True)
            for name, text in case["files"].items():
                write_text(config_dir / name, text)

            prompt = None
            if case["prompt"] is not None:
                prompt = lambda message, value=case["prompt"]: value

            channel_setup = None
            if case["channel_setup"]:
                def channel_setup(config_path, models_path):
                    write_text(
                        config_path,
                        tomli_w.dumps(
                            {
                                "llm": {
                                    "profiles": {
                                        "openai-main": {"provider": "openai", "api_key": "sk-wizard"}
                                    },
                                    "active_model": {"profile": "openai-main"},
                                }
                            }
                        ),
                    )
                    return True

            with Patched(str(config_dir), case["api_key"]):
                with mock.patch.object(B.getpass, "getpass", lambda *args, **kwargs: ""):
                    with mock.patch.object(B, "files", lambda package: _Resources()):
                        with mock.patch.object(
                            B,
                            "_check_node",
                            lambda: B.StartupCheck("Node.js", "ok", "Node.js v20.11.0 / npm 可用。"),
                        ):
                            with mock.patch.object(
                                B,
                                "_check_plugin_state",
                                lambda data: B.StartupCheck(
                                    "插件状态", "ok", "插件系统已禁用，已注册 0 个插件，当前启用 0 个。"
                                ),
                            ):
                                setup = B.initialize_user_configuration(
                                    config_dir, prompt=prompt, channel_setup=channel_setup
                                )

            files = {
                path.name: read_text(path)
                for path in sorted(config_dir.iterdir())
                if path.is_file()
            }
            summary = summarize(setup, config_dir)
            summary.update(case.get("summary_override") or {})
            records.append(
                {
                    "name": case["name"],
                    "initial_files": case["files"],
                    "env": case["api_key"] or {},
                    "channel_setup": bool(case["channel_setup"]),
                    "prompt": case["prompt"],
                    "final_files": files,
                    "summary": summary,
                }
            )
    return records


def main() -> None:
    fixture = {
        "templates": TEMPLATE_TEXT,
        "workspace_load": build_workspace_load(),
        "workspace_save": build_workspace_save(),
        "settings_enabled": build_settings_enabled(),
        "settings_window_config": build_settings_window_config(),
        "settings_window_store": build_settings_window_store(),
        "settings_compaction_percent": build_settings_compaction_percent(),
        "settings_show_thinking": build_settings_show_thinking(),
        "settings_subagent": build_settings_subagent(),
        "settings_mcp": build_settings_mcp(),
        "bootstrap_node": build_bootstrap_node(),
        "bootstrap_plugin": build_bootstrap_plugin(),
        "bootstrap_report": build_bootstrap_report(),
        "bootstrap_initialize": build_bootstrap_initialize(),
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    with FIXTURE_PATH.open("w", encoding="utf-8", newline="\n") as handle:
        json.dump(fixture, handle, ensure_ascii=False, indent=1)
        handle.write("\n")
    print("wrote %s: %s" % (FIXTURE_PATH, {key: len(value) for key, value in fixture.items()}))


if __name__ == "__main__":
    main()
