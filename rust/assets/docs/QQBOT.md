# QQ 机器人远程接入说明

OmniCrawl 通过 `rust/crates/omnicrawl-connectors/src/qqbot/` 提供 **QQ 机器人远程接入**：
在 QQ 里（单聊、群聊 @机器人、频道 @机器人）给机器人发消息，就能远程操作当前工作区的
OmniCrawl Agent——下达任务、审批敏感工具调用、查看状态、管理会话、切换工作区、接收附件。

本文档面向两类读者：
1. **使用者**：如何扫码或手工配置凭据、启动、验证。
2. **AI 助手（配置场景）**：当用户说"接入 QQ 机器人 / 配 QQ Bot / 在 QQ 上远程用"时，
   按本文档帮用户完成从创建机器人到启动验证的全流程。

## 工作原理

- 官方协议是 **WebSocket 网关**（`GET /gateway` 取 `wss://…`，`op=10 Hello` → `op=2 Identify`
  → `op=0 Dispatch` 收事件），不占用本机端口，也不需要公网 IP。
- 鉴权用 `AppID + AppSecret` 换 **AccessToken**（7200 秒，提前 5 分钟续期），业务请求带
  `Authorization: QQBot <token>`。
- **由宿主承载**：连接器进程是 `omnicrawl-host connector qqbot`，拿到的是与本地 TUI
  **同一套**真实工具表、会话目录与斜杠命令——能读写工作区文件、跑 shell、用记忆与知识库工具。
  内核自带的 `omnicrawl --connector qqbot` 只自持 `update_todos`/`ask_user`/`pause_work`，
  其余工具一律 `tool_unavailable`，**只能对话**；只有不带宿主载荷的平台（armv7 等）才回退。
- TUI 启动时若 `[qqbot]` 同时有 `app_id` 与 `app_secret`，会自动拉起该子进程；TUI 退出时
  一并回收。`OMNICRAWL_AUTO_START_CONNECTORS=0` 可关闭全部连接器的自动联动。
- 同一平台同一时刻只允许一个实例（`connector-QQ.lock`）；重复启动会被跳过并记录诊断。

### 平台限制（决定了显示方式）

QQ 从 2025-04 起下线了**主动消息**：所有下发都必须是对某条入站消息的**被动回复**
（携带它的 `msg_id`），且同一条入站消息只有 **5 次回复额度**（单聊 60 分钟、群聊 5 分钟）。
另外 QQ **不支持编辑消息**。因此本连接器：

- 没有流式预览：正文段在「工具调用」这个模型 pass 边界处封口成一条完整消息（`◇ …`）；
- 工具调用、执行计划、子任务进度合并成少数几条「进度」消息，而不是每个条目一条；
- **回执 + 中间过程合计最多 4 条**，第 5 条固定留给最终回答。中间额度用满后不再发送
  任何过程内容（进度、正文段、思考都不发），一直攒到最终文本才下发；
- 每次下发前预扣额度（`ReplyHandle::reserve`），一条超长正文切成多段时也不会越界。

### 消息类型

同一条入站消息的 5 次额度里，每条可以是不同消息类型（`msg_type` 决定哪个内容字段生效）：

| 类型 | `msg_type` | 内容字段 | 本连接器何时使用 |
|---|---|---|---|
| 纯文本 | 0 | `content` | 默认；正文里没有 Markdown 语法时 |
| Markdown | 2 | `markdown.content` | 单聊/群聊且正文含平台支持的语法 |
| 富媒体 | 7 | `media.file_info` | 回复里出现 `[FILE:路径]` 且文件可读 |

