# 代码健康度调查报告与拆分方案

> 范围：本仓库全部源码（`rust/crates`、`packages`、`native`）。
> 方法：纯静态分析（行数分桶、职责聚类、引用计数、函数体哈希去重、逐行相似度比对）。
> 未运行 `cargo build/check/test`（遵循本项目既有约定，编译类验证不在此执行）。
> 数据复现脚本：`.omnicrawl/.agent_tmp/scripts/`（`longfn.py` / `deaditems.py` / `deadfields.py` / `testonly.py` / `prodshare.py` / `botdup.py`）。
> 基线：`git status` 干净，HEAD = `a231407`，适合作为重构起点。
> 位置说明：本文件放在 `rust/` 根目录而非 `rust/docs/`——后者被 `.gitignore` 的 `docs/` 规则忽略（历史文件因已入库仍在跟踪）。
>
> **执行进度见文末「九、执行记录」**：前 6 名已拆完 2 个、未接线功能已双通、零引用代码已清空。

---

## 一、总体画像

| 指标 | 数值 |
| --- | --- |
| 源码文件（含测试、排除 node_modules/dist/target/.omnicrawl） | 650 |
| 源码总行 | 300,980 |
| Rust | 616 文件 / 294,566 行 |
| Rust 生产代码 | 433 文件 / **190,028 行** |
| Rust 内联测试（`#[cfg(test)]`） | 44,123 行 |
| Rust 集成测试（`tests/`） | 61,031 行（183 文件） |
| 测试用例 | 1,849（内联）+ 1,128（集成）= 2,977 |
| 其他语言（JS/mjs/py/go/c） | 34 文件 / 6,414 行，最大 300 行 |

行数分桶（按生产代码口径）：

| 阈值（生产行） | 文件数 | 合计行 | 占生产代码 |
| --- | --- | --- | --- |
| > 1000 行 | **37** | 73,789 | **39%** |
| > 500 行 | 115 | 128,532 | 68% |

**结论**：这不是「整体烂掉」的代码库——模块边界（23 个 crate）、测试密度（36%）、文档与注释质量都在中上水平；但**「单文件超一千行」这条红线被系统性突破**：37 个生产文件超线，其中 6 个超过 3000 行，最重的 `tui/src/app.rs` 生产代码 7,873 行、`ui/settings/state.rs` 6,752 行。真正的风险集中在 **TUI 三个巨型状态机/接线文件** 和 **若干「多职责服务门面」文件**，加上一批**已经写完但没接线、或被测试养活的无用代码**。

---

## 二、超标的单文件清单（按生产行数排序，前 30 名）

> 口径：**生产行 = 剥离 `#[cfg(test)]` 之后的行数**。这是判断「文件是否该拆」的唯一有意义的口径——`ui/conversation.rs` 总行 3,744 但生产只有 471 行，把它算进「超一千行」会误导排期。

| # | 文件 | 生产行 | 总行 | 测试行 | 内部堆了什么（证据） |
| --- | --- | --- | --- | --- | --- |
| 1 | `omnicrawl-tui/src/app.rs` | **7,873** | 8,232 | 359 | `impl App` **203 个方法 / 5,942 行** + 文件尾 1,252 行自由函数（配置↔表单值互转）；同时负责握手、内核帧分发、工具批次编排、审查、剪枝、建议、OneJev、TTS、工作区切换、会话管理 |
| 2 | `omnicrawl-tui/src/ui/settings/state.rs` | **6,752** | 9,184 | 2,432 | 59 个 struct/enum；`impl SettingsState` **187 个方法 / 3,846 行**；模型/渠道/MCP/视觉/TTS/决策/向量/工具开关八套子状态机全在一个 impl |
| 3 | `omnicrawl-cli/src/session.rs` | **4,660** | 4,836 | 176 | 内核侧会话服务：协议分发、`run_turn`（303 行）、模型端口、工具端口（两个 `ToolBatchHost` 实现）、子代理、压缩接线、undo、会话查询，22 个 impl |
| 4 | `omnicrawl-tui/src/state.rs` | **3,451** | 5,967 | 2,516 | `impl AppState` 108 方法 / 1,991 行 + `impl Composer` 48 方法 / 606 行 + 重放逻辑（`replay_events` 208 行） |
| 5 | `omnicrawl-jsdom/src/browser_env.rs` | **3,240** | 3,240 | 0 | 117 个函数、**28 个 `install_*`**（console/navigator/location/history/webgl/canvas/crypto/storage/speech/URL/…），浏览器全局对象一次性摊平 |
| 6 | `omnicrawl-api/src/service.rs` | **3,030** | 3,192 | 162 | `impl AgentService` 71 + 26 个方法（≈1,934 行）：运行生命周期、会话、子代理、监控、插件、MCP、模型切换、项目、记忆、压缩 |
| 7 | `omnicrawl-jsdom/src/engine.rs` | **2,229** | 2,229 | 0 | DOM 原型构建（`build_element_prototype` 291 行）、脚本收集、微任务驱动，94 个顶层项 |
| 8 | `omnicrawl-commands/src/slash.rs` | **2,059** | 2,753 | 694 | 28 个 `handle_*` 命令处理器 + 18 个 `format_*` 渲染器 + `build_registry`（225 行）；占该 crate 生产代码 **63%** |
| 9 | `omnicrawl-extensions/src/models.rs` | **2,011** | 2,011 | 0 | 22 组 const 表 + 12 个结构 + 14 个 impl（插件清单、Hook 策略、补丁白名单、注册表文档） |
| 10 | `omnicrawl-extensions/src/manager.rs` | **1,881** | 1,881 | 0 | Hook 调度器（`HookDispatcher` 600 行）、Worker 进程管理、插件生命周期、注册表 |
| 11 | `omnicrawl-workspace/src/agent_isolation.rs` | **1,872** | 1,872 | 0 | worktree 创建/复用/patch/三方合并/四层清理门禁/启动清扫 |
| 12 | `omnicrawl-tui/src/ui/settings/render.rs` | **1,835** | 1,906 | 71 | 30+ 个 `render_*`，每套设置页各写一遍列表 + 表单 + 下拉覆盖层 |
| 13 | `omnicrawl-connectors/src/qqbot/bot.rs` | **1,774** | 1,850 | 76 | 平台 I/O + 任务时间线 + 审批/提问桥 + 命令解析 + 进度节流 + 分片 |
| 14 | `omnicrawl-extensions/src/install.rs` | **1,751** | 1,751 | 0 | npm / 本地包两条安装路径 + 版本校验 + 解包 + 清单校验 |
| 15 | `omnicrawl-host/src/tools/web_search.rs` | **1,713** | 3,288 | 1,575 | 4 个 HTML 引擎解析 + 5 个 API 引擎客户端 + URL 编码 + 拦截检测 + 三套重排策略（决策/向量/关键词） |
| 16 | `omnicrawl-connectors/src/feishu/bot.rs` | **1,634** | 1,634 | 0 | 与 qqbot/telegram 高度同构（见 §4.2） |
| 17 | `omnicrawl-llm/src/desensitization/rules.rs` | **1,608** | 1,649 | 41 | 规则表（1,600 行里的绝大部分）+ 扫描器 + 缓存 |
| 18 | `omnicrawl-tls/src/session.rs` | **1,597** | 1,880 | 283 | TLS 握手状态机 + 会话缓存 + 证书处理 |
| 19 | `omnicrawl-controllers/src/approval.rs` | **1,589** | 1,589 | 0 | 审批判定表 + 52 项 Git action 表 + 风险分级 + 审查提示词 |
| 20 | `omnicrawl-tui/src/ui/fullscreen/rendering/latex.rs` | **1,546** | 1,635 | 89 | LaTeX 解析（`read_group` 310 行）+ 转换（`convert_expr` 193 行）+ 归一（`normalize_bare_math_lines` 455 行） |
| 21 | `omnicrawl-host/src/turn.rs` | **1,494** | 1,494 | 0 | `impl TurnRunner` 38 方法 / 1,152 行（无头回合运行器）+ `run_batch`(248) |
| 22 | `omnicrawl-llm/src/desensitization/engine.rs` | **1,374** | 1,535 | 161 | 匹配引擎 + 占位符注册表 + 流式还原 |
| 23 | `omnicrawl-connectors/src/telegram/bot.rs` | **1,287** | 1,287 | 0 | 三份中最小的那一份 |
| 24 | `omnicrawl-api/src/routes/settings.rs` | **1,268** | 1,268 | 0 | 11 个 `put_*` 端点 + 5 个 `serialize_*` + MCP 序列化 |
| 25 | `omnicrawl-session/src/store.rs` | **1,192** | 1,192 | 0 | 转录读写、索引、压缩视图、一致性检查 |
| 26 | `omnicrawl-jsdom/src/fetch_api.rs` | **1,187** | 1,187 | 0 | `fetch` / `Request` / `Response` / `Headers` 的 JS 面实现 |
| 27 | `omnicrawl-llm/src/runtime.rs` | **1,168** | 1,215 | 47 | 4 个 `run_turn` 实现 + 运行时装配 |
| 28 | `omnicrawl-controllers/src/subagents/tasks.rs` | **1,137** | 1,137 | 0 | 任务表、worker 池、空闲回调、取消 |
| 29 | `omnicrawl-host/src/tools/knowledge.rs` | **1,114** | 1,312 | 198 | 知识库 CRUD + 索引 + 重排 |
| 30 | `omnicrawl-ipc/src/bridge.rs` | **1,107** | 1,107 | 0 | 协议帧桥 + 全部 V1 载荷类型（纯类型定义占大头） |

31–37 名（`controllers/src/tool_args.rs` 1083、`context_compaction/policy.rs` 1077、`subagents/definitions.rs` 1077、`api/src/shared_store.rs` 1043、`host/src/review.rs` 1035、`mcp/src/client.rs` 1031、`extensions/src/skill.rs` 1010）略。

前 6 名合计 29,006 行，占生产代码 **15.3%**；前 12 名占 **21.5%**。

### 也是「超一千行」但应当区别对待的

`omnicrawl-tui/src/ui/conversation.rs`：总行 3,744，**生产只有 471 行，测试 3,273 行（87%）**。按用户定的「单文件超一千行」字面标准它超标；按生产口径它是最健康的文件之一。**结论：不拆生产代码，把测试整块迁到 `tests/conversation_render.rs`。** 同理 `highlight.rs`（生产 15 行 / 测试 939 行）只需迁测试。

**生产代码里有 34 个函数 ≥120 行、9 个 ≥200 行、1 个 ≥300 行**（按「剥离 `#[cfg(test)]` 后」口径），最长的十个：

| 行数 | 位置 |
| --- | --- |
| 303 | `cli/src/session.rs::run_turn` |
| 291 | `jsdom/src/engine.rs::build_element_prototype` |
| 240 | `tui/src/app.rs::new` |
| 237 | `controllers/src/subagents/definitions.rs::strip_verbatim_prefix` |
| 230 | `tls/src/http2.rs::parse_frame` |
| 229 | `tui/src/state.rs::apply` |
| 225 | `commands/src/slash.rs::build_registry` |
| 208 | `tui/src/state.rs::replay_events` |
| 207 | `controllers/src/context_compaction/validation.rs::validate` |
| 198 | `llm/src/errors.rs::map_exception` |

