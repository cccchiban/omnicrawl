#!/usr/bin/env python3
"""生成 `omnicrawl/agent/subagents/` 的对照数据集。

期望值全部来自 Python 真实现：能直接调的函数直接调，需要文件系统的用例在临时目录里
搭好布局后驱动真类。所有路径统一归一成 `<ROOT>`（临时根）与 `<REPO>`（仓库根）占位，
两侧各自还原再比对。

用法：``python rust/tools/gen_subagents_fixture.py``
输出：``rust/crates/omnicrawl-controllers/tests/fixtures/subagents_parity.json``
"""

from __future__ import annotations

import json
import shutil
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = (
    ROOT / "rust/crates/omnicrawl-controllers/tests/fixtures/subagents_parity.json"
)

sys.path.insert(0, str(ROOT))

from omnicrawl.agent.subagents.definitions import (  # noqa: E402
    AgentDefinitionRegistry,
    parse_agent_definition,
)

_module_file = Path(sys.modules[parse_agent_definition.__module__].__file__).resolve()
if not _module_file.is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")

TEMPLATES_DIR = ROOT / "rust/assets/templates/subagents"


def outcome(callable_, *args, **kwargs):
    """记录一次调用的返回值或错误文案。"""

    try:
        return {"ok": True, "value": callable_(*args, **kwargs), "error": None}
    except Exception as exc:  # noqa: BLE001 - 对照数据集要原样记录失败文案
        return {"ok": False, "value": None, "error": str(exc)}


class Normalizer:
    """把临时根与仓库根换成占位符，分隔符统一成正斜杠。

    `tempfile` 给的可能是 8.3 短名，而 `Path.resolve()` 会展开成长名；两种形式都要替换，
    否则诊断里的 resolved 路径会漏出真实临时目录。
    """

    def __init__(self, temporary_root: Path):
        self.roots = self._prefixes(temporary_root)
        self.repos = self._prefixes(ROOT)

    @staticmethod
    def _prefixes(path: Path) -> list[str]:
        variants = {str(path), str(Path(path).resolve())}
        normalized = {item.replace("\\", "/") for item in variants}
        return sorted(normalized, key=len, reverse=True)

    def text(self, value) -> str:
        if value is None:
            return ""
        text = str(value).replace("\\", "/")
        for prefix in self.roots:
            text = text.replace(prefix, "<ROOT>")
        for prefix in self.repos:
            text = text.replace(prefix, "<REPO>")
        return text

    def view(self, definition) -> dict:
        return {
            "name": definition.name,
            "description": definition.description,
            "system_prompt": definition.system_prompt,
            "tools": list(definition.tools),
            "disallowed_tools": list(definition.disallowed_tools),
            "model": definition.model,
            "permission_mode": definition.permission_mode,
            "background": definition.background,
            "isolation": definition.isolation,
            "skills": list(definition.skills),
            "mcp_servers": list(definition.mcp_servers),
            "git_mode": definition.git_mode,
            "source": definition.source,
            "source_path": self.text(definition.source_path)
            if definition.source_path is not None
            else None,
        }


def document(frontmatter: str, body: str = "正文。\n") -> str:
    return "---\n%s\n---\n%s" % (frontmatter, body)


PARSE_CASES = [
    ("最小定义", "minimal.md", document("name: probe\ndescription: 一个探针"), 0),
    (
        "完整字段",
        "full.md",
        document(
            "\n".join(
                [
                    "name: full-agent",
                    "description: 完整定义",
                    "tools:",
                    "  - read",
                    "  - grep",
                    "disallowedTools:",
                    "  - write_file",
                    "model: deepseek-v4",
                    "permissionMode: standard",
                    "background: true",
                    "isolation: worktree",
                    "skills:",
                    "  - skill-one",
                    "mcpServers:",
                    "  - server-one",
                    "gitMode: full",
                ]
            )
        ),
        0,
    ),
    (
        "折叠标量描述",
        "folded.md",
        document(
            "name: folded\ndescription: >\n  第一行描述。\n  第二行描述。",
        ),
        0,
    ),
    (
        "字面标量描述",
        "literal.md",
        document("name: literal\ndescription: |\n  第一行\n  第二行"),
        0,
    ),
    (
        "引号与转义",
        "quoted.md",
        document('name: "quoted"\ndescription: \'带 空格\'\nmodel: "m: 1"'),
        0,
    ),
    ("BOM 前缀", "bom.md", "\ufeff" + document("name: bom\ndescription: 带 BOM"), 0),
    (
        "正文首尾空白",
        "trim.md",
        document("name: trim\ndescription: 去空白", "\n\n  正文内容  \n\n"),
        0,
    ),
    ("大写 name 归一", "upper.md", document("name: UpperCase\ndescription: 归一"), 0),
    ("兼容字段被忽略", "legacy.md", document("name: legacy\ndescription: 旧字段\nmaxTurns: 3\nmaxToolCalls: 5"), 0),
    ("空数组字段", "empty-lists.md", document("name: empties\ndescription: 空数组\nskills: []\nmcpServers: []"), 0),
    ("tools 空白项被过滤", "blank-items.md", document('name: blanks\ndescription: 过滤空项\ntools:\n  - "  "\n  - read'), 0),
    ("缺少 frontmatter", "no-frontmatter.md", "name: probe\ndescription: 裸正文\n", 0),
    ("frontmatter 未闭合", "unclosed.md", "---\nname: probe\ndescription: 未闭合\n", 0),
    ("空 frontmatter", "empty.md", "---\n---\n正文。", 0),
    ("frontmatter 是列表", "list.md", "---\n- a\n- b\n---\n正文。", 0),
    ("未知字段", "unknown-field.md", document("name: probe\ndescription: 有未知字段\nbogus: 1"), 0),
    ("name 下划线", "bad-name-underscore.md", document("name: bad_name\ndescription: 非法"), 0),
    ("name 前导连字符", "bad-name-lead.md", document("name: -lead\ndescription: 非法"), 0),
    ("name 尾随连字符", "bad-name-trail.md", document("name: trail-\ndescription: 非法"), 0),
    ("name 连续连字符", "bad-name-double.md", document("name: double--dash\ndescription: 非法"), 0),
    ("name 非 ASCII", "bad-name-cjk.md", document("name: 中文名\ndescription: 非法"), 0),
    ("name 过长", "bad-name-long.md", document("name: %s\ndescription: 过长" % ("a" * 65)), 0),
    ("description 缺失", "no-description.md", document("name: probe"), 0),
    ("description 空串", "blank-description.md", document('name: probe\ndescription: ""'), 0),
    ("description 过长", "long-description.md", document("name: probe\ndescription: %s" % ("字" * 301)), 0),
    ("tools 非数组", "tools-not-list.md", document("name: probe\ndescription: 非数组\ntools: read"), 0),
    ("tools 含非字符串", "tools-not-string.md", document("name: probe\ndescription: 非字符串\ntools:\n  - 1"), 0),
    ("tools 超上限", "tools-too-many.md", document("name: probe\ndescription: 超上限\ntools:\n%s" % "\n".join("  - t%d" % index for index in range(65))), 0),
    ("tools 单项过长", "tools-item-long.md", document("name: probe\ndescription: 单项过长\ntools:\n  - %s" % ("x" * 129)), 0),
    ("tools 重复项", "tools-duplicate.md", document("name: probe\ndescription: 重复\ntools:\n  - read\n  - read"), 0),
    ("permissionMode 非法", "bad-mode.md", document("name: probe\ndescription: 非法\npermissionMode: root"), 0),
    ("background 非布尔", "bad-background.md", document("name: probe\ndescription: 非布尔\nbackground: 1"), 0),
    ("isolation 非法", "bad-isolation.md", document("name: probe\ndescription: 非法\nisolation: container"), 0),
    ("gitMode 非法", "bad-gitmode.md", document("name: probe\ndescription: 非法\ngitMode: write"), 0),
    ("YAML 语法错误", "yaml-error.md", "---\nname: probe\ndescription: [1\n---\n正文。", 0),
    ("正文恰好上限", "body-limit.md", document("name: probe\ndescription: 上限", "x" * 64000), 0),
    ("正文超上限", "body-over.md", document("name: probe\ndescription: 超限", "x" * 64001), 0),
    ("文件超上限", "file-over.md", document("name: probe\ndescription: 超限", "") + "x" * (256 * 1024), 0),
]


