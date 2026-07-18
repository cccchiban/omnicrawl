# OmniCrawl

本目录实现本地 OmniCrawl，提供终端 TUI 和本机 HTTP/SSE API：

1. TUI 使用键盘交互，HTTP API 供后续 Web 或桌面前端接入。
2. 通过统一模型运行时调用多家协议：OpenAI Chat Completions / Responses、Anthropic Messages、Google Gemini Generate Content（各用原生 SDK）。
3. AI 会按 Agent 循环处理任务：理解目标、读取项目文件、搜索文本、写文件或执行命令；默认会在工具执行前拦截确认，也可开启自动审批模式。
4. AI 回复会在 TUI 中显示，或通过 SSE 事件流推送给 API 客户端。
5. 支持多 Profile、`models.yaml` 自定义模型目录，以及 TUI `/model` 双列热切换（不重启、不清空会话）。
6. 提供默认关闭的定义式 SubAgent：支持 1–4 个 `fresh`/受控 `fork` 任务、后台管理、模型覆盖、跨进程安全快照恢复和 Session/SSE/TUI/API 生命周期观察。默认角色仅只读；`verify` 只能运行固定检查；通用写 Agent 必须显式开启，并优先在独立 Git worktree 中执行，由父 Agent 决定应用或丢弃结果。

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
- `subagent`：仅在 `subagents.enabled=true` 时注册；支持有界批量 `run`、后台 `spawn`、`list/get/cancel`，以及显式开启后的 `fork`、模型覆盖和 Worktree `list/apply/discard` 控制。默认角色仅只读；`verify` 只能调用固定检查标识，通用写 Agent 与 Worktree 均需额外开关。

安全边界：

- 文件工具只能访问当前项目目录内的路径；`config.yaml`/`config.json`、`models.yaml`、`.env`、`.git`、虚拟环境和缓存目录仍是受保护路径。
- `approval.mode` 默认为 `manual`，所有受限工具都会先在终端显示确认页；按 `Enter`、`Y` 或 `1` 允许，按 `N` 或 `2` 拒绝；方向键只会被消费，不会触发工具执行。
- `approval.mode` 设为 `auto` 时完全自动批准受限工具；设为 `review` 时只把疑似删除行为交给同一模型的非思考模式审查，其他工具调用自动执行。自动模式不显示确认页，只显示步骤和执行记录。
- 命令工具不是系统级沙箱；所有命令均通过明确的 PowerShell 或 Git Bash 解释器以 `shell=False` 启动。确认前请检查命令内容，尤其是删除、移动、覆盖、联网下载、安装依赖、修改系统配置等操作。
- bb-browser 是内置 CLI 能力，不通过 MCP 暴露；需要安装或更新时使用项目里的 npm 依赖，或设置 `BB_BROWSER_COMMAND` 指向本机可执行文件。
- MCP 默认关闭；开启后会在启动时发现已启用的 MCP Server，并把 Tool 以 `server.tool` 名称追加到 Agent 工具列表，同时按需读取 Resource 和 Prompt。单个 Server 失败只会显示降级诊断，不影响内置工具。
- SubAgent 默认关闭。read_only 角色只能使用工作区读取/搜索和可用的 Memory 只读工具；`memory_write`、MCP Tool、Skill 控制面、浏览器、父控制面和再次创建 SubAgent 均不会因父 Host 已注册而进入子工具集。模型任务参数也不能提交自定义工具、Skill、MCP Server 或 permission profile。显式设置 `subagents.enable_verify_agent=true` 后，内置 `verify` 额外获得子任务私有的 `verify_command`：只能选择 `unit_tests`、`compileall`、`git_diff_check` 三项固定检查，Host 以静态 argv 和 `shell=False` 启动。通用写 Agent 还需显式开启 standard/worktree 开关，写入和变更性操作继续经过来源明确的审批。任务并发与模型请求并发分别受配置上限约束，父历史、活动 Skill、Runtime 字段和普通 Session 消息不会被 fresh 子循环覆盖；Fork 只消费创建时冻结且已脱敏的父公开上下文。父 Session 归档/恢复会先取消旧会话子任务，待处理 Worktree 会阻止切换工作区，避免旧任务或旧仓库写能力进入新的所有权边界。结果会先脱敏和裁剪，大结果写入父 Session 管控的 JSON artifact；Provider reasoning 不进入公开结果，API/TUI 只接收安全生命周期摘要。
- Agent 不再限制主循环的连续工具步骤；SubAgent 则按角色定义和全局配置限制模型回合、工具次数与调用边界时间。时间预算会阻止继续启动新步骤，并收紧单次模型请求超时，但无法强制终止不响应取消的第三方 SDK 或系统调用。AI 返回空响应时会最多重试 5 次，每次请求超时 180 秒。可通过环境变量调整：

