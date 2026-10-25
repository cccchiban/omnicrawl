# Telegram 远程接入（Bot）说明

OmniCrawl 通过 `rust/crates/omnicrawl-connectors/src/telegram/` 提供 **Telegram Bot 远程接入**：
用户创建自己的 Bot 后，可在手机/任意设备上的 Telegram 中远程操作当前工作区的
OmniCrawl Agent——发送任务文本、审批敏感工具调用、查看状态、管理会话、切换工作区、
接收文件并交给 Agent 处理。

本文档面向两种读者：
1. **使用者**：如何创建 Bot、配置并启动 tg 客户端。
2. **AI 助手（配置场景）**：当用户说"配置 tg bot / 配 Telegram / 远程控制"时，
   按本文档帮用户完成从创建 Bot 到启动验证的全流程。

## 工作原理

- 基于 Telegram Bot API 的 **getUpdates 长轮询**（无需公网 IP、无需 webhook、无需域名）。
- tg 客户端是**独立进程**，与 TUI/API 完全解耦：
  - TUI 启动时若 Telegram 配置同时包含 Bot Token 和授权用户 ID，会自动拉起该子进程；
  - TUI 关闭时会自动终止该子进程及其后代；
  - 未配置 token 或授权用户 ID 时不会自动启动。
  - 自动启动失败只记录警告，不阻塞 TUI；设置
    `OMNICRAWL_AUTO_START_CONNECTORS=0` 可关闭 Telegram/飞书自动联动。
- **由宿主承载**：连接器进程是宿主载荷里的 `omnicrawl-host connector telegram`，因此
  远程任务用的是**与本地 TUI 同一套**工具表、会话目录与斜杠命令——能读写工作区文件、
  跑 shell、用记忆与知识库工具。内核自带的 `omnicrawl --connector telegram` 只自持
  `update_todos` / `ask_user` / `pause_work`，其余工具一律回 `tool_unavailable`、
  会话也不落盘，**只能对话**；只有不带宿主载荷的平台（如 armv7）才会退回那条路径。
- 跨进程同步：推理强度（`/reasoning`）、审批模式（`/approval`，默认 `review` 自动审查）与工作区（`/workspace`）通过 `config.toml`
  持久化；TUI 侧重启或下次读取时同样生效。Telegram 远程仅支持 `manual`/`review`，禁止 `auto`（完全自动仅限本地 TUI）；若磁盘上为 `auto`，Telegram 侧按 `review` 降级生效。

## 配置步骤（AI 引导用户完成）

### 1. 创建 Bot 并获取 Token

