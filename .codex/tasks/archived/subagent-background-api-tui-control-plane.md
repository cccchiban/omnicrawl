# 任务：SubAgent 后台任务 API/TUI 控制面

状态：已完成
创建：2026-07-15
更新：2026-07-15

## 需求摘要

- 补齐当前服务或 Session 范围内 SubAgent 后台任务的只读查询与取消控制面。
- 提供顶层 HTTP API、斜杠命令和 TUI 状态展示。
- 不开放远程创建任务、不引入 ApprovalBroker、不启用命令执行型 `verify`。

## 关键决策

- 在 `SubAgentCoordinator` 增加 owner/session 范围的查询与取消门面；`LocalToolAgent` 只向当前 `current_session_id` 委托，避免 API/TUI 接触 TaskManager 私有字段或绕过隔离。
- HTTP 仅新增 `GET /api/v1/subagents`、`GET /api/v1/subagents/{task_id}`、`POST /api/v1/subagents/{task_id}/cancel`；不提供远程创建入口。未启用功能返回 `SUBAGENT_UNAVAILABLE`/503，当前会话外或不存在的任务统一映射为 `SUBAGENT_NOT_FOUND`/404。
- `/tasks`、`/task <task_id>`、`/task cancel <task_id>` 共用 `slash.py` 处理器，因此终端与全屏 TUI 行为一致；TUI 仅展示安全 task 元数据与状态，不展示 prompt 或原始结果。
- API 取消只提交合作式取消请求并立即返回 `cancelling`，不调用生命周期层的有界 `cancel_and_wait()`。
- OpenAPI 的精确操作集合是 API 模块边界测试的一部分；新增路由后同步更新模块与操作契约，避免接口文档、实际路由与回归测试漂移。

## 实现计划

- [x] 1. 梳理 TaskManager、Agent、API、斜杠命令和全屏 TUI 的现有控制边界与测试模式。
- [x] 2. 添加最小 Agent/API 读写接口及 API 路由，保留 owner/session 隔离和取消语义。
- [x] 3. 添加 `/tasks`、`/task <id>`、`/task cancel <id>` 与 TUI 状态展示接线。
- [x] 4. 覆盖成功、未找到、跨会话隔离、取消和 UI/命令行为，并完成回归验证。

## 已修改文件

- `omnicrawl/agent/subagents/coordinator.py`
- `omnicrawl/agent/core.py`
- `omnicrawl/api/routes/subagents.py`（新增）
- `omnicrawl/api/routes/__init__.py`
- `omnicrawl/commands/slash.py`
- `omnicrawl/ui/chat_session.py`
- `omnicrawl/ui/fullscreen/commands.py`
- `omnicrawl/ui/fullscreen/__init__.py`
- `docs/API.md`
- `docs/SUBAGENT_DESIGN.md`
- `tests/test_subagent_coordinator.py`
- `tests/test_subagent_integration.py`
- `tests/test_api.py`
- `tests/test_api_module_boundaries.py`
- `tests/test_agent_context.py`
- `tests/test_fullscreen_commands.py`
- `tests/test_fullscreen_tui.py`
- `.codex/tasks/subagent-background-api-tui-control-plane.md`（本记录，归档后移入 `archived/`）

## 验证

- `python -m unittest tests.test_subagent_coordinator tests.test_subagent_integration tests.test_api tests.test_agent_context tests.test_fullscreen_commands tests.test_fullscreen_tui -q`：148 tests passed。
- `python -m unittest tests.test_api_module_boundaries.APIModuleBoundaryTests.test_api_submodules_are_real_files tests.test_api_module_boundaries.APIModuleBoundaryTests.test_openapi_contract_keeps_expected_operations -v`：2 tests passed。
- `python -m unittest discover -s tests -q`：518 tests passed。
- `python -m compileall -q omnicrawl main.py tests`：通过。
- `git diff --check`：通过。

## 自审与残余风险

- 独立只读审查未发现阻塞问题；确认 API/TUI 没有可调用的 owner/session 参数或远程创建入口，取消路径不等待子任务退出，公开字段不包含 prompt。
- 全量回归首次发现 OpenAPI 精确操作集合遗漏新路由；已按现有模块边界模式补齐并以专项测试和全量测试复验。
- 本轮不实现 ApprovalBroker、`verify` 命令 profile、Fork、模型覆盖或共享工作区写能力。
- Provider SDK 的真实流事件仍由独立 Adapter 回归覆盖；本轮关注统一 Runtime 和控制面边界。
