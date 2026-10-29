# omnicrawl-connectors

消息平台连接器的 Rust 内核移植，语义基准是 Python 侧 `omnicrawl/connectors/telegram.py`、
`omnicrawl/connectors/fsapp.py` 与 `omnicrawl/connectors/feishu_inbox.py`。

连接器只做**平台 I/O 与显示映射**：轮询/长连接、消息收发、命令分发、审批与提问桥、
文件接收与分类、时间线展示。回合、斜杠命令、运行期配置同步（推理强度/审批模式/工作区）
与配置持久化由宿主侧承担（边界见 `src/agent.rs` 的 `AgentDriver`），最终形态是 Rust 内核的
协议 v1 宿主对端（`rust/docs/protocol-v1.md`）。

**承载方决定能力边界**。宿主载荷里的 `omnicrawl-host connector <平台>` 由
`omnicrawl-api` 的 `service::AgentService` 承载（`rust/crates/omnicrawl-api/src/connector.rs`）：
连接器拿到的是**真实工具表、会话存储与 `omnicrawl-commands` 命令注册表**，因此远程任务能
读写工作区、跑 shell、用记忆与知识库工具。内核自带的 `omnicrawl --connector <平台>` 只在
进程内自持 `update_todos` / `ask_user` / `pause_work`，其余工具一律回 `tool_unavailable`、
会话也不落盘，**只能对话**；它只保留给不带宿主载荷的平台（armv7 等）。

## 目录

```
src/
├── agent.rs                # 连接器 ↔ 宿主的边界：TurnEvent、AgentDriver、确认/提问桥
├── http.rs                 # 共用阻塞式 HTTP 传输（ureq + rustls）、表单与百分号编码
├── json.rs                 # Python json.dumps 等价序列化（卡片与提示负载的字节形状）
├── text.rs                 # 平台无关：内部标签清理、脱敏、分段与正文定型（两侧共用）
├── render.rs               # 平台无关：工具摘要/正文、变更预览、计划与子任务文本、卡片 JSON
├── ws.rs                   # 平台无关：WebSocket 握手、帧编解码、TLS 建连与 SHA-1（飞书与 QQ 共用）
├── telegram/
│   ├── config.rs           # 环境变量 > [telegram] 段的配置解析与校验
│   ├── format.rs           # 文本分段、流式裁剪与收尾计划（按字符计）
│   ├── files.rs            # 文件字段提取、扩展名分类、落盘命名与相对路径
│   ├── dispatch.rs         # 更新路由（标识/白名单/命令/文件）与 /thinking、/workspace 判定
│   ├── api.rs              # Bot API：getUpdates、sendMessage、editMessageText、setMyCommands、getFile、下载
│   ├── timeline.rs         # 时间线：正文段/工具/思考/计划/子任务各自独立成一条消息
│   └── bot.rs              # 轮询服务：单活动任务、时间线装配、工具确认桥与提问桥
└── feishu/
    ├── config.rs           # 环境变量 > [feishu] > 根级 fs_* 别名的配置解析与诊断
    ├── file_send.rs        # [FILE:] 发文件：路径校验、扩展名分流、消息体与提示文案
    ├── files.rs            # 资源分类、临时目录落盘、post 富文本与 [FILE:] 标记
    ├── dedupe.rs           # 进程内去重与跨重启指纹（sha256 前 32 位）
    ├── inbox.rs            # 持久入站队列：pending 落盘与重放、done 去重与紧凑化、纯内存降级
    ├── timeline.rs         # 时间线条目：正文/工具/思考/计划/子任务各自独立成消息（卡片）
    ├── api.rs              # 开放接口：租户令牌缓存、消息创建/更新、资源下载、长连接端点
    ├── ws.rs               # 飞书长连接：pbbp2 帧编解码、LongConnection 与重连退避（帧与 TLS 走 crate::ws）
    └── bot.rs              # 事件接入、命令分发、任务时间线装配、审批与提问桥、长连接主循环
├── mqtt/                   # 遥测进、动作出（不是聊天平台，没有时间线）
│   ├── config.rs           # [mqtt] 段解析：broker、主题、窗口、动作表、规则
│   ├── codec.rs            # MQTT 3.1.1 编解码：连接/订阅/发布/确认/心跳
│   ├── client.rs           # 阻塞式客户端：TCP 与 TLS、keepalive、QoS1 发布
│   ├── policy.rs           # 窗口聚合、规则状态机、迟滞、冷却、读数解析
│   ├── prompt.rs           # 固定模板、动作 JSON 解析与参数夹取
│   ├── planner.rs          # 规划器边界（宿主实现：决策模型优先、主模型回落）
│   ├── audit.rs            # JSONL 审计与启动时的冷却回放
│   └── bot.rs              # 主循环：订阅→聚合→规则→唤醒→下发→等回执
└── qqbot/
    ├── config.rs           # [qqbot] 段解析、校验与诊断（Secret 掩码）
    ├── api.rs              # AccessToken 缓存、网关地址、被动回复（文本/Markdown/富媒体）、上传、下载
    ├── gateway.rs          # QQ 网关 op 语义：Hello → Identify/Resume、心跳、Dispatch 解析
    ├── bind.rs             # 扫码绑定：create_bind_task → 二维码 → poll_bind_result → AES-256-GCM
    ├── dispatch.rs         # 事件解析与路由：单聊/群聊/频道、白名单、命令分类（复用 telegram 那份）
    ├── files.rs            # 附件分类与落盘命名（扩展名规则与 Telegram 同源）
    ├── markdown.rs         # Markdown 适用范围与正文规整（代码块→引用、表格→列表）
    ├── media.rs            # [FILE:] → 富媒体：file_type 判定、5MB 上限、标记剥离
    └── bot.rs              # 回复额度（中间 ≤4、总 ≤5）、时间线装配、审批与提问桥、网关主循环
```