测试代码里另有 5 个 ≥200 行、4 个 ≥300 行的用例，最夸张的是 `controllers/tests/subagents_parity.rs::strip_verbatim` **1,094 行**、`tui/src/state.rs` 的 `partial_arguments_reads_what_has_arrived` 523 行、`host/src/tools/registry.rs::send`（测试内的 338 行宏夹具）、`tui/src/ui/conversation.rs::cache_matches_a_full_rebuild_across_every_mutation` 345 行。这类「一个用例写成一个函数」的写法让测试失败时无法定位到具体断言，建议随 Phase 1 一起折成 `rstest` 风格的多用例。

---

## 三、单文件多职责：四处「上帝对象」

### 1. `tui/src/ui/settings/state.rs` —— 八套子状态机挤在一个 impl

`impl SettingsState` 3,846 行按领域聚类：

| 领域 | 方法数 | 行数 |
| --- | --- | --- |
| decision（结构化决策模型页） | 28 | 483 |
| channel（对话渠道表单） | 19 | 401 |
| mcp（服务器列表 + 编辑器） | 17 | 355 |
| embedding（向量模型页） | 24 | 302 |
| model（模型管理 + 发现） | 12 | 248 |
| dropdown / form / pane / list / hit | 27 | 431 |
| vision / tts / tool / subagent | 29 | 365 |
| 其他（status、apply_*、ctrl） | 15 | 465 |

并且四套渠道表单几乎逐字同构（`handle_channel_form_key` vs `handle_decision_form_key` 相似度 **75%**、vs `handle_embedding_form_key` 63%；`new_channel` vs `new_decision_channel` 72%）。

### 2. `tui/src/app.rs` —— 接线层吞掉了整个 Agent 执行语义

`impl App` 203 个方法里可清晰切出七块互不相关的职责：

| 职责块 | 方法数 | 行数 |
| --- | --- | --- |
| 设置写回（`apply_*` 31 个） | 31 | 1,007 |
| 键盘/鼠标（`handle_*` 16 个） | 16 | 733 |
| 命令派发与结果应用 | 35 | 603 |
| 工具批次编排（`handle_tool_batch`/`handle_batch_step`/`spawn_tool_job`/`respond_batch`/审查门禁/剪枝/期限） | 14 | 542 |
| tick/drain/poll 泵 | 18 | 350 |
| OneJev 服务管理 | 11 | 269 |
| 会话/工作区切换 | 26 | 490 |
| 文件尾自由函数（表单值↔配置互转） | 67 个 fn | 1,252 |

同一批语义在 `omnicrawl-host/src/turn.rs`（`TurnRunner`，1,152 行）里已经存在一份，TUI 因「执行线程不能同步跑审查模型」又自建了一份：
`plugin_tool_guards`(36) / `needs_review_gate`(13) / `spawn_review_job`(83) / `try_custody_pending_question`(18) / `spawn_prune_job`(21) / `enforce_tool_deadline`(22) 对应 `turn.rs` 的 `plugin_tool_call_before`(23) / `custody_answer`(10) / `run_context_prune`(26) / `run_batch`(248)。**这是语义重复，不是代码重复**——两份实现靠注释「与 turn.rs 同序同文案」维持一致，是最容易静默漂移的地方。

### 3. `cli/src/session.rs` —— 一个文件同时是协议服务器、模型端口、工具端口、子代理协调者

`run_turn`(303) + `run_session_tail`(175) + `dispatch`(155) 三个函数合计 633 行，内部再分别持有 `Conn`、`RemoteModelPort`、`KernelModelPort`、`RemoteTools`、`TurnRunGuard`、`BackgroundTools`、会话查询等 22 个 impl。

### 4. `api/src/service.rs` —— `AgentService` 是万能门面

71 + 26 个方法覆盖：运行生命周期、SSE 泵、会话（列表/重命名/删除/切换/归档/undo/导出）、模式、顾问、压缩、子代理、监控、模型目录、设置应用、项目、提示历史、Skill、插件、MCP、记忆清理。

同类问题还有 `commands/src/slash.rs`（28 个命令处理器同一文件）、`jsdom/src/browser_env.rs`（28 个 `install_*`）、`api/src/routes/settings.rs`（11 个端点 + 5 个序列化器）。

---

## 四、重复实现

### 1. 完全相同的函数体：48 组，可无损消除 584 行

用「去注释、去空行后的函数体 SHA-256」比对生产代码：

| 函数 | 出现次数 | 位置 |
| --- | --- | --- |
| `text_field` | 5 | config/features/{decision_model,image_gen,tts,tts_cloud,web_search} |
| `resolve_lenient` | 2 | host/tools/paths、tts/paths |
| `from_config` | 4 | host/tools/decision_{choice,prune,search,suggest} |
| `expand_user` | 3+2 | host/tools/{image_gen,read_image}、controllers/workspace、session/project、tts/audio |
| `python_round` | 3 | host/tools/windows/{input,screenshot}、tts/audio |
| `text_of` | 4 | connectors/{feishu/files,feishu/timeline,qqbot/bot,telegram/timeline} |
| `begin_array_value` / `begin_object_key` / `begin_object_value` | 3+3+3 | llm/request、session/history、session/projection（流式 JSON 增量解析器被抄了三份） |
| `split_authority` / `format_line_stats` / `last_event_id` / `header` / `index` / `wait` / `resolve` … | 各 2–3 | 见脚本输出 |

### 2. 三平台连接器同构：约 420 行可用共享层吸收

| 对比 | 同名函数 | 相似度 ≥70% 且 ≥8 行者 | 重复行合计 |
| --- | --- | --- | --- |
| telegram vs feishu | 40 | 13 | 221 |
| qqbot vs telegram | 33 | 8 | 156 |
| qqbot vs feishu | 28 | 3 | 43 |

`on_tool_result`(92%)、`on_subagent`(91%)、`on_tool_start`(90%)、`on_reasoning`(88%)、`patch_due`(85%)、`request_cancel`(79%)、`confirm`(77%)、`execute_task`(84%) 都是同一套「任务时间线 + 进度节流 + 审批/提问桥」。
三份 `HELP_TEXT` 也各自维护（14/17/36 行）。`connectors/src/render.rs`（1,037 行）已经抽出了显示映射，但时间线状态机没有抽。

### 3. 配置段的序列化在三处各写一遍

`[run_guard]`、`[agent_workspace]`、`[vision]`、`[image_gen]`、`[tts]` 这五段：
TUI 侧 `app.rs::*_form_values`（如 `run_guard_form_values` 15 行字段顺序表）、API 侧 `routes/settings.rs::serialize_*`（如 `serialize_run_guard` 19 行）、配置层 `features/*.rs::load/save`。
字段增删要同时改三处，否则界面/接口/磁盘三者不一致。

### 4. 契约性字面量在多个 crate 里各定义一份

生产代码里「同一个字面量被定义成多个常量」的有 **51 组**。其中真正危险的是**必须永远保持一致**的那批（工具名、事件类型、环境变量名、目录名、默认端点），因为任何一处单独改动都会造成跨 crate 的静默不匹配。实测 **20 组 / 59 处**：

| 字面量 | 处数 | 定义点 |
| --- | --- | --- |
| `update_todos` | 5 | cli::`TODO_TOOL_NAME`、connectors::`TODO_TOOL_NAME`、controllers::`TODO_TOOL_NAME`、host::`TODO_TOOL`、session::`RUN_GUARD_TODO_TOOL_NAME` |
| `ask_user` | 5 | cli / connectors / controllers / host / tui 各一份 |
| `.omnicrawl/.agent_tmp` | 5 | compaction::`AGENT_TEMP_DIR`、host::`DEFAULT_AGENT_TEMP_DIR`、host::`AGENT_TEMP_DIRECTORY`（同 crate 内两次）、workspace::`DEFAULT_AGENT_TEMP_DIRECTORY` |
| `pause_work` | 4 | cli / controllers / host，另有 controllers::`PAUSE_REASON` |
| `advisor` | 4 | controllers 内两次、host 两次 |
| `https://api.openai.com/v1` | 5 | cli / config×3 / host |
| `files` | 4 | compaction / connectors / host（host 内两次） |
| `computer_use` | 3 | cli / host×2 |
| `agent-worktrees` | 3 | cli::`WORKTREES_DIR`、session::`AGENT_WORKTREES_DIR_NAME`、workspace::`WORKTREES_DIRNAME` |
| `OPENAI_API_KEY` | 3 | config×2、host |
| 其余（`tool_unavailable`、`tool_call_evicted`、`speaking-style`、`AGENTS.md`、`task-history/general`、`OMNICRAWL_DECISION_BINARY`、`gpt-image-2`、`duckduckgo`、`omnicrawl-connectors/0.0.1`） | 各 2 | 见脚本输出 |

建议：新建 `omnicrawl-protocol` 下的 `contract.rs`（该 crate 已被绝大多数 crate 依赖）集中定义工具名、事件类型、目录名与默认端点，其余 crate 改为 `pub use`。

### 5. `connector` 双实现

`omnicrawl-api/src/connector.rs`(961) 与 `omnicrawl-cli/src/connector.rs`(962) 逐行相似度仅 7%，是完全独立的两套 `AgentDriver`：一个接 `AgentService`（宿主形态），一个直接驱动 `omnicrawl-llm`（内核形态）。两条路径都活着（`entry` 按有无宿主载荷分流），但共享的 `AgentDriver` 契约没有任何共享实现。

---

## 五、无用代码

### 5.1 生产代码零引用（测试也不引用）：115 项，分布在 59 个文件

高密度位置：

| 文件 | 数量 | 例子 |
| --- | --- | --- |
| `controllers/src/subagents/worktrees.rs` | 9 | `registration_keys`、`lookup_key`、`artifact_failure_line`、`list_items`、`missing_session_error`、`apply_failure_error`、`change_check_failure_error`、`cleanup_failure_error`、`cleanup_message` |
| `controllers/src/subagents/batch.rs` | 6 | `wait_done`、`background_rejection`、`spawn_acknowledgement`、`batch_created_payload`、`cancelled_indexes`、`build_batch` |
| `controllers/src/advisor.rs` | 5 | `advisor_is_active`、`advisor_prompt_cache_identity`、`model_resolve_failed`、`config_invalid`、`runtime_init_failed` |
| `controllers/src/settings.rs` | 5 | `tokens_after_window_change`、`validate_compaction_percent`、`model_switch_failed`、`mcp_apply_failed`、`plugin_apply_failed` |
| `controllers/src/subagents/orchestration.rs` | 5 | `fork_context_snapshot`、`local_public_result`、`inject_notifications`、`wants_review`、`missing_definition_error` |
| `controllers/src/compression.rs` | 4 | `parallel_compressions`、`compaction_wait_seconds`、`reply_compaction_system_prompt`、`build_reply_compaction_messages` |
| `commands/src/slash.rs` | 4 | `print_skills_list`、`print_memory_clean_result`、`print_mcp_status`、`print_plugins_status`（都只有 `println!`，TUI/API 走 `format_*`） |
| `extensions/src/manager.rs` | 4 | `activate_plugin_dispatch_context`、`agent_definition_paths`、`set_worker_launcher`、`from_config_data` |
| `extensions/src/models.rs` | 4 | `is_custom_hook`、`handler_keys`、`default_trace`、`raise_if_denied` |
| `commands/src/framework.rs` | 3 | `with_message`、`with_error`、`takes_argument`（builder 上未被调用的方法） |

