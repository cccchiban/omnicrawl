# 任务：OmniCrawl SubAgent Phase 1C

状态：已完成
创建：2026-07-14
更新：2026-07-14

## 需求摘要

- 基于 `docs/SUBAGENT_DESIGN.md` 继续开发同步定义式 SubAgent。
- 本轮完成父 Session 生命周期事件、安全结果投影与任务级 artifact。
- 本轮完成 API SSE 与全屏 TUI 的最小子任务可观察性。
- 本轮不实现后台 TaskManager、ApprovalBroker、verify、Fork、写能力，以及 `close()`/工作区切换的主动取消与有界等待。

## 关键决策

- Coordinator 统一产生 batch/task 生命周期事件，LocalToolAgent 在单一串行出口中先写父 Session，再通知 API/TUI。
- `subagent_*` 事件保持 additive，不加入 `MODEL_CONTEXT_EVENT_TYPES`。
- 子任务完整结果先脱敏，再裁剪；超过 `result_summary_chars` 时写入 `.agent_sessions/artifacts/<session_id>/subagents/<task_id>.json`。
- 公开模型错误使用固定消息，不回显 Provider 原始请求或异常正文。
- `public_tool_arguments()` 是外层工具公开参数的统一投影；`subagent` 的 `tasks[].prompt` 不进入确认、SSE、TUI 工具展示或 Session 工具事件。
- TUI `KeyboardInterrupt` 与 API 取消均映射为任务 `cancelled` 终态；终态 guard 防止重复事件。
- Session 持久化错误仍中断父回合；API/TUI observer 错误只记录类型，不反向破坏任务状态机。

## 主要修改

- `omnicrawl/agent/subagents/coordinator.py`
- `omnicrawl/agent/core.py`
- `omnicrawl/agent/tools.py`
- `omnicrawl/agent/session_facade.py`
- `omnicrawl/state/session.py`
- `omnicrawl/state/session_models.py`
- `omnicrawl/state/session_artifacts.py`
- `omnicrawl/api/service.py`
- `omnicrawl/ui/fullscreen/turns.py`
- `omnicrawl/ui/fullscreen/__init__.py`
- `omnicrawl/commands/slash.py`
- `tests/test_agent_execution.py`
- `tests/test_subagent_coordinator.py`
- `tests/test_subagent_integration.py`
- `tests/test_session_store.py`
- `tests/test_api.py`
- `tests/test_fullscreen_turns.py`
- `tests/test_fullscreen_tui.py`
- `README.md`
- `docs/README.md`
- `docs/API.md`
- `docs/SUBAGENT_DESIGN.md`

## TDD 修复记录

### Red

新增并实际运行 8 个复现测试，确认以下问题在修复前失败：

- `KeyboardInterrupt` 不产生 cancelled 终态；
- Provider 异常正文回显完整请求；
- API/TUI observer 异常破坏生命周期；
- GitHub/AWS/PEM 凭据进入摘要或 artifact；
- Session `tool_call_requested` 保存完整子任务 prompt；
- API `confirmation.required` 发送完整子任务 prompt；
- 公开参数二次投影丢失 task_count/description/agent_type；
- `fail_fast=true` 中途取消时，尚未提交的任务缺少 `cancelled` 终态。

### Green

- 显式处理 `KeyboardInterrupt`，取消 pending future，并增加任务终态 guard；
- 引入幂等的 `public_tool_arguments()` 并接入所有公开/持久化出口；
- observer 异常隔离；
- 公开模型错误固定化；
- 扩展 GitHub token、AWS Access Key ID、PEM 私钥脱敏。

### Verify

- Red 用例修复后：6 tests passed；投影幂等测试 passed。
- Phase 1C 与依赖模块定向回归：121 tests passed。
- 全量回归：`python -m unittest discover -s tests -q`，481 tests passed。
- `python -m compileall -q omnicrawl main.py tests` 通过。
- `git diff --check` 通过。

## 残余风险

- Provider SDK 或系统调用若不响应协作式取消，当前同步线程仍无法被强制终止。
- `close()` 和工作区切换尚未拥有独立取消域与有界等待；设计验收项保持未勾选。
- 四种 Provider 的专用 Fake Runtime 契约测试尚未补齐。
- 后台任务、ApprovalBroker、verify、Fork、模型覆盖与 Worktree 写隔离仍属于 Phase 2/3。
