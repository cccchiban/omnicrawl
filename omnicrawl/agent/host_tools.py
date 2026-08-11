"""Provider 固定工具与 Host 侧动态工具目录。

Provider 只注册 ``search_tools`` 和 ``invoke_tool``。真实 ToolDefinition、参数
Schema、执行器和审批策略仍由 Host 持有；本模块只负责发现、分发前校验以及把
Host 目录转换成固定的 Provider 工具面。
"""

from __future__ import annotations

import json
import re
import unicodedata
from dataclasses import dataclass
from difflib import SequenceMatcher
from functools import lru_cache
from typing import Any, Mapping, Sequence

from .llm_protocol import tool_parameters_schema
from .tools import TOOL_NAME_ALIASES, normalize_tool_call, normalize_tool_name
from .types import ToolCall, ToolDefinition, ToolResult

SEARCH_TOOLS_NAME = "search_tools"
INVOKE_TOOL_NAME = "invoke_tool"
PROVIDER_TOOL_NAMES = (SEARCH_TOOLS_NAME, INVOKE_TOOL_NAME)

# 英文查询中的虚词：命中这些词不能代表工具能力，分词阶段直接丢弃。
_EN_STOP_WORDS = frozenset(
    {
        "a", "an", "and", "at", "for", "help", "in", "me", "of", "on",
        "or", "please", "show", "the", "to", "with",
    }
)
# 中文查询中的单字虚词：单字命中价值极低，分词阶段丢弃。
_HAN_STOP_WORDS = frozenset("请帮我的一了么吗呢啊吧和与或把被让给下就都")
# 别名反查表：工具名 -> 常见误写/缩写（来自 TOOL_NAME_ALIASES），搜索时等价命中。
_ALIASES_BY_TOOL_NAME: dict[str, tuple[str, ...]] = {}
for _alias, _real_name in TOOL_NAME_ALIASES.items():
    _ALIASES_BY_TOOL_NAME.setdefault(_real_name, []).append(_alias)
_ALIASES_BY_TOOL_NAME = {
    name: tuple(aliases) for name, aliases in _ALIASES_BY_TOOL_NAME.items()
}


def _normalize_text(value: str) -> str:
    """搜索用文本归一化：NFKC 折叠全角/半角差异后再统一小写。"""

    return unicodedata.normalize("NFKC", value).casefold().strip()


def _query_terms(value: str) -> list[str]:
    """把归一化后的查询拆成匹配用词元。

    - 英文/数字：整段保留，过滤虚词；
    - 中文：整段保留 + 相邻两字 bigram，让"读取文件"能命中描述里的
      "读取"或"文件"；同时补充首尾单字（过滤虚词），让"写文件"
      这类查询能命中描述里的"写"。
    """

    normalized = _normalize_text(value)
    terms: list[str] = []
    for ascii_part in re.findall(r"[a-z0-9_]+", normalized):
        if ascii_part not in _EN_STOP_WORDS:
            terms.append(ascii_part)
    for han_part in re.findall(r"[\u4e00-\u9fff]+", normalized):
        if len(han_part) == 1:
            if han_part not in _HAN_STOP_WORDS:
                terms.append(han_part)
        else:
            terms.append(han_part)
            terms.extend(
                han_part[index : index + 2] for index in range(len(han_part) - 1)
            )
            for edge in (han_part[0], han_part[-1]):
                if edge not in _HAN_STOP_WORDS:
                    terms.append(edge)
    return terms


@lru_cache(maxsize=256)
def _schema_property_names(schema_json: str) -> frozenset[str]:
    """从工具参数 Schema 提取第一层属性名，供搜索做低权重匹配。"""

    try:
        schema = json.loads(schema_json)
    except Exception:
        return frozenset()
    if not isinstance(schema, dict):
        return frozenset()
    properties = schema.get("properties")
    if not isinstance(properties, dict):
        return frozenset()
    return frozenset(str(key) for key in properties)

_SEARCH_TOOLS_SCHEMA = json.dumps(
    {
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "minLength": 1,
                "description": "工具名称、能力或任务目标。",
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": 6,
                "default": 4,
            },
        },
        "required": ["query"],
        "additionalProperties": False,
    },
    ensure_ascii=False,
)
_INVOKE_TOOL_SCHEMA = json.dumps(
    {
        "type": "object",
        "properties": {
            "tool_name": {
                "type": "string",
                "minLength": 1,
                "description": "search_tools 返回的真实工具名。",
            },
            "arguments": {
                "type": "object",
                "additionalProperties": True,
                "description": "严格按照 search_tools 返回的工具契约填写。",
            },
        },
        "required": ["tool_name", "arguments"],
        "additionalProperties": False,
    },
    ensure_ascii=False,
)