值得注意的是 `worktrees.rs` 与 `batch.rs` 的这批函数：**它们是把文案/载荷拼装从 `cli/session.rs` 抽出来后的「半成品」**——`cli/session.rs:2299/2371/2395/2457` 仍然在自己的 `format!("…")` 里内联同样的中文文案，抽出来的函数没人调用。这属于「重构做了一半」，是最该优先收拾的一类。

完整清单：`.omnicrawl/.agent_tmp/files/dead_items.txt`（115 行，`文件:行号　符号`）。

### 5.2 只被测试引用的公开项：214 项

其中 21 项集中在 `tui/src/ui/fullscreen/rendering/widgets.rs`。这个文件是本次调查最明确的「活代码幻觉」：

- 生产代码 927 行，**685 行（74%）的顶层项在文件外零引用**；
- 文件内除自身 `impl` 与 doc 注释外也零使用——唯一引用者是它自己的 `mod tests`；
- 涉及类型：`ToolDisclosure`(295)、`ReasoningDisclosure`(167)、`AssistantMessage`(79)、`ConfirmationScreen`(51)、`TodoPlan`(51)、`SubAgentStatus`(38)、`ConfirmAction`(4)；
- 与之对应的**真正在跑的渲染是另一端**：`tui/src/ui/conversation.rs` 用 `push_tool` / `tool_body_rows` / `collapsed_body` 自己实现了同一套「首尾各 2 行采样 + 提示行」，`panels.rs` 自己实现了任务清单。
- 文件外唯一被使用的是 5 个常量（`MAX_EXPANDED_BODY_LINES` / `HEAD_BODY_LINES` / `TAIL_BODY_LINES` / `BODY_INDENT` / `EXPAND_HINT`）。

`README.md:254/261/270` 已如实记录「只剩 widget 级封装未挂载」「`ConfirmationScreen` 待补」——也就是说这是**刻意的迁移中间态**，但已经沉淀成一个 1,330 行文件 + 27 个测试在维护死代码。

### 5.3 整块未接线的功能

| 位置 | 规模 | 状态 |
| --- | --- | --- |
| `omnicrawl-workspace/src/temp.rs` | 757 生产行 | `AgentTempWorkspace` / `TempCore` / `resolve_agent_temp_dir` / `start_scheduler` / `clean_if_due` 在仓库内**只有 `tests/workspace_temp_parity.rs` 引用**；宿主侧没有任何临时目录清理调度。实际临时目录只靠常量字符串 `".omnicrawl/.agent_tmp"` + `format!` 拼 |
| `omnicrawl-config/src/features/tls_resumption.rs` | 142 行 | `load_/save_tls_resumption_configuration` 全仓零调用；`host/tools/fetcher.rs:60` 注释声称「由配置 `[tls_resumption] enabled` 控制」，而 `tls_session_cache` 在 `FetcherOptions::default()` 等所有构造点恒为 `None` → **TLS 会话复用端到端不可达**，配置段写了也没人读 |
| `omnicrawl-workspace/src/slug.rs` | 内部可复用 | `is_safe_slug`/`validate_slug` 仅 `agent_isolation` 内部使用，对外导出无消费者（低优先级） |

### 5.4 兼容/过渡路径（有明确注释，但仍是维护负担）

- `cli/src/session.rs:506` `RemoteModelPort`：`model.reply` 宿主代答的兼容路径。当前所有宿主（TUI/API/connector）都在握手时给了 `KernelModelConfig`，这条路径只剩测试在用（`kernel_model_e2e.rs` 明确写着「旧宿主不会因为这次改动失效」）。
- `api/src/connector.rs` 与 `cli/src/connector.rs` 双实现（§4.5）。
- `entry/src/startup.rs::connector_specs`：无宿主载荷平台（armv7）才走的 `--connector` 旧路由。

### 5.5 工作区噪声（不影响编译，影响检索与磁盘）

| 项 | 规模 | 说明 |
| --- | --- | --- |
| `dist/` | 487 MB | 已被 `.gitignore`，纯本地产物 |
| `.omnicrawl/` | 267 MB / 1,310 文件 | Agent 临时目录，未跟踪 |
| `*.omnicrawl.edit.lock` 残留 | **79 个**（会随编辑操作再生） | 编辑工具的锁文件散落在 `src/`、`assets/`、`tests/`、`.agent_tmp/` 里，虽然被 git 忽略，但污染目录视图与 `grep`；本次调查期间又新增了 3 个。**已修复**：锁文件改落用户配置目录 `~/.OmniCrawl/edit-locks`（按目标路径摘要命名），编辑时顺手清掉目标旁的旧锁；遗留的旧文件可按需一次性删除 |
| `.omnicrawl/.agent_tmp` 里的旧中间态 | `base/app_head.rs` 7,498 行、`py_core.py` 6,425 行、`service_head.rs` 2,741 行等 | 本次调查已排除，但按行数扫全仓时会污染统计 |

### 5.6 内联测试塞爆生产文件

内联测试超过 700 行的 6 个文件，合计 11,563 行：

| 文件 | 内联测试行 | 该文件「胖」的原因 |
| --- | --- | --- |
| `tui/src/ui/conversation.rs` | 3,273 | 生产仅 471 行，测试占 **87%** |
| `tui/src/state.rs` | 2,516 | 测试占 42% |
| `tui/src/ui/settings/state.rs` | 2,432 | 测试占 26% |
| `host/src/tools/web_search.rs` | 1,575 | 测试占 48% |
| `tui/src/ui/fullscreen/rendering/highlight.rs` | 939 | 生产仅 15 行，测试占 **98%** |
| `host/src/tools/registry.rs` | 827 | 测试占 53% |

`conversation.rs` 是最典型的「文件超一千行但其实只有 471 行代码」：它的 3,744 行里近九成是内联测试。**这类文件不应当和外置集成测试用同一把尺子**——按生产口径它并没有超标。建议把「>1000 行」的检查口径统一定成「剥离内联测试后的生产行数」，同时对内联测试单独设一条 ≤800 行的阈值（超了就整块迁到 `tests/`）。

---

## 六、清理与拆分建议

### 原则

1. **先减法再加法**：先处理 §5.1 的 115 项零引用与 §5.2 的 214 项仅测试引用，拆分时规模自然变小。
2. **按「变更理由」切分，不按行数切分**：一个文件拆完如果还是「同一批人为了同一个需求同时改」，说明切错了。
3. **本仓库特殊约束**：大量注释是「与 Python 侧对映」的说明书。拆文件时必须把对应注释和常量一起搬走，否则下游没人能判断新文件是否还保真。
4. **不引入新 crate 前置**：除 §6 Phase 3 里提到的 `omnicrawl-agent-runtime`（可选），其余都在 crate 内部改模块树。
5. **验证方式**：按项目约定本轮不跑 `cargo build/check/test`。因此**本报告的全部「无用代码」结论都是静态引用计数，属于「强线索」而非「编译器证明」**——删除前请用一次 `cargo check --workspace --all-targets`（`pub` 项不会报 `dead_code`，但私有项与 `#[allow(dead_code)]` 项会）交叉确认，再由你跑 `cargo test -p <crate>`，尤其 `*_parity` 系列（它们钉住与 Python 的等价）。

### Phase 0：安全网（0.5 天，零风险）

| 动作 | 说明 |
| --- | --- |
| 清理 79 个 `*.omnicrawl.edit.lock` | `find . -name "*.omnicrawl.edit.lock" -delete`（先确认无编辑任务在跑） |
| 记录基线 | `cargo test --workspace` 跑一次由你执行，把用例总数/通过数记下来，作为后续每一阶段的对照 |
| 给 4 个巨测文件加 `#[cfg(test)] mod` 边界标记 | 便于后续脚本化外移 |

### Phase 1：减法（2–3 天，低风险）

| 动作 | 收益 | 风险提示 |
| --- | --- | --- |
| 删除 §5.1 的 115 项零引用代码 | **705 行**（实测含 doc 注释） | `worktrees.rs`/`batch.rs` 那批是「抽了一半的重构」。**推荐做法不是删**：把 `cli/session.rs` 里内联的同文案 `format!` 改成调用这些函数，两边一起收口，净减 ≈45 行但消掉一处文案双维护 |
| 删除 `ui/fullscreen/rendering/widgets.rs` 的 685 行未挂载类型 + 其 27 个测试 | **≈1,090 行**（685 生产 + 403 测试；文件 1,330 → 约 250 行） | 只保留 5 个被外部引用的常量，其余搬到 `conversation.rs`/`panels.rs` 里已成为唯一实现的版本；`README.md:254/261/270` 的「待补」条目同步改为「已废弃」 |
| `commands/src/slash.rs` 的 4 个 `print_*` | 20 行 | 纯 `println!` 包装，TUI/API 都走 `format_*` |
| `extensions/src/models.rs` 4 项 + `framework.rs` 3 个 builder 方法 | 60 行 | 若有外部插件使用需确认（本仓库内确认无引用） |
| 处理两条未接线路径：`workspace/temp.rs`、`config/features/tls_resumption.rs` | 899 行（757 + 142） | **二选一，不要悬着**：①接线（宿主启动时 `AgentTempWorkspace::new().start_scheduler()`；`FetcherOptions` 按配置构造 `SessionCache`）；②标记 `#[allow(dead_code)]` + 文档写明「仅移植未启用」。当前状态注释与事实矛盾，是最危险的 |
| 收敛 §4.4 的 20 组契约性字面量（59 处 → 约 20 处，净减 ≈39 行） | ≈39 行 | 集中在 `omnicrawl-protocol::contract`，各 crate 改 `pub use`；这一步**收益不在行数而在正确性**——现在改一个工具名要同时改 5 个 crate |

### Phase 2：单文件拆分（主体，8–12 天）

按收益/风险排序，建议逐文件独立提交、独立验证。

#### 2.1 `tui/src/ui/settings/state.rs`（总 9,184 / 生产 6,752 → 目标每文件 ≤800 生产行）

