#!/usr/bin/env python3
"""生成 openai_chat 请求构建对照数据集，供 Rust 侧 `omnicrawl-llm` 的 parity 测试使用。

期望值全部由 Python 真实现产出，不靠人读代码对齐：请求体用一个只记录参数的假客户端
在 `chat.completions.create` 处拦下，取的就是 Python 实际交给 SDK 的 kwargs；
另外三组（provider_options 校验、GPT 系列判定、prompt_cache_key）直接调用
``openai_common`` 的同名函数。

用法：``python rust/tools/gen_llm_request_fixture.py``
输出：``rust/crates/omnicrawl-llm/tests/fixtures/openai_chat_request_parity.json``
"""

from __future__ import annotations

import json
import sys
import threading
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = (
    ROOT / "rust/crates/omnicrawl-llm/tests/fixtures/openai_chat_request_parity.json"
)

# 必须加载仓库源码：已安装的 omnicrawl 在 site-packages，会对照到另一份实现。
sys.path.insert(0, str(ROOT))

import omnicrawl.llm.providers.openai_chat as C  # noqa: E402
import omnicrawl.llm.providers.openai_common as CO  # noqa: E402
from omnicrawl.llm.protocol import (  # noqa: E402
    ConversationMessage,
    GenerationOptions,
    ImageBlock,
    ModelTurnRequest,
    TextBlock,
    ToolCallBlock,
    ToolResultBlock,
    ToolSpec,
)

