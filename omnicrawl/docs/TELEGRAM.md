# Telegram 远程接入（Bot）说明

OmniCrawl 通过 `omnicrawl/connectors/telegram.py` 提供 **Telegram Bot 远程接入**：
用户创建自己的 Bot 后，可在手机/任意设备上的 Telegram 中远程操作当前工作区的
OmniCrawl Agent——发送任务文本、审批敏感工具调用、查看状态、管理会话、切换工作区、
接收文件并交给 Agent 处理。

本文档面向两种读者：
1. **使用者**：如何创建 Bot、配置并启动 tg 客户端。
2. **AI 助手（配置场景）**：当用户说"配置 tg bot / 配 Telegram / 远程控制"时，
   按本文档帮用户完成从创建 Bot 到启动验证的全流程。

## 工作原理

- 基于 Telegram Bot API 的 **getUpdates 长轮询**（无需公网 IP、无需 webhook、无需域名）。
- 零新增依赖：仅使用 `requests` 调用 Telegram HTTP API。
- tg 客户端是**独立进程**，与 TUI/API 完全解耦：
  - TUI 启动时若 Telegram 配置同时包含 Bot Token 和授权用户 ID，会自动拉起该子进程；
  - TUI 关闭时会自动终止该子进程及其后代；
  - 未配置 token 或授权用户 ID 时不会自动启动；手工执行
    `python -m omnicrawl.connectors.telegram` 仍只会打印中文提示并以退出码 1 结束。
  - 自动启动失败只记录警告，不阻塞 TUI；设置
    `OMNICRAWL_AUTO_START_CONNECTORS=0` 可关闭 Telegram/飞书自动联动。
- 跨进程同步：推理强度（`/reasoning`）、审批模式（`/approval`，默认 `review` 自动审查）与工作区（`/workspace`）通过 `config.toml`
  持久化；tg 客户端每次任务开始前重读配置并应用到当前 Agent，TUI 侧重启或下次
  读取时同样生效。Telegram 远程仅支持 `manual`/`review`，禁止 `auto`（完全自动仅限本地 TUI）；若磁盘上为 `auto`，Telegram 侧按 `review` 降级生效。

## 配置步骤（AI 引导用户完成）

### 1. 创建 Bot 并获取 Token