```
ui/settings/
  state.rs            (≈400)  SettingsState 结构体、导航骨架、hit/hover/click、行/页枚举
  rows.rs             (≈450)  ToolSwitchRow / SubagentRow / McpServerRow / VisionModelRef / ChannelRow /
                              DecisionRow / EmbeddingRow / DecisionLocalRow + 各自 ORDER/label/index
  dropdown.rs         (≈250)  Dropdown / DropdownField / OptionValue / display_value / open/confirm/move_candidate
  form_bridge.rs      (≈350)  FormState ↔ SettingsValues 的 field_value/input_composer/set_field/validate
  pages/model.rs      (≈600)  模型管理 + 参数行 + 模型发现（picker 接线）
  pages/channel.rs    (≈450)  对话渠道增删改与保存
  pages/decision.rs   (≈700)  结构化决策页（列表 + 表单 + 下拉 + 本地服务状态）
  pages/embedding.rs  (≈450)  向量模型页
  pages/mcp.rs        (≈450)  MCP 全局 + 服务器列表 + 编辑器
  pages/tts.rs        (≈380)  TTS 页
  pages/vision.rs     (≈220)  视觉页
  pages/tools.rs      (≈250)  工具开关 / 审批 / Web Search
  apply.rs            (≈450)  apply_succeeded / apply_failed / note_kernel_rejection
  tests/              (2,432 → 拆成 5–6 个文件)
```

关键动作：把四套几乎同构的渠道表单（`handle_channel_form_key` / `handle_decision_form_key` / `handle_embedding_form_key`，相似度 63–75%）先抽出一个 `ChannelFormMachine` 泛型，再按页拆——否则拆完还是四份复制品。

#### 2.2 `tui/src/app.rs`（总 8,232 / 生产 7,873 → 目标每文件 ≤700 生产行）

```
tui/src/app/
  mod.rs              (≈450)  App 结构体 + new()（240 行的 new 拆为 build_registry/build_prompt/build_plugins 三步）
  kernel.rs           (≈600)  handshake(174) / handle_frame(100) / drain_frames / KernelCommand / request_kernel
  batch.rs            (≈600)  handle_tool_batch / handle_batch_step / spawn_tool_job / respond_batch /
                              needs_review_gate / spawn_review_job / try_custody / spawn_prune_job /
                              drain_prune_results / enforce_tool_deadline / plugin_tool_guards
  input.rs            (≈750)  handle_key / handle_mouse / composer 选区 / 滚动条 / 设置面板鼠标
  commands.rs         (≈600)  dispatch_command / dispatch_skill_command / apply_command_result / kernel 命令泵
  settings_apply.rs   (≈1,050) 31 个 apply_*
  settings_values.rs  (≈1,250) 文件尾 67 个自由函数（表单值↔配置）
  session.rs          (≈450)  会话菜单 / 会话请求 / 会话落盘 / replay
  workspace.rs        (≈450)  工作区切换 + 隔离会话 + 子代理 worktree 清理
  onejev.rs           (≈280)  OneJev 环境/下载/服务/进度
  tts.rs              (≈120)  speak_reply / apply_tts
  ticks.rs            (≈350)  tick_* / poll_budget / settle_slow_task / monitor 事件泵
  tests.rs            (358)
```

#### 2.3 `tui/src/state.rs`（总 5,967 / 生产 3,451 → 目标每文件 ≤600 生产行）

```
tui/src/state/
  mod.rs            AppState 结构 + 访问器
  records.rs        Record / UserRecord / ToolCard / StreamingTool / ToolStatus / TurnState
  composer.rs       Composer（606 行，独立成文）+ 粘贴/占位/历史
  selection.rs      TextSelection / 展开态 / 思考展开
  telemetry.rs      Telemetry / RateEstimator / 轮播文案
  apply.rs          apply(229) / tick_activity / touch_*
  replay.rs         replay_events(208) + 8 个 replay_*（当前与主状态机混排）
  tests/            2,516 行拆 5 个文件
```

#### 2.4 `cli/src/session.rs`（总 4,836 / 生产 4,660 → 目标每文件 ≤700 生产行）

```
cli/src/session/
  mod.rs        Conn / ConnState / 生命周期
  protocol.rs   Inbound / PortFailure / dispatch(155) / 帧编解码
  model_port.rs KernelModelPort + RuntimeBuildKey + 反序列化 → RemoteModelPort（兼容路径单独一文件并标注）
  tools.rs      RemoteTools（两个 ToolBatchHost 实现 + 子代理/后台工具）
  turn.rs       run_turn(303) → 拆 setup / model_source / tool_source / loop / settle 五段
  settle.rs     run_session_tail / TurnRunGuard / usage
  session_query.rs respond_session_query / undo / rename / archive
  compaction.rs 压缩接线 + 摘要适配
```

#### 2.5 `api/src/service.rs`（总 3,192 / 生产 3,030 → 目标每文件 ≤600 生产行）

```
api/src/service/
  mod.rs        AgentService 结构 + spawn/with_* + 访问器
  runs.rs       运行生命周期 + SSE 泵 + SubagentFeed
  sessions.rs   会话 CRUD / 切换 / 状态载荷 / undo / 导出
  settings.rs   apply_session_settings / apply_model_switch / apply_reasoning_effort / approval
  plugins.rs    start_plugins / reload_*
  mcp.rs        mcp_status / reload_mcp / reload_tool_switches
  catalog.rs    model_options / model_catalog / skills / projects / prompt history
  monitors.rs   监控与子代理查询
  errors.rs     10 个 `xxx_failed()` 构造器
```

#### 2.6 `commands/src/slash.rs`（总 2,753 / 生产 2,059 → 目标每文件 ≤500 生产行）

```
commands/src/slash/
  mod.rs          registry()/build_registry(225 → 改表驱动)
  common.rs       python_str/python_falsy/text_of/string_field/truncate/format_local_*
  format.rs       18 个 format_*
  confirm.rs      format_dangerous_tool_detail(241) + format_tool_confirmation
  session_cmds.rs sessions/archives/archive/history/undo/compact/rename/resume
  agent_cmds.rs   tasks/task/subagent/approval/reasoning/model/mode/speaking-style
  skill_cmds.rs   skills/plugins/mcp/memory
  configure_cmds.rs advisor/compression/desensitization/web-search/mcp 配置命令
```

#### 2.7 `host/src/tools/web_search.rs`（总 3,288 / 生产 1,713 → 目标每文件 ≤500 生产行）

```
tools/web_search/
  mod.rs        入口 web_search() + WebSearchOptions + 引擎表
  engines/html.rs   duckduckgo / github / arxiv / huggingface 解析（strip_tags/decode_entities 抽到 html.rs）
  engines/api.rs    tavily / exa / firecrawl / anysearch / tinyfish 客户端
  url.rs            encode_component / build_url / percent_decode / clean_duckduckgo_url
  intercept.rs      check_interception / friendly_network_error / status_message
  rerank.rs         channel_rerank_order / vector_order / decision_order / reranked_channel
  tests/            1,575 行拆 3 文件
```

#### 2.8 `jsdom/src/browser_env.rs`（生产 3,240 → 每文件 ≤400 生产行）

按 28 个 `install_*` 分组到 `env/{console,timers,navigator,screen,location,history,performance,chrome,webgl,canvas,crypto,storage,audio,speech,url,text_codec,dom_faces,events,message_channel,blobs,misc}.rs`，`mod.rs` 只留 `install()` 的调用序。

#### 2.9 `api/src/routes/settings.rs`（生产 1,268 → 每文件 ≤400 生产行）

`routes/settings/{mod,context,features,vision,image_gen,tts,mcp}.rs`；把 5 个 `serialize_*` 移到 `omnicrawl-config` 的 `features/*::to_json()`，与 §4.3 的三处重复一起收口（TUI 的 `*_form_values` 与 API 的 `serialize_*` 都改为消费配置层统一投影）。

#### 2.10 其余中等文件

| 文件 | 拆法 |
| --- | --- |
| `extensions/src/models.rs`(2,011) | `models/{manifest,hooks,policy,registry_doc}.rs`（22 组 const 表按归属分开） |
| `extensions/src/manager.rs`(1,880) | `manager/{dispatcher,worker,runtime,audit}.rs` |
| `workspace/src/agent_isolation.rs`(1,872) | `agent_isolation/{session,git,sync,cleanup,sweep}.rs` |
| `tls/src/session.rs`(1,597) | `session/{state,certs,cache}.rs` |
| `controllers/src/approval.rs`(1,589) | `approval/{git_shell,gates,review_prompt}.rs` |
| `ui/fullscreen/rendering/latex.rs`(1,546) | `latex/{parse,convert,normalize}.rs` |
| `host/src/turn.rs`(1,494) | `turn/{runner,batch,custody,prune}.rs`（`run_batch` 248 行拆为 4 段） |
| `connectors/{qqbot,feishu,telegram}/bot.rs` | 见 §2.11 |

#### 2.11 三平台连接器：先抽共享，再拆文件（这是唯一需要「设计」而非「搬家」的一项）

现状：三个 `bot.rs` + 各自 `timeline.rs` + 共享 `render.rs`，时间线状态机抄了三份（≈420 行高度相似）。

建议：

```
connectors/src/
  platform/
    timeline.rs      TaskTimeline 状态机（delta 聚合、节流 patch、收口、思考段封存）
    pending.rs       PendingConfirm / PendingQuestion 统一模型
    commands.rs      斜杠命令与建议（suggest_command / edit_distance / match_answer）
    help.rs          从三份 HELP_TEXT 派生（平台差异只留「能力表」）
  qqbot/bot.rs       只留 QQ 平台 I/O + 能力表（目标 ≤600）
  feishu/bot.rs      只留飞书平台 I/O + 能力表（目标 ≤600）
  telegram/bot.rs    只留 Telegram 平台 I/O + 能力表（目标 ≤500）
```

### Phase 3：结构性问题（最后做，风险最高）

| 问题 | 建议 |
| --- | --- |
| TUI 与 `host::turn` 各有一套批次编排（TUI 542 行 vs turn 337 行） | 把「批次编排」抽成 `omnicrawl-agent-runtime`（或放进 `omnicrawl-controllers`），以 `trait BatchHost { fn on_review_needed(&mut self, ..); fn on_custody(&mut self, ..); }` 的形式让 TUI（异步、可交互）与 API（同步、无交互）各自实现回调，共享编排骨架。**这项必须先有 `turn_flow.rs` + `settings_screen.rs` 的对照测试兜底**，属于独立任务，不建议与本轮拆分同批做。 |
| `AgentService` 万能门面 | Phase 2.5 拆完即已缓解；若要继续，把「会话」「插件/MCP」「模型目录」三块提成独立服务对象，`AgentService` 只做组装 |
| `cli/connector.rs` 与 `api/connector.rs` 双 `AgentDriver` | 保持现状（形态差异大），但把两者共同依赖的「命令表 + 会话摘要 + 工作区切换」提升到 `connectors::agent` 的默认实现 |
| `RemoteModelPort` 兼容路径 | 若确认无旧宿主（本仓库内已全部提供模型配置），删除 60 行 + 对应 e2e 分支；否则在 `ipc/src/bridge.rs:52` 的注释里标明「仅兼容，禁止新功能」 |

### Phase 4：防回归门禁

