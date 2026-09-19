#!/usr/bin/env python3
"""生成模型配置（`config/models/` 的 llm / llm_multi / model_store / vision）对照数据集。

期望值来自 Python 真实现：推理强度归一化、`LLMConfig` 归一化与校验、模型目录解析与写回、
视觉代理与原生视觉三态、多模型 Profile 解析与模型选择。

用法：``python rust/tools/gen_config_models_fixture.py``
输出：``rust/crates/omnicrawl-config/tests/fixtures/config_models_parity.json``
"""

from __future__ import annotations

import dataclasses
import json
import os
import sys
import tempfile
from pathlib import Path
from unittest import mock

import tomli_w

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-config/tests/fixtures/config_models_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.config.core import runtime as R  # noqa: E402
from omnicrawl.config.models import llm as L  # noqa: E402
from omnicrawl.config.models import llm_multi as M  # noqa: E402
from omnicrawl.config.models import model_store as S  # noqa: E402
from omnicrawl.config.models import vision as V  # noqa: E402

for module in (R, L, M, S, V):
    if not Path(module.__file__).resolve().is_relative_to(ROOT):
        raise SystemExit("加载到的不是仓库源码")

MANAGED_HOME = "C:\\oc-models\\home"
MANAGED_DIR = MANAGED_HOME + "\\" + R.USER_CONFIG_DIRNAME


class Patched:
    """统一注入 home 与环境变量（Python 侧清空 os.environ 后再给用例值）。"""

    def __init__(self, env: dict[str, str], managed_dir: str = MANAGED_DIR) -> None:
        self._env = {"USERPROFILE": MANAGED_HOME, "HOME": MANAGED_HOME}
        self._env.update(env)
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


