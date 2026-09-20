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

## 会话生命周期编排（`store.rs`）

对齐 Python `SessionStore` 的同名方法，全部走「先进程内、再跨进程」的同一把写锁：

- `rename_session`：写 `session_renamed` 事件（标题进转录，索引重建时能还原最后一次命名），空标题报
  `会话标题不能为空。`。
- `export_session_markdown`：导出文件落 `exports/chat_export_<id>_<YYYYMMDD_HHMMSS>.md`，再写
  `session_exported`（只记相对路径与 `format=markdown`，不把正文写回转录）；空内容报 `导出内容不能为空。`。
- `archive_session` / `unarchive_session`：写 `session_archived` / `session_unarchived` 事件 → 转录在
  `sessions/` 与 `archive/` 之间移动 → 用新路径与 `archived_at` 改写索引条目（标题与计数保留）。
  重复归档报 `会话已在归档中：<id>`；取消归档对未归档会话是空操作。
- `delete_session` / `discard_empty_session`：删转录 + artifact 目录（清理失败不阻塞）+ 索引条目；
  丢弃空会话只在「未归档、`message_count == 0`、且事件全部属于 `EMPTY_SESSION_EVENT_TYPES`」时返回真。
- `list_sessions_filtered`：Python `list_sessions` 的工作区/项目过滤、归档可见性、`limit` 收敛（1..100）；
  无参 `list_sessions()` 是内核内部的全量视图（含归档、不截断），两者语义差别写在方法文档里。
- `list_project_paths`：按大小写折叠去重与排序。
- `read_artifact_text` / `write_tool_result_artifact`：委托 `artifact.rs`，路径规范化、会话归属与目录边界一致。
- `append_event` 的载荷整理统一走 `SessionArtifactStore::prepare_event_payload`：超长工具输出转 artifact、
  值级脱敏与补字段都由同一处负责，`store.rs` 不再自带一份 inline 分支。

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

## 记忆层格式（`memory.rs`）

长期记忆建在会话目录之上：正文落成 Markdown，元数据落进 `index.json`。本片搬的是**格式契约**，
读写与检索在后续切片。

- **Markdown 记录**：frontmatter 只有两部分（`timestamp` 与 `related_directories`，逐条缩进两项、
  字符串按 Python 规则转义反斜杠与引号），随后空行、正文（去尾部空白）、结尾换行。
- **正文提取**：`read_markdown_body` 先统一换行符，没有 frontmatter 就返回整份文本，
  有则取第二个 `---` 之后的部分——两条路径都去首尾空白。
- **目录归一化**：反斜杠折成正斜杠、空白折成 `-`、连续 `/` 折成一个、逐片段过滤非法字符后**小写**；
  空目录、`.`、`..`、含上级跳转、盘符或根斜杠开头都会拒绝，且拒绝文案与 Python 逐字一致。
- **文件路径归一化**：必须以 `.md` 结尾；目录走目录规则（小写），文件名只去非法字符、**保留大小写**。
- **关联目录去重**：无效条目（例如模型用 `.` 表示「全库」）视为不限定目录直接跳过，而不是报错。
- **时间戳**：转本地时区、精确到秒渲染（与 Python `isoformat(timespec="seconds")` 同形）；
  无时区的写法按 UTC 解析后再转本地。
- **索引条目**：字段校验顺序与 Python 一致——先逐项检查类型与取值，**最后**才做路径/目录/时间戳归一化，
  因此多个字段同时出错时报的是 Python 那一处。

### 与 Python 的一处实现差异

Python 用正则做目录/文件名的字符过滤；内核用手写匹配器（字符类逐个判断），行为一致但不引正则依赖。

## 记忆排序（`memory_ranking.rs`）

记忆存储把「选哪个目录、怎么摘要、怎么排序」这些策略放在这一层，纯逻辑、不碰磁盘。

- **目录分类**：按关键词启发式取第一条命中的规则（偏好 / 项目 / 代码 / 错误 / 外部服务 / 任务），
  都不命中落到 `task-history/general`。