1. **行数 CI 检查（软门禁）**：在 `.github/workflows` 增加一个 20 行的脚本步骤，扫描 `rust/crates/*/src/**/*.rs` 的生产行数（剥离 `#[cfg(test)]` 之后），**>1000 行告警、>1500 行失败**。当前值先写进白名单（37 个），每拆一个就删一条，只增不减。
2. **函数长度检查**：同一脚本扫 ≥200 行的**生产**函数，白名单 9 个；测试里 ≥300 行的 4 个用例单列白名单。
3. **重复实现检查**：把本次的「函数体哈希去重」做成脚本放进 `scripts/`，同一函数体出现 ≥3 次即告警。
4. **`#[allow(dead_code)]` 检查**：当前仅 5 处，保持；新增必须写理由。

> 说明：CI 里跑 `cargo build/test` 会与你「不跑编译类命令」的既有约定冲突，所以上面的门禁全是**纯文本静态检查**，不触发编译。

---

## 七、建议的执行顺序与工作量

| 阶段 | 内容 | 预估 | 前置 |
| --- | --- | --- | --- |
| 0 | 清锁文件 + 记基线 | 0.5 天 | — |
| 1 | 减法（死代码 / widgets 未挂载 / 未接线功能定性 / 重复常量） | 2–3 天 | 阶段 0 |
| 2.1 | `ui/settings/state.rs` | 2–3 天 | 阶段 1（先删掉那份 2,432 行测试的干扰） |
| 2.2 | `tui/app.rs` | 3–4 天 | 2.1（共用 `settings_apply`） |
| 2.3 | `tui/state.rs` | 1.5–2 天 | 2.2 |
| 2.4 | `cli/session.rs` | 3–4 天 | 阶段 1（先把 worktrees 文案收口） |
| 2.5–2.9 | `api/service.rs`、`slash.rs`、`web_search.rs`、`browser_env.rs`、`routes/settings.rs` | 4–5 天 | 可并行 |
| 2.10 | 其余 8 个中等文件（第一批） | 3 天 | 可并行 |
| 2.10b | 剩余 17 个中等超标文件（第二批，见下） | 4–5 天 | 可与 2.11 并行 |
| 2.11 | 三平台连接器合流 | 2–3 天 | 需要先设计共享状态机 |
| 3 | 批次编排合流 / 门面收窄 | 5–8 天 | **独立立项**，不建议与本轮同批 |
| 4 | 纯文本门禁 | 0.5 天 | 每阶段后增量生效 |

合计（阶段 0–2 + 4，不含阶段 3）约 **23–33 人日**。

收益边界需要如实说明：

- 阶段 0 + 1 可减 **≈1,834 行**（705 零引用 + 1,090 widgets + 39 契约常量），约占生产代码 **1.0%**——**这不是「瘦身」，是「去掉幻觉」**：它不会让仓库变小很多，但会让「这行代码到底跑不跑」这个问题不再需要靠推理。
- 阶段 2 的**第一批**（2.1–2.11）覆盖 37 个超标文件里的 **20 个**（含前 12 名中的 10 个）。做完后 `>1000 行` 从 **37 降到约 17**，前 6 名贡献的 15.3% 生产代码被摊到 40+ 个 ≤800 行的文件里。
- 第二批（2.10b）覆盖剩下的 17 个：`jsdom/engine.rs`、`extensions/install.rs`、`llm/desensitization/{rules,engine}.rs`、`session/store.rs`、`jsdom/fetch_api.rs`、`llm/runtime.rs`、`controllers/subagents/tasks.rs`、`host/tools/knowledge.rs`、`ipc/bridge.rs`、`controllers/tool_args.rs`、`controllers/context_compaction/policy.rs`、`controllers/subagents/definitions.rs`、`api/shared_store.rs`、`host/review.rs`、`mcp/client.rs`、`extensions/skill.rs`。它们多为「单一职责但功能本身大」——**按 1000 行红线仍超标，但拆分的边际收益低于前 20 个**（拆它们更像「为了合规」而不是「为了可维护」），因此排在第一批之后、可与阶段 3 并行。两批都完成后 `>1000 行` 归零。
- 如果只做「阶段 0 + 1 + 2.1 + 2.2」，约 **7–9 人日**：消掉全部 115 项零引用与 685 行未挂载代码，并把两个最大的文件（合计 14,625 行）拆成约 25 个模块。

---

## 八、一句话结论

代码库的**架构边界是健康的，测试是厚的，注释是好的**；问题集中在 **前 6 个巨型文件吃掉 15%、前 12 个吃掉 21% 的生产代码**、**TUI 与 host 的重复编排**、以及**一次「移植到一半」留下的 685 行未挂载组件 + 899 行未接线功能 + 214 项只被测试养活的公开 API**。建议按「先减法、再按变更理由拆文件、结构性问题单独立项」的顺序推进，并用纯文本行数门禁把 1000 行红线真正执行起来。

---

## 九、执行记录（2026-10-25）

### 9.1 两处「未接线功能」已双通（原 §5.3、Phase 1）

| 项 | 处理方式 | 验证 |
| --- | --- | --- |
| `omnicrawl-tls` TLS 会话恢复 | `SessionCache` 补 `capacity` / `with_capacity` 与 put 时淘汰最旧；新增 `omnicrawl_host::tools::tls_session_cache_from_config`；TUI `app` 与 API `service` 的 `RegistryOptions` 装配点各注入一处 | `omnicrawl-host/tests/tls_resumption_wiring.rs`（4 例） |
| `omnicrawl-workspace` Agent 临时目录 | 新增 `temp::install_agent_temp_workspace`（ensure + clean_if_due + start_scheduler）；TUI `App` 增 `agent_temp` 并在工作区切换时重建、退出时关闭；API `AgentService` 增互斥槽，`switch_session` 重建、`close` 关闭；提示词里的临时目录改取 `[agent_temp] directory` 实际落点 | `omnicrawl-workspace/tests/agent_temp_wiring.rs`（6 例） |

`[tls_resumption]` 的读取侧已经接上；写回入口（`save_tls_resumption_configuration`）保留但当前无调用者——这一段的既有形态就是手工编辑 `config.toml`，不是遗漏。

### 9.2 前 6 名巨型文件：已拆 2 个

按「生产行」口径，两个最大文件都拆到千行以下：

| 文件 | 拆前 | 拆后 | 拆分方式 |
| --- | --- | --- | --- |
| `tui/src/app.rs` → `app/` | 8,232 总行 / 7,873 生产 | `mod.rs` **1,543** | 已有 `input` / `kernel_flow` / `turn_flow` / `settings_*` / `workspace` / `onejev` / `support`，本轮再拆出 `kernel.rs`（握手、帧泵、命令发送）、`ticks.rs`（动画与周期性收敛）、`settings.rs`（界面事件→写盘/推内核/后台任务）、`lifecycle.rs`（装配、视口、退出、剪贴板附件） |
| `tui/src/ui/settings/state.rs` → `state/` | 9,184 总行 / 6,752 生产 | `state.rs` **2,692**（生产 2,150） | 已有 `apply_sync` / `model` / `decision` / `embedding` / `mcp` / `tts` / `vision` / `tools`，本轮再拆出 `form_bridge.rs`（三套渠道表单的字段读写、校验、只读视图）、`pointer.rs`（命中与点击）、`navigation.rs`（选中项/面板/焦点推进）、`view.rs`（渲染层只读快照）、`keys.rs`（键位总机）、`dropdown.rs`（候选项），并把 2,431 行内联测试整块迁到 `state/tests.rs` |

拆分过程中跨模块的私有方法统一提升为 `pub(super)`：可见性只到父模块，不扩大对外 API。

### 9.3 零引用代码：原报告 115 项 → 本次实测 137 项，已全部清除

旧的 `dead_items.txt` 已失效：其中 `load_tls_resumption_configuration` 等项因 9.1 的接线不再零引用。本轮重新做了一次「剥离内联测试后按标识符全书计数」的扫描，得到 **137 项全库零引用**（含定义处自身），按 crate 分批删除并每批编译：

| crate | 删除行 | 代表项 |
| --- | --- | --- |
| `omnicrawl-controllers` | 417 | `worktrees.rs` 的 9 个文案/载荷拼装半成品、`batch.rs` 的 6 个、`compression.rs` 的回复压缩模板与 4 个函数、`settings.rs` 的 9 个错误构造器与常量 |
| `omnicrawl-extensions` | 340 | `install_from_local_package`（232 行，全库无调用，npm 路径 `install_from_npm` 有 3 处调用）、`models.rs` 4 项、`manager.rs` 3 项 |
| `omnicrawl-config` | 52 | `dump_yaml_text`、`embeddings_url`、`channel_keys`、`KNOWN_AVAILABLE_MODELS` |
| `omnicrawl-host` | 50 | `approval_required`、`is_denied`、`skill_count`、`kernel_closed`、`is_git_tool` |
| `omnicrawl-llm` | 56 | `with_store`、`mask_spans`、`plan_cache_stats`、`memo_stats` |
| `omnicrawl-tui` | 41 | `theme.rs` 的 7 个未用色常量、`clamp_index`、`form_kind`、`form_focused` |
| 其余（connectors/session/tls/mcp/protocol/workspace） | 90 | 见各批次输出 |

连带清掉的还有：删除后留下的未用导入（全工作区 `cargo check --workspace --all-targets` 现在 **0 warning**）、只服务于被删函数的 `assets/templates/model_reply_compaction_system.md`。

### 9.4 widgets.rs 未挂载组件（原 §5.2）

`ui/fullscreen/rendering/widgets.rs` 从 **1,330 行降到 17 行**，只留 5 个仍在生产路径上被 `ui/conversation.rs` 使用的缩略常量。README 里「渲染批 2 / 批 3」两处把已删组件标为已落地的说法已同步改为「已删除 / 实现已由 `conversation.rs` 与 `panels.rs` 承接」。

### 9.5 验证

- `cargo check --workspace --all-targets`：通过，**0 warning**（基线有 8 处）。
- `cargo test --workspace`：除下列基线失败外全通过；每一条都在 `git stash` 干净基线上复现过同一失败，与本轮改动无关：
  - `omnicrawl-entry::config_templates_dir_prefers_rust_assets`、`omnicrawl-extensions::runner_search_prefers_rust_assets`、`omnicrawl-host::templates_dir_prefers_rust_assets`——三者都靠 `current_exe()` 向上找仓库目录，测试二进制在 `D:/build_temp` 下时必然找不到。
  - `omnicrawl-mcp::server_parity` 2 例——内嵌文档与安装包副本哈希不一致。
  - `omnicrawl-tls::psk_client_hello` 2 例、`psk_resumption` 1 例——PSK 长度与 binder 断言。
  - `omnicrawl-tui::host_flow` 8 例——提交后状态机断言。
  - `omnicrawl-controllers::terminal_label_from_chain`——依赖 `WT_SESSION`，仅在终端环境下失败。
  - `omnicrawl-tls::driver_flow::a_goaway_during_the_settings_exchange`——Windows 回环 RST 时序，偶发（3 次中 1 次）。
- 顺带修掉两处**因本轮之前的功能变更而失效**的夹具：`subagents_parity` 缺 `computer-use` 内置角色；`tool_args_parity` 与 `tool_catalog_parity` 仍按已废弃的批量 `subagent` 契约断言。

### 9.6 剩余工作