- **Markdown**：填了 `markdown` 就**不能**再带 `content`，内容不能为空。频道场景仍需内邀
  开通，因此频道消息一律走纯文本。平台只认一套子集，本连接器在发之前把不认的写法**就地
  规整**（`markdown.rs`），而不是把整条消息降级成纯文本：
  - 围栏代码块 ``` → 块引用（`> rust` + `> 代码行`）；
  - 表格 → 列表项（`- 项 | 值`），分隔行丢弃；
  - `###` 及更深标题 → 压到 `##`，缺空格的 `#标题` 补空格（`#!/bin/sh` 不动）；
  - 列表前紧接正文时补空行（平台不认黏在正文后的列表）；`* 项目` 统一成 `- 项目`；
    嵌套缩进统一成 4 空格；`>引用` 补空格；行内代码的反引号去掉（平台不渲染）；
  - 只认公网可达的图片 URL，本地相对路径的 `![alt](本地路径)` 退化成文字，避免转存失败。
  Markdown 被平台拒绝时自动按纯文本重发一次（同样占额度）；正文里没有任何平台语法时
  直接发纯文本，不套一层解析。
- **富媒体**：先 `POST /v2/{users|groups}/{openid}/files`（`srv_send_msg=false`）拿
  `file_info`，再发 `msg_type=7`。按扩展名选 `file_type`：png/jpg/jpeg → 1（图片）、
  mp4 → 2（视频）、silk/mp3/wav → 3（语音）、其余 → 4（文件）。二进制直传上限 **5MB**
  （更大的要走分片上传，本连接器不支持）；文件读不到或超限时不发富媒体，正文里保留
  `[FILE:...]` 路径文本，用户仍能找到文件。只有全部标记都发成功才会从正文里摘掉它们。

Agent 侧的发图方式与飞书一致：在回复里写 `[FILE:输出/图片.png]`（绝对路径或相对工作区路径）。

## 配置步骤（AI 引导用户完成）

### 1. 创建机器人

打开 QQ 开放平台的快捷创建入口（浏览器或手机 QQ 均可）：
<https://bot.qq.com/wiki/agent-qqbot/> 页面里的「快速注册创建 QQ机器人」。
扫码登录后点「创建机器人」，会立刻成功并向你的 QQ 发一条消息。
到 <https://q.qq.com/> 管理端可完善资料、查看 **AppID / AppSecret**。

### 2. 获取凭据（两种方式，任选其一）

**方式 A：扫码绑定（推荐，最省事）**

```powershell
omnicrawl-host connector qqbot --bind
```

控制台会打印二维码（也可复制链接在浏览器打开），手机 QQ 扫码确认后平台把凭据交给本机
（Secret 用 AES-256-GCM 传输，密钥只在本地生成与解密）。成功后自动写入
`~/.OmniCrawl/config.toml` 的 `[qqbot]`，并把扫码者的 openid 写进 `allowed_user_ids`。
扫码页上的接入方名称固定显示为「OmniCrawl」。

**方式 B：手工填写**

在 `config.toml` 顶层追加（与 `config.example.toml` 说明一致）：

```toml
[qqbot]
# QQ 开放平台管理端 → 机器人 → 开发设置里的 AppID 与 AppSecret。切勿提交真实凭据到仓库。
app_id = "102012345"
app_secret = "aBcDeFgHiJkLmNoPqRsTuVwXyZ123456"

# 允许操作机器人的用户 openid（强烈建议填写；留空 = 公开访问模式）。
# 单聊事件里是 author.user_openid，群聊里是 author.member_openid，频道里是 author.id。
# 不同 AppID 下同一用户的 openid 不同，因此白名单是「按机器人」的。
allowed_user_ids = ["F6A8B9C0D1E2"]

# 可选：允许在群聊里操作机器人的群 openid（author.member_openid 旁边的 group_openid）。
# 填了就以「群」为单位放行——群内任何成员 @机器人都能使用，不必逐人登记；
# 单聊与频道仍按 allowed_user_ids 判定。留空 = 群聊继续按成员 openid 判定。
allowed_group_ids = []

# 敏感工具确认与 ask_user 的超时秒数，超时自动拒绝/放弃作答。
confirmation_timeout_seconds = 300

# 可选：沙箱环境（不受频率限制，适合调试）。
sandbox = false

# 可选：事件订阅位掩码，默认 1<<25（单聊+群聊）+ 1<<30（频道公域 @）。
intents = 1107296256
```