- **摘要生成**：去掉成对代码围栏 → 去掉行首的列表/引用/标题标记 → 空白压成单空格 → 若第一个句末标记
  落在上限内就截到那里 → 仍超长则按**字符**截断并补省略号。
- **正文合并**：新内容已被旧内容包含时保留旧的，否则追加「补充：」段。
- **打分**：目录匹配分（同级 3.0 / 上下级 2.0 / 关联命中 1.5 / 关联上下级 1.0）、检索 token 命中比例、
  完整查询串命中加分、反复使用的轻微偏好、新鲜度轻微偏好；关联展开按层数递减。
- **检索 token**：ASCII 词块（≥2 字符）+ 中文片段本身（≤8 字）与全部 2/3 元组。

正则等价物全部用手写匹配器实现（成对围栏取最短、行首标记带空白回退、空白折叠），不引入依赖。

### `text_similarity`：difflib 的逐位复刻

「两条记忆是否近似重复」由 `difflib.SequenceMatcher.ratio()`（Ratcliff-Obershelp 匹配）判定，阈值 0.9——
它是**合并还是新建**的依据，所以这里按同一算法复刻，而不是找近似替代。两个容易漏的点：

1. **autojunk**：`b` 长度 ≥ 200 时，出现次数超过 `n // 100 + 1` 的元素会被剔出索引（视作"太常见，不具区分度"）。
2. **非垃圾扩展**：剔出索引的高频元素并不等于丢弃——主流匹配之外还有一步向两侧延伸，
   把相邻的"非垃圾"元素一并吃进来。少了这一步，**两条完全相同的长文本会被算成 0 相似度**（写错过一次）。

同前：`j2len` 的越界下标语义（`j == 0` 时查不到、取 0）也必须照搬，用饱和减法会让连击长度无界增长。

## 记忆存储：索引与读取（`memory_store.rs`）

长期记忆建在会话目录之上：正文是 Markdown，元数据在 `index.json`。这一层已落地「索引 + 读取加深」链路。

- **索引解析**：读 `index.json`，取顶层 `memories` 数组（缺失当空、不是列表则报错），逐条走条目校验。
- **索引落盘**：先按（存储目录、路径）排序，再写同目录临时文件并原子替换（Windows 被占用时短重试）。
- **读取加深**：`read(ids)` 先去掉空白与重复 id，然后对真正读到的记忆刷新时间戳、`touch_count + 1`、
  按新时间戳重写 Markdown，最后返回全文。索引里没有、或文件不在磁盘上的 id 直接跳过。
- **路径防护**：条目路径解析后必须仍落在记忆根目录内，否则报「记忆路径越界」。

读取加深会写盘，所以对照数据集里把时间戳统一归一化成占位：两侧都跑同一批步骤，
比对「每步返回的记录」与「最终文件快照」。

## 记忆写入与清理（`memory_store.rs`）

- **批量校验先行**：一次 `write` 的全部请求先校验与归一化（正文非空、存储目录归一化、
  关联目录去重并补上存储目录），全部通过才开始落盘——避免后续请求非法时前面已经写出
  Markdown 而索引没更新。
- **创建还是合并**：先在既有条目里找「近似重复」——内容归一化后相同，或相似度达到 0.9
  （就是上面复刻的 difflib 判定）。命中则合并（`补充：` 段、`touch_count + 1`、刷新时间戳与摘要），
  否则新建。
- **id 生成**：时间戳精确到秒，冲突时追加三位序号；同秒写入多条会得到 `-002`、`-003`。
- **失败回滚**：落盘中途出错时删掉本次新建的文件、把改动过的文件还原成旧内容（尽力而为），
  不留「有文件没索引」的记忆。
- **写入后清理**：按 `7 + touch_count` 天规则删除过期记忆，并递归清掉空目录；
  只有真的删了东西才重写索引。

