# OmniCrawl

OmniCrawl 是一款本地运行的个人 AI 编程助手（终端工作台），提供全屏 TUI 与本地 HTTP/SSE API。它以 Agent 循环方式执行任务：理解目标 → 读取项目文件 → 搜索文本 → 编写或修改文件 → 执行命令，并默认在工具执行前请求人工确认（也可切换自动审批模式）。

## 功能特性

- **多模型协议**：通过统一运行时调用 OpenAI Chat Completions / Responses、Anthropic Messages、Google Gemini Generate Content（各用原生 SDK），支持思考模式、流式输出与中断自动重试。
- **终端 TUI**：Textual 全屏工作台，键盘交互，内置模型热切换、渠道管理、设置面板、审批控制、事务式 `/undo` 回退、取消回合上下文保留。
- **HTTP/SSE API**：本地服务可对接 Web 或桌面前端，`Swagger UI` 文档开箱即用。
- **工具与安全**：内置文件读写、文本搜索、本地图片读取、视觉模型代理、Bash/PowerShell、Windows 桌面控制、记忆与 SubAgent 等工具，可在 `config.yaml` 的 `tools` 段逐工具开关；SubAgent 支持独立 Git worktree 隔离执行。
- **统一工具分发**：模型只看到固定的 `search_tools` 与 `invoke_tool`；真实工具 Schema、审批策略、MCP 能力和执行器由 Host 侧目录维护，详见 `omnicrawl/docs/TOOL_CALLING.md`。
- **可扩展**：支持 NPM Hook 插件、Skill 技能、MCP Server 接入，以及自定义 `models.yaml` 模型目录。

## 环境要求

- Python `>=3.9`
- 安装完整功能建议本机具备 Node.js 20+（仅插件功能需要，缺失不影响无插件模式启动）

## 安装

### 从 PyPI 安装（推荐）

```powershell
pip install omnicrawl-agent
```

### 从源码运行

```powershell
git clone https://github.com/cccchiban/omnicrawl.git
cd omnicrawl

pip install -r requirements.txt
# 可选：以可编辑方式安装 console script（同时提供 ocl 和 omnicrawl 两个命令）
pip install -e . --no-build-isolation
```

## 启动

### 终端 TUI

```powershell
ocl
```

`omnicrawl` 为兼容旧命令的等价入口；源码目录下也可直接运行 `python main.py`。

首次启动会引导配置模型渠道（预置 OpenAI、Anthropic、Gemini，支持自定义 Base URL 与 API Key），配置保存在本机 `~/.OmniCrawl`。至少保存一个已启用且具备 Key 的默认渠道后，重新运行 `ocl` 即可进入工作台。

常用操作：

- `Enter` 发送消息，`Shift+Enter` 换行；`Esc` 取消当前任务或聚焦输入框
- `/model` 双列切换模型；`/settings` 打开设置与渠道管理；`/approval:auto|manual|review` 切换审批模式
- `/new` 开启新对话；`/undo` 事务式回退上一轮；`/skills`、`/mcp` 查看技能与 MCP 状态
- `Ctrl+Q` 退出

### 本地 HTTP/SSE API

```powershell
$env:OMNICRAWL_API_TOKEN = "请替换为随机长令牌"
python -m omnicrawl.api
```

默认监听 `127.0.0.1:8765`，Swagger UI 位于 `http://127.0.0.1:8765/docs`，机器可读契约位于 `/openapi.json`。除健康检查与文档外，所有接口需携带 `Authorization: Bearer <token>`。完整接入说明见 `omnicrawl/docs/API.md`。

## 项目结构

```text
omnicrawl/
├── agent/         # Agent 循环、SubAgent、上下文压缩与技能
├── api/           # 本地 HTTP/SSE 服务
├── config/        # 配置加载、模型渠道与工具开关
├── llm/           # 多协议模型运行时
├── ui/            # 终端 TUI（Textual）
├── workspace/     # 工作区工具与搜索索引
└── docs/          # 技术文档
```

## 开源协议

本项目基于 [MIT License](LICENSE) 开源发布。

```
MIT License

Copyright (c) 2025 OmniCrawl

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
