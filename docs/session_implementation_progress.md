# Agent 会话系统实现进度

本文档跟踪 `docs/session_design.md` 的分阶段落地情况。状态只记录已经完成、正在实现和明确延期的内容，避免和设计文档重复。

## 当前阶段

| 项目 | 状态 | 说明 |
|------|------|------|
| 当前阶段 | 已完成 | 第一阶段：`SessionStore` + JSONL 追加写 + `session_id`；第二阶段基础 `/sessions`、`/resume` 已同步落地。 |
| 开始时间 | 2026-06-16 | 按会话设计文档从最小可用闭环开始实现。 |
| 当前目标 | 已完成 | 会话能自动创建、记录转录、列出最近会话，并通过 `/resume` 恢复 `_history`。 |

## 阶段清单

| 阶段 | 内容 | 状态 | 验收标准 |
|------|------|------|----------|
| 第一期 | `SessionStore`、`.agent_sessions/index.json`、`sessions/<id>.jsonl`、自动 `session_id` | 已完成 | 每轮对话生成可读 JSONL，索引随事件更新。 |
| 第二期 | `/sessions`、`/resume <session_id>`、恢复 `_history` | 已完成 | 可以列出当前项目会话，恢复后继续追问能看到旧上下文。 |
| 第三期 | 提示历史 `history.jsonl` | 未开始 | 用户输入历史可按当前项目复用，不自动进入模型上下文。 |
| 第四期 | `compact_summary` 长会话压缩 | 未开始 | 超过历史上限时生成摘要并保留任务主线。 |
| 第五期 | Qt 会话列表与正式导出 | 未开始 | GUI 可恢复、重命名、导出当前会话。 |
| 第六期 | 大工具输出 artifact、脱敏、归档 | 未开始 | 大输出不拖慢恢复，敏感内容可控。 |

## 本轮实现记录

| 时间 | 进展 | 结果 |
|------|------|------|
| 2026-06-16 | 新增实现进度文档 | 已创建本文件，作为后续会话系统实现的跟踪入口。 |
| 2026-06-16 | 新增 `ai_voice_agent/session.py` | 已建立会话事件、索引、JSONL 读写和恢复数据模型。 |
| 2026-06-16 | 接入 `LocalToolAgent` | 已自动创建会话，并记录 user、assistant、tool call、tool result、审批和中断事件。 |
| 2026-06-16 | 接入 TUI / Qt 斜杠命令 | 已支持 `/sessions` 查看最近会话、`/resume <session_id>` 恢复会话上下文。 |
| 2026-06-16 | 更新测试 | 新增 `tests/test_session_store.py`，并扩展 Agent 上下文测试覆盖事件写入和恢复。 |
| 2026-06-16 | 运行验证 | `python -m pytest -q` 通过，结果：148 passed，1 个第三方 `pyreadline` 弃用警告。 |

## 已知限制

| 限制 | 影响 | 后续处理 |
|------|------|----------|
| 第一版只恢复 user/assistant/compact_summary | 工具调用链会被记录，但暂不完整回放进 `_history`。 | 后续补 tool call 消息还原和大输出 artifact。 |
| 第一版不做自动摘要压缩 | 长会话仍受 `max_history_turns` 最近窗口限制。 | 第四期接入 `SessionCompactor`。 |
| 第一版不做敏感信息脱敏 | 会话文件可能包含用户输入和工具输出。 | 先通过 `.gitignore` 忽略 `.agent_sessions/`，后续增加脱敏策略。 |
