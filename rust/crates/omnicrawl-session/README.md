# omnicrawl-session

会话存储的 Rust 内核：会话事件与索引条目的模型、命名与时间校验。语义基准是
Python 侧 `omnicrawl/state/session_models.py`。

两个实现读写同一批会话文件（`.agent_sessions/index.json` 与 `sessions/<id>.jsonl`），
所以这一层是纯粹的数据契约：字段名、校验规则、错误文案、JSONL 行的字节布局都必须一致，
否则「Python 写的会话内核读不懂」——用户切一次实现就等于丢一次历史。

## 本片范围

- `SessionEvent`：会话 JSONL 里的一条版本化事件（`version`/`session_id`/`event_id`/`parent_id`/
  `type`/`created_at`/`payload`）；`create` 生成 id 与时间戳，`from_dict`/`to_dict` 走 JSON 边界，
  `to_json_line` 输出转录里的一行。
- `SessionIndexEntry`：`index.json` 里的会话目录条目，含 `archived_at` 与计数校验。
- `naming`：会话 id、事件类型、转录相对路径的校验与规范化，标题折叠，
  以及事件类型分组常量（消息事件 / 参与模型上下文的事件 / 空会话事件 / 子任务事件）。
- `time`：时间戳解析与格式化，对齐 Python `datetime` 的 ISO-8601 语义（UTC、微秒）。
- `SessionStoreError`：错误文案与 Python `SessionStoreError` 逐字一致。

## 存储层（`store.rs`）

- `SessionStore`：目录骨架（`sessions/`、`artifacts/`、`summaries/`、`exports/`、`archive/compacted/`）、
  `index.json`（`{"schema_version":1,"sessions":[…]}` + 换行）、`history.jsonl`、锁文件 `.session_store.lock`（记持有者 pid）。
- `start_session`：按时间戳分配会话 id（撞车重试）、写索引条目、追加 `session_started` 事件。
- `append_event`：读索引 → 找条目（`未找到会话：<id>`）→ 生成事件 → 追加一行到 `sessions/<id>.jsonl`
  → 更新索引（计数、标题、`last_event_type`、`updated_at`）→ 原子替换 `index.json`。
- `read_events`：坏行记为诊断（stderr）并跳过；`list_sessions` 按更新时间倒序（稳定排序保证同刻按创建顺序）。
- 路径边界：转录路径必须落在根目录内，越界拒绝（索引可能被外部改写，不能当可信输入）。

## 跨进程锁与耐久写（`locking.rs`）

对齐 Python `omnicrawl/state/session_locking.py`：同一个会话目录允许多进程访问，写路径靠 OS 级文件锁互斥。

- **锁原语**：Windows 用字节区间锁（`LockFileEx`，与 Python `msvcrt.locking` 同一个机制）、POSIX 用 `flock`。
  两个实现锁的是同一个文件、同一个区间，所以能互相排斥；进程退出时 OS 自动释放，不存在需要抢占的残留锁。
- **锁文件**：`.session_store.lock`，内容 `pid=<进程号>`；打开时**不**截断，截断发生在拿到锁之后。
- **两层串行**：先进程内互斥（线程之间不去争 OS 锁），再取跨进程文件锁；顺序固定避免交叉死锁。
- **超时**：默认 30s 轮询 50ms，文案与 Python 一致（`获取会话存储写锁超时（30.0s）：<锁文件路径>：<最后一次错误>`）。
- **耐久写**：追加转录与替换索引默认 fsync；索引走「同目录临时文件 + 原子替换」，Windows 上目标被短暂占用
  （拒绝访问）时按线性退避重试 8 次，替换后尽量 fsync 目录（Windows 目录句柄不支持则忽略）。
- **读路径不加锁**：与 Python 一致，`read_events` / `list_sessions` 是无锁读者。

读路径与写路径的取舍是有意的：会话读多写少，读锁会拖慢恢复；代价是读者可能看到「索引已更新、转录刚写完」
之间的瞬时状态，这与 Python 侧行为一致。

## 投影（`projection.rs`）

把事件流还原成模型上下文，对齐 Python `omnicrawl/state/session_projection.py` 的**纯函数**部分：

- `active_session_events`：回退过滤——`turn_undone` 自身不参与投影，它列出的 id 也被隐藏，
  让恢复、会话列表与客户端转录共享同一个逻辑视图。
- `session_title_from_events`：标题按「session_started → 首条 user_message → session_renamed」演进。
- `recover_run_guard_state` / `apply_run_guard_event`：运行护栏的待续文本与最后一份待办；
  增量版本与整体恢复共用规则，避免每个工具事件都重扫整份 JSONL。
- `event_to_model_message`：一条事件对应哪条模型消息（含 `tool_call_requested` / `tool_call_denied` /
  `tool_result` 的上下文文案、`compact_summary` 前缀、取消与暂停的兜底摘要）。
- `tool_result_message` / `interrupted_tool_result_message` / `format_tool_result_content`：工具结果的
  模型可见正文（运行时代理与恢复投影共用，保证逐字一致）。
- `complete_tool_pairing`：为缺失结果的 `tool_calls` 补「已中断」占位、丢弃孤儿或错配的 tool 消息，
  保证协议配对合法。
- `tool_result_output_text` / `function_tool_call`：输出优先级与 OpenAI 形状的 tool_call 构造。

> 有状态的那一层 `TurnHistoryProjector`（压缩边界、子任务结果、增量投影）尚未搬运。

工具参数的上下文文案是 `json.dumps(..., ensure_ascii=False, sort_keys=True)` 的等价物：键按字典序、
分隔符带空格、非 ASCII 原样输出——所以内核自己实现了一个小的 Python 风格序列化器，而不是直接用
`serde_json::to_string`（后者是紧凑分隔符且不排序）。

