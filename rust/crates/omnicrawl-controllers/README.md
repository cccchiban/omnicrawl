# omnicrawl-controllers

Agent 控制器域（Python 侧 `omnicrawl/agent/controllers/`）的 Rust 移植。

Python 把这些逻辑写成挂在宿主 Agent 上的 Mixin：判定、校验、文案与预算计算和副作用
（git 子进程、线程池、文件系统、UI 回调、其它子系统对象）混在同一个方法里。本 crate
只收前者；后者一律留给宿主，经参数或 trait 注入——最典型的是 [`undo::SnapshotStore`]，
git 快照能力由宿主实现，crate 内不起子进程。

## 目录

| Python | Rust | 内容 |
| --- | --- | --- |
| `controllers/shared.py` | `src/shared.rs` | 常量表、整数配置校验、未知工具/超时结果文案、哈希函数名反查 |
| `controllers/undo.py` | `src/undo.rs` | undo 安全性判定、副作用账本、快照事件与恢复预检、快照路径防穿越 |
| `controllers/workspace/switching.py`、`toolbox.py` | `src/workspace.rs` | 切换目标校验与拒绝文案、内部目录保护提示、截图目录 |
| `controllers/memory/stores.py` | `src/memory.rs` | 三类作用域记忆目录解析、会话级记忆清理 |
| `controllers/session/settings.py`、`config/features/{approval,tools,subagents}.py` | `src/settings.rs` | 审批模式与推理强度归一化、压缩阈值换算、工具开关名与禁用集合、SubAgent 资源参数校验、`set_model` 选择串 |
| `controllers/session/control.py` | `src/control.rs` | 插件子系统状态文案、退出收尾动作、关闭/停用前的排空决策、关闭阶段顺序与关闭回调处置、隔离收尾摘要、父 Session 切换排空 |
| `controllers/tools/approval.py`、`agent/toolkit/approval_policy.py` | `src/approval.rs` | 审批归属判定、shell 命令分流、git 风险分级、删除意图、审查结论解析与失败文案、审批/执行阶段顺序、落盘事件载荷、生效模式与展示文本 |
| `agent/context_compaction/*.py` | `src/context_compaction/` | 上下文 Token 估算、回合预算测量、压缩批次与超限恢复批次、自动压缩决策、结构化摘要校验、投影、测量账本、摘要授权的事件证据恢复、结构化摘要生成与压缩编排 |
| `controllers/advisor.py` | `src/advisor.rs` | 顾问可用性判定、消息分支（剥孤儿调用 + user 尾）、工具清单、结果信封与错误文案 |
| `controllers/plugins.py` | `src/plugins.rs` | Hook fail-closed 判定、拒绝事实与文案、分发结局归一化、会话生命周期 Hook 名与标识、分发上下文冻结归一化、回合 Hook 处置 |
| `controllers/turn/loop.py`（接线面） | `src/turn/turn_loop.rs` | 13 回调面与缺省回落（重试提示回落状态回调）、`request_reply` / `execute_tool_batch` 两个循环端口与守门回调、进入循环前的取消检查、最终回复的收尾补发、用量累计与失败分类、运行时视觉能力与原生视觉开关 |
| `controllers/turn/loop.py`（文本规则） | `src/turn/turn_text.rs` | 短「继续/重试」识别与上一轮任务还原、被取消回合的历史摘要、助手消息的推理字段 |
| `agent/context/prompt_context.py`、`controllers/turn/loop.py`（上下文消息） | `src/turn/context_messages.rs` | 项目规范消息外壳（来源 + 权限边界）、插件 `context.build.before` 附加上下文的注入规则与收尾载荷 |
| `controllers/turn/loop.py`（工具调用事件） | `src/turn/tool_events.rs` | 原始 assistant 消息里取 `arguments_json`/`function_name`（按 ID 或函数名匹配首个命中）、协议原文不落盘的字段边界 |
| `controllers/turn/loop.py`（技能命令） | `src/turn/skill_command.rs` | `/skill:<名> [任务]` 的解析与加载文案（未命中带可用列表、命中无任务给默认提示） |
| `controllers/subagents/worktrees.py`、`orchestration.py`（判定面） | `src/subagents/` | worktree 登记键与查找归一化、会话去重投影、产物摘要渲染、丢弃保护判定、失败描述、Fork 上下文冻结、公开结果投影、后台通知注入、结果校验与定义缺失文案 |
| `agent/subagents/worktree.py`（判定面） | `src/subagents/worktrees.rs` | 分支名片段清洗（非安全字符折叠 + 48 字符截断）、主树脏时的门禁文案（8 行预览）、worktree 变更统计（目录不存在即无变更、`rev-list` 失败保守记 1） |
| `agent/subagents/read_only_commands.py`（判定构件） | `src/subagents/read_only.rs` | 可执行名归一化、命令链分段（重定向/空片段/未闭合引号）、`shlex.split` 子集与逐 token 段判定（白名单 + git/curl/sed/find/浏览器/PowerShell Web 请求专项）、策略入口（monitor 白名单、命令文本与 diagnostic 复核） |
| `agent/toolkit/tools.py`（目录与注册） | `src/tool_catalog.rs` + `data/agent_tools.json` | 目录数据由 `rust/tools/gen_agent_tools_data.py` 导出；注册规则（可选 runner 省略、知识库/Windows 整组、记忆开关、SubAgent 角色枚举、禁用过滤）在 Rust 重放 |
| `agent/toolkit/tools.py`、`host_tools.py` | `src/tool_args.rs` | 工具名/参数名归一化、参数投影（Session/确认页/SSE）、紧凑 Schema、Schema 校验与结果信封 |
| Python `json.dumps` 子集 | `src/json.rs` | Python 风格 JSON 文本与 `repr`（工具结果信封、审查指令、参数摘要共用） |
| `controllers/tools/output.py` | `src/output.rs` | 批次输出预算与落盘预览、工具结果消息、视觉旁路文案 |
| `controllers/tools/compression.py`、`runtime/tool_output_compressor.py`（纯逻辑） | `src/compression.rs` | 压缩选取、展示文本、参数摘要、压缩请求消息、超长输出采样、回包清洗与截断、取消判定、思考深度、内置系统提示 |
| `controllers/tools/building.py` | `src/building.rs` | 模式模板装载与 system prompt 组装 |
| `controllers/tools/implementations.py`（判定面） | `src/tool_impl.rs` | `update_todos` 的清单投影与输出信封、`ask_user` 入参校验与回答信封、记忆工具 `scope` 解析（实现本体是 I/O，仍在宿主） |
| `agent/runtime/llm_protocol.py`（一函数） | `src/shared.rs` | `resolve_tool_name_from_hashed_function_name` |
| `agent/types.py` | `src/types.rs` | `ToolCall` / `ToolResult` / `ToolImageAttachment` |
| `state/session_artifacts.py`、`state/session_projection.py`（各一函数） | `src/output.rs` | 复用 `omnicrawl-session` 的 `preview_text` / `tool_result_message`（不再重复实现） |
| `controllers/__init__.py` 的错误面 | `src/error.rs` | `AgentError`（只承载文案） |
| `controllers/session/store.py`（事件投影编排） | `src/store.rs` | 落盘事件用未脱敏原始 payload 投影、未落盘事件的等价内存事件构造（序号推进与 ID 形状）、投影方式选择 |

