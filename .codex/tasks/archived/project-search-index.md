# 任务：项目文件名与内容快速索引

状态：已完成
创建：2026-07-27
更新：2026-07-27

## 需求摘要

- 新增独立 `find_files` 文件名/路径搜索工具，保留 `list_files`。
- `search_text` 只做普通关键词搜索；文件名和内容索引均默认关闭并在设置中独立启用。
- 索引限定在 PRJ，后台构建并在 TUI 版本号下显示进度；用户主目录或文件系统根目录本身禁止项目级内容搜索。
- 持久化索引快照与 Windows NTFS USN 游标，后续启动优先追增量，无法使用 USN 时安全降级重建。

## 关键决策

- 文件名索引加载到内存；内容索引使用 SQLite FTS5 trigram，并在返回前逐行复核精确子串。
- 快照存放于 `~/.OmniCrawl/search-index/`，按规范化 PRJ 路径哈希隔离，不写入项目目录。
- Windows NTFS 使用 USN Journal 做增量恢复；权限、日志截断、损坏快照或结构异常时后台完整重建。
- 索引关闭或未就绪时，两个搜索工具均保留受工作区边界保护的直接扫描降级。

## 实现计划

- [x] 1. 实现配置、索引服务、持久化快照与 USN 增量读取
- [x] 2. 接入 WorkspaceTools、Agent 工具表、设置和工作区生命周期
- [x] 3. 接入 TUI 进度，补测试、示例配置和文档并完成验证

## 已修改文件

- `omnicrawl/workspace/search_index.py`、`omnicrawl/workspace/usn.py`、`omnicrawl/workspace/tools.py`
- `omnicrawl/agent/core.py`、`omnicrawl/agent/tools.py`、`omnicrawl/agent/subagents/`
- `omnicrawl/entry.py`、`omnicrawl/api/app.py`、`omnicrawl/commands/slash.py`
- `omnicrawl/ui/fullscreen/`、`omnicrawl/ui/tool_labels.py`
- `config.example.yaml`、`omnicrawl/config/templates/config.example.yaml`
- `README.md`、`omnicrawl/docs/SEARCH_INDEX.md`、`omnicrawl/docs/TERMINAL_UI.md`
- `tests/test_search_index.py` 及相关工具、设置、TUI、入口和 SubAgent 回归测试

## 验证

- `python -m unittest discover -s tests -q`：通过。
- `python -m unittest tests.test_search_index -q`：12 项通过。
- 本机 Windows NTFS：USN Journal 状态读取、新建文件事件捕获、持久化游标增量恢复通过。
- `python -m black --check ...`：通过。
- `python -m isort --profile black --check-only ...`：通过。
- `python -m compileall -q omnicrawl main.py`：通过。
- `git diff --check`：通过。
- `ruff`：当前环境未安装，已由上述格式、编译和测试检查替代。
