# 任务：拆分项目级、会话级和用户级记忆

状态：已完成
创建：2026-07-13
更新：2026-07-13

## 需求摘要

将现有共享 `memory/` 拆分为三类独立记忆：项目级记忆存放在当前工作区 `.oclmemory/`，会话级记忆按 `~/.omnicrawl/Session_memory/<session_id>/` 隔离，用户级记忆存放在 `~/.omnicrawl/User_memory/`。为三类记忆提供独立搜索、读取、关联扩展和写入工具；旧 `memory/` 自动迁移到项目级并保留备份。

## 关键决策

- 项目级记忆目录固定为当前工作区根目录下的 `.oclmemory/`。
- 首次发现旧 `memory/` 时迁移到 `.oclmemory/`；目标已存在时导入并将旧目录改名为带时间戳的备份。
- 会话级记忆使用每个 `session_id` 独立目录，禁止跨会话查询。
- 自动上下文压缩只写会话级记忆。
- 项目技术事实写项目级，用户偏好/纠错写用户级；模型通过独立工具选择作用域。
- 全局 `memory.enabled` 继续作为三类记忆的总开关。

## 实现计划

- [x] 1. 扩展 MemoryStore 迁移能力并加入三类路径工厂
- [x] 2. 将工具拆分为 project/session/user 三组并绑定会话生命周期
- [x] 3. 将上下文压缩结果改写入会话级记忆，更新提示词和文档
- [x] 4. 添加隔离、迁移、工具路由和压缩回归测试
- [x] 5. 执行定向测试、全量测试和代码差异检查

## 已修改文件

- `.codex/tasks/memory-scopes.md`
- `omnicrawl/state/memory.py`
- `omnicrawl/agent/core.py`
- `omnicrawl/agent/memory_tools.py`
- `omnicrawl/agent/tools.py`
- `omnicrawl/agent/session_facade.py`
- `omnicrawl/agent/subagents/approval.py`
- `omnicrawl/agent/subagents/coordinator.py`
- `omnicrawl/agent/system_prompt.md`
- `omnicrawl/commands/slash.py`
- `omnicrawl/ui/fullscreen/tool_diff.py`
- `omnicrawl/ui/tool_labels.py`
- `omnicrawl/docs/memory_system_design.md`
- `omnicrawl/docs/session_design.md`
- `tests/test_memory_scopes.py`

- 通过：`python -m unittest tests.test_memory_scopes -v`
- 通过：记忆工具、记忆模块边界、上下文压缩、工作区切换、子代理协调和 UI 工具标签定向回归
- 通过：`python -m compileall -q omnicrawl main.py`
- 通过：`git diff --check`
- 全量 `python -m unittest discover -s tests` 共 840 条用例，其中 836 条通过；剩余 4 条为既有全屏 UI 时序/全局配置隔离失败，单独复跑仍与本次记忆改动无关。
- 迁移失败时保留旧 `memory/`；目标存在时生成 `.migrated-*` 备份。
