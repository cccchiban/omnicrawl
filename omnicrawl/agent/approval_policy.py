"""Agent 子系统内部模块。

本文件由原合并入口按既有模块边界恢复，职责说明见模块内公开对象。
"""

from __future__ import annotations

import json
import re
from typing import Any

from .types import ToolDefinition


TOOL_REVIEW_SYSTEM_PROMPT = (
    "你是本地 OmniCrawl 的命令执行安全审查器。"
    "自动审查模式下，Host 会把通过 bash 和 powershell 工具执行的命令交给你审查；其他工具调用由 Host 自动放行。"
    "你只判断这一次命令执行是否可以自动批准，不执行命令，也不补写方案。"
    "请用严格 JSON 回复：{\"approve\": true/false, \"reason\": \"一句中文理由\"}。"
    "命令目标清晰、影响范围明确、且不会破坏系统时可以批准。"
    "当命令明显越界访问、读取密钥、破坏系统、删除或覆盖大量文件、修改真实生产数据、"
    "执行无法判断影响的危险命令，或参数不足以判断时，必须拒绝。"
)

_DELETE_COMMAND_PATTERN = re.compile(
    r"(?<![\w.-])(?:rm|rmdir|del|erase|rd|remove-item|ri|unlink|clean)"
    r"(?:\.exe|\.cmd|\.bat|\.ps1)?(?=\s|$|[;&|])",
    re.IGNORECASE,
)
_GIT_CLEAN_PATTERN = re.compile(r"(?<![\w.-])git(?:\.exe)?\s+clean(?=\s|$|[;&|])", re.IGNORECASE)
_FIND_DELETE_PATTERN = re.compile(r"(?<![\w.-])find(?:\.exe)?\b.*(?:\s-delete\b|\s-exec\s+rm\b)", re.IGNORECASE)
# 匹配常见 shell 分隔符之后的 Git 命令，同时跳过 ``-C`` / ``-c`` / ``--no-pager``
# 等全局选项。解析的目标是风险下界：无法证明为只读的 Git 子命令必须进入确认。
_GIT_COMMAND_PATTERN = re.compile(
    r"(?<![\w.-])git(?:\.exe)?"
    r"(?:\s+(?:--[a-z0-9][\w-]*(?:=[^\s;&|]+)?|-C\s+[^\s;&|]+|-c\s+[^\s;&|]+))*"
    r"\s+([a-z][\w-]*)",
    re.IGNORECASE,
)
_GIT_READ_ONLY_SUBCOMMANDS = frozenset(
    {
        "blame",
        "cat-file",
        "check-attr",
        "check-ignore",
        "describe",
        "diff",
        "for-each-ref",
        "fsck",
        "grep",
        "help",
        "log",
        "ls-files",
        "ls-remote",
        "ls-tree",
        "name-rev",
        "rev-list",
        "rev-parse",
        "show",
        "show-ref",
        "shortlog",
        "status",
        "symbolic-ref",
        "var",
        "verify-commit",
        "verify-tag",
        "whatchanged",
    }
)
_GIT_INTENT_KEYS = {
    "action",
    "command",
    "cmd",
    "method",
    "op",
    "operation",
    "script",
    "verb",
}
_DELETE_INTENT_PATTERN = re.compile(
    r"(^|[._:/\\-])(?:delete|del|erase|remove|rm|rmdir|unlink|删除|移除|清空)($|[._:/\\-])",
    re.IGNORECASE,
)
_DELETE_TEXT_INTENT_PATTERN = re.compile(
    r"(^|[\s._:/\\-])(?:delete|del|erase|remove|rm|rmdir|unlink)($|[\s._:/\\-])",
    re.IGNORECASE,
)
_DELETE_DESCRIPTION_START_PATTERN = re.compile(
    r"^(?:delete|del|erase|remove|rm|rmdir|unlink)($|[\s._:/\\-])",
    re.IGNORECASE,
)
_DELETE_LOCALIZED_TERMS = ("删除", "移除", "清空")
_DELETE_INTENT_KEYS = {
    "action",
    "command",
    "cmd",
    "method",
    "mode",
    "op",
    "operation",
    "script",
    "verb",
}
_MCP_DELETE_INTENT_KEYS = _DELETE_INTENT_KEYS


def is_shell_command_tool_call(tool: ToolDefinition, arguments: dict[str, Any]) -> bool:
    """判断工具调用是否通过 bash 或 powershell 执行命令，供自动审查模式决定是否进入审查。

    自动审查模式的目标是减少普通文件读写、搜索等工具的审批噪音，只把真正需要
    守住的 bash/powershell 命令执行交给审查模型。这里优先按工具名识别，同时兼容
    bashcommand / powershellcommand 这类常见别名；不扫描正文参数，避免误判。
    """

    normalized_name = re.sub(r"[^a-z0-9]+", "_", tool.name.casefold()).strip("_")
    return normalized_name in {"bash", "bashcommand", "powershell", "powershellcommand"}


def is_delete_behavior_tool_call(tool: ToolDefinition, arguments: dict[str, Any]) -> bool:
    """判断工具调用是否带有显式删除意图，供 review 模式决定是否进入审查。

    review 模式的目标是减少普通读写、搜索和测试命令的审批噪音，只把真正需要
    守住的删除类动作交给审查模型。这里优先识别工具名和命令字符串，
    同时检查 MCP 常见的 action/operation/method 等意图字段；避免扫描 content
    这类正文参数，以免用户写入的普通文本里出现 delete 一词就被误判。
    """

    if text_has_delete_intent(tool.name):
        return True

    command = arguments.get("command")
    if isinstance(command, str) and command_has_delete_intent(command):
        return True

    if not tool_accepts_shell_command(tool) and description_has_delete_intent(tool.description):
        return True

    return arguments_have_delete_intent(arguments, intent_keys=_MCP_DELETE_INTENT_KEYS)