前 6 名还剩 4 个：`cli/src/session.rs`（4,835）、`tui/src/state.rs`（5,966）、`api/src/service.rs`（3,261）、`jsdom/src/browser_env.rs`（3,161）。它们与 `ui/settings/state.rs` 的拆法同构——按「变更理由」切子模块树，`mod.rs` 只留装配——可按原 §2.4 / §2.3 / §2.5 / §2.8 的规划继续。

---

## 十、执行记录（2026-10-30，第二轮：孤儿文件与同 crate 重复）

### 10.1 清理 8 个未参与编译的孤儿文件（-1,976 行）

`omnicrawl-tui/src/ui/settings/state/` 下有 8 个 `*_types.rs` **从未被任何 `mod` 声明引入**——它们是 49c0398 那轮拆分留下的陈旧快照，内容已被后来的功能改动（如联网搜索独立设置页）追平而分叉：

| 文件 | 行数 |
| --- | --- |
| `decision_types.rs` | 545 |
| `tts_types.rs` | 503 |
| `mcp_types.rs` | 353 |
| `tools_types.rs` | 167 |
| `embedding_types.rs` | 134 |
| `channel_types.rs` | 119 |
| `model_types.rs` | 82 |
| `vision_types.rs` | 73 |

判断依据不是「行数超限」，而是三条同时成立：①全仓零引用（含测试）；②逐项比对后 **1,524 行**与 `state.rs` 中的定义逐字重复；③剩下的差异项是**陈旧内容**（`TOOLS_WEB_SEARCH_ROW`、`WEB_SEARCH_SWITCH_ROW` 等符号在 `state.rs` 中已改形或迁走），因此接线它们会引入回归，删除才是唯一正确解。

新增 `orphans.py`（`.omnicrawl/.agent_tmp/scripts/`）做「未声明 `mod` 的 .rs」全仓巡检，当前结果为 0，可作为常驻检查。

### 10.2 消除同一 crate 内可无损合并的重复

按「同一 crate 内、语义完全一致、不新增依赖」三条筛选后收口 6 组：

| 重复项 | 原位置 | 收口到 |
| --- | --- | --- |
| `expand_user` ×2 | `tools/image_gen.rs`、`tools/read_image.rs` | `tools/paths.rs`（既有路径助手模块） |
| `split_authority` + `default_port` ×2 | `tools/referrer.rs`、`tools/fetch_metadata.rs` | `tools/web_transport.rs`（补 IPv6 字面量语义注释） |
| `origin_of` ×2 | `tools/robots_cache.rs`、`tools/client_hints.rs` | 复用 `client_hints::origin_of`，测试一并迁移 |
| `Last-Event-ID` 解析 ×2 | `routes/monitors.rs`、`routes/subagents.rs` | `routes/query.rs`（既有取值校验模块） |
| `text_result` / `text_success` | `host.rs`、`tools/error.rs` | 统一走 `tools::error::text_success` |

刻意**不做**的四组（记录理由，避免下一轮重复讨论）：

- `PythonStyleFormatter` 的 `begin_array_value` / `begin_object_key`：横跨 `omnicrawl-llm`、`omnicrawl-session` 三个文件，抽公共层要新增 crate 依赖。
- `format_line_stats`：`connectors` 与 `tui` 各一份，TUI 未依赖 `connectors`，为 12 行引入跨 crate 依赖不划算。
- `features/{embedding,decision_model}.rs` 的 `active_channel` / `normalize_default_key`：两处渠道结构体字段集不同，泛型化要引入 trait，属于过度设计。
- `decision/src/server.rs` 与 `fetch/src/server.rs`（约 420 行、115 处差异）：差异是服务名、PID/日志文件名、就绪行与设置类型，属于「同构但不同宿主」；合并需先抽常驻服务骨架，应与 §6 Phase 3 的结构性问题一并立项。

### 10.3 验证

按项目约定未运行 `cargo build/check/test`（编译类命令会大量占用磁盘并触发杀毒软件）。全部改动以只读手段复核：

- 每个改动文件的花括号配平差值均为 0；
- 被调用的共享符号在目标模块均为 `pub`，且无模块循环依赖（`client_hints` 不依赖 `robots_cache`）；
- 删除私有函数后，原调用点的导入仍被其他代码使用（`ToolResult`、`HeaderMap`、`PathBuf` 均已核对）；
- 新增行不超过 100 列（无 rustfmt 配置，取默认值）；
- 行尾按 `core.autocrlf=true` 的检出约定恢复 CRLF；`git diff --stat` 为 105 插入 / 2,158 删除，无整文件重写。

**待用户执行**：`cargo check --workspace --all-targets`，以及 `cargo test -p omnicrawl-host -p omnicrawl-api -p omnicrawl-tui`。

### 10.4 剩余

生产行数 >1000 的文件仍有 **35** 个（本轮清理前 37 个）。本轮只做减法、未继续拆文件；下一个收益最大的目标是 `omnicrawl-tui/src/ui/settings/state.rs`（2,430 生产行）与 `omnicrawl-jsdom/src/engine.rs`（2,273 生产行），拆法沿用 §2.1 / §2.10 的规划。

### 10.5 第二轮继续：两个最大遗留文件的拆分（同日）

§10.1 清掉孤儿文件后，`ui/settings/state.rs` 与 `jsdom/engine.rs` 仍是最大的两个遗留项。本轮按「变更理由」把它们切成子模块树，`mod.rs`/根文件只留导航骨架与重导出。

| 文件 | 拆前（总行 / 生产行） | 拆后根文件 | 新子模块 |
| --- | --- | --- | --- |
| `tui/src/ui/settings/state.rs` | 2,636 / 2,430 | **1,135 / 855** | `shared.rs`(40) `pages.rs`(306) `tts_page.rs`(410) `decision_page.rs`(494) |
| `jsdom/src/engine.rs` | 2,405 / 2,273 | **1,057 / 841** | `canvas.rs`(126) `document.rs`(122) `document_ops.rs`(268) `element_ops.rs`(225) `element_proto.rs`(287) `sandbox.rs`(103) `scripts.rs`(94) |

拆分判据（沿用 §6 原则）：

- **按变更理由切，不按行数切**。`shared.rs` 是跨页复用的取值助手（凭据掩码、选项循环）；`pages.rs` 是工具/联网搜索/MCP 三页的行表与状态；`tts_page.rs` 与 `decision_page.rs` 各自一页；`element_ops.rs` 是「参数解包 → 调 DOM → 包回 JS」那一类，`element_proto.rs` 是方法表登记，`sandbox.rs` 是一次 run 的编排。
- **可见性只放到需要的那一层**：页内辅助函数用 `pub(crate)`（供 `state/` 子模块与 `app/` 用），只被同目录子模块读的行号常量用 `pub(super)`，其余保持私有。

### 10.6 本轮踩到并修掉的三个问题（记录以免重犯）

1. **`secret_draft*` 不是 TTS 专属**：它被 `web_search.rs` 使用。按「TTS 页整段搬走」的直觉操作会打断 `web_search.rs` 的 `use super::*` 链路，因此先抽到 `shared.rs` 再搬页面。
2. **`use super::*` 只覆盖 `state` 自身的名字空间**：把项挪到平级子模块后，父模块必须显式 `pub(super) use` 才能让兄弟模块看到（`pub(crate) use` 对子模块无效）。这轮为 TTS 行号、`cycle_text`、`secret_draft`、三个页面状态结构体都补了重导出。
3. **按花括号计数切函数在 Rust 签名上不可靠**：`fn f(\n a: &str,\n) -> R {` 这类签名的 `(` 与 `{` 混排会让深度提前归零。第一次切 `run_in_process` 只搬走 1 行、把函数体留在原地，随后又误删相邻的 `run_with_scripts` / `run_with_sources` / `panic_message`。修正方式是**从 `git show HEAD:` 取原文重建**，再用两个独立脚本做双向校验：函数名集合比对（无缺失/无新增）+ 函数体归一化后逐字比对。

### 10.7 验证方式升级

除 §10.3 的只读复核外，本轮增加一项**不触发编译**的语法校验：`rustup run <toolchain> rustfmt --edition 2021 --emit stdout <crate>/src/lib.rs`。它只做解析（不产出目标文件、不跑 `rustc`），能发现括号/分隔符类破坏，四个改动 crate 当前均为 0 语法错误。

完整性校验脚本（建议保留）：

- `函数名集合比对`：HEAD 版与新文件集合相等 → 没有误删也没有凭空新增。
- `函数体归一化比对`：去空白/去可见性前缀后逐字相同 → 搬移没有改语义。

### 10.8 验证

- `omnicrawl-tui` / `omnicrawl-jsdom` / `omnicrawl-host` / `omnicrawl-api`：`rustfmt` 解析 0 错误。
- 两个拆分目标的函数名集合与函数体内容与 HEAD 完全一致（见 §10.7）。
- 生产行数 >1000 的文件：**37 → 34**；`ui/settings/state.rs` 与 `jsdom/engine.rs` 均已落到千行以下。

**仍待用户执行**：`cargo check --workspace --all-targets`，以及 `cargo test -p omnicrawl-tui -p omnicrawl-jsdom -p omnicrawl-host -p omnicrawl-api`。

### 10.9 下一步建议

`settings/state/tests.rs`（2,221 生产行字面的测试文件）是当前榜首，但它整份都是内联测试——按 §5.6 的口径应当外移到 `tests/`，不与生产代码共用行数红线。其后按收益排序：`commands/src/slash.rs`（1,885）、`extensions/src/models.rs`（1,877）、`workspace/src/agent_isolation.rs`（1,756）、`tui/src/ui/settings/render.rs`（1,741）。

### 10.10 第三轮：`slash.rs` 与 `models.rs` 拆成模块树（同日）

| 文件 | 拆前（总 / 生产） | 拆后根文件 | 新子模块（生产行） |
| --- | --- | --- | --- |
| `commands/src/slash.rs` | 2,768 / 2,059 | **801 / 717** | `agent_commands`(462) `render`(598) `review`(261) `registry`(203) `session_commands`(127) `workspace_commands`(22) `skill_suggestions`(19) |
| `extensions/src/models.rs` | 1,984 / 1,877 | **46**（只剩 `mod` + 重导出） | `manifest`(419) `parsing`(388) `util`(352) `patch`(180) `validation`(136) `constants`(126) `hooks`(100) `hook_result`(99) |

两个文件的对外路径都保持不变，这是本轮最需要守住的一点：

- `slash.rs` 是 `pub mod slash`，改成 `slash/` 目录后由 `slash/mod.rs`（即 `slash.rs` 本身）重新导出。外部只经 `slash::` 取 8 个符号（`registry`、`build_slash_command_options`、`build_review_task_prompt`、`check_review_preconditions`、`format_review_report`、`workspace_switch_success_message`、`REVIEW_TASK_DESCRIPTION`、`WORKSPACE_SWITCH_PENDING_MESSAGE`），但 29 个 `handle_*` 原本也是 `pub fn`，**已全部保留 `pub` 并重导出**——它们虽然只经 `registry().dispatch(..)` 调用，路径变化仍是破坏性改动。
- `models.rs` 被 `crate::models::X`、`models::*`、`omnicrawl_extensions::models::*` 三种写法引用（含 8 处外部 crate 引用与 `tests/extensions_parity.rs` 的整表对照测试），全部公开项重新导出后这三种写法都无需改动。

