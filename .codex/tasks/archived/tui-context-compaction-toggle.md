# 任务：TUI 上下文压缩开关

状态：已完成
创建：2026-07-21
更新：2026-07-21

## 需求摘要

- 阅读上下文压缩设计基线，在全屏 TUI `/settings` 面板增加模型辅助上下文压缩总开关。
- 开关即时修改运行态并持久化 `context_compaction.enabled`，默认行为保持关闭。

## 关键决策

- 只开放设计文档定义的 `enabled` 总开关，不在紧凑设置面板暴露阈值、冷却期或跨供应商等高级参数。
- 启用时复用已有上下文窗口安全校验；校验失败时保留旧运行态并回滚配置写入。
- 切换后重建 Agent 工具表，使 `recall_session_evidence` 与开关状态同步。

## 实现计划

- [x] 1. 阅读设计文档并定位配置、Agent 和 TUI 设置链路
- [x] 2. 增加设置页、持久化和运行时切换回归测试
- [x] 3. 实现开关、Agent setter 与工具表同步
- [x] 4. 同步项目文档并完成定向、全量验证和自审

## 已修改文件

- `.codex/tasks/archived/tui-context-compaction-toggle.md`
- `omnicrawl/ui/fullscreen/settings.py`
- `omnicrawl/agent/core.py`
- `tests/test_settings_panel.py`
- `tests/test_fullscreen_tui.py`
- `README.md`
- `docs/README.md`
- `docs/TERMINAL_UI.md`
- `docs/context_compaction_cost_optimization_design.md`

## 验证

- 设置页与上下文压缩定向回归：`python -m pytest tests/test_settings_panel.py tests/test_fullscreen_tui.py tests/test_context_compaction_policy.py tests/test_context_compaction_integration.py tests/test_context_compaction_evidence.py -q`，107 项通过。
- 可见性修复回归：`python -m pytest tests/test_settings_panel.py tests/test_fullscreen_tui.py tests/test_terminal_ui.py tests/test_inline_input.py -q`，124 项通过。
- 全量回归：`python -m pytest tests -q`，712 项通过。
- 语法检查：`python -m compileall -q omnicrawl tests/test_settings_panel.py tests/test_fullscreen_tui.py`，通过。
- 差异检查：`git diff --check`，通过。
- UI 验证：Textual 测试在 `100x32` 终端打开设置面板，确认 9 行完整渲染且“上下文压缩：已关闭”可见。
- 独立审查：未发现阻断问题；补齐关闭时移除证据工具、工具表重建失败回滚两条回归测试。

## 可见性缺陷修复

- 现象：设置面板已创建“上下文压缩”末行，但标准 `100x32` 终端中的列表可视高度只有 16 行，9 个双行设置项需要 18 行，末行被裁到视口外。
- 修复：设置列表改用 `VerticalScroll`，标准高度补足列表空间；方向键改变选中项时调用 `scroll_visible`，确保矮终端也能访问末尾开关。
- Red：标准尺寸下末行底部为 24、列表底部为 22，可见性断言失败。
- Green：`100x32` 打开后末行直接可见；`100x24` 选中末项后自动滚入视口。

## 方向键交互缺陷修复

- 现象：引入 `VerticalScroll` 后，上下方向键被获得焦点的滚动容器优先消费，`SettingsScreen.action_move_up/action_move_down` 未执行。
- Red：在 `100x24` 终端发送 8 次真实 `Down` 后，选中项仍为 `model`，而不是 `context_compaction`。
- 修复：将设置面板的 `Up/Down` 声明为高优先级 `Binding`；列表滚动继续由选中行的 `scroll_visible` 驱动。
- Green：真实 `Down` 可移动到“上下文压缩”，真实 `Up` 可返回“子任务功能”；相关测试 124 项、全量测试 712 项通过。
