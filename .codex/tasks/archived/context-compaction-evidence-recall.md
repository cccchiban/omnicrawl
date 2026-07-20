# 任务：上下文压缩按需证据恢复

状态：已完成
创建：2026-07-20
更新：2026-07-20

## 需求摘要

- 新增模型只读工具 `recall_session_evidence`。
- 工具仅能读取当前 Session 且由当前有效 `compact_summary` 引用的事件与 artifact。
- 单次最多 8 个引用，合计输出预算约 4,000 Token；超出时返回截断诊断并允许分批读取。
- 缺失、越界或不可读证据返回结构化诊断，不阻断当前模型回合。
- 文本 artifact 可读取；二进制 artifact 只返回安全元数据和路径。
- 不默认恢复整个冷历史，不允许跨 Session 读取。

## 关键决策

- 恢复入口选择模型只读工具，不自动注入，也不新增手动 Slash 命令。
- 模型只能传事件 ID；artifact 路径必须从获准事件 payload 中派生，不能信任模型任意传入路径。
- 授权集合每次调用都从当前有效事件流中的最后一个 `compact_summary` 重建，`/undo` 后不会继续使用失效摘要。
- 工具最多接受 8 个唯一事件 ID，总输出按约 4,000 Token 预算截断。
- 所有失败均作为工具结果中的逐项结构化诊断返回，不抛出阻断回合的异常。
- 完整 Session JSONL 继续作为事实源；运行时 `_history` 只接收本次工具调用返回的相关证据。
- 工具仅在 Session 可用且 `context_compaction.enabled=true` 时注册，不下放给 SubAgent 的只读工具集合。
- 事件与 artifact 读取绑定工具调用开始时捕获的同一 `store/state`，避免 Session 切换期间混用授权集合。

## 实现计划

- [x] 1. 定义证据恢复数据契约、授权集合和预算算法
- [x] 2. 增加失败测试：未授权、跨 Session artifact、超项、预算截断和二进制元数据
- [x] 3. 实现当前 Session 证据读取服务
- [x] 4. 注册 `recall_session_evidence` 只读工具并集成当前有效摘要
- [x] 5. 补充恢复、滚动摘要和工具回合集成验证
- [x] 6. 更新设计文档、执行全量回归与 Quick Review

## 已修改文件

- `.codex/tasks/archived/context-compaction-evidence-recall.md`
- `.codex/tasks/archived/context-compaction-model-auto.md`
- `config.example.yaml`
- `docs/context_compaction_cost_optimization_design.md`
- `omnicrawl/config/context_compaction.py`
- `omnicrawl/agent/context_compaction/evidence.py`
- `omnicrawl/agent/context_compaction/__init__.py`
- `omnicrawl/agent/tools.py`
- `omnicrawl/agent/types.py`
- `omnicrawl/agent/core.py`
- `tests/test_context_compaction_evidence.py`
- `tests/test_context_compaction_policy.py`
- `tests/test_context_compaction_summary.py`
- `tests/test_context_compaction_integration.py`
- `tests/test_workspace_switch.py`

## 验证

- Red：`python tests/test_context_compaction_evidence.py` 因公共证据恢复接口不存在而失败。
- 证据恢复测试：`python -m pytest tests/test_context_compaction_evidence.py -q`，13 项通过。
- 四阶段定向验收：`python -m pytest tests/test_context_compaction_policy.py tests/test_context_compaction_summary.py tests/test_context_compaction_evidence.py tests/test_context_compaction_integration.py tests/test_context_compaction_module_boundaries.py -q`，51 项通过。
- 补充覆盖：非法 JSON 重试、超大事件分块合并、结构化校验重试后降级、普通 `/compact` 确定性 Slash 路径、四完整回合冷却、真实摘要 Runtime 成功返回、模型/上下文窗口切换校验和工作区切换缓存刷新。
- 全量回归：`python -m pytest tests -q`，707 项通过。
- 编译检查：`python -m compileall -q omnicrawl/agent/context_compaction omnicrawl/config/context_compaction.py omnicrawl/agent/core.py omnicrawl/agent/tools.py omnicrawl/agent/types.py`，通过。
- 差异检查：`git diff --check`，通过。
- 文件边界：context_compaction 生产文件均低于 800 行，`evidence.py` 为 386 行。
- 初次 Quick Review：审查子代理超时；父 Agent 自审发现并修复 Session 切换时事件读取与 artifact 读取可能绑定不同 store/state 的边界。
- 验收 Review：初次无 Blocker，并指出冷却路径和普通 `/compact` 独立断言不足，已补充对应集成测试。
- 提交前 Review：发现并修复真实摘要 Runtime 缺少 `SummaryModelResponse` 导入、模型切换未重新校验压缩窗口、摘要 Service 沿用旧模型，以及工作区切换后沿用旧缓存身份四项问题；均先以失败测试复现再修复。
- 全量回归期间出现过无关 TUI 异步时序失败；失败用例独立运行通过，未修改 UI；最终全量重新运行 707 项通过。

## 残余风险

- 不执行真实摘要模型调用；模型是否调用证据工具取决于摘要来源 ID 和当前问题是否需要精确细节。
- 证据恢复不会自动注入整个冷历史；超过单次预算时需要模型按更少事件 ID 分批调用。
