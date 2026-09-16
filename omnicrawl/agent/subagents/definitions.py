"""Markdown Agent 定义解析、来源发现、优先级覆盖和诊断。"""

from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping, Sequence


_NAME_PATTERN = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")
_ALLOWED_FIELDS = {
    "name",
    "description",
    "tools",
    "disallowedTools",
    "model",
    # 兼容旧角色定义：这两个字段不再参与运行预算，存在时仅忽略。
    "maxTurns",
    "maxToolCalls",
    "permissionMode",
    "background",
    "isolation",
    "skills",
    "mcpServers",
    # 结构化 git 工具的暴露档位：readonly（只读包装，默认）或 full（完整
    # 子命令 + 自动批准，供评审等需要完整 git 上下文的角色使用）。
    "gitMode",
}
_ALLOWED_PERMISSION_MODES = {
    "delegated-read-only",
    "explicit-command-allowlist",
    "standard",
}
_ALLOWED_GIT_MODES = {"readonly", "full"}
_ALLOWED_ISOLATIONS = {"shared", "worktree"}
_MAX_DEFINITION_FILE_BYTES = 256 * 1024
_MAX_SYSTEM_PROMPT_CHARS = 64_000
_MAX_LIST_ITEMS = 64
_MAX_LIST_ITEM_CHARS = 128


class AgentDefinitionError(RuntimeError):
    """单个 Agent 定义读取或校验失败。"""


@dataclass(frozen=True)
class AgentDefinition:
    """创建子执行时冻结的角色定义快照。"""

    name: str
    description: str
    system_prompt: str = ""
    tools: tuple[str, ...] = ()
    disallowed_tools: tuple[str, ...] = ()
    model: str = "inherit"
    permission_mode: str = "delegated-read-only"
    background: bool = False
    isolation: str = "shared"
    skills: tuple[str, ...] = ()
    mcp_servers: tuple[str, ...] = ()
    # 结构化 git 工具档位：readonly（只读包装）或 full（完整子命令 + 自动批准）。
    git_mode: str = "readonly"
    source_path: Path | None = None
    source: str = "builtin"


@dataclass(frozen=True)
class AgentDefinitionDiagnostic:
    """发现阶段的无效文件或同名覆盖诊断。"""

    kind: str
    message: str
    path: str
    winner_path: str = ""
    loser_path: str = ""


