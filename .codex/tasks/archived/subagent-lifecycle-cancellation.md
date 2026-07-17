# 任务：OmniCrawl SubAgent 生命周期取消域

状态：已完成
创建：2026-07-14
更新：2026-07-14

## 需求摘要

- 继续落实 `docs/SUBAGENT_DESIGN.md` 中尚未完成的 Phase 1 验收项。
- 为同步 SubAgent 增加独立取消域、活动批次注册和有界等待。
- 父 Run 取消、`LocalToolAgent.close()` 与工作区切换必须主动取消子任务。
- 工作区切换无法在期限内回收子任务时，保持旧工作区资源完整并拒绝切换。
- 本轮不扩展后台任务、ApprovalBroker、verify、Fork 或写能力。

## 关键决策

- Coordinator 是同步子任务线程、排队 Future、取消令牌和真实存活状态的唯一所有者。
- 采用合作式取消；Python 线程不可强杀时，有界等待超时后不拆除共享 Runtime、MCP、Session、临时目录或 PluginRuntime。
- 排队 Future 的取消和终态事件在独立清理线程完成；慢/失败事件出口计入批次 idle，但不能突破调用方 deadline 或泄漏剩余任务。
- 工作区切换在准备新工作区前先暂停新批次并回收旧任务，避免准备阶段临时修改 Agent 可变字段时与子线程竞态。
- Agent 关闭超时后注册 idle callback，最后一个子任务退出时自动完成资源和进程级 PluginRuntime 关闭。
- PluginRuntime 的首次启动与工作区重建采用候选 Manager 事务式提交，失败时关闭候选并保持或安全降级。

## 实现计划

- [x] 1. 为 Coordinator 增加批次取消令牌、活动注册、暂停/关闭和有界等待
- [x] 2. 将组合取消检查传入子 Agent Loop，并补强父取消级联
- [x] 3. 接入 `LocalToolAgent.close()` 与工作区切换的安全回收顺序
- [x] 4. 增加 Coordinator、Agent close、PluginRuntime 和工作区切换回归测试
- [x] 5. 更新设计/README 状态并完成定向与全量验证

## 已修改文件

- `omnicrawl/agent/subagents/coordinator.py`
- `omnicrawl/agent/core.py`
- `omnicrawl/extensions/plugin_manager.py`
- `omnicrawl/entry.py`
- `omnicrawl/api/app.py`
- `tests/test_subagent_coordinator.py`
- `tests/test_subagent_integration.py`
- `tests/test_workspace_switch.py`
- `tests/test_plugin_manager.py`
- `README.md`
- `docs/README.md`
- `docs/SUBAGENT_DESIGN.md`
- `.codex/tasks/subagent-lifecycle-cancellation.md`

## 验证

- 生命周期定向回归：`python -m unittest tests.test_plugin_manager tests.test_subagent_coordinator tests.test_workspace_switch tests.test_subagent_integration`，59 tests passed。
- 全量回归：`python -m unittest discover -s tests -q`，494 tests passed。
- `python -m compileall -q omnicrawl main.py tests` 通过。
- `git diff --check` 通过。
- 三轮只读审查发现的取消 deadline、事件出口异常、deferred close、PluginRuntime 初始化/切换泄漏问题均已补测并修复；最终无已知高严重度 blocker。

## 残余风险

- 第三方 Provider SDK 或工具若永久不响应合作式取消，Python 线程仍无法被强制终止；实现会继续保留共享资源，避免跨线程拆除。
- 四种 Provider 的专用 Fake Runtime 契约测试仍未完成。
- 后台任务、ApprovalBroker、verify、Fork、模型覆盖和 Worktree 写隔离仍属于后续 Phase 2/3。
