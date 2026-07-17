# 任务：OmniCrawl SubAgent 四种 Provider Fake Runtime 契约

状态：已完成
创建：2026-07-15
更新：2026-07-15

## 需求摘要

- 完成 `docs/SUBAGENT_DESIGN.md` Phase 1 验收中唯一未勾选的“四种 Provider 的 Fake Runtime 契约测试”。
- 仅补充 SubAgent 经统一 `ModelRuntimeManager` 执行时的协议回归，不接入真实 SDK、网络或新增依赖。
- 覆盖 OpenAI Chat Completions、OpenAI Responses、Anthropic Messages、Gemini Generate Content 四种协议身份。

## 关键决策

- 在 `LocalToolAgent._execute_subagent_task()` 的集成边界注入 Fake Runtime，而非测试真实 Provider SDK 的事件对象；这与设计稿“Provider 无关的统一 Runtime”边界一致。
- 每种 Provider 身份共享同一组契约：fresh 子任务文本/用量回传、工具调用往返、取消回调下传与异常传播。
- 测试使用临时工作区、`ModelRuntimeManager` 与本地 Fake Runtime，不读取密钥、不访问网络，也不修改真实 Git 工作区。
- 只断言 Host 明确承诺的结构化工具结果包装和正文保留，不把内部 `ToolResult.output` 误当成模型上下文的直接透传值。

## 实现计划

- [x] 1. 在 SubAgent 集成测试中新增可记录请求、脚本化响应和取消信号的 Fake Runtime 夹具。
- [x] 2. 对四种 Provider 协议身份执行文本/用量、工具往返和取消契约测试。
- [x] 3. 更新设计验收项，并运行定向、全量、编译和 diff 检查。

## 已修改文件

- `tests/test_subagent_integration.py`
- `docs/SUBAGENT_DESIGN.md`
- `.codex/tasks/subagent-provider-fake-runtime-contracts.md`（本记录，归档后移入 `archived/`）

## 验证

- `python -m unittest tests.test_subagent_integration -v`：16 tests passed。
- `python -m unittest tests.test_agent_execution tests.test_subagent_config tests.test_subagent_definitions tests.test_subagent_coordinator tests.test_subagent_tasks tests.test_subagent_integration tests.test_model_runtime -q`：85 tests passed。
- `python -m unittest discover -s tests -q`：512 tests passed。
- `python -m compileall -q omnicrawl main.py tests`：通过。
- `git diff --check`：通过。

## 自审与残余风险

- 自审未发现本轮测试/文档修改的明确阻塞问题。
- Fake Runtime 验证的是 SubAgent 与统一 Runtime 的协议边界；各厂商真实 SDK 事件形态仍由 Provider Adapter 独立回归负责，未在本轮重复模拟。
- 一项独立只读审查因子代理运行时回合上限结束，未产生可采纳结论；没有修改任何项目文件。
