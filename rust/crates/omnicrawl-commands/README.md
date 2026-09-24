# omnicrawl-commands

斜杠命令框架与内置命令（Python 侧 `omnicrawl/commands/` 的 Rust 移植）。

全仓库唯一的命令事实来源：命令只声明一次，解析、注册校验、分发、帮助与补全菜单都从同一份
声明派生，TUI / Telegram / 飞书 / 本地 API 各入口只调 `registry().dispatch(...)`。

## 模块

| 文件 | 对映 Python | 职责 |
| --- | --- | --- |
| `src/framework.rs` | `commands/framework.py` | `CommandType` / `Command` / `ParsedCommand` / `CommandResult` / `CommandContext` / `CommandRegistry`：归一化、别名冲突、解析、`argv`、隐藏命令、补全、帮助、`immediate` |
| `src/agent.rs` | （Python 无对应物） | `CommandAgent` 能力面与命令用到的值类型（`SessionSummary` / `SubAgentRun` / `GitOutput`），外加单测用的宿主替身 |
| `src/slash.rs` | `commands/slash.py` | `registry()` / `build_registry()` 与 27 条内置命令、工具确认文案、会话与历史的展示、`/review` 报告渲染、advisor 与 mode 辅助函数、`build_slash_commands` / `build_slash_command_options` |

## 与 Python 的两处必要差异

1. **宿主能力经 trait 注入**。Python 的处理器拿 `CommandContext.agent`（`Any`）按名调用
   `LocalToolAgent` 的方法；Rust 把这份能力面收成 `CommandAgent`（29 个必需方法 + 4 个带默认
   实现的方法），因此本 crate 不依赖任何宿主，也不会与 `omnicrawl-host` 互相引用。
   宿主缺哪项能力就必须显式返回 `AgentError`（如「会话列表读取失败：…」），只有 Python
   本身写了兜底的分支才给默认实现：`config_environment`（进程环境）、`compaction_notice`
   （空串）、`skill_metas`（`None` → 「Skill 子系统未启用。」）、`plugins_status`（`None`
   → 「插件状态接口不可用。」）。

2. **延迟执行把宿主当参数**。Python 的 `deferred` 是无参闭包（闭包捕获了 agent）；
   Rust 的 `DeferredCommand = Box<dyn FnOnce(&dyn CommandAgent) -> CommandResult + Send>`
   在 `CommandResult::resolve(agent)` 时才收到宿主。好处是闭包只捕获自己需要的数据，
   宿主对象既不必包 `Arc`，也不必是 `Send + Sync`，闭包本身仍可交给工作线程
   （`/compact`、`/review`、`/mcp`、`/workspace` 四条慢命令照 Python 的语义拆成
   即时返回 + 延迟执行）。

配置读写（审批模式、推理强度、模型选择、顾问、工作区）直接用 `omnicrawl-config` 已搬好的
写回函数并传入 `agent.config_environment()`，与 Python `slash.py` 同时调 agent 方法与配置函数
的结构一致。

## 宿主接线（当前状态）

本 crate 只提供框架与命令定义；宿主侧 TUI 已把能力面接上（`omnicrawl-tui/src/commands.rs`
的 `TuiHostAgent`），当初挡在前面的三件事都已解决：

1. **协议缺口已补**：`session.list` / `session.history` / `session.resume` / `session.new` /
   `session.archive` / `session.rename` / `session.compact` / `turn.undo` / `subagent.query` 等
   方法都已在内核侧落地，命令层对内核只需转发；`/mcp` 走宿主工具表
   （`McpClientManager::format_status()`），不需要协议方法。
2. **代理对象不再要求线程安全**：命令处理器是同步接口，TUI 用 `TuiHostAgent`（以 `RefCell`
   借用 `App`）直接实现，需要内核对往返的命令就地做一次同步 `request_kernel`，
   没有另建 `Arc` 代理。
3. **三条慢命令不进注册表**：`/undo`、`/compact`、`/review` 由 TUI 在分发前拦下
   （`KernelCommand::from_parsed`）→ 宿主异步下发内核（整轮回退、摘要模型、
   子代理工具批次要回到宿主执行层，同步等会死锁）；命令层这三条只回「已交给宿主」。
   需要延迟执行的命令用 `CommandResult::with_deferred`（如 `/workspace`），`/settings` 则经
   注册表返回 `with_open_settings`，由 TUI 打开面板。

## 与 Python 的已知文案差异

- `CommandResult` 里 Python 未捕获的宿主异常（如 `set_approval_mode`、`reset_conversation`
  抛出的 `AgentError`）在 Rust 侧会转成一条消息返回（原文为错误文本），不再向上传播。
- `slash.py` 的四个 `print_*` 行内 UI 包装（`print_skills_list` 等）照搬保留，仅行内 UI 入口调用。
- `/workspace` 的 `switch_workspace` 失败在 Python 会向上抛，Rust 侧返回错误文本。
- `name.replace('-', " ")`、「按字符数截断」等与 Python 同口径；`casefold()` 用
  `to_lowercase()` 代替（命令名与别名都是 ASCII 或中文，两者同效）。

## 验证（本机不跑 cargo）

本环境禁止任何编译/构建/测试类命令（`cargo fmt/check/build/test` 一律不跑），因此改动只能用
源码级对照脚本核对：

```bash
python .omnicrawl/.agent_tmp/scripts/check_commands_parity.py   # Python 真实现 ↔ Rust 命令表逐字段比对
python .omnicrawl/.agent_tmp/scripts/check_rust_balance.py rust/crates/omnicrawl-commands/src/*.rs
```

对照脚本用 `ast` 解析 `omnicrawl/commands/slash.py` 的 `@REGISTRY.command(...)`，与
`build_registry()` 逐条比对名称、处理器名、别名、用法、类型、参数提示、隐藏、说明文案与
补全形态；当前 27 条命令全部一致（0 失败）。

有 cargo 的机器上按仓库约定验证：

```bash
cd rust && cargo test -p omnicrawl-commands && cargo clippy -p omnicrawl-commands --all-targets -- -D warnings
```

`src/framework.rs` 与 `src/slash.rs` 的 `#[cfg(test)] mod tests` 覆盖：注册与别名冲突、
解析与整词别名、未命中哨兵、延迟执行的惰性、候选菜单（`insert` 与命令名一致、Skill 候选不带
`parameters` 键）、帮助文本、工具确认文案、`/review` 报告渲染与前置检查、会话/历史/Skill/
插件/子任务的展示文案、远端通道对 `/approval:auto`、`/quit`、`/settings` 的拒绝。