怎么拿到自己的 openid：启动机器人（未填白名单时为公开访问模式），私聊发 `/status`；
日志 `~/.OmniCrawl/logs/QQ.log` 里会打印未授权/收到消息的 openid，复制进
`allowed_user_ids` 后重启即可收紧。未授权日志会带昵称与会话（`群/单聊` 的 openid 前缀），
方便在群里直接认出是哪位成员在申请授权：

```
[qqbot] 未授权用户 小明（D35B438688525E560746778E7FACEB26）在 group:B2C3D4E5 的 GROUP_AT_MESSAGE_CREATE 被忽略（不在 allowed_user_ids）。
```

白名单是**用户级**：同一个 AppID 下群聊的 `member_openid` 与单聊的 `user_openid` 是同一个值，
加进名单后该用户的单聊与群聊同时生效，不需要分别配置。

### 3. 按群放行（`allowed_group_ids`）

如果机器人只服务你自己管理的群、群成员都可信，逐人登记 openid 会很繁琐——每来一个新成员
都要查一次日志。此时改用群白名单：配了 `allowed_group_ids` 之后，**群聊的判定依据从「成员
openid」换成「群 openid」**，群内任何成员 @机器人都能用；单聊与频道不受影响，仍按
`allowed_user_ids` 判定。

```toml
[qqbot]
allowed_user_ids = ["F6A8B9C0D1E2"]   # 决定谁能私聊机器人
allowed_group_ids = ["B2C3D4E5F6A7"]  # 这些群里谁 @ 都能用
```

判定优先级：

| 配置 | 群聊判定 | 单聊 / 频道判定 |
|---|---|---|
| `allowed_user_ids` 为空或含 `*` | 所有人 | 所有人（公开访问模式，启动告警） |
| 两张表都填 | 只看群 openid | 只看用户 openid |
| 只填 `allowed_user_ids` | 按成员 openid（与旧版一致） | 按用户 openid |

群 openid 怎么拿：把机器人拉进群后，让**任意群成员**在群里发一条消息（@ 机器人或普通发言都行），
日志里会打印这次被忽略的会话键，`group:` 后面就是群 openid：

```
[qqbot] 群消息未 @ 机器人，忽略：group:B2C3D4E5F6A7（GROUP_MESSAGE_CREATE）
[qqbot] 未授权用户 小明（D35B438688525E560746778E7FACEB26）在 group:B2C3D4E5F6A7 的 GROUP_AT_MESSAGE_CREATE 被忽略（不在 allowed_group_ids）。
```

注意两点：`allowed_group_ids` 一旦填写，**群白名单优先**——此时即使某个成员的 openid 在
`allowed_user_ids` 里，他在**未列入的群**里 @机器人也会被拒（他的私聊不受影响）。另外未授权
消息只写日志、不回复，不会打扰群友。

### 群「接收所有消息」与 `GROUP_MESSAGE_CREATE`

群管理员在机器人资料页开启「接收所有消息」后，平台改用 `GROUP_MESSAGE_CREATE` 推送群里的
**每一条**消息，`GROUP_AT_MESSAGE_CREATE` 就不再出现。这是平台行为，与「允许被添加好友」
「允许被添加到任意群聊」两个开关无关——那两个只决定机器人能否被添加，不影响已入群收消息。

连接器对全量模式的处理：

- 只有确实 @ 到机器人的消息才当指令，其余记一行 `群消息未 @ 机器人，忽略` 后丢弃。
- @ 判定按以下任一命中即算（实测报文见下）：
  1. 正文含 `<@机器人openid>` / `<@!机器人openid>` 占位符；
  2. `mentions[]` 里有 `id` 或 `member_openid` 等于机器人 openid，或该项 `bot: true`；
  3. 正文含 `@机器人昵称` 且昵称后紧跟空白或标点（手打 `@昵称` 的情况）。
- 机器人身份取自网关 `READY` 的 `user.id` / `user.username`，缺字段时退回 `/users/@me`；
  启动日志会打印 `机器人身份：小囫囵（openid=...），用于识别群内 @`。
