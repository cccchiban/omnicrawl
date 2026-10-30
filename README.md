# OmniCrawl

<p align="center">
  <img src="assets/logo.jpg" alt="OmniCrawl Logo" width="200">
</p>


[![CI](https://github.com/cccchiban/omnicrawl/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/cccchiban/omnicrawl/actions/workflows/ci.yml)
[![npm](https://img.shields.io/npm/v/omnicrawl-cli?label=npm&color=cb3837)](https://www.npmjs.com/package/omnicrawl-cli)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)


OmniCrawl 是纯 Rust 构建的本地 AI 编程终端工作台，提供全屏 TUI 与本地 HTTP/SSE API。它通过「理解目标 → 读取文件 → 搜索文本 → 编辑文件 → 执行命令」的 Agent 循环自动推进任务。工具审批默认采用「自动审查」模式：文件删除、下载执行及高风险 Git 操作交由独立审查模型判定，也可按需切换为逐次人工确认或全部自动批准。

## 功能特性

- **终端 TUI**：全屏工作台（`omnicrawl-tui`），支持键鼠交互、模型热切换、渠道管理、设置面板、审批控制与事务级 `/undo` 回退；取消当前回合仍保留上下文。
- **HTTP/SSE API**：本地服务可直接对接 Web 或桌面前端，内置 `Swagger UI` 接口文档。
- **工具与安全**：内置文件读写、文本搜索、本地图片读取、视觉模型代理、Bash/PowerShell、Windows 桌面控制、记忆与 SubAgent 等工具，可在 `config.toml` 的 `tools` 段逐项开关。SubAgent 支持独立 Git worktree 隔离执行，其全局设置与模型配置独立存放于 `subagents.toml`（模板见 `rust/assets/config-templates/subagents.example.toml`，不再读取 `config.toml` 的 `[subagents]` 段）。
- **统一工具分发**：Provider 顶层直接注册当前 Agent 的全部可见工具，模型以真实工具名原生调用；Host 侧统一维护完整 Schema、审批策略、MCP 能力与执行器并做二次校验（详见 `rust/assets/docs/TOOL_CALLING.md`）。
- **扩展能力**：支持 Skill 技能、MCP Server 接入及自定义 `models.toml` 模型目录。

## 环境要求

- 本体为纯 Rust 二进制，无额外运行时依赖
- 从源码构建需 Rust 1.90+（工作区 MSRV）

## 安装

```powershell
npm install -g omnicrawl-cli   # 安装启动器与对应平台的 Rust 二进制包
omnicrawl                      # 启动全屏 TUI
```

平台包按 `os/cpu` 自动分发，支持 `@omnicrawl/cli-win32-x64`、`cli-win32-ia32`、`cli-linux-x64`、`cli-linux-arm64-musl`（arm64）与 `cli-linux-arm-musl`（armv7）。

### 从源码构建

```powershell
git clone https://github.com/cccchiban/omnicrawl.git
cd omnicrawl/rust
cargo build --release -p omnicrawl-cli      # 构建内核（协议 v1 NDJSON）
cargo test --workspace                      # 运行工作区测试
```

宿主载荷（含统一入口 `omnicrawl-host`、内核、TUI、API、决策接口与 MCP Server）由 `node packages/cli/scripts/build-host.mjs` 装配，再由 `prepare.mjs` 打包为 npm 产物。交叉编译与各平台前置依赖见 `rust/docs/python-free-build.md`。

## 启动与使用

### 终端 TUI

```powershell
omnicrawl
```

`omnicrawl` 为统一命令入口（由 npm 启动器解析至当前平台的 Rust 宿主），同时提供以下子命令：
- `omnicrawl api`：启动本地 HTTP/SSE 服务
- `omnicrawl decision`：启动本地结构化决策模型服务
- `omnicrawl kernel`：直接运行协议内核

**首次配置**：首次启动将进入命令行向导，引导填写渠道名、Provider（OpenAI / Anthropic / Gemini）、协议、Base URL、模型 ID 及凭据环境变量名。配置保存在 `~/.OmniCrawl`，配置好启用且含有效 Key 的默认渠道后即可进入工作台，其余选项可在 `/settings` 中修改。

**连接器自动联动**：若已配置 Telegram（Bot Token + 授权用户 ID）、飞书（App ID + App Secret）或 MQTT（`[mqtt]` 段的 broker、topics、actions 齐全），启动 TUI 时会自动拉起对应的独立子进程，退出时自动回收；未配置项不启动，连接器异常不阻塞 TUI。同一平台与用户下仅运行单个活动实例，多 TUI/API 进程并存时后启动方会自动跳过，避免重复长连接。设置环境变量 `OMNICRAWL_AUTO_START_CONNECTORS=0` 可关闭自动拉起。
> 注：MQTT 并非聊天平台，而是由遥测事件触发，结合规则与决策模型自动执行开关棚、通风、灌溉等操作（详见 `rust/assets/docs/MQTT.md`）；Telegram 与飞书的配置及安全边界见 `rust/assets/docs/TELEGRAM.md`、`rust/assets/docs/FSAPP.md`。

**常用快捷键与命令**：

- `Enter` 发送，`Ctrl+J` 换行，`Esc` 取消当前回合或清空输入
- `/model <key|profile/model_id|model_id>` 切换模型；`/settings` 打开设置与渠道管理；`/approval:auto|manual|review` 切换审批模式
- `/new` 新建对话，`/sessions` 会话列表，`/compact` 压缩上下文，`/undo` 事务级回退上一轮
- `/skills`、`/mcp` 查看技能与 MCP 状态
- `Ctrl+L` 清屏，`Ctrl+Q` 退出

### 本地 HTTP/SSE API

在 `~/.OmniCrawl/config.toml` 中配置访问令牌：

```toml
[api]
bearer_token = "请替换为随机长令牌"
host = "127.0.0.1"
port = 8765
```

启动服务：

```powershell
omnicrawl api
```

默认监听 `127.0.0.1:8765`，Swagger UI 位于 `http://127.0.0.1:8765/docs`，OpenAPI 契约位于 `/openapi.json`。除健康检查与文档接口外，所有请求均需携带 `Authorization: Bearer <token>`。服务仅绑定回环地址；若令牌为空或 CORS 来源设为 `*`，将拒绝启动。完整接入说明见 `rust/assets/docs/API.md`。

## 项目结构

```text
omnicrawl/
├── rust/                       # 核心源码：内核 + 宿主 + TUI + API + MCP（纯 Rust）
│   ├── crates/omnicrawl-entry/     # 统一入口（omnicrawl-host）：路由 TUI / api / decision / fetch / kernel / connector
│   ├── crates/omnicrawl-cli/       # 内核进程（协议 v1 NDJSON）
│   ├── crates/omnicrawl-tui/       # 全屏终端工作台
│   ├── crates/omnicrawl-host/      # 宿主：工具执行、审批、审查
│   ├── crates/omnicrawl-api/       # 本地 HTTP/SSE 服务
│   ├── crates/omnicrawl-mcp/       # MCP Server
│   ├── crates/omnicrawl-decision/  # 结构化决策模型本地服务
│   ├── crates/omnicrawl-tts/       # 语音合成
│   └── assets/docs/                # 内置技术文档（omnicrawl://docs/）
├── packages/                   # npm 分发：启动器（平台包在发布时生成）
├── assets/                     # 仓库静态资源（Logo 等）
└── tests/                      # npm 端到端测试夹具
```

## 开源协议

本项目基于 [MIT License](LICENSE) 开源。

```text
MIT License

Copyright (c) 2026 cccchiban

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
