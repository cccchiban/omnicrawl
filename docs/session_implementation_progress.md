# Agent 会话系统实现进度

本文档跟踪 `docs/session_design.md` 的分阶段落地情况。状态只记录已经完成、正在实现和明确延期的内容，避免和设计文档重复。

## 当前阶段

| 项目 | 状态 | 说明 |
|------|------|------|
| 当前阶段 | 进行中 | 第一阶段：`SessionStore` + JSONL 追加写 + `session_id`；第二阶段基础 `/sessions`、`/resume`、第三阶段提示历史基础能力、第四阶段确定性会话压缩、第五阶段 Qt 会话列表与正式导出基础闭环已同步落地。 |
| 开始时间 | 2026-06-16 | 按会话设计文档从最小可用闭环开始实现。 |
| 当前目标 | 进行中 | 会话能自动创建、记录转录、列出最近会话、记录提示历史，通过 `/resume` 恢复 `_history`，在长会话中通过 `compact_summary` 保留早期上下文摘要，并在 Qt GUI 中完成会话列表、恢复、重命名、压缩和正式导出。 |

## 阶段清单

| 阶段 | 内容 | 状态 | 验收标准 |
|------|------|------|----------|
| 第一期 | `SessionStore`、`.agent_sessions/index.json`、`sessions/<id>.jsonl`、自动 `session_id` | 已完成 | 每轮对话生成可读 JSONL，索引随事件更新。 |
| 第二期 | `/sessions`、`/resume <session_id>`、恢复 `_history` | 已完成 | 可以列出当前项目会话，恢复后继续追问能看到旧上下文。 |
| 第三期 | 提示历史 `history.jsonl` | 已完成 | 用户输入历史可按当前项目复用，不自动进入模型上下文。 |
| 第四期 | `compact_summary` 长会话压缩 | 已完成 | 超过历史上限时生成摘要并保留任务主线。 |
| 第五期 | Qt 会话列表与正式导出 | 已完成 | GUI 可恢复、重命名、压缩、导出当前会话。 |
| 第六期 | 大工具输出 artifact、脱敏、归档 | 未开始 | 大输出不拖慢恢复，敏感内容可控。 |

## 本轮实现记录

| 时间 | 进展 | 结果 |
|------|------|------|
| 2026-06-16 | 新增实现进度文档 | 已创建本文件，作为后续会话系统实现的跟踪入口。 |
| 2026-06-16 | 新增 `ai_voice_agent/session.py` | 已建立会话事件、索引、JSONL 读写和恢复数据模型。 |
| 2026-06-16 | 接入 `LocalToolAgent` | 已自动创建会话，并记录 user、assistant、tool call、tool result、审批和中断事件。 |
| 2026-06-16 | 接入 TUI / Qt 斜杠命令 | 已支持 `/sessions` 查看最近会话、`/resume <session_id>` 恢复会话上下文。 |
| 2026-06-16 | 接入提示历史 | 已新增 `.agent_sessions/history.jsonl`，记录真实用户提示并支持 `/history` 查询；TUI 上箭头历史可从持久化历史种子预热。 |
| 2026-06-16 | 更新测试 | 新增 `tests/test_session_store.py`，并扩展 Agent 上下文测试覆盖事件写入和恢复。 |
| 2026-06-16 | 运行验证 | `python -m pytest -q` 通过，结果：148 passed，1 个第三方 `pyreadline` 弃用警告。 |
| 2026-06-16 | 接入长会话压缩 | `_append_history()` 超过历史窗口时会写入 `compact_summary`，并以本地确定性摘要替代早期明细；新增 `/compact` 手动压缩命令。 |
| 2026-06-16 | 加强恢复逻辑 | `SessionStore.load_session()` 可按 `compact_summary.remaining_message_count` 恢复摘要边界和最近消息；`/resume` 会固定保留摘要并避免半轮消息开头。 |
| 2026-06-16 | 更新压缩测试 | 覆盖自动压缩事件、手动 `/compact`、摘要恢复边界和完整最近轮次窗口。 |
| 2026-06-16 | 补齐会话重命名与正式导出 | `SessionStore` 新增 `session_renamed`、`session_exported` 事件，正式导出写入 `.agent_sessions/exports/`，TUI 新增 `/rename <title>`。 |
| 2026-06-16 | 接入 Qt 会话侧边栏 | Qt GUI 侧边栏支持刷新最近会话、新对话、恢复、重命名、压缩和正式导出；恢复时按 JSONL 事件流重新渲染 user/assistant/tool/compact_summary。 |
| 2026-06-16 | 更新 Qt 与会话测试 | 覆盖重命名、正式导出、Agent 事件读取、Qt 桥接信号、前端会话回调和事件投影。 |

## 已知限制

| 限制 | 影响 | 后续处理 |
|------|------|----------|
| 第一版只恢复 user/assistant/compact_summary | 工具调用链会被记录，但暂不完整回放进 `_history`。 | 后续补 tool call 消息还原和大输出 artifact。 |
| Qt 恢复只回放已支持事件类型 | Qt 消息区可回放 user、assistant、tool call、tool result、compact_summary；审批事件暂不单独渲染。 | 后续如需审计视图，可把 tool approval/denial 渲染为系统事件卡片。 |
| 压缩摘要为本地确定性摘要 | 不调用模型生成高质量语义摘要，摘要细节弱于专门模型总结。 | 后续可引入可选 `SessionCompactor` 模型摘要，但需避免额外失败影响主链路。 |
| 第一版不做敏感信息脱敏 | 会话文件可能包含用户输入和工具输出。 | 先通过 `.gitignore` 忽略 `.agent_sessions/`，后续增加脱敏策略。 |
| TUI 历史种子仅取当前工作区提示 | 不同工作区之间不会共享上箭头历史。 | 符合当前项目隔离要求；后续如需跨项目检索再扩展。 |
