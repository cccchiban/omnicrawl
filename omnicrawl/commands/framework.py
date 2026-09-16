"""声明式斜杠命令框架：注册、解析、执行。

设计约定
--------
业务模块只声明命令的 *元数据 + 处理器*（``Command``），框架负责其余一切：

- **解析**：把一行输入拆成命令名与参数（``CommandRegistry.parse``），
  兼容 ``/name args`` 与无斜杠的整词别名（如「退出」）。
- **注册**：按名与别名建立索引（``register`` / ``command`` 装饰器），
  注册期即检测重名与别名冲突，避免运行期出现「命令被后注册者静默覆盖」。
- **分发**：``dispatch`` 命中命令后构造 :class:`CommandContext` 并调用处理器，
  未命中返回 :data:`UNHANDLED`，调用方据此决定是否按普通对话处理。
- **列表**：``display_names`` / ``options`` / ``help_text`` 由同一份声明派生，
  帮助、补全菜单与真实可执行命令不再各自硬编码。

本模块不依赖 omnicrawl 其他子系统，便于单测与复用；命令类型只表达调度语义，
远端/本地等通道差异由处理器读取 :attr:`CommandContext.channel` 自行判断。
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from typing import Any, Callable, Sequence

__all__ = [
    "Command",
    "CommandContext",
    "CommandParseError",
    "CommandRegistry",
    "CommandResult",
    "CommandType",
    "ParsedCommand",
    "UNHANDLED",
]


class CommandType(str, Enum):
    """命令类型：决定框架与交互端默认的调度方式。

    ``immediate`` 为真时，交互端允许在 Agent 回合进行中立即执行该命令；
    否则必须排队，避免与后台回合并发修改状态。
    """

    #: 纯界面动作（开关面板、退出）：无业务副作用。
    UI = "ui"
    #: 只读查询：不修改任何状态。
    QUERY = "query"
    #: 状态变更（会话、配置、工作区）：需要排队。
    ACTION = "action"
    #: 网络/进程/文件 I/O 或模型循环：交互端应交给慢命令 worker。
    BACKGROUND = "background"

    @property
    def immediate(self) -> bool:
        """是否允许在 Agent 回合进行中立即执行。"""

        return self in _IMMEDIATE_TYPES


_IMMEDIATE_TYPES = frozenset({CommandType.UI, CommandType.QUERY})


class CommandParseError(ValueError):
    """命令声明非法（空命令名、重名或别名冲突）。"""


@dataclass(frozen=True)
class ParsedCommand:
    """一行输入的解析结果。

    ``matched`` 保存真正命中的名字或别名（已归一化、不含前导斜杠），
    处理器需要区分别名语义时可读取它。
    """

    command: "Command"
    args: str = ""
    argv: tuple[str, ...] = ()
    matched: str = ""


@dataclass(frozen=True)
class CommandResult:
    """一次命令执行的结构化结果。

    ``message``/``error`` 是面向用户的文本；``deferred`` 用于把网络、进程、
    文件 I/O 或模型循环推迟到交互端的工作线程执行（见
    ``omnicrawl.ui.fullscreen`` 的慢命令 worker）。其余字段是给交互层的调度
    提示，非交互入口（如连接器）可以直接忽略。
    """

    handled: bool = True
    message: str | None = None
    error: str | None = None
    data: Any = None
    # —— 交互层调度提示 ——
    refresh_context: bool = False
    exit_requested: bool = False
    open_settings: bool = False
    open_config_chat: bool = False
    clear_conversation: bool = False
    replay_conversation: bool = False
    workspace_switch_requested: bool = False
    stream_subagent_conversation: bool = False
    working_status: str | None = None
    #: 延迟执行：交互端应在线程 worker 中调用它拿到真正结果。
    deferred: Callable[[], "CommandResult"] | None = None

    @property
    def ok(self) -> bool:
        """是否成功；``error`` 为空即视为成功。"""

        return self.error is None

    def resolve(self) -> "CommandResult":
        """执行延迟部分；无延迟时返回自身。

        连接器等同步入口可直接调用本方法把慢命令当普通命令执行。
        """

        if self.deferred is None:
            return self
        return self.deferred()


UNHANDLED = CommandResult(handled=False)


@dataclass(frozen=True)
class CommandContext:
    """命令执行上下文：处理器所需的输入与运行环境。

    ``agent`` 是命令操作的宿主对象（通常是 ``LocalToolAgent``，也可能是
    测试替身）。``channel`` 标识来源入口（``tui`` / ``telegram`` / ``fsapp``），
    处理器据此落实通道能力差异。
    """

    agent: Any
    command: "Command | None" = None
    raw: str = ""
    args: str = ""
    argv: tuple[str, ...] = ()
    channel: str = "tui"
    #: 派生 SubAgent 任务（如 /review）的进度事件回调，可选。
    on_subagent_event: Callable[[str, dict[str, Any]], None] | None = None

    @property
    def is_remote(self) -> bool:
        """是否来自远程连接器（飞书 / Telegram）。"""

        return self.channel in _REMOTE_CHANNELS

    @property
    def arg(self) -> str:
        """首个参数，无参数时为空串。"""

        return self.argv[0] if self.argv else ""


_REMOTE_CHANNELS = frozenset({"telegram", "feishu", "fsapp", "remote"})

Handler = Callable[[CommandContext], CommandResult]


@dataclass(frozen=True)
class Command:
    """一条可分发命令的声明。

    字段与设计文档一致：``name`` / ``aliases`` / ``description`` / ``usage`` /
    ``type`` / ``arg_prompt`` / ``hidden`` / ``handler``。``handler`` 因无默认值
    在 dataclass 中需排在默认字段之前，语义顺序不受影响。

    另有三个纯展示用扩展字段，用于表达本仓库既有命令的补全形态与参数提示：

    - ``completions``：共享同一处理器的额外补全/帮助形态（纯展示，不参与分发）。
    - ``usage`` 缺省时由 ``name`` 生成 ``/name``。
    - ``parameters``：可选参数 ``(参数, 一句话说明)``；交互端在命令名后提示并补全
      ``--chat`` 这类开关参数，处理器自行解析 ``ctx.args``。
    """

    name: str
    handler: Handler
    aliases: tuple[str, ...] = ()
    description: str = ""
    usage: str = ""
    type: CommandType = CommandType.QUERY
    arg_prompt: str | None = None
    hidden: bool = False
    completions: tuple[str, ...] = ()
    parameters: tuple[tuple[str, str], ...] = ()

    def __post_init__(self) -> None:
        if not _normalize(self.name):
            raise CommandParseError("命令名不能为空。")

    @property
    def display(self) -> str:
        """帮助与补全中展示的命令串。"""

        return f"/{self.name}"

    @property
    def effective_usage(self) -> str:
        """用法文本；未声明时回退为 ``/name``。"""

        return self.usage or self.display

    @property
    def takes_argument(self) -> bool:
        """是否接受参数（决定补全插入后是否补空格）。"""

        return self.arg_prompt is not None


def _normalize(token: str) -> str:
    """归一化命令名/别名：去前导斜杠与空白、大小写折叠。"""

    return token.strip().lstrip("/").strip().casefold()


class CommandRegistry:
    """命令注册表：唯一的命令事实来源。

    典型用法（声明式注册）::

        REGISTRY = CommandRegistry()

        @REGISTRY.command(
            name="compact",
            aliases=("c",),
            description="压缩当前会话上下文。",
            usage="/compact",
            type=CommandType.BACKGROUND,
        )
        def handle_compact(ctx: CommandContext) -> CommandResult:
            ...
    """

    def __init__(self) -> None:
        self._commands: dict[str, Command] = {}
        self._aliases: dict[str, str] = {}

    # ── 注册 ──────────────────────────────────────────────────

    def register(self, command: Command) -> Command:
        """注册一条命令；重名或别名冲突立即报错，避免静默覆盖。"""

        key = _normalize(command.name)
        if key in self._commands or key in self._aliases:
            raise CommandParseError(f"命令重复注册：{command.name}")

        alias_keys: list[tuple[str, str]] = []
        seen_aliases: set[str] = set()
        for alias in command.aliases:
            alias_key = _normalize(alias)
            if not alias_key:
                raise CommandParseError(f"命令 {command.name} 含空别名。")
            if alias_key == key or alias_key in seen_aliases:
                raise CommandParseError(f"命令 {command.name} 的别名冲突：{alias}")
            if alias_key in self._commands or alias_key in self._aliases:
                raise CommandParseError(f"命令 {command.name} 的别名冲突：{alias}")
            seen_aliases.add(alias_key)
            alias_keys.append((alias_key, alias))

        self._commands[key] = command
        for alias_key, _alias in alias_keys:
            self._aliases[alias_key] = key
        return command

    def command(
        self,
        name: str,
        *,
        aliases: Sequence[str] = (),
        description: str = "",
        usage: str = "",
        type: CommandType = CommandType.QUERY,
        arg_prompt: str | None = None,
        hidden: bool = False,
        completions: Sequence[str] = (),
        parameters: Sequence[tuple[str, str]] = (),
    ) -> Callable[[Handler], Handler]:
        """装饰器工厂：声明并注册一条命令，返回原函数。"""

        def decorator(handler: Handler) -> Handler:
            self.register(
                Command(
                    name=name,
                    handler=handler,
                    aliases=tuple(aliases),
                    description=description,
                    usage=usage,
                    type=type,
                    arg_prompt=arg_prompt,
                    hidden=hidden,
                    completions=tuple(completions),
                    parameters=tuple(parameters),
                )
            )
            return handler

        return decorator

    # ── 解析 ──────────────────────────────────────────────────

    def resolve(self, token: str) -> Command | None:
        """按命令名或别名解析；未注册时返回 None。"""

        key = _normalize(token)
        if not key:
            return None
        command = self._commands.get(key)
        if command is not None:
            return command
        canonical = self._aliases.get(key)
        if canonical is None:
            return None
        return self._commands.get(canonical)

    def parse(self, text: str) -> ParsedCommand | None:
        """解析一行输入；不是已注册命令时返回 None。

        ``/name args`` 与无斜杠的整词别名都支持；无斜杠输入不接受参数，
        因此 ``普通对话文本`` 不会被误判成命令。
        """

        stripped = text.strip()
        if not stripped:
            return None
        if stripped.startswith("/"):
            head, _, tail = stripped[1:].strip().partition(" ")
            token = head
            args = tail.strip()
        else:
            # 无斜杠：仅整词匹配别名（如「退出」），避免把普通输入当命令。
            token = stripped
            args = ""
        command = self.resolve(token)
        if command is None:
            return None
        return ParsedCommand(
            command=command,
            args=args,
            argv=tuple(args.split()),
            matched=_normalize(token),
        )

    # ── 执行 ──────────────────────────────────────────────────

    def dispatch(
        self,
        text: str,
        *,
        agent: Any,
        channel: str = "tui",
        on_subagent_event: Callable[[str, dict[str, Any]], None] | None = None,
    ) -> CommandResult:
        """解析并分发一条输入；未命中原样返回 :data:`UNHANDLED`。"""

        parsed = self.parse(text)
        if parsed is None:
            return UNHANDLED
        return self.invoke(
            parsed,
            agent=agent,
            raw=text.strip(),
            channel=channel,
            on_subagent_event=on_subagent_event,
        )

    def invoke(
        self,
        parsed: ParsedCommand,
        *,
        agent: Any,
        raw: str = "",
        channel: str = "tui",
        on_subagent_event: Callable[[str, dict[str, Any]], None] | None = None,
    ) -> CommandResult:
        """调用已解析命令的处理器，并规范化其返回值。"""

        ctx = CommandContext(
            agent=agent,
            command=parsed.command,
            raw=raw or parsed.command.display,
            args=parsed.args,
            argv=parsed.argv,
            channel=channel,
            on_subagent_event=on_subagent_event,
        )
        result = parsed.command.handler(ctx)
        if result is None:
            # 处理器无输出时视为「已处理、无消息」，避免调用方额外分支。
            return CommandResult()
        if not isinstance(result, CommandResult):
            raise TypeError(
                f"命令 {parsed.command.name} 的处理器必须返回 CommandResult，"
                f"实际为 {type(result).__name__}。"
            )
        return result

    # ── 列表与元数据 ──────────────────────────────────────────

    def commands(self, *, include_hidden: bool = True) -> list[Command]:
        """按注册顺序返回命令列表。"""

        return [
            command
            for command in self._commands.values()
            if include_hidden or not command.hidden
        ]

    def display_names(self, *, include_hidden: bool = True) -> list[str]:
        """返回所有可输入的命令串（含补全形态），用于 Tab 补全。"""

        names: list[str] = []
        for command in self.commands(include_hidden=include_hidden):
            names.append(command.display)
            names.extend(command.completions)
        return names

    def options(self, *, include_hidden: bool = False) -> list[dict[str, Any]]:
        """构造交互端菜单元数据；结构沿用既有 TUI 契约。

        ``parameters`` 是命令声明的可选参数 ``(参数, 说明)``，供输入框在命令名后
        提示并补全 ``--chat`` 这类开关；其余键保持既有字符串形状。
        """

        options: list[dict[str, Any]] = []
        for command in self.commands(include_hidden=include_hidden):
            for display in (command.display, *command.completions):
                options.append(
                    {
                        "command": display,
                        "insert": display,
                        "title": display,
                        "description": command.description or "执行斜杠命令。",
                        "category": "命令",
                        "search": display,
                        "parameters": command.parameters,
                    }
                )
        return options

    def help_text(self, *, include_hidden: bool = False) -> str:
        """渲染帮助列表；``hidden`` 命令默认不出现。"""

        lines = ["可用命令："]
        for command in self.commands(include_hidden=include_hidden):
            lines.append(f"  {command.effective_usage:<30} {command.description}")
            for extra in command.completions:
                lines.append(f"  {extra:<30} {command.description}")
        return "\n".join(lines)
