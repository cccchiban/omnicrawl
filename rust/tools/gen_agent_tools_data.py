#!/usr/bin/env python3
"""导出 Agent 工具目录数据：`omnicrawl/agent/toolkit/tools.py` 的真实现 → JSON 数据文件。

工具目录是**数据**（名称、说明、参数 Schema、是否需确认），注册规则是**逻辑**。这里只把
数据导出来，供 `omnicrawl-controllers` 的 `tool_catalog` 模块在 Rust 侧重放注册规则；
每个工具属于哪个可选 runner（即「runner 为空就省略」）由**实测**得出，不靠人读代码。

用法：``python rust/tools/gen_agent_tools_data.py``
输出：``rust/crates/omnicrawl-controllers/data/agent_tools.json``
"""

from __future__ import annotations

import json
import sys
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[2]
DATA_PATH = ROOT / "rust/crates/omnicrawl-controllers/data/agent_tools.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.agent.toolkit import tools as T  # noqa: E402

if not Path(T.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")


def runner(_arguments):
    return None


CORE_RUNNERS = (
    "list",
    "read",
    "grep",
    "edit_file",
    "write_file",
    "bash",
    "powershell",
    "monitor",
    "find",
    "read_image",
    "web_search",
    "fetcher",
    "image_gen",
    "tts",
    "git",
)
KB_RUNNERS = ("kb_search", "kb_read", "kb_write", "kb_append", "kb_list")
META_RUNNERS = ("update_todos", "ask_user", "pause_work", "evidence_recall", "advisor")
WINDOWS_RUNNERS = (
    "windows_window",
    "windows_control",
    "windows_input",
    "windows_clipboard",
    "windows_screenshot",
)
MEMORY_RUNNERS = ("memory_search", "memory_read", "memory_expand_related", "memory_write")

SUBAGENT_TYPES_PLACEHOLDER = ["__SUBAGENT_TYPES__"]


class FakeMcpManager:
    """只提供注册表三个映射，供 `build_mcp_tools` 遍历。"""

    def __init__(self, tools=(), resources=(), prompts=()):
        self.registry = SimpleNamespace(
            tools={item.logical_name: item for item in tools},
            resources={item.logical_uri: item for item in resources},
            prompts={item.logical_name: item for item in prompts},
        )


def meta(name, *, description="说明", schema="{}", requires_confirmation=False):
    return SimpleNamespace(
        logical_name=name,
        logical_uri=name,
        server_name="srv",
        description=description,
        argument_schema=schema,
        requires_confirmation=requires_confirmation,
    )


def build_group(key, runners, **extra):
    """按 runner 全给 / 全不给 / 缺某一个，推断每个工具的可选 runner 归属。"""

    full = getattr(T, key)(**{name: runner for name in runners}, **extra)
    empty_kwargs = {name: None for name in runners}
    empty_kwargs.update(extra)
    empty = getattr(T, key)(**empty_kwargs)
    empty_names = {tool.name for tool in empty}

    entries = []
    for tool in full:
        if tool.name in empty_names:
            optional = None
        else:
            optional = None
            for name in runners:
                kwargs = {item: runner for item in runners}
                kwargs[name] = None
                kwargs.update(extra)
                try:
                    probe = getattr(T, key)(**kwargs)
                except Exception:  # noqa: BLE001 - 该 runner 缺席时整组报错，视为组规则
                    optional = name
                    break
                if all(item.name != tool.name for item in probe):
                    optional = name
                    break
        entries.append(
            {
                "name": tool.name,
                "description": tool.description,
                "argument_schema": tool.argument_schema,
                "requires_confirmation": tool.requires_confirmation,
                "model_output_is_bounded": tool.model_output_is_bounded,
                "run_in_subprocess": tool.run_in_subprocess,
                "runner": optional,
            }
        )
    return entries


def main() -> None:
    data = {
        "source": "omnicrawl/agent/toolkit/tools.py",
        "groups": {
            "core": build_group("_core_tool_definitions", CORE_RUNNERS),
            "knowledge": build_group("_knowledge_tool_definitions", KB_RUNNERS),
            "meta": build_group("_meta_tool_definitions", META_RUNNERS),
            "windows": build_group("_windows_tool_definitions", WINDOWS_RUNNERS),
            "memory": build_group("_memory_tool_definitions", MEMORY_RUNNERS),
            "subagent": build_group(
                "_subagent_tool_definition",
                ("subagent",),
                subagent_types=SUBAGENT_TYPES_PLACEHOLDER,
            ),
        },
        "group_rules": {
            "knowledge_all_or_nothing": True,
            "windows_all_or_nothing": True,
            "subagent_requires_types": True,
            "memory_requires_flag": True,
            "errors": {},
        },
        "mcp": {
            "tool": {
                "name": "{logical_name}",
                "description": "{description}（MCP Server：{server_name}）",
                "argument_schema": "{argument_schema}",
                "requires_confirmation": "{requires_confirmation}",
            },
            "resource": {
                "name": "mcp_read_resource__{logical_uri}",
                "description": "读取 MCP Resource：{logical_uri}（MCP Server：{server_name}）",
                "argument_schema": "{}",
                "requires_confirmation": False,
            },
            "prompt": {
                "name": "mcp_get_prompt__{logical_name}",
                "description": "获取 MCP Prompt：{logical_name}（MCP Server：{server_name}）",
                "argument_schema": '{"arguments": {}}',
                "requires_confirmation": False,
            },
        },
        "disabled_rule": "disabled_tools 中的名称不出现在结果表里（含 MCP 动态工具）。",
    }

    # MCP 三类名称/说明模板用真实现核对一次，避免模板写错
    probe = FakeMcpManager(
        tools=[meta("srv.tool", description="工具说明", schema='{"a": 1}', requires_confirmation=True)],
        resources=[meta("file://x")],
        prompts=[meta("srv.prompt")],
    )
    built = T.build_mcp_tools(
        mcp_manager=probe,
        mcp_call=lambda tool_meta, arguments: None,
        mcp_read_resource=lambda logical_uri: None,
        mcp_get_prompt=lambda logical_name, arguments: None,
    )
    data["mcp_samples"] = [
        {
            "name": tool.name,
            "description": tool.description,
            "argument_schema": tool.argument_schema,
            "requires_confirmation": tool.requires_confirmation,
        }
        for tool in built
    ]

    # 组规则错误文案：直接触发真实现的报错，避免手抄
    def error_message(callable_, **kwargs):
        try:
            callable_(**kwargs)
        except ValueError as exc:
            return str(exc)
        return ""

    # 知识库的整组规则在 build_agent_tools 里（`_knowledge_tool_definitions` 本身不检查）
    required_kwargs = {
        name: runner
        for name in ("list", "read", "grep", "edit_file", "write_file", "bash", "powershell", "monitor")
    }
    required_kwargs.update(
        {
            "mcp_manager": FakeMcpManager(),
            "memory_enabled": False,
            "mcp_call": lambda tool_meta, arguments: None,
            "mcp_read_resource": lambda logical_uri: None,
            "mcp_get_prompt": lambda logical_name, arguments: None,
            "memory_search": runner,
            "memory_read": runner,
            "memory_expand_related": runner,
            "memory_write": runner,
        }
    )
    partial_kb = dict(required_kwargs)
    partial_kb.update({name: (runner if name == "kb_search" else None) for name in KB_RUNNERS})
    partial_windows = {
        name: (runner if name == "windows_window" else None) for name in WINDOWS_RUNNERS
    }
    data["group_rules"]["errors"] = {
        "knowledge_partial": error_message(T.build_agent_tools, **partial_kb),
        "windows_partial": error_message(T._windows_tool_definitions, **partial_windows),
        "subagent_without_types": error_message(
            T._subagent_tool_definition, subagent=runner, subagent_types=()
        ),
    }

    DATA_PATH.parent.mkdir(parents=True, exist_ok=True)
    DATA_PATH.write_text(
        json.dumps(data, ensure_ascii=False, indent=2) + chr(10), encoding="utf-8"
    )
    counts = ", ".join(
        f"{group}={len(items)}" for group, items in data["groups"].items()
    )
    print(f"已写入 {DATA_PATH.relative_to(ROOT)}：{counts}；MCP 样本 {len(built)} 条")


if __name__ == "__main__":
    main()
