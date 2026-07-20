# 任务：上下文压缩第一阶段测量模式

状态：已完成
创建：2026-07-20
更新：2026-07-20

## 需求摘要

- 按 `docs/context_compaction_cost_optimization_design.md` 第一阶段实现上下文 Token 测量。
- 不调用摘要模型，不改变现有 `_history` 和确定性自动压缩行为。
- 功能默认关闭；显式开启后在完整回合结束时记录预算快照和模拟压缩收益。

## 关键决策

- 首批范围仅为阶段一（用户确认 `1A`）。
- 最近原文窗口沿用当前 6 轮（用户确认 `2A`）。
- 普通 `/compact` 保持确定性；未来模型入口默认同供应商且禁止跨供应商（用户确认 `3A`，本阶段不接模型）。
- 测量结果追加为 Session 诊断事件，不进入模型上下文投影。

## 实现计划

- [x] 1. 增加配置、预算模型、Token 估算与测量账本
- [x] 2. 在完整回合结束点做薄集成，并保持确定性压缩不变
- [x] 3. 增加边界、集成和模块约束测试，同步配置示例
- [x] 4. 执行定向测试、全量回归和静态检查

## 已修改文件

- `.codex/tasks/context-compaction-measurement.md`
- `config.example.yaml`
- `omnicrawl/config/context_compaction.py`
- `omnicrawl/agent/context_compaction/__init__.py`
- `omnicrawl/agent/context_compaction/models.py`
- `omnicrawl/agent/context_compaction/policy.py`
- `omnicrawl/agent/context_compaction/ledger.py`
- `omnicrawl/agent/context_compaction/service.py`
- `omnicrawl/agent/core.py`
- `tests/test_context_compaction_policy.py`
- `tests/test_context_compaction_integration.py`
- `tests/test_context_compaction_module_boundaries.py`

## 验证

- Red：新增测试初次收集失败，缺少 `omnicrawl.agent.context_compaction` 与 `omnicrawl.config.context_compaction`，符合预期。
- 定向测试：`python -m pytest tests/test_context_compaction_policy.py tests/test_context_compaction_integration.py tests/test_context_compaction_module_boundaries.py -q`，9 项通过。
- 全量回归：`python -m pytest tests -q`，664 项通过。
- 语法检查：`python -m compileall -q omnicrawl/agent/context_compaction omnicrawl/config/context_compaction.py omnicrawl/agent/core.py`，通过。
- 差异检查：`git diff --check`，通过。
- Quick review：默认关闭、纯测量、事件顺序、确定性压缩兼容和模块边界通过；补充测量异常降级测试。子代理报告中的一项 Important 引用了当前代码不存在的 API，经本地源码复核后判定为上下文漂移，未据此误改。

## 残余范围

- 本任务只完成设计第一阶段，不调用摘要模型。
- 70K 自动模型压缩、4 回合冷却、85% 紧急旁路、ROI 金额和按需证据恢复仍属于后续阶段。
- 当前 Token 估算为无依赖启发式算法；测量事件同时保存供应商实际 Usage，供后续误差校准。