def parse_cases(normalizer: Normalizer, case_dir: Path) -> list[dict]:
    cases = []
    for label, filename, content, pad in PARSE_CASES:
        target = case_dir / filename
        payload = content + "x" * pad
        target.write_text(payload, encoding="utf-8")
        observed = outcome(parse_agent_definition, target, source="project")
        entry = {
            "label": label,
            "file": filename,
            "content": content,
            "content_pad": pad,
            "ok": observed["ok"],
        }
        if observed["ok"]:
            entry["definition"] = normalizer.view(observed["value"])
            entry["error"] = None
            entry["error_prefix"] = None
        else:
            text = normalizer.text(observed["error"])
            prefix_only = text.startswith("Agent 定义 YAML 解析失败：")
            entry["definition"] = None
            entry["error"] = None if prefix_only else text
            entry["error_prefix"] = "Agent 定义 YAML 解析失败：" if prefix_only else None
        cases.append(entry)
    return cases


def template_cases(normalizer: Normalizer) -> list[dict]:
    cases = []
    for path in sorted(TEMPLATES_DIR.glob("*.md")):
        observed = outcome(parse_agent_definition, path, source="builtin")
        entry = {
            "label": path.name,
            "file": "rust/assets/templates/subagents/%s" % path.name,
            "ok": observed["ok"],
        }
        if observed["ok"]:
            entry["definition"] = normalizer.view(observed["value"])
            entry["error"] = None
        else:
            entry["definition"] = None
            entry["error"] = normalizer.text(observed["error"])
        cases.append(entry)
    return cases


DISCOVER_FILES = {
    "ws/.omnicrawl/agents/project-shared.md": document("name: shared\ndescription: 项目覆盖"),
    "ws/.omnicrawl/agents/project-only.md": document("name: project-only\ndescription: 项目独有"),
    "ws/.omnicrawl/agents/broken.md": document("name: bad_name\ndescription: 非法"),
    "ws/.omnicrawl/agents/notes.txt": "不是定义",
    "ws/.omnicrawl/agents/nested/deep.md": document("name: nested\ndescription: 子目录"),
    "ws/.agents/agents/compat.md": document("name: compat\ndescription: 兼容目录"),
    "home/.OmniCrawl/agents/user-only.md": document("name: user-only\ndescription: 用户级"),
    "home/.OmniCrawl/agents/dup.md": document("name: dup\ndescription: 用户级同名"),
    "builtin/dup.md": document("name: dup\ndescription: 内置同名"),
    "builtin/shared.md": document("name: shared\ndescription: 内置被覆盖"),
    "builtin/builtin-only.md": document("name: builtin-only\ndescription: 仅内置"),
    "plugin/plug.md": document("name: plug\ndescription: 插件定义"),
}


def discover_cases(normalizer: Normalizer, root: Path) -> dict:
    for relative, content in DISCOVER_FILES.items():
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")

    plugins = [
        ("alpha", "plugin/plug.md"),
        ("beta", "home/.OmniCrawl/agents/user-only.md"),
    ]
    registry = AgentDefinitionRegistry(
        builtin_directory=root / "builtin",
        home_directory=root / "home",
    )
    registry.discover(
        root / "ws",
        plugin_definitions=[(name, root / relative) for name, relative in plugins],
    )
    return {
        "files": dict(DISCOVER_FILES),
        "plugins": [{"name": name, "path": relative} for name, relative in plugins],
        "definitions": [
            {
                "name": item.name,
                "source": item.source,
                "source_path": normalizer.text(item.source_path),
            }
            for item in registry.list_all()
        ],
        "diagnostics": [
            {
                "kind": item.kind,
                "message": normalizer.text(item.message),
                "path": normalizer.text(item.path),
                "winner_path": normalizer.text(item.winner_path),
                "loser_path": normalizer.text(item.loser_path),
            }
            for item in registry.diagnostics
        ],
        "lookup": [
            {"key": key, "name": getattr(registry.get(key), "name", None)}
            for key in ["shared", " SHARED ", "dup", "plug", "missing", ""]
        ],
    }


def execution_cases() -> dict:
    from omnicrawl.agent.subagents import execution as execution_module

    context = execution_module.SubAgentExecutionContext()
    return {
        "fork_boilerplate": execution_module.FORK_BOILERPLATE,
        "context_defaults": {
            "context": context.context,
            "model_snapshot": context.model_snapshot,
            "fork_messages": list(context.fork_messages),
            "parent_system_prompt": context.parent_system_prompt,
            "skill_context": context.skill_context,
            "worktree_session": context.worktree_session,
            "workspace_root": context.workspace_root,
            "isolation": context.isolation,
            "task_id": context.task_id,
            "batch_id": context.batch_id,
        },
    }


VERIFY_PARSE_CASES = [
    ("默认超时", {"check": "unit_tests"}, 120),
    ("显式超时", {"check": "compileall", "timeout_seconds": 5}, 120),
    ("等于上限", {"check": "git_diff_check", "timeout_seconds": 120}, 120),
    ("低于下限", {"check": "unit_tests", "timeout_seconds": 0}, 120),
    ("高于上限", {"check": "unit_tests", "timeout_seconds": 121}, 120),
    ("布尔超时", {"check": "unit_tests", "timeout_seconds": True}, 120),
    ("浮点超时", {"check": "unit_tests", "timeout_seconds": 5.5}, 120),
    ("字符串超时", {"check": "unit_tests", "timeout_seconds": "5"}, 120),
    ("空超时", {"check": "unit_tests", "timeout_seconds": None}, 120),
    ("未知检查", {"check": "nope"}, 120),
    ("检查非字符串", {"check": 7}, 120),
    ("缺少检查", {"timeout_seconds": 5}, 120),
    ("额外参数", {"check": "unit_tests", "bogus": 1}, 120),
    ("多个额外参数", {"check": "unit_tests", "z": 1, "a": 2}, 120),
    ("参数非对象", "not-a-dict", 120),
]