## 尚未搬的部分（宿主粘合层）

以下内容依赖宿主对象或多子系统协作，留在 Python 侧；后续批次按依赖顺序收口：

- `tools/approval.py` 的编排段：插件钩子（`tool.call.before` / `tool.approval.before` /
  `tool.execute.before` 等）、审查模型的 Responses 请求、用户确认面板、会话事件持久化。
  判定件（`agent/toolkit/approval_policy.py` 的全部规则）与编排契约（阶段顺序、事件载荷、
  生效模式、展示文本回落）已在 `src/approval.rs`；剩下的是真实的钩子调用、模型请求与落盘。
- `tools/building.py` 的 `_build_tools` / `_build_mcp_tools`：工具表构建，依赖
  `agent/toolkit/tools.py`。
- `tools/implementations.py`：工具实现本体（属 `agent/toolkit/`）。判定面（清单投影、`ask_user` 入参、
  记忆 `scope`）已在 `src/tool_impl.rs`；文件系统 / HTTP / 子进程 / MCP / TTS 的调用仍在 Python。
- `toolkit/tools.py` 的**执行函数绑定**：目录与注册规则已搬（`src/tool_catalog.rs` + 导出的
  `data/agent_tools.json`），但各工具的实现本体（`agent/toolkit/*`、`workspace/`、`mcp/`…）仍在 Python。