- 正文里平台插入的 @ 前缀会被剥掉，因此命令写法与 @ 事件一致（`/status` 等照常）。
- 白名单判定与 @ 事件完全相同，未授权仍只记日志不回复。

实测报文差异（重要）：群里**手打**的 `@小囫囵 /status` 不带占位符、`mentions` 为空数组，
平台把它当作普通文本；用 QQ 的「@ 提及」功能真正选中机器人才会生成
`<@成员openid>` 占位符与 `mentions[]` 条目。因此昵称兜底是必要的，否则手打 @ 会被当成闲聊。

### 4. 启动

```powershell
# 宿主承载（推荐：真实工具表与会话）
omnicrawl-host connector qqbot

# 无宿主载荷的平台（如 armv7）：退回内核承载，只能对话
omnicrawl --connector qqbot
```

启动成功标志：日志出现 `网关已连接（心跳 45s）` 与 `登录成功，开始接收事件。`
随后在 QQ 里给机器人发 `/status` 做冒烟验证。

**关闭**：`Ctrl+C`（会释放挂起的确认与提问，并回收 Agent 资源）。

## 支持的命令

### 基础命令（connector 层直接处理）

| 命令 | 作用 |
|------|------|
| `/start` | 查看使用说明 |
| `/status` | 状态（工作区、会话、是否忙碌） |
| `/session` | 当前会话 ID |
| `/reset`（或 `/new`） | 开启新会话（清空对话历史） |
| `/cancel` | 取消当前任务 |
| `/thinking on\|off` | 查看/切换思考内容显示（默认关闭） |
| `/speaking-style [on\|off]` | 查看/开关简洁回复模式（默认开启；主 Agent 提示词注入模式块） |
| `/workspace [路径]` | 查看/切换工作区（切换会持久化并同步到 TUI） |
| `/approve` | 批准当前等待确认的工具调用 |
| `/reject` | 拒绝当前等待确认的工具调用 |

### harness 管理命令（与本地 TUI 同一套注册表）

宿主承载时这些命令真的可用：`/sessions`、`/resume <id>`、`/history`、`/undo`、`/compact`、
`/rename`、`/plan`、`/speaking-style`、`/tasks`、`/approval`、`/reasoning`、`/model`、`/skills`、`/mcp` 等。
`/quit`、`/settings` 这类本地交互命令在远程被拒绝（与其它连接器一致）；
`/approval:auto`（完全自动）同样只限本地 TUI，远程可切 `manual`/`review`。

`/skills` 在该通道只列名称与来源作用域，不带每个 Skill 的 description：介绍常是一整段
说明，在 1800 字/条的被动回复里会把列表撑成好几条长消息。

命令拼错时（如 `/speaking-styl`）不会被执行，回执按命令表给出最接近的一条：
`未知命令。是不是想用 /speaking-style？发送 /start 查看可用命令。`。比对只看命令词
（忽略 `@机器人` 后缀与参数），按字符编辑距离取最小者；5–8 个字符的命令容许 1 处差异、
更长的容许 2 处，短命令不猜——一个字符之差就可能撞上另一条真命令。

未授权用户的消息（含命令）**一律忽略且不回复**，只留日志。

### 简洁回复模式（默认开启）

连接器启动时会调用宿主的模式切换，把 `speaking-style` 模式注入主 Agent 的 system prompt
（`<active_mode_prompt name="speaking-style">`，模板 `templates/speaking-style.md`）：
面向用户的回复不超过十行，只保留结论与必要依据。这与本地 TUI 里 `/plan` 的注入原理相同，
只影响模型提示词，不改对话历史与工具权限。

- 启动时若已经有别的模式生效（例如先手工开了 `plan`），连接器不覆盖它，只在日志里说明。
- `/speaking-style` 不带参数查看状态，`on` 开启、`off` 关闭；关闭后回到基础提示词。
- 提示词只压缩表达，不省略风险、失败与未验证项；需要长篇输出时模型会把内容写进文件并给路径。

## 显示方式

