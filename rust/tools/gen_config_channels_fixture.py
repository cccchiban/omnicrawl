#!/usr/bin/env python3
"""生成模型渠道配置（channels）的对照数据集。

期望值来自 Python 真实现：默认草稿、key 归一化、渠道读取、写回文本与凭据判定。

用法：``python rust/tools/gen_config_channels_fixture.py``
输出：``rust/crates/omnicrawl-config/tests/fixtures/config_channels_parity.json``
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
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-config/tests/fixtures/config_channels_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.config.core import runtime as R  # noqa: E402
from omnicrawl.config.models import channels as C  # noqa: E402

for module in (R, C):
    if not Path(module.__file__).resolve().is_relative_to(ROOT):
        raise SystemExit("加载到的不是仓库源码")

MANAGED_HOME = "C:\\oc-channels\\home"
MANAGED_DIR = MANAGED_HOME + "\\" + R.USER_CONFIG_DIRNAME


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


def write_text(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="\n") as handle:
        handle.write(content)


def read_text(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def channel_payload(channel: C.ChannelConfig) -> dict[str, object]:
    return {
        "key": channel.key,
        "name": channel.name,
        "profile_id": channel.profile_id,
        "provider": channel.provider,
        "protocol": channel.protocol,
        "base_url": channel.base_url,
        "api_key": channel.api_key,
        "model_id": channel.model_id,
        "enabled": channel.enabled,
        "api_key_env": channel.api_key_env,
        "user_agent": channel.user_agent,
    }


def build_channel_load() -> list[dict[str, object]]:
    cases = [
        ("缺文件", {}, None),
        (
            "正常配对",
            {
                "llm": {
                    "profiles": {
                        "openai-main": {
                            "provider": "openai",
                            "api_key": "sk-x",
                            "base_url": "https://api.openai.com/v1",
                            "user_agent": "ua/1.0",
                        }
                    },
                    "active_model": {"key": "gpt-5"},
                }
            },
            {
                "version": 1,
                "models": {
                    "gpt-5": {
                        "display_name": "GPT-5",
                        "profile": "openai-main",
                        "model_id": "gpt-5",
                        "protocol": "openai_responses",
                    },
                    "orphan": {"display_name": "孤儿", "profile": "missing", "model_id": "x"},
                    "not-a-table": 5,
                },
            },
        ),
        (
            "protocol 回落到 Provider 默认",
            {
                "llm": {
                    "profiles": {"anthropic-main": {"provider": "Anthropic"}},
                    "active_model": {"key": "claude"},
                }
            },
            {
                "models": {
                    "claude": {"profile": "anthropic-main", "model_id": "claude-sonnet-4-5"}
                }
            },
        ),
        (
            "default_protocol 回落",
            {
                "llm": {
                    "profiles": {
                        "gem-main": {"provider": "gemini", "default_protocol": "gemini_generate_content"}
                    },
                    "active_model": {"key": "gem"},
                }
            },
            {"models": {"gem": {"profile": "gem-main", "model_id": "gemini-2.5-pro"}}},
        ),
        (
            "关闭条目仍列出",
            {
                "llm": {
                    "profiles": {"openai-main": {"provider": "openai", "enabled": False}},
                    "active_model": {"key": "gpt-5"},
                }
            },
            {
                "models": {
                    "gpt-5": {"profile": "openai-main", "model_id": "gpt-5", "enabled": False}
                }
            },
        ),
        (
            "默认 key 回落到首个启用项",
            {
                "llm": {
                    "profiles": {
                        "a": {"provider": "openai"},
                        "b": {"provider": "gemini"},
                    },
                    "active_model": {"key": "missing"},
                }
            },
            {
                "models": {
                    "first": {"profile": "a", "model_id": "gpt-5"},
                    "second": {"profile": "b", "model_id": "gemini-2.5-pro"},
                }
            },
        ),
        (
            "模型条目缺 profile",
            {"llm": {"profiles": {}}},
            {"models": {"x": {"display_name": "无 profile"}}},
        ),
    ]
    records = []
    with tempfile.TemporaryDirectory(prefix="oc-channels-load-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, config, models) in enumerate(cases):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            if config:
                write_text(user_dir / "config.toml", tomli_w.dumps(config))
            if models is not None:
                write_text(user_dir / "models.toml", tomli_w.dumps(models))
            record: dict[str, object] = {"name": name, "config": config, "models": models}
            with Patched(str(user_dir)):
                try:
                    configuration = C.load_channel_configuration()
                    record["result"] = {
                        "channels": [channel_payload(item) for item in configuration.channels],
                        "default_key": configuration.default_key,
                    }
                except Exception as exc:  # noqa: BLE001
                    record["error"] = str(exc)
            records.append(record)
    return records


def channels_fixture() -> list[C.ChannelConfig]:
    return [
        C.ChannelConfig(
            key="openai-main",
            name="OpenAI 主渠道",
            profile_id="openai-main",
            provider="openai",
            protocol="openai_chat_completions",
            base_url="https://api.openai.com/v1",
            api_key="sk-x",
            model_id="gpt-5",
            enabled=True,
            api_key_env="OPENAI_API_KEY",
            user_agent="ua/1.0",
        ),
        C.ChannelConfig(
            key="gem-main",
            name="Gemini 主渠道",
            profile_id="gem-main",
            provider="gemini",
            protocol="gemini_generate_content",
            base_url="https://generativelanguage.googleapis.com",
            api_key="",
            model_id="gemini-2.5-pro",
            enabled=True,
            api_key_env="GEMINI_API_KEY",
            user_agent="",
        ),
    ]


SAVE_CASES = [
    ("清理被移除渠道", "gpt-5"),
    ("默认 key 回落到启用项", "gem-main"),
]


def build_channel_save() -> list[dict[str, object]]:
    records = []
    with tempfile.TemporaryDirectory(prefix="oc-channels-save-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, default_key) in enumerate(SAVE_CASES):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            config_path = user_dir / "config.toml"
            models_path = user_dir / "models.toml"
            write_text(
                config_path,
                tomli_w.dumps(
                    {
                        "llm": {
                            "profiles": {
                                "stale-main": {"provider": "openai", "api_key": "sk-stale"},
                                "openai-main": {"provider": "openai", "api_key": "old"},
                            },
                            "active_model": {"source": "custom", "key": "stale"},
                        }
                    }
                ),
            )
            write_text(
                models_path,
                tomli_w.dumps(
                    {
                        "version": 1,
                        "models": {
                            "stale": {"profile": "stale-main", "model_id": "stale-model"},
                            "openai-main": {
                                "profile": "openai-main",
                                "model_id": "old-model",
                                "api_key": "leak",
                            },
                        },
                    }
                ),
            )
            record: dict[str, object] = {
                "name": name,
                "channels": [channel_payload(item) for item in channels_fixture()],
                "default_key": default_key,
            }
            with Patched(str(user_dir)):
                try:
                    C.save_channel_configuration(
                        C.ChannelConfiguration(tuple(channels_fixture()), default_key)
                    )
                    record["config_text"] = read_text(config_path)
                    record["models_text"] = read_text(models_path)
                except Exception as exc:  # noqa: BLE001
                    record["error"] = str(exc)
            records.append(record)
    return records


def invalid_channels(kind: str) -> list[C.ChannelConfig]:
    base = channels_fixture()[0]
    if kind == "空集合":
        return []
    if kind == "key 非法":
        return [C.ChannelConfig(**{**base.__dict__, "key": "Bad Key"})]
    if kind == "key 重复":
        return [base, base]
    if kind == "缺名称":
        return [C.ChannelConfig(**{**base.__dict__, "name": "  "})]
    if kind == "请求方式不支持":
        return [C.ChannelConfig(**{**base.__dict__, "provider": "mistral"})]
    if kind == "协议不匹配":
        return [C.ChannelConfig(**{**base.__dict__, "protocol": "anthropic_messages"})]
    if kind == "Base URL 无效":
        return [C.ChannelConfig(**{**base.__dict__, "base_url": "api.openai.com"})]
    if kind == "缺模型 ID":
        return [C.ChannelConfig(**{**base.__dict__, "model_id": " "})]
    if kind == "User-Agent 含换行":
        return [C.ChannelConfig(**{**base.__dict__, "user_agent": "a\nb"})]
    if kind == "全部禁用":
        return [C.ChannelConfig(**{**base.__dict__, "enabled": False})]
    if kind == "Profile 复用冲突":
        second = C.ChannelConfig(**{**base.__dict__, "key": "other", "provider": "gemini"})
        return [base, second]
    raise AssertionError(kind)


INVALID_KINDS = [
    "空集合",
    "key 非法",
    "key 重复",
    "缺名称",
    "请求方式不支持",
    "协议不匹配",
    "Base URL 无效",
    "缺模型 ID",
    "User-Agent 含换行",
    "全部禁用",
    "Profile 复用冲突",
]


def build_channel_validate() -> list[dict[str, object]]:
    records = []
    for kind in INVALID_KINDS:
        record: dict[str, object] = {"name": kind}
        channels = invalid_channels(kind)
        record["channels"] = [channel_payload(item) for item in channels]
        record["default_key"] = channels[0].key if channels else ""
        with tempfile.TemporaryDirectory(prefix="oc-channels-validate-") as tmp:
            user_dir = Path(tmp).resolve() / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            with Patched(str(user_dir)):
                try:
                    C.save_channel_configuration(
                        C.ChannelConfiguration(tuple(channels), record["default_key"])
                    )
                    record["result"] = "saved"
                except Exception as exc:  # noqa: BLE001
                    record["error"] = str(exc)
        records.append(record)
    return records


def build_channel_credentials() -> list[dict[str, object]]:
    """凭据只认渠道里的明文 `api_key`：环境变量通道已从 Rust 侧移除。

    期望值按「明文优先且只认明文」的口径录制，与 Rust 的
    `missing_enabled_credentials` / `has_usable_channel` 逐字段一致。
    """
    cases = [
        ("全部具备", {}),
        ("默认渠道禁用", {"disable_default": True}),
    ]

    def missing_of(channels, default_key):
        return [
            item.name
            for item in channels
            if item.enabled and not (item.api_key or "").strip()
        ]

    def usable_of(channels, default_key):
        for item in channels:
            if item.key == default_key and item.enabled:
                return bool((item.api_key or "").strip())
        return False

    records = []
    for name, options in cases:
        channels = channels_fixture()
        if options.get("disable_default"):
            channels = [
                C.ChannelConfig(**{**item.__dict__, "enabled": False}) if index == 0 else item
                for index, item in enumerate(channels)
            ]
        default_key = "gem-main" if options.get("disable_default") else "openai-main"
        configuration = C.ChannelConfiguration(tuple(channels), default_key)
        records.append(
            {
                "name": name,
                "channels": [channel_payload(item) for item in channels],
                "default_key": default_key,
                "env": {},
                "missing": missing_of(channels, default_key),
                "usable": usable_of(channels, default_key),
            }
        )
    return records


def build_channel_defaults() -> list[dict[str, object]]:
    records = []
    for provider, key in [
        ("openai", None),
        ("anthropic", "claude-main"),
        ("gemini", ""),
        ("mistral", None),
    ]:
        record: dict[str, object] = {"provider": provider, "key": key}
        try:
            record["result"] = channel_payload(C.default_channel(provider, key=key))
        except Exception as exc:  # noqa: BLE001
            record["error"] = str(exc)
        records.append(record)
    return records


def build_channel_unique_key() -> list[dict[str, object]]:
    cases = [
        ("OpenAI 主渠道", []),
        ("  GPT 5  ", []),
        ("OpenAI 主渠道", ["openai-主渠道"]),
        ("!!!", []),
        ("a b c", ["a-b-c"]),
        ("a b c", ["a-b-c", "a-b-c-2"]),
        ("---", []),
    ]
    return [
        {"name": name, "existing": existing, "expected": C.unique_channel_key(name, set(existing))}
        for name, existing in cases
    ]


def build_channel_labels() -> list[dict[str, object]]:
    records = []
    for provider in ["openai", "anthropic", "gemini", "mistral"]:
        records.append({"kind": "provider", "value": provider, "label": C.provider_label(provider)})
    for protocol in [
        "openai_chat_completions",
        "openai_responses",
        "anthropic_messages",
        "gemini_generate_content",
        "unknown_protocol",
    ]:
        records.append({"kind": "protocol", "value": protocol, "label": C.protocol_label(protocol)})
    return records


def main() -> None:
    fixture = {
        "channel_defaults": build_channel_defaults(),
        "channel_unique_key": build_channel_unique_key(),
        "channel_labels": build_channel_labels(),
        "channel_load": build_channel_load(),
        "channel_save": build_channel_save(),
        "channel_validate": build_channel_validate(),
        "channel_credentials": build_channel_credentials(),
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    with FIXTURE_PATH.open("w", encoding="utf-8", newline="\n") as handle:
        json.dump(fixture, handle, ensure_ascii=False, indent=1)
        handle.write("\n")
    print("wrote %s: %s" % (FIXTURE_PATH, {key: len(value) for key, value in fixture.items()}))


if __name__ == "__main__":
    main()