```powershell
$env:AGENT_REQUEST_RETRY_COUNT = "5"
$env:AGENT_REQUEST_TIMEOUT_SECONDS = "180"
$env:AGENT_COMMAND_TIMEOUT_SECONDS = "360"
python main.py
```

## Hook 插件（NPM）

OmniCrawl 支持在 Host 生命周期节点分发 Hook，并通过独立 Node Worker 加载 NPM 插件。默认关闭，不影响现有 TUI / Skill / MCP / Session。

```powershell
# 诊断环境（以下三条等价）
python main.py plugin doctor
python -m omnicrawl plugin doctor
omnicrawl plugin doctor   # 需先 pip install -e .

# 启用全局插件系统（写入 config.yaml/config.json 的 plugins.enabled）
python main.py plugin system enable

# 注册本地开发插件（dev 模式，不进可回滚 store）
python main.py plugin install .\path\to\plugin --dev --project

# 从 NPM 安装（需 Node 20+；强制 --ignore-scripts）
python main.py plugin install @scope/name@1.2.3 --project --enable --yes

python main.py plugin list
python main.py plugin disable @scope/name --project
python main.py plugin rollback @scope/name --project
```

TUI 内可输入 `/plugins` 查看当前 Worker 只读状态；安装/更新/卸载仍走进程级 CLI。输入 `/settings` 可打开中文设置面板，修改模型、推理强度、审批模式、记忆、MCP、插件和子任务总开关；修改立即生效并持久化到当前项目配置。高风险的 Worktree、共享写入和网络安装等细项不会通过面板开放。

设计说明见 `docs/HOOK_PLUGIN_DESIGN.md`。注意：Worker 隔离用于故障边界，**不是**恶意代码沙箱；只安装可信插件。插件若要提供最低优先级的 Agent Markdown 定义，必须在 manifest 的 `omnicrawl.agents` 中声明包内路径，并同时声明且获批 `agent:definitions` 权限。

## 安装依赖

运行终端工作台需要 Python `>=3.9,<4.0`。

```powershell
pip install -r requirements.txt

# 可选：安装 console script（omnicrawl）
# 若本机 pip/setuptools 较旧，加 --no-build-isolation 更稳
pip install -e . --no-build-isolation
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

- TUI 会启动为全屏 Textual 工作台，顶部 HUD 显示工作区、模型、推理、审批和 Token 摘要，主区保留对话与工具记录，输入框固定在底部。
- 在输入框按 Enter 发送消息；任务生成期间输入框不会提交新的消息。
- 需要人工审批的工具会显示居中确认模态框；选择“允许执行”或“拒绝”后继续，按 `Ctrl+C` 会取消当前任务并拒绝等待中的确认。
- 输入 `/new`：清空模型对话历史，开启新对话。
- 输入 `/skills`：查看已加载的 Skill；输入 `/skill:<名称> 任务` 可手动调用指定 Skill。
- 输入 `/mcp`：查看 MCP 开关、Server 连接状态、已发现能力和最近诊断。
- 输入 `/model`：打开双列模型选择界面（自定义 `models.yaml` + API 自动发现）；列表默认获得焦点，使用 `↑↓` 选择、`←→` 切列、`Enter` 确认，按 `/` 可进入搜索框；`/model --refresh` 刷新发现缓存；`/model <key|alias|model_id|profile/model_id>` 直接切换。
- 输入 `/approval`：查看当前工具审批模式；输入 `/approval:manual`、`/approval:auto`、`/approval:review` 可切换审批模式并同步写入配置文件。
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
├── main.py                     # 程序启动入口，保持 python main.py 运行方式
├── omnicrawl/                  # 业务模块包
│   ├── __init__.py              # 旧路径兼容导出（session/memory/llm 等）
│   ├── agent/                   # Agent 主循环与工具/协议/历史
│   │   ├── __init__.py           # 稳定公共 API 导出
│   │   ├── core.py               # LocalToolAgent 外层生命周期与 Host 接线
│   │   ├── execution.py           # 可复用 AgentLoopRunner
│   │   ├── subagents/             # 定义注册表、协调器及内置角色/后台/恢复/Worktree
│   │   ├── tools.py / history.py / llm_protocol.py
│   │   └── system_prompt.md       # 运行时系统提示词模板
│   ├── mcp/                     # MCP 配置、安全、审计、客户端与内置 Server
│   ├── api/                     # FastAPI + SSE 接口
│   │   ├── app.py / service.py / models.py / deps.py
│   │   └── routes/                # 按资源分组的 HTTP 路由
│   ├── state/                   # Session、Memory、Project 存储
│   │   ├── session*.py            # 会话门面与子域
│   │   └── memory*.py             # 记忆存储与排序纯逻辑
│   ├── config/                  # LLM、审批、运行时与模型目录
│   │   ├── llm.py / llm_multi.py # LLMConfig、多 Profile 与 active_model
│   │   ├── llm_client.py         # OpenAI Responses 网络客户端（遗留/审查）
│   │   ├── model_catalog.py      # 自定义 + 自动发现双列目录
│   │   ├── model_store.py        # models.yaml
│   │   ├── migration.py          # config.json → config.yaml 迁移
│   │   └── runtime.py            # YAML 优先配置仓库
│   ├── llm/                     # 统一模型协议、Runtime Manager、Provider Adapter
│   │   └── providers/            # openai_chat / openai_responses / anthropic / gemini
│   ├── workspace/               # 工作区工具、Monitor、临时目录
│   │   ├── monitor.py            # 后台任务生命周期
│   │   └── process_control.py    # Windows Job Object 平台实现
│   ├── ui/                      # 全屏 TUI、输入与兼容输出
│   │   └── fullscreen/           # Textual 工作台（含 model_picker）
│   ├── commands/                # 斜杠命令
│   └── extensions/              # Skill 等扩展
├── docs/                       # 设计说明和实现文档
├── tests/                      # 单元与模块边界回归
├── config.example.yaml         # 推荐：多模型运行配置模板
├── models.example.yaml         # 自定义模型目录模板
├── config.example.json         # 兼容旧 JSON 模板
└── requirements.txt            # Python 依赖
```

