# 任务：OmniCrawl SubAgent 任务保留与清理闭环

状态：已完成
创建：2026-07-15
更新：2026-07-15

## 需求摘要

- 落实 `docs/SUBAGENT_DESIGN.md` Phase 2 中尚未完成的“增加任务保留和清理”。
- 终态后台 SubAgent 任务及其尚未消费的通知必须在配置的 TTL 到期后自动从进程内 TaskManager 清理，不能依赖后续 `list`、`get` 或通知 drain 调用触发。
- 保持既有 Session 归档策略：归档保留 Artifact 引用，只有删除会话时才删除关联 Artifact。

## 关键决策

- 复用 `SubAgentTaskManager` 的既有 TTL 规则和单一内存状态机，不引入外部队列、持久化恢复或新的配置字段。
- TaskManager 使用自身持有的可停止 `Condition` 清理线程，仅等待下一项终态记录到期；新终态和永久关闭都会唤醒它重新计算，无固定频率轮询。
- 查询入口仍保留同步清理，以维持短 TTL、时钟变化与既有控制面调用的一致性。
- 到期边界采用 `<=` / `>` 配对：记录在准确到期时立即回收，避免线程在零超时条件下短暂自旋。

## 实现计划

- [x] 1. 为 TaskManager 添加可停止、按下一到期时间唤醒的 TTL 清理调度，并在永久关闭时回收线程。
- [x] 2. 先新增自动清理、精确到期边界与关闭生命周期的回归测试，再完成实现。
- [x] 3. 运行 SubAgent / Session 定向回归、全量测试、编译和差异检查。
- [x] 4. 更新设计稿、README 与任务记录并归档。

## 已修改文件

- `omnicrawl/agent/subagents/tasks.py`
- `tests/test_subagent_tasks.py`
- `README.md`
- `docs/README.md`
- `docs/SUBAGENT_DESIGN.md`
- `.codex/tasks/archived/subagent-task-retention-cleanup.md`

## 验证

- 改动前：SubAgent 相关 107 项测试通过。
- Red：`test_terminal_task_expires_without_followup_query` 在旧实现中稳定失败，证明过期记录依赖后续查询才会清理。
- Green：`python -m unittest tests.test_subagent_tasks -v`，10 项通过。
- 到期边界 Red/Green：精确 TTL 边界用例先失败，再修正为到期即回收后通过。
- 跨模块定向回归：`python -m unittest tests.test_subagent_tasks tests.test_subagent_coordinator tests.test_subagent_integration tests.test_agent_context tests.test_workspace_switch tests.test_session_store -q`，138 项通过。
- 重复稳定性：TTL 自动清理、精确到期、永久关闭 3 项用例连续 5 轮通过。
- 全量：`python -m unittest discover -s tests -q`，557 项通过。
- 编译：`python -m compileall -q omnicrawl main.py tests` 通过。
- 差异：`git diff --check` 与未跟踪文件空白检查通过。

## 残余风险

- 保留策略仅覆盖进程内 TaskManager 的任务快照和未消费通知；服务重启后的跨进程任务恢复仍未实现。
- 独立 Plugin dispatch context、Worktree 隔离和通用写 Agent 仍按设计稿留待后续阶段。
- 两次外部只读 reviewer 分别因上游 400 与超时未返回有效审查结果；未将其作为验证证据，已用本地边界审查、定向回归、重复稳定性与全量回归替代。
