# 任务：统一用户配置目录到 `.OmniCrawl`

状态：已完成
创建：2026-06-20
更新：2026-06-20

## 需求摘要

- pip 安装后的运行配置、模型配置、全局 AGENTS.md 和用户级运行数据统一放在用户主目录下的 `.OmniCrawl`。
- 旧配置目录自动迁移，迁移完成后不保留旧目录。
- 全局 AGENTS.md 与项目目录 AGENTS.md 同时生效，项目级规则优先。

## 关键决策

- 新目录统一为 `Path.home() / ".OmniCrawl"`，不再区分 Windows `%APPDATA%` 与 Unix XDG 目录。
- 旧目录优先在启动时迁移；迁移失败或尚未完成时，读取路径仍兼容旧目录，但所有默认写入固定落到新目录。目标文件冲突时保留目标文件，并将旧文件写入目标目录的 `.migrated.bak` 备份名，避免覆盖现有配置。
- 全局 AGENTS.md 由用户目录加载，项目 AGENTS.md 继续由工作区加载；两者使用明确来源标记，项目内容排在全局内容之后。
- 用户插件目录也统一迁移到 `.OmniCrawl/plugins`，兼容旧的 `~/.omnicrawl/plugins`。

## 实现计划

- [x] 1. 为新目录、旧目录迁移和全局 AGENTS.md 增加回归测试
- [x] 2. 修改运行时路径、启动初始化和插件目录
- [x] 3. 合并全局与项目 AGENTS.md 上下文
- [x] 4. 更新 README 和运行时路径说明
- [x] 5. 执行全量测试、代码审查和手工路径验证

## 已修改文件

- `omnicrawl/config/runtime.py`
- `omnicrawl/config/bootstrap.py`
- `omnicrawl/agent/core.py`
- `omnicrawl/agent/prompt_context.py`
- `omnicrawl/agent/subagents/definitions.py`
- `omnicrawl/extensions/plugin_registry.py`
- `README.md`
- `tests/test_runtime_config.py`
- `tests/test_startup_setup.py`
- `tests/test_agent_context.py`
- `tests/test_plugin_registry.py`

## 验证

- 全量 pytest：816 个测试通过
- `python -m compileall -q omnicrawl` 通过
- `git diff --check` 通过