| 条目 | 消息内容 |
|---|---|
| 回执 | `✅ 已收到任务，开始执行（/cancel 可取消）。` |
| 正文段 | 模型一个 pass 的文本（`◇` 前缀），在工具调用处封口；含 Markdown 语法时按 Markdown 发。首行是标题/列表/引用时 `◇` 单独一行，否则前缀会把块语法吃掉 |
| 进度 | 工具调用合并段，每次调用**只占一行** `● 摘要 · ✓ 成功`，最多 12 条后折叠为「…另有 N 次」；**工具输出不下发** |
| 执行计划 | `**执行计划 · N/M 完成**`（`▣`/`▢`） |
| 子任务进度 | `**◆ 子任务进度**` 树 |
| 思考 | `🧠 思考` 面板（仅 `/thinking on`） |
| 富媒体 | 回复里的 `[FILE:路径]` → 上传后按 `msg_type=7` 发出（图片/视频/语音/文件） |
| 最终回答 | 剩余未发送的正文；已发过的正文段不会重发（宿主给回的最终文本通常只是最后一个模型 pass，因此按「已发正文段」逐段扣除，不整段重发） |

下发顺序是固定的：回执 → （正文段 / 进度，最多再 3 条）→ 最终回答。中间额度用满后，
过程内容全部跳过，只等最终文本。

摘要与正文的生成规则和 Telegram、飞书、TUI 共用同一份实现（`src/render.rs`），
但 QQ **只取摘要行**：工具名、关键参数与结果概要（如 `list . · 12 项`、`grep … · 目标: x`）
留在进度消息里，工具输出正文一律不发——工具结果属于内部过程，端上只需要知道「调用了
什么、成没成」。`/thinking on` 打开时才额外可见思考内容。

## 提问与审批

- **审批归属**：敏感工具确认只能由**发起该任务的白名单用户本人**批准或拒绝；
  同一群聊里的其他白名单用户无权代批。超时（默认 300 秒）自动拒绝，`/cancel` 会同步
  释放挂起的确认（按拒绝处理）。
- **提问（`ask_user`）**：Agent 请求补充信息时发一条 `❓ <问题>`；有选项时列出编号，
  回复序号或选项正文都算作答。回复只交给当前提问，不会启动新任务；超时未作答时
  该轮按「未作答」继续（不编造答案）。

## 附件接收

把图片/文件发给机器人（或群内 @机器人 时附带），连接器会下载并按扩展名分类存到工作区
`.omnicrawl/.agent_tmp/` 的 `images/`、`videos/`、`audio/`、`scripts/`、`code/`、`files/`
子目录，然后把「已收到附件：位于 <相对路径>」作为任务文本交给 Agent；同一条消息的文字
作为补充说明一并附带（例如发截图时写"提取里面的文字"）。

## 安全边界

- **AppSecret 是真正的风险面**：持有者可绕过本程序直接调用开放平台接口。
  只放本地 `config.toml`（或扫码写入），`.gitignore` 排除，绝不提交仓库；
  怀疑泄露时到管理端重置。
- **白名单建议必填**：留空即公开访问模式，任何能给机器人发消息的用户都可操作你的
  工作区（启动时会打印告警）。群聊场景建议用 `allowed_group_ids` 只放行自己管理的群，
  而不是把群成员一个个加进 `allowed_user_ids`。
- **群白名单不区分群内成员**：`allowed_group_ids` 一旦填写，列入的群里**任何人** @机器人
  都能驱动 Agent（包括该群后续加入的新成员）。它换来的是免登记，代价是失去了成员级
  粒度；群本身的可信度就是这条边界的全部依据。
- **白名单是「交出工作区」**：连接器只有一个活动任务槽，`Inbound::conversation_key`
  目前不参与隔离，因此所有白名单用户共用同一个会话与工作区——他们能读写同一批文件、
  执行命令、`/workspace` 切换目录、`/cancel` 打断正在跑的任务。审批仍按发起者本人
  归属（别人不能代批），但不构成文件级隔离。
- **工具确认**：`manual`/`review` 模式下 `bash`/`powershell` 等敏感工具走 `/approve`；
  `auto` 仅限本地 TUI。