@dataclass(frozen=True)
class PreparedToolInvocation:
    """经过真实工具解析、参数归一化和 Schema 校验后的调用。"""

    requested_name: str
    tool_name: str
    arguments: dict[str, Any]
    tool: ToolDefinition


class HostToolCatalog:
    """当前 Agent 可见的真实工具目录。

    目录是一次构建时的快照，避免工具开关、MCP 能力刷新或工作区切换过程中，
    一次模型回合看到的搜索结果和实际执行器发生不一致。
    """

    def __init__(self, tools: Mapping[str, ToolDefinition]) -> None:
        self._tools = dict(tools)

    @property
    def tools(self) -> Mapping[str, ToolDefinition]:
        return self._tools

    def search(self, arguments: dict[str, Any]) -> ToolResult:
        """按名称和描述搜索工具，并返回候选工具的紧凑参数契约。"""

        query = arguments.get("query")
        if not isinstance(query, str) or not query.strip():
            return _error_result(
                "invalid_arguments",
                "search_tools.query 必须是非空字符串。",
                retryable=True,
            )

        raw_limit = arguments.get("limit", 4)
        limit = _bounded_int(raw_limit, default=4, minimum=1, maximum=6)
        query_text = query.strip()
        ranked: list[tuple[int, str, ToolDefinition]] = []
        for name, tool in self._tools.items():
            score = self._match_score(query_text, tool)
            if score > 0:
                ranked.append((score, name, tool))
        ranked.sort(key=lambda item: (-item[0], item[1]))
        selected = ranked[:limit]
        payload = {
            "schema_version": 1,
            "ok": True,
            "query": query_text,
            "tools": [
                {
                    "name": name,
                    "description": _short_text(tool.description, 240),
                    "parameters": compact_tool_schema(tool),
                    "requires_confirmation": bool(tool.requires_confirmation),
                }
                for _score, name, tool in selected
            ],
            "count": len(selected),
            "truncated": len(ranked) > len(selected),
        }
        return _bounded_json_result(payload, max_chars=5200)

    def prepare_invocation(
        self,
        arguments: Mapping[str, Any] | Any,
    ) -> PreparedToolInvocation | ToolResult:
        """解析 ``invoke_tool`` 外层参数并校验真实工具契约。"""

        if not isinstance(arguments, Mapping):
            return _error_result(
                "invalid_arguments",
                "invoke_tool.arguments 必须是 JSON 对象。",
                retryable=True,
            )

        raw_name = arguments.get("tool_name")
        if not isinstance(raw_name, str) or not raw_name.strip():
            return _error_result(
                "invalid_arguments",
                "invoke_tool.tool_name 必须是非空字符串。",
                retryable=True,
            )
        requested_name = raw_name.strip()

        raw_arguments = arguments.get("arguments")
        if not isinstance(raw_arguments, Mapping):
            return _error_result(
                "invalid_arguments",
                "invoke_tool.arguments 必须是 JSON 对象。",
                tool_name=requested_name,
                retryable=True,
            )

        tool_name = self._resolve_name(requested_name)
        tool = self._tools.get(tool_name)
        if tool is None:
            suggestions = self._suggest(requested_name)
            return _error_result(
                "unknown_tool",
                f"工具目录中不存在：{requested_name}。",
                tool_name=requested_name,
                retryable=True,
                extra={"suggestions": suggestions},
            )

        normalized_call = normalize_tool_call(
            ToolCall(name=tool_name, arguments=dict(raw_arguments)),
            self._tools,
        )
        normalized_arguments = normalized_call.arguments
        issues = validate_tool_arguments(tool, normalized_arguments)
        if issues:
            return _error_result(
                "invalid_arguments",
                f"工具 {tool.name} 的参数未通过 Schema 校验。",
                tool_name=tool.name,
                retryable=True,
                extra={
                    "issues": issues,
                    "contract": compact_tool_schema(tool),
                },
            )

        return PreparedToolInvocation(
            requested_name=requested_name,
            tool_name=tool.name,
            arguments=normalized_arguments,
            tool=tool,
        )

    def dispatcher_required(self, _arguments: dict[str, Any]) -> ToolResult:
        """防止绕过审批直接执行 Provider 元工具。"""

        return _error_result(
            "dispatcher_required",
            "invoke_tool 必须由 Host 分发器执行，不能直接运行。",
            retryable=False,
        )

    def _resolve_name(self, requested_name: str) -> str:
        normalized = normalize_tool_name(requested_name, self._tools)
        if normalized in self._tools:
            return normalized
        folded = _normalize_text(requested_name)
        matches = [name for name in self._tools if _normalize_text(name) == folded]
        return matches[0] if len(matches) == 1 else normalized

    def _suggest(self, requested_name: str) -> list[str]:
        ranked = sorted(
            (
                (self._match_score(requested_name, tool), name)
                for name, tool in self._tools.items()
            ),
            key=lambda item: (-item[0], item[1]),
        )
        return [name for score, name in ranked[:5] if score > 0]

    @staticmethod
    def _match_score(query: str, tool: ToolDefinition) -> int:
        """查询与工具的多层匹配打分。

        档位设计（分数越高越优先）：
        - 完全等于工具名：+1000，直接置顶；
        - 查询是工具名连续子串 / 等于工具别名：+300；
        - 查询是描述连续子串：+120；
        - 工具名拼写错误（相似度 >= 0.85）：+150 兑底；
        - 别名是查询的子串：+60；
        - 分词命中工具名：+80，命中描述：+20；
        - 参数名命中：+10（最低权重）。
        所有文本先做 NFKC 归一化，全角/半角等价。
        """

        query_folded = _normalize_text(query)
        if not query_folded:
            return 0
        name = _normalize_text(tool.name)
        description = _normalize_text(tool.description)
        if query_folded == name:
            return 1000
        score = 0
        if query_folded in name:
            score += 300
        if query_folded in description:
            score += 120
        # 工具别名（readimage -> read_image）等价于工具名子串档。
        for alias in _ALIASES_BY_TOOL_NAME.get(tool.name, ()):
            alias_folded = _normalize_text(alias)
            if query_folded == alias_folded:
                score += 300
            elif alias_folded in query_folded or query_folded in alias_folded:
                score += 60
        # 拼写错误兑底：与工具名相似度足够高时给中等分。
        if query_folded != name:
            similarity = SequenceMatcher(None, query_folded, name).ratio()
            if similarity >= 0.85:
                score += 150
        for term in _query_terms(query_folded):
            if term in name:
                score += 80
            elif term in description:
                score += 20
        # 参数名命中（低权重）：查询/参数名互为子串即可。
        for parameter in _schema_property_names(tool.argument_schema):
            if parameter in query_folded or query_folded in parameter:
                score += 10
        return score


