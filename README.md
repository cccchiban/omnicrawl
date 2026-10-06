# OmniCrawl

<p align="center">
  <img src="assets/logo.jpg" alt="OmniCrawl Logo" width="200">
</p>

OmniCrawl 是一款本地运行的个人 AI 编程助手（终端工作台），提供全屏 TUI 与本地 HTTP/SSE API。它以 Agent 循环方式执行任务：理解目标 → 读取项目文件 → 搜索文本 → 编写或修改文件 → 执行命令，并默认在工具执行前请求人工确认（也可切换自动审批模式）。

## 功能特性

- **多模型协议**：通过统一运行时调用 OpenAI Chat Completions / Responses、Anthropic Messages、Google Gemini Generate Content（各用原生 SDK），支持思考模式、流式输出与中断自动重试。
- **终端 TUI**：Rust 全屏工作台（`omnicrawl-tui`），键盘交互，内置模型热切换、渠道管理、设置面板、审批控制、事务式 `/undo` 回退、取消回合上下文保留。
- **HTTP/SSE API**：本地服务可对接 Web 或桌面前端，`Swagger UI` 文档开箱即用。
- **工具与安全**：内置文件读写、文本搜索、本地图片读取、视觉模型代理、Bash/PowerShell、Windows 桌面控制、记忆与 SubAgent 等工具，可在 `config.toml` 的 `tools` 段逐工具开关；SubAgent 支持独立 Git worktree 隔离执行。子代理全局设置与每个子代理的模型选择放在独立的 `subagents.toml`（模板见 `subagents.example.toml`），已从 `config.toml` 完全迁移，不再回退读取 `config.toml` 的 `[subagents]` 段。
- **统一工具分发**：模型只看到固定的 `search_tools` 与 `invoke_tool`；真实工具 Schema、审批策略、MCP 能力和执行器由 Host 侧目录维护，详见 `rust/assets/docs/TOOL_CALLING.md`。
- **可扩展**：支持 NPM Hook 插件、Skill 技能、MCP Server 接入，以及自定义 `models.toml` 模型目录。

## 环境要求

- 产品是 Rust 二进制：运行**不需要** Python 运行时；插件功能需要 Node.js 20+
- `grep`/`find` 的文本搜索由 Rust 侧实现（`rust/crates/omnicrawl-host` 的 `regex`/`ignore`，
  与 `native/` 的 Go 扩展语义对齐）；Go 原生扩展只服务对照测试

## 安装

```powershell
npm install -g omnicrawl-cli   # 启动器 + 平台包（内核 + 宿主载荷，纯 Rust 二进制）
omnicrawl                      # 全屏 TUI
```

平台包按 `os/cpu` 分发：`@omnicrawl/cli-win32-x64`、`cli-win32-ia32`、`cli-linux-x64`、
`cli-linux-arm64-musl`、`cli-linux-arm-musl`（后两个分别覆盖 arm64 与 armv7；32 位 Windows
与 armv7 只带内核，启动器会自动退回协议直连）。

### 从源码构建

```powershell
git clone https://github.com/cccchiban/omnicrawl.git
cd omnicrawl/rust
cargo build --release -p omnicrawl-cli      # 内核（协议 v1 NDJSON）
cargo test --workspace                       # 全量对照测试
```

宿主/TUI/API 会链接 BoringSSL（C/C++ 工具链），构建前置与交叉编译方式见
`rust/docs/python-free-build.md`；发布载荷由 `packages/cli/scripts/build-host.mjs` 装配，
`prepare.mjs` 组装 npm 产物。

### 原生搜索扩展

Go 原生搜索扩展（`native/`，与 Rust 侧搜索语义对齐）可就地重建（仅服务对照测试，不再随包分发）：

```powershell
python native/build.py --out native/_ocsearch.pyd
```

构建需要 Go 1.21+ 与 cgo 可用的 C 编译器（`gcc`/`clang`，Windows 上是 MinGW-w64，**MSVC 的 `cl.exe` 不被 cgo 支持**；可 `winget install BrechtSanders.WinLibs.POSIX.UCRT`）。

## 启动

### 终端 TUI

```powershell
omnicrawl
```

`omnicrawl` 就是产品入口（npm 启动器，按平台包解析到 Rust 宿主）；进程内还提供
`omnicrawl api`（本地 HTTP/SSE 服务）与 `omnicrawl kernel`（直接跑协议内核）两个子命令。

首次启动会引导配置模型渠道（预置 OpenAI、Anthropic、Gemini，支持自定义 Base URL 与 API Key），配置保存在本机 `~/.OmniCrawl`。至少保存一个已启用且具备 Key 的默认渠道后，重新运行 `omnicrawl` 即可进入工作台。

如果本机配置了 Telegram（Bot Token + 授权用户 ID）或飞书（App ID + App Secret），
启动 TUI 时会自动拉起对应的独立连接器子进程；未配置的平台不会启动，连接器故障不阻塞
TUI，退出 TUI 时会自动回收连接器。同一平台同一用户只允许一个活动连接器实例：
多个 TUI/API 进程并存时，后启动方会自动跳过，避免 Telegram/飞书重复长连接。
设置 `OMNICRAWL_AUTO_START_CONNECTORS=0` 可关闭该联动。配置方法与安全边界见
`rust/assets/docs/TELEGRAM.md`、`rust/assets/docs/FSAPP.md`。

常用操作：

- `Enter` 发送消息，`Shift+Enter` 换行；`Esc` 取消当前任务或聚焦输入框
- `/model` 双列切换模型；`/settings` 打开设置与渠道管理；`/approval:auto|manual|review` 切换审批模式
- `/new` 开启新对话；`/undo` 事务式回退上一轮；`/skills`、`/mcp` 查看技能与 MCP 状态
- `Ctrl+Q` 退出

### 本地 HTTP/SSE API

```powershell
$env:OMNICRAWL_API_TOKEN = "请替换为随机长令牌"
omnicrawl api
```

默认监听 `127.0.0.1:8765`，Swagger UI 位于 `http://127.0.0.1:8765/docs`，机器可读契约位于 `/openapi.json`。除健康检查与文档外，所有接口需携带 `Authorization: Bearer <token>`。完整接入说明见 `rust/assets/docs/API.md`。

## 项目结构

```text
omnicrawl/
├── rust/                       # 产品本体：内核 + 宿主 + TUI + API + MCP（纯 Rust）
│   ├── crates/omnicrawl-cli/       # 内核进程（协议 v1 NDJSON）
│   ├── crates/omnicrawl-tui/       # 全屏终端工作台
│   ├── crates/omnicrawl-host/      # 宿主：工具执行、审批、审查
│   ├── crates/omnicrawl-api/       # 本地 HTTP/SSE 服务
│   ├── crates/omnicrawl-mcp/       # MCP Server
│   ├── crates/omnicrawl-decision/  # 结构化决策模型本地服务
│   ├── crates/omnicrawl-tts/       # 语音合成
│   └── assets/docs/                # 内置技术文档（omnicrawl://docs/）
├── packages/                   # npm 分发：启动器、平台包、插件 SDK/Host
├── native/                     # Go 原生搜索扩展（仅服务对照测试）
└── designs/                    # 设计稿与规划
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