def is_git_mutation_tool_call(tool: ToolDefinition, arguments: dict[str, Any]) -> bool:
    """判断一次调用是否会修改 Git 状态或无法证明为只读。

    仅把明确的只读子命令（如 ``status``、``diff``、``log``、``show``）排除。
    其他 Git 子命令，包括未知子命令和可能改写索引、工作树、引用、配置或远端的
    操作，均保守地需要确认。检查范围限定在命令/动作字段，避免普通文件正文中
    出现 ``git commit`` 文本就被误判。
    """

    if _arguments_have_git_mutation_intent(arguments):
        return True

    normalized_name = re.sub(r"[^a-z0-9]+", "_", tool.name.casefold()).strip("_")
    if not normalized_name.startswith("git"):
        return False
    if normalized_name == "git":
        return True

    parts = normalized_name.split("_")
    if len(parts) < 2:
        return True
    return _git_subcommand_requires_confirmation(parts[1])


def command_has_git_mutation_intent(command: str) -> bool:
    """判断 shell 文本中是否含有变更性 Git 子命令。"""

    return any(
        _git_subcommand_requires_confirmation(match.group(1))
        for match in _GIT_COMMAND_PATTERN.finditer(command)
    )


def _arguments_have_git_mutation_intent(value: Any) -> bool:
    if isinstance(value, dict):
        for raw_key, item in value.items():
            if not isinstance(raw_key, str):
                continue
            key = raw_key.strip().casefold()
            if key in _GIT_INTENT_KEYS and isinstance(item, str):
                if command_has_git_mutation_intent(item):
                    return True
            elif isinstance(item, (dict, list)) and _arguments_have_git_mutation_intent(item):
                return True
        return False
    if isinstance(value, list):
        return any(_arguments_have_git_mutation_intent(item) for item in value)
    return False


def _git_subcommand_requires_confirmation(subcommand: str) -> bool:
    return subcommand.strip().casefold() not in _GIT_READ_ONLY_SUBCOMMANDS


def tool_accepts_shell_command(tool: ToolDefinition) -> bool:
    return "command" in tool.argument_schema.lower() or "cmd" in tool.argument_schema.lower()


def arguments_have_delete_intent(
    value: Any,
    *,
    intent_keys: set[str] = _DELETE_INTENT_KEYS,
) -> bool:
    if isinstance(value, dict):
        for raw_key, item in value.items():
            if not isinstance(raw_key, str):
                continue

            key = raw_key.strip().lower()
            if text_has_delete_intent(key):
                return True
            if key in intent_keys and isinstance(item, str):
                if command_has_delete_intent(item) or text_has_delete_intent(item):
                    return True
            elif isinstance(item, dict):
                if arguments_have_delete_intent(item, intent_keys=intent_keys):
                    return True
            elif isinstance(item, list):
                if any(arguments_have_delete_intent(child, intent_keys=intent_keys) for child in item):
                    return True
    elif isinstance(value, list):
        return any(arguments_have_delete_intent(item, intent_keys=intent_keys) for item in value)
    return False


def command_has_delete_intent(command: str) -> bool:
    return bool(
        _DELETE_COMMAND_PATTERN.search(command)
        or _GIT_CLEAN_PATTERN.search(command)
        or _FIND_DELETE_PATTERN.search(command)
        or _DELETE_INTENT_PATTERN.search(command)
    )


def text_has_delete_intent(text: str) -> bool:
    if any(term in text for term in _DELETE_LOCALIZED_TERMS):
        return True
    normalized_text = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", text)
    return bool(_DELETE_TEXT_INTENT_PATTERN.search(normalized_text))


def description_has_delete_intent(text: str) -> bool:
    stripped = text.lstrip(" \t\r\n-_*:;,.")
    if any(stripped.startswith(term) for term in _DELETE_LOCALIZED_TERMS):
        return True
    normalized_text = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", stripped)
    return bool(_DELETE_DESCRIPTION_START_PATTERN.search(normalized_text))


def parse_tool_review_response(review_text: str) -> tuple[bool, str]:
    """解析审查模型 JSON；不可解析时按拒绝处理。"""

    text = review_text.strip()
    if not text:
        return False, "审查模型返回为空。"

    match = re.search(r"\{.*\}", text, re.DOTALL)
    payload = match.group(0) if match else text
    try:
        data = json.loads(payload)
    except json.JSONDecodeError:
        return False, f"审查模型返回不是 JSON：{text}"

    if not isinstance(data, dict):
        return False, "审查模型返回不是 JSON 对象。"

    reason_value = data.get("reason", "")
    reason = reason_value.strip() if isinstance(reason_value, str) else ""
    approve_value = data.get("approve")
    if approve_value is True:
        return True, reason
    if approve_value is False:
        # 模型明确拒绝：reason 为空时给默认理由，与“格式不完整”区分开，
        # 避免调用方把“模型拒绝”误报为“模型未给出结论”。
        return False, reason or "模型拒绝执行。"
    # approve 缺失或非布尔（如字符串 "true"）：格式不完整，同样按拒绝处理，
    # 但明确告知是格式问题而非模型决策；模型给出的 reason 仅作附加参考，
    # 不能因 reason 非空而掩盖格式问题。
    hint = "模型未给出明确的批准结论（approve 字段缺失或非布尔）。"
    return False, hint if not reason else f"{hint} 模型 reason：{reason}"
