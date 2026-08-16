# connectors 消息平台连接器

存放连接外部消息平台的 Python 代码。

| 文件/模块      | 用途                                   | 状态     |
|---------------|----------------------------------------|----------|
| `telegram.py` | Telegram Bot 远程操作 OmniCrawl Agent | ✅ 已实现 |
| `wechat.py`   | 微信接入（计划，个人号/公众号待定）     | 待实现   |

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
- 运行参数：`/workspace [路径]`（查看/切换工作区，切换会持久化并同步到 TUI）`/reasoning [级别]` `/thinking on|off`
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

## 约定

- 密钥、Token 等敏感配置不入库，从环境变量或 `config.toml` 读取。
- 各平台实现需提供独立的连接/断开与消息收发接口，便于在 OmniCrawl 内复用。
