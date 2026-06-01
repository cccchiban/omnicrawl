from __future__ import annotations

import json
import os
import re
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

from llm import LLMConfig, OpenAIResponseLLM, VALID_REASONING_EFFORTS, load_llm_config
from skill import SkillManager, SkillMatchResult


AGENTS_INSTRUCTIONS_FILE = "AGENTS.md"


class AgentError(RuntimeError):
    """Agent 循环、工具调用或安全校验失败时抛出。"""


class UserDeclinedOperation(AgentError):
    """用户在确认弹窗中选择 NO 时抛出。"""


@dataclass(frozen=True)
class ToolCall:
    """模型请求执行的一次工具调用。"""

    name: str
    arguments: dict[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class ToolResult:
    """工具调用返回给模型的结构化结果。"""

    ok: bool
    output: str


@dataclass(frozen=True)
class ToolDefinition:
    """Agent 可用工具的说明与执行函数。"""

    name: str
    description: str
    argument_schema: str
    requires_confirmation: bool
    run: Callable[[dict[str, Any]], ToolResult]


class _AgentReplyStreamer:
    """流式输出显式最终回复，同时避免工具调用前言打乱对话顺序。"""

    _OPEN_TAG = "<final>"
    _CLOSE_TAG = "</final>"

    def __init__(self, on_delta: Callable[[str], None]) -> None:
        self._on_delta = on_delta
        self._prefix_buffer = ""
        self._tail_buffer = ""
        self._inside_final = False
        self._closed = False
        self.streamed = False

    def push(self, delta: str) -> None:
        if self._closed or not delta:
            return

        if not self._inside_final:
            self._prefix_buffer += delta
            stripped = self._prefix_buffer.lstrip()
            lowered = stripped.lower()
            if not stripped:
                return

            if lowered.startswith(self._OPEN_TAG):
                self._inside_final = True
                self._push_final_text(stripped[len(self._OPEN_TAG) :])
                self._prefix_buffer = ""
                return

            if self._OPEN_TAG.startswith(lowered) or "<tool>".startswith(lowered):
                return

            first_char = stripped[0]
            if first_char in {"<", "{"}:
                self._closed = True
                self._prefix_buffer = ""
                return

            # 普通文本可能只是工具调用前的说明，例如“好的，开始安装。”后面紧跟
            # <tool>。这里先暂存不输出，等本次模型回复完整返回后，由 Agent
            # 判断它不是工具调用时再作为最终回答展示，避免 TUI 中助手消息提前占位，
            # 导致后续工具请求记录显示在最终回复下方。
            return

        self._push_final_text(delta)

    def finish(self) -> None:
        if self._inside_final and not self._closed and self._tail_buffer:
            self._emit(self._tail_buffer)
            self._tail_buffer = ""

    def _push_final_text(self, text: str) -> None:
        self._tail_buffer += text
        close_index = self._tail_buffer.lower().find(self._CLOSE_TAG)
        if close_index >= 0:
            self._emit(self._tail_buffer[:close_index])
            self._tail_buffer = ""
            self._closed = True
            return

        keep_chars = len(self._CLOSE_TAG) - 1
        if len(self._tail_buffer) <= keep_chars:
            return

        self._emit(self._tail_buffer[:-keep_chars])
        self._tail_buffer = self._tail_buffer[-keep_chars:]

    def _emit(self, text: str) -> None:
        if not text:
            return
        self.streamed = True
        self._on_delta(text)


def _read_int_env(name: str, default: int, *, min_value: int, max_value: int) -> int:
    """读取整数环境变量，并把配置错误转成 Agent 可捕获的中文错误。"""

    raw_value = os.getenv(name)
    if raw_value is None or raw_value.strip() == "":
        return default

    try:
        value = int(raw_value.strip())
    except ValueError as exc:
        raise AgentError(
            f"{name} 必须是 {min_value} 到 {max_value} 的整数，当前值：{raw_value}。"
        ) from exc

    return _validate_int_range(name, value, min_value=min_value, max_value=max_value)


def _validate_int_range(name: str, value: int, *, min_value: int, max_value: int) -> int:
    """校验整数范围，覆盖测试或调用方手动构造 AgentConfig 的情况。"""

    if isinstance(value, bool) or not isinstance(value, int):
        raise AgentError(f"{name} 必须是 {min_value} 到 {max_value} 的整数。")
    if value < min_value or value > max_value:
        raise AgentError(f"{name} 必须是 {min_value} 到 {max_value} 的整数，当前值：{value}。")
    return value


@dataclass
class AgentConfig:
    """本地 Agent 配置。

    workspace_root 约束所有文件工具的访问范围，避免模型误读或误写项目外路径。
    max_steps 控制单轮任务最多可连续调用多少次模型/工具，防止协议异常时无限循环。
    """

    llm: LLMConfig = field(default_factory=load_llm_config)
    workspace_root: Path = field(default_factory=lambda: Path.cwd())
    max_steps: int = field(
        default_factory=lambda: _read_int_env("AGENT_MAX_STEPS", 16, min_value=1, max_value=100)
    )
    max_history_turns: int = 6
    max_tool_output_chars: int = 6000
    skills_enabled: bool = True
    skill_paths: list[str] = field(default_factory=list)
    command_timeout_seconds: int = field(
        default_factory=lambda: _read_int_env(
            "AGENT_COMMAND_TIMEOUT_SECONDS", 120, min_value=1, max_value=300
        )
    )

    def __post_init__(self) -> None:
        self.max_steps = _validate_int_range(
            "AGENT_MAX_STEPS", self.max_steps, min_value=1, max_value=100
        )
        self.command_timeout_seconds = _validate_int_range(
            "AGENT_COMMAND_TIMEOUT_SECONDS",
            self.command_timeout_seconds,
            min_value=1,
            max_value=300,
        )


class LocalToolAgent:
    """能在本地项目内读文件、检索、按确认执行写入/命令的简化 Agent Harness。

    参考 pi 的核心思想：Agent 不是一次问答，而是"模型 -> 工具 -> 观察 -> 下一轮模型"的循环。
    当前实现选择文本协议而不是原生 Responses tool call，是为了兼容课程网关可能只实现
    OpenAI Responses 的文本流部分这一现实约束。
    """

    _TOOL_PATTERN = re.compile(r"<tool>\s*(.*?)\s*</tool>", re.DOTALL | re.IGNORECASE)
    _FINAL_PATTERN = re.compile(r"<final>\s*(.*?)\s*</final>", re.DOTALL | re.IGNORECASE)

    def __init__(
        self,
        config: AgentConfig | None = None,
        confirm: Callable[[str, dict[str, Any]], bool] | None = None,
    ) -> None:
        self.config = config or AgentConfig()
        self.workspace_root = self.config.workspace_root.resolve()
        self._confirm = confirm or self._confirm_in_terminal
        self._history: list[dict[str, str]] = []
        self._pending_task_messages: list[dict[str, str]] | None = None
        self._tools = self._build_tools()
        self._agents_instructions = self._load_agents_instructions()
        self._skill_manager: SkillManager | None = None
        self._active_skills: list[SkillMatchResult] = []
        if self.config.skills_enabled:
            self._skill_manager = SkillManager()
            self._skill_manager.discover(
                cwd=self.workspace_root,
                extra_paths=self.config.skill_paths,
            )

        if not self.config.llm.api_key.strip():
            raise AgentError("缺少 API Key，请在 config.json 的 llm.api_key 中配置，或设置 OPENAI_API_KEY。")

        try:
            from openai import OpenAI
        except ImportError as exc:
            raise AgentError("缺少 openai 依赖，请先执行：pip install -r requirements.txt") from exc

        self._client = OpenAI(
            api_key=self.config.llm.api_key,
            base_url=self.config.llm.base_url,
        )

    @property
    def skill_manager(self) -> SkillManager | None:
        """公开 SkillManager 供 main.py 查询 /skills 列表。"""
        return self._skill_manager

    def set_confirm_handler(self, confirm: Callable[[str, dict[str, Any]], bool]) -> None:
        """替换确认交互，便于全屏 TUI 和行内 UI 使用不同展示方式。"""

        self._confirm = confirm

    def run_stream(
        self,
        user_text: str,
        on_delta: Callable[[str], None],
        on_status: Callable[[str], None] | None = None,
    ) -> str:
        """执行一轮 Agent 任务，并把最终回答交给 on_delta 输出。

        工具调用过程通过 on_status 报告给命令行；最终回答仍走 on_delta，让现有 TTS
        分句播报逻辑可以继续复用。
        """

        text = user_text.strip()
        if not text:
            raise AgentError("用户输入为空，无法发送给 Agent。")

        status = on_status or (lambda _message: None)

        # 处理 /skill:name 命令
        self._active_skills = []
        if self._skill_manager is not None:
            if text.startswith("/skill:"):
                parts = text.split(None, 1)
                skill_name = parts[0][len("/skill:"):].strip()
                skill = self._skill_manager.match_by_name(skill_name)
                if skill is not None:
                    self._active_skills = [
                        SkillMatchResult(skill=skill, score=1.0, reason=f"手动调用：{skill_name}")
                    ]
                    status(f"已加载 Skill：{skill_name}")
                    text = parts[1] if len(parts) > 1 else f"请执行 {skill_name} 技能。"
                else:
                    status(f"未找到 Skill：{skill_name}")
                    available = ", ".join(m.name for m in self._skill_manager.list_all()) or "无"
                    text = f"Skill「{skill_name}」不存在。当前可用的 Skill：{available}"

        if self._pending_task_messages and self._is_continue_request(text):
            working_messages = [
                *self._pending_task_messages,
                {"role": "user", "content": "请继续处理上一个未完成任务。"},
            ]
        else:
            self._pending_task_messages = None
            working_messages = [*self._history, {"role": "user", "content": text}]

        all_reasoning_parts: list[str] = []
        has_tool_calls = False
        for step in range(1, self.config.max_steps + 1):
            raw_reply, reasoning, streamed_final = self._request_agent_reply(working_messages, on_delta)
            if reasoning:
                all_reasoning_parts.append(reasoning)
            tool_call = self._parse_tool_call(raw_reply)
            if tool_call is None:
                final_reply = self._parse_final_reply(raw_reply)
                if not streamed_final:
                    on_delta(final_reply)
                self._pending_task_messages = None
                combined_reasoning = "\n".join(all_reasoning_parts) if has_tool_calls else ""
                self._append_history(text, final_reply, combined_reasoning)
                return final_reply

            has_tool_calls = True
            tool = self._tools.get(tool_call.name)
            if tool is None:
                tool_result = ToolResult(
                    ok=False,
                    output=f"未知工具：{tool_call.name}。可用工具：{', '.join(self._tools)}",
                )
            else:
                status(f"Agent 第 {step} 步请求工具：{tool_call.name}")
                tool_result = self._run_tool(tool, tool_call.arguments)

            assistant_msg: dict[str, str] = {"role": "assistant", "content": raw_reply}
            if reasoning:
                assistant_msg["reasoning_content"] = reasoning
            working_messages.extend(
                [
                    assistant_msg,
                    {
                        "role": "user",
                        "content": self._format_tool_observation(tool_call, tool_result),
                    },
                ]
            )

        final_reply = (
            f"已达到本轮最多 {self.config.max_steps} 步限制，任务尚未完全结束。"
            "请回复「继续」让我基于现有上下文接着处理。"
        )
        self._pending_task_messages = working_messages
        on_delta(final_reply)
        self._append_history(text, final_reply)
        return final_reply

    def _request_agent_reply(
        self,
        messages: list[dict[str, str]],
        on_delta: Callable[[str], None],
    ) -> tuple[str, str, bool]:
        """请求模型给出下一步：要么调用一个工具，要么输出最终回答。

        返回 (reply, reasoning, streamed_final)。工具调用场景中 reasoning 会被
        保留在 working_messages 里并持续回传 API。
        """

        try:
            stream = self._client.responses.create(
                model=self.config.llm.model,
                instructions=self._system_prompt(),
                input=messages,
                stream=True,
                extra_body=self._build_extra_body(),
            )
        except Exception as exc:
            raise AgentError(f"Agent 请求失败：{OpenAIResponseLLM.format_request_error(exc)}") from exc

        chunks: list[str] = []
        reasoning_chunks: list[str] = []
        final_streamer = _AgentReplyStreamer(on_delta)
        try:
            for event in stream:
                for delta in OpenAIResponseLLM.extract_stream_text(event):
                    chunks.append(delta)
                    final_streamer.push(delta)
                if self.config.llm.thinking_enabled:
                    for delta in OpenAIResponseLLM.extract_stream_reasoning(event):
                        reasoning_chunks.append(delta)
        except Exception as exc:
            raise AgentError(f"Agent 流式回复中断：{OpenAIResponseLLM.format_request_error(exc)}") from exc

        final_streamer.finish()
        reply = "".join(chunks)
        if not reply:
            raise AgentError("Agent 返回内容为空或格式不可解析。")

        reasoning = "".join(reasoning_chunks).strip()
        return reply.strip(), reasoning, final_streamer.streamed

    def _build_extra_body(self) -> dict[str, Any]:
        """构造网关扩展参数；根据 reasoning_effort 决定是否启用思考模式。"""

        thinking_type = "enabled" if self.config.llm.thinking_enabled else "disabled"
        body: dict[str, Any] = {"thinking": {"type": thinking_type}}
        if self.config.llm.thinking_enabled and self.config.llm.reasoning_effort:
            if self.config.llm.reasoning_effort in VALID_REASONING_EFFORTS:
                body["reasoning_effort"] = self.config.llm.reasoning_effort
        return body

    def _parse_tool_call(self, raw_reply: str) -> ToolCall | None:
        """从模型回复中解析工具调用；解析失败时退化为最终回答，避免卡死。"""

        match = self._TOOL_PATTERN.search(raw_reply)
        payload = match.group(1).strip() if match else raw_reply.strip()

        try:
            data = json.loads(payload)
        except json.JSONDecodeError:
            return None

        if not isinstance(data, dict):
            return None

        name = data.get("name") or data.get("tool")
        arguments = data.get("arguments") or data.get("args") or {}
        if not isinstance(name, str) or not isinstance(arguments, dict):
            return None

        return ToolCall(name=name.strip(), arguments=arguments)

    def _parse_final_reply(self, raw_reply: str) -> str:
        """提取最终回答；没有显式 final 标签时直接使用模型原文。"""

        match = self._FINAL_PATTERN.search(raw_reply)
        if match:
            return match.group(1).strip()
        return raw_reply.strip()

    def _run_tool(self, tool: ToolDefinition, arguments: dict[str, Any]) -> ToolResult:
        """执行工具；所有工具调用都先经过人工确认。"""

        if tool.requires_confirmation and not self._confirm(tool.name, arguments):
            raise UserDeclinedOperation(f"用户选择 NO，已取消执行：{tool.name}。")

        try:
            result = tool.run(arguments)
        except Exception as exc:
            return ToolResult(ok=False, output=str(exc))

        return ToolResult(
            ok=result.ok,
            output=self._truncate_tool_output(result.output),
        )

    def _build_tools(self) -> dict[str, ToolDefinition]:
        """注册内置工具；所有工具执行前统一走确认门。"""

        tools = [
            ToolDefinition(
                name="list_files",
                description="列出工作区内的文件和目录，可选择递归。",
                argument_schema='{"path": ".", "recursive": false}',
                requires_confirmation=True,
                run=self._tool_list_files,
            ),
            ToolDefinition(
                name="read_file",
                description="读取 UTF-8 文本文件，可指定起始行和最多行数。",
                argument_schema='{"path": "main.py", "start_line": 1, "max_lines": 200}',
                requires_confirmation=True,
                run=self._tool_read_file,
            ),
            ToolDefinition(
                name="search_text",
                description="在工作区文本文件中搜索正则或普通文本。",
                argument_schema='{"pattern": "class Agent", "path": ".", "case_sensitive": false, "max_results": 50}',
                requires_confirmation=True,
                run=self._tool_search_text,
            ),
            ToolDefinition(
                name="replace_text",
                description="在单个文件中替换指定文本，适合小范围代码修改。",
                argument_schema='{"path": "main.py", "old_text": "...", "new_text": "...", "count": 1}',
                requires_confirmation=True,
                run=self._tool_replace_text,
            ),
            ToolDefinition(
                name="write_file",
                description="写入或追加 UTF-8 文本文件。",
                argument_schema='{"path": "notes.md", "content": "...", "mode": "overwrite"}',
                requires_confirmation=True,
                run=self._tool_write_file,
            ),
            ToolDefinition(
                name="run_command",
                description=(
                    "以工作区为当前目录执行任意本地命令、脚本或 shell 片段。"
                ),
                argument_schema='{"command": "python -m py_compile main.py", "timeout_seconds": 120}',
                requires_confirmation=True,
                run=self._tool_run_command,
            ),
        ]
        return {tool.name: tool for tool in tools}

    def _system_prompt(self) -> str:
        """构造工具协议提示词；每轮强制一个工具或一个最终回答，降低解析复杂度。"""

        tool_lines = "\n".join(
            (
                f"- {tool.name}: {tool.description}\n"
                f"  参数示例：{tool.argument_schema}\n"
            )
            for tool in self._tools.values()
        )
        system_prompt = (
            "你是一个可以长期处理本地项目任务的中文 AI Agent。"
            "你需要先理解用户目标，再在必要时调用工具收集证据、修改文件或验证结果。"
            "简单问答不需要工具，直接回答即可。\n\n"
            "工作区根目录："
            f"{self.workspace_root}\n\n"
            "可用工具：\n"
            f"{tool_lines}\n\n"
            "输出协议：每一轮只能输出以下两种格式之一，不要混用。\n"
            "1. 调用一个工具：\n"
            '<tool>{"name":"read_file","arguments":{"path":"main.py"}}</tool>\n'
            "2. 给用户最终回答：\n"
            "<final>这里写自然、简洁、可朗读的中文回答。</final>\n\n"
            "工具使用规则：\n"
            "- 一次只调用一个工具，拿到工具结果后再决定下一步。\n"
            "- 需要工具或命令时直接输出工具调用，不要把工具执行许可作为问题询问用户。\n"
            "- 文件工具只能访问工作区内路径；命令工具可执行目标所需的本地、系统或联网操作。\n"
            "- 不要编造工具结果；没有验证就说明未验证。\n"
            "- 如果需要修改代码，先读取相关文件，尽量小步改动，并在完成后用命令验证。"
        )
        # 手动调用 /skill:name 时注入 Skill 全文
        if self._skill_manager is not None and self._active_skills:
            system_prompt = self._skill_manager.inject(self._active_skills, system_prompt)
        # 渐进式披露：列出所有可用 Skill 的元数据，AI 自行用 read_file 加载
        elif self._skill_manager is not None:
            metas = self._skill_manager.list_all()
            skill_section = self._skill_manager.format_skills_for_prompt(metas)
            if skill_section:
                system_prompt += f"\n{skill_section}"
        if self._agents_instructions:
            system_prompt = f"{self._agents_instructions}\n\n---\n\n{system_prompt}"
        return system_prompt

    def _load_agents_instructions(self) -> str:
        """读取项目级 AGENTS.md，让模型在每轮上下文开头先看到协作规范。"""

        agents_path = self.workspace_root / AGENTS_INSTRUCTIONS_FILE
        if not agents_path.is_file():
            return ""

        try:
            instructions = agents_path.read_text(encoding="utf-8").strip()
            return self._strip_runtime_irrelevant_agents_sections(instructions)
        except UnicodeDecodeError as exc:
            raise AgentError(f"{AGENTS_INSTRUCTIONS_FILE} 必须是 UTF-8 文本。") from exc
        except OSError as exc:
            raise AgentError(f"读取 {AGENTS_INSTRUCTIONS_FILE} 失败：{exc}") from exc

    @staticmethod
    def _strip_runtime_irrelevant_agents_sections(instructions: str) -> str:
        """运行时工具审批由程序层处理，不把对应协作边界交给模型执行。"""

        return re.sub(
            r"\n## 10\. 用户确认边界\n.*?(?=\n## 11\. 工具策略\n)",
            "\n",
            instructions,
            flags=re.DOTALL,
        ).strip()

    def _tool_list_files(self, arguments: dict[str, Any]) -> ToolResult:
        path = self._safe_path(str(arguments.get("path") or "."))
        recursive = bool(arguments.get("recursive", False))
        if not path.exists():
            return ToolResult(ok=False, output=f"路径不存在：{self._relative_path(path)}")
        if path.is_file():
            return ToolResult(ok=True, output=self._relative_path(path))

        entries: list[str] = []
        iterator = path.rglob("*") if recursive else path.iterdir()
        for entry in sorted(iterator, key=lambda item: str(item).lower()):
            if self._should_skip_path(entry):
                continue
            suffix = "/" if entry.is_dir() else ""
            entries.append(f"{self._relative_path(entry)}{suffix}")
            if len(entries) >= 500:
                entries.append("... 已截断，结果超过 500 项。")
                break

        return ToolResult(ok=True, output="\n".join(entries) or "目录为空。")

    def _tool_read_file(self, arguments: dict[str, Any]) -> ToolResult:
        path = self._safe_path(str(arguments.get("path") or ""))
        start_line = max(1, int(arguments.get("start_line") or 1))
        max_lines = max(1, min(500, int(arguments.get("max_lines") or 200)))
        if not path.is_file():
            return ToolResult(ok=False, output=f"不是文件：{self._relative_path(path)}")

        text = self._read_text(path)
        lines = text.splitlines()
        start_index = start_line - 1
        selected = lines[start_index : start_index + max_lines]
        numbered = [f"{line_no}: {line}" for line_no, line in enumerate(selected, start=start_line)]
        if start_index + max_lines < len(lines):
            numbered.append("... 已截断，可提高 start_line 继续读取。")
        return ToolResult(ok=True, output="\n".join(numbered))

    def _tool_search_text(self, arguments: dict[str, Any]) -> ToolResult:
        pattern = str(arguments.get("pattern") or "")
        if not pattern:
            return ToolResult(ok=False, output="pattern 不能为空。")

        root = self._safe_path(str(arguments.get("path") or "."))
        case_sensitive = bool(arguments.get("case_sensitive", False))
        max_results = max(1, min(200, int(arguments.get("max_results") or 50)))
        flags = 0 if case_sensitive else re.IGNORECASE

        try:
            regex = re.compile(pattern, flags)
        except re.error:
            regex = re.compile(re.escape(pattern), flags)

        files = [root] if root.is_file() else self._iter_search_files(root)
        results: list[str] = []
        for file_path in files:
            if self._should_skip_path(file_path):
                continue
            try:
                lines = self._read_text(file_path).splitlines()
            except AgentError:
                continue
            for line_no, line in enumerate(lines, start=1):
                if regex.search(line):
                    results.append(f"{self._relative_path(file_path)}:{line_no}: {line}")
                    if len(results) >= max_results:
                        return ToolResult(ok=True, output="\n".join(results) + "\n... 已达到 max_results。")

        return ToolResult(ok=True, output="\n".join(results) or "未找到匹配结果。")

    def _tool_replace_text(self, arguments: dict[str, Any]) -> ToolResult:
        path = self._safe_path(str(arguments.get("path") or ""))
        old_text = str(arguments.get("old_text") or "")
        new_text = str(arguments.get("new_text") or "")
        count = int(arguments.get("count") or 1)
        if not path.is_file():
            return ToolResult(ok=False, output=f"不是文件：{self._relative_path(path)}")
        if not old_text:
            return ToolResult(ok=False, output="old_text 不能为空。")

        original = self._read_text(path)
        occurrences = original.count(old_text)
        if occurrences == 0:
            return ToolResult(ok=False, output="未找到 old_text，文件未修改。")

        replace_count = occurrences if count <= 0 else min(count, occurrences)
        updated = original.replace(old_text, new_text, replace_count)
        path.write_text(updated, encoding="utf-8")
        return ToolResult(
            ok=True,
            output=f"已修改 {self._relative_path(path)}，替换 {replace_count} 处。",
        )

    def _tool_write_file(self, arguments: dict[str, Any]) -> ToolResult:
        path = self._safe_path(str(arguments.get("path") or ""))
        content = str(arguments.get("content") or "")
        mode = str(arguments.get("mode") or "overwrite").lower()
        path.parent.mkdir(parents=True, exist_ok=True)
        if mode == "append":
            with path.open("a", encoding="utf-8") as file:
                file.write(content)
            action = "追加"
        elif mode in {"overwrite", "write"}:
            path.write_text(content, encoding="utf-8")
            action = "写入"
        else:
            return ToolResult(ok=False, output="mode 仅支持 overwrite 或 append。")

        return ToolResult(ok=True, output=f"已{action} {self._relative_path(path)}，字符数：{len(content)}。")

    def _tool_run_command(self, arguments: dict[str, Any]) -> ToolResult:
        command = str(arguments.get("command") or "").strip()
        if not command:
            return ToolResult(ok=False, output="command 不能为空。")

        timeout = int(arguments.get("timeout_seconds") or self.config.command_timeout_seconds)
        timeout = max(1, min(timeout, 300))
        try:
            completed = subprocess.run(
                command,
                cwd=str(self.workspace_root),
                shell=True,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=timeout,
            )
        except subprocess.TimeoutExpired:
            return ToolResult(ok=False, output=f"命令执行超过 {timeout} 秒，已终止。")

        output_parts = [f"退出码：{completed.returncode}"]
        if completed.stdout.strip():
            output_parts.append(f"stdout:\n{completed.stdout.strip()}")
        if completed.stderr.strip():
            output_parts.append(f"stderr:\n{completed.stderr.strip()}")
        return ToolResult(ok=completed.returncode == 0, output="\n\n".join(output_parts))

    def _iter_search_files(self, root: Path) -> list[Path]:
        """递归搜索时在目录层剪枝，避免进入 .git、虚拟环境或本地密钥目录。"""

        files: list[Path] = []
        for dirpath, dirnames, filenames in os.walk(root):
            current_dir = Path(dirpath)
            dirnames[:] = [
                dirname
                for dirname in sorted(dirnames, key=lambda s: s.lower())
                if not self._should_skip_path(current_dir / dirname)
            ]

            for filename in sorted(filenames, key=lambda s: s.lower()):
                file_path = current_dir / filename
                if not self._should_skip_path(file_path):
                    files.append(file_path)

        return files

    def _safe_path(self, raw_path: str) -> Path:
        """把模型给出的路径限制在工作区内，阻止 ../ 越界访问。"""

        raw_path = raw_path.strip()
        if not raw_path:
            raise AgentError("路径不能为空。")

        candidate = Path(raw_path)
        if not candidate.is_absolute():
            candidate = self.workspace_root / candidate
        resolved = candidate.resolve()
        if not self._is_relative_to(resolved, self.workspace_root):
            raise AgentError(f"拒绝访问工作区外路径：{raw_path}")
        if self._is_protected_path(resolved):
            raise AgentError(f"拒绝访问受保护路径：{self._relative_path(resolved)}")
        return resolved

    def _relative_path(self, path: Path) -> str:
        try:
            return str(path.resolve().relative_to(self.workspace_root))
        except ValueError:
            return str(path)

    def _should_skip_path(self, path: Path) -> bool:
        return self._is_protected_path(path)

    @staticmethod
    def _is_protected_path(path: Path) -> bool:
        """避免自动工具读取缓存、虚拟环境、Git 内部文件或本地密钥文件。"""

        protected_names = {
            ".git",
            ".venv",
            "venv",
            "env",
            "__pycache__",
            ".codex-ref",
            ".env",
            "config.json",
        }
        return any(part in protected_names or part.startswith(".env.") for part in path.parts)

    def _read_text(self, path: Path) -> str:
        try:
            return path.read_text(encoding="utf-8")
        except UnicodeDecodeError as exc:
            raise AgentError(f"文件不是 UTF-8 文本或包含二进制内容：{self._relative_path(path)}") from exc
        except OSError as exc:
            raise AgentError(f"读取文件失败：{self._relative_path(path)}，{exc}") from exc

    def _truncate_tool_output(self, output: str) -> str:
        if len(output) <= self.config.max_tool_output_chars:
            return output
        return output[: self.config.max_tool_output_chars] + "\n... 工具输出已截断。"

    @staticmethod
    def _format_tool_observation(tool_call: ToolCall, result: ToolResult) -> str:
        return (
            f"工具 {tool_call.name} 执行完成。\n"
            f"状态：{'成功' if result.ok else '失败'}\n"
            f"结果：\n{result.output}\n\n"
            "请基于该工具结果继续。若任务已经完成，请用 <final> 输出最终回答；"
            "若还需要更多信息，请继续用 <tool> 调用一个工具。"
        )

    def _append_history(self, user_text: str, assistant_text: str, reasoning: str = "") -> None:
        """写入对话历史；有工具调用时附带 reasoning_content 供后续轮次回传。"""

        assistant_msg: dict[str, str] = {"role": "assistant", "content": assistant_text}
        if reasoning:
            assistant_msg["reasoning_content"] = reasoning
        self._history.extend(
            [
                {"role": "user", "content": user_text},
                assistant_msg,
            ]
        )
        max_messages = self.config.max_history_turns * 2
        if len(self._history) > max_messages:
            self._history = self._history[-max_messages:]

    @staticmethod
    def _is_continue_request(user_text: str) -> bool:
        return user_text.strip().lower() in {"继续", "接着做", "继续处理", "continue", "c"}

    @staticmethod
    def _confirm_in_terminal(tool_name: str, arguments: dict[str, Any]) -> bool:
        print("\nAgent 请求执行受限工具：")
        print(f"工具：{tool_name}")
        print("参数：")
        print(json.dumps(arguments, ensure_ascii=False, indent=2))
        answer = input("是否允许执行？直接回车=YES，输入 n/no/否=NO：").strip().lower()
        return answer not in {"n", "no", "否", "false"}

    @staticmethod
    def _is_relative_to(path: Path, parent: Path) -> bool:
        try:
            path.relative_to(parent)
            return True
        except ValueError:
            return False
