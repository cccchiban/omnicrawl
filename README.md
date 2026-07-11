# OmniCrawl

本目录实现本地 OmniCrawl，提供终端 TUI 和本机 HTTP/SSE API：

1. TUI 使用键盘交互，HTTP API 供后续 Web 或桌面前端接入。
2. 使用 OpenAI Python SDK 调用 `https://xxx.xx/v1` 的 Responses API 兼容接口。
3. AI 会按 Agent 循环处理任务：理解目标、读取项目文件、搜索文本、写文件或执行命令；默认会在工具执行前拦截确认，也可开启自动审批模式。
4. AI 回复会在 TUI 中显示，或通过 SSE 事件流推送给 API 客户端。

## Agent 临时目录

项目内置 `.agent_tmp/` 作为 Agent 专用临时目录，用于存放一次性文件、图片、代码、视频和脚本，避免把临时产物散落在项目根目录。

```text
.agent_tmp/
├── files/    # 普通临时文件和中间结果
├── images/   # 截图、生成图片和图像处理中间文件
├── code/     # 一次性验证代码、草稿代码和临时样例
├── videos/   # 临时视频、录屏和转码中间文件
└── scripts/  # 只为当前任务服务的临时脚本
```

Agent 启动时会自动创建该目录，并通过 `.agent_tmp/.last_cleanup` 的文件时间记录上次清理时间；距离上次清理超过 24 小时时，启动阶段会立即补清理一次，运行期间也会继续按间隔清理。`.agent_tmp/README.md`、`.agent_tmp/.gitignore` 和 `.agent_tmp/.last_cleanup` 会被保留，其他临时内容会被清理后重建分类子目录。

## Agent 能力

程序启动后，普通对话会直接进入 Agent 模式，不需要额外输入 `/agent`。

启动时会自动检测当前要操作的项目路径：优先使用 `AI_WORKSPACE_ROOT` 环境变量；否则使用启动 Agent 时的目录，并向上查找 `.git`、`AGENTS.md`、`pyproject.toml`、`package.json`、`requirements.txt` 等常见项目标记。检测到的工作区会显示在启动面板的 `workspace` 行，并注入系统提示词，后续文件工具都会以该目录作为访问边界。

内置工具：

- `list_files`：列出工作区文件，默认执行前会要求确认。
- `read_file`：读取工作区内 UTF-8 文本文件，默认执行前会要求确认。
- `read_file` 支持 `start_line`/`max_lines` 行范围、`function_name` 函数或方法定位，以及 `text`/`context_lines` 文字片段上下文定位；Python 优先使用 AST，其他常见代码使用声明与大括号范围回退。
- `search_text`：在工作区内搜索文本或正则，默认执行前会要求确认。
- `replace_text`：替换单个文件中的文本，默认执行前会要求确认。
- `write_file`：写入或追加文件，默认执行前会要求确认。
- `bash`：使用 Git Bash 执行 Bash 命令，适合 POSIX Shell 语法与 Bash 脚本，默认执行前会要求确认。
- `powershell`：使用 PowerShell 执行 Windows 命令，优先使用 PowerShell 7，默认执行前会要求确认。
- `monitor`：受 Agent 管理地在后台执行命令，默认使用 PowerShell，也可显式指定 Bash；`start` 返回任务 ID，`poll` 按游标读取增量日志，`stop` 停止任务，`list` 查看任务。Agent 关闭或切换工作区时会自动终止其子进程树，默认执行前会要求确认。
- `bb_browser_cli`：调用 bb-browser CLI 操作真实浏览器；Agent 启动时不会预热或打开浏览器，首次实际调用该工具时由 CLI 按需启动 daemon 和受管浏览器，默认执行前会要求确认。

安全边界：

- 文件工具只能访问当前项目目录内的路径；`config.json`、`.env`、`.git`、虚拟环境和缓存目录仍是受保护路径。
- `approval.mode` 默认为 `manual`，所有受限工具都会先在终端显示确认页；按 `Enter`、`Y` 或 `1` 允许，按 `N` 或 `2` 拒绝；方向键只会被消费，不会触发工具执行。
- `approval.mode` 设为 `auto` 时完全自动批准受限工具；设为 `review` 时只把疑似删除行为交给同一模型的非思考模式审查，其他工具调用自动执行。自动模式不显示确认页，只显示步骤和执行记录。
- 命令工具不是系统级沙箱；所有命令均通过明确的 PowerShell 或 Git Bash 解释器以 `shell=False` 启动。确认前请检查命令内容，尤其是删除、移动、覆盖、联网下载、安装依赖、修改系统配置等操作。
- bb-browser 是内置 CLI 能力，不通过 MCP 暴露；需要安装或更新时使用项目里的 npm 依赖，或设置 `BB_BROWSER_COMMAND` 指向本机可执行文件。
- MCP 默认关闭；开启后会在启动时发现已启用的 MCP Server，并把 Tool 以 `server.tool` 名称追加到 Agent 工具列表，同时按需读取 Resource 和 Prompt。单个 Server 失败只会显示降级诊断，不影响内置工具。
- Agent 不再限制单轮连续工具步骤；AI 返回空响应时会最多重试 5 次，每次请求超时 180 秒。可通过环境变量调整：