## 显示口径（两个连接器一致）

Agent 运行时产生的每个条目独立成一条消息、按发生顺序出现；条目自己的流式更新只更新自己
那条消息，条目之间互不覆盖：

| 条目 | Telegram（纯文本） | 飞书（卡片） |
|---|---|---|
| 正文段 | `◇` 前缀，流式编辑，工具调用处封口 | 同左，Markdown 卡片 |
| 工具调用 | `● 摘要 · 状态 · 耗时` + 正文 | 同左 |
| 思考 | `🧠`，仅 `/thinking on` | `💭` 折叠面板 |
| 执行计划 | `执行计划 · N/M 完成`，原地替换 | 同左 |
| 子任务进度 | `◇ 子任务进度` 树，原地更新 | 同左 |

模型一个 pass 的正文段以工具调用为边界：工具前后的文本**各自成条**，不会黏在同一条消息里
（此前 Telegram 把所有增量累进同一条流式消息，长任务于是堆成一坨）。摘要与正文的生成规则
（`tool_summary` / `tool_body` / `format_duration` / `normalize_todos` / `subagents_lines`）
在 `src/render.rs` 里只有一份，两侧共用。

## 对照（parity）工作流

Python 侧是语义基准，期望值由脚本在**真实现**上跑出来：

```bash
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

`tests/telegram_bot.rs` 用同类桩件钉住 Telegram 的显示口径：工具前后的正文各自成条、
工具消息原地收口、执行计划只占一条消息并原地替换、未授权用户零请求。

## 飞书入站持久队列（`feishu/inbox.rs`）

对齐 Python `omnicrawl/connectors/feishu_inbox.py`：`enqueue` 先落盘（`pending.jsonl`）再登记内存，
重启后 `recover` 回放尚未确认的事件；去重键走 `done.jsonl`（24 小时窗口，跨进程生效）；
`confirm` 因 JSONL 追加写而全量重写 pending；启动时 `done` 超过上限就按最新 ts 紧凑化；
目录不可写或写失败时降级为纯内存模式（进程内仍去重、不再跨重启持久化），绝不阻断消息接收。

与 Python 的差异：日志由调用方决定怎么记（内核连接器没有 logging 设施）；文件句柄按次打开而不是
常驻（都是追加写 + flush，写失败即降级）；追加行是紧凑 JSON、紧凑化后的 done 行才是默认分隔符，
两处都与 Python 逐字节对齐（`tests/fixtures/feishu_inbox_parity.json` 会比对落盘字节）。

`tests/feishu_inbox_parity.json` 共 6 个场景：基本往返（含重投与空键）、重启回放、窗口过期、
启动紧凑化、纯内存降级、损坏文件（坏行 / 旧版本 / 缺字段 / 过期 / 非对象 payload）。

## `[FILE:...]` 发文件（`feishu/file_send.rs`）

对齐 Python `fsapp.py` 的 `_send_local_file` / `_send_generated_files`：`~` 展开后要求路径存在且是文件
（两种失败各有固定文案），图片扩展名走 image 通道（消息类型 `image`、消息体 `{"image_key": key}`），
音视频走 `media`、其余走 `file`（消息体 `{"file_key": key}`）；上传没给出 key 就发失败提示。
标记扫描复刻 `\[FILE:([^\]]+)\]`：`[FILE:]` 不是标记，`[FILE: ]` 是（路径去空白后为空，
落到「输出路径不是文件」）。

上传与消息发送既可经 `FileTransport` 端口注入宿主实现，也可用内核自持的
`impl FileTransport for FeishuApi`：`feishu/upload.rs` 拼 multipart（先文本字段、后文件字段），
端点 `im/v1/images` 与 `im/v1/files`，`file_type` 复用 `files::FILE_TYPE_MAP`，bot 在回合收尾
扫描 `[FILE:]` 标记回传文件。
（`tests/feishu_file_send_parity.json`：12 例本地文件 + 6 例标记扫描，对照上传/发送调用序列与结果；
`tests/feishu_upload.rs` 用传输桩件钉住 multipart 字节形状。）

改了实现要让冻结数据集同步更新；卡片负载按**字符串**比对，缩进与分隔符也是契约。

## 尚未移植

- 飞书文件回传（`[FILE:]` 标记 → 上传 → 消息）已落地：`FeishuApi::upload_image` / `upload_file`
  与 `FileTransport` 实现，bot 在回合收尾扫描标记。Python 的 Telegram 连接器只有纯文本发送
  （`_send_message`），没有同类文件回传，因此这一侧无对应缺口。
- ~~`connectors/autostart.py` 的子进程自动启动与单例锁~~ 已落地：`src/autostart.rs`
  （`ConnectorProcessManager`：跨进程单例锁、子进程编排、日志尾与 Windows Job Object 收尾），
  由 `omnicrawl-entry/src/startup.rs` 在宿主启动时装配。平台单例锁覆盖连接器运行期：Python 由
  连接器子进程自己持锁，Rust 侧由监督器持有到子进程退出。
- 飞书 SDK 的 `Content-Disposition` 文件名解析已落地：`download_resource` 先取响应头的
  `filename*`（RFC 5987 扩展参数，非 ASCII 名字走这条）再取 `filename`（RFC 6266），两者都
  拿不到才回落成 `file_key`，再由资源类型补扩展名（`.jpg`/`.opus`/`.bin`）。只取最后一个
  路径分量，非 UTF-8 字符集与非法百分号转义一律归为「拿不到」。`HttpReply`
  因此多了响应头（小写名存），查表大小写无关。
- Telegram 的 HTTP 错误文案无法逐字复刻 `requests` 的异常文本，`TelegramApiError::Network`
  只保证分类与中文前缀一致；飞书侧同理（`FeishuApiError::Transport`）。
- 飞书 `_prewarm_agent`（Agent 预热线程）：宿主构建好 Agent 后交给连接器即可，不再单独预热。

## QQ 机器人（`qqbot/`）

对齐 QQ 开放平台 API v2（`https://bot.qq.com/wiki/agent-qqbot/` 与
`https://bot.q.qq.com/wiki/develop/api-v2/`）：鉴权用 `AppID + AppSecret` 换 AccessToken
（7200 秒，提前 5 分钟续期），事件走 **WebSocket 网关**（`GET /gateway` → `op=10 Hello`
→ `op=2 Identify` / `op=6 Resume` → `op=0 Dispatch`），不占本地端口、不需公网地址。