def verify_cases() -> dict:
    import sys

    from omnicrawl.agent.subagents import verify as verify_module

    tool = verify_module.build_verify_command_tool(object(), max_timeout_seconds=120)
    checks = [
        {
            "identifier": item.identifier,
            "label": item.label,
            "argv": [
                "<PYTHON>" if value == sys.executable else value for value in item.argv
            ],
        }
        for item in verify_module.VERIFY_CHECKS.values()
    ]

    parse_cases = []
    for label, arguments, max_timeout in VERIFY_PARSE_CASES:
        observed = outcome(
            verify_module._parse_verify_arguments,
            arguments,
            max_timeout_seconds=max_timeout,
        )
        entry = {
            "label": label,
            "arguments": arguments,
            "max_timeout_seconds": max_timeout,
            "ok": observed["ok"],
        }
        if observed["ok"]:
            check, timeout = observed["value"]
            entry["check"] = check.identifier
            entry["timeout_seconds"] = timeout
            entry["error"] = None
        else:
            entry["check"] = None
            entry["timeout_seconds"] = None
            entry["error"] = observed["error"]
        parse_cases.append(entry)

    return {
        "tool_name": verify_module.VERIFY_COMMAND_TOOL_NAME,
        "description": tool.description,
        "argument_schema": tool.argument_schema,
        "requires_confirmation": tool.requires_confirmation,
        "checks": checks,
        "parse": parse_cases,
    }


class RecoveryEventProbe:
    """会话事件替身：真实现只按属性名读 type / payload / created_at。"""

    def __init__(self, event_type, payload, created_at=None):
        self.type = event_type
        self.payload = payload
        self.created_at = created_at


def _recovery_event(case) -> dict:
    return {
        "type": case.type,
        "payload": case.payload,
        "created_at_seconds": case.created_at.timestamp()
        if case.created_at is not None
        else None,
    }


def recovery_cases() -> dict:
    from datetime import datetime, timezone

    from omnicrawl.agent.subagents import recovery as recovery_module

    def stamp(seconds: float):
        return datetime.fromtimestamp(seconds, tz=timezone.utc)

    def started(task_id, created_at=None, **extra):
        payload = {"task_id": task_id}
        payload.update(extra)
        return RecoveryEventProbe("subagent_task_started", payload, created_at=created_at)

    def completed(task_id, created_at=None, **extra):
        payload = {"task_id": task_id, "status": "completed"}
        payload.update(extra)
        return RecoveryEventProbe("subagent_task_completed", payload, created_at=created_at)

    long_description = "描" * 130
    long_agent_type = "a" * 90
    scenarios = [
        ("空事件", [], "owner-1", "session-1"),
        (
            "排队任务被中断",
            [
                RecoveryEventProbe(
                    "subagent_task_queued",
                    {"task_id": "task-aaaaaaaaaaaa", "description": "排队中"},
                )
            ],
            "owner-1",
            "session-1",
        ),
        (
            "完成态带结果",
            [
                completed(
                    "task-bbbbbbbbbbbb",
                    description="完成的",
                    agent_type="review",
                    batch_id="batch-cccccccccccc",
                    result={"summary": "结果摘要", "artifacts": [{"type": "x"}], "usage": {"a": 1}},
                )
            ],
            "owner-1",
            "session-1",
        ),
        (
            "失败态带错误",
            [
                RecoveryEventProbe(
                    "subagent_task_failed",
                    {
                        "task_id": "task-dddddddddddd",
                        "error": {"code": "MY_CODE", "message": "炸了"},
                    },
                )
            ],
            "owner-1",
            "session-1",
        ),
        (
            "取消态无错误",
            [RecoveryEventProbe("subagent_task_cancelled", {"task_id": "task-eeeeeeeeeeee"})],
            "owner-1",
            "session-1",
        ),
        (
            "非法任务号被忽略",
            [completed("task-XYZ"), completed("task-0123456789ab")],
            "owner-1",
            "session-1",
        ),
        (
            "任务号带空白",
            [completed("  task-ffffffffffff  ")],
            "owner-1",
            "session-1",
        ),
        (
            "非生命周期事件被忽略",
            [RecoveryEventProbe("turn_started", {"task_id": "task-111111111111"})],
            "owner-1",
            "session-1",
        ),
        (
            "payload 非对象被忽略",
            [RecoveryEventProbe("subagent_task_completed", "not-a-mapping")],
            "owner-1",
            "session-1",
        ),
        (
            "状态回退到活状态",
            [
                completed("task-222222222222", created_at=stamp(10.0)),
                started("task-222222222222", created_at=stamp(20.0)),
            ],
            "owner-1",
            "session-1",
        ),
        (
            "状态归一为 waiting_approval",
            [
                RecoveryEventProbe(
                    "subagent_task_queued",
                    {"task_id": "task-333333333333", "status": " Waiting_Approval "},
                    created_at=stamp(30.0),
                )
            ],
            "owner-1",
            "session-1",
        ),
        (
            "非法状态回落事件类型",
            [
                RecoveryEventProbe(
                    "subagent_task_partial",
                    {"task_id": "task-444444444444", "status": "bogus"},
                    created_at=stamp(40.0),
                )
            ],
            "owner-1",
            "session-1",
        ),
        (
            "批次号从任务号派生",
            [completed("task-555555555555", batch_id="bad")],
            "owner-1",
            "session-1",
        ),
        (
            "描述与类型超长截断",
            [
                completed(
                    "task-666666666666",
                    description=long_description,
                    agent_type=long_agent_type,
                )
            ],
            "owner-1",
            "session-1",
        ),
        (
            "错误只带编号",
            [
                RecoveryEventProbe(
                    "subagent_task_failed",
                    {"task_id": "task-777777777777", "error": {"code": "ONLY_CODE"}},
                )
            ],
            "owner-1",
            "session-1",
        ),
        (
            "错误只带消息",
            [
                RecoveryEventProbe(
                    "subagent_task_failed",
                    {"task_id": "task-888888888888", "error": {"message": "只有消息"}},
                )
            ],
            "owner-1",
            "session-1",
        ),
        (
            "完成态无结果被补齐",
            [RecoveryEventProbe("subagent_task_completed", {"task_id": "task-999999999999"})],
            "owner-1",
            "session-1",
        ),
        (
            "顶层摘要与产物",
            [
                completed(
                    "task-aaaaaaaaaaab",
                    summary="顶层摘要",
                    artifacts=[{"index": index} for index in range(20)],
                    usage={"tokens": 3},
                )
            ],
            "owner-1",
            "session-1",
        ),
        (
            "结果字段兜底",
            [
                completed(
                    "task-aaaaaaaaaaac",
                    artifacts="bad",
                    usage="bad",
                    result={"artifacts": [{"from": "result"}], "usage": {"from": "result"}},
                )
            ],
            "owner-1",
            "session-1",
        ),
        (
            "payload 时间戳兜底",
            [
                RecoveryEventProbe(
                    "subagent_task_completed",
                    {"task_id": "task-aaaaaaaaaaad", "timestamp": 1700000000.25},
                )
            ],
            "owner-1",
            "session-1",
        ),
        (
            "排序按创建时间",
            [
                completed("task-bbbbbbbbbbbb", created_at=stamp(100.0)),
                started("task-aaaaaaaaaaae", created_at=stamp(50.0)),
            ],
            "owner-2",
            "session-2",
        ),
    ]

    cases = []
    for label, events, owner_id, session_id in scenarios:
        snapshots = recovery_module.rebuild_task_snapshots_from_session_events(
            events,
            owner_id=owner_id,
            session_id=session_id,
        )
        cases.append(
            {
                "label": label,
                "owner_id": owner_id,
                "session_id": session_id,
                "events": [_recovery_event(event) for event in events],
                "snapshots": snapshots,
            }
        )
    return {"cases": cases}