- `turn/loop.py` 的编排壳：插件钩子、会话事件与历史落盘、回合快照、压缩触发、
  run_guard 续跑与上下文超限恢复；接线面（13 回调面、两个循环端口与守卫、收尾补发、
  失败分类）已搬（`src/turn/turn_loop.rs`）。
- `turn/compaction.py` 的**会话与模型编排**已搬到 `omnicrawl-compaction`（`driver.rs` 落事件、
  归档、写记忆、自动召回、重建历史；`adapter.rs` 走内核运行时发摘要请求），判定面留在
  `src/turn/compaction.rs`；仍未接线的是内核进程侧（会话归属与回合结束后触发）。
- `subagents/{orchestration,worktrees}.py` 的进程面：Coordinator/TaskManager 生命周期、
  模型运行时引导（`_create_subagent_runtime_manager` / `_run_subagent_task_loop` /
  `_prepare_subagent_execution`）、worktree 的 git 创建/应用/清理、`_refresh_subagent_definitions`
  与事件观察者转发；判定与投影已在 `src/subagents/`。
- `advisor.py` 的模型面：`apply_model_selection` + 独立 Runtime 引导 + 协议单轮补全 +
  用量回调；判定、消息分支与信封已在 `src/advisor.rs`。
- `plugins.py` 的进程面：Plugin Runtime / Worker 生命周期、`HOOK_POLICIES` 表本身与
  配置读写；分发后的判定、文案与运行期处置决策（冻结上下文、回合 Hook、会话 Hook）已在 `src/plugins.rs`。
- `session/settings.py` 的 setter 事务（替换 `config` 字段 → 重建工具表 / Runtime /
  MCP Manager，失败回滚）、`session/store.py` 的存取门面（会话/项目/归档转发、`_session_facade`
  的磁盘入口）、`session/control.py` 的资源关闭与隔离区收尾：判定与文案已在
  `src/{settings,store,control}.rs`，事件投影编排已搬（`src/store.rs`）。
- `undo.py` 的 git 应用段（`_restore_turn_side_effects` 的补丁应用、
  `_begin_turn_snapshot` 的 store 构造）：crate 只到「可以安全应用」为止。

## 对照（parity）工作流

语义基准是 Python 真实现，不靠人读代码对齐：

```bash
python rust/tools/gen_controllers_fixture.py   # 用 omnicrawl/agent/controllers/ 生成期望值
cd rust && cargo test -p omnicrawl-controllers # 同输入重放 Rust 实现逐字段比对
```