```powershell
$env:AGENT_REQUEST_RETRY_COUNT = "5"
$env:AGENT_REQUEST_TIMEOUT_SECONDS = "180"
$env:AGENT_COMMAND_TIMEOUT_SECONDS = "360"
python main.py
```

## 安装依赖

运行终端工作台需要 Python `>=3.9,<4.0`。

```powershell
pip install -r requirements.txt
```

## 运行

### 终端 TUI

```powershell
python main.py
```

从 IDE、测试窗口或普通命令行运行时，Windows 会自动弹出独立 PowerShell 窗口。

### 本地 HTTP/SSE API

```powershell
$env:OMNICRAWL_API_TOKEN = "请替换为随机长令牌"
python -m omnicrawl.api
```

默认监听 `127.0.0.1:8765`。Swagger UI 位于 `http://127.0.0.1:8765/docs`，
机器可读契约位于 `/openapi.json`。除健康检查和文档外，所有接口必须携带
`Authorization: Bearer <token>`。完整接入说明见 `docs/API.md`。

运行后：

- TUI 会启动为全屏 Textual 工作台，左侧显示工作区与运行时摘要，主区保留对话、工具记录和状态，输入框固定在底部。
- 在输入框按 Enter 发送消息；任务生成期间输入框不会提交新的消息。
- 需要人工审批的工具会显示居中确认模态框；选择“允许执行”或“拒绝”后继续，按 `Ctrl+C` 会取消当前任务并拒绝等待中的确认。
- 输入 `/new`：清空模型对话历史，开启新对话。
- 输入 `/skills`：查看已加载的 Skill；输入 `/skill:<名称> 任务` 可手动调用指定 Skill。
- 输入 `/mcp`：查看 MCP 开关、Server 连接状态、已发现能力和最近诊断。
- 输入 `/approval`：查看当前工具审批模式；输入 `/approval:manual`、`/approval:auto`、`/approval:review` 可切换审批模式并同步写入 `config.json`。
- 任务执行中按 `Ctrl+C`：请求取消当前操作；空闲时按 `Ctrl+C` 退出工作台。`Ctrl+L` 只清空当前视图，不清空会话数据；也可以输入 `退出`、`结束` 或关闭窗口。

终端 UI 的设计和限制见 `docs/TERMINAL_UI.md`。
Skill 安装、编写和渐进式披露规范见 `docs/SKILL_INSTALLATION.md`。
运行时系统提示词模板见 `omnicrawl/agent/system_prompt.md`；模板只保留工具协议和按场景读取文档的路由说明，具体规范按需读取对应文档。

如果需要从固定位置启动 Agent 但操作另一个项目，可以显式指定工作区：

```powershell
$env:AI_WORKSPACE_ROOT = "D:\path\to\your-project"
python main.py
```

## 项目结构

```text
.
├── main.py                  # 程序启动入口，保持 python main.py 运行方式
├── omnicrawl/               # Agent、API、LLM 和终端 UI 业务模块
│   ├── api/                 # FastAPI + SSE 接口
│   └── agent/system_prompt.md # 运行时系统提示词模板
├── docs/                    # 设计说明和实现文档
├── config.example.json      # 本地配置模板
└── requirements.txt         # Python 依赖
```

## 可选配置

LLM 的 API Key、接口地址和模型必须通过 `config.json` 或环境变量提供。推荐写入项目目录下的 `config.json`。

先复制 `config.example.json` 为 `config.json`，再填写自己的密钥：