TIME_KEYS = {"created_at", "updated_at", "timestamp"}


def scrub_times(value):
    """把不稳定的时间戳换成占位符，其余结构原样保留。"""

    if isinstance(value, dict):
        return {
            key: (
                "<TIME>"
                if key in TIME_KEYS and isinstance(item, (int, float))
                else scrub_times(item)
            )
            for key, item in value.items()
        }
    if isinstance(value, list):
        return [scrub_times(item) for item in value]
    return value


def tasks_cases() -> dict:
    import time as time_module

    from omnicrawl.agent.subagents import tasks as tasks_module

    manager_class = tasks_module.SubAgentTaskManager
    spec_class = tasks_module.SubAgentTaskSpec

    def bound_view(value):
        return None if value is None else dict(value)

    bound_result_inputs = [
        ("空值", None),
        ("空对象", {}),
        (
            "全字段",
            {
                "status": "completed",
                "summary": "摘要",
                "artifacts": [{"a": 1}],
                "usage": {"tokens": 1},
                "error": None,
                "task_id": "t",
                "description": "d",
                "agent_type": "a",
                "definition_source": "builtin",
                "recovered": "yes",
                "hidden": 1,
            },
        ),
        ("摘要超长", {"summary": "x" * 6100}),
        ("产物超量", {"artifacts": [{"index": index} for index in range(20)]}),
        ("产物非数组", {"artifacts": "bad"}),
        ("回收标记假值", {"recovered": ""}),
        ("摘要非字符串", {"summary": 5}),
    ]
    bound_result_cases = [
        {
            "label": label,
            "input": value,
            "value": bound_view(manager_class._bound_result(value)),
        }
        for label, value in bound_result_inputs
    ]

    bound_error_inputs = [
        ("空值", None),
        ("只有编号", {"code": "X"}),
        ("只有消息", {"message": "m"}),
        ("空对象", {}),
        ("编号超长", {"code": "c" * 90}),
        ("消息超长", {"message": "m" * 510}),
        ("编号非字符串", {"code": 5}),
    ]
    bound_error_cases = [
        {
            "label": label,
            "input": value,
            "value": bound_view(manager_class._bound_error(value)),
        }
        for label, value in bound_error_inputs
    ]

    def runner(spec, cancel_event):
        if spec.task_id.endswith("2"):
            return {
                "status": "failed",
                "error": {"code": "MY_FAIL", "message": "失败原因"},
            }
        if spec.task_id.endswith("3"):
            raise RuntimeError("boom")
        return {"status": "completed", "summary": "完成摘要", "hidden": 1}

    def blocking_runner(spec, cancel_event):
        deadline = time_module.time() + 5
        while not cancel_event.is_set() and time_module.time() < deadline:
            time_module.sleep(0.01)
        return {"status": "completed", "summary": "迟到"}

    def wait_for_status(manager, task_id, status, timeout=5.0):
        deadline = time_module.time() + timeout
        while time_module.time() < deadline:
            current = manager.get(
                task_id, owner_id="owner-1", session_id="session-1"
            )
            if current is not None and current["status"] == status:
                return current
            time_module.sleep(0.01)
        return None

    scripted = []

    manager = manager_class(retention_seconds=3600, max_workers=2)
    specs = [
        spec_class("task-000000000001", "完成任务", "explore", "batch-000000000001"),
        spec_class("task-000000000002", "失败任务", "explore", "batch-000000000001"),
        spec_class("task-000000000003", "异常任务", "explore", "batch-000000000001"),
    ]
    spawn_result = manager.spawn(
        owner_id="owner-1", session_id="session-1", specs=specs, runner=runner
    )
    idle = manager.wait_for_idle(
        owner_id="owner-1", session_id="session-1", timeout=5
    )
    scripted.append(
        {
            "label": "同步完成失败与异常",
            "spawn": spawn_result,
            "idle": idle,
            "list": scrub_times(manager.list(owner_id="owner-1", session_id="session-1")),
            "get_first": scrub_times(
                manager.get(
                    "task-000000000001", owner_id="owner-1", session_id="session-1"
                )
            ),
            "get_other_session": manager.get(
                "task-000000000001", owner_id="owner-1", session_id="session-2"
            ),
            "notifications": sorted(
                scrub_times(
                    manager.drain_notifications(owner_id="owner-1", session_id="session-1")
                ),
                key=lambda item: item.get("task_id", ""),
            ),
            "notifications_again": scrub_times(
                manager.drain_notifications(owner_id="owner-1", session_id="session-1")
            ),
            "is_idle": manager.is_idle(owner_id="owner-1", session_id="session-1"),
        }
    )
    manager.close(owner_id="owner-1")

    manager = manager_class(retention_seconds=3600, max_workers=1)
    blocking_spec = spec_class(
        "task-000000000011", "阻塞任务", "explore", "batch-000000000011"
    )
    manager.spawn(
        owner_id="owner-1",
        session_id="session-1",
        specs=[blocking_spec],
        runner=blocking_runner,
    )
    running_snapshot = wait_for_status(manager, "task-000000000011", "running")
    cancel_result = manager.cancel(
        owner_id="owner-1", session_id="session-1", task_id="task-000000000011"
    )
    idle = manager.wait_for_idle(
        owner_id="owner-1", session_id="session-1", timeout=5
    )
    scripted.append(
        {
            "label": "取消运行中任务",
            "running": scrub_times(running_snapshot),
            "cancel": cancel_result,
            "idle": idle,
            "list": scrub_times(manager.list(owner_id="owner-1", session_id="session-1")),
        }
    )
    manager.close(owner_id="owner-1")

    manager = manager_class(retention_seconds=3600, max_workers=1)
    first = spec_class("task-000000000021", "阻塞一", "explore", "batch-000000000021")
    second = spec_class("task-000000000022", "排队二", "explore", "batch-000000000021")
    manager.spawn(
        owner_id="owner-1",
        session_id="session-1",
        specs=[first, second],
        runner=blocking_runner,
    )
    running_snapshot = wait_for_status(manager, "task-000000000021", "running")
    queued_snapshot = manager.get(
        "task-000000000022", owner_id="owner-1", session_id="session-1"
    )
    cancel_queued = manager.cancel(
        owner_id="owner-1", session_id="session-1", task_id="task-000000000022"
    )
    cancel_batch = manager.cancel(
        owner_id="owner-1", session_id="session-1", batch_id="batch-000000000021"
    )
    idle = manager.wait_for_idle(
        owner_id="owner-1", session_id="session-1", timeout=5
    )
    scripted.append(
        {
            "label": "取消排队与批次",
            "running": scrub_times(running_snapshot),
            "queued": scrub_times(queued_snapshot),
            "cancel_queued": cancel_queued,
            "cancel_batch": cancel_batch,
            "idle": idle,
            "list": scrub_times(manager.list(owner_id="owner-1", session_id="session-1")),
        }
    )
    manager.close(owner_id="owner-1")

    manager = manager_class(retention_seconds=3600, max_workers=2)
    duplicate_spec = spec_class(
        "task-000000000031", "重复任务", "explore", "batch-000000000031"
    )
    empty_spawn = outcome(
        manager.spawn,
        owner_id="owner-1",
        session_id="session-1",
        specs=[],
        runner=runner,
    )
    manager.spawn(
        owner_id="owner-1",
        session_id="session-1",
        specs=[duplicate_spec],
        runner=lambda spec, event: {"status": "completed"},
    )
    duplicate_spawn = outcome(
        manager.spawn,
        owner_id="owner-1",
        session_id="session-1",
        specs=[duplicate_spec],
        runner=runner,
    )
    cancel_missing = manager.cancel(
        owner_id="owner-1", session_id="session-1", task_id="task-000000000039"
    )
    manager.wait_for_idle(owner_id="owner-1", session_id="session-1", timeout=5)
    manager.close(owner_id="owner-1")
    closed_spawn = outcome(
        manager.spawn,
        owner_id="owner-1",
        session_id="session-1",
        specs=[spec_class("task-000000000032", "关闭后", "explore", "batch-000000000032")],
        runner=runner,
    )
    scripted.append(
        {
            "label": "校验与关闭",
            "empty_spawn": {"ok": empty_spawn["ok"], "error": empty_spawn["error"]},
            "duplicate_spawn": {
                "ok": duplicate_spawn["ok"],
                "error": duplicate_spawn["error"],
            },
            "cancel_missing": cancel_missing,
            "closed_spawn": {"ok": closed_spawn["ok"], "error": closed_spawn["error"]},
        }
    )

    manager = manager_class(retention_seconds=3600, max_workers=2)
    snapshots = [
        {
            "task_id": "task-00000000000a",
            "status": "completed",
            "owner_id": "owner-1",
            "session_id": "session-1",
            "description": "恢复一",
            "agent_type": "explore",
            "result": {"summary": "恢复摘要"},
        },
        {
            "task_id": "task-00000000000b",
            "status": "failed",
            "owner_id": "owner-1",
            "session_id": "session-1",
        },
        {
            "task_id": "task-00000000000c",
            "status": "running",
            "owner_id": "owner-1",
            "session_id": "session-1",
        },
        {
            "task_id": "task-00000000000d",
            "status": "completed",
            "owner_id": "owner-2",
            "session_id": "session-1",
        },
        {
            "task_id": "task-00000000000e",
            "status": "cancelled",
            "owner_id": "owner-1",
            "session_id": "session-1",
            "batch_id": "batch-00000000000e",
            "created_at": 1700000000.5,
        },
        "not-a-mapping",
    ]
    imported = manager.import_recovered_snapshots(
        owner_id="owner-1", session_id="session-1", snapshots=snapshots
    )
    scripted.append(
        {
            "label": "导入恢复快照",
            "snapshots": snapshots,
            "imported": imported,
            "list": scrub_times(manager.list(owner_id="owner-1", session_id="session-1")),
            "notifications": sorted(
                scrub_times(
                    manager.drain_notifications(owner_id="owner-1", session_id="session-1")
                ),
                key=lambda item: item.get("task_id", ""),
            ),
        }
    )
    manager.close(owner_id="owner-1")

    return {
        "bound_result": bound_result_cases,
        "bound_error": bound_error_cases,
        "scripted": scripted,
    }