## 有状态投影（`history.rs`）

`TurnHistoryProjector` 是运行期与恢复期**共用**的状态机：运行期把每条新落盘事件喂进去，
轮次收尾取走本轮新增的协议消息；重启恢复用同一份实现投影整份事件流。两条路径共用唯一实现，
同一会话内的历史与重启后的历史才会逐字一致——前缀缓存因此不会失效。

- **工具批合并**：一批连续的工具调用合并成一条 `assistant` 消息（锚点取该批最后一次调用事件，
  `content` 原样保留、可能是 `null`；`reasoning_content` 只取该批首次调用事件里那份，避免逐条覆盖导致抖动）。
- **结果与补位**：每个结果各一条 `tool` 消息（锚点自身事件）；批次结束时仍未返回结果的调用补
  「已中断」占位；没有配对来源的孤立结果被丢弃；`tool_call_id` 缺失时按工具名回退配对
  （取最近的同名未完成调用）。
- **参数原文优先级**：先用落盘的 `arguments_json`，回退到 `json.dumps(..., ensure_ascii=False)` 的默认分隔符写法。
- **压缩边界**：带 `remaining_event_ids` 的摘要把历史替换成「摘要 + 窗口」；只带
  `remaining_message_count` 的旧摘要走按数量的兼容分支。`project_session_history` 只认最后一个边界，
  边界之后的全部事件完整保留（恢复指令、后续工具调用与最终回复都不能丢），边界之前只留窗口。
- **`project_compaction_boundary_history`**：用投影专用临时事件（形状合法的全零会话 id、
  固定 `projection-only-` 前缀的锚点 id）重建运行期历史，保证压缩前后两份上下文一致。

## 边界要求

- 只做纯逻辑，不碰文件系统：磁盘读写（追加转录、维护索引、锁、归档、导出、一致性诊断）在后续切片。
- 校验发生在系统边界（读文件、读协议载荷），内部调用走类型化入口，不重复做 JSON 类型判断。
- `to_json_line` 不含换行符；分隔符紧凑、不转义非 ASCII，与 Python 的
  `json.dumps(..., ensure_ascii=False, separators=(",", ":"))` 同形。

## parity 工作流

```bash
python rust/tools/gen_session_fixture.py      # 期望值来自 session_models.py 真实现
cd rust && cargo test -p omnicrawl-session
```

fixture `tests/fixtures/session_models_parity.json` 共 107 个用例：会话 id 9、事件类型 13、
相对路径 12、标题 7、时间戳 17、事件校验 15、索引条目 17、载荷计数 9、事件创建 5、转录行 3。
转录行一组是字节级比对：一侧多一个空格或换一个键序就会被抓到。

## 已知差异

- **非对象输入**：给事件/索引传一个 JSON 数组时，Python 会抛 `TypeError`（`data["version"]` 直接炸），
  内核统一收敛成 `SessionStoreError`，文案按「缺少字段：version。」给出。
- **超出 `u64` 的整数**：载荷计数在 Python 里是任意精度整数，内核按 `u64` 处理，溢出的值按无效算 0。
- **锁不可重入**：Python 的 `ProcessFileLock` 在同一线程内可重入（`start_session` 持锁后再调 `append_event`），
  内核用「公开写方法 + `*_locked` 内部方法」替代，持锁期间不得再调公开写方法。
- **锁超时按调用传入**：Python 把同一根目录上的超时取 max、轮询取 min 后粘在共享实例上；内核每次调用都按策略走。
- **IO 错误文案不逐字对齐**：Python 会把系统错误包成两层路径（`写入会话转录失败：<p>，写入文件失败：<p>，…`），
  内核只保留一层；底层 OS 错误文本本身也不同（`[Errno 13]` vs `Access is denied. (os error 5)`）。
- **载荷整理只搬了 inline 分支**：`tool_result` 会补 `output_sha256`/`output_size_chars`/`storage=inline`；
  超长输出转 artifact 文件、以及敏感值脱敏（`_redact_sensitive_values`）尚未移植。
- **`session_started` 的 runtime 身份**：Python 记源码指纹与已加载模块，内核记实现名与版本；该字段不参与跨实现比对。
- **索引顶层异常路径未覆盖**：Python 在索引不是对象时会重写索引文件，机制未确认，本片未实现也未纳入对照。
- **运行期参数原文提供者未搬**：Python 的投影器可以注入一个「已发往 Provider 的 arguments 原文」回调
  （只在内存里、不落盘）；内核侧等运行时代理接进来时再补，现在只走落盘参数的投影。
- **子任务结果的投影未搬**：`subagent.event` 一类事件目前不参与投影（Python 侧也主要由运行期聚合）。
- **工具参数里的浮点写法**：Python 的 `json.dumps` 用 `repr`（`1e+20`），内核用 `ryu`（`1e20`），
  `1e-5` 这类还会写成小数——只在参数含极端浮点数时影响上下文正文，语义等价。
- **`casefold()` 用 `to_lowercase()` 近似**：待办状态值（ASCII）一致，非 ASCII 大小写折叠可能有差异。
- **`is_relative_to` 尚未搬**：这一层没有出现路径包含判断，等存储读写切片需要时再补（注意 Windows 上
  Python 的路径比较是大小写不敏感的）。

## 依赖

`chrono`（时间戳解析/格式化）、`sha2`（会话 id 与事件 id 的随机后缀散列）、`serde`/`serde_json`。
`serde_json` 在 workspace 里开了 `preserve_order`：JSON 对象保留插入序，转录行才能与 Python 字节一致。
