# omnicrawl-workspace

工作区层：路径段安全校验 + 主 Agent 隔离工作区。语义基准是 `omnicrawl/workspace/`
（`slug.py` + `agent_isolation.py`），内核侧完整搬运，不是接口占位。

隔离区的用途是让多个主 Agent 进程（TUI / API / 飞书连接器）各自在独立的 git worktree 或
本地目录副本里读写，互不写穿主工作区，结束时可把变更安全应用回主工作区，并自动清理不再
需要的隔离区。

## 模块对映

| Python | Rust | 说明 |
| --- | --- | --- |
| `slug.py::is_safe_slug` / `validate_slug` | `src/slug.rs` 同名 | 空白检查用 `strip()` 真值、长度与字符集用原始值，两条口径都照搬 |
| `slug.py::SlugSafetyError` | `src/slug.rs::SlugSafetyError` | 只承载文案 |
| `Path.home()` / `expanduser()` / `resolve()` | `src/paths.rs` | 与 `omnicrawl-extensions::path` 同一口径；两处各持一份是因为插件 crate 的这套工具是私有的，工作区层不该反向依赖插件子系统 |
| `agent_isolation.py::IsolationSession` | `src/agent_isolation.rs::IsolationSession` | 字段与默认值逐个对齐（`created_at` 秒、`branch_name` 空） |
| `agent_isolation.py::create_isolation_session` | `src/agent_isolation.rs::create_isolation_session` | 参数面收进 `IsolationOptions`（`new` / `with_instance_id` / `with_config` / `with_worktrees_root`） |
| `prepare_isolated_workspace` / `finalize_isolation_session` / `finalize_subagent_worktrees` | 同名 | 收尾摘要文案与 Python 逐字一致 |
| `apply_isolation_changes` / `cleanup_eligible` / `cleanup_isolation_session` | 同名 | 见下两节 |
| `sweep_expired_isolation_sessions` | 同名，参数收进 `SweepOptions` | `removed` / `kept` / `applied` 三段结果 |
| `start_background_isolation_sweep` | 同名 | 返回 `JoinHandle<()>`（Python 返回 `threading.Thread`） |
| `register_isolation_session` / `registered_isolation_sessions` / `unregister_isolation_session` | 同名 | 进程级注册表（`Mutex<Vec<_>>`；Python 是 `dict`，Rust 按实例 ID 覆盖写入，语义一致） |
| `_read_worktree_gitdir` / `resolve_worktree_head` / `_worktree_registered` 等 | 同名（去掉私有前缀） | 纯文件系统解析 linked worktree 的 HEAD 与反向注册，fail-closed |

## 变更应用（`apply_isolation_changes`）

- **worktree / subagent**：先在隔离区 `add -A` + 内部提交，保证 diff 基线干净；再对
  **有效基线**取 `diff`。有效基线取「隔离区 HEAD 与主工作区 HEAD 的共同祖先」，只接受比
  `base_ref` 更近的候选——模型主动把成果同步进主仓库（`git merge --ff-only <隔离区提交>`）时，
  已同步的提交不再回放，否则重命名 / 新增文件的三方应用会整批失败并误报冲突。
- **三方应用**：先把主工作区当前内容纳入 index（主树已有的未提交修改成为 ours），再
  `git apply --3way --whitespace=nowarn`。成功时保留主修改并叠加 AI 增量；失败时 Git 原生写入
  `<<<<<<<` / `=======` / `>>>>>>>` 并标记未合并，patch 留在
  `~/.omnicrawl/agent-worktrees/patches/agent-<实例 ID>.patch`。不自行改写冲突文件，也不伪造
  unmerged 标记；冲突列表优先取真正处于未合并状态的文件。
- **local**：没有 git 基线，方向固定为隔离区 → 主工作区，只新增 / 覆盖、永不删除。
- patch 一律以 **LF** 写出：Windows 文本模式会把 `\n` 折成 `\r\n`，`git apply` 对 CRLF 补丁
  会整体失败（`patch does not apply`）。

## 四层清理门禁（`cleanup_eligible`）

四层**全部**通过的隔离区才允许自动清理，退出收尾与启动清扫共用同一判定：

1. 目录名以 `aw-`（主隔离区）/ `sw-`（SubAgent worktree）开头，即「标记为临时」；
2. 不在 `in_use` 里（当前使用中），且已过保留期（`MIN_KEEP_SECONDS = 3600s`；
   `created_at == 0` 视为无时间信息，跳过该层）；
3. fail-closed 变更检查：`git status --porcelain` 非空即保留；
4. worktree / subagent 模式再加一层：主隔离区看 `origin..HEAD` 是否有未推送 commit，
   SubAgent 看 `base_ref..HEAD` 是否有未审查的新提交——未推送 / 未审查的提交可能仍是唯一副本。