对照数据集里 id 与时间戳都要归一化：id 先按出现顺序编号（先扫输出、再扫文件名与正文），
替换时**先长后短**——同秒生成的 id 互为前缀，短的先替换会串到长的里面去。

## 记忆检索入口（`memory_store.rs`）

- **search**：按查询文本与候选目录打分（复用排序层的 token 抽取与目录匹配打分）。
  没有查询也没有候选目录时返回**全部**候选，方便先看一眼有哪些记忆。
  排序键依次是分数、时间戳、使用次数（降序），完全并列时**保持索引顺序**（稳定排序）。
  索引里有但文件不在磁盘上的条目直接跳过。
- **expand_related**：从种子记忆出发，用它们的存储目录与关联目录当第一层「前沿」；
  命中的条目再把自身目录并入下一层前沿，逐层衰减。层数上限 3、条数上限 20。
- 两个入口的条数上限语义与 Python `_clamp` 一致：非法值按 1 处理，合法值夹在 [1, 上限] 内。

对照数据集里的条目时间戳取**一年前**的固定时刻：这样打分的「新鲜度」项恒为 0，分数与排序才能
确定性比对；返回的时间戳统一按 UTC 秒渲染，避免比对依赖运行机器的时区。

## 提示词段落与旧目录迁移（`memory_store.rs`）

- **format_prompt_section**：按作用域标签与四个工具名生成 L0 调用规则，并列出推荐存储目录与
  当前已有目录（去重排序、最多 20 条；一条都没有时给一句提示）。
- **migrate_legacy_memory**：目标不存在时**直接改名**，完整保留旧索引、时间戳与触碰次数；
  目标已存在时先经 `MemoryStore` 把旧正文导入，再把源目录改名为
  `<原名>.migrated-<时间戳>` 备份——**导入失败就不动源目录**，避免启动过程造成不可逆的数据丢失。
  源与目标同路径、源不存在都按「不迁移」返回；源不是目录则报错。
  与 Python 的差异：不做 `expanduser()`（内核侧路径由调用方给全）。

## 工具输出 artifact 与核心凭据脱敏（`artifact.rs` / `redaction.rs`）

超长工具输出不直接进模型上下文：完整内容写进会话 artifacts 目录，上下文只留头尾预览与相对路径；
**落盘的每一份文本都先过脱敏**，公开消费者永远接触不到原始文本。

- **阈值**：内联 8KB（超过就转存）、预览 1200 字符（头尾各半、中间省略提示）。
- **载荷投影**：`prepare_event_payload` 先处理 HTML artifact，再递归脱敏，最后决定内联还是落盘；
  无论哪种都补上 `output_sha256` 与 `output_size_chars`。
- **产物命名**：`tool_result_<sha256 前 16 位>.txt`、`html_preview_<sha256 前 16 位>.html`、
  子任务 `subagents/<task_id>.json`——同名即同内容，天然可去重。
- **路径安全**：artifact 路径必须是 `artifacts/` 下的安全相对路径，且只能读当前会话目录；
  越界、上级跳转、非 `artifacts` 开头一律拒绝。
- **脱敏顺序**（`redaction.rs`，六轮固定顺序）：私钥块 → 赋值式 → `authorization` 赋值 →
  `Bearer` → 厂商密钥（`sk-/ak-/ah-`）→ GitHub 令牌 → 云 access key；结构侧递归清理凭据字段
  （含 `x-` 前缀的头名），超长列表只保留前 100 项。

Python 用正则表达，这里全部用**手写匹配器**实现（字符类与量词都是固定形态），不引正则依赖。
两个必须照搬的细节：可选组 `(?: [A-Z0-9]+)?` 是**贪婪 + 回溯**（不带类型的私钥头也要能匹配），
`` 边界要按**字符**判断而不是按字节（否则会切坏多字节字符）。

## 边界要求

