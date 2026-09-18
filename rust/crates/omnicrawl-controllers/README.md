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
| `controllers/session/settings.py`、`config/features/{approval,tools,subagents}.py` | `src/settings.rs` | 审批模式与推理强度归一化、压缩阈值换算、工具开关名与禁用集合、SubAgent 资源参数校验 |
| `controllers/session/control.py` | `src/control.rs` | 插件子系统状态文案、退出收尾动作、关闭/停用前的排空决策 |
| `controllers/tools/approval.py`、`agent/toolkit/approval_policy.py` | `src/approval.rs` | 审批归属判定、shell 命令分流、git 风险分级、删除意图、审查结论解析与失败文案 |
| `agent/context_compaction/{models,policy,validation,projection}.py` | `src/context_compaction/` | 上下文 Token 估算、回合预算测量、压缩批次与超限恢复批次、自动压缩决策、结构化摘要校验、摘要在前/原文在后的模型上下文投影 |
| `controllers/advisor.py` | `src/advisor.rs` | 顾问可用性判定、消息分支（剥孤儿调用 + user 尾）、工具清单、结果信封与错误文案 |
| `controllers/plugins.py` | `src/plugins.rs` | Hook fail-closed 判定、拒绝事实与文案、分发结局归一化、会话生命周期 Hook 名 |
| `controllers/subagents/worktrees.py`、`orchestration.py`（判定面） | `src/subagents/` | worktree 登记键与查找归一化、会话去重投影、产物摘要渲染、丢弃保护判定、失败描述、Fork 上下文冻结、公开结果投影、后台通知注入、结果校验与定义缺失文案 |
| `agent/toolkit/tools.py`（目录与注册） | `src/tool_catalog.rs` + `data/agent_tools.json` | 目录数据由 `rust/tools/gen_agent_tools_data.py` 导出；注册规则（可选 runner 省略、知识库/Windows 整组、记忆开关、SubAgent 角色枚举、禁用过滤）在 Rust 重放 |
| `agent/toolkit/tools.py`、`host_tools.py` | `src/tool_args.rs` | 工具名/参数名归一化、参数投影（Session/确认页/SSE）、紧凑 Schema、Schema 校验与结果信封 |
| Python `json.dumps` 子集 | `src/json.rs` | Python 风格 JSON 文本与 `repr`（工具结果信封、审查指令、参数摘要共用） |
| `controllers/tools/output.py` | `src/output.rs` | 批次输出预算与落盘预览、工具结果消息、视觉旁路文案 |
| `controllers/tools/compression.py` | `src/compression.rs` | 压缩选取、展示文本、参数摘要 |
| `controllers/tools/building.py` | `src/building.rs` | 模式模板装载与 system prompt 组装 |
| `agent/runtime/llm_protocol.py`（一函数） | `src/shared.rs` | `resolve_tool_name_from_hashed_function_name` |
| `agent/types.py` | `src/types.rs` | `ToolCall` / `ToolResult` / `ToolImageAttachment` |
| `state/session_artifacts.py`、`state/session_projection.py`（各一函数） | `src/output.rs` | 复用 `omnicrawl-session` 的 `preview_text` / `tool_result_message`（不再重复实现） |
| `controllers/__init__.py` 的错误面 | `src/error.rs` | `AgentError`（只承载文案） |

## 尚未搬的部分（宿主粘合层）

以下内容依赖宿主对象或多子系统协作，留在 Python 侧；后续批次按依赖顺序收口：

- `tools/approval.py` 的编排段：插件钩子（`tool.call.before` / `tool.approval.before` /
  `tool.execute.before` 等）、审查模型的 Responses 请求、用户确认面板、会话事件持久化；
  判定件（`agent/toolkit/approval_policy.py` 的全部规则）已在 `src/approval.rs`。
- `tools/building.py` 的 `_build_tools` / `_build_mcp_tools`：工具表构建，依赖
  `agent/toolkit/tools.py`。