启动清扫（`sweep_expired_isolation_sessions`）只处理 `cleanup_on_exit=auto` 的会话；
`apply_on_exit=true` 的会话先尝试延迟收尾（apply 回主工作区）再走门禁；SubAgent 的成果
**绝不**在清扫 / 退出时自动 apply，必须由父 Agent 显式审查。无元数据的旧目录只做回收判定，
不做 apply 回写（无法还原基线）。

## 与 Python 的差异

- **实例 ID 哈希**：Python 是 `abs(hash(str(path))) % 0xFFFFF`（每进程随机加盐，跨进程不可比）；
  内核侧改用 FNV-1a，因此同一工作区两侧算出的数值不同。语义一致：同进程多次调用稳定、
  不同进程启动时刻不同 → 各自独立隔离区。对照数据集里不含这个值。
- **进程启动时刻**：Python 在 Windows 走 `GetProcessTimes`、其余平台读 `/proc/self/stat`；
  Rust 标准库没有对应系统调用，退化为「进程内首次调用时刻」（`OnceLock` 缓存），语义一致：
  同一进程内稳定，不同进程不同。
- **配置来源**：`AgentWorkspaceConfig` 由调用方经 `IsolationOptions` 注入（`omnicrawl-config`
  的 `features::agent_workspace`），不在这里读 `config.toml`；Python 侧同样是外部构造后传入。
- **元数据文件**：形状对齐 `json.dumps(..., ensure_ascii=False, indent=2)`（`serde_json`
  的 `to_string_pretty`），**未**做逐字节对照；两侧都按目录名定位（`aw-<id>.json`）。
- **文件复制**：`shutil.copytree` / `shutil.ignore_patterns(".git")` 换成自己的遍历
  （`copy_tree` / `mirror_directory`），符号链接的处理与 Python 的 `symlinks=True` 对齐：
  local 模式重建符号链接，镜像回主工作区时跳过符号链接。
- **git 子进程**：与 Python 相同（`git` 可执行文件必须在 PATH 上），失败文案与 `check` 语义对齐：
  `check=false` 时非零退出返回空的 stdout（调用方的门禁判定照搬 Python 的「空值即不拦」）。

## 对照

```bash
cd rust && cargo test -p omnicrawl-workspace
```

`tests/workspace_isolation_parity.rs` 七组（数据集 `tests/fixtures/workspace_isolation_parity.json`）：

1. **slug**（34 例）：合法性、长度上限（64 / 8）、规范化结果与三条错误文案；
2. **diff 统计**（7 例）：`count_changed` 与 `patch_files`，含空 diff、仅文件头、引号路径；
3. **gitdir HEAD 解析**（14 例）：detached（SHA-1 / SHA-256）、符号引用的 loose ref 与
   packed-refs（含 `#` 头与 `^` 剥皮行）、相对与绝对 commondir、缺 ref / 缺 commondir /
   垃圾内容 / 短哈希 / 大写哈希 / 空 HEAD；
4. **清扫条目重建**（11 例）：元数据驱动（绝对 / 相对 `worktree_path`、`sw-` 条目）、
   被篡改的元数据（指向别的目录、路径穿越）、缺字段、坏 JSON、非法实例 ID、
   遗留目录的 `.git` 推断与 mtime 创建时间；
5. **元数据读取**（6 例）：正常对象、数组、`null`、坏 JSON、空文件、文件缺失；
6. **元数据路径**（3 例）：`<隔离区根>/<目录名>.json`；
7. **四层门禁前两层**（7 例）：命名前缀、使用中、保留期与剩余秒数、`created_at == 0`、
   local / worktree 模式差异。

需要真跑 `git` 的部分由 `tests/isolation_git_roundtrip.rs` 覆盖（真仓库 + 真 worktree）：
worktree 往返（改动不穿透主工作区 → 第三层门禁拦截 → apply 回主工作区 → 门禁放行 → 清理并
删元数据）、复用（从元数据恢复 `base_ref` 与 `created_at`，且不重建目录）、local 模式镜像
（排除 `.git`）、`finalize_isolation_session` 的摘要文案与 `keep` 策略。环境里没有可用的
`git` 时整组跳过并打印原因，不把环境差异判成失败。

## 尚未移植 / 尚未接线

- 同包的 `connector_singleton`、`context`、`monitor`、`process_control`、`search_backend`、
  `temp`、`tools` 仍不在本 crate；
- ~~宿主还没有调用入口~~ 三条宿主路径都已接线：`omnicrawl-tui/src/main.rs`
  （启动准备 + 后台清扫 + 失败收尾）、`omnicrawl-api/src/service.rs`（`prepare_isolated_workspace` /
  `start_background_isolation_sweep` / `finalize_isolation_session`）与两边的会话收尾
  （`app.rs` 的 `finalize_isolation_session` + `finalize_subagent_worktrees`）。