**平台形态决定了显示方式**，这与 Telegram/飞书不同：2025-04 起 QQ 关闭了主动消息，
所有下发都必须是对某条入站消息的被动回复（携带它的 `msg_id`），同一条入站消息只有
**5 次回复额度**（单聊 60 分钟、群聊 5 分钟），且**消息不能编辑**。因此：

- 没有流式预览：正文段在「工具调用」这个模型 pass 边界封口成一条 `◇ …` 消息；
- 工具调用/执行计划/子任务进度合并成少数几条进度消息，而不是每条一个；工具调用每次
  只占一行 `● 摘要 · ✓ 成功`，**工具输出不下发**（输出属于内部过程）；
- **中间过程（回执 + 正文段 + 进度 + 思考）最多 4 条**，第 5 条固定留给最终回答；
  `ReplyHandle::reserve` 在每次下发前预扣额度（中间额度与总额度），中间额度用满后
  `can_send_intermediate` 转 false，过程内容一律不再下发，一直攒到最终文本；
  已发过的正文段收尾时不重发（`remaining_after` 只补剩余部分）。

消息类型按内容自动选择（`msg_type` 决定哪个字段生效）：

- 纯文本（`msg_type=0`，`content`）：默认；
- Markdown（`msg_type=2`，`markdown.content`）：单聊/群聊且正文含平台支持的语法。
  频道场景需内邀开通，一律走纯文本。平台不认的写法在 `markdown.rs` 里**就地规整**
  而不是整条降级：围栏代码块→块引用、表格→列表项、`###`→`##`、列表前补空行、
  `* `→`- `、嵌套缩进统一 4 空格、行内反引号去掉、非公网图片退化成文字；被拒时
  自动按纯文本重发一次（同样占额度）；
