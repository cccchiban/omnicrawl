# omnicrawl-api

本地 HTTP/SSE API 服务端：`omnicrawl/api/` 的 Rust 移植。契约来源是 `omnicrawl/docs/API.md`、
Python 真实现（`omnicrawl/api/`）与行为测试（`tests/test_api*.py`、`tests/test_settings_api.py`）。

分工与 Python 侧一致：`omnicrawl/api/service.py` 只做运行编排、不碰 HTTP 细节，`app.py` 负责
配置装载与装配。本 crate 先落地 HTTP 面（配置、鉴权、信封、CORS、路由装配），Agent 运行时
（回合、工具批次、会话、后台任务）由宿主层接入。

HTTP 栈用 `axum` + `tokio`（用户确认）。这与 workspace 里「内核自带阻塞式 HTTP/1.1 客户端、
不引入异步运行时」并不冲突：客户端只要一次请求-响应，服务端要同时扛住多个 SSE 长连接。

## 已搬范围

**`config` / `error` / `app`**：`APIConfig` 与 `load_api_config`（校验顺序与文案逐字对齐，
环境变量优先于 `config.toml` 的 `api` 段）；`ApiError` 与统一信封
（`{"data": ...}` / `{"error": {code, message, details}}`）；`/health` 公共、`/api/v1` 已匹配路由走
Bearer 鉴权（常量时间比较）、精确来源白名单 CORS、框架级 404/405 沿用 Starlette 形状。

**`runs`**：运行记录与事件流（单活动运行、事件递增 ID 与 SSE 分帧、保留窗口、游标过期、
审批/提问终态规则、取消把待决项一并置为终态、24 位十六进制 ID）。后端分两层：
`RunBackend::Memory`（默认，单进程）与 `RunBackend::Shared`（`shared_store`，见下）。

**`service`（管理面）**：`service.rs` 末尾的 `impl AgentService` 承载 Python 侧
`AgentAPIService.agent` 上的同名入口。差别是这里没有常驻 Agent：会话与项目按
`options.session_root` 现开现读（文件即事实来源），设置按请求读写 `config.toml`，后台任务与
MCP 由宿主进程内的管理器持有，模型目录走注入式网络端口（`model_discovery.rs`）。
`options_from_process` 里建一次 MCP 管理器与 MonitorManager，并同时挂进工具表
（`RegistryOptions`）与各自的 `/mcp`、`/monitors` 查询面；`kernel_program` 记下内核路径，
供运行期重起内核（切换会话 / 工作区）。

**`shared_store`**：`api/` 的跨进程运行状态存储。Python 用 SQLite；Rust 侧改成**单文件 JSON
快照 + 跨进程文件锁**（workspace 没有 SQLite crate，引入 `rusqlite` 要新增依赖并编译 C 源）。
每次操作在锁内完成「读 → 改 → 原子写回」，语义与内存后端逐条对齐；`api.workers > 1` 时由
`main.rs` 换成 `RunBackend::Shared`（与 Python 的触发条件一致）。快照分区与 Python 表一一对应：
`runs`（内嵌 events / confirmations / questions）、`decisions`（跨进程决策投递）、
`subagent_events` + `subagent_counters`（会话级后台事件与独立 id 计数器）、`task_sources`
（task_id → 来源 Run/Session）；淘汰运行记录时连带清理同名关联数据，与 Python `_delete_run`
同规则。服务层按「共享后端 → 用共享存储 / 内存后端 → 用进程内会话事件流」分支，
对应 Python 服务里的 `self._store is not None`。

**`openapi`**：`/openapi.json`（OpenAPI 3.1）与 `/docs`（Swagger UI）。Python 靠 FastAPI
自动生成；Rust 无反射，按 `api/routes/*` 的手工清单构造同一份契约（路径、标签、
路径/查询参数、请求体模型），两个端点都在鉴权层之外（对应 `docs/API.md` 的说明）。

**多进程监听**：`api.workers > 1` 时二进制先以监督进程启动，按 worker 数用
`OMNICRAWL_API_WORKER` 环境变量拉起自己的子进程；子进程各自起内核/隔离工作区，并用
SO_REUSEPORT 绑定同一端口（等价 uvicorn 的多 worker）。不支持 SO_REUSEPORT 的平台
（Windows、Solaris/illumos）退化为单进程并打印告警，运行状态仍走共享存储。

**`routes/query.rs`**：查询参数与请求体的取值校验，按 Python 侧 Pydantic 模型的限长、
取值域与失败形状逐条实现；校验失败一律 `422 VALIDATION_ERROR` 加逐字段 `details`。

### 已搬端点（`api/routes/*` 全覆盖）