def as_json(value):
    if dataclasses.is_dataclass(value):
        return as_json(dataclasses.asdict(value))
    if isinstance(value, dict):
        return {key: as_json(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [as_json(item) for item in value]
    if isinstance(value, Path):
        return str(value)
    return value


# ---------------------------------------------------------------- 推理强度


REASONING_EFFORT_CASES = [
    "", " ", "disabled", "off", "none", "low", "medium", "med", "high", "xhigh",
    "x_high", "extra_high", "very_high", "max", "maximum", "NONE", " High ", "high-x",
    "high x", "bogus", "極高",
]


def build_reasoning_effort() -> list[dict[str, object]]:
    cases = []
    for value in REASONING_EFFORT_CASES:
        try:
            record = {"value": value, "expected": L.normalize_reasoning_effort(value)}
        except L.LLMError as exc:
            record = {"value": value, "error": str(exc)}
        cases.append(record)
    return cases


# ---------------------------------------------------------------- LLMConfig


LLM_NORMALIZE_CASES = [
    ("默认视图", {}, {}),
    ("凭据从环境取", {}, {"OPENAI_API_KEY": "env-key", "OPENAI_BASE_URL": "https://env", "OPENAI_MODEL": "env-model"}),
    (
        "legacy 齐全",
        {"api_key": "k", "base_url": "https://api", "model": "gpt-5.2"},
        {},
    ),
    ("legacy 缺 api_key", {"base_url": "https://api", "model": "m"}, {}),
    ("legacy 缺 base_url", {"api_key": "k", "model": "m"}, {}),
    ("legacy 缺 model", {"api_key": "k", "base_url": "https://api"}, {}),
    ("custom 齐全", {"model_source": "custom", "model": "m", "api_key": "k"}, {}),
    ("custom 缺 model", {"model_source": "custom", "api_key": "k"}, {}),
    ("custom 缺 api_key", {"model_source": "custom", "model": "m"}, {}),
    ("custom 缺 api_key 带 env 名", {"model_source": "custom", "model": "m", "api_key_env": "MY_KEY"}, {}),
    ("窗口为零", {"api_key": "k", "base_url": "https://api", "model": "m", "context_window_tokens": 0}, {}),
    ("窗口为负", {"api_key": "k", "base_url": "https://api", "model": "m", "context_window_tokens": -1}, {}),
    ("用户代理带换行", {"api_key": "k", "base_url": "https://api", "model": "m", "user_agent": "a\nb"}, {}),
    ("用户代理带回车", {"api_key": "k", "base_url": "https://api", "model": "m", "user_agent": "a\rb"}, {}),
    ("思考类型空白", {"api_key": "k", "base_url": "https://api", "model": "m", "thinking_type": "  "}, {}),
    ("思考类型大写", {"api_key": "k", "base_url": "https://api", "model": "m", "thinking_type": " ENABLED "}, {}),
    ("推理强度别名", {"api_key": "k", "base_url": "https://api", "model": "m", "reasoning_effort": " Extra_High "}, {}),
    ("推理强度非法", {"api_key": "k", "base_url": "https://api", "model": "m", "reasoning_effort": "bogus"}, {}),
    (
        "字段两端空白",
        {"api_key": " k ", "base_url": " https://api ", "model": " m ", "user_agent": " ua "},
        {},
    ),
    ("原生视觉显式关闭", {"api_key": "k", "base_url": "https://api", "model": "m", "native_vision": False}, {}),
]


def build_llm_normalize() -> list[dict[str, object]]:
    cases = []
    for name, inputs, env in LLM_NORMALIZE_CASES:
        with Patched(env):
            try:
                config = L.LLMConfig(**inputs)
                record: dict[str, object] = {"expected": as_json(config)}
            except L.LLMError as exc:
                record = {"error": str(exc)}
        record.update({"name": name, "input": inputs, "env": env})
        cases.append(record)
    return cases


THINKING_CASES = [
    ("disabled", ""), ("", "disabled"), ("enabled", ""), ("", ""), ("none", "enabled"),
    ("high", "disabled"), ("max", "enabled"), ("disabled", "enabled"),
]


def build_thinking() -> list[dict[str, object]]:
    cases = []
    for effort, thinking in THINKING_CASES:
        config = L.LLMConfig(api_key="k", base_url="https://api", model="m")
        config.reasoning_effort = effort
        config.thinking_type = thinking
        cases.append(
            {"reasoning_effort": effort, "thinking_type": thinking, "expected": config.thinking_enabled}
        )
    return cases


ACTIVE_REF_CASES = [
    ("custom", {"source": "custom", "key": "local-qwen"}),
    ("detected 全字段", {"source": "detected", "profile": "openai", "model_id": "gpt-5.2", "protocol": "openai_chat_completions"}),
    ("detected 空协议", {"source": "detected", "profile": "p", "model_id": "m", "protocol": ""}),
]


def build_active_refs() -> list[dict[str, object]]:
    cases = []
    for name, inputs in ACTIVE_REF_CASES:
        reference = L.ActiveModelRef(**inputs)
        cases.append({"name": name, "input": inputs, "expected": reference.to_dict()})
    return cases


# ---------------------------------------------------------------- model_store


MODEL_STORE_CASES = [
    ("空对象", {}),
    ("版本缺省", {"models": {}}),
    ("版本为一", {"version": 1, "models": {}}),
    ("版本为零", {"version": 0, "models": {}}),
    ("版本为字符串", {"version": "1", "models": {}}),
    ("版本为浮点", {"version": 1.5, "models": {}}),
    ("models 不是对象", {"version": 1, "models": [1, 2]}),
    ("models 为空串", {"version": 1, "models": ""}),
    (
        "最小合法条目",
        {
            "version": 1,
            "models": {"a": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions"}},
        },
    ),
    (
        "完整条目",
        {
            "version": 2,
            "models": {
                "local-qwen": {
                    "display_name": " 本地 Qwen ",
                    "profile": " p ",
                    "model_id": " qwen3 ",
                    "protocol": "openai_responses",
                    "enabled": False,
                    "aliases": [" qwen ", "", "本地"],
                    "description": " 说明 ",
                    "tags": ["a", " b "],
                    "context_window_tokens": 32768,
                    "max_output_tokens": 4096,
                    "temperature": 0.7,
                    "native_vision": True,
                    "capabilities": {"streaming": True, "tools": False, "vision": True},
                    "provider_options": {"top_p": 0.9},
                    "sort_order": 3,
                }
            },
        },
    ),
    ("key 含大写", {"models": {"Bad": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions"}}}),
    ("key 含斜杠", {"models": {"a/b": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions"}}}),
    ("key 以连字符开头", {"models": {"-a": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions"}}}),
    ("条目不是对象", {"models": {"a": 5}}),
    ("缺 profile", {"models": {"a": {"model_id": "m", "protocol": "openai_chat_completions"}}}),
    ("缺 model_id", {"models": {"a": {"profile": "p", "protocol": "openai_chat_completions"}}}),
    ("协议不支持", {"models": {"a": {"profile": "p", "model_id": "m", "protocol": "bogus"}}}),
    ("协议缺失", {"models": {"a": {"profile": "p", "model_id": "m"}}}),
    ("enabled 非布尔", {"models": {"a": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions", "enabled": 1}}}),
    ("aliases 非列表", {"models": {"a": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions", "aliases": "x"}}}),
    ("tags 非列表", {"models": {"a": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions", "tags": 5}}}),
    ("temperature 非数字", {"models": {"a": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions", "temperature": "x"}}}),
    ("temperature 空串", {"models": {"a": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions", "temperature": ""}}}),
    ("sort_order 非整数", {"models": {"a": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions", "sort_order": 1.5}}}),
    ("native_vision 非布尔", {"models": {"a": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions", "native_vision": "yes"}}}),
    ("provider_options 非对象", {"models": {"a": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions", "provider_options": 5}}}),
    ("窗口负数", {"models": {"a": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions", "context_window_tokens": -1}}}),
    ("窗口浮点", {"models": {"a": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions", "context_window_tokens": 1.5}}}),
    ("凭据字段 api_key", {"models": {"a": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions", "api_key": "x"}}}),
    ("凭据字段 token", {"models": {"a": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions", "token": "x"}}}),
    (
        "别名冲突",
        {
            "models": {
                "a": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions", "aliases": ["x"]},
                "b": {"profile": "p", "model_id": "n", "protocol": "openai_chat_completions", "aliases": ["x"]},
            }
        },
    ),
    (
        "排序",
        {
            "models": {
                "zeta": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions", "sort_order": 1},
                "alpha": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions"},
                "Beta": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions"},
            }
        },
    ),
    (
        "能力窗口回填",
        {
            "models": {
                "a": {
                    "profile": "p",
                    "model_id": "m",
                    "protocol": "openai_chat_completions",
                    "context_window_tokens": 1000,
                    "capabilities": {"max_output_tokens": 500},
                }
            }
        },
    ),
    (
        "顶层窗口优先于能力窗口",
        {
            "models": {
                "a": {
                    "profile": "p",
                    "model_id": "m",
                    "protocol": "openai_chat_completions",
                    "context_window_tokens": 1000,
                    "max_output_tokens": 128,
                    "capabilities": {"context_window_tokens": 9999, "max_output_tokens": 500},
                }
            }
        },
    ),
    (
        "温度写在 provider_options",
        {
            "models": {
                "a": {
                    "profile": "p",
                    "model_id": "m",
                    "protocol": "openai_chat_completions",
                    "provider_options": {"temperature": 0.3},
                }
            }
        },
    ),
    (
        "顶层温度优先",
        {
            "models": {
                "a": {
                    "profile": "p",
                    "model_id": "m",
                    "protocol": "openai_chat_completions",
                    "temperature": 0.1,
                    "provider_options": {"temperature": 0.3},
                }
            }
        },
    ),
]


def build_model_store_parse() -> list[dict[str, object]]:
    cases = []
    for name, data in MODEL_STORE_CASES:
        try:
            store = S.parse_model_store(data)
            record: dict[str, object] = {"expected": {"version": store.version, "models": as_json(store.models)}}
        except S.ModelStoreError as exc:
            record = {"error": str(exc)}
        record.update({"name": name, "data": data})
        cases.append(record)
    return cases


MODEL_SAVE_CASES = [
    (
        "最小条目",
        S.ModelStore(version=1, models=(S.CustomModelRecord(key="a", display_name="A", profile="p", model_id="m", protocol="openai_chat_completions"),)),
    ),
    (
        "完整条目",
        S.ModelStore(
            version=2,
            models=(
                S.CustomModelRecord(
                    key="local-qwen",
                    display_name="本地 Qwen",
                    profile="p",
                    model_id="qwen3",
                    protocol="openai_responses",
                    enabled=False,
                    aliases=("qwen", "本地"),
                    description="说明",
                    tags=("a",),
                    context_window_tokens=32768,
                    max_output_tokens=4096,
                    temperature=0.7,
                    native_vision=True,
                    provider_options={"top_p": 0.9},
                    sort_order=3,
                ),
            ),
        ),
    ),
    (
        "能力只写有意义的键",
        S.ModelStore(
            version=1,
            models=(
                S.CustomModelRecord(
                    key="a",
                    display_name="A",
                    profile="p",
                    model_id="m",
                    protocol="openai_chat_completions",
                    capabilities=S.ModelCapabilities(streaming=False, tools=True, vision=False),
                ),
            ),
        ),
    ),
    ("空目录", S.ModelStore(version=1, models=())),
]


def build_model_store_save() -> list[dict[str, object]]:
    cases = []
    with tempfile.TemporaryDirectory(prefix="oc-models-store-save-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, store) in enumerate(MODEL_SAVE_CASES):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            target = user_dir / "models.toml"
            with Patched({}, str(user_dir)):
                S.save_model_store(store)
            cases.append(
                {
                    "name": name,
                    "store": as_json(store),
                    "text": target.read_text(encoding="utf-8"),
                }
            )
    return cases


def build_resolve_alias() -> list[dict[str, object]]:
    store = S.ModelStore(
        version=1,
        models=(
            S.CustomModelRecord(key="a", display_name="A", profile="p", model_id="m", protocol="openai_chat_completions", aliases=("x",)),
            S.CustomModelRecord(key="b", display_name="B", profile="p", model_id="n", protocol="openai_chat_completions", aliases=("y", "x")),
        ),
    )
    single = S.ModelStore(
        version=1,
        models=(
            S.CustomModelRecord(key="a", display_name="A", profile="p", model_id="m", protocol="openai_chat_completions", aliases=("x",)),
        ),
    )
    cases = []
    for name, target, token in (
        ("按 key", single, "a"),
        ("按别名", single, " x "),
        ("空白", single, "   "),
        ("未命中", single, "zzz"),
        ("多命中", store, "x"),
    ):
        try:
            record = target.resolve_alias(token)
            value = record.key if record is not None else None
            cases.append({"name": name, "token": token, "expected": value})
        except S.ModelStoreError as exc:
            cases.append({"name": name, "token": token, "error": str(exc)})
    return cases


# ---------------------------------------------------------------- load / save


LLM_LOAD_CASES = [
    ("legacy 单模型", {"llm": {"api_key": "k", "base_url": "https://api", "model": "m"}}, {}),
    (
        "legacy 全部可选字段",
        {
            "llm": {
                "api_key": "k",
                "base_url": "https://api",
                "model": "m",
                "thinking_type": "enabled",
                "reasoning_effort": "high",
                "user_agent": "ua",
                "context_window_tokens": 64000,
            }
        },
        {},
    ),
    ("缺 base_url", {"llm": {"api_key": "k", "model": "m"}}, {}),
    ("环境变量补齐", {"llm": {}}, {"OPENAI_API_KEY": "ek", "OPENAI_BASE_URL": "https://env", "OPENAI_MODEL": "em"}),
    ("环境变量优先", {"llm": {"api_key": "k", "base_url": "https://api", "model": "m"}}, {"OPENAI_MODEL": "from-env"}),
    ("上下文窗口非法", {"llm": {"api_key": "k", "base_url": "https://api", "model": "m", "context_window_tokens": 0}}, {}),
    ("llm 段不是对象", {"llm": 5}, {}),
    (
        "多模型段",
        {
            "llm": {
                "profiles": {
                    "ch1": {"provider": "openai", "base_url": "https://api", "api_key": "k"},
                },
                "active_model": {"source": "detected", "profile": "ch1", "model_id": "gpt-5.2"},
            }
        },
        {},
    ),
]


def build_llm_load() -> list[dict[str, object]]:
    cases = []
    with tempfile.TemporaryDirectory(prefix="oc-models-load-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, config, env) in enumerate(LLM_LOAD_CASES):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            write_text(user_dir / "config.toml", tomli_w.dumps(config))
            with Patched(env, str(user_dir)):
                try:
                    record: dict[str, object] = {"expected": as_json(L.load_llm_config())}
                except (L.LLMError, S.ModelStoreError) as exc:
                    record = {"error": str(exc)}
            record.update({"name": name, "config": config, "env": env})
            cases.append(record)
    return cases


LLM_SAVE_CASES = [
    (
        "写推理强度（单模型）",
        {"llm": {"api_key": "k", "base_url": "https://api", "model": "m"}},
        "save_reasoning_effort",
        "high",
    ),
    (
        "写推理强度（多模型）",
        {"llm": {"profiles": {"p": {"provider": "openai"}}, "defaults": {"reasoning_effort": "low"}}},
        "save_reasoning_effort",
        "none",
    ),
    (
        "写推理强度非法",
        {"llm": {"api_key": "k", "base_url": "https://api", "model": "m"}},
        "save_reasoning_effort",
        "bogus",
    ),
    (
        "写模型引用（单模型）",
        {"llm": {"api_key": "k", "base_url": "https://api", "model": "m"}},
        "save_active_model_ref",
        {"source": "custom", "key": "local-qwen", "model_id": ""},
    ),
    (
        "写模型引用（多模型）",
        {"llm": {"profiles": {"p": {"provider": "openai"}}}},
        "save_active_model_ref",
        {"source": "detected", "profile": "p", "model_id": "gpt-5.2", "protocol": "openai_chat_completions"},
    ),
]


def build_llm_save() -> list[dict[str, object]]:
    cases = []
    with tempfile.TemporaryDirectory(prefix="oc-models-save-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, config, action, payload) in enumerate(LLM_SAVE_CASES):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            target = user_dir / "config.toml"
            write_text(target, tomli_w.dumps(config))
            with Patched({}, str(user_dir)):
                try:
                    if action == "save_reasoning_effort":
                        L.save_reasoning_effort(payload)
                    else:
                        L.save_active_model_ref(L.ActiveModelRef(**payload))
                    text = target.read_text(encoding="utf-8")
                    record: dict[str, object] = {"text": text}
                except (L.LLMError, S.ModelStoreError) as exc:
                    record = {"error": str(exc)}
            record.update({"name": name, "config": config, "action": action, "payload": payload})
            cases.append(record)
    return cases


# ---------------------------------------------------------------- vision


VISION_LOAD_CASES = [
    ("缺段", {}),
    ("空段", {"vision": {}}),
    ("段为空串", {"vision": ""}),
    ("段不是对象", {"vision": []}),
    ("开关非布尔", {"vision": {"enabled": 1}}),
    ("开关打开", {"vision": {"enabled": True}}),
    ("模型引用非列表", {"vision": {"models": 5}}),
    ("自定义引用", {"vision": {"enabled": True, "models": [{"source": "custom", "key": " a "}]}}),
    (
        "检测引用",
        {
            "vision": {
                "models": [
                    {"source": "detected", "profile": "p", "model_id": "m", "protocol": "openai_chat_completions"}
                ]
            }
        },
    ),
    ("引用项非对象", {"vision": {"models": ["x"]}}),
    ("自定义缺 key", {"vision": {"models": [{"source": "custom"}]}}),
    ("检测缺 model_id", {"vision": {"models": [{"source": "detected", "profile": "p"}]}}),
    (
        "检测协议非法",
        {"vision": {"models": [{"source": "detected", "profile": "p", "model_id": "m", "protocol": "bogus"}]}},
    ),
    ("来源非法", {"vision": {"models": [{"source": "other", "key": "a"}]}}),
    (
        "重复引用",
        {
            "vision": {
                "models": [
                    {"source": "custom", "key": "a"},
                    {"source": "custom", "key": "a"},
                ]
            }
        },
    ),
    (
        "有序引用",
        {
            "vision": {
                "models": [
                    {"source": "custom", "key": "a"},
                    {"source": "detected", "profile": "p", "model_id": "m"},
                ]
            }
        },
    ),
]


def build_vision_load() -> list[dict[str, object]]:
    cases = []
    with tempfile.TemporaryDirectory(prefix="oc-vision-load-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, config) in enumerate(VISION_LOAD_CASES):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            write_text(user_dir / "config.toml", tomli_w.dumps(config))
            with Patched({}, str(user_dir)):
                try:
                    value = V.load_vision_configuration()
                    record: dict[str, object] = {"expected": as_json(value)}
                except V.VisionConfigError as exc:
                    record = {"error": str(exc)}
            record.update({"name": name, "config": config})
            cases.append(record)
    return cases


NATIVE_VISION_CASES = [
    (None, None),
    (True, True),
    (False, False),
    (1, None),
    ("yes", None),
    ([], None),
]


def build_native_vision_parse() -> list[dict[str, object]]:
    return [
        {"value": value, "expected": V.parse_native_vision(value)} for value, _ in NATIVE_VISION_CASES
    ]


def build_native_vision_resolve() -> list[dict[str, object]]:
    cases = []
    with tempfile.TemporaryDirectory(prefix="oc-vision-resolve-") as tmp:
        root = Path(tmp).resolve()
        layout = {
            "models.toml": {
                "version": 1,
                "models": {
                    "a": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions", "native_vision": True},
                    "b": {"profile": "p", "model_id": "n", "protocol": "openai_chat_completions", "native_vision": False},
                },
            },
            "config.toml": {
                "llm": {
                    "profiles": {
                        "p": {"provider": "openai", "native_vision": True},
                        "q": {"provider": "openai", "native_vision": False},
                    }
                }
            },
        }
        inputs = [
            ("模型覆盖渠道", "a", "q"),
            ("渠道覆盖", "", "p"),
            ("渠道显式关闭", "", "q"),
            ("都未配置", "", ""),
            ("模型不存在回落渠道", "zzz", "p"),
            ("模型显式关闭", "b", "p"),
        ]
        for index, (name, catalog_key, profile_id) in enumerate(inputs):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            write_text(user_dir / "config.toml", tomli_w.dumps(layout["config.toml"]))
            write_text(user_dir / "models.toml", tomli_w.dumps(layout["models.toml"]))
            with Patched({}, str(user_dir)):
                setting = V.resolve_native_vision(catalog_key=catalog_key, profile_id=profile_id)
                expected = {
                    "value": setting.value,
                    "scope": setting.scope,
                    "label": setting.label,
                    "scope_text": setting.scope_text,
                }
            cases.append(
                {"name": name, "catalog_key": catalog_key, "profile_id": profile_id, "expected": expected}
            )
    return cases


def build_native_vision_save() -> list[dict[str, object]]:
    cases = []
    with tempfile.TemporaryDirectory(prefix="oc-vision-save-") as tmp:
        root = Path(tmp).resolve()
        inputs = [
            ("模型开", "model", True, "a", ""),
            ("模型清空", "model", None, "a", ""),
            ("模型不存在", "model", True, "zzz", ""),
            ("模型缺 key", "model", True, "", ""),
            ("渠道开", "channel", False, "", "p"),
            ("渠道清空", "channel", None, "", "p"),
            ("渠道不存在", "channel", True, "", "zzz"),
            ("范围非法", "other", True, "a", ""),
        ]
        for index, (name, scope, value, catalog_key, profile_id) in enumerate(inputs):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            write_text(
                user_dir / "config.toml",
                tomli_w.dumps({"llm": {"profiles": {"p": {"provider": "openai", "native_vision": True}}}}),
            )
            write_text(
                user_dir / "models.toml",
                tomli_w.dumps(
                    {
                        "version": 1,
                        "models": {
                            "a": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions"}
                        },
                    }
                ),
            )
            with Patched({}, str(user_dir)):
                try:
                    V.save_native_vision(
                        value, scope=scope, catalog_key=catalog_key, profile_id=profile_id
                    )
                    record: dict[str, object] = {
                        "config_text": (user_dir / "config.toml").read_text(encoding="utf-8"),
                        "models_text": (user_dir / "models.toml").read_text(encoding="utf-8"),
                    }
                except V.VisionConfigError as exc:
                    record = {"error": str(exc)}
            record.update(
                {
                    "name": name,
                    "scope": scope,
                    "value": value,
                    "catalog_key": catalog_key,
                    "profile_id": profile_id,
                }
            )
            cases.append(record)
    return cases


# ---------------------------------------------------------------- llm_multi


MULTI_CONFIG = {
    "llm": {
        "profiles": {
            "ch1": {
                "provider": "openai",
                "base_url": "https://api.ch1/v1",
                "api_key": "plain-key",
                "api_key_env": "CH1_KEY",
                "user_agent": "ua1",
                "default_protocol": "openai_responses",
                "default_context_window_tokens": 64000,
                "discovery": {"enabled": False},
                "provider_options": {"top_p": 0.8},
            },
            "ch2": {"provider": "anthropic", "enabled": False},
            "ch3": {"provider": "gemini"},
            "ch4": {"base_url": "https://api.ch4"},
        },
        "defaults": {"reasoning_effort": "medium", "context_window_tokens": 32000},
        "active_model": {"source": "detected", "profile": "ch1", "model_id": "gpt-5.2"},
    }
}

MULTI_MODELS = {
    "version": 1,
    "models": {
        "local-qwen": {
            "display_name": "本地 Qwen",
            "profile": "ch1",
            "model_id": "qwen3-local",
            "protocol": "openai_responses",
            "aliases": ["qwen"],
            "context_window_tokens": 32768,
            "max_output_tokens": 2048,
            "temperature": 0.5,
            "native_vision": True,
        },
        "broken": {
            "profile": "ch2",
            "model_id": "x",
            "protocol": "anthropic_messages",
        },
    },
}


MULTI_CASES = [
    ("active_model 检测", MULTI_CONFIG, MULTI_MODELS, {}),
    (
        "active_model 自定义",
        {
            "llm": {
                "profiles": MULTI_CONFIG["llm"]["profiles"],
                "active_model": {"source": "custom", "key": "local-qwen"},
            }
        },
        MULTI_MODELS,
        {},
    ),
    ("缺 profiles", {"llm": {"active_model": {"source": "detected", "profile": "p", "model_id": "m"}}}, {}, {}),
    (
        "Profile 不存在",
        {"llm": {"profiles": {"ch1": {"provider": "openai"}}, "active_model": {"source": "detected", "profile": "nope", "model_id": "m"}}},
        {},
        {},
    ),
    (
        "Profile 已禁用",
        {"llm": {"profiles": {"ch2": {"provider": "anthropic", "enabled": False}}, "active_model": {"source": "detected", "profile": "ch2", "model_id": "m"}}},
        {},
        {},
    ),
    (
        "协议与 Provider 不匹配",
        {"llm": {"profiles": {"ch1": {"provider": "openai"}}, "active_model": {"source": "detected", "profile": "ch1", "model_id": "m", "protocol": "anthropic_messages"}}},
        {},
        {},
    ),
    (
        "自定义 key 不存在",
        {"llm": {"profiles": {"ch1": {"provider": "openai"}}, "active_model": {"source": "custom", "key": "nope"}}},
        {},
        {},
    ),
    (
        "自定义缺 key",
        {"llm": {"profiles": {"ch1": {"provider": "openai"}}, "active_model": {"source": "custom"}}},
        {},
        {},
    ),
    (
        "检测缺 model_id",
        {"llm": {"profiles": {"ch1": {"provider": "openai"}}, "active_model": {"source": "detected", "profile": "ch1"}}},
        {},
        {},
    ),
    (
        "环境变量指定模型",
        {"llm": {"profiles": {"ch1": {"provider": "openai", "api_key": "k"}}, "active_model": {"source": "detected", "profile": "ch1", "model_id": "gpt-5.2"}}},
        {},
        {"OMNICRAWL_MODEL": "qwen"},
    ),
    (
        "环境变量 profile/model",
        {"llm": {"profiles": {"ch1": {"provider": "openai", "api_key": "k"}}, "active_model": {"source": "detected", "profile": "ch1", "model_id": "gpt-5.2"}}},
        {},
        {"OPENAI_MODEL": "ch1/gpt-4o"},
    ),
    (
        "环境变量覆盖 profile",
        {"llm": {"profiles": {"ch3": {"provider": "gemini", "api_key": "k"}}, "active_model": {"source": "detected", "profile": "ch3", "model_id": "gemini-3"}}},
        {},
        {"OMNICRAWL_PROFILE": "ch3"},
    ),
    ("active_model 非对象", {"llm": {"profiles": {"ch1": {"provider": "openai"}}, "active_model": 5}}, {}, {}),
    (
        "未知 Provider 默认协议",
        {"llm": {"profiles": {"ch9": {"provider": "bogus"}}, "active_model": {"source": "detected", "profile": "ch9", "model_id": "m"}}},
        {},
        {},
    ),
    (
        "默认窗口回退",
        {"llm": {"profiles": {"ch1": {"provider": "openai", "api_key": "k"}}, "defaults": {"context_window_tokens": 96000}, "active_model": {"source": "detected", "profile": "ch1", "model_id": "m"}}},
        {},
        {},
    ),
    (
        "自定义模型窗口不被 defaults 覆盖",
        {"llm": {"profiles": MULTI_CONFIG["llm"]["profiles"], "defaults": {"context_window_tokens": 96000}, "active_model": {"source": "custom", "key": "local-qwen"}}},
        MULTI_MODELS,
        {},
    ),
    (
        "渠道原生视觉",
        {"llm": {"profiles": {"ch1": {"provider": "openai", "api_key": "k", "native_vision": False}}, "active_model": {"source": "detected", "profile": "ch1", "model_id": "m"}}},
        {},
        {},
    ),
    (
        "超时与重试非法回落",
        {"llm": {"profiles": {"ch1": {"provider": "openai", "api_key": "k"}}, "defaults": {"request_timeout_seconds": 0, "request_retry_count": -3}, "active_model": {"source": "detected", "profile": "ch1", "model_id": "m"}}},
        {},
        {},
    ),
    (
        "超时与重试生效",
        {"llm": {"profiles": {"ch1": {"provider": "openai", "api_key": "k"}}, "defaults": {"request_timeout_seconds": 30, "request_retry_count": 2}, "active_model": {"source": "detected", "profile": "ch1", "model_id": "m"}}},
        {},
        {},
    ),
    (
        "缺 api_key 报错",
        {"llm": {"profiles": {"ch1": {"provider": "openai"}}, "active_model": {"source": "detected", "profile": "ch1", "model_id": "m"}}},
        {},
        {},
    ),
]


def build_multi_load() -> list[dict[str, object]]:
    cases = []
    with tempfile.TemporaryDirectory(prefix="oc-multi-load-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, config, models, env) in enumerate(MULTI_CASES):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            write_text(user_dir / "config.toml", tomli_w.dumps(config))
            if models:
                write_text(user_dir / "models.toml", tomli_w.dumps(models))
            with Patched(env, str(user_dir)):
                try:
                    record: dict[str, object] = {"expected": as_json(L.load_llm_config())}
                except (L.LLMError, S.ModelStoreError, V.VisionConfigError) as exc:
                    record = {"error": str(exc)}
            record.update({"name": name, "config": config, "models": models, "env": env})
            cases.append(record)
    return cases


SELECTION_CASES = [
    ("自定义 key", "local-qwen", MULTI_CONFIG, MULTI_MODELS, {}),
    ("自定义别名", "qwen", MULTI_CONFIG, MULTI_MODELS, {}),
    ("profile/model", "ch1/gpt-5.2", MULTI_CONFIG, MULTI_MODELS, {}),
    ("裸模型", "gpt-4o-mini", MULTI_CONFIG, MULTI_MODELS, {}),
    ("空选择", "   ", MULTI_CONFIG, MULTI_MODELS, {}),
    ("格式无效", "/m", MULTI_CONFIG, MULTI_MODELS, {}),
    ("profile 不存在", "nope/m", MULTI_CONFIG, MULTI_MODELS, {}),
    ("引用了禁用 profile", "broken", MULTI_CONFIG, MULTI_MODELS, {}),
]


def build_selection() -> list[dict[str, object]]:
    cases = []
    with tempfile.TemporaryDirectory(prefix="oc-multi-select-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, token, config, models, env) in enumerate(SELECTION_CASES):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            write_text(user_dir / "config.toml", tomli_w.dumps(config))
            write_text(user_dir / "models.toml", tomli_w.dumps(models))
            with Patched(env, str(user_dir)):
                try:
                    base = L.LLMConfig(
                        api_key="k",
                        base_url="https://api",
                        model="m",
                        provider="openai",
                        protocol="openai_chat_completions",
                        profile_id="ch1",
                        model_source="custom",
                    )
                    record: dict[str, object] = {"expected": as_json(M.apply_model_selection(base, token))}
                except (L.LLMError, S.ModelStoreError, V.VisionConfigError) as exc:
                    record = {"error": str(exc)}
            record.update({"name": name, "token": token, "config": config, "models": models})
            cases.append(record)
    return cases


def build_profiles() -> list[dict[str, object]]:
    cases = []
    section = MULTI_CONFIG["llm"]
    profiles = M.parse_profiles(section)
    cases.append(
        {
            "name": "解析 profiles",
            "section": section,
            "expected": {key: as_json(value) for key, value in profiles.items()},
        }
    )
    cases.append({"name": "无 profiles", "section": {"defaults": {}}, "expected": {}})
    cases.append({"name": "profiles 非对象", "section": {"profiles": 5}, "expected": {}})
    return cases


def build_config_to_profile() -> list[dict[str, object]]:
    config = L.LLMConfig(
        api_key="k",
        base_url="https://api",
        model="gpt-5.2",
        profile_id="ch1",
        provider="anthropic",
        protocol="anthropic_messages",
        catalog_key="local-qwen",
        model_source="custom",
        context_window_tokens=32000,
        max_output_tokens=2048,
        temperature=0.4,
        reasoning_effort="high",
    )
    profile, descriptor = M.llm_config_to_profile_and_descriptor(config)
    return [
        {
            "name": "视图转 profile 与 descriptor",
            "config": as_json(config),
            "profile": as_json(profile),
            "descriptor": {
                "model_id": descriptor.identity.model_id,
                "profile_id": descriptor.identity.profile_id,
                "provider": descriptor.identity.provider,
                "protocol": descriptor.identity.protocol,
                "catalog_key": descriptor.identity.catalog_key,
                "display_name": descriptor.display_name,
                "source": descriptor.source,
                "context_window_tokens": descriptor.context_window_tokens,
                "max_output_tokens": descriptor.max_output_tokens,
                "temperature": descriptor.temperature,
                "capabilities": descriptor.capabilities.to_dict(),
            },
        }
    ]


def main() -> None:
    fixture = {
        "reasoning_effort": build_reasoning_effort(),
        "llm_normalize": build_llm_normalize(),
        "thinking_enabled": build_thinking(),
        "active_refs": build_active_refs(),
        "model_store_parse": build_model_store_parse(),
        "model_store_save": build_model_store_save(),
        "resolve_alias": build_resolve_alias(),
        "llm_load": build_llm_load(),
        "llm_save": build_llm_save(),
        "vision_load": build_vision_load(),
        "native_vision_parse": build_native_vision_parse(),
        "native_vision_resolve": build_native_vision_resolve(),
        "native_vision_save": build_native_vision_save(),
        "multi_load": build_multi_load(),
        "selection": build_selection(),
        "profiles": build_profiles(),
        "config_to_profile": build_config_to_profile(),
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    with FIXTURE_PATH.open("w", encoding="utf-8", newline="\n") as handle:
        json.dump(fixture, handle, ensure_ascii=False, indent=1)
        handle.write("\n")
    print(
        "wrote %s: %s"
        % (FIXTURE_PATH, {key: len(value) for key, value in fixture.items()})
    )


if __name__ == "__main__":
    main()
