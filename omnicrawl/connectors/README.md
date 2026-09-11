# connectors 消息平台连接器

存放连接外部消息平台的 Python 代码。

| 文件/模块      | 用途                                   | 状态     |
|---------------|----------------------------------------|----------|
| `telegram.py` | Telegram Bot 远程操作 OmniCrawl Agent | ✅ 已实现 |
| `fsapp.py`   | 飞书自建应用 WebSocket 远程接入         | ✅ 已实现 |
| `autostart.py` | TUI 启动时自动管理 Telegram/飞书子进程 | ✅ 已实现 |
| `wechat.py`   | 微信接入（计划，个人号/公众号待定）     | 待实现   |

## 自动启动

启动 TUI（`ocl`、`omnicrawl`、`python -m omnicrawl` 或 `python main.py`）时：

- Telegram 同时配置 Bot Token 和 `allowed_user_ids` 后自动启动；
- 飞书同时配置 App ID 和 App Secret 后自动启动；
- 未配置的平台跳过；连接器启动失败只记录警告，不阻塞 TUI；
- 同一平台同一用户只允许一个活动实例：多个进程（多个 TUI、TUI + API、
  或与手工 `python -m` 并存）同时启动时，后启动方检测到已有实例会自动
  跳过，避免 Telegram/飞书出现重复长连接；
- TUI 退出时自动终止已启动的连接器及其后代进程；
- 设置 `OMNICRAWL_AUTO_START_CONNECTORS=0` 可关闭自动联动，改为手工运行连接器。

单例实现位于 `omnicrawl/workspace/connector_singleton.py`：跨进程文件锁
（Windows `msvcrt.locking` / POSIX `fcntl.flock`）+ 锁文件 PID 存活检测，
崩溃残留的锁会被下一个启动方自动接管。

两个连接器均使用独立子进程，不把凭证写入命令行参数。自动启动只读取本地配置，不会
在 TUI 主进程中建立 Telegram/飞书网络连接。

## telegram.py 使用说明

1. 在 Telegram 中向 [@BotFather](https://t.me/BotFather) 创建 Bot，获取 Token。
2. 获取自己的 Telegram 用户 ID（例如向 @userinfobot 发消息）。
3. 配置（环境变量优先，也可写入 config.toml 的 `[telegram]` 段）：

   - `TELEGRAM_BOT_TOKEN`：Bot Token（必填）
   - `TELEGRAM_ALLOWED_USER_IDS`：允许操作的用户 ID，逗号分隔（必填，安全白名单）
   - `TELEGRAM_CONFIRM_TIMEOUT`：工具确认超时秒数（默认 300）

4. 启动：`python -m omnicrawl.connectors.telegram`

支持命令：

- 任务控制：`/status` `/session` `/reset` `/cancel` `/approve` `/reject` `/start`
- 运行参数：`/workspace [路径]`（查看/切换工作区，切换会持久化并同步到 TUI）`/reasoning [级别]` `/thinking on|off` `/plan`（启用主 Agent 计划模式）
- 输出显示：`/thinking on|off`（思考内容开关，默认关闭；开启后以独立 🧠 消息显示）
- 会话管理：`/sessions` `/archives` `/archive` `/resume <id>` `/resume latest`（一步恢复最近活动会话）`/rename <标题>` `/undo` `/compact [--model]` `/history [关键词]`
- 子系统状态：`/tasks` `/task <id>` `/task cancel <id>` `/mcp` `/plugins` `/skills` `/memory:clean` `/reasoning [级别]` `/approval` `/approval:manual`

其余文本作为任务交给 OmniCrawl Agent 执行。敏感工具调用（bash/powershell）
会请求确认，超时自动拒绝。

文件接收：直接发送图片/文档/视频/语音等文件，会自动下载并按类型分类存入
工作区 `.omnicrawl/.agent_tmp/` 的 `images/` `videos/` `scripts/` `code/`
`audio/` `files/` 子目录，然后 Bot 把「已收到文件：位于 <相对路径>」作为任务文本
交给 Agent 处理；消息 caption 作为补充说明一并附带（例如发截图时写
“提取里面的文字”）。

输出展示：最终回答以打字机效果流式显示（编辑同一条消息）；思考内容
（`/thinking on` 开启）、工具调用、状态提示各自独立成消息，互不合并。
任务失败/取消时已流式输出的部分会保留（追加中断标记），错误单独一条
消息，不覆盖已看到的内容。

跨进程同步：TUI 中 `/reasoning` 与 `/workspace` 的切换都会写回 config.toml；
本 Bot 每次任务开始前重读并应用到当前 Agent，因此 TUI 里调整的推理强度
与工作区会同步到 Telegram 侧；Telegram 侧 `/workspace` 同样持久化。

跨端接力：`/resume latest` 直接恢复最近活动的会话（按最后更新时间），
电脑 TUI 聊到一半用手机继续时，一条命令即可接上完整上下文。

安全边界：远程**不允许**把审批模式切换为自动/审查（会放开 bash 等工具
执行确认），只可查看与切回手动模式。

## fsapp.py 使用说明

飞书连接器使用 `lark-oapi` WebSocket 长连接，配置和扫码授权限制见
`omnicrawl/docs/FSAPP.md`。独立运行：`python -m omnicrawl.connectors.fsapp`。

输出展示（对齐 TUI 消息流）：每个条目独立成一条消息、按发生顺序出现。
正文段以 `◇` 前缀流式更新、在每次工具调用处封口另起一条；工具调用各自成一条
消息（`● 工具名 参数摘要 · ✓ 成功 · 耗时` 加采样后的输出正文，与 TUI 工具卡同
规则），开始即出现、完成时原地收口；思考（`/thinking on` 时折叠面板）、执行
计划与子任务进度也各自成一条消息原地更新。提问（`ask_user`）仍使用独立选项
卡片。详见 `omnicrawl/docs/FSAPP.md` 的「飞书侧显示方式」。

## 约定

- 密钥、Token 等敏感配置不入库，从环境变量或 `config.toml` 读取。
- 各平台实现需提供独立的连接/断开与消息收发接口，便于在 OmniCrawl 内复用。