COORDINATOR_DEFS = {
    "review.md": document(
        "name: review\ndescription: 评审\npermissionMode: delegated-read-only\nisolation: shared\ntools:\n  - read\n  - grep\n  - git"
    ),
    "review-full.md": document(
        "\n".join(
            [
                "name: review-full",
                "description: 评审完整 git",
                "permissionMode: delegated-read-only",
                "isolation: shared",
                "tools:",
                "  - read",
                "  - git",
                "gitMode: full",
            ]
        )
    ),
    "explore.md": document(
        "name: explore\ndescription: 探索\npermissionMode: delegated-read-only\nisolation: shared\nskills:\n  - one"
    ),
    "verify-agent.md": document(
        "name: verify-agent\ndescription: 验证\npermissionMode: explicit-command-allowlist\nisolation: shared\nbackground: true\ntools:\n  - read\n  - verify_command"
    ),
    "general-purpose.md": document(
        "name: general-purpose\ndescription: 通用\npermissionMode: standard\nisolation: worktree\ntools:\n  - read\n  - write_file\n  - bash"
    ),
    "shared-writer.md": document(
        "name: shared-writer\ndescription: 共享写\npermissionMode: standard\nisolation: shared\ntools:\n  - read\n  - write_file"
    ),
    "bad-readonly.md": document(
        "name: bad-readonly\ndescription: 后台只读\npermissionMode: delegated-read-only\nisolation: shared\nbackground: true"
    ),
    "with-mcp.md": document(
        "name: with-mcp\ndescription: 带 MCP\npermissionMode: delegated-read-only\nisolation: shared\nmcpServers:\n  - one"
    ),
    "worktree-readonly.md": document(
        "name: worktree-readonly\ndescription: 只读工作树\npermissionMode: delegated-read-only\nisolation: worktree"
    ),
}