if not Path(C.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit(f"加载到的不是仓库源码：{C.__file__}")


class CaptureError(Exception):
    """建连前拦下请求：能拿到 kwargs 就够，后续错误映射不参与对照。"""


class CapturingCompletions:
    def __init__(self, owner: object) -> None:
        self._owner = owner

    def create(self, **kwargs: object) -> object:
        self._owner.kwargs = kwargs
        raise CaptureError("fixture capture")


class CapturingClient:
    """替身客户端：只记录 Python 交给 SDK 的请求参数。"""

    def __init__(self) -> None:
        self.kwargs: dict = {}
        self.chat = SimpleNamespace(completions=CapturingCompletions(self))


def text(value: str) -> dict:
    return {"Text": {"text": value}}


def image(
    media_type: str = "image/png", data_base64: str = "aGk=", detail: str = "auto"
) -> dict:
    return {
        "Image": {
            "media_type": media_type,
            "data_base64": data_base64,
            "detail": detail,
        }
    }


def tool_call(
    call_id: str = "call_1",
    name: str = "read_file",
    arguments: dict | None = None,
    provider_call_id: str = "",
) -> dict:
    return {
        "ToolCall": {
            "call_id": call_id,
            "name": name,
            "arguments": {} if arguments is None else arguments,
            "provider_call_id": provider_call_id,
        }
    }


def tool_result(
    call_id: str = "call_1", ok: bool = True, content: str = "done"
) -> dict:
    return {"ToolResult": {"call_id": call_id, "ok": ok, "content": content}}


def message(role: str, blocks: list | None = None, reasoning: str = "", tools: list | None = None) -> dict:
    return {
        "role": role,
        "blocks": list(blocks or []),
        "reasoning": reasoning,
        "tools": list(tools or []),
    }


READ_FILE_TOOL = {
    "name": "read_file",
    "description": "读取文件内容",
    "parameters": {
        "type": "object",
        "properties": {"path": {"type": "string"}},
        "required": ["path"],
    },
}

LIST_DIR_TOOL = {"name": "list_dir", "description": "列出目录", "parameters": {}}

IDENTITY = {
    "profile_id": "course-gateway",
    "provider": "openai",
    "protocol": "openai_chat_completions",
    "model_id": "gpt-5.2",
}


def python_blocks(raw_blocks: list) -> tuple:
    converted: list = []
    for item in raw_blocks:
        (kind, payload), = item.items()
        if kind == "Text":
            converted.append(TextBlock(text=payload["text"]))
        elif kind == "Image":
            converted.append(
                ImageBlock(
                    media_type=payload["media_type"],
                    data_base64=payload["data_base64"],
                    detail=payload.get("detail", "auto"),
                )
            )
        elif kind == "ToolCall":
            converted.append(
                ToolCallBlock(
                    call_id=payload["call_id"],
                    name=payload["name"],
                    arguments=payload["arguments"],
                    provider_call_id=payload.get("provider_call_id", ""),
                )
            )
        elif kind == "ToolResult":
            converted.append(
                ToolResultBlock(
                    call_id=payload["call_id"],
                    ok=payload["ok"],
                    content=payload["content"],
                )
            )
        else:
            raise SystemExit(f"未知消息块：{kind}")
    return tuple(converted)


def python_messages(raw_messages: list) -> tuple:
    return tuple(
        ConversationMessage(
            role=item["role"],
            blocks=python_blocks(item.get("blocks", [])),
            reasoning=item.get("reasoning", ""),
            tools=tuple(ToolSpec(**tool) for tool in item.get("tools", [])),
        )
        for item in raw_messages
    )


def case(
    name: str,
    *,
    model: str = "qwen3.6-plus",
    system_prompt: str = "你是助手。",
    messages: list | None = None,
    tools: list | None = None,
    options: dict | None = None,
    profile_request_timeout_seconds: float = 120.0,
    prompt_cache_capable: bool = False,
    prompt_cache_identity: dict | None = None,
) -> dict:
    return {
        "name": name,
        "input": {
            "model": model,
            "system_prompt": system_prompt,
            "messages": messages or [],
            "tools": tools or [],
            "options": options or {},
            "profile_request_timeout_seconds": profile_request_timeout_seconds,
            "prompt_cache_capable": prompt_cache_capable,
            "prompt_cache_identity": prompt_cache_identity or {},
        },
    }


REQUEST_CASES = [
    case(
        "text_only_default_options",
        messages=[message("user", [text("你好")])],
    ),
    case("blank_system_prompt", system_prompt="   ", messages=[message("user", [text("hi")])]),
    case(
        "assistant_tool_round_trip",
        messages=[
            message("user", [text("读一下 a.py")]),
            message(
                "assistant",
                [text("我来读。"), tool_call("call_1", "read_file", {"path": "a.py"})],
                reasoning="先看文件",
            ),
            message("tool", [tool_result("call_1", True, "print(1)")]),
        ],
        tools=[READ_FILE_TOOL],
    ),
    case(
        "assistant_tool_calls_without_reasoning",
        messages=[
            message(
                "assistant",
                [tool_call("call_2", "read_file", {"path": "b.py"})],
            )
        ],
        tools=[READ_FILE_TOOL],
    ),
    case(
        "assistant_text_with_reasoning",
        messages=[message("assistant", [text("结论")], reasoning="想过一遍")],
    ),
    case("assistant_empty_blocks", messages=[message("assistant")]),
    case(
        "user_image_and_text",
        messages=[message("user", [text("看这张图"), image("image/jpeg", "aGk=", "high")])],
    ),
    case("user_image_only", messages=[message("user", [image()])]),
    case("user_empty_text_block", messages=[message("user", [text("")])]),
    case("user_no_blocks", messages=[message("user")]),
    case("tool_message_with_text_block", messages=[message("tool", [text("被忽略")])]),
    case(
        "dynamic_tools_dedup",
        messages=[
            message("system", [text("按需加载")], tools=[READ_FILE_TOOL]),
            message("system", tools=[READ_FILE_TOOL, LIST_DIR_TOOL]),
            message("user", [text("继续")]),
        ],
    ),
    case(
        "dynamic_tools_all_duplicate",
        messages=[
            message("system", tools=[LIST_DIR_TOOL]),
            message("system", [text("整条被丢弃")], tools=[LIST_DIR_TOOL]),
            message("user", [text("继续")]),
        ],
    ),
    case(
        "dynamic_tools_drop_blocks",
        messages=[message("system", [text("不能带 content")], tools=[LIST_DIR_TOOL])],
    ),
    case("unknown_role_passthrough", messages=[message("developer", [text("hi")])]),
    case(
        "tool_call_id_fallback_to_provider_id",
        messages=[
            message(
                "assistant",
                [tool_call("", "read_file", {"path": "c.py"}, "provider_call_9")],
            )
        ],
    ),
    case(
        "tools_with_explicit_tool_choice",
        messages=[message("user", [text("列目录")])],
        tools=[READ_FILE_TOOL, LIST_DIR_TOOL],
        options={"tool_choice": "required"},
    ),
    case(
        "zero_max_tokens_and_zero_temperature",
        messages=[message("user", [text("hi")])],
        options={"max_output_tokens": 0, "temperature": 0.0},
    ),
    case(
        "max_tokens_and_temperature_set",
        messages=[message("user", [text("hi")])],
        options={"max_output_tokens": 4096, "temperature": 0.2},
    ),
    case(
        "timeout_falls_back_to_profile",
        messages=[message("user", [text("hi")])],
        options={"request_timeout_seconds": 0.0},
        profile_request_timeout_seconds=90.0,
    ),
    case(
        "timeout_from_options",
        messages=[message("user", [text("hi")])],
        options={"request_timeout_seconds": 45.5},
        profile_request_timeout_seconds=90.0,
    ),
    case(
        "reasoning_effort_enables_thinking",
        messages=[message("user", [text("hi")])],
        options={"reasoning_effort": "high"},
    ),
    case(
        "reasoning_effort_disabled",
        messages=[message("user", [text("hi")])],
        options={"reasoning_effort": "disabled"},
    ),
    case(
        "reasoning_effort_none",
        messages=[message("user", [text("hi")])],
        options={"reasoning_effort": "none"},
    ),
    case(
        "provider_options_preset_thinking",
        messages=[message("user", [text("hi")])],
        options={
            "reasoning_effort": "low",
            "provider_options": {
                "thinking": {"type": "enabled"},
                "top_p": 0.9,
                "seed": 7,
            },
        },
    ),
    case(
        "prompt_cache_gpt_capable",
        model="gpt-5.2",
        messages=[message("user", [text("hi")])],
        prompt_cache_capable=True,
        prompt_cache_identity=IDENTITY,
    ),
    case(
        "prompt_cache_gpt_without_capability",
        model="gpt-5.2",
        messages=[message("user", [text("hi")])],
        prompt_cache_capable=False,
        prompt_cache_identity=IDENTITY,
    ),
    case(
        "prompt_cache_non_gpt_with_capability",
        model="qwen3.6-plus",
        messages=[message("user", [text("hi")])],
        prompt_cache_capable=True,
        prompt_cache_identity=IDENTITY,
    ),
    case(
        "nested_and_unicode_arguments",
        messages=[
            message("user", [text("算一下")]),
            message(
                "assistant",
                [
                    tool_call(
                        "call_3",
                        "calc",
                        {
                            "路径": "中文/文件.py",
                            "n": 3,
                            "f": 1.5,
                            "ok": True,
                            "none": None,
                            "列表": [1, {"k": "v"}],
                        },
                    )
                ],
                reasoning="需要计算",
            ),
            message("tool", [tool_result("call_3", False, "")]),
        ],
    ),
    case(
        "forbidden_option_model",
        messages=[message("user", [text("hi")])],
        options={"provider_options": {"model": "gpt-5.2"}},
    ),
    case(
        "forbidden_option_timeout",
        messages=[message("user", [text("hi")])],
        options={"provider_options": {"timeout": 30}},
    ),
    case(
        "forbidden_option_tools",
        messages=[message("user", [text("hi")])],
        options={"provider_options": {"tools": []}},
    ),
    case(
        "unknown_option_key",
        messages=[message("user", [text("hi")])],
        options={"provider_options": {"verbosity": "high"}},
    ),
]

OPTIONS_CASES = [
    {},
    {"top_p": 0.9},
    {"thinking": {"type": "enabled"}},
    {"reasoning_effort": "high", "seed": 1, "service_tier": "flex"},
    {"model": "gpt-5.2"},
    {"stream": True},
    {"temperature": 0.5},
    {"base_url": "https://example.invalid"},
]

GPT_MODEL_CASES = [
    "gpt-5.2",
    "chatgpt-4o-latest",
    "o3-mini",
    "o1",
    "o4",
    "o",
    "gpt",
    "openai",
    "O3",
    " gpt-5.2",
    "gpt5",
    "qwen3.6-plus",
    "deepseek-v4-flash",
]

PROMPT_CACHE_KEY_CASES = [
    {"model": "gpt-5.2", "identity": IDENTITY},
    {"model": " GPT-5.2 ", "identity": IDENTITY},
    {"model": "chatgpt-4o", "identity": {"教师": "王", "profile_id": "p2"}},
    {"model": "o3-mini", "identity": {}},
    {"model": "qwen3.6-plus", "identity": IDENTITY},
]

# 工具调用参数串的书写形式：每层只放一个键，键序差异不参与，逐字节钉住分隔符与转义。
ARGUMENT_STRING_CASES = [
    {},
    {"path": "a.py"},
    {"n": 0},
    {"n": -3},
    {"n": 12345678901234567890},
    {"f": 0.1},
    {"f": -0.0},
    {"s": "中文/文件.py"},
    {"s": "行\n制表\t\"引号\"\\反斜杠"},
    {"s": ""},
    {"b": True},
    {"z": None},
    {"obj": {"k": "v"}},
    {"arr": []},
    {"arr": [1, 2]},
    {"arr": [{"k": "v"}]},
]

# 浮点写法：Python 的 repr 与 Rust 的 ryu 指数记法不同（`1e+20` vs `1e20`），
# 语义必须等价——Rust 侧只校验解析回 f64 后相等。
FLOAT_NOTATION_CASES = [1e20, 1e-7, 1e-5, 1e16, 1e17, 0.1]


def run_request_case(raw_case: dict) -> dict:
    case_input = raw_case["input"]
    client = CapturingClient()
    runtime = C.OpenAIChatCompletionsRuntime(
        identity=SimpleNamespace(model_id=case_input["model"]),
        capabilities=SimpleNamespace(
            streaming=True,
            tools=True,
            prompt_cache=case_input["prompt_cache_capable"],
        ),
        client=client,
        profile=SimpleNamespace(
            request_timeout_seconds=case_input["profile_request_timeout_seconds"]
        ),
        descriptor=SimpleNamespace(),
    )
    request = ModelTurnRequest(
        identity=runtime.identity,
        system_prompt=case_input["system_prompt"],
        messages=python_messages(case_input["messages"]),
        tools=tuple(ToolSpec(**tool) for tool in case_input["tools"]),
        generation_options=GenerationOptions(**(case_input.get("options") or {})),
        prompt_cache_identity=dict(case_input["prompt_cache_identity"]),
    )

    error = None
    try:
        # 生成器体在首次 next 时才执行，请求参数在此之前就已组装完毕。
        next(runtime._stream_turn_events(request, None, None, threading.Event()))
    except Exception as exc:  # noqa: BLE001 - 只对照错误文案，类型不参与
        error = getattr(exc, "message", None) or str(exc)

    kwargs = dict(client.kwargs)
    # create 未被调用说明请求在组装阶段就被拒（provider_options 校验），
    # 那条异常才是对照对象；其余情况异常只是拦下请求的哨兵。
    build_error = None if kwargs else error
    # timeout 是 SDK 参数、不进请求体：Rust 侧单独返回给传输层，故此处拆开记录。
    timeout = kwargs.pop("timeout", None)
    return {
        "name": raw_case["name"],
        "input": case_input,
        "expected": {"body": kwargs or None, "timeout_seconds": timeout},
        "error": build_error,
    }


def run_options_case(options: dict) -> dict:
    try:
        sanitized = CO.sanitize_provider_options(options)
    except Exception as exc:  # noqa: BLE001 - 同上
        return {
            "options": options,
            "ok": False,
            "sanitized": None,
            "message": getattr(exc, "message", None) or str(exc),
        }
    return {"options": options, "ok": True, "sanitized": sanitized, "message": None}


def run_prompt_cache_key_case(raw_case: dict) -> dict:
    return {
        "model": raw_case["model"],
        "identity": raw_case["identity"],
        "expected": CO.build_prompt_cache_key(raw_case["identity"], model=raw_case["model"]),
    }


def main() -> None:
    fixture = {
        "source": [
            "omnicrawl/llm/providers/openai_chat.py",
            "omnicrawl/llm/providers/openai_common.py",
        ],
        "requests": [run_request_case(item) for item in REQUEST_CASES],
        "provider_options": [run_options_case(item) for item in OPTIONS_CASES],
        "gpt_model": [
            {"model": model, "expected": CO.is_openai_gpt_model(model)}
            for model in GPT_MODEL_CASES
        ],
        "prompt_cache_key": [
            run_prompt_cache_key_case(item) for item in PROMPT_CACHE_KEY_CASES
        ],
        "argument_string": [
            {"arguments": item, "expected": json.dumps(item, ensure_ascii=False)}
            for item in ARGUMENT_STRING_CASES
        ],
        "float_notation": [
            {"value": item, "python": json.dumps(item)} for item in FLOAT_NOTATION_CASES
        ],
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(
        f"已写入 {FIXTURE_PATH.relative_to(ROOT)}："
        f"请求 {len(fixture['requests'])}、provider_options {len(fixture['provider_options'])}、"
        f"GPT 判定 {len(fixture['gpt_model'])}、prompt_cache_key {len(fixture['prompt_cache_key'])}、"
        f"参数串 {len(fixture['argument_string'])}、浮点写法 {len(fixture['float_notation'])}"
    )


if __name__ == "__main__":
    main()