- `tools/implementations.py`：工具实现本体（属 `agent/toolkit/`）。
- `toolkit/tools.py` 的**执行函数绑定**：目录与注册规则已搬（`src/tool_catalog.rs` + 导出的
  `data/agent_tools.json`），但各工具的实现本体（`agent/toolkit/*`、`workspace/`、`mcp/`…）仍在 Python。
- `turn/{loop,compaction}.py`：回合循环与压缩编排；其判定依赖的 `context_compaction`
  **预算/批次/决策 + 摘要校验 + 投影**已搬（`src/context_compaction/`），仍未搬的是同目录的
  `service.py`（摘要模型调用编排）、`summary.py`（结构化摘要生成）、`evidence.py`（证据检索），
  以及 `omnicrawl-core` runner 与 `omnicrawl-ipc` 回调面的接线。
- `subagents/{orchestration,worktrees}.py` 的进程面：Coordinator/TaskManager 生命周期、
  模型运行时引导（`_create_subagent_runtime_manager` / `_run_subagent_task_loop` /
  `_prepare_subagent_execution`）、worktree 的 git 创建/应用/清理、`_refresh_subagent_definitions`
  与事件观察者转发；判定与投影已在 `src/subagents/`。
- `advisor.py` 的模型面：`apply_model_selection` + 独立 Runtime 引导 + 协议单轮补全 +
  用量回调；判定、消息分支与信封已在 `src/advisor.rs`。
- `plugins.py` 的进程面：Plugin Runtime / Worker 生命周期、`HOOK_POLICIES` 表本身与
  配置读写；分发后的判定与文案已在 `src/plugins.rs`。
- `session/settings.py` 的 setter 事务（替换 `config` 字段 → 重建工具表 / Runtime /
  MCP Manager，失败回滚）、`session/store.py` 的存取门面、`session/control.py` 的资源关闭
  与隔离区收尾：判定与文案已在 `src/{settings,control}.rs`。
- `undo.py` 的 git 应用段（`_restore_turn_side_effects` 的补丁应用、
  `_begin_turn_snapshot` 的 store 构造）：crate 只到「可以安全应用」为止。

## 对照（parity）工作流

语义基准是 Python 真实现，不靠人读代码对齐：

```bash
python rust/tools/gen_controllers_fixture.py   # 用 omnicrawl/agent/controllers/ 生成期望值
cd rust && cargo test -p omnicrawl-controllers # 同输入重放 Rust 实现逐字段比对
```

`tests/fixtures/controllers_parity.json` 覆盖 811 个用例：整数配置读取与区间校验、未知工具
文案（含哈希名反查）、超时结果、限时执行、undo 安全性 15 例、副作用账本与预检 16 例、
快照路径防穿越 13 例、工作区切换 5 例、记忆目录 16 例、输出预算与视觉旁路 26 例、
压缩 13 例、模式与 system prompt 19 例、审批 269 例（名称/字段识别、git 风险分级与变更
判定、命令分流、删除意图、审查结论解析与审批归属）、会话侧 92 例（设置层 76 例与控制面
16 例），以及顾问 23 例（消息分支、工具清单、黑名单、调用与信封）与插件 49 例
（`HOOK_POLICIES` 全表 fail-closed、拒绝事实与文案、分发结局），以及工具参数层 73 例
（标识符/工具名/参数名归一化、参数投影、Schema 压缩与校验、结果信封、MCP 结果文本、读取助手），
工具目录 14 例（11 组 runner/开关组合的注册结果 + MCP 三类名称与说明模板），上下文压缩 79 例（Token 估算、用量累计、预算快照与校验、触发决策、批次选择、事件投影），
含结构化摘要校验 26 例与摘要/原文投影 11 例，以及子代理域 59 例（登记键与查找归一化、会话去重投影、
worktree 产物摘要与收集失败、丢弃保护与三类文案、失败描述、Fork 上下文冻结与任务指令、
公开结果本地投影、后台通知注入、结果校验与定义缺失文案）。

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

## 验证

```bash
cd rust
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p omnicrawl-controllers
```