- 只做纯逻辑，不碰文件系统：磁盘读写（追加转录、维护索引、锁、归档、导出、一致性诊断）在后续切片。
- 校验发生在系统边界（读文件、读协议载荷），内部调用走类型化入口，不重复做 JSON 类型判断。
- `to_json_line` 不含换行符；分隔符紧凑、不转义非 ASCII，与 Python 的
  `json.dumps(..., ensure_ascii=False, separators=(",", ":"))` 同形。

## parity 工作流

```bash
python rust/tools/gen_session_fixture.py      # 期望值来自 session_models.py 真实现
python rust/tools/gen_project_fixture.py      # 项目列表存储：纯函数 + ProjectStore 流程轨迹
python rust/tools/gen_session_records_fixture.py  # 记录解码与诊断：迁移 / 解码 / 转录读取 / 索引文档 / 切行
python rust/tools/gen_prompt_history_fixture.py   # 提示历史：展示清洗 / 条目构造与解析 / 存储轨迹
python rust/tools/gen_turn_snapshot_fixture.py    # 工作区快照：脚本化 git 场景（捕获 / 回退 / 冲突）
cd rust && cargo test -p omnicrawl-session
```

fixture `tests/fixtures/session_models_parity.json` 共 107 个用例：会话 id 9、事件类型 13、
相对路径 12、标题 7、时间戳 17、事件校验 15、索引条目 17、载荷计数 9、事件创建 5、转录行 3。
转录行一组是字节级比对：一侧多一个空格或换一个键序就会被抓到。

fixture `tests/fixtures/project_parity.json`：纯函数 11 组（展示名清洗与报错、路径键、隔离工作树判定、
路径归一与报错、条目解析与报错、扫描排除、git 根、变量展开）加 5 条 ProjectStore 轨迹
（CRUD 往返、扫描入库、总览聚合、非 JSON 文件、顶层形状错误）。轨迹在真实临时目录上跑，
逐例比对每步返回值与最终 `projects.json` 字节；两侧用固定时刻，落盘时间戳逐字一致。

## 项目列表存储（`project.rs`）