COORDINATOR_CONFIGS = {
    "default": {},
    "verify": {"enable_verify_agent": True},
    "standard": {"allow_standard_agent": True, "allow_worktree": True},
    "standard-shared": {
        "allow_standard_agent": True,
        "allow_shared_workspace_writes": True,
    },
    "fork": {"allow_fork": True},
    "worktree": {"allow_worktree": True},
}

PARENT_TOOLS = [
    "list",
    "find",
    "read",
    "read_image",
    "grep",
    "web_search",
    "memory_search",
    "memory_read",
    "memory_expand_related",
    "write_file",
    "Edit_file",
    "memory_write",
    "subagent",
    "git",
    "bash",
    "powershell",
    "monitor",
]


def _task(description="描述", prompt="提示", agent_type="review", **extra):
    payload = {
        "description": description,
        "prompt": prompt,
        "subagent_type": agent_type,
    }
    payload.update(extra)
    return payload


COORDINATOR_VALIDATE_CASES = [
    ("合法只读任务", "default", {"action": "run", "tasks": [_task()]}),
    ("合法 spawn", "default", {"action": "spawn", "tasks": [_task()]}),
    ("action 非法", "default", {"action": "query", "tasks": [_task()]}),
    ("action 缺失", "default", {"tasks": [_task()]}),
    ("未知顶层字段", "default", {"action": "run", "tasks": [_task()], "zz": 1, "aa": 2}),
    ("tasks 非数组", "default", {"action": "run", "tasks": {"a": 1}}),
    ("tasks 缺失", "default", {"action": "run"}),
    ("tasks 为空", "default", {"action": "run", "tasks": []}),
    (
        "tasks 超上限",
        "default",
        {"action": "run", "tasks": [_task() for _ in range(5)]},
    ),
    ("并发零", "default", {"action": "run", "tasks": [_task()], "max_concurrency": 0}),
    ("并发超上限", "default", {"action": "run", "tasks": [_task()], "max_concurrency": 5}),
    ("并发为布尔", "default", {"action": "run", "tasks": [_task()], "max_concurrency": True}),
    ("并发为字符串", "default", {"action": "run", "tasks": [_task()], "max_concurrency": "2"}),
    ("并发合法", "default", {"action": "run", "tasks": [_task()], "max_concurrency": 3}),
    ("fail_fast 非布尔", "default", {"action": "run", "tasks": [_task()], "fail_fast": 1}),
    ("fail_fast 合法", "default", {"action": "run", "tasks": [_task()], "fail_fast": True}),
    ("任务非对象", "default", {"action": "run", "tasks": ["x"]}),
    (
        "未知任务字段",
        "default",
        {"action": "run", "tasks": [_task(zz=1, aa=2)]},
    ),
    ("描述为空", "default", {"action": "run", "tasks": [_task(description="  ")]}),
    ("描述非字符串", "default", {"action": "run", "tasks": [_task(description=7)]}),
    ("提示缺失", "default", {"action": "run", "tasks": [{"description": "d", "subagent_type": "review"}]}),
    ("类型为空", "default", {"action": "run", "tasks": [_task(agent_type=" ")]}),
    ("未知定义", "default", {"action": "run", "tasks": [_task(agent_type="nope")]}),
    (
        "未启用 verify",
        "default",
        {"action": "run", "tasks": [_task(agent_type="verify-agent")]},
    ),
    (
        "已启用 verify",
        "verify",
        {"action": "run", "tasks": [_task(agent_type="verify-agent")]},
    ),
    (
        "未启用 standard",
        "default",
        {"action": "run", "tasks": [_task(agent_type="general-purpose")]},
    ),
    (
        "已启用 standard",
        "standard",
        {"action": "run", "tasks": [_task(agent_type="general-purpose")]},
    ),
    (
        "standard 共享写已开启",
        "standard-shared",
        {"action": "run", "tasks": [_task(agent_type="shared-writer")]},
    ),
    (
        "standard 共享写未开启",
        "standard",
        {"action": "run", "tasks": [_task(agent_type="shared-writer")]},
    ),
    (
        "后台只读定义不可用",
        "default",
        {"action": "run", "tasks": [_task(agent_type="bad-readonly")]},
    ),
    (
        "带 skills 定义不可用",
        "default",
        {"action": "run", "tasks": [_task(agent_type="explore")]},
    ),
    (
        "带 MCP 定义不可用",
        "default",
        {"action": "run", "tasks": [_task(agent_type="with-mcp")]},
    ),
    (
        "worktree 只读未开启",
        "default",
        {"action": "run", "tasks": [_task(agent_type="worktree-readonly")]},
    ),
    (
        "worktree 只读已开启",
        "worktree",
        {"action": "run", "tasks": [_task(agent_type="worktree-readonly")]},
    ),
    (
        "context 非法",
        "default",
        {"action": "run", "tasks": [_task(context="shared")]},
    ),
    (
        "context fork 未开启",
        "default",
        {"action": "run", "tasks": [_task(context="fork")]},
    ),
    (
        "context fork 已开启",
        "fork",
        {"action": "run", "tasks": [_task(context="fork")]},
    ),
    ("参数非对象", "default", "not-a-dict"),
]


COORDINATOR_TOOL_CASES = [
    ("只读继承父工具", "review", "default"),
    ("只读 git 完整档", "review-full", "default"),
    ("verify profile", "verify-agent", "verify"),
    ("standard 工作树", "general-purpose", "standard"),
    ("standard 共享", "shared-writer", "standard-shared"),
    ("只读无白名单", "worktree-readonly", "worktree"),
]


WORKTREE_CONTROL_CASES = [
    ("列出会话", {"action": "list_worktrees"}),
    ("应用默认策略", {"action": "apply_worktree", "task_id": "task-1"}),
    (
        "应用 merge 并清理",
        {"action": "apply_worktree", "branch": " b ", "strategy": "merge", "cleanup": True},
    ),
    ("应用非法策略", {"action": "apply_worktree", "task_id": "t", "strategy": "squash"}),
    ("应用缺键", {"action": "apply_worktree"}),
    ("丢弃默认", {"action": "discard_worktree", "task_id": "task-1"}),
    (
        "丢弃带参数",
        {"action": "discard_worktree", "branch": "b", "remove_branch": False, "force": True},
    ),
    ("丢弃缺键", {"action": "discard_worktree", "task_id": "  "}),
    ("未知字段", {"action": "apply_worktree", "task_id": "t", "zz": 1}),
]


QUERY_CASES = [
    ("列出", {"action": "list"}),
    ("读取任务", {"action": "get", "task_id": "task-1"}),
    ("读取缺任务号", {"action": "get"}),
    ("读取空任务号", {"action": "get", "task_id": ""}),
    ("取消任务", {"action": "cancel", "task_id": "task-1"}),
    ("取消批次", {"action": "cancel", "batch_id": "batch-1"}),
    ("取消缺参数", {"action": "cancel"}),
    ("未知字段", {"action": "cancel", "task_id": "t", "zz": 1}),
]