| Python 源 | 端点 | 状态 |
| --- | --- | --- |
| `routes/system.py` | `GET /runtime` | 1/1 |
| `routes/runs.py` | `/runs` 6 个端点（含 SSE） | 6/6 |
| `routes/support.py` | `GET /history`、`GET /skills`、`GET /mcp`、`POST /memory/clean` | 4/4 |
| `routes/projects.py` | `/projects` 增删改查、`/overview`、`/import`、`/pin`、`/switch` | 8/8 |
| `routes/sessions.py` | 列表/新建/诊断/事件/恢复/重命名/压缩/归档/删除/导出/artifact | 12/12 |
| `routes/settings.py` | `GET /settings` + 10 个域 PUT | 11/11 |
| —（Python 侧无此端点） | `PUT /settings/mcp` | 新增 |
| `routes/monitors.py` | 列表 / 单个 / 日志 SSE | 3/3 |
| `routes/configuration.py` | `/models`、`/models/catalog`、`/models/refresh`、`/models/current`、`/reasoning`、`/approval` | 6/6 |
| `routes/subagents.py` | `/subagents`、`/subagents/events`、`/subagents/{id}`、`/subagents/{id}/cancel` | 4/4 |
| `app.py`（FastAPI 自动文档） | `GET /docs`、`GET /openapi.json` | 2/2 |

跨 crate 的配套改动：

- `omnicrawl-host`：`MonitorManager` 新增查询面（`tasks()` / `task()` / `poll_view()` /
  `wait_for_events()` 与三个对外视图结构）；`TurnRunner` 新增 `set_approval_mode`、
  `apply_session_settings`、`compact_session`、`manage_subagents`、`drain_notifications`、
  `rebuild_registry`（运行期重建工具表，供 MCP 设置热更新用）；`RunnerOptions` 加 `Clone`。
- `omnicrawl-ipc`：协议新增 `session.compact` 与 `subagent.query`（命令表与往返测试样本同步更新）。
- `omnicrawl-cli`：内核处理 `session.compact`（压完把历史换成摘要 + 保留窗口，回
  `{summary, compacted}`）与 `subagent.query`（复用 `SubAgentTaskManager` 的 list/get/cancel，
  回 `{unavailable, tasks, task, result}`）。
- `omnicrawl-compaction`：`CompactionDriver::manual_compact`、`AfterTurnReport.summary`；
  压缩载荷收尾抽成 `apply_compaction`，回合结束与显式压缩共用。
- `omnicrawl-session`：`SessionStore::prompt_history` /
  `read_session_events_with_diagnostics`。

两个「协议没有对应命令」的点按宿主侧等价做法落地：

1. **换会话**（新建 / 恢复 / 归档 / 工作区切换）：`KernelSession::open` 支持按 session_id 续跑，
   因此宿主重起内核并把目标会话写进 `initialize`（新建 = 空 id，工作区切换 = 同一会话换根，
   会话目录不绑工作区）。
2. **会话级后台事件流**（`/subagents/events`）：宿主侧投影。回合内由回合线程的 sink 直接喂；
   回合外由 `start_event_pump()` 起的线程周期性排空内核通知（`drain_notifications`）。
   单进程写进进程内 `SubagentFeed`，`workers > 1` 写进共享存储的 `subagent_events`，
   SSE 侧只在游标之后有新事件时推送。

## 跨进程语义

`decisions` / `subagent_events` / `subagent_counters` / `task_sources` 四张表与 `/docs`、
`/openapi.json` 均已落地。`workers > 1` 的多进程监听按 uvicorn 同构实现（监督进程 +
SO_REUSEPORT）；Rust 侧不做进程内存共享，跨进程可见的状态统一经共享存储读写：

- 人工决策（取消 / 审批 / 提问）由任意 worker 写进 `decisions`，持有 Run 的所有者在阻塞等待
  循环里取走；审批/提问本身仍是共享记录上的原子抢占（重复决议 → 409）。
- 会话级后台事件与任务来源写进共享存储，所有者与旁观 worker 读到同一份事件；id 由
  `subagent_counters` 单调分配，事件受保留窗口裁剪也不回退。
- SO_REUSEPORT 只负责把连接分到各个 worker，不保证粘性；客户端后续请求可能落在任意 worker。

## 对照与验证

- Python 基线：`python -m pytest tests/test_api.py tests/test_settings_api.py tests/test_api_module_boundaries.py tests/test_api_multiworker.py -q`。
- 配置面逐条对照：`rust/tools/gen_api_config_fixture.py` + `tests/config_parity.rs`。
- 装配面端到端：`tests/server.rs` 在真实回环端口上跑鉴权、信封、CORS、预检与框架级错误。
- 协议面：`omnicrawl-ipc` 的 `tests/bridge_round_trip.rs` 覆盖 `Command::METHODS` 全部方法。
- 管理面：本机禁止编译，因此只做只读静态校验——逐个 Python `@router.*` 路径与 Rust 注册表比对
  （`routes/mod.rs` + `app.rs::module_routers`），并核对调用到的 `omnicrawl-config` /
  `omnicrawl-session` / `omnicrawl-extensions` / `omnicrawl-mcp` / `omnicrawl-host` /
  `omnicrawl-ipc` / `omnicrawl-cli` / `omnicrawl-compaction` 公开签名与结构体字段。
  编译与端到端验证待用户在本地执行。