def build_provider_tools(catalog: HostToolCatalog) -> dict[str, ToolDefinition]:
    """构建固定的 Provider 工具面，真实工具不会出现在返回值中。"""

    return {
        SEARCH_TOOLS_NAME: ToolDefinition(
            name=SEARCH_TOOLS_NAME,
            description=(
                "搜索当前 Agent 可用的工具目录。请先用 search_tools 找到目标工具，"
                "再根据返回的紧凑参数契约调用 invoke_tool。"
            ),
            argument_schema=_SEARCH_TOOLS_SCHEMA,
            requires_confirmation=False,
            run=catalog.search,
            model_output_is_bounded=True,
        ),
        INVOKE_TOOL_NAME: ToolDefinition(
            name=INVOKE_TOOL_NAME,
            description=(
                "执行工具目录中的工具。请先用 search_tools 找到目标工具，再用 invoke_tool 严格按返回的"
                "契约填写 tool_name 和 arguments；参数错误会返回结构化诊断供修正重试。"
            ),
            argument_schema=_INVOKE_TOOL_SCHEMA,
            # 真实工具解析后才决定是否需要审批，外层元工具不能代表批准。
            requires_confirmation=False,
            run=catalog.dispatcher_required,
            model_output_is_bounded=True,
        ),
    }


def compact_tool_schema(tool: ToolDefinition) -> dict[str, Any]:
    """删除长描述和默认值，只保留模型填写参数所需的 Schema 信息。"""

    try:
        schema = tool_parameters_schema(tool)
    except Exception:
        schema = {"type": "object", "properties": {}}
    return _compact_schema_node(schema, depth=0)


