# 任务：SubAgent ApprovalBroker Core

状态：已完成
创建：2026-07-15
更新：2026-07-15

## 需求摘要

- 为 OmniCrawl SubAgent 实现 ApprovalBroker Core：所有未来需要人工确认的子工具调用必须经单一串行 Broker 仲裁。
- 用户确认的策略边界：子 Agent 默认不需要人工确认；仅所有删除意图，以及变更性 Git 操作需要逐次人工确认。
- `git status`、`git diff`、`git log`、`git show` 等只读 Git 查询免确认；无法确认是否只读的未知 Git 子命令按需确认。
- 本轮不开放 verify、命令工具、写能力、远程审批路由或新的后台审批控制面；当前 `read_only` 子代理权限保持不变。

## 关键决策

- Broker 使用线程安全 FIFO 队列，任一时刻最多向确认处理器提交一个请求。
- 请求始终携带任务、批次、角色和工具来源；公开事件只使用脱敏参数投影，绝不包含完整任务 prompt。
- 任务/批次/父生命周期取消会拒绝待处理请求；已取消或已终态任务的迟到批准不可恢复执行。
- 复用现有 `tool.call.before`、`tool.approval.before/after`、`tool.execute.*` 链路；插件只能拒绝，不能替代用户批准。
- Core 复用既有终端、全屏 TUI 与活动父 Run API 确认处理器，但不新增远程审批接口；父 Run 已结束的后台风险请求安全拒绝。
- 外层 `subagent` 分发也取消常规人工确认；现有定义仍强制 `read_only`，因此不扩大任何当前可用权限。

## 实现计划

- [x] 1. 核对现有审批、工具、任务取消、API/TUI 和 Session 事件边界
- [x] 2. 实现 ApprovalBroker、任务来源上下文与删除/Git 审批策略
- [x] 3. 接线到 Coordinator 生命周期及子工具审批链，保持 read_only 能力边界不变
- [x] 4. 增加并发、取消、迟到决策、策略与来源脱敏测试
- [x] 5. 更新设计文档并完成定向、全量、编译与 diff 验证

## 已修改文件

- `omnicrawl/agent/subagents/approval.py`：新增 Broker、来源模型、线程局部审批作用域与窄风险策略。
- `omnicrawl/agent/approval_policy.py`：新增变更性 Git 识别；未知 Git 子命令按需确认。
- `omnicrawl/agent/subagents/coordinator.py`：绑定任务来源、取消 task/batch/all 待决审批、生命周期关闭 Broker。
- `omnicrawl/agent/core.py`：接线子任务审批作用域与专用确认 handler；保留父工具旧审批钩子调用兼容性。
- `omnicrawl/agent/tools.py`：受限 `subagent` 分发取消常规人工确认。
- `omnicrawl/api/models.py`、`omnicrawl/api/service.py`：为活动父 Run 的确认增加可选可信 SubAgent 来源字段。
- `omnicrawl/commands/slash.py`、`omnicrawl/ui/fullscreen/__init__.py`、`omnicrawl/state/session_models.py`：显示安全来源并持久化 waiting-approval 生命周期事件。
- `docs/SUBAGENT_DESIGN.md`、`docs/API.md`：同步实现状态、策略和 API/SSE 边界。
- `tests/test_subagent_approval.py`、`tests/test_subagent_coordinator.py`、`tests/test_subagent_integration.py`、`tests/test_api.py`、`tests/test_fullscreen_tui.py`：覆盖策略、FIFO、取消、迟到批准、关闭清理、上下文来源、API/TUI 与外层分发策略。

## 验证

- Red：全量测试首次运行稳定复现 2 项兼容性回归：普通父工具路径向被替换的 `_approve_tool_for_batch` 传入新关键字参数。
- Green：恢复普通父工具的旧调用形态，仅子任务 Broker 路径传递新增上下文；两项复现测试通过。
- 受影响回归：`python -m unittest tests.test_agent_execution tests.test_subagent_approval tests.test_subagent_coordinator tests.test_subagent_integration tests.test_api tests.test_fullscreen_tui tests.test_approval tests.test_agent_context tests.test_session_store tests.test_agent_module_boundaries tests.test_api_module_boundaries -q`：204 项通过。
- 全量：`python -m unittest discover -s tests -q`：530 项通过（36.098 秒）。
- 编译：`python -m compileall -q omnicrawl main.py tests`：通过。
- 差异检查：`git diff --check`：通过。
- 审查：已完成主链路自审；独立只读 reviewer 因回合上限未产出有效结论，未将其视为验证证据。

## 残余风险与后续

- 当前没有可触发 Broker 的生产子工具：`read_only` profile 仍无命令、删除或 Git 工具；Broker 为 verify/后续 profile 预置。
- 跨父 Run 的后台远程审批查询/决议 API、持久审批队列和交互控制面仍未实现，属于后续 1A-Full。
- Git 检测覆盖常见标准命令与子命令；未知 Git 子命令保守要求确认，但 shell alias/自定义包装命令的严格语义识别应在未来命令 allowlist 阶段继续收紧。