class AgentDefinitionRegistry:
    """按“离项目越近优先级越高”规则构建不可变定义索引。

    扫描顺序从高到低，首次出现的名称获胜：项目 `.omnicrawl`、项目兼容
    `.agents`、用户、包内内置、已批准插件。插件只能提供最低优先级定义，
    不能覆盖 Host 或用户配置。
    """

    def __init__(
        self,
        *,
        builtin_directory: Path | None = None,
        home_directory: Path | None = None,
    ) -> None:
        self._builtin_directory = Path(
            builtin_directory
            or Path(__file__).resolve().parents[2] / "templates" / "subagents"
        ).resolve()
        self._home_directory = Path(home_directory or Path.home()).expanduser().resolve()
        self._index: dict[str, AgentDefinition] = {}
        self._diagnostics: list[AgentDefinitionDiagnostic] = []
        self._real_paths: set[str] = set()

    @property
    def diagnostics(self) -> tuple[AgentDefinitionDiagnostic, ...]:
        return tuple(self._diagnostics)

    def discover(
        self,
        workspace_root: Path,
        *,
        plugin_definitions: Sequence[tuple[str, Path]] = (),
    ) -> None:
        """重新发现当前工作区定义；已启动任务应继续使用旧定义快照。"""

        workspace = Path(workspace_root).resolve()
        self._index.clear()
        self._diagnostics.clear()
        self._real_paths.clear()

        directories = (
            ("project", workspace / ".omnicrawl" / "agents"),
            ("project-compat", workspace / ".agents" / "agents"),
            ("user", self._home_directory / ".OmniCrawl" / "agents"),
            ("builtin", self._builtin_directory),
        )
        for source, directory in directories:
            self._scan_directory(directory, source)

        for plugin_name, path in plugin_definitions:
            self._load_path(Path(path), f"plugin:{plugin_name}")

    def get(self, name: str) -> AgentDefinition | None:
        return self._index.get(str(name).strip().casefold())

    def list_all(self) -> list[AgentDefinition]:
        return sorted(self._index.values(), key=lambda item: item.name)

    def _scan_directory(self, directory: Path, source: str) -> None:
        if not directory.is_dir():
            return
        try:
            paths = sorted(
                (item for item in directory.glob("*.md") if item.is_file()),
                key=lambda item: item.name.casefold(),
            )
        except OSError as exc:
            self._diagnostics.append(
                AgentDefinitionDiagnostic(
                    kind="invalid",
                    message=f"扫描 Agent 定义目录失败：{exc}",
                    path=str(directory),
                )
            )
            return
        try:
            allowed_root = directory.resolve(strict=True)
        except OSError as exc:
            self._diagnostics.append(
                AgentDefinitionDiagnostic(
                    kind="invalid",
                    message=f"解析 Agent 定义来源目录失败：{exc}",
                    path=str(directory),
                )
            )
            return
        for path in paths:
            self._load_path(path, source, allowed_root=allowed_root)

    def _load_path(
        self,
        path: Path,
        source: str,
        *,
        allowed_root: Path | None = None,
    ) -> None:
        try:
            resolved = path.expanduser().resolve(strict=True)
        except OSError as exc:
            self._diagnostics.append(
                AgentDefinitionDiagnostic(
                    kind="invalid",
                    message=f"Agent 定义路径不存在或无法解析：{exc}",
                    path=str(path),
                )
            )
            return
        real_key = str(resolved)
        if allowed_root is not None and not _is_relative_to(resolved, allowed_root):
            self._diagnostics.append(
                AgentDefinitionDiagnostic(
                    kind="invalid",
                    message="Agent 定义解析后超出声明的来源目录，已拒绝。",
                    path=str(path),
                )
            )
            return
        if real_key in self._real_paths:
            return
        self._real_paths.add(real_key)

        try:
            definition = parse_agent_definition(resolved, source=source)
        except AgentDefinitionError as exc:
            self._diagnostics.append(
                AgentDefinitionDiagnostic(
                    kind="invalid",
                    message=str(exc),
                    path=real_key,
                )
            )
            return

        existing = self._index.get(definition.name)
        if existing is not None:
            winner_path = str(existing.source_path or existing.source)
            self._diagnostics.append(
                AgentDefinitionDiagnostic(
                    kind="collision",
                    message=f'Agent 定义名称 "{definition.name}" 冲突，保留高优先级来源。',
                    path=real_key,
                    winner_path=winner_path,
                    loser_path=real_key,
                )
            )
            return
        self._index[definition.name] = definition


def parse_agent_definition(path: Path, *, source: str) -> AgentDefinition:
    """解析 YAML frontmatter + Markdown body，不复用仅支持标量的 Skill 解析器。"""

    file_path = Path(path)
    try:
        file_size = file_path.stat().st_size
    except OSError as exc:
        raise AgentDefinitionError(f"读取 Agent 定义元数据失败：{file_path}，{exc}") from exc
    if file_size > _MAX_DEFINITION_FILE_BYTES:
        raise AgentDefinitionError(
            f"Agent 定义文件大小超过 {_MAX_DEFINITION_FILE_BYTES} 字节：{file_path}"
        )
    try:
        text = file_path.read_text(encoding="utf-8-sig")
    except (OSError, UnicodeError) as exc:
        raise AgentDefinitionError(f"读取 Agent 定义失败：{file_path}，{exc}") from exc

    frontmatter_text, body = _split_frontmatter(text, file_path)
    if len(body) > _MAX_SYSTEM_PROMPT_CHARS:
        raise AgentDefinitionError(
            f"Agent 定义正文超过 {_MAX_SYSTEM_PROMPT_CHARS} 字符：{file_path}"
        )
    try:
        import yaml
    except ImportError as exc:
        raise AgentDefinitionError("缺少 PyYAML，无法解析 Agent 定义。") from exc
    try:
        raw = yaml.safe_load(frontmatter_text)
    except yaml.YAMLError as exc:
        raise AgentDefinitionError(f"Agent 定义 YAML 解析失败：{file_path}，{exc}") from exc
    if not isinstance(raw, Mapping):
        raise AgentDefinitionError(f"Agent 定义 frontmatter 必须是对象：{file_path}")

    unknown = sorted(set(raw) - _ALLOWED_FIELDS)
    if unknown:
        raise AgentDefinitionError(
            f"Agent 定义包含未知字段：{', '.join(map(str, unknown))}（{file_path}）"
        )

    name = _required_string(raw, "name", file_path, max_length=64).casefold()
    if not _NAME_PATTERN.fullmatch(name):
        raise AgentDefinitionError(
            f"Agent 定义 name 只能使用小写字母、数字和单连字符：{name}（{file_path}）"
        )
    description = _required_string(raw, "description", file_path, max_length=300)
    tools = _string_tuple(raw, "tools", file_path)
    disallowed_tools = _string_tuple(raw, "disallowedTools", file_path)
    model = _optional_string(raw, "model", "inherit", file_path, max_length=200)
    permission_mode = _optional_string(
        raw,
        "permissionMode",
        "delegated-read-only",
        file_path,
        max_length=64,
    )
    if permission_mode not in _ALLOWED_PERMISSION_MODES:
        raise AgentDefinitionError(
            f"Agent 定义 permissionMode 不受支持：{permission_mode}（{file_path}）"
        )
    background = _bool_value(raw, "background", False, file_path)
    isolation = _optional_string(raw, "isolation", "shared", file_path, max_length=32)
    if isolation not in _ALLOWED_ISOLATIONS:
        raise AgentDefinitionError(f"Agent 定义 isolation 不受支持：{isolation}（{file_path}）")
    git_mode = _optional_string(raw, "gitMode", "readonly", file_path, max_length=32)
    if git_mode not in _ALLOWED_GIT_MODES:
        raise AgentDefinitionError(
            f"Agent 定义 gitMode 不受支持：{git_mode}（{file_path}）"
        )

    return AgentDefinition(
        name=name,
        description=description,
        system_prompt=body.strip(),
        tools=tools,
        disallowed_tools=disallowed_tools,
        model=model,
        permission_mode=permission_mode,
        background=background,
        isolation=isolation,
        skills=_string_tuple(raw, "skills", file_path),
        mcp_servers=_string_tuple(raw, "mcpServers", file_path),
        git_mode=git_mode,
        source_path=file_path.resolve(),
        source=source,
    )