`tests/fixtures/controllers_parity.json` 覆盖 1105 个用例：整数配置读取与区间校验、未知工具
文案（含哈希名反查）、超时结果、限时执行、undo 安全性 15 例、副作用账本与预检 16 例、
快照路径防穿越 13 例、工作区切换 5 例、记忆目录 16 例、输出预算与视觉旁路 26 例、
压缩 42 例（选取/摘要/展示 + 提示词、采样、回包清洗、标签行、截断、取消判定、思考深度），模式与 system prompt 19 例、审批 269 例（名称/字段识别、git 风险分级与变更
判定、命令分流、删除意图、审查结论解析与审批归属）、会话侧 92 例（设置层 76 例与控制面
16 例），以及顾问 23 例（消息分支、工具清单、黑名单、调用与信封）与插件 49 例
（`HOOK_POLICIES` 全表 fail-closed、拒绝事实与文案、分发结局），以及工具参数层 73 例
（标识符/工具名/参数名归一化、参数投影、Schema 压缩与校验、结果信封、MCP 结果文本、读取助手），
工具目录 14 例（11 组 runner/开关组合的注册结果 + MCP 三类名称与说明模板），上下文压缩 79 例（Token 估算、用量累计、预算快照与校验、触发决策、批次选择、事件投影），
编排层另有 13 例（`tests/compaction_orchestration_parity.rs`：测量账本、证据恢复、摘要解析/分块/生成、压缩编排、回合判定面），
含结构化摘要校验 26 例与摘要/原文投影 11 例，以及子代理域 59 例（登记键与查找归一化、会话去重投影、
worktree 产物摘要与收集失败、丢弃保护与三类文案、失败描述、Fork 上下文冻结与任务指令、
公开结果本地投影、后台通知注入、结果校验与定义缺失文案），以及回合接线 14 例（回调轨迹、
两个端口的形参与批次步号、循环收到的消息、最终回复补发、用量累计、失败分类与取消检查点），
以及会话事件投影编排 6 例（`tests/store_parity.rs`：内存事件的逐字段形状与序号推进、
落盘与未落盘事件各自的投影方式），以及会话生命周期编排 20 例（`tests/lifecycle_parity.rs`：
用探针真跑 `close()` 得到的阶段轨迹、关闭回调处置、隔离收尾摘要与回调条件、父 Session 切换排空、
`set_model` 选择串、undo 回退的失败包装文案），以及工具实现判定面 22 例（`tests/tool_impl_parity.rs`：
清单投影与输出文本、`ask_user` 三条拒绝文案与回答信封、记忆 `scope` 解析与未启用文案），
以及审批编排 12 例（`tests/approval_flow_parity.rs`：真跑 `_approve_tool_for_batch` /
`_execute_approved_tool` 得到的阶段轨迹与短路点、落盘事件载荷、生效模式、展示文本回落），
以及插件运行期 15 例（`tests/plugin_runtime_parity.rs`：分发上下文冻结的三种形态
（标准上下文原样、普通对象归一化、拿不到 freeze 时空上下文）、回合 Hook 处置与异常吞掉、
会话生命周期 Hook 名与会话标识回落），以及回合文本规则 28 例（`tests/turn_text_parity.rs`：
短「继续/重试」的标点归一化识别、上一轮任务还原文案、被取消回合的摘要（含工具计数）、
助手消息的推理字段），以及视觉能力判定 11 例（`tests/vision_capability_parity.rs`：
运行时快照缺能力/缺 runtime/缺快照三种退化、显式开关盖过能力、未配置时回落能力），
以及上下文消息装配 10 例（`tests/context_messages_parity.rs`：项目规范消息的空白退化与外壳、
插件附加上下文的字符串包装/字典归一化/非法项忽略/非数组忽略，以及构建后钩子的条数载荷），
以及工具调用事件字段 12 例（`tests/tool_events_parity.rs`：按 ID 或函数名匹配首个命中、
`arguments` 非字符串时不带协议原文、函数名为空不写字段、以及落盘前剔除协议原文的边界），
以及技能命令 9 例（`tests/skill_command_parity.rs`：无管理器/非命令原样放行、命中加载并带出任务
（多空白分隔）、命中无任务给默认提示、未命中带可用列表、空技能名与前导空白不算命令），
以及只读命令策略 111 例（`tests/read_only_commands_parity.rs`：可执行名归一化（引号/路径/扩展名）、
命令链分段（`&&`/`||`/`;`/换行、引号内的 `>`、转义的 `>`、重定向与空片段拒绝）、
curl 的远端写入与本地落盘判定、浏览器 CLI 的子命令与 `--out`/`--file` 判定、
逐 token 段判定（posix 与非 posix 两种引号语义、git 变更子命令、`sed -i`、`find -delete/-exec`、
PowerShell Web 请求的 `-OutFile`/`-Method`）、
以及策略入口 23 例（`monitor` 动作白名单与 shell 回落、不支持的工具名、命令缺失/超长/命令替换/
PowerShell 脚本块/重定向、`diagnostic_command` 的形状与只读复核），
以及 worktree git 判定 24 例（`tests/worktree_git_parity.rs`：分支名片段的字符折叠与截断、
主树脏时的门禁文案（8 行预览与 `; ...` 收尾）、变更统计（目录不存在即无变更、
`rev-list` 失败保守记 1、计数为空按 0））。

期望值来自真实现：能直接调的函数直接调；挂在 Mixin 上的方法用一个最小探针对象驱动
（只补上方法真正读到的属性，不改写被测逻辑）。模板装载一组需要读仓库内
`omnicrawl/templates/`——那一组对照的是「读到了什么」，因此测试按仓库布局定位模板目录。

## 已知与 Python 的差异