## 已知差异

- 数字字符串里的下划线分隔（Python `int("1_0") == 10`）不支持。
- `/openapi.json` 是手工维护的契约（非 FastAPI 反射生成），字段摘要与限长对齐 `models.py`，
  但响应体 schema 统一为通用对象；`api` 段的容器类型当 `host` 时只保证落到同一条非回环文案。
- 不支持 SO_REUSEPORT 的平台（Windows、Solaris/illumos）`api.workers > 1` 退化为单进程。
- 存储错误的状态码：Python 侧会话/项目的 `AgentError` 会落到通用 `500 INTERNAL_ERROR`；
  这里按「资源缺失 → 404、其余 → 400」收紧（文案不变），删除当前活跃会话改为
  `409 SESSION_ACTIVE`，工作区路径非法改为 `400 INVALID_WORKSPACE`。
- 运行期改配置（模型 / 推理强度 / 审批模式）、压缩、后台任务查询在回合进行中返回
  `409 RUN_ACTIVE`：内核主循环在跑回合时被占用，`session.settings` / `session.compact` /
  `subagent.query` 要到回合结束才会被处理；Python 侧 Agent 在进程内，可以即时生效。
- 内核命令失败统一回 `502 KERNEL_COMMAND_FAILED` / `KERNEL_RESTART_FAILED` /
  `KERNEL_SETTINGS_FAILED`；嵌入模式（`with_runner`）没有内核启动信息，切换会话回
  `503 KERNEL_RESTART_UNAVAILABLE`。
- 共享存储的差异：介质是 JSON + 文件锁（不是 SQLite）；保留窗口按
  `api-runs-<host>-<port>.json` 单文件维护，四张跨进程表的分区与 Python 表同名同义。
  运行记录带 `owner_pid`，装配共享后端时按所有者 PID 存活情况收敛孤儿运行
  （`reconcile_orphan_runs`，对应 Python 的 `pid_is_running` 判定与失败文案）。
  旧快照缺 `owner_pid` 时按 0 处理，与 `pid_is_running(0)` 为假一致，仍会被收敛。
- 管理面读的是磁盘上的 `config.toml` 与运行期管理器，Python 读的是 Agent 内存态；两者只在
  「有别的进程改过配置」时有差别。
- 设置写端点没有「运行态」可回滚（先校验、再原子落盘）；`PUT /settings/features` 的 `plugins`
  写盘之后会调 `AgentService::set_plugins_enabled` 热切换运行期（事务式重建 Worker 与执行计划，
  失败回 `502 PLUGIN_RUNTIME_FAILED`；未装配插件运行期的嵌入模式回 `503 PLUGIN_RUNTIME_UNAVAILABLE`），
  响应里多出 `plugins`（运行态摘要：`enabled` / `handlers` / `diagnostics` / 插件表）与 `diagnostics`。
  `GET /settings` 的 `features.plugins` 现在是**运行期**是否生效（原先是配置值），运行态细节在
  同响应的 `plugins` 字段；插件运行期在 `AgentService::spawn` 装配（`options.plugins`），
  嵌入模式（`with_runner`）默认没有。
- `PUT /settings/mcp` 是本移植**新增**的端点：Python 侧 MCP 设置只由 Textual 工作台在进程内改，
  宿主自持 MCP 之后需要一个能写 `[mcp]` 段并立刻重连的入口。语义与同一组设置端点一致：字段全部
  可选（`enabled` / `default_timeout_seconds` / `policy` / `servers` 整表替换 /
  `server` 单条增改带 `original_name` 改名 / `delete_server`），未传字段保持原值。写盘后调
  `AgentService::reload_mcp` 热更新：按新配置重连、重建工具表并把新声明经 `session.settings`
  下发给内核（回合在途时按 `409 RUN_ACTIVE` 语义放弃运行期改动，但磁盘已是新值，响应里用
  `applied` / `detail` 如实回报）。Server 名、传输、地址与超时边界沿用读取器的校验：写盘后回读
  一次，不合格就按写前快照**逐字节还原**配置并回 `400 INVALID_SETTING`。
- `PATCH /sessions/current` 返回会话索引条目，Python 返回 `SessionState`；
  `POST /sessions/{id}/resume` 与 `/sessions/current/archive` 返回 Rust 版会话状态视图
  （索引元数据 + 转录投影 + `pending_user_text` / `todo_items`）。
- 会话根已对齐 Python 的 `~/.OmniCrawl/.agent_sessions`（不绑工作区）。
- 请求体 JSON 本身语法错误时，axum 的 `Json` 拒绝体是纯文本 `400`，Python 回 `422` 校验信封。
- `PUT /settings/context` 与 `/settings/context_compaction` 的边界取值报错类型按
  `greater_than_equal` / `less_than_equal` 给出，Pydantic 对 `gt=0` 用的是 `greater_than`。