def _split_frontmatter(text: str, path: Path) -> tuple[str, str]:
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        raise AgentDefinitionError(f"Agent 定义缺少 YAML frontmatter：{path}")
    closing_index = None
    for index in range(1, len(lines)):
        if lines[index].strip() == "---":
            closing_index = index
            break
    if closing_index is None:
        raise AgentDefinitionError(f"Agent 定义 frontmatter 未闭合：{path}")
    return "\n".join(lines[1:closing_index]), "\n".join(lines[closing_index + 1 :])


def _required_string(
    raw: Mapping[str, Any],
    name: str,
    path: Path,
    *,
    max_length: int,
) -> str:
    value = raw.get(name)
    if not isinstance(value, str) or not value.strip():
        raise AgentDefinitionError(f"Agent 定义 {name} 必须是非空字符串：{path}")
    result = value.strip()
    if len(result) > max_length:
        raise AgentDefinitionError(
            f"Agent 定义 {name} 最长 {max_length} 字符，当前 {len(result)}：{path}"
        )
    return result


def _optional_string(
    raw: Mapping[str, Any],
    name: str,
    default: str,
    path: Path,
    *,
    max_length: int,
) -> str:
    if name not in raw:
        return default
    return _required_string(raw, name, path, max_length=max_length)


def _string_tuple(raw: Mapping[str, Any], name: str, path: Path) -> tuple[str, ...]:
    value = raw.get(name, [])
    if value is None:
        return ()
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        raise AgentDefinitionError(f"Agent 定义 {name} 必须是字符串数组：{path}")
    if len(value) > _MAX_LIST_ITEMS:
        raise AgentDefinitionError(
            f"Agent 定义 {name} 最多 {_MAX_LIST_ITEMS} 项：{path}"
        )
    stripped_items = [item.strip() for item in value if item.strip()]
    if any(len(item) > _MAX_LIST_ITEM_CHARS for item in stripped_items):
        raise AgentDefinitionError(
            f"Agent 定义 {name} 的单项最长 {_MAX_LIST_ITEM_CHARS} 字符：{path}"
        )
    normalized = tuple(dict.fromkeys(stripped_items))
    if len(normalized) != len(stripped_items):
        raise AgentDefinitionError(f"Agent 定义 {name} 不允许重复项：{path}")
    return normalized


def _bool_value(raw: Mapping[str, Any], name: str, default: bool, path: Path) -> bool:
    value = raw.get(name, default)
    if not isinstance(value, bool):
        raise AgentDefinitionError(f"Agent 定义 {name} 必须是布尔值：{path}")
    return value


def _is_relative_to(path: Path, root: Path) -> bool:
    try:
        path.relative_to(root)
        return True
    except ValueError:
        return False