def coordinator_cases(normalizer: Normalizer, root: Path) -> dict:
    from omnicrawl.agent.subagents import coordinator as coordinator_module
    from omnicrawl.agent.subagents.definitions import AgentDefinitionRegistry
    from omnicrawl.agent.types import ToolDefinition
    from omnicrawl.config.features.subagents import SubAgentConfig

    builtin = root / "coordinator" / "builtin"
    builtin.mkdir(parents=True, exist_ok=True)
    for name, content in COORDINATOR_DEFS.items():
        (builtin / name).write_text(content, encoding="utf-8")
    registry = AgentDefinitionRegistry(
        builtin_directory=builtin, home_directory=root / "coordinator" / "home"
    )
    registry.discover(root / "coordinator" / "ws")

    class CoordinatorProbe(coordinator_module.SubAgentCoordinator):
        """只补方法真正读到的属性，不启动线程池。"""

        def __init__(self, config, parent_tools=None, verify_tools=None):
            self.config = config
            self.registry = registry
            self._tools_provider = lambda: dict(parent_tools or {})
            self._verify_tools_provider = lambda: dict(verify_tools or {})

    def build_tools(require_confirmation=True):
        tools = {}
        for name in PARENT_TOOLS:
            tools[name] = ToolDefinition(
                name=name,
                description="占位",
                argument_schema="{}",
                requires_confirmation=require_confirmation,
                run=(lambda name: lambda arguments: {"tool": name})(name),
            )
        return tools

    def config_for(key):
        return SubAgentConfig(**COORDINATOR_CONFIGS[key])

    validate_cases = []
    for label, config_key, arguments in COORDINATOR_VALIDATE_CASES:
        probe = CoordinatorProbe(config_for(config_key))
        observed = outcome(probe._validate_arguments, arguments)
        result = observed["value"]
        if result is not None:
            result = [
                normalizer.text(item) if isinstance(item, str) else item
                for item in result
            ]
        validate_cases.append(
            {
                "label": label,
                "config": config_key,
                "arguments": arguments,
                "result": result,
                "error": observed["error"],
            }
        )

    tool_cases = []
    for label, definition_name, config_key in COORDINATOR_TOOL_CASES:
        parent_tools = build_tools()
        verify_tools = {
            "verify_command": ToolDefinition(
                name="verify_command",
                description="占位",
                argument_schema="{}",
                requires_confirmation=False,
                run=lambda arguments: {"tool": "verify_command"},
            )
        }
        probe = CoordinatorProbe(config_for(config_key), parent_tools, verify_tools)
        definition = registry.get(definition_name)
        tools = probe._tools_for_definition(definition)
        available = dict(parent_tools)
        available.update(verify_tools)
        source_tools = (
            available
            if definition.permission_mode == "explicit-command-allowlist"
            else parent_tools
        )
        wrapped = sorted(
            name for name, tool in tools.items() if tool.run is not source_tools[name].run
        )
        tool_cases.append(
            {
                "label": label,
                "definition": definition_name,
                "config": config_key,
                "parent_tools": list(PARENT_TOOLS),
                "parent_confirmation": {name: True for name in PARENT_TOOLS},
                "effective_confirmation": {
                    name: bool(tool.requires_confirmation)
                    for name, tool in available.items()
                },
                "tools": sorted(tools),
                "confirmation": {
                    name: bool(tool.requires_confirmation) for name, tool in tools.items()
                },
                "wrapped": wrapped,
            }
        )

    worktree_cases = [
        {"label": label, "arguments": arguments, **_worktree_probe(arguments)}
        for label, arguments in WORKTREE_CONTROL_CASES
    ]

    query_cases = [
        {"label": label, "arguments": arguments, **_query_probe(arguments)}
        for label, arguments in QUERY_CASES
    ]

    from types import SimpleNamespace

    task = SimpleNamespace(
        batch_id="batch-1",
        task_id="task-1",
        description="描述",
        agent_type="review",
        definition=SimpleNamespace(source="builtin"),
    )
    failure = coordinator_module.SubAgentCoordinator._failure_payload(
        task,
        code="SUBAGENT_MODEL_ERROR",
        message="子任务模型请求失败。",
        diagnostic={"category": "RATE_LIMIT", "retryable": True},
    )
    cancelled = coordinator_module.SubAgentCoordinator._cancelled_payload(
        task, "任务已取消。"
    )
    diagnostics = []
    for label, message, model, wire_model in [
        ("普通异常", "boom", "", ""),
        ("配额关键词", "rate limit exceeded", "deepseek-v4", "deepseek-v4-2024"),
        ("鉴权关键词", "invalid api key", "", "wire"),
    ]:
        diagnostics.append(
            {
                "label": label,
                "message": message,
                "model": model,
                "wire_model": wire_model,
                "value": coordinator_module._build_failure_diagnostics(
                    RuntimeError(message), model=model, wire_model=wire_model
                ),
            }
        )

    projection = {
        "failure": failure,
        "cancelled": cancelled,
        "top_level": {
            "batch_id": None,
            "status": "failed",
            "results": [],
            "error": {"code": "SUBAGENT_DISABLED", "message": "未启用"},
        },
        "json_text": coordinator_module.SubAgentCoordinator._json_result(
            False, {"b": 1, "a": [1, 2]}
        ).output,
        "task_event": {
            "batch_id": "batch-1",
            "task_id": "task-1",
            "agent_type": "review",
            "description": "描述",
            "definition_source": "builtin",
            "status": "running",
        },
        "terminal_event": {
            "batch_id": "batch-1",
            "task_id": "task-1",
            "agent_type": "review",
            "description": "描述",
            "definition_source": "builtin",
            "status": "completed",
            "summary": "ok",
            "artifacts": [],
            "usage": {},
            "error": None,
        },
    }

    return {
        "definitions": COORDINATOR_DEFS,
        "configs": COORDINATOR_CONFIGS,
        "validate": validate_cases,
        "tools": tool_cases,
        "worktree": worktree_cases,
        "query": query_cases,
        "projection": projection,
        "diagnostics": diagnostics,
    }


def _worktree_probe(arguments):
    """用真实现解析 worktree 控制动作；回调换成固定返回值。"""

    from omnicrawl.agent.subagents.coordinator import SubAgentCoordinator

    class Probe(SubAgentCoordinator):
        def __init__(self):
            self._apply_worktree = lambda key, strategy, cleanup: "已应用"
            self._discard_worktree = lambda key, remove_branch, force: "已丢弃"
            self._list_worktrees = lambda: []

        def _top_level_error(self, code, message):
            raise RuntimeError("%s|%s" % (code, message))

        def _json_result(self, ok, payload):
            return payload

    try:
        Probe()._worktree_control_action(dict(arguments))
    except RuntimeError as exc:
        code, _, message = str(exc).partition("|")
        return {"kind": None, "code": code, "message": message}
    return {"kind": "ok", "code": None, "message": None}