- **脱敏**：回传用户的错误信息经 `redact_sensitive_text` 脱敏，不泄露密钥。
- **并发**：不要同时用 TUI 和机器人驱动**同一个会话**（并发写 JSONL 会损坏转录）；
  跨端切换用 `/sessions` + `/resume <id>`。

## 排障

| 现象 | 原因与处理 |
|---|---|
| 启动即退出：`缺少 QQ 机器人 AppID` | `[qqbot]` 段没有 `app_id`/`app_secret`，先扫码或手工填写 |
| `网关连接中断：…` 循环 | 凭据错（AppSecret 与 AppID 不配对）、网络不通或被墙；看 `logs/QQ.log` |
| 发消息没有反应 | 未登录成功（没有 `登录成功` 日志），或发送者不在 `allowed_user_ids` |
| 群里 @了但没回 | 机器人未进群 / 未升级最新版手机 QQ；群消息需 @机器人才会推送 |
| 回复「已用尽额度」迹象（少于预期条数） | 同一条入站消息只有 5 次回复额度，其中中间过程最多 4 条；发新消息即可继续 |
| 长任务只有回执和结论 | 中间额度已用满（4 条），过程内容被跳过，属预期行为 |
| Markdown 显示成原始符号 | 频道场景需内邀开通（一律纯文本）；单聊/群聊看日志 `Markdown 消息发送失败`，多为内容违规或无权限，此时已自动按纯文本重发 |
| 图片没收到，只看到 `[FILE:…]` 路径 | 文件不存在、超过 5MB 直传上限，或上传被平台拒绝；看日志里的「富媒体未发送/发送失败」 |
| 私聊 60 分钟后才收到长任务结果 | 被动回复窗口已过期，平台拒收；缩短任务或分批提问 |

日志：`~/.OmniCrawl/logs/QQ.log`（TUI 自动启动时写入）。手工启动时日志直接打在终端。

## 文件边界

- `rust/crates/omnicrawl-connectors/src/qqbot/`
  - `config.rs`：`[qqbot]` 段解析、校验与诊断（掩码后的 Secret）
  - `api.rs`：AccessToken、网关地址、被动回复（纯文本 / Markdown / 富媒体）、媒体上传与附件下载
  - `gateway.rs`：WebSocket 网关的 `op` 语义（Identify/Resume/心跳/Dispatch）
  - `bind.rs`：扫码绑定（create_bind_task → 二维码 → poll_bind_result → AES-256-GCM 解密）
  - `dispatch.rs`：事件解析与路由（单聊/群聊/频道、白名单、命令分类）
  - `files.rs`：附件分类与落盘命名（复用 Telegram 的扩展名规则）
  - `markdown.rs`：Markdown 适用范围与正文规整（代码块→引用、表格→列表、深标题压到二级）
  - `media.rs`：`[FILE:…]` → 富媒体的类型判定、5MB 限制与正文剥离
  - `bot.rs`：回复额度（中间 ≤4、总 ≤5）、时间线装配、审批与提问桥、主循环
- `rust/crates/omnicrawl-connectors/src/ws.rs`：与飞书共用的 WebSocket 原语
  （握手、帧编解码、TLS 建连、SHA-1）
- `rust/crates/omnicrawl-api/src/connector.rs`：宿主承载入口（`connector qqbot`）
- `rust/crates/omnicrawl-api/src/qqbind.rs`：`--bind` 写回配置
- `rust/crates/omnicrawl-entry/src/startup.rs`：连接器承载方选择与自动启动
- 文档：本文件（`omnicrawl://docs/QQBOT.md`）

## 验证清单

```powershell
cargo test -p omnicrawl-connectors
cargo test -p omnicrawl-api
```

手工冒烟：给机器人发 `/status` 应返回状态；发普通文本应回执后给出结论；
发一张图片应出现「✅ 已收到 1 个附件：…」；未授权用户发消息应无任何回复。
验证「真的能访问项目」：发「列出当前工作区根目录下的文件名，不要修改任何文件」。
