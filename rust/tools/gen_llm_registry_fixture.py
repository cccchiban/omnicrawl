#!/usr/bin/env python3
"""生成模型能力（`capabilities.py`）与协议解析（`registry.py`）的对照数据集。

期望值来自 Python 真实现：
- `capabilities_from_mapping` / `merge_capabilities` / `to_dict` 与四个 Adapter 的保守默认值；
- `ProviderProfile.resolve_protocol` 与 `protocol_for_provider`（含三条报错文案）。

用法：``python rust/tools/gen_llm_registry_fixture.py``
输出：``rust/crates/omnicrawl-llm/tests/fixtures/llm_registry_parity.json``
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-llm/tests/fixtures/llm_registry_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.llm import capabilities as C  # noqa: E402
from omnicrawl.llm import registry as R  # noqa: E402
from omnicrawl.llm.errors import ModelError  # noqa: E402

for module in (C, R):
    if not Path(module.__file__).resolve().is_relative_to(ROOT):
        raise SystemExit("加载到的不是仓库源码")

BOOL_FIELDS = (
    "streaming",
    "tools",
    "parallel_tool_calls",
    "reasoning",
    "vision",
    "model_discovery",
    "prompt_cache",
)
INT_FIELDS = ("context_window_tokens", "max_output_tokens")
CAPABILITY_FIELDS = BOOL_FIELDS + INT_FIELDS

MAPPING_CASES = [
    ("不是映射（None）", None),
    ("不是映射（列表）", [1, 2]),
    ("不是映射（字符串）", "streaming=true"),
    ("空映射", {}),
    ("显式全真", {name: True for name in BOOL_FIELDS}),
    ("显式全假", {name: False for name in BOOL_FIELDS}),
    ("部分声明", {"streaming": False, "reasoning": True}),
    ("布尔位给整数", {"streaming": 1, "tools": 0}),
    ("布尔位给字符串", {"streaming": "true"}),
    ("布尔位给 None", {"tools": None}),
    ("窗口值为正", {"context_window_tokens": 128000, "max_output_tokens": 4096}),
    ("窗口值为零", {"context_window_tokens": 0, "max_output_tokens": 0}),
    ("窗口值为负", {"context_window_tokens": -1, "max_output_tokens": -100}),
    ("窗口值为浮点", {"context_window_tokens": 128000.0}),
    ("窗口值为布尔", {"context_window_tokens": True}),
    ("窗口值为字符串", {"max_output_tokens": "4096"}),
    ("未知字段忽略", {"streaming": True, "unknown_field": 7}),
]

MERGE_CASES = [
    ("单层", [C.ModelCapabilities(streaming=False)]),
    ("空层被跳过", [None, C.ModelCapabilities(tools=True), None]),
    (
        "后者覆盖前者",
        [C.ModelCapabilities(tools=True, vision=True), C.ModelCapabilities(tools=False)],
    ),
    ("未声明不覆盖", [C.ModelCapabilities(reasoning=True), C.ModelCapabilities(vision=True)]),
    (
        "显式假可覆盖真",
        [C.ModelCapabilities(prompt_cache=True), C.ModelCapabilities(prompt_cache=False)],
    ),
    (
        "窗口值只被正数覆盖",
        [C.ModelCapabilities(context_window_tokens=100), C.ModelCapabilities()],
    ),
    (
        "保守默认 + 发现 + 用户配置",
        [
            C.conservative_anthropic_capabilities(),
            C.ModelCapabilities(vision=False, context_window_tokens=200000),
            C.ModelCapabilities(tools=True),
        ],
    ),
    ("全部为 None", [None, None]),
]

DEFAULT_CASES = [
    ("openai_chat", C.conservative_openai_chat_capabilities()),
    ("openai_responses", C.conservative_openai_responses_capabilities()),
    ("anthropic", C.conservative_anthropic_capabilities()),
    ("gemini", C.conservative_gemini_capabilities()),
]

PROTOCOL_CASES = [
    ("openai 默认", "openai", "", ""),
    ("anthropic 默认", "anthropic", "", ""),
    ("gemini 默认", "gemini", "", ""),
    ("显式指定协议", "openai", "", "openai_responses"),
    ("取 Profile 默认", "openai", "openai_responses", ""),
    ("显式覆盖 Profile 默认", "openai", "openai_responses", "openai_chat_completions"),
    ("纯空白指定退到 Provider 默认", "openai", "", "   "),
    ("纯空白指定忽略 Profile 默认", "openai", "anthropic_messages", " "),
    ("协议带空白会被裁剪", "openai", " openai_responses ", ""),
    ("anthropic 正常", "anthropic", "", "anthropic_messages"),
    ("gemini 正常", "gemini", "", "gemini_generate_content"),
    ("协议与 Provider 不匹配", "anthropic", "", "openai_chat_completions"),
    ("gemini 用了 anthropic 协议", "gemini", "", "anthropic_messages"),
    ("未知 Provider（空协议）", "unknown-provider", "", ""),
    ("未知 Provider（带协议）", "unknown-provider", "", "openai_chat_completions"),
    ("Provider 大小写敏感", "OpenAI", "", ""),
    ("不支持的协议", "openai", "", "made_up_protocol"),
    ("不支持的协议（来自默认层）", "openai", " made_up ", ""),
]

PROVIDER_DEFAULT_CASES = [
    ("openai 首选 responses", "openai", "openai_responses"),
    ("anthropic 首选自身协议", "anthropic", "anthropic_messages"),
    ("无首选", "gemini", ""),
    ("首选与 Provider 不匹配", "openai", "anthropic_messages"),
    ("未知 Provider", "nope", ""),
]


def raw(caps) -> dict:
    """把能力对象摊平成「未解析」的字段字典（None 表示未声明）。"""

    return {name: getattr(caps, name) for name in CAPABILITY_FIELDS}


def protocol_entry(label: str, provider: str, default_protocol: str, requested: str) -> dict:
    entry = {
        "label": label,
        "provider": provider,
        "default_protocol": default_protocol,
        "requested": requested,
    }
    profile = R.ProviderProfile(id="tmp", provider=provider, default_protocol=default_protocol)
    try:
        entry["protocol"] = profile.resolve_protocol(requested)
    except ModelError as exc:
        entry["error"] = exc.message
    return entry


def provider_default_entry(label: str, provider: str, preferred: str) -> dict:
    entry = {"label": label, "provider": provider, "preferred": preferred}
    try:
        entry["protocol"] = R.protocol_for_provider(provider, preferred)
    except ModelError as exc:
        entry["error"] = exc.message
    return entry


def main() -> None:
    mapping = []
    for label, raw_value in MAPPING_CASES:
        caps = C.capabilities_from_mapping(raw_value)
        mapping.append(
            {"label": label, "raw": raw_value, "parsed": raw(caps), "to_dict": caps.to_dict()}
        )

    merge = []
    for label, layers in MERGE_CASES:
        merge.append(
            {
                "label": label,
                "layers": [None if layer is None else raw(layer) for layer in layers],
                "merged": C.merge_capabilities(*layers).to_dict(),
            }
        )

    fixture = {
        "source": [
            "omnicrawl/llm/capabilities.py",
            "omnicrawl/llm/registry.py",
        ],
        "mapping": mapping,
        "merge": merge,
        "defaults": [{"label": label, "to_dict": caps.to_dict()} for label, caps in DEFAULT_CASES],
        "protocols": [
            protocol_entry(label, provider, default_protocol, requested)
            for label, provider, default_protocol, requested in PROTOCOL_CASES
        ],
        "provider_defaults": [
            provider_default_entry(label, provider, preferred)
            for label, provider, preferred in PROVIDER_DEFAULT_CASES
        ],
    }

    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + chr(10), encoding="utf-8"
    )
    print(
        "已写入 %s：映射 %d、合并 %d、默认值 %d、协议 %d、Provider 默认 %d"
        % (
            FIXTURE_PATH.relative_to(ROOT),
            len(mapping),
            len(merge),
            len(fixture["defaults"]),
            len(fixture["protocols"]),
            len(fixture["provider_defaults"]),
        )
    )


if __name__ == "__main__":
    main()
