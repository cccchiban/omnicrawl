# 任务：TUI 上下文长度设置

状态：已完成
创建：2026-07-18
更新：2026-07-18

## 需求摘要

- 在全屏 TUI `/settings` 面板增加“上下文长度”设置，界面单位为十进制 K。
- 设置作用于当前活动模型。
- 使用左右键在 32K、64K、128K、256K、512K、1024K、2048K 预设档位间切换并立即应用。

## 关键决策

- 自定义模型写回 `models.yaml` 当前 `catalog_key` 的 `context_window_tokens`。
- 非自定义模型没有独立模型记录，回退写入 `config.yaml` 的 `llm.defaults.context_window_tokens`（旧式单模型配置写 `llm.context_window_tokens`）。
- 运行时 setter 与持久化任一步失败都恢复原值，避免界面、内存和配置产生半生效状态。
- K 按 1000 Token 计算，即 128K = 128000 Token。

## 实现计划

- [x] 1. 定位设置面板、模型配置优先级和持久化链路
- [x] 2. 增加上下文窗口持久化与 Agent 运行时 setter
- [x] 3. 增加 TUI 预设档位行和即时应用逻辑
- [x] 4. 补充配置、Agent、TUI 和回滚测试
- [x] 5. 同步文档并完成定向/全量验证

## 已修改文件

- `.codex/tasks/tui-context-window-setting.md`
- `omnicrawl/config/llm.py`
- `omnicrawl/llm/__init__.py`
- `omnicrawl/agent/core.py`
- `omnicrawl/ui/fullscreen/settings.py`
- `tests/test_settings_panel.py`
- `tests/test_fullscreen_tui.py`
- `README.md`

## 验证

- 上下文相关和设置面板定向测试：64 项通过。
- `python -m compileall -q omnicrawl tests`：通过。
- `git diff --check`：通过。
- 全量测试：`python -m unittest discover -s tests`，637 项通过。
