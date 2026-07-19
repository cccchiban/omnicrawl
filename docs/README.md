# 文档目录

本目录集中存放项目设计说明和实现记录。

运行时系统提示词模板位于项目包内的 `omnicrawl/agent/system_prompt.md`。模板只保留工具协议和“遇到什么任务先读哪个文档”的路由说明；具体规范应按需读取下列文档，避免每轮注入大量固定规则。

## 文档清单

- `TERMINAL_UI.md`：终端 UI 的交互约定、技术方案和限制说明。
- `WINDOWS_DESKTOP_TOOLS.md`：Windows 内置窗口、UI Automation、SendInput、文本剪贴板与原生截图/视觉模型注入工具的接口、安全边界和验证说明。
- `API.md`：本地 HTTP/SSE API 的配置、鉴权、接口、事件流和前端接入示例。
- `skill_system_impl.md`：Agent Skill 子系统的设计与实现记录。
- `SKILL_INSTALLATION.md`：AI Skill 安装、编写、验证和渐进式披露使用规范。
- `HOOK_PLUGIN_DESIGN.md`：Hook 生命周期、NPM 插件契约、CLI 安装管理、权限隔离、覆盖/删除语义与回滚方案。
- `memory_system_design.md`：Agent 记忆系统的流程、存储结构和清理机制设计。
- `session_design.md`：Agent 会话系统的生命周期、持久化、恢复和长会话压缩设计；包含初始方案与当前实施状态说明。
- `session_store_technical_debt.md`：SessionStore 并发一致性、敏感信息、版本迁移和崩溃恢复治理记录；主要项目已完成，保留历史问题与残余限制。
- `agent_refactor_plan.md`：Agent、MCP、Session、API、全屏 UI 等大文件与高耦合模块的分阶段治理计划；阶段 7 架构复查已收尾。
- `MCP_USAGE.md`：MCP 配置、调用、排障和渐进式披露使用规范。
- `MULTI_MODEL_API_DESIGN.md`：多模型原生 SDK 接入、统一内部协议、`/model` 热切换、双列模型目录及 YAML 配置迁移方案。**主链路已落地**（OpenAI Chat Runtime、config.yaml/models.yaml、ModelPicker、API catalog）；Claude/Gemini 真机联调与部分契约测试仍可补强，详见文档 §0。
- `SUBAGENT_DESIGN.md`：子 Agent 与任务分发设计及实施状态。Phase 0–3、受控 `verify`/Fork/模型覆盖、后台任务与审批控制面、跨进程安全快照恢复、独立 Plugin dispatch context、Worktree 写隔离及默认关闭的通用写 Agent 均已落地。任务支持有界并发、取消、TTL 清理、一次性通知、Session artifact、安全 SSE/TUI/API 控制面和父侧 apply/discard。Plugin、MCP、Skill、Memory、Session、API、TUI 专项边界及无限递归、权限扩大、隐藏推理泄露、跨工作区残留任务四项最终安全不变量均已完成回归。

说明：`session_implementation_progress.md` 与 `MCP_DESIGN_TECHNICAL.md` 当前不在仓库中；会话落地进度与 MCP 设计细节分别以 `session_design.md`、`MCP_USAGE.md` 及源码/测试为准。
