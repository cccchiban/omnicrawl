# 任务：OmniCrawl SubAgent 最终安全不变量审计

状态：已完成
创建：2026-07-17
更新：2026-07-17

## 需求摘要

- 完成 `docs/SUBAGENT_DESIGN.md` 最终未勾选的安全验收项。
- 以可执行回归证明：不存在无限递归、权限扩大、隐藏推理/完整 prompt/凭据泄露，以及跨工作区残留任务、审批或 Worktree。
- 若审计发现实现缺陷，先添加可复现失败测试，再做最小修复并完成全量回归。

## 关键决策

- 将四项安全不变量映射到现有 Coordinator、Execution、Approval、TaskManager、Recovery、Session/API/SSE 和 Workspace 生命周期，不新造平行业务实现。
- 测试不依赖真实模型、网络、MCP Server 或 Git 远端；使用 Fake Runtime、临时目录、事件夹具和可控取消信号。
- 仅在证据覆盖全部四项后勾选最终验收，不以“现有测试大致相关”代替专项证明。

## 实现计划

- [x] 1. 建立四项安全不变量的代码路径与现有测试矩阵。
- [x] 2. 补充专项回归并运行，确认当前实现存在 Worktree 残留和 Session 切换生命周期缺口。
- [x] 3. 修复发现的问题，验证取消、恢复、审批、工作区和公开投影边界。
- [x] 4. 运行定向与全量测试，更新设计稿和任务记录后归档。

## 已修改文件

- `omnicrawl/agent/core.py`
- `omnicrawl/agent/session_facade.py`
- `tests/test_subagent_worktree.py`
- `tests/test_subagent_integration.py`
- `tests/test_workspace_switch.py`
- `tests/test_agent_context.py`
- `README.md`
- `docs/README.md`
- `docs/SUBAGENT_DESIGN.md`
- `.codex/tasks/subagent-final-security-invariants.md`

## 已发现问题

- 工作区切换前原实现只取消运行中任务，但未阻止已登记 Worktree 被带入新工作区；新工作区仍可能通过 `list/apply/discard_worktree` 控制旧仓库。
- `_prepare_subagent_execution` 原先在 Plugin/Fork 上下文冻结前创建 Worktree；冻结失败会留下无可执行任务的 Worktree。
- 归档/恢复父 Session 原先未取消旧 Session 的后台 SubAgent，任务可能在 Session 所有权切换后继续运行。

## 安全不变量证据

- 无限递归：配置固定 `max_depth=1`；任务字段不能注入 tools/skills/mcpServers/permissionMode；read_only、verify、standard 均不向子 Agent 暴露 `subagent`。
- 权限扩大：工具集合始终取 Host profile 与定义白名单交集；Plugin guard 只能拒绝；MCP/Skill/Memory 写控制面不进入子工具；standard 与 Worktree 仍受显式开关、审批和路径边界约束。
- 信息泄露：任务错误、Session/SSE/API、恢复快照和 artifact 使用安全投影与脱敏；新增统一 Runtime reasoning 测试证明隐藏 reasoning 不进入 final_text、summary 或 artifact。
- 生命周期残留：父取消/关闭/工作区切换/Session 归档与恢复均取消子任务和审批；待处理 Worktree 阻止跨工作区切换；上下文冻结失败不创建 Worktree，登记失败立即清理 Git 资源。

## 验证

- Red：未修复代码下，Worktree 跨工作区切换、Plugin context 失败回滚、父 Session 恢复取消三个测试稳定失败。
- Green：Worktree 创建/登记/切换、Session 恢复取消与归档超时保护、隐藏 reasoning 公开结果隔离测试全部通过。
- 安全定向回归：SubAgent + Worktree + Workspace + Session + API/TUI 共 261 项通过。
- 全量：`python -m unittest discover -s tests -q`，596 项通过。
- 编译：`python -m compileall -q omnicrawl main.py tests` 通过。
- 差异：`git diff --check` 通过。

## 残余限制

- 后台批次 `fail_fast`、`AgentDefinition.skills` 和 `mcpServers` 仍按设计显式拒绝，不属于本轮安全缺陷。
- 应用关闭时不会自动删除尚未处理的 Worktree，以避免丢失用户改动；工作区切换会强制用户先 apply/discard。外部只读 scout 因 180 秒超时未返回，未作为验收证据。
