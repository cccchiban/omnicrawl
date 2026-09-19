#!/usr/bin/env python3
"""生成模型目录（model_catalog）的对照数据集。

期望值来自 Python 真实现：`/models` 探测的各失败分支、目录聚合、缓存命中与模型写回。

用法：``python rust/tools/gen_config_catalog_fixture.py``
输出：``rust/crates/omnicrawl-config/tests/fixtures/config_catalog_parity.json``
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
import urllib.error
import urllib.request
from pathlib import Path
from unittest import mock

import tomli_w

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-config/tests/fixtures/config_catalog_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.config.core import runtime as R  # noqa: E402
from omnicrawl.config.models import llm as L  # noqa: E402
from omnicrawl.config.models import model_catalog as MC  # noqa: E402
from omnicrawl.llm.registry import DiscoveryModel, DiscoveryResult  # noqa: E402

for module in (R, L, MC):
    if not Path(module.__file__).resolve().is_relative_to(ROOT):
        raise SystemExit("加载到的不是仓库源码")

MANAGED_HOME = "C:\\oc-catalog\\home"
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


class FakeResponse:
    def __init__(self, body: bytes) -> None:
        self._body = body

    def read(self, size: int = -1) -> bytes:
        return self._body if size < 0 else self._body[:size]

    def __enter__(self) -> "FakeResponse":
        return self

    def __exit__(self, *exc_info) -> bool:
        return False


DETECT_CASES = [
    ("正常去重", "https://api.example.com/v1", {"kind": "body", "text": '{"data":[{"id":"gpt-5"},{"id":"gpt-5"},{"id":"claude-3"}]}'}),
    ("顶层数组", "https://api.example.com/v1/", {"kind": "body", "text": '["gpt-5","gemini-2.5-pro"]'}),
    ("字段回退", "https://api.example.com/v1", {"kind": "body", "text": '{"data":[{"model":"glm-4"},{"name":"qwen3"},{"id":""},{"id":"   "},7]}'}),
    ("带 BOM", "https://api.example.com/v1", {"kind": "body", "bytes": [239, 187, 191, 123, 125]}),
    ("错误体", "https://api.example.com/v1", {"kind": "body", "text": '{"error":{"message":"quota exceeded"}}'}),
    ("错误串", "https://api.example.com/v1", {"kind": "body", "text": '{"error":"boom"}'}),
    ("空列表", "https://api.example.com/v1", {"kind": "body", "text": '{"data":[]}'}),
    ("非 JSON", "https://api.example.com/v1", {"kind": "body", "text": 'not json\n'}),
    ("非 UTF-8", "https://api.example.com/v1", {"kind": "body", "bytes": [255, 254, 253]}),
    ("缺 base_url", "", {"kind": "body", "text": "{}"}),
    ("HTTP 401", "https://api.example.com/v1", {"kind": "http", "status": 401}),
    ("HTTP 403", "https://api.example.com/v1", {"kind": "http", "status": 403}),
    ("HTTP 404", "https://api.example.com/v1", {"kind": "http", "status": 404}),
    ("HTTP 429", "https://api.example.com/v1", {"kind": "http", "status": 429}),
    ("HTTP 503", "https://api.example.com/v1", {"kind": "http", "status": 503}),
    ("HTTP 418", "https://api.example.com/v1", {"kind": "http", "status": 418}),
    ("连接失败", "https://api.example.com/v1", {"kind": "urlerror", "cause": "connection refused", "message": "<urlopen error connection refused>"}),
    ("读取失败", "https://api.example.com/v1", {"kind": "oserror", "message": "read error"}),
]


def build_catalog_detect() -> list[dict[str, object]]:
    records = []
    for name, base_url, response in DETECT_CASES:
        config = L.LLMConfig(
            api_key="sk-test",
            base_url=base_url,
            model="gpt-5",
            model_source="custom",
            profile_id="openai-main",
        )
        record: dict[str, object] = {"name": name, "base_url": base_url, "api_key": "sk-test"}
        record["response"] = response
        captured: dict[str, object] = {}

        def fake_urlopen(request, timeout=None):
            captured["url"] = request.full_url
            captured["timeout"] = timeout
            captured["authorization"] = request.get_header("Authorization")
            captured["accept"] = request.get_header("Accept")
            captured["user_agent"] = request.get_header("User-agent")
            captured["method"] = request.get_method()
            if response["kind"] == "body":
                if "bytes" in response:
                    return FakeResponse(bytes(response["bytes"]))
                return FakeResponse(str(response["text"]).encode("utf-8"))
            if response["kind"] == "http":
                raise urllib.error.HTTPError(
                    request.full_url, response["status"], "err", {}, None
                )
            if response["kind"] == "urlerror":
                raise urllib.error.URLError(response["cause"])
            raise OSError(response["message"])

        with mock.patch.object(MC.urllib.request, "urlopen", fake_urlopen):
            try:
                options = MC.detect_model_options(config)
                record["result"] = [option.to_ui_dict() for option in options]
            except Exception as exc:  # noqa: BLE001
                record["error"] = str(exc)
        record["request"] = {
            "url": captured.get("url"),
            "authorization": captured.get("authorization"),
            "accept": captured.get("accept"),
            "user_agent": captured.get("user_agent"),
            "method": captured.get("method"),
        }
        records.append(record)
    return records


def build_catalog_misc() -> dict[str, object]:
    providers = [
        "gpt-5",
        "chatgpt-4o",
        "o1-mini",
        "claude-sonnet-4-5",
        "anthropic/claude-3",
        "gemini-2.5-pro",
        "models/gemini-1.5",
        "deepseek-chat",
        "deepseek/v3",
        "qwen3-max",
        "qwq-32b",
        "glm-4",
        "chatglm3",
        "zhipu-ai",
        "unknown-model",
        "  GPT-5  ",
    ]
    options = [
        MC.ModelOption(id="gpt-5", name="gpt-5", provider="gpt"),
        MC.ModelOption(id="claude-sonnet-4-5", name="claude", provider="claude"),
    ]
    return {
        "provider_detect": [
            {"input": item, "expected": MC.detect_model_provider(item)} for item in providers
        ],
        "ensure_current": [
            {"options": [option.to_ui_dict() for option in options], "current": "", "expected": [option.to_ui_dict() for option in MC.ensure_current_model_option(options, "")]},
            {"options": [option.to_ui_dict() for option in options], "current": "gpt-5", "expected": [option.to_ui_dict() for option in MC.ensure_current_model_option(options, "gpt-5")]},
            {"options": [option.to_ui_dict() for option in options], "current": " gemini-2.5-pro ", "expected": [option.to_ui_dict() for option in MC.ensure_current_model_option(options, " gemini-2.5-pro ")]},
        ],
        "format_options": [
            {"options": [option.to_ui_dict() for option in options], "current": "gpt-5", "limit": 40, "expected": MC.format_model_options(options, current_model="gpt-5")},
            {"options": [option.to_ui_dict() for option in options], "current": "", "limit": 1, "expected": MC.format_model_options(options, limit=1)},
            {"options": [], "current": "", "limit": 40, "expected": MC.format_model_options([])},
        ],
        "env_override": [
            {"env": {}, "expected": False},
            {"env": {"OMNICRAWL_MODEL": "gpt-5"}, "expected": True},
            {"env": {"OPENAI_MODEL": " gpt-5 "}, "expected": True},
            {"env": {"OMNICRAWL_MODEL": "  "}, "expected": False},
        ],
    }


def descriptor_payload(item: MC.CatalogModel) -> dict[str, object]:
    descriptor = MC.catalog_model_to_descriptor(item)
    return {
        "profile_id": descriptor.identity.profile_id,
        "provider": descriptor.identity.provider,
        "protocol": descriptor.identity.protocol,
        "model_id": descriptor.identity.model_id,
        "catalog_key": descriptor.identity.catalog_key,
        "display_name": descriptor.display_name,
        "context_window_tokens": descriptor.context_window_tokens,
        "max_output_tokens": descriptor.max_output_tokens,
        "aliases": list(descriptor.aliases),
        "tags": list(descriptor.tags),
        "source": descriptor.source,
        "sort_order": descriptor.sort_order,
        "enabled": descriptor.enabled,
    }


def build_catalog_descriptor() -> list[dict[str, object]]:
    cases = [
        MC.CatalogModel(
            source="custom",
            key="gpt-5",
            profile_id="openai-main",
            provider="openai",
            protocol="openai_responses",
            model_id="gpt-5",
            display_name="GPT-5",
            context_window_tokens=200_000,
            tags=("fast",),
            aliases=("gpt5",),
            sort_order=3,
        ),
        MC.CatalogModel(
            source="detected",
            key="openai-main/gpt-5",
            profile_id="openai-main",
            provider="openai",
            protocol="openai_chat_completions",
            model_id="gpt-5",
            display_name="",
        ),
    ]
    records = []
    for item in cases:
        record: dict[str, object] = {"name": item.source}
        try:
            record["result"] = descriptor_payload(item)
        except Exception as exc:  # noqa: BLE001
            record["error"] = str(exc)
        records.append(record)
    return records


def build_catalog_save_model() -> list[dict[str, object]]:
    cases = [
        ("单模型段写回", {"llm": {"model": "old", "api_key": "sk"}}, "gemini-2.5-pro"),
        ("别名命中写回", {"llm": {"profiles": {"p": {"provider": "openai"}}}}, "claude"),
        ("profile/model 形式", {"llm": {"profiles": {"openai-main": {"provider": "openai"}}}}, "openai-main/gpt-5"),
        ("裸 model 走当前 Profile", {"llm": {"profiles": {"openai-main": {"provider": "openai"}}, "active_model": {"profile": "openai-main"}}}, "gpt-5"),
        ("空 ID", {"llm": {}}, "   "),
        ("无法解析 Profile", {"llm": {"profiles": {"p": {"provider": "openai"}}}}, "missing/xyz"),
    ]
    models_toml = {
        "version": 1,
        "models": {
            "claude": {
                "display_name": "Claude",
                "profile": "anthropic-main",
                "model_id": "claude-sonnet-4-5",
                "protocol": "anthropic_messages",
            }
        },
    }
    records = []
    with tempfile.TemporaryDirectory(prefix="oc-catalog-save-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, config, model_id) in enumerate(cases):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            write_text(user_dir / "config.toml", tomli_w.dumps(config))
            write_text(user_dir / "models.toml", tomli_w.dumps(models_toml))
            record: dict[str, object] = {
                "name": name,
                "model_id": model_id,
                "initial_config": config,
                "models": models_toml,
            }
            with Patched(str(user_dir)):
                try:
                    MC.save_llm_model(model_id)
                    record["config_text"] = read_text(user_dir / "config.toml")
                except Exception as exc:  # noqa: BLE001
                    record["error"] = str(exc)
            records.append(record)
    return records


def model_payload(model: MC.CatalogModel) -> dict[str, object]:
    return {
        "source": model.source,
        "key": model.key,
        "profile_id": model.profile_id,
        "provider": model.provider,
        "protocol": model.protocol,
        "model_id": model.model_id,
        "display_name": model.display_name,
        "context_window_tokens": model.context_window_tokens,
        "availability": model.availability,
        "matched_custom_key": model.matched_custom_key,
        "tags": list(model.tags),
        "aliases": list(model.aliases),
        "sort_order": model.sort_order,
    }


BUILD_CONFIG = {
    "llm": {
        "profiles": {
            "openai-main": {
                "provider": "openai",
                "api_key": "sk-x",
                "base_url": "https://api.example.com/v1",
                "discovery_timeout_seconds": 7,
            }
        },
        "active_model": {"key": "gpt-5"},
    }
}

BUILD_MODELS = {
    "version": 1,
    "models": {
        "gpt-5": {
            "display_name": "GPT-5",
            "profile": "openai-main",
            "model_id": "gpt-5",
            "protocol": "openai_chat_completions",
            "tags": ["fast"],
            "aliases": ["gpt5"],
            "sort_order": 2,
        },
        "disabled": {
            "display_name": "关闭的模型",
            "profile": "openai-main",
            "model_id": "gpt-4o",
            "protocol": "openai_chat_completions",
            "enabled": False,
        },
    },
}

DISCOVERY_OK = DiscoveryResult(
    profile_id="openai-main",
    status="ok",
    models=(
        DiscoveryModel(profile_id="openai-main", provider="openai", protocol="openai_chat_completions", model_id="gpt-5", display_name="GPT-5"),
        DiscoveryModel(profile_id="openai-main", provider="openai", protocol="openai_chat_completions", model_id="gpt-4.1", display_name=""),
    ),
)

DISCOVERY_FAILED = DiscoveryResult(
    profile_id="openai-main",
    status="unavailable",
    models=(),
    message="",
)


def build_catalog_build() -> list[dict[str, object]]:
    cases = [
        ("命中自定义条目", DISCOVERY_OK, True, False),
        ("发现失败出诊断", DISCOVERY_FAILED, True, False),
        ("跳过发现", DISCOVERY_OK, False, False),
        ("刷新绕过缓存", DISCOVERY_OK, True, True),
    ]
    records = []
    with tempfile.TemporaryDirectory(prefix="oc-catalog-build-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, discovery, include_detected, second_pass) in enumerate(cases):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            write_text(user_dir / "config.toml", tomli_w.dumps(BUILD_CONFIG))
            write_text(user_dir / "models.toml", tomli_w.dumps(BUILD_MODELS))
            calls: list[tuple[str, float]] = []

            class FakeAdapter:
                def discover_models(self, profile, timeout_seconds=None):
                    calls.append((profile.id, timeout_seconds))
                    return discovery

            record: dict[str, object] = {
                "name": name,
                "include_detected": include_detected,
                "second_pass": second_pass,
                "discovery_status": discovery.status,
                "config": BUILD_CONFIG,
                "models": BUILD_MODELS,
            }
            with Patched(str(user_dir)):
                MC.clear_discovery_cache()
                with mock.patch.object(MC, "get_adapter", lambda protocol: FakeAdapter()):
                    first = MC.build_catalog(include_detected=include_detected)
                    first_calls = list(calls)
                    calls.clear()
                    second_calls = list(calls)
                    if second_pass:
                        second = MC.build_catalog(include_detected=include_detected, refresh=True)
                        second_calls = list(calls)
                    else:
                        second = MC.build_catalog(include_detected=include_detected)
                        second_calls = list(calls)
            record["first"] = {
                "current": first["current"],
                "custom": [model_payload(item) for item in first["custom"]],
                "detected": sorted(
                    (model_payload(item) for item in first["detected"]),
                    key=lambda item: (item["profile_id"], item["model_id"]),
                ),
                "diagnostics": sorted(first["diagnostics"], key=lambda item: item["profile"]),
            }
            record["first_calls"] = len(first_calls)
            record["second_calls"] = len(second_calls)
            record["second_detected"] = sorted(
                (model_payload(item) for item in second["detected"]),
                key=lambda item: (item["profile_id"], item["model_id"]),
            )
            records.append(record)
    return records


def main() -> None:
    fixture = {
        "catalog_detect": build_catalog_detect(),
        "catalog_misc": build_catalog_misc(),
        "catalog_descriptor": build_catalog_descriptor(),
        "catalog_save_model": build_catalog_save_model(),
        "catalog_build": build_catalog_build(),
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    with FIXTURE_PATH.open("w", encoding="utf-8", newline="\n") as handle:
        json.dump(fixture, handle, ensure_ascii=False, indent=1)
        handle.write("\n")
    print("wrote %s: %s" % (FIXTURE_PATH, {key: len(value) for key, value in fixture.items()}))


if __name__ == "__main__":
    main()