1. 在 Telegram 中与 [@BotFather](https://t.me/BotFather) 对话。
2. 发送 `/newbot`，按提示设置 Bot 显示名称和用户名（用户名以 `bot` 结尾）。
3. BotFather 会返回 **Token**（形如 `123456789:AA...`）。**该 Token 即远程操作凭证，必须保密**。

### 2. 获取授权用户 ID（安全白名单）

1. 与 [@userinfobot](https://t.me/userinfobot) 对话（或任意能查 ID 的机器人）。
2. 它会返回你的数字 **User ID**（形如 `123456789`）。只允许该 ID 的用户操作你的 Bot。

### 3. 写入配置

在 `config.toml` 顶层追加（与 `config.example.toml` 的 `[telegram]` 段一致）：

```toml
[telegram]
# 由 @BotFather 创建 Bot 后获取，替换为实际 Token，勿提交真实 Token 到仓库。
bot_token = "123456789:AAxxxxxxxxxxxxxxxxxxxx"
# 允许远程操作的用户 ID 列表（安全白名单，必填；向 @userinfobot 可查自己的 ID）。
allowed_user_ids = [123456789]
# 敏感工具（bash/powershell）确认与提问的超时秒数，超时自动拒绝/放弃作答。
confirmation_timeout_seconds = 300
```

配置读取规则（`load_telegram_config`）：只读 `config.toml` 的 `[telegram]` 段。
缺失任一必填项（token / 白名单）时打印明确提示并以退出码 1 结束。
（历史上还接受 `TELEGRAM_*` 环境变量，Rust 侧已统一为「配置只读 TOML」。）

### 4. 启动

启动 TUI（`ocl`、`omnicrawl` 或源码目录下的 `omnicrawl`）时，满足配置条件会自动启动
Telegram 子进程。也可以继续手工独立启动：

```powershell
# 宿主承载（推荐：真实工具表与会话）
omnicrawl-host connector telegram

# 无宿主载荷的平台（如 armv7）：退回内核承载，只能对话
omnicrawl --connector telegram
```

同一平台同一用户只允许一个活动连接器实例：多个 TUI/API 进程并存，或 TUI
自动启动与手工启动同时存在时，后启动的一方会检测到已有实例并跳过，避免
Telegram 轮询被重复启动。单例机制见 `connectors/README.md`。

**一个平台只能有一个轮询实例**：若另一处（如内核承载的旧进程）仍在轮询同一个
Bot，新实例会持续收到 Telegram 的 `409 Conflict` 并退避重试，表现为「另一个
实例在抢更新」。先停掉旧进程再启动新的。

如需排障或只运行 TUI，可在启动前关闭自动联动：

```powershell
$env:OMNICRAWL_AUTO_START_CONNECTORS = "0"
```

启动成功的标志：日志输出 `Telegram Bot 已启动（polling），允许用户：[...]`。
随后在 Telegram 中给自己的 Bot 发 `/status` 做冒烟验证。

**关闭**：`Ctrl+C`（会优雅停止轮询并回收 Agent 资源）。

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

### 客户端命令菜单

Bot 启动时会用 `setMyCommands` 把宿主承载方的命令表发布成 Telegram 的**命令菜单**
（输入框左侧菜单键点开的 `/xxx` 列表），因此不需要手动在 BotFather 里执行 `/setcommands`：

- 宿主承载（`omnicrawl-host connector telegram`）取 `omnicrawl-commands` 注册表里除
  `ui` 类以外的全部命令（`/quit`、`/settings` 在远程被拒绝，不进菜单）；内核自带路径
  只有 `/status`、`/new`、`/workspace`、`/help` 四条。
- 只有符合 Telegram 命令名规范（`a-z0-9_`、字母开头、≤32 字符）的名字才会进菜单：
  `/memory:clean`、`/approval:review` 这类带分隔符的命令**不出现在菜单里**，但直接输入
  照常可用。
- 发布失败（网络、限流）只记日志，不影响命令分发；菜单只是客户端侧的可视提示。
- 菜单发布对所有私聊生效（`scope` 缺省为默认）；群组菜单由群管理员在群设置里决定，
  Bot 无权修改。

启动成功后日志会多一行 `已发布 N 条命令菜单。`。

**审批归属**：敏感工具确认只能由**发起该任务的白名单用户本人**批准/拒绝
（`/approve` `/reject`）；同一群聊里的其他白名单用户无权代为审批。
**取消与确认**：`/cancel` 会同时释放挂起的确认请求（按拒绝处理）与挂起的提问，
两条回调都不会悬挂到超时。

**提问（`ask_user`）**：Agent 请求补充信息时会发一条 `❓ <问题>` 消息——有选项时逐条
列出编号，直接回复选项正文即可；`select` 提问只接受已声明的选项之一，`confirm`
也接受「是/否」这类文本。回复只交给当前提问，不会启动新任务；超时未作答时该轮按
「未作答」继续推进（不编造答案）。取消任务同样会释放挂起的提问。

### harness 管理命令（与本地 TUI 同一套 `omnicrawl-commands` 注册表）

由宿主承载的连接器直接复用 TUI 的命令注册表，因此这些命令**真的可用**：
`/sessions`、`/archives`、`/archive`、`/history`、`/undo`、`/compact`、
`/rename`、`/resume <id>`、`/plan`（启用主 Agent 计划模式，后续任务追加模式提示词）、
`/speaking-style [on|off]`（开关简洁回复模式，回复不超过十行；默认关闭）、
`/tasks`、`/task cancel`、`/approval`、`/reasoning`、`/model`、`/advisor`、
`/skills`、`/memory:clean`、`/mcp`、`/plugins` 等。
`/review` 例外：它要派生子 Agent 且子任务的工具批次必须回到宿主执行，连接器的同步
路径会在等待中阻塞事件循环，因此返回明确的「暂不可用」而不是悄悄变成一次对话。

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

## Telegram 侧显示方式（与飞书对齐）

与飞书连接器、TUI 的消息流一致：Agent 运行时产生的每个条目独立成一条消息、按发生顺序
出现；条目自己的流式更新只编辑自己那条消息，条目之间互不覆盖。

| 条目 | 消息内容 | 更新方式 |
|---|---|---|
| 正文段 | 模型一个 pass 的文本（`◇` 前缀） | 流式编辑，工具调用处封口 |
| 工具调用 | `● 摘要 · 状态 · 耗时` + 采样后的输出正文 | 调用开始即出现，结果落地原地收口 |
| 思考 | `🧠 思考内容`（仅 `/thinking on`） | 流式编辑，正文或工具开始时收口 |
| 执行计划 | `执行计划 · N/M 完成` 清单（`▣`/`▢`） | 首次更新时出现，之后原地替换 |
| 子任务进度 | `◇ 子任务进度` 树（同一批次一条消息） | 事件驱动原地更新 |
| 状态提示 | `⏳ …`（重试、上下文压缩、Skill 加载等） | 每个变化一条短消息 |

- **正文段**：模型一个 pass 的文本流式编辑同一条消息，约 0.9 秒一个节拍（Telegram 对同一
  条消息的编辑频率约 1 次/秒，不做逐字刷新）；工具调用是模型 pass 的边界，封口当前正文段后
  下一段正文**另起一条新消息**，工具前后的文本不会黏在同一条消息里；模型流中断重试时丢弃
  已作废的半截输出。
- **工具调用**：调用一开始就发送独立消息（`● 摘要 · … 调用中`），结果落地后在同一格里补齐
  状态、真实耗时与采样正文；任务取消或失败时仍在运行的记录收口为 `↷ 已取消`。摘要与正文
  的生成规则和飞书、TUI 共用同一份实现（`read`/记忆/知识库工具不展示正文，`write_file`/
  `Edit_file` 展示变更预览）。
- **提问与计划不产生工具消息**：`ask_user` 有独立提问消息，`update_todos` 由计划条目原地更新。
- **不做逐秒编辑**：Telegram 对同内容编辑会回 400，连接器按「最后落地内容」去重。

## 限制

- 文本命令与文件消息；不支持语音转写（语音仅落盘到 `audio/` 交 Agent 处理）。
- 长任务取消依赖模型响应间隙的 `cancel_check`。
- 长任务会产生多条消息（每个条目一条）；这是与飞书/TUI 一致的设计，不会把整轮输出压缩进
  一条消息。

## 健壮性行为（P3 修复）

- **流式输出不丢内容**：任务失败/取消时，已流式输出的部分会保留在正文消息中
  （追加「…（输出中断）」标记），错误/取消提示单独一条消息发送，不覆盖。
- **关闭流程**：`close()` 先释放挂起的工具确认（按拒绝）与挂起的提问（按未作答）
  并请求取消，再等待任务线程自行退出（上限 10s），最后才关闭 Agent；文件下载线程在
  服务停止后不再启动新任务。
- **错误退出**：轮询遇到不可恢复的 Telegram API 错误（Token 失效等）时打印
  清晰消息并以退出码 1 结束，不输出原始 traceback。
- **事件游标过期**：回合事件按条数保留，长流输出会挤掉早期增量；连接器跳到保留
  窗口起点继续读，不会因为「没跟上增量」把整轮判失败。

## 文件边界

- `rust/crates/omnicrawl-connectors/src/telegram/`：Bot 轮询服务、命令分发、时间线显示
  （`timeline.rs`：正文段/工具/思考/计划/子任务各自独立成条）、工具确认桥与提问桥。
- `rust/crates/omnicrawl-connectors/src/text.rs`、`src/render.rs`：与平台无关的文本清理与
  显示映射（两个连接器共用，飞书侧保留同名再导出）。
- `rust/crates/omnicrawl-connectors/src/lib.rs`、`README.md`：包说明与使用约定。
- `rust/crates/omnicrawl-api/src/connector.rs`：**宿主侧连接器承载**（`connector <平台>`）：把连接器的 `AgentDriver` 面接到本地 API 的同一个 `AgentService`（真实工具表、会话、命令注册表）。
- `rust/crates/omnicrawl-config/src/core/workspace.rs`：`[workspace] root` 持久化（跨端工作区同步）。
- `rust/crates/omnicrawl-entry/src/startup.rs`：按宿主载荷可用性选择连接器承载方（`connector_specs`）。
- `omnicrawl/docs/TELEGRAM.md`：本文档。

## 验证清单

配置或修改后至少执行：

```powershell
cargo test -p omnicrawl-api -p omnicrawl-connectors
```

手工冒烟：给自己 Bot 发 `/status` 应返回状态文本；发普通文本应启动任务并流式回发；
发一张图片应出现「✅ 已收到文件：位于 .omnicrawl/.agent_tmp/images/...」；
未授权用户发消息应无任何回复。要验证「真的能访问项目」，发一句
「列出当前工作区根目录下的文件名，不要修改任何文件」——由宿主承载时这会真的读目录；
内核承载（无宿主载荷平台）下它会回「工具在当前模式下不可用」。
