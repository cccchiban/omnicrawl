# AI 语音 Agent

本目录实现命令行 AI 语音 Agent：

1. 麦克风录音并识别为文字。
2. 使用 OpenAI Python SDK 调用 `https://xxx.xx/v1` 的 Responses API 兼容接口。
3. AI 会按 Agent 循环处理任务：理解目标、读取项目文件、搜索文本、写文件或执行命令；默认会在工具执行前拦截确认，也可开启自动审批模式。
4. AI 回复会在命令行中显示，并由 `ai_voice_agent/text_to_speech.py` 按句子分段排队播报。

## Agent 能力

程序启动后，普通对话会直接进入 Agent 模式，不需要额外输入 `/agent`。

内置工具：

- `list_files`：列出工作区文件，默认执行前会要求确认。
- `read_file`：读取工作区内 UTF-8 文本文件，默认执行前会要求确认。
- `search_text`：在工作区内搜索文本或正则，默认执行前会要求确认。
- `replace_text`：替换单个文件中的文本，默认执行前会要求确认。
- `write_file`：写入或追加文件，默认执行前会要求确认。
- `run_command`：以工作区为当前目录执行任意命令、脚本或 shell 片段，默认执行前会要求确认。

安全边界：

- 文件工具只能访问当前项目目录内的路径；`config.json`、`.env`、`.git`、虚拟环境和缓存目录仍是受保护路径。
- `approval.mode` 默认为 `manual`，所有受限工具都会先在终端显示确认页；确认界面默认选中 `YES`，左右箭头可切换 `YES`/`NO`；按 Enter 提交当前选项，按 `Y` 直接执行，按 `N` 直接取消并把失败结果返回给 AI 继续处理。
- `approval.mode` 设为 `auto` 时完全自动批准受限工具；设为 `review` 时会用同一模型的非思考模式审查本次工具调用，审查通过才执行。自动模式不显示确认页，只显示步骤和执行记录。
- 命令工具不是系统级沙箱；程序会用 `shell=True` 执行用户确认后的命令字符串。确认前请检查命令内容，尤其是删除、移动、覆盖、联网下载、安装依赖、修改系统配置等操作。
- MCP 默认关闭；开启后会在启动时发现已启用的 MCP Server，并把 Tool 以 `server.tool` 名称追加到 Agent 工具列表，同时按需读取 Resource 和 Prompt。单个 Server 失败只会显示降级诊断，不影响内置工具。
- Agent 不再限制单轮连续工具步骤；AI 返回空响应时会最多重试 5 次，每次请求超时 180 秒。可通过环境变量调整：

```powershell
$env:AGENT_REQUEST_RETRY_COUNT = "5"
$env:AGENT_REQUEST_TIMEOUT_SECONDS = "180"
$env:AGENT_COMMAND_TIMEOUT_SECONDS = "180"
python main.py
```

## 安装依赖

```powershell
pip install -r requirements.txt
```

如果 `PyAudio` 安装失败，建议先升级 pip：

```powershell
python -m pip install --upgrade pip setuptools wheel
pip install -r requirements.txt
```

## 运行

```powershell
python main.py
```

从 IDE、测试窗口或普通命令行运行 `python main.py` 后，程序会自动弹出一个独立 PowerShell 窗口，真实语音 Agent 在新窗口中进行。

运行后：

- 程序会在普通终端历史里显示一个灰色封口、淡蓝色文字的配置面板，然后进入内联对话；`>` 表示用户输入，`^` 表示 AI 回复。
- 启动时如果检测到多个录音输入设备，会要求输入列表中的麦克风序号；不知道选哪个时优先尝试 `Realtek`、`麦克风阵列` 或你实际插入的耳机麦克风。
- Windows 上默认使用 `System.Speech` 语音后端，避免 `pyttsx3` 初始化报“没有注册类”。
- 语音播报使用 FIFO 队列，按句子顺序播报；Windows `System.Speech` 后端会复用长驻语音进程，减少句间卡顿；一轮播报完成后才进入下一轮输入。
- AI 回复朗读过程中按 Enter 可立即打断本轮朗读；也可以直接输入下一句并回车，程序会打断朗读并把这句作为下一轮问题。
- 直接按 Enter：开始录音识别。
- 直接输入文字：跳过录音，用键盘内容交给 Agent 处理。
- 输入 `/new`：清空模型对话历史，开启新对话。
- 输入 `/skills`：查看已加载的 Skill；输入 `/skill:<名称> 任务` 可手动调用指定 Skill。
- 输入 `/mcp`：查看 MCP 开关、Server 连接状态、已发现能力和最近诊断。
- 输入 `/approval`：查看当前工具审批模式；输入 `/approval:manual`、`/approval:auto`、`/approval:review` 可切换审批模式并同步写入 `config.json`。
- 执行中按 `Ctrl+C`：取消当前操作并回到输入栏。
- 输入栏按 `Ctrl+C`：第一次取消输入，连续第二次退出程序；也可以输入 `退出`、`结束` 或关闭窗口。

终端 UI 的设计和限制见 `docs/TERMINAL_UI.md`。当前版本不新增第三方依赖，使用普通终端内联 UI。
Skill 安装、编写和渐进式披露规范见 `docs/SKILL_INSTALLATION.md`。
运行时系统提示词模板见 `ai_voice_agent/system_prompt.md`；模板只保留工具协议和按场景读取文档的路由说明，具体规范按需读取对应文档。

## 项目结构

```text
.
├── main.py                  # 程序启动入口，保持 python main.py 运行方式
├── ai_voice_agent/          # Agent、LLM、语音和终端 UI 业务模块
│   └── system_prompt.md     # 运行时系统提示词模板
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
    "thinking_type": "disabled"
  },
  "voice": {
    "speech_to_text_enabled": true,
    "text_to_speech_enabled": true
  },
  "approval": {
    "mode": "manual"
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
        "args": ["-m", "ai_voice_agent.mcp.server"],
        "env": {},
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

语音功能可在 `config.json` 的 `voice` 段分别开关：

- `speech_to_text_enabled`：是否启用语音转文字。设为 `false` 后，直接按 Enter 不再录音，本次会话改用键盘输入。
- `text_to_speech_enabled`：是否启用文字转语音。设为 `false` 后，AI 回复只显示在命令行，不再朗读。

工具审批可在 `config.json` 的 `approval.mode` 配置：

- `manual`：默认人工确认。
- `auto`：完全自动批准所有受限工具调用。
- `review`：使用同一模型的非思考模式审查工具调用，审查通过后自动执行。

MCP 可在 `config.json` 的 `mcp` 段配置。当前实现支持本地 `stdio` MCP Server 的初始化、能力发现、工具调用、Resource 读取、Prompt 获取、审计日志和 `/mcp` 状态诊断；`streamable_http` 会被识别但暂不连接。内置 `local_project` Server 可通过 `python -m ai_voice_agent.mcp.server` 暴露当前项目只读文件、搜索、命令工具、项目文档 Resource 和常用 Prompt。环境变量 `MCP_ENABLED`、`MCP_DEFAULT_TIMEOUT_SECONDS` 和 `MCP_MAX_TOOL_OUTPUT_CHARS` 可临时覆盖全局配置。MCP 的渐进式阅读、配置、调用和排障规范见 `docs/MCP_USAGE.md`。

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