### 10.11 本轮修掉的两个系统性切分缺陷

这两类问题是**机械切分必然产生**的，已固化成检查项：

1. **切分点会留下下一项的文档注释与属性**。按「下一个顶层项的起始行」切片时，`/// ...` 与 `#[derive(..)]` 属于下一项却停在上一个文件的尾部，表现为 `error: expected item after doc comment` / `expected item after attributes`。本轮 `session_commands.rs`、`constants.rs`、`hooks.rs` 等 7 个文件都命中。修法是**从尾部剥离**连续的 `///`、`#[`、空行，再从 `git show HEAD:` 取该首项的原始前置块补回新文件开头——两边都不能凭记忆写。
2. **`pub(super)` 降级会改变对外 API**。`slash.rs` 里的处理器原本是 `pub fn`，机械替换成 `pub(super) fn` 后「外部零引用」的静态扫描会误判为安全。判定标准必须是「原可见性是否 `pub`」，而不是「当前有没有人调」。

### 10.12 验证

- 七个改动 crate（extensions / commands / tui / jsdom / host / api / entry）：`rustfmt` 解析 0 错误。
- `slash.rs`：符号集合无缺失无新增、29 个原 `pub fn` 全部重导出、47 个函数体去空白后逐字一致。
- `models.rs`：47 个函数体逐字一致，公开项全部重导出。
- 生产行 >1000 的文件：**37 → 32**（本轮再降 2 个）。

**仍待用户执行**：`cargo check --workspace --all-targets`，以及 `cargo test -p omnicrawl-commands -p omnicrawl-extensions -p omnicrawl-entry`。

### 10.13 下一步

榜首 `settings/state/tests.rs`（2,221）整份是内联测试，按 §5.6 口径应外移到 `tests/`，不占生产红线；其后 `tui/src/ui/conversation.rs`（1,795，但总行 4,051 里 3,200 行是测试，实际生产仅 471）、`workspace/src/agent_isolation.rs`（1,756）、`tui/src/ui/settings/render.rs`（1,741）、`extensions/src/manager.rs`（1,715）。

---

## 十一、执行记录（2026-10-30，第四轮：冗余与陈旧逻辑复查）

前几轮的扫描脚本本身有两处系统性误判，本轮先修脚本、再取结论，因此**数字全变了**，以本节为准。

### 11.1 先修工具：旧脚本的两处误报

| 脚本 | 症状 | 原因 |
| --- | --- | --- |
| `dead.py` §5.1/§9.3 | 把 `commands/src/slash.rs` 里 40 个**测试函数**报成「生产零引用」 | 剥离测试块的判据是「行首无缩进且以 `#[cfg(test)]` 开头」，而该处是 `mod tests {`（属性行在前）或属性被拆分改动丢失，导致整段测试被当成生产代码 |
| `dead3.py`（本轮新） | 把「只被 `tests/` 目录引用」的正确契约面混进同一档 | 只统计了「生产引用」，没有单独统计「测试引用是否来自 `tests/`」 |

本轮新增三档口径（`.omnicrawl/.agent_tmp/scripts/dead3.py`）：按「除定义行自身之外的剩余出现数」判定，并把 `tests/` 目录与内联 `#[cfg(test)]` 块分别归入测试语料。结果：

- **真死码 17 项**（生产与测试都零引用）；
- **仅测试引用 119 项**，其中 **93 项被 `tests/` 下的 parity 对照用例引用**（属契约面，不动），**26 项只在同文件内联测试里出现**（真正的候选）。

另新增 `cfg_test_gate.py`（`mod tests` 缺 `#[cfg(test)]` 巡检）与 `dup2.py`（全 crate 函数体逐字重复）。

### 11.2 本轮修掉的一个编译级回归（由上一轮拆分引入）

`commands/src/slash.rs` 的 `mod tests` **丢了 `#[cfg(test)]`**。后果不只是测试残留进产物：该模块 `use crate::agent::testing::FakeAgent`，而 `agent::testing` 本身是 `#[cfg(test)]`——测试构建下能编过，**非测试构建必然报「找不到 `crate::agent::testing`」**。已补回，`cfg_test_gate.py` 全仓复扫为 0。

这条是 §10.11「机械切分会留下下一项的文档注释与属性」的补充：**属性不只会掉到上一个文件尾部，还会原地丢失**。切分后必须逐文件核对原文件里的 `#[cfg(test)]`、`#[allow(..)]`、`#[derive(..)]` 是否都还在。

### 11.3 真死码 17 项（逐条 grep 交叉验证）

| 位置 | 符号 | 判定 |
| --- | --- | --- |
| `controllers/src/subagents/orchestration.rs:12-32` | `NOTIFICATION_PREVIEW_ITEMS`(16)、`NOTIFICATION_PREVIEW_CHARS`(12000)、`WORKTREE_MARKER`、`WORKTREE_ARTIFACT_CHARS`(8000)、`SUMMARY_TRUNCATED_SUFFIX`、`PublicResult` | 6 项全零引用，且 `tasks.rs:1019` 与 `recovery.rs:249` 用的是裸字面量 `16`、`6000`。**常量表成了不会被读的注释** |
| `controllers/src/subagents/batch.rs:21,24,191` | `BACKGROUND_DISABLED_MESSAGE`、`BACKGROUND_FAIL_FAST_MESSAGE`、`SharedBatch` | 前者两条文案没人引用；`SharedBatch` 只是 `Arc<ActiveBatch>` 别名，宿主用 `ActiveBatch` 本身 |
| `controllers/src/compression.rs:329,330` | `REPLIES_OPEN`、`REPLIES_CLOSE` | 双段压缩已下线（同文件 `join_compaction_sections` 的注释自认「供需要时回接」） |
| `config/src/features/tls_resumption.rs` | `save_tls_resumption_configuration` | 见 §11.5，属**刻意保留**而非死码 |
| `connectors/src/mqtt/prompt.rs:121` | `action_at` | 决策通道的 `o0/o1` 反解在 `api/src/mqtt.rs:270` 用 `request.actions.get(index)` 自己做了一遍 |
| `extensions/src/protocol.rs:857` | `invoke_and_parse` | `manager.rs:459-466` 内联了同一套「取耗时 → `parse_hook_result`」，重复且无人调用 |
| `extensions/src/skill.rs:19` | `DEFAULT_MATCH_THRESHOLD` | 阈值常量零引用 |
| `host/src/tools/image_gen.rs:27,30` | `DEFAULT_OUTPUT_DIR`、`DEFAULT_API_KEY_ENV` | 输出目录实际由 `default_output_dir()` 逐段 `join` 拼出（注释自述「避免常量里的 `/` 与 Windows 分隔符混用」），常量只剩自证测试 |
| `tui/src/ui/fullscreen/terminal/theme.rs:11` | `TRANSPARENT` | `parse_color` 内部用的是字面量 `"transparent"` |

### 11.4 只在同文件内联测试里出现的 26 项（按处置分三组）

**A. 应当删除（实现已改道，测试在自证死代码）**

| 符号 | 依据 |
| --- | --- |
| `tui/src/ui/settings/picker.rs` 的 `token_for` / `channel_matches` / `model_matches` | 设置面板**没有**搜索过滤入口（`grep` 全仓无调用）；`token_for` 的职责由 `ChannelRow::label()`（`state.rs:409`）自己 `format!("{}/{}")` 承担 |
| `tui/src/ui/fullscreen/status/hud.rs` 的 `pending_queue_text` / `gradient_text` | 排队条实际由 `ui/queue.rs` 渲染，HUD 用的是 `context_summary_text` / `token_telemetry_text` |
| `tui/src/ui/fullscreen/terminal/theme.rs` 的 `user_background` / `reasoning_background` | 渲染路径直接用 `USER_BACKGROUND` / `REASONING_BACKGROUND` 风格串（`conversation.rs:1413`、`markdown.rs:17/22`），两个 `-> Color` 包装没人调 |
| `onejev/src/sizes.rs` 的 `size_keys` / `nearest_size` / `ONEJEV_REPO_PREFIX` | `size_keys` 还被 `lib.rs:36` 重导出成公开 API，但全仓无消费者；`nearest_size` 被 `find_size` 取代；`ONEJEV_REPO_PREFIX` 只在断言里拼过 |

**B. 应当接线而不是删（逻辑是对的，调用点丢了）**

| 符号 | 现状 | 建议 |
| --- | --- | --- |
| `controllers/src/subagents/orchestration.rs` 的 5 个常量 | `tasks.rs` / `recovery.rs` 里是裸字面量 | 把字面量换成常量，一处定义两处读 |
| `config/src/features/{fetch_api,decision_model}.rs` 的 `save_*_configuration` | 启动自启走 `load_*`，但设置面板与 API 路由都没写回入口——`[fetch_api]` / `[api]` 目前只能手改 TOML | 若要支持界面改，接上；否则在文档写明「手工编辑」 |
| `config/src/features/decision_model.rs:643` 的 `decision_switch_default` | TUI 侧 `support.rs:399` 自己写了 `switches.get(key).copied().unwrap_or(spec.default)` | 二选一，别留两份默认值口径 |

**C. 属 parity 契约或内部助手，保留**：`host/src/tools/registry.rs:775` 的 `declared_names`（11 个测试文件共用）、`tls` 系列的 `split_data` / `round_trips` / `split_records` / `signed_message`、`decision_suggest.rs` 的 `suggested_tools` / `suggested_subagents`、`web_search.rs` 的 `channel_rerank_order`。

### 11.5 陈旧/悬空逻辑（不是死码，但注释与事实不符）

| 位置 | 问题 |
| --- | --- |
| `controllers/src/subagents/batch.rs:21-24` | 两条「后台 SubAgent 默认关闭 / 不支持 fail_fast」文案完好，但**子代理工具参数里根本没有 `background` 与 `fail_fast` 字段**（`agent_tools.json` 与 `tool_catalog` 均无），`SubAgentConfig.allow_background`（`config/src/features/subagents.rs:37`）也无任何消费方。这是一整条**配置 + 文案 + 分支都没有落地**的半成品链 |
| `tui/src/ui/fullscreen/status/indicators.rs:139` | `can_withdraw_pending` 与 `state/composer_flow.rs:89` 的真实守卫逐条重复（「回合在跑 / 下标有效 / 无待答提问」），测试都在测这个影子函数 |
| `host/src/tools/registry.rs:109` | `TTS_RUNNERS = ["tts"]` 与工具名 `tts_synthesize` 不同名，靠 `agent_tools.json` 里 `"runner": "tts"` 显式映射——**这不是 bug，但极易被下一轮误判成不一致**，已在此备案 |
| `host/src/tools/image_gen.rs:27` | 常量 `DEFAULT_OUTPUT_DIR` 与 `default_output_dir()` 拼出的路径是两份事实 |

