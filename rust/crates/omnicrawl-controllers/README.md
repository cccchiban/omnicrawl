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
| `controllers/tools/output.py` | `src/output.rs` | 批次输出预算与落盘预览、工具结果消息、视觉旁路文案 |
| `controllers/tools/compression.py` | `src/compression.rs` | 压缩选取、展示文本、参数摘要 |
| `controllers/tools/building.py` | `src/building.rs` | 模式模板装载与 system prompt 组装 |
| `agent/runtime/llm_protocol.py`（一函数） | `src/shared.rs` | `resolve_tool_name_from_hashed_function_name` |
| `agent/types.py` | `src/types.rs` | `ToolCall` / `ToolResult` / `ToolImageAttachment` |
| `state/session_artifacts.py`、`state/session_projection.py`（各一函数） | `src/output.rs` | `preview_text` / `tool_result_message` |
| `controllers/__init__.py` 的错误面 | `src/error.rs` | `AgentError`（只承载文案） |

## 尚未搬的部分（宿主粘合层）

以下内容依赖宿主对象或多子系统协作，留在 Python 侧；后续批次按依赖顺序收口：

- `tools/approval.py`：插件钩子编排、批内审批流程；其判定件在
  `agent/toolkit/approval_policy.py`（shell 分类、git 风险分级、删除意图、审查结论解析），
  随工具层一起搬。
- `tools/building.py` 的 `_build_tools` / `_build_mcp_tools`：工具表构建，依赖
  `agent/toolkit/tools.py`。
- `tools/implementations.py`：工具实现本体（属 `agent/toolkit/`）。
- `session/{control,settings,store}.py`、`turn/{loop,compaction}.py`、
  `subagents/{orchestration,worktrees}.py`、`advisor.py`、`plugins.py`：
  会话门面、回合循环与子代理编排，依赖 `omnicrawl-core` 的 runner、`omnicrawl-session`
  的存储与 `omnicrawl-ipc` 的回调面，先接线再搬家。
- `undo.py` 的 git 应用段（`_restore_turn_side_effects` 的补丁应用、
  `_begin_turn_snapshot` 的 store 构造）：crate 只到「可以安全应用」为止。

## 对照（parity）工作流

语义基准是 Python 真实现，不靠人读代码对齐：

```bash
python rust/tools/gen_controllers_fixture.py   # 用 omnicrawl/agent/controllers/ 生成期望值
cd rust && cargo test -p omnicrawl-controllers # 同输入重放 Rust 实现逐字段比对
```

`tests/fixtures/controllers_parity.json` 覆盖 152 个用例：整数配置读取与区间校验、未知工具
文案（含哈希名反查）、超时结果、限时执行、undo 安全性 15 例、副作用账本与预检 16 例、
快照路径防穿越 13 例、工作区切换 5 例、记忆目录 16 例、输出预算与视觉旁路 26 例、
压缩 13 例、模式与 system prompt 19 例。

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

## 验证

```bash
cd rust
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p omnicrawl-controllers
```