旧导入路径仍可用（例如 `omnicrawl.session`、`omnicrawl.memory`、`omnicrawl.llm`、`omnicrawl.skill`），实现位于上述真实子模块。多模型设计与落地状态见 `docs/MULTI_MODEL_API_DESIGN.md`；大文件治理进度见 `docs/agent_refactor_plan.md`。

## 可选配置

### 推荐：YAML 多模型配置

LLM 凭据、Profile 与当前模型推荐通过 `config.yaml` + `models.yaml` 提供。

```powershell
copy config.example.yaml config.yaml
copy models.example.yaml models.yaml
```

`config.yaml` 关键字段：

```yaml
version: 2
llm:
  active_model:
    source: custom
    key: default-chat
  profiles:
    openai-main:
      provider: openai
      base_url: "https://api.openai.com/v1"
      api_key_env: OPENAI_API_KEY
      default_protocol: openai_chat_completions
      discovery:
        enabled: true
```

`models.yaml` 只放模型元信息，**不要写 api_key**：

```yaml
version: 1
models:
  default-chat:
    display_name: "Default Chat Model"
    profile: openai-main
    model_id: gpt-5.2
    protocol: openai_chat_completions
    aliases: [default, chat]
    context_window_tokens: 128000
```

仅有旧 `config.json` 时，程序会优先尝试迁移为 `config.yaml` + `models.yaml`，并留下 `config.json.migrated.bak` 备份。该备份包含原始配置和可能的明文凭据，已默认被 `.gitignore` 排除；确认迁移无误后应删除，并在疑似泄露时轮换对应密钥。也可继续使用 JSON 扁平配置。

### 兼容：JSON 扁平配置

先复制 `config.example.json` 为 `config.json`，再填写自己的密钥：

```json
{
  "llm": {
    "api_key": "你的 API Key",
    "base_url": "https://xxx.xx/v1",
    "model": "deepseek-v4-flash",
    "thinking_type": "disabled",
    "reasoning_effort": "",
    "context_window_tokens": 128000
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

`config.yaml`、`models.yaml`、`config.json` 已加入 `.gitignore`，不要把真实密钥写进示例文件或源码。

工具审批可在配置文件的 `approval.mode` 配置：

- `manual`：默认人工确认。
- `auto`：完全自动批准所有受限工具调用。
- `review`：仅对疑似删除行为使用同一模型的非思考模式审查，审查通过后自动执行；非删除工具调用自动放行，不再进入模型审查。

SubAgent 各项能力通过 `subagents` 段独立启用，所有高风险能力默认关闭：

```yaml
subagents:
  enabled: false
  max_depth: 1
  max_concurrency: 2
  max_tasks_per_batch: 4
  model_request_concurrency: 2
  default_max_turns: 20
  default_max_tool_calls: 50
  default_timeout_seconds: 300
  allow_background: false
  allow_fork: false
  allow_worktree: false
  allow_standard_agent: false
  allow_shared_workspace_writes: false
  enable_verify_agent: false
  verify_command_timeout_seconds: 120
  task_retention_minutes: 60