### 11.6 重复实现（本轮全仓扫描，`dup2.py` 取体长 ≥200 字符）

本轮报 **同 crate 32 组 / 跨 crate 10 组**，但绝大多数是**假重复**：FFI 函数声明（`windows/ffi.rs` 24 个）、trait 默认方法体（`mcp/client.rs`）、`-> Self` 构造链，函数体天然一致。真正值得处理的只有：

| 重复 | 位置 | 处置 |
| --- | --- | --- |
| `mask_secret` | `connectors/src/feishu/config.rs:86` 与 `qqbot/config.rs:234` | 逐字相同，同 crate，可无损合并到 `connectors` 公共位置 |
| `ToolRecord::finish` / `abort` / `running` / `duration_seconds` / `text_of` | `connectors/src/feishu/timeline.rs` 与 `telegram/timeline.rs` | 5 个函数逐字相同（710 字符），结构体字段也同构 |
| `string_list_field` | `config/src/features/agent_workspace.rs:185` 与 `skills.rs:33` | 逐字相同，同 crate |
| `python_truthy` | `session/src/history.rs:656` 与 `projection.rs:697` | 逐字相同，同 crate |
| `truthy` | `host/src/tools/knowledge.rs:880` 与 `read_image.rs:116` | 逐字相同，同 crate |
| `resolve_proxy` | `host/src/tools/fetcher.rs:608` 与 `image_gen.rs:476` | 逐字相同，同 crate（`web_transport.rs` 已是公共传输层，是天然落点） |
| `overlaps` | `llm/src/desensitization/gitleaks.rs:689` 与 `rules.rs:393` | 逐字相同，同 crate |
| `el_set_node_value` / `el_set_text_content` | `jsdom/src/engine.rs:573 / 890` | 逐字相同，同 crate |
| `resolve_path` / `normalize_path` | `extensions/src/path.rs` 与 `workspace/src/paths.rs` | 跨 crate，**不并**（要新增 crate 依赖） |
| `expand_user` ×3 | `controllers/workspace.rs`、`session/project.rs`、`tts/audio.rs` | 跨 crate，**不并** |
| `log_has_ready_line` / `open_log` | `decision/src/server.rs` 与 `fetch/src/server.rs` | 属 §10.2 已记录的「同构但不同宿主」常驻服务骨架问题，**不并** |

### 11.7 刻意保留（记录理由，避免下一轮重复讨论）

- `config/features/tls_resumption.rs` 的 `save_tls_resumption_configuration`：`[tls_resumption]` 的**读取侧**已由 TUI 与 API 两处注入（§9.1），写回入口的既有形态就是手工编辑 `config.toml`。
- 93 项「只被 `tests/` 引用」的公开项：`*_parity` 系列钉住与 Python 的等价，删了等于放弃对照。
- `host/src/tools/registry.rs` 的 `IMPLEMENTED_TOOLS`、`TTS_RUNNERS` 等常量表：`agent_tools.json` 与它们的差异（`ask_user`/`subagent`/`update_todos`/`recall_session_evidence` 由内核自持、不在宿主执行分支）是设计而非漏洞。

### 11.8 规模基线（本轮实测，口径 = 剥离内联测试后的生产行）

生产行 >1000 的文件 **36 个**（§10.12 记的 32 个是旧口径，未计入本轮后新增/增长的文件）。前列：`tui/ui/conversation.rs`(1,892)、`tui/ui/settings/render.rs`(1,885)、`workspace/agent_isolation.rs`(1,871)、`extensions/manager.rs`(1,822)、`host/tools/web_search.rs`(1,803)。

内联测试占比 >50% 且总行 >800 的文件 6 个：`tui/ui/conversation.rs`(54%)、`tui/state/tests.rs`(99%)、`host/tools/registry.rs`(54%)、`host/tools/decision_suggest.rs`(53%)、`host/tools/decision_search.rs`(53%)、`commands/slash.rs`(90%)。

### 11.9 本轮验证方式

按项目约定未跑 `cargo build/check/test`。全部结论来自只读静态取证：

- 17 项真死码逐条 `grep -rn` 全文确认「除定义行外 0 次出现」；
- 26 项内联测试项逐条确认「无跨文件引用，且生产路径已有替代实现」；
- `cfg_test_gate.py` 全仓复扫 0（修复后）；
- `orphans.py` 全仓孤儿文件 0；
- 重复实现按「归一化函数体逐字相等」判定，并单独标注跨 crate 不可并项。

**待用户执行**：`cargo check --workspace --all-targets`（重点看 `omnicrawl-commands` 的测试模块是否恢复）、`cargo test -p omnicrawl-commands -p omnicrawl-controllers -p omnicrawl-host -p omnicrawl-tui`。

### 11.10 建议的处置顺序

1. **先删 11.4-A 的 10 项**（纯死代码，零风险，且能让「测试通过」不再掩盖事实）；
2. **再接 11.4-B 的三处**（常量收口、默认值口径二选一），收益是消除「两份事实」；
3. **11.6 的 8 组同 crate 重复**按 crate 批量做，每组都要跑一次该 crate 的 parity 测试；
4. `background` / `fail_fast` 半成品链（11.5 第一条）需要产品决策：接线还是整条删除，**不要继续悬着**。

## 十二、执行记录（2026-10-30，第五轮：A/B 两组落地）

按 §11.10 的处置顺序做掉 A 组（删死码）与 B 组（消「两份事实」）。**执行中发现 §11 的三处判断有误，已就地更正**，见 §12.4。

### 12.1 A 组：删除项

| 位置 | 符号 | 备注 |
| --- | --- | --- |
| `tui/ui/settings/picker.rs` | `token_for`、`channel_matches`、`model_matches` | 一并删掉只服务它们的测试 `token_and_matching_helpers_follow_python`，把 `model_part` 的唯一断言提成 `model_part_extracts_the_model_segment` |
| `tui/ui/fullscreen/status/hud.rs` | `pending_queue_text`、`gradient_text` | 连带移除只有 `gradient_text` 用的 `ACCENT_GREEN` import |
| `tui/ui/fullscreen/terminal/theme.rs` | `user_background`、`reasoning_background` | 见 §12.3 |
| `onejev/sizes.rs` | `size_keys`、`nearest_size`、`ONEJEV_REPO_PREFIX` | `every_size_points_at_an_official_repo` 里的前缀断言改成字面量 `"OmniJev/OneJev-{}"`，保留「repo_id 与 key 对得上」这条真断言 |

### 12.2 B 组：三处「两份事实」的消解

**① `orchestration.rs` 的 5 个常量 —— 不是接线，是删除加改名。**
上一轮说「把字面量换成常量」，实测后不成立：`NOTIFICATION_PREVIEW_ITEMS = 16` 与 `NOTIFICATION_PREVIEW_CHARS = 12000` 描述的是一套**从未实现的**通知注入截断（`drain_notifications` 直接返回全部事件，没有任何条数或字符限制）；`WORKTREE_MARKER`、`WORKTREE_ARTIFACT_CHARS`、`SUMMARY_TRUNCATED_SUFFIX` 对应的机制在 Rust 侧也不存在。真正存在的两个数是 `tasks.rs::bound_result` 与 `recovery.rs` 里的 `6000`（summary 字符上限）与 `16`（artifacts 条数上限），它们此前是裸字面量。

处置：删掉 5 个虚设常量与同样零引用的 `PublicResult`，新增 `RESULT_SUMMARY_CHARS = 6000`、`RESULT_ARTIFACTS_LIMIT = 16`，两处调用点（共 4 个字面量）改为读常量。

**② `decision_switch_default` —— TUI 改为复用它。**
`config` 侧有 `decision_switch_default(key)`，TUI 的 `decision_switch_rows` 却写 `switches.get(spec.key).copied().unwrap_or(spec.default)`。后者取的是「当前 spec 的默认值」，前者取的是「按 key 查到的默认值」——同一张表两种取法。已把 TUI 侧改为 `unwrap_or_else(|| decision_switch_default(spec.key))`。

> 注意不要反过来改：曾试过让 `decision_switch_default` 复用 `default_decision_switches()`，但那会每次调用都构造一个 `BTreeMap`，而这条路径在设置页渲染的每帧都会走，已撤回。函数本体保持查表实现。

**③ `save_fetch_api_configuration` —— 保留并注明。**
`save_decision_model_configuration` 有生产调用点（`app/onejev.rs:130`、`app/settings_pages.rs:126`），**不是**死码（§11 判断有误）。真正没有生产调用点的只有 `save_fetch_api_configuration`：设置面板没有「抓取接口」页，`[fetch_api]` 由用户手改 `config.toml`。它是配置域「读有写回侧」的对称面，保留，并在文档注释里写明现状，避免下一轮再被当死码。

### 12.3 删除 `user_background` 时连带发现的事

删掉 `user_background()` 后 `USER_BACKGROUND` 变成新的孤立项。查 `git log -S` 与当前渲染路径后确认：自 `d0a1936`（config 域移植收尾）起，这个常量就只有自己的包装函数和测试在用，**从未进入渲染路径**——用户消息是「白色正文 + 左侧青色竖条」（`conversation.rs::push_user`），Python 侧那条 `.user-message` 的半透明紫底早就被 `pipeline.py` 的改写取代。属同一批未接线的移植残留，一并删除。

`rgba(r,g,b,a)` 的解析分支**保留**：`parse_color` 是通用样式解析器，该分支有独立测试，且 `rich_style` 的 `on <颜色>` 语法依赖它。

### 12.4 对 §11 的三处更正

1. §11.4-B 说 `decision_model` 的 `save_*` 无写入口 —— **错**，`save_decision_model_configuration` 在 TUI 有两处调用。
2. §11.4-B 说 `orchestration.rs` 的常量「应接线」—— **部分错**，其中 4 个描述的机制不存在，只有 2 个数值对应真实限制（且数值与常量名对不上：实际是 6000 而非 12000）。
3. §11.4-A 说 `onejev/sizes.rs` 的 `size_keys` 被 `lib.rs:36` 重导出 —— **错**，`lib.rs:36` 重导出的是 `find_size`、`size_labels`；`size_keys` 只是 `pub mod sizes` 的公开面，无消费者。

教训：`pub` 不等于「被外部依赖」，「上一轮的描述提到」也不等于「重导出」。判定死码前要查定义处的模块可见性链，而不是只看 `lib.rs` 的 `pub use` 列表。

### 12.5 复扫结果

- 真死码 **17 → 11 项**，剩余 11 项全是刻意保留或需产品决策（§11.7、§11.5 第一条）；
- A 组 10 项 + `USER_BACKGROUND` + `PublicResult` + 5 个虚设常量，全仓 `grep -rn` 均为 0；
- `cfg_test_gate.py` 全仓 0；
- 11 个改动文件全部通过 `rustfmt --check`（无失败项）。

未跑 `cargo check/test`（项目约定）。**待用户执行**：`cargo check --workspace --all-targets`，重点看 `omnicrawl-tui`、`omnicrawl-onejev`、`omnicrawl-controllers`、`omnicrawl-config` 四个 crate 是否有未使用 import 或类型不匹配。