def validate_tool_arguments(
    tool: ToolDefinition,
    arguments: Mapping[str, Any] | Any,
) -> list[dict[str, str]]:
    """校验真实工具参数，覆盖项目现有工具使用的 JSON Schema 子集。"""

    schema = compact_tool_schema(tool)
    issues: list[dict[str, str]] = []
    _validate_schema_node(arguments, schema, "arguments", issues)
    return issues


def tool_validation_error_result(
    tool: ToolDefinition,
    issues: Sequence[Mapping[str, str]],
) -> ToolResult:
    """把 Host 二次校验失败转换为可供模型修正的结构化结果。"""

    return _error_result(
        "invalid_arguments",
        f"工具 {tool.name} 的参数未通过 Schema 校验。",
        tool_name=tool.name,
        retryable=True,
        extra={
            "issues": [dict(issue) for issue in issues],
            "contract": compact_tool_schema(tool),
        },
    )


def public_invoke_arguments(arguments: Mapping[str, Any] | Any) -> dict[str, Any]:
    """为未知或未通过校验的 invoke_tool 生成不包含参数值的公开投影。"""

    if not isinstance(arguments, Mapping):
        return {"invalid": True}
    result: dict[str, Any] = {
        "tool_name": str(arguments.get("tool_name") or "")[:200],
    }
    inner = arguments.get("arguments")
    if isinstance(inner, Mapping):
        result["argument_keys"] = sorted(str(key)[:100] for key in inner.keys())[:50]
        result["argument_count"] = len(inner)
    else:
        result["arguments_valid"] = False
    return result


def _compact_schema_node(value: Any, *, depth: int) -> Any:
    if depth > 6:
        return {"type": "object"}
    if isinstance(value, Mapping):
        allowed = {
            "type",
            "properties",
            "required",
            "additionalProperties",
            "items",
            "enum",
            "const",
            "oneOf",
            "anyOf",
            "allOf",
            "minLength",
            "maxLength",
            "minimum",
            "maximum",
            "minItems",
            "maxItems",
            "pattern",
        }
        return {
            str(key): (
                {
                    str(property_name): _compact_schema_node(property_schema, depth=depth + 1)
                    for property_name, property_schema in child.items()
                }
                if key == "properties" and isinstance(child, Mapping)
                else _compact_schema_node(child, depth=depth + 1)
            )
            for key, child in value.items()
            if key in allowed
        }
    if isinstance(value, list):
        return [_compact_schema_node(item, depth=depth + 1) for item in value[:20]]
    return value


def _validate_schema_node(
    value: Any,
    schema: Mapping[str, Any],
    path: str,
    issues: list[dict[str, str]],
) -> None:
    enum = schema.get("enum")
    if isinstance(enum, list) and value not in enum:
        issues.append({"path": path, "message": f"必须是以下值之一：{enum!r}"})
        return
    if "const" in schema and value != schema["const"]:
        issues.append({"path": path, "message": f"必须等于 {schema['const']!r}"})
        return

    alternatives = schema.get("oneOf") or schema.get("anyOf")
    if isinstance(alternatives, list) and alternatives:
        valid = False
        for alternative in alternatives:
            if not isinstance(alternative, Mapping):
                continue
            candidate: list[dict[str, str]] = []
            _validate_schema_node(value, alternative, path, candidate)
            if not candidate:
                valid = True
                break
        if not valid:
            issues.append({"path": path, "message": "不符合任一允许的参数结构。"})
        return

    expected_type = schema.get("type")
    if expected_type == "object":
        if not isinstance(value, Mapping):
            issues.append({"path": path, "message": "必须是 object。"})
            return
        properties = schema.get("properties")
        properties = properties if isinstance(properties, Mapping) else {}
        required = schema.get("required")
        if isinstance(required, list):
            for key in required:
                if key not in value:
                    issues.append({"path": f"{path}.{key}", "message": "缺少必填字段。"})
        if schema.get("additionalProperties") is False:
            extras = [key for key in value if key not in properties]
            for key in extras:
                issues.append({"path": f"{path}.{key}", "message": "不是声明的字段。"})
        for key, child_schema in properties.items():
            if key in value and isinstance(child_schema, Mapping):
                _validate_schema_node(value[key], child_schema, f"{path}.{key}", issues)
        return

    if expected_type == "array":
        if not isinstance(value, list):
            issues.append({"path": path, "message": "必须是 array。"})
            return
        _validate_number_bound(value, schema, path, issues, "minItems", "至少包含")
        _validate_number_bound(value, schema, path, issues, "maxItems", "最多包含")
        item_schema = schema.get("items")
        if isinstance(item_schema, Mapping):
            for index, item in enumerate(value):
                _validate_schema_node(item, item_schema, f"{path}[{index}]", issues)
        return

    if expected_type == "string":
        if not isinstance(value, str):
            issues.append({"path": path, "message": "必须是 string。"})
            return
        _validate_number_bound(value, schema, path, issues, "minLength", "长度至少为")
        _validate_number_bound(value, schema, path, issues, "maxLength", "长度最多为")
        pattern = schema.get("pattern")
        if isinstance(pattern, str):
            try:
                if re.search(pattern, value) is None:
                    issues.append({"path": path, "message": "不符合字段格式要求。"})
            except re.error:
                pass
        return

    if expected_type == "integer":
        if isinstance(value, bool) or not isinstance(value, int):
            issues.append({"path": path, "message": "必须是 integer。"})
            return
        _validate_number_bound(value, schema, path, issues, "minimum", "不能小于")
        _validate_number_bound(value, schema, path, issues, "maximum", "不能大于")
        return

    if expected_type == "number":
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            issues.append({"path": path, "message": "必须是 number。"})
            return
        _validate_number_bound(value, schema, path, issues, "minimum", "不能小于")
        _validate_number_bound(value, schema, path, issues, "maximum", "不能大于")
        return

    if expected_type == "boolean" and not isinstance(value, bool):
        issues.append({"path": path, "message": "必须是 boolean。"})
    elif expected_type == "null" and value is not None:
        issues.append({"path": path, "message": "必须是 null。"})


