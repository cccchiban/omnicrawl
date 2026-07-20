# 任务：上下文压缩模型摘要与自动策略

状态：已完成
创建：2026-07-20
更新：2026-07-20

## 需求摘要

- 在第一阶段测量模式上实现设计第二、三阶段。
- 新增结构化模型摘要、Schema/来源/精确证据校验和确定性降级。
- 保留普通 `/compact` 的确定性行为，新增 `/compact --model`。
- 功能开启后由新 service 接管自动压缩：预计下一请求达到 70,000 Token 时批量压缩，冷却 4 个完整回合，85% 容量时可绕过冷却。
- 支持没有旧冷历史时的单回合大型载荷摘要。

## 关键决策

- 用户确认范围为第二、三阶段（1B）。
- `/compact` 保持确定性，新增 `/compact --model`（2A）。
- 摘要模型默认复用当前供应商；`summary_profile` 可指定 models.yaml key/alias、模型 ID 或 `profile/model_id`；跨供应商默认禁止（3A）。
- 不执行真实付费请求，使用测试替身验证模型调用边界。
- 完整 JSONL 始终保留，结构化摘要只改变活动模型投影。
- 自动模型失败也建立 4 回合冷却边界，避免每回合连续重试。
- 用户后续确认不实施 Profile 价格、金额 ROI、回本报告或阈值校准阶段；原第五阶段“按需证据恢复”调整为第四阶段。

## 实现计划

- [x] 1. 增加摘要数据模型、提示词、模型调用器和质量校验
- [x] 2. 增加批次选择、70K/冷却/紧急旁路与单回合策略
- [x] 3. 集成 `/compact --model`、同供应商 Profile 解析和 Session v2 事件
- [x] 4. 让开启状态接管自动压缩，失败时安全降级
- [x] 5. 补齐恢复、滚动摘要、打包、模块边界与集成测试
- [x] 6. 执行定向测试、全量回归和 Quick Review

## 已修改文件

- `.codex/tasks/context-compaction-model-auto.md`
- `config.example.yaml`
- `docs/context_compaction_cost_optimization_design.md`
- `omnicrawl/config/context_compaction.py`
- `omnicrawl/agent/context_compaction/__init__.py`
- `omnicrawl/agent/context_compaction/models.py`
- `omnicrawl/agent/context_compaction/policy.py`
- `omnicrawl/agent/context_compaction/summary.py`
- `omnicrawl/agent/context_compaction/summary_prompt.md`
- `omnicrawl/agent/context_compaction/validation.py`
- `omnicrawl/agent/context_compaction/projection.py`
- `omnicrawl/agent/context_compaction/ledger.py`
- `omnicrawl/agent/context_compaction/service.py`
- `omnicrawl/agent/core.py`
- `omnicrawl/commands/slash.py`
- `pyproject.toml`
- `setup.cfg`
- `tests/test_context_compaction_policy.py`
- `tests/test_context_compaction_summary.py`
- `tests/test_context_compaction_integration.py`
- `tests/test_context_compaction_module_boundaries.py`

## 验证

- 定向测试：`python -m pytest tests/test_context_compaction_policy.py tests/test_context_compaction_summary.py tests/test_context_compaction_integration.py tests/test_context_compaction_module_boundaries.py -q`，28 项通过。
- 全量回归：`python -m pytest tests -q`，683 项通过。
- 语法检查：`python -m compileall -q omnicrawl/agent/context_compaction omnicrawl/config/context_compaction.py omnicrawl/agent/core.py omnicrawl/commands/slash.py`，通过。
- 差异检查：`git diff --check`，通过。
- wheel：使用本机已安装的 setuptools/wheel 离线构建，安装到临时目录后通过 `importlib.resources` 读取 `summary_prompt.md`，验证通过；临时构建产物已清理。
- Quick Review：修复滚动压缩遗漏上次 recent events 的边界；补充真实 `resume_session` 保留摘要测试；补充自动模型失败冷却，避免连续重试。

## 残余范围

- 本阶段持久化的 `covered_event_ids`、`remaining_event_ids` 和 exact evidence 来源已由后续任务 `context-compaction-evidence-recall` 用于第四阶段按需证据恢复。
- 不规划 Profile 价格、金额 ROI、回本报告或阈值校准能力。
- 未执行真实摘要模型请求；生产使用前应先在测试 Profile 上显式启用并观察测量事件。
