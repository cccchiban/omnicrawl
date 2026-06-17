# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目概述

本地 AI 语音 Agent 桌面应用，支持 Qt Fluent GUI 和终端 TUI 两种前端。通过 OpenAI Python SDK 调用兼容 `/v1` 接口的 LLM API，内置文件/搜索/命令工作区工具、MCP 客户端/服务端、Skill 系统、会话持久化和长期记忆。

## 常用命令

```bash
# 安装依赖
pip install -r requirements.txt

# 运行（Qt GUI 或 TUI，取决于 config.json 中 frontend.type）
python main.py

# 恢复会话
python main.py --resume <SESSION_ID>

# 运行测试（无自定义 pytest 配置）
pytest

# 运行单个测试文件
pytest tests/test_agent_context.py

# 运行单个测试函数
pytest tests/test_llm_config.py::test_load_config_from_env
```

## 架构

### 启动流程

`main.py` → 加载各模块配置（LLM / Voice / Approval / TempWorkspace / ProjectContext / Frontend）→ `create_ui()` 工厂创建前端 → 初始化语音 → 创建 `LocalToolAgent` → 进入对话循环（Qt: 后台线程 + 主线程 Qt 事件循环；TUI: 同步内联循环）

### 核心模块依赖关系

```
main.py
  ├── agent.py (LocalToolAgent) ← 核心调度：LLM 调用 → 工具执行 → 审批 → Skill/Memory/Session/MCP
  │   ├── llm.py              ← LLMConfig (frozen dataclass) + OpenAI Responses API 客户端
  │   ├── workspace_tools.py  ← 内置工具实现（list_files/read_file/search_text/replace_text/write_file/run_command）
  │   ├── approval.py         ← 工具审批模式 (manual/auto/review)
  │   ├── skill.py            ← Skill 发现、索引、匹配、注入
  │   ├── memory.py           ← 长期记忆分类与检索
  │   ├── session.py          ← JSONL 会话持久化、压缩、恢复、归档
  │   ├── mcp/                ← MCP 子系统
  │   │   ├── client.py       ← MCPClientManager (stdio 传输)
  │   │   ├── server.py       ← LocalMCPServer（暴露 workspace_tools 为 MCP 工具）
  │   │   ├── registry.py     ← 能力注册表（tools/resources/prompts）
  │   │   ├── security.py     ← 工具确认 + 敏感值脱敏
  │   │   └── audit.py        ← JSONL 审计日志
  │   └── slash_commands.py   ← /new /skills /mcp /approval /model 等斜杠命令
  ├── ui/base.py              ← BaseUI 抽象基类
  │   ├── tui/                ← TerminalUI（ANSI 终端 UI，模块化拆分）
  │   └── ui/qt/              ← QtUI（PyQt5 + QWebEngineView + HTML/CSS/JS 前端）
  ├── audio_setup.py          ← 语音配置加载 + 麦克风选择
  ├── speech_to_text.py       ← PyAudio 录音 + Google Web Speech API
  ├── text_to_speech.py       ← TTS (Windows: System.Speech, fallback: pyttsx3)
  ├── speech_playback.py      ← 流式语音播放器（FIFO 队列，句子级播报）
  └── runtime_config.py       ← config.json 加载/保存工具函数
```

### 关键设计模式

- **配置层**：所有配置模块依赖 `runtime_config.py` 的 `load_config_data()` / `save_config_data()` / `get_section()`，配置对象均为 `@dataclass(frozen=True)` 不可变实例
- **UI 抽象**：`BaseUI` 定义接口，`create_ui()` 工厂按 `frontend.type` 选择实现；`terminal_ui.py` 是向后兼容的 shim，实际代码在 `tui/` 包
- **工具共享**：`WorkspaceTools` 同时被内置 Agent 工具和本地 MCP Server 复用
- **受保护路径**：`.git`、`config.json`、`.env`、venv 目录在 `WorkspaceTools` 中强制禁止访问
- **语音管道**：STT → Agent → TTS → StreamingSpeechPlayer（句子级 FIFO 队列，支持打断）

## 编码约定

- 所有 `.py` 文件使用 `from __future__ import annotations`
- 自定义错误类继承 `RuntimeError`（如 `LLMError`、`AgentError`、`MCPConfigError`）
- 用户界面输出、注释、文档字符串使用中文
- 临时文件统一放 `.agent_tmp/`（按 files/images/code/videos/scripts 分类），不散落在项目根目录
- 系统提示词模板 `system_prompt.md` 使用 `{workspace_root}` / `{agent_temp_dir}` / `{tool_lines}` 占位符

## 环境变量

关键覆盖变量（优先级高于 config.json）：

| 变量 | 用途 |
|------|------|
| `OPENAI_API_KEY` / `OPENAI_BASE_URL` / `OPENAI_MODEL` | LLM 配置 |
| `OPENAI_THINKING_TYPE` | 思考模式 |
| `AI_WORKSPACE_ROOT` | 显式指定工作区路径 |
| `AI_CONFIG_FILE` | 自定义配置文件路径 |
| `TTS_BACKEND` | 强制 TTS 后端（system_speech / pyttsx3） |
| `MIC_DEVICE_INDEX` / `MIC_DEVICE_KEYWORD` | 麦克风选择 |
| `MCP_ENABLED` / `MCP_DEFAULT_TIMEOUT_SECONDS` | MCP 覆盖 |
| `AGENT_REQUEST_RETRY_COUNT` / `AGENT_REQUEST_TIMEOUT_SECONDS` | Agent 循环调优 |

## 测试

- 框架：pytest，无自定义配置文件
- 14 个测试文件在 `tests/`，覆盖所有主要模块
- 无 conftest.py，无自定义 fixture 或 marker