- 富媒体（`msg_type=7`，`media.file_info`）：回复里的 `[FILE:路径]` 先
  `POST /v2/{users|groups}/{openid}/files`（`srv_send_msg=false`）拿 `file_info` 再发。
  按扩展名给 `file_type`（png/jpg→1、mp4→2、silk/mp3/wav→3、其余→4），二进制直传
  上限 5MB；文件读不到或超限时不下发富媒体、正文保留 `[FILE:…]` 路径（`media.rs`）。

摘要与正文的生成规则仍与另外两个连接器、TUI 共用 `src/render.rs` 一份实现。
`tests/qqbot_bot.rs` 用桩件钉住这些约束：正文段与工具段分离、三次工具调用合并成一条、
多 pass 长任务总下发条数 ≤ 5 且中间恰好停在 4 条、Markdown 走 `msg_type=2`、
图片标记经上传后按 `msg_type=7` 发出、未授权零请求、审批/提问桥、附件落到 `.agent_tmp/images`。

扫码绑定（`bind.rs`）照官方 `@tencent-connect/qqbot-connector` 的流程实现：
`lite/create_bind_task` 由本地生成 32 字节随机 key（base64），扫码完成后平台回
`bot_encrypt_secret = base64(nonce[12] || ciphertext || tag[16])`，本地用 AES-256-GCM
解出 AppSecret——Secret 不经第三方。CLI 入口是 `omnicrawl-host connector qqbot --bind`，
写回 `config.toml` 时保留其它段（`omnicrawl-api/src/qqbind.rs`）。

## MQTT 自动化回路（`src/mqtt/`）

连接器矩阵里的第四个入口，但形态完全不同：**遥测进、动作出**，没有消息时间线可渲染。

- 入口是传感器报文（温湿度、墒情、光照），出口是一条带 `request_id` 与 `ttl_seconds` 的下发指令；
- 规则层（阈值 + 持续时长 + 迟滞 + 冷却）永远在线，只在命中时**唤醒一次**模型；
- 模型只在两次触发之间「不存在」：没有沉睡进程，也不占运行名额；
- 模型输出被约束在动作表内、参数按 `min/max` 夹取，动作 id 不认识直接否决；
- 模型不可用时回落规则声明的 `fallback`，`fail_safe` 规则根本不问模型；
- 每次决策落一行审计（触发原因、窗口快照、动作、指令、回执），并作为下次唤醒的「最近动作」。

传输层是手写的 MQTT 3.1.1（连接/订阅/发布/确认/心跳），与飞书长连接同一条路线
（rustls + webpki 根证书，无原生依赖），不引第三方 MQTT 客户端。

宿主装配在 `omnicrawl-api/src/mqtt.rs`，两种规划器按 `[mqtt] planner` 选：

- `agent`（默认）：拉起内核子进程跑一个回合，模型可用白名单里的工具（默认只读 + 记忆/知识库/
  联网检索），审批固定自动审查；白名单外的工具既不声明给模型，宿主也不执行。
- `decision`：一次结构化决策——决策模型（`decision_models.toml`）可用时走 choice，
  未配置时回落主模型单轮补全。

两条路径都汇到同一份动作校验：动作必须在动作表内，参数按 min/max 夹取。

端到端测试用回环假 broker 跑完整回路：

```bash
cargo test -p omnicrawl-connectors --test mqtt_loop
```

使用说明与完整配置见 `rust/assets/docs/MQTT.md`。