def _query_probe(arguments):
    from omnicrawl.agent.subagents.coordinator import SubAgentCoordinator

    class Probe(SubAgentCoordinator):
        def __init__(self):
            pass

        def _top_level_error(self, code, message):
            raise RuntimeError("%s|%s" % (code, message))

        def list_tasks(self):
            return []

        def get_task(self, task_id):
            # 参数判定与「任务是否存在」分开：这里让任务存在，只对照校验层。
            return {"task_id": task_id}

        def cancel_task(self, *, task_id=None, batch_id=None):
            return {"ok": True}

        def _json_result(self, ok, payload):
            return payload

    try:
        Probe()._query_action(dict(arguments))
    except RuntimeError as exc:
        code, _, message = str(exc).partition("|")
        return {"kind": None, "code": code, "message": message}
    return {"kind": "ok", "code": None, "message": None}


def _batch_script(operations, task_count):
    """按脚本驱动真 `_ActiveBatch`，记录每次返回值与末态。"""

    from types import SimpleNamespace

    from omnicrawl.agent.subagents.coordinator import _ActiveBatch

    class FakeFuture:
        pass

    batch = _ActiveBatch(
        batch_id="batch-1",
        tasks=tuple(SimpleNamespace() for _ in range(task_count)),
    )
    results = []
    for operation in operations:
        name = operation[0]
        if name == "register":
            results.append(batch.register_future(FakeFuture(), operation[1]))
        elif name == "cancel":
            results.append(batch.begin_cancel(operation[1]))
        elif name == "finish":
            results.append(batch.mark_finished(operation[1]))
        elif name == "untracked":
            results.append(list(batch.untracked_indexes()))
    return {
        "results": results,
        "cancel_reason": batch.cancel_reason,
        "done": batch.done_event.is_set(),
        "finished": sorted(batch.finished_indexes),
    }


BATCH_SCRIPTS = [
    ("顺序登记与完成", 3, [["register", 0], ["register", 1], ["finish", 0], ["untracked"]]),
    ("取消后再登记", 2, [["cancel", "父任务已取消子任务。"], ["register", 0], ["cancel", "再来一次"]]),
    ("重复完成不重复计数", 2, [["finish", 0], ["finish", 0], ["finish", 1]]),
    ("全部未登记", 2, [["untracked"]]),
    ("登记与完成交错", 3, [["register", 2], ["finish", 2], ["register", 0], ["untracked"], ["finish", 1]]),
]


SHARED_WRITER_CASES = [
    ("工作树写任务", "worktree", "standard", ["write_file"], "shared"),
    ("共享写任务", "shared", "standard", ["write_file"], "shared"),
    ("共享只读任务", "shared", "standard", ["read"], "shared"),
    ("共享只读代理", "shared", "delegated-read-only", ["write_file"], "shared"),
    ("继承定义共享", "", "standard", ["bash"], "shared"),
    ("继承定义工作树", "", "standard", ["bash"], "worktree"),
    ("命令工具也算写", "shared", "standard", ["powershell"], "shared"),
    ("无写工具", "shared", "standard", ["grep", "read"], "shared"),
]


CANCELLATION_CASES = [
    ("取消异常", "SubAgentCancelled"),
    ("标准取消异常", "CancelledError"),
    ("全大写", "SUBCANCEL"),
    ("小写下划线", "cancel_scope"),
    ("键盘中断", "KeyboardInterrupt"),
    ("运行时错误", "RuntimeError"),
    ("超时", "TimeoutError"),
]


BATCH_STATUS_CASES = [
    ("全部完成", [{"status": "completed"}, {"status": "completed"}]),
    ("部分完成", [{"status": "completed"}, {"status": "failed"}]),
    ("全部失败", [{"status": "failed"}, {"status": "cancelled"}]),
    ("空批次", []),
]


def batch_cases() -> dict:
    from types import SimpleNamespace

    from omnicrawl.agent.subagents.coordinator import SubAgentCoordinator

    scripts = [
        {"label": label, "task_count": task_count, "operations": operations, **_batch_script(operations, task_count)}
        for label, task_count, operations in BATCH_SCRIPTS
    ]

    class WriterProbe(SubAgentCoordinator):
        """`_requires_shared_writer_lock` 只读 task 的属性，不需要初始化宿主。"""

        def __init__(self):
            pass

    writer_probe = WriterProbe()
    shared = []
    for label, isolation, permission_mode, tool_names, definition_isolation in SHARED_WRITER_CASES:
        task = SimpleNamespace(
            execution_context=SimpleNamespace(isolation=isolation),
            definition=SimpleNamespace(
                permission_mode=permission_mode, isolation=definition_isolation
            ),
            tools={name: None for name in tool_names},
        )
        shared.append(
            {
                "label": label,
                "isolation": isolation,
                "permission_mode": permission_mode,
                "tool_names": tool_names,
                "definition_isolation": definition_isolation,
                "expected": bool(writer_probe._requires_shared_writer_lock(task)),
            }
        )

    cancellation = [
        {
            "label": label,
            "exception_type": name,
            "expected": "cancel" in name.casefold(),
        }
        for label, name in CANCELLATION_CASES
    ]

    statuses = []
    for label, results in BATCH_STATUS_CASES:
        status_list = [item["status"] for item in results]
        completed_count = status_list.count("completed")
        if completed_count == len(results):
            status = "completed"
        elif completed_count:
            status = "partial"
        else:
            status = "failed"
        summary = SubAgentCoordinator._json_result(
            status == "completed",
            {"batch_id": "batch-1", "status": status, "results": results},
        )
        statuses.append(
            {
                "label": label,
                "results": results,
                "status": status,
                "ok": status == "completed",
                "text": summary.output,
            }
        )

    return {
        "scripts": scripts,
        "shared_writer": shared,
        "cancellation": cancellation,
        "statuses": statuses,
    }


def main() -> None:
    root = Path(tempfile.mkdtemp(prefix="omnicrawl-subagents-"))
    case_dir = root / "cases"
    case_dir.mkdir(parents=True, exist_ok=True)
    normalizer = Normalizer(root)
    discover_root = root / "discover"
    discover_root.mkdir(parents=True, exist_ok=True)
    try:
        payload = {
            "definitions": {
                "parse": parse_cases(normalizer, case_dir),
                "templates": template_cases(normalizer),
                "discover": discover_cases(normalizer, discover_root),
            },
            "execution": execution_cases(),
            "verify": verify_cases(),
            "recovery": recovery_cases(),
            "coordinator": coordinator_cases(normalizer, root),
            "batch": batch_cases(),
            "tasks": tasks_cases(),
        }
    finally:
        shutil.rmtree(root, ignore_errors=True)

    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(payload, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    print("已写入 %s" % FIXTURE_PATH)


if __name__ == "__main__":
    main()
