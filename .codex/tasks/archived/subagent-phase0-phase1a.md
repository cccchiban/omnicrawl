# 任务：OmniCrawl SubAgent Phase 0 + Phase 1A

状态：已完成
创建：2026-07-14
更新：2026-07-14

## 需求摘要

- 依据 `docs/SUBAGENT_DESIGN.md` 开始实现 SubAgent。
- 本轮完成 Phase 0：抽取内部 `AgentLoopRunner`，保持 `LocalToolAgent.run_stream()` 的现有外部行为、事件顺序、审批、并发屏障、Runtime 与 Plugin 生命周期不变。
- 本轮完成 Phase 1A：默认关闭的配置、完整来源的 Markdown Agent 定义注册表、内置 `explore`/`plan`、单个同步 `fresh`/`read_only` 子任务及统一 `subagent(action=run)` 工具。
- 本轮不实现批量并发、后台任务、Session 子任务事件、API/TUI 子任务进度、Fork、写能力和 verify Agent。

## 关键决策

- 主 Agent Phase 0 不启用默认循环上限；Runner 作为内部 API，不加入稳定公共导出。
- 工具执行使用批次回调，保留“整批先审批、读工具并行、写/删除屏障串行、结果原序回填”契约。
- Phase 1A 单次只允许一个任务；配置保留设计稿的 `max_concurrency=2`，但本轮不执行 fan-out。
- Agent 定义来源按优先级支持：项目级、兼容项目级、用户级、内置级、插件级；项目越近优先级越高。
- 内置 `explore`/`plan` 使用包内 Markdown 资源，并同步 Python 打包配置。
- 子任务只允许 `fresh` 上下文和 read-only 工具交集，禁止 `subagent`、写文件、命令、monitor 和 Memory 写。
- `subagents.enabled=false` 时不注册 `subagent` 工具，不改变现有工具 Schema 和行为。
- 当前工作树以提交 `04bbd4f docs: add subagent architecture design` 为干净基线。

## 实现计划

- [x] 1. 新增 `AgentLoopRunner` 的 Red 测试并抽取主循环
- [x] 2. 验证主 Agent 审批、并发屏障、Session、Runtime 与 Plugin 生命周期保持不变
- [x] 3. 新增 SubAgent 配置模型、环境变量收紧语义和加载测试
- [x] 4. 新增 Markdown Agent 定义、完整来源注册表、碰撞诊断和打包资源测试
- [x] 5. 新增单任务同步 `SubAgentCoordinator`、read-only 工具过滤与隔离测试
- [x] 6. 条件注册统一 `subagent` 工具，并接入 TUI/API 默认 Agent 构造路径
- [x] 7. 更新示例配置、项目文档与实现进度
- [x] 8. 运行定向测试、全量测试、compileall、diff-check 和 quick review

## 已修改文件

- `.codex/tasks/subagent-phase0-phase1a.md`
- `omnicrawl/agent/execution.py`
- `omnicrawl/agent/core.py`
- `tests/test_agent_execution.py`
- `omnicrawl/config/subagents.py`
- `omnicrawl/agent/subagents/__init__.py`
- `omnicrawl/agent/subagents/definitions.py`
- `omnicrawl/agent/subagents/coordinator.py`
- `omnicrawl/agent/subagents/builtin/explore.md`
- `omnicrawl/agent/subagents/builtin/plan.md`
- `omnicrawl/agent/tools.py`
- `omnicrawl/entry.py`
- `omnicrawl/api/app.py`
- `omnicrawl/extensions/plugin_models.py`
- `omnicrawl/extensions/plugin_manager.py`
- `pyproject.toml`
- `setup.cfg`
- `config.example.yaml`
- `config.example.json`
- `README.md`
- `docs/README.md`
- `docs/SUBAGENT_DESIGN.md`
- `tests/test_subagent_config.py`
- `tests/test_subagent_definitions.py`
- `tests/test_subagent_coordinator.py`
- `tests/test_subagent_integration.py`
- `tests/test_plugin_manifest.py`
- `tests/test_plugin_manager.py`
- `tests/test_runtime_config.py`
- `tests/test_agent_module_boundaries.py`

## 验证

- 开始前基线：`python -m unittest discover -s tests -q`，415 tests passed。
- Phase 0 定向验证：`python -m unittest tests.test_agent_execution tests.test_agent_context tests.test_agent_hooks tests.test_approval tests.test_model_runtime -q`，79 tests passed。
- Phase 0：`python -m compileall -q omnicrawl tests` 与 `git diff --check` 通过。
- Phase 0 + 1A 定向验证：134 tests passed。
- 首轮全量回归：448 tests passed；审查修复和补测后最终全量回归：`python -m unittest discover -s tests -q`，456 tests passed。
- `python -m compileall -q omnicrawl main.py tests`、`python -m json.tool config.example.json`、`git diff --check` 通过。
- 安装包资源：`python setup.py bdist_wheel` 成功，wheel 内含 `explore.md` 与 `plan.md`；临时 build、egg-info 和 wheel 已清理。
- 旧版 pip 在中文 Windows 路径执行 `pip wheel` 时因 `nul` 路径复制缺陷失败，已切换 setuptools 原地构建完成同等验证。
- Quick review 首轮发现：取消异常可能被吞、项目定义符号链接逃逸、定义资源无上限、超时语义不准确；均已修复或明确边界，并增加取消传播、项目/插件 symlink containment、文件/正文/列表上限、模型请求 timeout 收紧和 subagent 串行屏障测试。
- 复核结论：未发现 blocker/high；最终 `compileall`、JSON 校验、`git diff --check` 通过，暂存区为空。

## 残余风险

- `default_timeout_seconds` 是模型/工具调用边界预算，并会收紧单次模型请求 timeout；无法强制中断不响应取消的第三方 SDK 或系统调用。
- 本轮按确认范围只交付单个同步 fresh/read-only 任务。批量并发、Session 生命周期事件、API/TUI 子任务进度、artifact、后台任务、Fork、verify 与写能力留待后续 Phase 1B/1C、Phase 2/3。
