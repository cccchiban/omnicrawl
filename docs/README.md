# 文档目录

本目录集中存放项目设计说明和实现记录。

运行时系统提示词模板位于项目包内的 `omnicrawl/system_prompt.md`。模板只保留工具协议和“遇到什么任务先读哪个文档”的路由说明；具体规范应按需读取下列文档，避免每轮注入大量固定规则。

## 文档清单

- `TERMINAL_UI.md`：终端 UI 的交互约定、技术方案和限制说明。
- `skill_system_impl.md`：Agent Skill 子系统的设计与实现记录。
- `SKILL_INSTALLATION.md`：AI Skill 安装、编写、验证和渐进式披露使用规范。
- `memory_system_design.md`：Agent 记忆系统的流程、存储结构和清理机制设计。
- `session_design.md`：Agent 会话系统的生命周期、持久化、恢复和长会话压缩设计。
- `session_implementation_progress.md`：Agent 会话系统按阶段落地的进度跟踪。
- `agent_refactor_plan.md`：`agent.py` 拆分、精简和分阶段验证路线。
- `MCP_USAGE.md`：MCP 配置、调用、排障和渐进式披露使用规范。
- `MCP_DESIGN_TECHNICAL.md`：MCP 子系统设计、技术方案、安全策略和实施路线。
