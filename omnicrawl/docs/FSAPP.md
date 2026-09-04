# FSAPP：飞书机器人连接器配置指南

`omnicrawl/connectors/fsapp.py` 是 OmniCrawl 的飞书远程连接器。它通过
`lark-oapi` 的 WebSocket 长连接接收飞书消息，再交给当前 OmniCrawl Agent 执行，
并把状态、工具调用结果和最终回答发送回飞书。

本文面向：

- **使用者**：创建飞书自建应用、配置凭证、启动连接器并验证消息链路；
- **AI 助手**：按照本文档引导用户配置，不索取或回显 App Secret，不把真实凭证写入仓库；
- **维护者**：了解当前连接器支持的授权方式、命令、权限和已知限制。

## 先说结论：当前不支持扫码授权

当前 `fsapp.py` **不支持运行时扫码授权**。它不是“用户扫码后登录”的连接器，
而是使用飞书**自建应用（Custom App）的 App ID 和 App Secret**，以应用身份建立
WebSocket 长连接。

代码当前没有以下能力：

- 生成或展示二维码；
- 生成飞书 OAuth 授权地址；
- 接收 OAuth 重定向回调；
- 用授权码换取用户 `access_token` 或 `refresh_token`；
- 通过扫码绑定、登录或切换使用者。

飞书控制台中可能存在“扫码安装应用”、用户可以扫描二维码加入群聊或打开应用等
操作，但这些是**飞书平台的安装/加群操作**，不代表 `fsapp.py` 实现了扫码登录。
运行连接器仍然必须配置 App ID 和 App Secret。

## 工作原理

```text
飞书用户
   │ 发送文本、文件或命令
   ▼
飞书机器人 ── WebSocket 长连接 ──> fsapp.py
                                      │
                                      ├─ 校验 sender.open_id 白名单
                                      ├─ 下载消息中的图片/文件到 .agent_tmp
                                      ├─ 调用 OmniCrawl LocalToolAgent.run_stream()
                                      └─ 发送卡片、文本、文件、提问和审批结果回飞书
```

连接器使用：

- 事件：`im.message.receive_v1`；
- 接收方式：飞书事件订阅的 **WebSocket 长连接**，不需要公网 Webhook 地址；
- 认证方式：飞书自建应用的 `app_id` + `app_secret`；
- Agent：通过 `create_default_agent()` 创建 OmniCrawl 默认 Agent；
- 提问：`ask_user` 的三种模式（`select`/`question`/`confirm`）都会携带 `options`，`select` 发送交互式卡片让用户从选项中单选，`question`/`confirm` 也以选项卡片呈现，同时允许用户直接回复文本作为自定义答案；
- 任务并发：一个连接器进程内同一时间只执行一个 Agent 任务；
- 文件目录：收到的资源保存到当前 Agent 工作区的 `.omnicrawl/.agent_tmp/` 分类目录。

> 当前连接器进程与 OmniCrawl TUI/API 是独立入口。启动 TUI 时若飞书 App ID 和
> App Secret 均已配置，TUI 会自动拉起独立的飞书连接器子进程，并在 TUI 退出时自动
> 回收；未配置或连接器启动失败不会阻塞 TUI。不要同时让多个进程并发写入同一个会话，
> 否则可能造成会话转录竞争；需要跨端接力时，使用 `/sessions`、`/resume <id>` 或
> `/resume latest`。

## 前置条件

1. 已安装 Python 3.9 或更高版本，并能正常运行 OmniCrawl。
2. 已准备 OmniCrawl 的模型配置和 API Key。连接器只负责飞书接入，Agent 本身仍需要
   可用的 LLM 配置。
3. 在飞书开放平台拥有创建和发布自建应用的权限。
4. 已安装可选依赖 `lark-oapi`：

   ```powershell
   pip install lark-oapi
   ```

   `lark-oapi` 当前不是 OmniCrawl 的必装依赖，项目不会因为普通 TUI/API 使用而自动
   安装它。未安装时，`fsapp.py` 仍可以被导入，但启动连接器会给出明确提示并退出。

5. 已准备至少一个允许使用机器人的飞书用户 `open_id`，通常形如 `ou_...`。代码的
   白名单比较的是事件中的 `sender.sender_id.open_id`，不是用户姓名、手机号或邮箱。