对齐 Python `omnicrawl/state/project.py`：读写 `<session_root>/projects.json`、扫描会话索引得到项目
路径、创建与导入目录，以及只读聚合的项目总览（按 git 仓库根归并子目录会话、排除隔离工作树与
临时残留）。路径归一化对齐 `Path.resolve(strict=False)`（存在时解析真实路径、不存在时词法归一），
Windows 上剥掉 `\\?\` 前缀。

三处已知差异：`casefold()` 用 `to_lowercase()` 近似；`os.path.expandvars` 只实现
`$VAR` / `${VAR}` /（Windows）`%VAR%` 这个子集，未定义变量原样保留；落盘用的临时文件名与
Python 的 `<file>.json.tmp` 不同（走 `locking::atomic_write_text` 约定），最终文件字节一致。

## 记录解码与诊断（`records.rs`）

对齐 Python `omnicrawl/state/session_records.py`：按版本分发解码（`migrate_event_dict` 支持 v1 /
v0 / 缺 version 的遗留格式，只做内存迁移不改写磁盘）、事件解码（字典与 JSONL 单行）、转录整份读取
（坏行不阻断其余有效事件，尾部半行降级为 warning）、索引文档解析与构造；诊断码与严重级保持稳定
（`DIAG_*` / `SEVERITY_*`，严重级沿用 `consistency.rs` 的同一套取值）。

两处已知差异：诊断明细里的 `json_error` 是各自 JSON 库的错误文本（对照片两侧都替换成占位符再比对）；
大转录的只读内存映射快路径未搬，一律整份读取后按 Python `splitlines()` 的规则切行（`split_lines_python`
覆盖 `\r`、`\r\n`、`\v`、`\f`、`\x1c`–`\x1e`、`\u{85}`、`\u{2028}`、`\u{2029}`）。
`log_record_diagnostics` 保留入口但由调用方提供 sink：内核没有 Python 的 logging 设施。

fixture `tests/fixtures/session_records_parity.json`：迁移 8、字典解码 8、单行解码 6、转录读取 6
（真实临时文件，含空行、中间坏行、尾部半行、会话不一致）、索引解析 9、索引构造 2、切行 7。

## 提示历史（`prompt_history.rs`）

对齐 Python `omnicrawl/state/prompt_history.py`：`.agent_sessions/history.jsonl` 的数据模型与追加语义
（`append` 空提示返回 `None`；`search` 按时间倒序去重、支持项目 / 会话 / 关键词过滤与 1–100 的条数上限）、
整份读取的结构化诊断（坏行不阻断，尾部半行降级 warning）、展示清洗（换行统一、4000 字符截断）。
条目字段与落盘行字节与 Python 一致（紧凑 JSON + 保留插入序）；`pasted_contents` 与展示文本都过脱敏。

与 Python 的一处差异：`json_error` 明细是各自 JSON 库的错误文本（对照片两侧替换成占位符再比对）；
`log_record_diagnostics` 在 Python 侧直接写 logging，内核由调用方决定怎么记。

fixture `tests/fixtures/prompt_history_parity.json`：展示清洗 6、条目构造 5、条目解析 10，
另加 3 条存储轨迹（追加与查询、坏行文件、空提示被忽略）——轨迹在真实临时目录上跑，
追加传固定时刻，逐例比对每步返回值与最终 `history.jsonl` 字节。

## 工作区轮次快照（`turn_snapshot.rs`）

对齐 Python `omnicrawl/state/turn_snapshot.py`：只依赖用户仓库自身的 Git 状态，不创建对象库。
`capture` 跑 `git diff HEAD --binary --full-index`（带 `core.quotepath=false`）与
`git ls-files --others --exclude-standard`；`transition` 先校验当前状态仍等于轮次终点，
再 `git reset --hard HEAD`、`git apply --binary --whitespace=nowarn`，最后删掉本轮新增的未跟踪文件，
返回「轮次中被删除、没有内容副本」的提示列表。`has_head` 带 60 秒 TTL 缓存。

与 Python 的差异：`git` 子进程的超时由内核自己轮询实现（`std::process` 没有内置超时），
超时文案里的参数表按 Rust 的 `Debug` 形式给出；`SnapshotConflictError` 折成
`SnapshotError::is_conflict()`。

fixture `tests/fixtures/turn_snapshot_parity.json`：5 个脚本化场景（往返回退、轮次后被改导致冲突、
非 Git 目录、被删除的未跟踪文件无法恢复、干净工作区），两侧执行同一份脚本，
比对补丁 sha256、未跟踪清单、回退返回值与错误文案。

## 已知差异

- **非对象输入**：给事件/索引传一个 JSON 数组时，Python 会抛 `TypeError`（`data["version"]` 直接炸），
  内核统一收敛成 `SessionStoreError`，文案按「缺少字段：version。」给出。
- **超出 `u64` 的整数**：载荷计数在 Python 里是任意精度整数，内核按 `u64` 处理，溢出的值按无效算 0。
- **锁不可重入**：Python 的 `ProcessFileLock` 在同一线程内可重入（`start_session` 持锁后再调 `append_event`），
  内核用「公开写方法 + `*_locked` 内部方法」替代，持锁期间不得再调公开写方法。
- **锁超时按调用传入**：Python 把同一根目录上的超时取 max、轮询取 min 后粘在共享实例上；内核每次调用都按策略走。
- **IO 错误文案不逐字对齐**：Python 会把系统错误包成两层路径（`写入会话转录失败：<p>，写入文件失败：<p>，…`），
  内核只保留一层；底层 OS 错误文本本身也不同（`[Errno 13]` vs `Access is denied. (os error 5)`）。
- **载荷整理与 Python 同源**：`tool_result` 的 inline/artifact 分流、`output_sha256`/`output_size_chars`/`storage`
  与值级脱敏都由 `artifact.rs` 的 `prepare_event_payload` 负责，与 Python `_prepare_event_payload` 一致。
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
