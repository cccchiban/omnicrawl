#!/usr/bin/env python3
"""生成 `omnicrawl-host` 的 prompt cache 身份对照数据集。

期望值来自 Python 真实现：
  * `omnicrawl/agent/context/prompt_context.py` 的 `build_prompt_cache_identity`
  * `omnicrawl/llm/providers/openai_common.py` 的 `build_prompt_cache_key`

对照目标是**哈希的逐字节一致**：两侧对同一段稳定前缀必须派生出同一个
`prompt_cache_key`，否则同一前缀在 Provider 侧会落到不同缓存项上。因此数据集同时
记录七个身份字段与最终的 cache key——后者是整条链（七个字段 → 排序紧凑 JSON →
sha256 → 截断 32 位）的合成结果，一旦哪一环漂了它先响。

用例刻意覆盖三类易错点：
  * `no_skills` / `empty_tools`：空数组也要参与哈希（`sha256("[]")`），不能跳过；
  * `cjk_and_order`：`ensure_ascii=False`（中文原样输出，不转 \\uXXXX）与
    `sort_keys=True`（键序不影响结果）；
  * `stripped_instructions`：项目规范首尾空白变化不应改变身份。

工作区根与 Skill 路径刻意用 `PurePosixPath`：本脚本在 Windows 上跑时 `Path` 的
`str()` 会给出反斜杠，而 Rust 侧按原样比较字符串，用正斜杠才能让两侧输入严格一致。

用法（仓库根目录）：

    python rust/tools/gen_prompt_cache_fixture.py
    cd rust && cargo test -p omnicrawl-host --test prompt_cache_parity
"""

from __future__ import annotations

import json
import sys
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parents[2]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from omnicrawl.agent.context.prompt_context import (  # noqa: E402
    build_prompt_cache_identity,
)
from omnicrawl.extensions.skill import (  # noqa: E402
    Skill,
    SkillMatchResult,
    SkillMeta,
)
from omnicrawl.llm.providers.openai_common import (  # noqa: E402
    build_prompt_cache_key,
)

MODEL = "gpt-4o-mini"

OUTPUT = (
    ROOT
    / "rust"
    / "crates"
    / "omnicrawl-host"
    / "tests"
    / "fixtures"
    / "prompt_cache_parity.json"
)


def make_skill(raw: dict[str, object]) -> Skill:
    """按数据集里的一行构造真实 Skill（含正文，活动 Skill 的 body_hash 用它）。"""

    meta = SkillMeta(
        name=str(raw["name"]),
        description=str(raw["description"]),
        source_path=PurePosixPath(str(raw["source_path"])),
        base_dir=PurePosixPath(str(raw["base_dir"])),
        scope=str(raw["scope"]),
        disable_model_invocation=bool(raw["disable_model_invocation"]),
    )
    return Skill(meta=meta, body=str(raw["body"]))


# 工具声明故意让键序与「字典序」不一致，用来钉住 sort_keys 的行为。
TOOLS = [
    {
        "type": "function",
        "function": {
            "name": "read",
            "description": "读取文件",
            "parameters": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            },
        },
    },
    {
        "function": {
            "name": "bash",
            "description": "执行命令",
            "parameters": {"properties": {}, "type": "object", "required": []},
        },
        "type": "function",
    },
]

SKILLS = [
    {
        "name": "bili-note",
        "description": "提取 B 站视频为可读 Markdown",
        "scope": "user",
        "source_path": "/home/u/.omnicrawl/skills/bili-note/SKILL.md",
        "base_dir": "/home/u/.omnicrawl/skills/bili-note",
        "disable_model_invocation": False,
        "body": "# B 站笔记\n\n按字幕提取并整理。\n",
    },
    {
        "name": "ui-design",
        "description": "UI 设计准则",
        "scope": "project",
        "source_path": "/ws/demo/.omnicrawl/skills/ui-design/SKILL.md",
        "base_dir": "/ws/demo/.omnicrawl/skills/ui-design",
        "disable_model_invocation": True,
        "body": "# UI 设计\n\n层级、间距、对比。\n",
    },
]

CASES: list[dict[str, object]] = [
    {
        "name": "basic",
        "system_prompt": "你是 OmniCrawl。\n",
        "workspace_root": "/ws/demo",
        "project_instructions": "# 项目规范\n\n先读文档。",
        "skills": SKILLS,
        "active_skills": [0],
        "chat_tools": TOOLS,
    },
    {
        "name": "no_skills",
        "system_prompt": "你是 OmniCrawl。\n",
        "workspace_root": "/ws/demo",
        "project_instructions": "# 项目规范\n\n先读文档。",
        "skills": [],
        "active_skills": [],
        "chat_tools": TOOLS,
    },
    {
        "name": "empty_tools",
        "system_prompt": "你是 OmniCrawl。\n",
        "workspace_root": "/ws/demo",
        "project_instructions": "",
        "skills": SKILLS,
        "active_skills": [],
        "chat_tools": [],
    },
    {
        "name": "cjk_and_order",
        "system_prompt": "你是一个中文助手，回答要克制。\n",
        "workspace_root": "/ws/中文 工作区",
        "project_instructions": "任务不清先确认。",
        "skills": SKILLS,
        "active_skills": [1],
        "chat_tools": TOOLS,
    },
    {
        "name": "stripped_instructions",
        "system_prompt": "你是 OmniCrawl。\n",
        "workspace_root": "/ws/demo",
        # 首尾空白不该改变身份（Python 侧走 `.strip()`）。
        "project_instructions": "\n\n   # 项目规范\n\n先读文档。  \n\n",
        "skills": [],
        "active_skills": [],
        "chat_tools": TOOLS,
    },
]


def main() -> int:
    cases: list[dict[str, object]] = []
    for case in CASES:
        skills = [make_skill(raw) for raw in case["skills"]]
        active = [
            SkillMatchResult(skill=skills[index], score=0.9, reason="关键词命中")
            for index in case["active_skills"]
        ]
        identity = build_prompt_cache_identity(
            system_prompt=str(case["system_prompt"]),
            workspace_root=PurePosixPath(str(case["workspace_root"])),
            project_instructions=str(case["project_instructions"]),
            skill_manager=_ListOnlyManager(skills),
            active_skills=active,
            chat_tools=case["chat_tools"],
        )
        payload = identity.as_payload(model=MODEL)
        cases.append(
            {
                "name": case["name"],
                "inputs": {
                    "system_prompt": case["system_prompt"],
                    "workspace_root": case["workspace_root"],
                    "project_instructions": case["project_instructions"],
                    "skills": case["skills"],
                    "active_skills": case["active_skills"],
                    "chat_tools": case["chat_tools"],
                },
                "expected": {
                    "identity": payload,
                    "prompt_cache_key": build_prompt_cache_key(payload, model=MODEL),
                },
            }
        )

    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT.write_text(
        json.dumps(
            {"model": MODEL, "cases": cases},
            ensure_ascii=False,
            indent=2,
            sort_keys=True,
        )
        + "\n",
        encoding="utf-8",
    )
    print(f"已写入 {OUTPUT.relative_to(ROOT)}：{len(cases)} 个用例")
    return 0


class _ListOnlyManager:
    """只提供 `list_all()` 的最小替身，避免数据集依赖真实 Skill 目录。"""

    def __init__(self, skills: list[Skill]) -> None:
        self._skills = skills

    def list_all(self) -> list[SkillMeta]:
        return [skill.meta for skill in self._skills]


if __name__ == "__main__":
    raise SystemExit(main())
