# TUI 中文设置面板

状态：已完成

## 目标

新增仅支持 `/settings` 的 Textual 设置面板，集中修改模型、推理强度、工具审批模式、Memory、MCP、Plugin 和 SubAgent。所有选项使用中文；修改后即时生效并持久化到项目配置。

## 已确认需求

- 命令仅为 `/settings`，不提供 `/setting` 别名。
- 设置范围：模型、推理强度、审批模式、Memory、MCP、Plugin、SubAgent。
- 保存策略：选择后立即应用，并写入项目配置。
- 复用现有模型选择器及现有配置/生命周期能力，不引入新依赖。

## 实现计划

- [x] 1. 确认需求与现有命令/TUI 入口。
- [x] 2. 盘点各设置的持久化键和运行时切换边界。
- [x] 3. 实现配置保存与 Agent 即时应用接口。
- [x] 4. 实现中文设置 ModalScreen 和 `/settings` 分派。
- [x] 5. 补充命令、设置应用和 TUI 交互测试。
- [x] 6. 完成定向/全量验证、文档同步和任务归档。

## 预计修改

- `omnicrawl/commands/slash.py`
- `omnicrawl/ui/fullscreen/commands.py`
- `omnicrawl/ui/fullscreen/__init__.py`
- `omnicrawl/ui/fullscreen/settings.py`（新增）
- `omnicrawl/agent/core.py`
- `omnicrawl/config/settings.py`
- `omnicrawl/config/` 下相关配置持久化模块
- `tests/` 下命令、TUI 与配置测试
- `README.md`、`docs/README.md`

## 风险

- Memory、MCP、Plugin、SubAgent 都持有运行时资源；切换必须遵循关闭旧资源、构造新资源、失败回滚的顺序。
- 环境变量覆盖配置时，必须明确提示当前选择可能在重启后被环境变量覆盖。

## 已实现

- 新增 `config/settings.py`，基于原子配置写回保留 YAML/JSON 其它字段。
- 新增 `SettingsScreen`，仅支持 `/settings`；`/setting` 不会被识别。
- 面板只开放安全总开关，不开放 SubAgent Worktree、共享写入或 Plugin 网络安装等高风险细项。
- Memory、MCP、Plugin、SubAgent 均有即时运行时应用入口，资源/任务失败时保留旧状态；模型复用既有 ModelPicker。
- 启动时读取 `memory.enabled`，其它已有开关继续由各自配置加载器读取。

## 验证

- `tests.test_settings_panel`：8 项通过。
- 设置命令、运行时配置和 TUI 定向测试：33 项通过。
- `test_fullscreen_tui.FullscreenTUITest.test_settings_command_opens_chinese_settings_screen`：通过。
- 初始全量：`python -m unittest discover -s tests -q`，606 项通过。
- 编译：`python -m compileall -q omnicrawl main.py tests` 通过。
- 差异：`git diff --check` 通过。

## 缺陷修复记录

- 现象：首次打开 `/settings` 时 7 个设置行均为空白。
- 根因：行组件先以空字符串创建，`on_mount` 阶段的更新发生在子组件初始化完成前，首轮内容被覆盖。
- 修复：在 `compose()` 创建行组件时直接写入当前设置文本，挂载后刷新仅负责同步动态状态。
- 回归：新增 `test_should_render_settings_rows_when_panel_opens`，修复前 7 行内容均为空，修复后全部包含当前值；全量 606 项通过。

## 实时生效审计

- 模型：设置项会打开 ModelPicker；选择后先持久化，再切换 Runtime，并刷新模型与 Token HUD。
- 推理强度：立即修改下一次模型请求读取的 `reasoning_effort`，并同步 `thinking_type`。
- 工具审批：立即修改 Agent 审批模式，后续工具调用直接读取新值。
- Memory：立即替换 MemoryStore 并重建工具表；开启/关闭均已覆盖。
- MCP：事务式替换 Manager；下一次能力预热/模型回合按新状态发现工具，关闭时回收旧 Manager。
- Plugin：通过进程级 PluginRuntime 启停 Worker，并刷新 SubAgent 插件定义。
- SubAgent：关闭前取消并等待任务退出；开启时重建 Coordinator 及 Host 工具表。
- 审计修复：补齐 SubAgent 开启后重建 Coordinator 和 Host 工具表的问题；设置面板关闭后刷新 HUD，避免推理强度和审批模式显示旧值。
- 提交前审查修复：兼容内联界面会明确提示 `/settings` 仅支持全屏 TUI，不会将命令发送给模型；插件运行中重新开启时会补发 `app.start.after` 生命周期钩子；若运行时切换失败且配置文件回滚也失败，面板会明确提示配置与运行态可能不一致。
- 验证：设置即时应用、四个功能开关双向切换、模型入口/选择、配置写回和真实 Runtime setter 相关测试共 100 项通过；提交前全量 617 项通过，`compileall` 与 `git diff --check` 通过。

## 限制

- `/settings` 只支持复数形式；`/setting` 不会触发面板。
- 面板只提供安全总开关，不开放 SubAgent Worktree、共享写入和 Plugin 网络安装等高风险细项。
- 环境变量（例如 `MCP_ENABLED`、`OMNICRAWL_SUBAGENTS_ENABLED`、`REASONING_EFFORT`）仍可在启动时覆盖配置文件；面板会修改项目配置，但不会覆盖环境变量。