1. 在 Telegram 中与 [@BotFather](https://t.me/BotFather) 对话。
2. 发送 `/newbot`，按提示设置 Bot 显示名称和用户名（用户名以 `bot` 结尾）。
3. BotFather 会返回 **Token**（形如 `123456789:AA...`）。**该 Token 即远程操作凭证，必须保密**。

### 2. 获取授权用户 ID（安全白名单）

1. 与 [@userinfobot](https://t.me/userinfobot) 对话（或任意能查 ID 的机器人）。
2. 它会返回你的数字 **User ID**（形如 `123456789`）。只允许该 ID 的用户操作你的 Bot。

### 3. 写入配置（二选一，环境变量优先级更高）

**方式 A：config.toml**（推荐，随 OmniCrawl 用户配置统一管理）

在 `config.toml` 顶层追加（与 `config.example.toml` 的 `[telegram]` 段一致）：

```toml
[telegram]
# 由 @BotFather 创建 Bot 后获取，替换为实际 Token，勿提交真实 Token 到仓库。
bot_token = "123456789:AAxxxxxxxxxxxxxxxxxxxx"
# 允许远程操作的用户 ID 列表（安全白名单，必填；向 @userinfobot 可查自己的 ID）。
allowed_user_ids = [123456789]
# 敏感工具（bash/powershell）确认超时秒数，超时自动拒绝。
confirmation_timeout_seconds = 300
```

**方式 B：环境变量**

```powershell
$env:TELEGRAM_BOT_TOKEN = "123456789:AAxxxxxxxxxxxxxxxxxxxx"
$env:TELEGRAM_ALLOWED_USER_IDS = "123456789"          # 逗号分隔可配多个
$env:TELEGRAM_CONFIRM_TIMEOUT = "300"
```

配置读取规则（`load_telegram_config`）：环境变量 > config.toml `[telegram]` 段。
缺失任一必填项（token / 白名单）时，`main()` 打印明确提示并以退出码 1 结束。

### 4. 启动

启动 TUI（`ocl`、`omnicrawl`、`python -m omnicrawl` 或源码目录下的
`python main.py`）时，满足配置条件会自动启动 Telegram 子进程。也可以继续手工独立启动：

```powershell
python -m omnicrawl.connectors.telegram
```

如需排障或只运行 TUI，可在启动前关闭自动联动：

```powershell
$env:OMNICRAWL_AUTO_START_CONNECTORS = "0"
```

启动成功的标志：日志输出 `Telegram Bot 已启动（polling），允许用户：[...]`。
随后在 Telegram 中给自己的 Bot 发 `/status` 做冒烟验证。

**关闭**：`Ctrl+C`（`KeyboardInterrupt` 会优雅停止轮询并回收 Agent 资源）。

## 支持的命令

### 基础命令（connector 层直接处理）

| 命令 | 作用 |
|------|------|
| `/start` | 查看使用说明 |
| `/status` | harness 状态（工作区、会话、是否忙碌） |
| `/session` | 当前会话 ID |
| `/reset` | 开启新会话（清空对话历史） |
| `/cancel` | 取消当前任务 |
| `/thinking on\|off` | 查看/切换思考内容显示（默认关闭） |
| `/workspace [路径]` | 查看/切换工作区（切换会持久化并同步到 TUI） |
| `/approve` | 批准当前等待确认的工具调用 |
| `/reject` | 拒绝当前等待确认的工具调用 |

命令兼容 `@BotName` 后缀（如 `/status@MyBot`）。未授权用户的消息（含命令）一律
忽略且不回复，只留日志。

**审批归属**：敏感工具确认只能由**发起该任务的白名单用户本人**批准/拒绝
（`/approve` `/reject`）；同一群聊里的其他白名单用户无权代为审批。
**取消与确认**：`/cancel` 会同时释放挂起的确认请求（按拒绝处理），
确认回调不会悬挂到超时。

### harness 管理命令（转发 TUI 同一套 slash.py 实现）

`/sessions`、`/archives`、`/archive`、`/history`、`/undo`、`/compact`、
`/rename`、`/resume <id>`、`/resume latest`（直接恢复最近活动会话）、`/plan`
（启用主 Agent 计划模式，后续任务追加 `omnicrawl/templates/plan.md`）、
`/tasks`、`/task cancel`、`/approval`、`/reasoning`、`/skills`、
`/memory:clean`、`/mcp`、`/plugins`、`/review`（派生评审子 Agent：完整 git
权限 + 自动批准收集 diff，按结构化 JSON 输出审查结果）等。

Telegram 远程：`/approval` 查看当前模式，`/approval:manual|review`（含 `/auto-approve:off`、`/auto-review:on`）可远程切换并持久化到 `config.toml`，跨进程同步生效；`/approval:auto`（含 `/auto-approve:on`）在远程被拒绝并提示“仅限本地 TUI”。未知 `/` 命令返回提示且不启动任务。默认审批为 `review`（自动审查）。

### 文件接收

直接发送图片/文档/视频/语音等文件，会自动下载并按类型分类存入工作区
`.omnicrawl/.agent_tmp/` 的 `images/` `videos/` `scripts/` `code/` `audio/`
`files/` 子目录，然后 Bot 把「已收到文件：位于 <相对路径>」作为任务文本交给
Agent 处理；消息 `caption` 作为补充说明一并附带（例如发截图时写"提取里面的文字"）。
文件下载/保存在**后台线程**执行，不阻塞轮询线程——下载大文件期间
`/cancel`、`/status` 等命令与新消息仍可正常响应。

## 安全边界

- **白名单必填**：`allowed_user_ids` 为空时拒绝启动，防止裸奔。
- **未授权零回复**：非白名单用户的文本/命令/文件全部忽略，不提示、不执行。
- **Token 是真正的风险面**：持有 Token 者可绕过 Bot 程序直接调用 Telegram API，
  白名单形同虚设。Token 只能放环境变量或本地 config.toml，`.gitignore` 排除，
  绝不提交仓库；怀疑泄露时用 BotFather `/revoke` 作废重发。
- **工具确认**：`manual`/`review` 模式下敏感工具（bash/powershell）请求确认时，通过 `/approve`、`/reject`
  交互，**仅限发起任务的白名单用户本人**可审批（群聊中他人无权代批）；
  超时自动拒绝，`/cancel` 取消任务时同步释放挂起的确认。
  `auto`（完全自动）仅限本地 TUI 配置，Telegram 远程不支持——远程默认 `review`，且若磁盘上为 `auto` 会在 Telegram 侧按 `review` 降级生效，不自动放行。
- **脱敏**：所有回传用户的错误信息经 `redact_sensitive_text` 脱敏，不泄露路径/密钥。
- **并发警告**：不要同时用 TUI 和 tg 客户端驱动**同一个会话**（并发写 JSONL 会损坏
  会话转录）。会话存档共享于 `~/.omnicrawl/.agent_sessions`，跨端切换用
  `/sessions` + `/resume <id>`，或 `/resume latest` 一步恢复最近会话。

## 限制

- 文本命令与文件消息；不支持语音转写（语音仅落盘到 `audio/` 交 Agent 处理）。
- 长任务取消依赖模型响应间隙的 `cancel_check`。
- 流式回答按约 1 秒粒度编辑消息（Telegram 频率限制），非逐 token。

## 健壮性行为（P3 修复）

- **流式输出不丢内容**：任务失败/取消时，已流式输出的部分会保留在流式消息中
  （追加「…（输出中断）」标记），错误/取消提示单独一条消息发送，不覆盖。
- **关闭流程**：`close()` 先释放挂起的工具确认（按拒绝）并请求取消，再等待
  任务线程自行退出（上限 10s），最后才关闭 Agent；文件下载线程在服务停止后
  不再启动新任务。
- **错误退出**：轮询遇到不可恢复的 Telegram API 错误（Token 失效等）时打印
  清晰消息并以退出码 1 结束，不输出原始 traceback。
- **配置同步开销**：每次任务前同步 TUI 侧配置时，先按 config.toml 的 mtime
  判断是否有变化，未变则跳过重读（任务间零磁盘 I/O）。

## 文件边界

- `omnicrawl/connectors/telegram.py`：Bot 轮询服务、命令分发、任务执行、工具确认桥。
- `omnicrawl/connectors/__init__.py`、`README.md`：包说明与使用约定。
- `omnicrawl/config/workspace.py`：`[workspace] root` 持久化（跨端工作区同步）。
- `tests/test_telegram_connector.py`：回归测试（mock Telegram API 与 Agent，无网络）。
- `omnicrawl/docs/TELEGRAM.md`：本文档。

## 验证清单

配置或修改后至少执行：

```powershell
python -m pytest tests/test_telegram_connector.py tests/test_api_module_boundaries.py -q
python -m compileall -q omnicrawl/connectors/telegram.py
```

手工冒烟：给自己 Bot 发 `/status` 应返回状态文本；发普通文本应启动任务并流式回发；
发一张图片应出现「✅ 已收到文件：位于 .omnicrawl/.agent_tmp/images/...」；
未授权用户发消息应无任何回复。