1. 线程本地上下文：`_execute_call_with_timeout` 在 Python 侧会带上 `contextvars`
   副本，内核版不带；需要线程本地状态的调用方自行捕获。
2. `Path.resolve()` 只做字面归一化（绝对化 + 展开 `.`／`..`），不解析符号链接。会话目录内
   若有指向目录外的符号链接，两侧的越界判定会不同。
3. `str(value)` 只覆盖标量（字符串、数字、`True`/`False`、`None`）；容器退化为 JSON 文本，
   Python 是 `repr` 形态。
4. 整数解析是 `int()` 的子集：支持可选符号、ASCII 数字与数字间的下划线；Unicode 数字、
   任意精度整数未搬（超出 `i64` 按解析失败处理）。
5. `_validate_context_compaction_window` 在 Python 侧已是显式 no-op（只为保留兼容入口），
   内核没有这份历史包袱，不再提供该函数。
6. `_load_system_prompt_template` 只搬文件读取与错误文案；`build_system_prompt` 的渲染属
   提示上下文子系统。
7. 平台相关文案（文件系统错误的尾巴）按前缀／后缀对照，不逐字比对。
8. 少数只读字段的读取口径：`(windows_window/clipboard/screenshot)` 的 action 判定按
   Python 的 `str(arguments.get(...) or default).strip()` 实现；非字符串 action 走
   `str()` 的标量子集。
9. 审批规则的手写匹配器用「逐字符小写比较」实现 `re.IGNORECASE`（Cyrillic 等非 ASCII
   大小写对同样覆盖）；差别只在小写展开为多字符的字符（如 `İ`）不会与单字符模式匹配。
10. `parse_tool_review_response` 的 JSON 候选扫描用 `serde_json` 解析：`NaN`/`Infinity`
    这类 Python 接受但 JSON 标准不允许的载荷在内核侧解析失败。
11. setter 的「类型守卫」文案（如「工具开关必须是布尔值。」「XX 配置必须是 XX 类型。」）
    是 Python 鸭子类型产物，内核靠类型系统保证，不再逐条搬运；领域性校验（空模型 ID、
    正整数 Token／百分比、开关名、资源参数区间）已搬。
12. `{value:g}` 只覆盖非科学计数法区间：整数值不带小数点，其余走 `f64` 最短表示。
13. 「当前会话不存在」在 Python 里是 `state is None` 的前置判断，内核把它建模进
    `session_closed_action(Option<&str>)`。
14. 接线的两个循环端口共用同一个报告句柄：Python 侧它们共用 `self`，内核在 `run_stream`
    内部用 `Rc<RefCell<_>>` 保证同一时刻只有一处可变借用；宿主看不到这层共享。
15. `_execute_tool_batch` 的 `prompt` / `status` / `active_runtime_snapshot` / `vision_base_llm` /
    `record_tool_execution` / `check_cancelled` 由宿主的批次实现自己持有（闭包捕获），
    接线只给调用、起始步号与报告句柄；`run_stream` 的 `turn_id` 与 `working_messages` 的
    上下文部分同理由宿主传入。
16. 取消分类：Python 沿异常因果链按类名与 `ModelErrorCode.CANCELLED` 判定
    （`_is_turn_cancel_exception`），内核收敛为 `LoopError::Cancelled` 的 typed 判定。
17. 内存事件的会话标识：Python 是 `str(getattr(state, "session_id", 占位))`，属性存在但值为
    `None` 时会得到字符串 `"None"`；内核用 `Option<&str>` 表达「属性缺失」，这种取值未纳入对照。
18. 内存事件序号：Python 的序号是任意精度整数，内核对 `u32` 饱和自增；该序号只在单轮内存事件里
    递增，溢出不可达。
19. 隔离收尾摘要：Python 不返回摘要，只在满足回调条件时把摘要交给 `on_finalized`；因此「不满足
    回调条件」的用例里摘要文本无从观察，`finalize_isolation_summary` 只对照回调条件，摘要按语义
    （会话不参与、子任务为空即空串）成立。
20. 生效审批模式：Python 的 `getattr(config, "approval_mode", REVIEW)` 在「属性存在但值为 `None`」
    时会返回 `None`；内核用 `Option<&str>` 表达「未配置」，这种取值未纳入对照。

## 验证

```bash
cd rust
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p omnicrawl-controllers
```
