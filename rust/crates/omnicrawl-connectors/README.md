# omnicrawl-connectors

消息平台连接器的 Rust 内核移植，语义基准是 Python 侧 `omnicrawl/connectors/telegram.py`、
`omnicrawl/connectors/fsapp.py` 与 `omnicrawl/connectors/feishu_inbox.py`。

连接器只做**平台 I/O 与显示映射**：轮询/长连接、消息收发、命令分发、审批与提问桥、
文件接收与分类、时间线展示。回合、斜杠命令、运行期配置同步（推理强度/审批模式/工作区）
与配置持久化由宿主侧承担（边界见 `src/agent.rs` 的 `AgentDriver`），最终形态是 Rust 内核的
协议 v1 宿主对端（`rust/docs/protocol-v1.md`）。

## 目录

```
src/
├── agent.rs                # 连接器 ↔ 宿主的边界：TurnEvent、AgentDriver、确认/提问桥
├── http.rs                 # 共用阻塞式 HTTP 传输（ureq + rustls）、表单与百分号编码
├── json.rs                 # Python json.dumps 等价序列化（卡片与提示负载的字节形状）
├── telegram/
│   ├── config.rs           # 环境变量 > [telegram] 段的配置解析与校验
│   ├── format.rs           # 文本分段、流式裁剪与收尾计划（按字符计）
│   ├── files.rs            # 文件字段提取、扩展名分类、落盘命名与相对路径
│   ├── dispatch.rs         # 更新路由（标识/白名单/命令/文件）与 /thinking、/workspace 判定
│   ├── api.rs              # Bot API：getUpdates、sendMessage、editMessageText、getFile、下载
│   └── bot.rs              # 轮询服务：单活动任务、流式显示、工具确认桥
└── feishu/
    ├── config.rs           # 环境变量 > [feishu] > 根级 fs_* 别名的配置解析与诊断
    ├── text.rs             # 内部标签清理、脱敏、分段与正文定型
    ├── render.rs           # 工具摘要/正文、文件变更预览、计划与子任务文本、卡片 JSON
    ├── files.rs            # 资源分类、临时目录落盘、post 富文本与 [FILE:] 标记
    ├── dedupe.rs           # 进程内去重与跨重启指纹（sha256 前 32 位）
    ├── timeline.rs         # 时间线条目：正文/工具/思考/计划/子任务各自独立成消息
    ├── api.rs              # 开放接口：租户令牌缓存、消息创建/更新、资源下载、长连接端点
    ├── ws.rs               # 长连接：pbbp2 帧编解码、WebSocket 握手与帧收发、重连退避
    └── bot.rs              # 事件接入、命令分发、任务时间线装配、审批与提问桥、长连接主循环
```

## 对照（parity）工作流

Python 侧是语义基准，期望值由脚本在**真实现**上跑出来：

```bash
python rust/tools/gen_connectors_telegram_fixture.py
python rust/tools/gen_connectors_feishu_fixture.py
cd rust && cargo test -p omnicrawl-connectors
```

两份数据集覆盖：

- Telegram（`tests/fixtures/telegram_parity.json`）：分段与裁剪、流式收尾的消息调用序列、
  文件提取/分类/落盘命名（含重名逐轮递进）、配置解析（数组与全角逗号、错误文案）、
  更新路由（未授权不回复）、`/thinking` 与 `/workspace` 判定。
- 飞书（`tests/fixtures/feishu_parity.json`）：清理标签与折叠空行、长文分段与卡片切分、
  工具摘要与正文（隐藏正文/文件变更预览）、`difflib.SequenceMatcher` 的 `+N -M` 统计、
  计划与思考面板、子任务进度树、卡片 JSON（键序与 `json.dumps` 分隔符）、配置解析与掩码、
  去重键、时间线条目真正发出的消息序列，以及用 `lark-oapi` 的 `pbbp2` 生成器编出的
  帧字节（Rust 解码逐字段比对、重编码按规范形态逐字节比对）。

`tests/feishu_bot.rs` 另外用桩件跑编排：白名单、消息去重、命令分发、正文/工具卡片序列、
审批与取消的等待语义、提问卡片与文本回答的唤醒。

改任一侧实现都要重跑生成脚本再跑测试；卡片负载按**字符串**比对，缩进与分隔符也是契约。

## 尚未移植

- `feishu_inbox.py` 的**持久**入站队列（`pending.jsonl` 跨重启重放、`done.jsonl` 紧凑化、
  24 小时窗口去重）：当前只有进程内去重、跨重启指纹与进程内排队。
- 飞书文件上传与 `[FILE:...]` 标记发文件（`_upload_image` / `_upload_file` /
  `_send_local_file` / `_send_generated_files`）：出站只发文本与卡片。
- `connectors/autostart.py` 的子进程自动启动与单例锁：与 TUI 生命周期绑定，等宿主侧编排
  迁移后一并处理。
- 飞书 SDK 的 `Content-Disposition` 文件名解析：缺文件名时回落成 `file_key`，
  由资源类型补扩展名（`.jpg`/`.opus`/`.bin`）。
- Telegram 的 HTTP 错误文案无法逐字复刻 `requests` 的异常文本，`TelegramApiError::Network`
  只保证分类与中文前缀一致；飞书侧同理（`FeishuApiError::Transport`）。
- 飞书 `_prewarm_agent`（Agent 预热线程）：宿主构建好 Agent 后交给连接器即可，不再单独预热。
