# Agent 会话系统实现进度

本文档跟踪 `docs/session_design.md` 的分阶段落地情况。状态只记录已经完成、正在实现和明确延期的内容，避免和设计文档重复。

## 当前阶段

| 项目 | 状态 | 说明 |
|------|------|------|
| 当前阶段 | 已完成 | 第一阶段：`SessionStore` + JSONL 追加写 + `session_id`；第二阶段基础 `/sessions`、`/resume`、第三阶段提示历史基础能力、第四阶段确定性会话压缩、第五阶段 Qt 会话列表与正式导出基础闭环、第六阶段大工具输出 artifact、基础脱敏与手动归档已同步落地。 |
| 开始时间 | 2026-06-16 | 按会话设计文档从最小可用闭环开始实现。 |
| 当前目标 | 已完成 | 会话能自动创建、记录转录、列出最近会话、记录提示历史，通过 `/resume` 恢复 `_history`，在长会话中通过 `compact_summary` 保留早期上下文摘要，在 Qt GUI 中完成会话列表、恢复、重命名、压缩和正式导出，并对大工具输出做 artifact 分级存储、基础敏感信息脱敏和手动归档。 |

## 阶段清单

| 阶段 | 内容 | 状态 | 验收标准 |
|------|------|------|----------|
| 第一期 | `SessionStore`、`.agent_sessions/index.json`、`sessions/<id>.jsonl`、自动 `session_id` | 已完成 | 每轮对话生成可读 JSONL，索引随事件更新。 |
| 第二期 | `/sessions`、`/resume <session_id>`、恢复 `_history` | 已完成 | 可以列出当前项目会话，恢复后继续追问能看到旧上下文。 |
| 第三期 | 提示历史 `history.jsonl` | 已完成 | 用户输入历史可按当前项目复用，不自动进入模型上下文。 |
| 第四期 | `compact_summary` 长会话压缩 | 已完成 | 超过历史上限时生成摘要并保留任务主线。 |
| 第五期 | Qt 会话列表与正式导出 | 已完成 | GUI 可恢复、重命名、压缩、导出当前会话。 |
| 第六期 | 大工具输出 artifact、脱敏、归档 | 已完成 | 大输出 artifact 与基础脱敏已完成；支持 `/archive` 手动归档当前会话、`/archives` 查看归档，会话恢复时可自动解除归档并继续写入。 |

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
| 2026-06-16 | 接入大工具输出 artifact | `tool_result` 超过 8KB 时 JSONL 保存摘要、预览、哈希和 artifact 相对路径，完整输出写入 `.agent_sessions/artifacts/<session_id>/`；超过 128KB 的 artifact 按上限截断并记录原始大小与哈希。 |
| 2026-06-16 | 接入基础脱敏 | 会话事件写入前递归脱敏常见 `api_key`、`token`、`password`、`secret`、`authorization`、`cookie` 等字段，并对文本中的常见密钥赋值、Bearer Token 和 `sk/ak/ah-` 形态密钥做基础替换。 |
| 2026-06-16 | 恢复工具事件摘要 | `/resume` 恢复时把 `tool_call_requested`、`tool_call_denied` 和 `tool_result` 转为 assistant 摘要消息进入 `_history`，避免直接回放不完整 `role=tool` 链。 |
| 2026-06-16 | 接入手动会话归档 | `SessionStore` 支持 `archive/` 转录目录和 `archived_at` 索引字段；新增 `/archive`、`/archives`，恢复归档会话时自动移回 `sessions/` 并继续写入。 |

## 已知限制

| 限制 | 影响 | 后续处理 |
|------|------|----------|
| 工具调用链以摘要形式恢复 | 工具调用链会被记录，恢复时以 assistant 摘要进入 `_history`，不直接还原为 Chat Completions 原生 `tool_calls` / `role=tool` 链。 | 后续如需要精确继续半轮工具调用，可增加原生消息链恢复。 |
| Qt 恢复只回放已支持事件类型 | Qt 消息区可回放 user、assistant、tool call、tool result、compact_summary；审批事件暂不单独渲染。 | 后续如需审计视图，可把 tool approval/denial 渲染为系统事件卡片。 |
| 压缩摘要为本地确定性摘要 | 不调用模型生成高质量语义摘要，摘要细节弱于专门模型总结。 | 后续可引入可选 `SessionCompactor` 模型摘要，但需避免额外失败影响主链路。 |
| 基础脱敏不是安全边界 | 已覆盖常见密钥字段和部分文本模式，但无法保证识别所有秘密、业务口令或私有数据。 | `.agent_sessions/` 仍默认由 `.gitignore` 忽略；后续可增加可配置规则、导出前扫描和用户确认。 |
| TUI 历史种子仅取当前工作区提示 | 不同工作区之间不会共享上箭头历史。 | 符合当前项目隔离要求；后续如需跨项目检索再扩展。 |
| 会话归档为手动触发 | 不会自动清理或迁移旧会话，避免用户看不到仍在使用的会话。 | 后续如需自动归档，可基于 `updated_at`、消息数量和用户配置增加保守策略。 |