```json
{
  "llm": {
    "api_key": "你的 API Key",
    "base_url": "https://xxx.xx/v1",
    "model": "deepseek-v4-flash",
    "thinking_type": "disabled",
    "reasoning_effort": ""
  },
  "approval": {
    "mode": "manual"
  },
  "api": {
    "bearer_token": "请替换为随机长令牌",
    "host": "127.0.0.1",
    "port": 8765,
    "allowed_origins": ["http://localhost:5173"],
    "confirmation_timeout_seconds": 300
  },
  "agent_temp": {
    "enabled": true,
    "directory": ".agent_tmp",
    "cleanup_enabled": true,
    "cleanup_interval_hours": 24
  },
  "mcp": {
    "enabled": false,
    "default_timeout_seconds": 30,
    "max_tool_output_chars": 6000,
    "servers": {
      "local_project": {
        "enabled": true,
        "transport": "stdio",
        "command": "python",
        "args": ["-m", "omnicrawl.mcp.server"],
        "env": {},
        "timeout_seconds": 360,
        "risk_level": "trusted"
      }
    },
    "policy": {
      "require_confirmation_for_write": true,
      "require_confirmation_for_command": true,
      "allow_external_network_tools": false,
      "audit_log_enabled": true
    }
  }
}
```

`config.json` 已加入 `.gitignore`，不要把真实密钥写进 `config.example.json` 或源码。

工具审批可在 `config.json` 的 `approval.mode` 配置：

- `manual`：默认人工确认。
- `auto`：完全自动批准所有受限工具调用。
- `review`：仅对疑似删除行为使用同一模型的非思考模式审查，审查通过后自动执行；非删除工具调用自动放行，不再进入模型审查。

思考深度可在 `config.json` 的 `llm.reasoning_effort` 配置，支持 `none`、`low`、`medium`、`high`、`xhigh`、`max`；也兼容 `x-high`、`x_high` 等写法。设置为 `low` 及以上会自动启用 thinking。

模型列表会按当前 `llm.base_url` 自动请求 OpenAI 兼容的 `/models` 接口检测。TUI 中输入 `/model` 可查看可用模型，输入 `/model <模型ID>` 可实时切换并写回 `config.json`；API 使用 `/api/v1/models` 和 `/api/v1/models/current`。若设置了 `OPENAI_MODEL` 环境变量，重启后仍会优先使用环境变量。

MCP 可在 `config.json` 的 `mcp` 段配置。当前实现支持本地 `stdio` MCP Server 的初始化、能力发现、工具调用、Resource 读取、Prompt 获取、审计日志和 `/mcp` 状态诊断；`streamable_http` 会被识别但暂不连接。内置 `local_project` Server 可通过 `python -m omnicrawl.mcp.server` 暴露当前项目只读文件、搜索、命令工具、项目文档 Resource 和常用 Prompt。bb-browser 不通过 MCP 接入，统一由内置 `bb_browser_cli` 工具调用 CLI。环境变量 `MCP_ENABLED`、`MCP_DEFAULT_TIMEOUT_SECONDS` 和 `MCP_MAX_TOOL_OUTPUT_CHARS` 可临时覆盖全局配置。MCP 的渐进式阅读、配置、调用和排障规范见 `docs/MCP_USAGE.md`。

如果没有 `config.json`，必须设置对应环境变量；如果同时存在，环境变量优先，便于临时覆盖本地配置：

```powershell
$env:OPENAI_API_KEY = "你的 API Key"
$env:OPENAI_MODEL = "deepseek-v4-flash"
$env:OPENAI_THINKING_TYPE = "disabled"
$env:OPENAI_BASE_URL = "https://xxx.xx/v1"
python main.py
```

如果要强制使用某个语音后端：

```powershell
$env:TTS_BACKEND = "system_speech"
python main.py
```

`pyttsx3` 在部分 Windows/Anaconda 环境会因为 SAPI COM 组件注册异常报“没有注册类”。程序默认不会再走这个后端；确实要测试时可设置。注意：`pyttsx3` 的跨线程打断能力不如默认的 `System.Speech` 稳定：

```powershell
$env:TTS_BACKEND = "pyttsx3"
python main.py
```

如果一直显示“未检测到语音”，通常是默认麦克风选错。优先在启动时按列表序号选择，或用设备名关键字固定选择：

```powershell
$env:MIC_DEVICE_KEYWORD = "Realtek"
python main.py
```

高级排查时也可以指定 PyAudio 底层设备编号：

```powershell
$env:MIC_DEVICE_INDEX = "22"
python main.py
```

Windows 上 PyAudio 会把同一物理设备通过多个音频后端重复列出。程序默认只展示更接近系统设置的 WASAPI 输入端点；如果需要排查全部 PortAudio 输入设备：

```powershell
$env:MIC_SHOW_ALL_INPUTS = "1"
python main.py
```