def _validate_number_bound(
    value: Any,
    schema: Mapping[str, Any],
    path: str,
    issues: list[dict[str, str]],
    key: str,
    message_prefix: str,
) -> None:
    bound = schema.get(key)
    comparable = (
        len(value)
        if key in {"minLength", "maxLength", "minItems", "maxItems"}
        else value
    )
    if isinstance(bound, (int, float)) and not isinstance(bound, bool):
        if key.startswith("min") and comparable < bound:
            issues.append({"path": path, "message": f"{message_prefix} {bound}。"})
        elif key.startswith("max") and comparable > bound:
            issues.append({"path": path, "message": f"{message_prefix} {bound}。"})
        elif key == "minimum" and comparable < bound:
            issues.append({"path": path, "message": f"{message_prefix} {bound}。"})
        elif key == "maximum" and comparable > bound:
            issues.append({"path": path, "message": f"{message_prefix} {bound}。"})


def _short_text(value: str, limit: int) -> str:
    text = str(value or "").strip()
    return text if len(text) <= limit else text[:limit] + "..."


def _bounded_int(value: Any, *, default: int, minimum: int, maximum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        return default
    return max(minimum, min(maximum, value))


def _bounded_json_result(payload: dict[str, Any], *, max_chars: int) -> ToolResult:
    text = json.dumps(payload, ensure_ascii=False, indent=2)
    if len(text) > max_chars:
        payload = dict(payload)
        if isinstance(payload.get("tools"), list):
            payload["tools"] = payload["tools"][:2]
            payload["count"] = len(payload["tools"])
        payload["truncated"] = True
        text = json.dumps(payload, ensure_ascii=False, indent=2)
    return ToolResult(ok=True, output=text, full_output=text)


def _error_result(
    code: str,
    message: str,
    *,
    tool_name: str = "",
    retryable: bool,
    extra: Mapping[str, Any] | None = None,
) -> ToolResult:
    payload: dict[str, Any] = {
        "schema_version": 1,
        "ok": False,
        "error": {
            "code": code,
            "message": message,
            "retryable": retryable,
        },
    }
    if tool_name:
        payload["tool_name"] = tool_name
    if extra:
        payload["error"].update(dict(extra))
    text = json.dumps(payload, ensure_ascii=False, indent=2)
    return ToolResult(ok=False, output=text, full_output=text)


__all__ = [
    "HostToolCatalog",
    "INVOKE_TOOL_NAME",
    "PreparedToolInvocation",
    "PROVIDER_TOOL_NAMES",
    "SEARCH_TOOLS_NAME",
    "build_provider_tools",
    "compact_tool_schema",
    "public_invoke_arguments",
    "tool_validation_error_result",
    "validate_tool_arguments",
]
