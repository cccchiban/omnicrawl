# 任务：OmniCrawl SubAgent MCP / Skill / Memory 边界回归

状态：已完成
创建：2026-07-17
更新：2026-07-17

## 需求摘要

- 继续落实 `docs/SUBAGENT_DESIGN.md` 最终验收中的边界回归要求。
- 补齐 MCP 工具不可误放行、AgentDefinition 不可自行开放 MCP/Skill、Memory 只读能力不可扩大为写入的专项测试。
- 验证 fresh 子任务不继承父 Agent 的 SkillManager 或活动 Skill；Fork 仅使用创建时冻结且已脱敏的公开上下文。
- 若测试暴露实现缺陷，执行最小修复；完成后同步 README 与设计实施状态。

## 关键决策

- 复用现有 `test_subagent_coordinator.py`、`test_subagent_integration.py` 和 `test_subagent_fork.py`，避免为单一权限矩阵新增碎片化测试模块。
- 使用伪工具、Fake Runtime 和临时目录验证边界，不依赖真实 MCP Server、Memory 数据、Skill 仓库、网络或模型凭据。
- 只做权限与上下文隔离回归，不在本轮开放 `AgentDefinition.skills` 或 `mcpServers` 能力。

## 实现计划

- [x] 1. 盘点 MCP / Skill / Memory 的现有权限过滤与上下文构造路径。
- [x] 2. 添加专项回归测试，并先验证当前实现是否存在缺口。
- [x] 3. 对暴露的问题实施最小修复，运行定向边界验证。
- [x] 4. 运行全量验证，更新设计稿、README、文档索引与任务记录并归档。

## 已修改文件

- `tests/test_subagent_coordinator.py`
- `tests/test_subagent_integration.py`
- `tests/test_subagent_fork.py`
- `README.md`
- `docs/README.md`
- `docs/SUBAGENT_DESIGN.md`
- `.codex/tasks/subagent-mcp-skill-memory-boundaries.md`

## 验证

- 基线：原 SubAgent 专项 125 项通过。
- 新增测试所在模块：`python -m unittest tests.test_subagent_coordinator tests.test_subagent_integration tests.test_subagent_fork -q`，65 项通过。
- 跨边界定向回归：SubAgent + MCP + Skill 上下文 + Memory + Plugin + Session + API + TUI 共 336 项通过。
- 全量：`python -m unittest discover -s tests -q`，590 项通过。
- 编译：`python -m compileall -q omnicrawl main.py tests` 通过。
- 差异与编码：`git diff --check`、本轮 Python/任务文件 UTF-8 BOM 与行尾空白检查通过；既有 CRLF→LF 提示不影响检查结果。
- Quick review：本地按权限扩大、上下文串扰、测试脆弱性和文档过度宣称四类检查，未发现明确问题。外部 reviewer 连续 180 秒超时，未将其作为验证证据。
- 新增用例未暴露产品实现缺陷，因此本轮无需修改生产代码；现有固定 profile 交集、任务字段白名单和 fresh/Fork 上下文构造已满足设计边界。

## 残余风险

- 本轮验证的是 Host 当前明确不开放 MCP/Skill 写能力的边界；未来若正式开放 `AgentDefinition.skills` 或 `mcpServers`，必须新增风险等级、审批与真实适配契约测试，不能仅放宽当前集合。
- 设计稿最终仍保留“无限递归、权限扩大、隐藏推理泄露、跨工作区残留任务”统一安全不变量审计，未在本轮提前勾选。