## 一、在飞书开放平台创建应用

不同版本的飞书开放平台菜单名称可能略有差异，以下步骤按“自建应用/机器人/事件订阅”
相关入口操作。

### 1. 创建自建应用

1. 打开飞书开放平台，进入目标企业租户。
2. 创建一个**企业自建应用（Custom App）**。
3. 记录应用的：
   - `App ID`，通常以 `cli_` 开头；
   - `App Secret`。
4. App Secret 只保存在本机环境变量或本地配置文件中，不要发送到聊天、提交 Git 或
   写入本文档。

### 2. 启用机器人能力

在应用能力/功能页面启用机器人或消息能力，并创建/发布应用版本。应用需要安装到
当前租户，且机器人需要被加入目标单聊或群聊；仅创建应用但没有发布或安装，通常无法
收到用户消息。

### 3. 配置权限

根据实际功能在权限管理中申请并由管理员授权。至少需要覆盖以下两类能力：

| 功能 | 权限/能力方向 |
|---|---|
| 接收用户消息 | 消息接收事件及读取消息内容 |
| 发送机器人回复 | 以应用/机器人身份发送消息 |
| 接收图片、文件、音频、视频 | 消息资源读取/下载 |
| 回传 Agent 生成的文件 | 图片/文件上传及发送 |
| 群聊使用 | 机器人加入群聊及群消息相关权限 |

飞书控制台会根据应用版本显示具体权限名称。请以控制台中与上述能力对应的实际权限
名称为准，并在权限变更后重新发布应用版本。

### 4. 配置事件订阅

1. 进入事件订阅页面。
2. 选择 **使用长连接接收事件 / WebSocket 长连接**。
3. 订阅事件 `im.message.receive_v1`（接收消息事件）。
4. 不需要为本连接器配置公网 Webhook URL；代码通过 `lark.ws.Client` 主动连接飞书。
5. 保存并发布应用版本。

如果控制台要求先建立长连接才能保存事件订阅，先完成本地配置并启动 `fsapp.py`，
再回到控制台保存事件。

## 二、配置 OmniCrawl

连接器读取 OmniCrawl 的 TOML 运行配置。默认路径为：

```text
~/.OmniCrawl/config.toml
```

如果设置了 `AI_CONFIG_FILE`，则使用该环境变量指定的 TOML 文件。

### 方式 A：环境变量（适合服务进程和 CI）

Windows PowerShell：

```powershell
$env:FEISHU_APP_ID = "cli_xxxxxxxxxxxx"
$env:FEISHU_APP_SECRET = "请在本机填写真实值，不要提交到仓库"
$env:FEISHU_ALLOWED_USER_IDS = "ou_xxxxxxxxxxxx,ou_yyyyyyyyyyyy"
$env:FEISHU_CONFIRM_TIMEOUT = "300"
```

Linux/macOS shell：

```bash
export FEISHU_APP_ID="cli_xxxxxxxxxxxx"
export FEISHU_APP_SECRET="请在本机填写真实值，不要提交到仓库"
export FEISHU_ALLOWED_USER_IDS="ou_xxxxxxxxxxxx,ou_yyyyyyyyyyyy"
export FEISHU_CONFIRM_TIMEOUT="300"
```

`FEISHU_ALLOWED_USER_IDS` 使用英文逗号分隔，可以配置多个 `open_id`。

### 方式 B：`config.toml`（适合本机长期运行）

在 `~/.OmniCrawl/config.toml` 中增加：

```toml
[feishu]
# 飞书自建应用的 App ID，通常以 cli_ 开头。
app_id = "cli_xxxxxxxxxxxx"

# App Secret 只写入本机配置，不提交 Git，不粘贴到聊天记录。
app_secret = "请填写本机真实值"

# 安全白名单：填写允许操作 Agent 的飞书用户 open_id。
allowed_user_ids = ["ou_xxxxxxxxxxxx"]

# 敏感工具确认等待时间，超时自动拒绝。
confirmation_timeout_seconds = 300
```

### 配置优先级和兼容字段

`fsapp.py` 对同一配置项按以下顺序取值：

```text
环境变量 > [feishu] 段 > 根级兼容字段
```

