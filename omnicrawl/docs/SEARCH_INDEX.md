# 项目搜索索引

OmniCrawl 把本地搜索拆成两个原生工具：

- `find_files`：只匹配文件名、目录名和 PRJ 相对路径，不读取文件内容。
- `grep`：在 UTF-8 文本文件中执行 grep 风格搜索。pattern 默认按正则表达式解释（`use_regex=false` 时按精确子串），支持大小写开关、匹配行上下文（`context_lines`）、每文件匹配计数（`count`）、仅列出匹配文件（`files_with_matches`）和 `include`/`exclude` 文件名 glob 过滤。

`list_files` 仍负责列出指定目录，不承担搜索语义。

## 配置与运行方式

两个加速索引默认关闭，可通过 `config.yaml` 或 TUI `/settings` 独立开启：

```yaml
file_name_index:
  enabled: false

content_index:
  enabled: false
```

开关只控制加速层。索引关闭、尚未就绪或查询失败时，工具会在既有工作区安全边界内直接扫描，因此工具能力不会消失。

索引构建在后台线程执行。TUI 在右上角版本号下方显示“正在加载项目搜索索引”“正在建立项目文件索引”或内容索引百分比；就绪后该位置恢复为空。

## 数据与增量更新

索引严格限定在当前 PRJ，跳过 `.git`、虚拟环境、`.env`、运行配置和其他受保护路径。
索引层额外忽略构建产物/缓存/工具私有目录（`build`、`dist`、`.pytest_cache`、
`.agents`、`.claude`、`.codex`、`.pi-subagents`、`.agent_tmp`、`logs`、`designs` 等，
见 `WorkspaceTools.INDEX_EXCLUDED_NAMES`）：这些目录不进索引快照，但普通工具
仍可直接访问和搜索——`find_files`/`grep` 在走索引的同时会补充扫描这些
目录，保证开启索引后结果与直接扫描一致。持久化文件按规范化 PRJ 路径的
SHA-256 前缀隔离，保存在：

```text
~/.OmniCrawl/search-index/<workspace-hash>.sqlite3
```

文件名快照启动后加载到内存；内容使用 SQLite FTS5 trigram 查找候选文件，再逐行复核精确子串和大小写，避免 FTS 分词改变工具结果。内容索引只服务 `grep` 的字面量模式（`use_regex=false`）；默认正则模式由工具层直接扫描，不经索引。超过 2MB 的文件只保留文件名条目，不建立内容索引，避免大文件撑爆 trigram 表。

Agent 的 `write_file` 和 `replace_text` 会立即刷新对应条目；内容读取失败（如
Windows 独占锁）的文件会登记为补偿更新，在后续轮询中重试，不会静默丢失。
Windows 本地 NTFS 卷优先直接读取 USN Journal V2 记录，并持久化 Journal ID 与 USN 游标；游标连续时，后续启动只回放增量。以下情况会在后台完整重建：

- 卷不是 NTFS或系统拒绝读取 Journal；
- Journal ID 变化、游标早于 `LowestValidUsn` 或快照版本不兼容；
- USN 记录损坏或无法映射到现有 PRJ 路径。

USN 回放或增量应用失败只会降级到低频完整核对，索引服务不会因此退出；
核对按文件 mtime/size 增量重读，未变化的文件保留现有内容索引。
非 Windows、非 NTFS 或 USN 不可用时，索引使用低频完整核对来同步外部变更。

## 根目录限制

文件名搜索允许 PRJ 位于任意位置，包括用户主目录和文件系统根目录。内容搜索额外限制如下：

- 当搜索路径本身是用户主目录或文件系统根目录时，`grep` 拒绝执行；
- 显式指定其下的项目子目录仍可搜索；
- PRJ 本身处于受限根目录时，不建立覆盖整个 PRJ 的内容索引，子目录查询自动降级为直接扫描；
- 在用户主目录或文件系统根目录启动 ocl/API 时，即使配置开启了 `content_index.enabled`，入口也会自动禁用内容索引（不修改配置文件），避免工作区回退到 Agent 程序目录后意外建立大范围内容索引。

该限制避免在终端默认用户目录或盘符根目录中意外读取大范围文件内容，同时保留明确子项目的搜索能力。

## 主要实现

- `omnicrawl/workspace/search_index.py`：快照、后台构建、名称查询、FTS 查询和增量应用。
- `omnicrawl/workspace/usn.py`：Windows NTFS USN Journal 只读适配器。
- `omnicrawl/workspace/tools.py`：工具契约、安全边界和直接扫描降级。
- `omnicrawl/workspace/context.py`：工作区检测与受限根目录自动禁用策略（`should_disable_content_index`）。
- `omnicrawl/agent/core.py`：启动、设置切换、工作区切换和关闭生命周期。
- `omnicrawl/ui/fullscreen/`：设置开关和 HUD 进度显示。

## 验证

```powershell
python -m unittest tests.test_search_index -v
python -m unittest tests.test_workspace_tools tests.test_workspace_switch -v
python -m unittest tests.test_fullscreen_tui tests.test_settings_panel -v
python -m compileall -q omnicrawl main.py
git diff --check
```