```

启用后加载顺序为项目 `.omnicrawl/agents/*.md`、兼容项目 `.agents/agents/*.md`、用户 `~/.omnicrawl/agents/*.md`、包内 `explore`/`plan`/`verify`/`general-purpose`、已批准插件定义；同名时高优先级来源获胜。环境变量只能关闭能力或收紧并发、超时等限制，不能扩大配置。当前实现支持 1–4 个同步任务、有界任务并发、独立模型请求限流、输入顺序聚合、失败隔离和同步 `fail_fast`；默认仅只读，显式启用 `verify` 后只能执行固定的全量 unittest、compileall 和 `git diff --check`。显式设置 `allow_fork: true` 后可使用创建时冻结、脱敏的父公开上下文；任务模型按任务 > 角色定义 > 父模型解析并以独立 Runtime 运行。显式设置 `allow_background: true` 后支持 `spawn/list/get/cancel`，终态任务与未消费通知按 TTL 自动回收，通知只注入一次。父 Run 取消、Agent 关闭和工作区切换会级联取消并有界等待；跨进程恢复只导入安全任务快照，非终态任务折叠为 `SUBAGENT_INTERRUPTED`，不自动重跑或恢复 prompt、Runtime、审批和通知。显式开启 `allow_worktree` 与 `allow_standard_agent` 后，`general-purpose` 可在独立 worktree 写入，主工作树脏时拒绝创建/应用，父 Agent 通过控制动作审查并 apply/discard；共享工作区写入还需额外开启 `allow_shared_workspace_writes` 并受单写锁约束。跨父 Run 的后台审批记录仍仅存在于当前进程，不跨进程恢复；后台 `fail_fast` 尚未开放。

思考深度可在 `llm.reasoning_effort`（或 `llm.defaults.reasoning_effort`）配置，支持 `none`、`low`、`medium`、`high`、`xhigh`、`max`；也兼容 `x-high`、`x_high` 等写法。设置为 `low` 及以上会自动启用 thinking。

`context_window_tokens` 可写在自定义模型或 Profile 默认值中。全屏 TUI 顶部 Token 遥测行使用该值计算 `CTX` 占用率；`/model` 切换后 HUD 会刷新，最近 Token 显示会清零。

会话转录、PromptHistory、HTML 工具预览和 MCP 审计日志会清理常见 API Key、Token、Cookie、密码及 Bearer 凭据后再写入新记录。该保护只覆盖后续新写入内容；已存在的本地会话文件不会被程序自动重写，如需清理历史数据请先自行备份并人工审查。

### 模型目录与热切换

- 自定义模型：`models.yaml`（key、alias、protocol、能力、上下文窗口、`max_output_tokens`、`temperature`）。
  其中 `max_output_tokens` / `temperature` 会在每次请求时自动写入对应 Provider 的生成参数（OpenAI=`max_tokens`，Anthropic=`max_tokens`，Gemini=`max_output_tokens`）。
- 自动发现：各启用 Profile 通过对应原生 SDK / 兼容接口探测；失败只写诊断，不阻断自定义模型。
- TUI：`/model` 打开双列选择器；`/model --refresh` 刷新；`/model <选择>` 直接切换。
- API：
  - `GET /api/v1/models`：兼容旧扁平列表
  - `GET /api/v1/models/catalog`：双列 + diagnostics
  - `POST /api/v1/models/refresh`：刷新发现缓存
  - `PUT /api/v1/models/current`：切换当前模型（支持旧 `model` 字段与 `source/key`、`source/profile/model_id`）
- 环境变量：`OMNICRAWL_MODEL`、`OMNICRAWL_PROFILE` 优先；仍兼容 `OPENAI_MODEL` / `OPENAI_API_KEY` / `OPENAI_BASE_URL`。

设计细节与实施状态见 `docs/MULTI_MODEL_API_DESIGN.md`。

MCP 可在配置文件的 `mcp` 段配置。当前实现支持本地 `stdio` MCP Server 的初始化、能力发现、工具调用、Resource 读取、Prompt 获取、审计日志和 `/mcp` 状态诊断；`streamable_http` 会被识别但暂不连接。除内置 `local_project` 的显式只读能力外，MCP Tool 默认需要审批，避免第三方 Server 通过模糊工具名绕过确认。内置 `local_project` Server 可通过 `python -m omnicrawl.mcp.server` 暴露当前项目只读文件、搜索、命令工具、项目文档 Resource 和常用 Prompt。bb-browser 不通过 MCP 接入，统一由内置 `bb_browser_cli` 工具调用 CLI。环境变量 `MCP_ENABLED`、`MCP_DEFAULT_TIMEOUT_SECONDS` 和 `MCP_MAX_TOOL_OUTPUT_CHARS` 可临时覆盖全局配置。MCP 的渐进式阅读、配置、调用和排障规范见 `docs/MCP_USAGE.md`。

如果没有本地配置文件，必须设置对应环境变量；如果同时存在，环境变量优先，便于临时覆盖本地配置：

```powershell
$env:OPENAI_API_KEY = "你的 API Key"
$env:OMNICRAWL_MODEL = "default-chat"   # 或 OPENAI_MODEL=裸模型ID
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