当前支持的字段如下：

| 作用 | 环境变量 | 推荐 TOML 字段 | 兼容字段 |
|---|---|---|---|
| App ID | `FEISHU_APP_ID` | `feishu.app_id` | `fs_app_id` |
| App Secret | `FEISHU_APP_SECRET` | `feishu.app_secret` | `fs_app_secret` |
| 用户白名单 | `FEISHU_ALLOWED_USER_IDS` | `feishu.allowed_user_ids` | `fs_allowed_users` |
| 确认超时 | `FEISHU_CONFIRM_TIMEOUT` | `feishu.confirmation_timeout_seconds` | — |

白名单也兼容 `[feishu]` 下的 `allowed_users` 和 `fs_allowed_users`。推荐使用
`[feishu].allowed_user_ids`，不要混用多套字段。

### 白名单安全说明

当前代码为了兼容参考实现，白名单为空或包含 `"*"` 时会进入**公开访问模式**：任何能
给机器人发消息的用户都可能触发 Agent。启动时会记录警告，但不会阻止启动。

因此生产配置必须填写明确的白名单：

```toml
allowed_user_ids = ["ou_真实用户_open_id"]
```

不要把用户名、显示名、手机号或邮箱直接填入该字段。若暂时不知道自己的 `open_id`，
应通过飞书事件调试信息或企业通讯录/API 获取；也可以在隔离的测试租户中短暂使用空白
白名单观察日志中的 `sender.open_id`，获取后立即恢复为明确白名单。测试期间不要让未授权
用户接触机器人。

## 三、检查和启动

### 1. 只检查飞书配置

```powershell
python -m omnicrawl.connectors.fsapp --check
```

示例输出：

```json
{
  "app_id": "cli_xxxxxxxxxxxx",
  "app_secret": "secr****3456",
  "app_secret_present": true,
  "allowed_users": [
    "ou_xxxxxxxxxxxx"
  ],
  "public_access": false,
  "confirmation_timeout_seconds": 300.0,
  "ready": true
}
```

`--check` 不会建立飞书长连接，也不会发送消息。它会掩码显示 App Secret，不应把包含
配置诊断的终端日志公开发布。

`ready` 为 `false` 时，先检查 App ID 和 App Secret 是否为空、环境变量是否覆盖了
TOML 中的正确值，以及 `AI_CONFIG_FILE` 是否指向预期文件。

### 2. 可选：检查配置并初始化 Agent

```powershell
python -m omnicrawl.connectors.fsapp --check-agent
```

该命令除了检查飞书配置，还会创建 OmniCrawl Agent。它可能读取模型配置、API Key 和
工作区，因此只在本机配置已准备好时使用。

### 3. 启动连接器

启动 TUI（`ocl`、`omnicrawl`、`python -m omnicrawl` 或源码目录下的
`python main.py`）时，满足配置条件会自动启动飞书连接器子进程。也可以继续手工独立启动：

```powershell
python -m omnicrawl.connectors.fsapp
```

自动联动默认开启。若只想运行 TUI，或需要手工调试连接器，可关闭自动启动：

```powershell
$env:OMNICRAWL_AUTO_START_CONNECTORS = "0"
```

自动启动只检查本地配置，不会在主进程中创建飞书 Agent 或建立网络连接；缺少
`lark-oapi`、网络错误或连接器退出只会记录警告，不会阻止 TUI 启动。

**运行日志**：TUI 自动拉起的连接器子进程把 stdout/stderr 落盘到用户配置目录
`~/.OmniCrawl/logs/飞书.log`（Telegram 为 `~/.OmniCrawl/logs/Telegram.log`），
因此连接器不会污染全屏界面，排查“进程活着但收不到消息”等场景时可随时查看该
文件中的白名单拦截、断线重连和卡片回调日志。手工 `python -m` 启动时日志仍输出
到当前终端。

**多进程单例**：每个连接器平台在同一用户下只允许一个活动实例。多个 TUI/API
进程并存、或 TUI 自动启动与手工 `python -m` 同时运行时，后启动的一方会检测
到已有实例并跳过（日志提示“已有实例在运行”），避免飞书 WebSocket 被重复
建立、消息被多个实例重复处理。单例锁文件位于用户配置目录
`~/.OmniCrawl/connector-飞书.lock`，记录持有进程 PID；进程崩溃残留时，下一个
启动方会自动接管。

成功建立长连接后，日志会出现类似：

```text
飞书 Agent 已启动（WebSocket 长连接），App ID：cli_xxx，等待消息...
```

停止方式：在运行连接器的终端按 `Ctrl+C`。连接器会请求取消活动任务、释放等待中的
工具确认、停止 WebSocket 客户端并关闭 Agent。

## 四、首次验证

建议按以下顺序验证，避免一开始就发送需要高权限工具的复杂任务：

1. 在飞书中打开机器人单聊，或把机器人加入测试群聊。
2. 发送 `/start`，确认能收到帮助文本。
3. 发送 `/status`，确认工作区、会话和忙闲状态可返回。
4. 发送一个只读任务，例如：

   ```text
   列出当前工作区根目录下的文件名，不要修改任何文件。
   ```

5. 再发送一张图片或一个小文件，确认资源能保存到 `.omnicrawl/.agent_tmp/` 并交给
   Agent 处理。
6. Agent 提问时，`select` 问题点击卡片选项，`question`/`confirm` 问题直接回复文本；回答只会交给当前提问，不会启动新任务。
7. 在需要敏感工具确认时，使用：

   ```text
   /approve
   ```

   或：

   ```text
   /reject
   ```

### 常用命令

| 命令 | 作用 |
|---|---|
| `/start`、`/help` | 查看帮助 |
| `/status` | 查看工作区、会话和任务状态 |
| `/session` | 查看当前会话 ID |
| `/reset`、`/new` | 清空当前对话并开启新会话 |
| `/cancel` | 请求取消当前任务；等待提问时也可取消 |
| `/approve` | 批准当前等待的敏感工具调用 |
| `/reject` | 拒绝当前等待的敏感工具调用 |
| `/thinking on\|off` | 开关思考内容展示 |
| `/workspace [路径]` | 查看或切换工作区 |
| `/plan` | 启用主 Agent 计划模式 |
| `/sessions`、`/resume <id>` | 查看/恢复会话 |
| `/resume latest` | 恢复最近活动会话 |
| `/approval` | 查看审批模式 |
| `/approval:manual` | 切换为手动确认 |
| `/approval:review` | 切换为自动审查 |
| `/reasoning [级别]` | 查看或设置推理强度（下一次请求生效） |
| `/model [选择]` | 查看或切换模型（选择支持 models.toml key、profile/model_id 或裸 model_id；下一次请求生效） |
| `/tasks`、`/mcp`、`/plugins`、`/skills` | 查看对应子系统状态 |

> **运行中切换**：`/model` 与 `/reasoning` 都可在任务进行中切换，当前回合
> 继续使用旧配置，从修改后的下一次请求开始生效（不会中断正在执行的任务）。

远程连接器不允许通过命令开启完全自动批准；敏感工具应保留人工确认或审查边界。

## 五、AI 配置操作规范

当用户要求“配置飞书机器人”时，AI 助手应按以下顺序处理：

1. 先确认用户使用的是飞书企业自建应用，而不是个人 OAuth 登录需求。
2. 指导用户在飞书开放平台创建/发布机器人、授权消息权限并启用 WebSocket 事件订阅。
3. 让用户在**自己的终端或本地配置文件**中填写 App ID、App Secret 和白名单；不要让
   用户把 Secret 粘贴到对话中，也不要在回复中回显 Secret。
4. 执行或指导执行 `python -m omnicrawl.connectors.fsapp --check`，只根据
   `ready`、`public_access` 和依赖错误做诊断。
5. 确认 `public_access` 为 `false` 后再启动连接器。
6. 使用 `/start`、`/status` 和一个只读任务完成最小冒烟测试。
7. 若用户提出“扫码登录/扫码授权”，必须说明当前连接器不支持该流程；不要把飞书
   控制台的扫码安装、扫码加群操作描述成 `fsapp.py` 的授权功能。

AI 不应：

- 索取、保存、输出或提交 App Secret；
- 为了获取 `open_id` 而在生产环境长期关闭白名单；
- 未经用户确认安装新的依赖或连接真实飞书租户；
- 把公网 Webhook、OAuth 回调或二维码参数写进当前配置，因为 `fsapp.py` 不读取这些
  配置；
- 在多个 OmniCrawl 进程中同时驱动同一个会话。

## 六、常见问题排查

### 1. 提示缺少 `lark-oapi`

执行：

```powershell
pip install lark-oapi
```

然后重新运行 `--check` 或启动命令。该依赖没有写入 OmniCrawl 的默认依赖列表，属于
飞书连接器的可选运行依赖。

### 2. `ready: false`

检查：

- `FEISHU_APP_ID` 和 `FEISHU_APP_SECRET` 是否为空；
- PowerShell 当前会话是否设置了旧环境变量；
- `AI_CONFIG_FILE` 是否指向错误的配置文件；
- `[feishu]` 是否拼写正确；
- TOML 是否能被正常解析。

环境变量优先级高于 TOML。即使 TOML 已经改正确，旧环境变量仍可能覆盖它。

### 3. 长连接启动但收不到消息

检查：

- 应用是否已发布并安装到当前租户；
- 机器人是否已加入目标单聊或群聊；
- 是否订阅了 `im.message.receive_v1`；
- 事件订阅是否选的是 WebSocket 长连接，而不是等待公网 Webhook；
- 消息接收权限是否已通过管理员授权；
- 运行连接器的网络是否允许访问飞书服务；
- 发送消息的用户 `open_id` 是否在白名单中。

### 4. 日志显示“忽略未授权飞书用户”

这是白名单校验生效的表现。日志会显示收到事件的 `sender.open_id`，将确认过的用户
`open_id` 加入 `allowed_user_ids` 后重启连接器。不要直接把白名单改成 `*` 作为长期修复。

### 5. 能收到消息但无法回复

检查应用的机器人发消息权限、应用发布状态和目标会话权限。文件/图片接收或回传失败
时，还要检查消息资源读取、上传和发送相关权限。

### 6. Agent 提问时点击 select 卡片按钮没反应

`lark-oapi` 1.7.x 的 WebSocket 客户端会丢弃卡片回调（CARD）数据帧。`fsapp.py`
启动时会对 SDK 客户端实例安装兼容补丁，把卡片回调分发给已注册的
`p2.card.action.trigger` 处理器（即 Agent 提问按钮）。若运行日志出现
“当前 lark-oapi 版本不支持 CARD 帧补丁”，说明 SDK 结构变化导致补丁未生效，
此时可升级/降级 `lark-oapi` 到 1.7.3 附近版本，或直接回复文本作为答案
（`select`/`question`/`confirm` 提问都接受文本回答）。

### 7. 为什么看不到二维码或扫码登录入口

这是当前设计的正常表现。`fsapp.py` 使用 App ID/App Secret 和应用级 WebSocket 长
连接，没有二维码生成、OAuth 授权码交换或扫码登录状态机。若确实需要“每个用户扫码
授权后再绑定”的产品流程，需要另行设计并实现飞书 OAuth/二维码授权模块，不能只通过
当前配置文件开启。

## 七、文件与代码边界

| 路径 | 作用 |
|---|---|
| `omnicrawl/connectors/fsapp.py` | 飞书 WebSocket 连接、消息分派、Agent 任务、审批和文件收发 |
| `omnicrawl/docs/FSAPP.md` | 本配置与排障指南 |
| `~/.OmniCrawl/config.toml` | 本机运行配置，包含 `[feishu]` 凭证和白名单 |
| `<workspace>/.omnicrawl/.agent_tmp/` | 飞书接收文件和 Agent 临时产物 |

`pyproject.toml` 已配置 `omnicrawl` 包包含 `docs/*.md`，因此该文档会随包数据规则被
打包；它不包含任何真实凭证。

## 验证清单

配置完成后至少执行：

```powershell
python -m omnicrawl.connectors.fsapp --check
python -m py_compile omnicrawl/connectors/fsapp.py
```

然后在飞书客户端完成：

- `/start` 回复验证；
- `/status` 状态验证；
- 一个只读 Agent 任务验证；
- 一个白名单外用户的拒绝验证；
- 如需使用敏感工具，再验证 `/approve` 和 `/reject`。
