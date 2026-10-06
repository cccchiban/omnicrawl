# omnicrawl-extensions

`omnicrawl/extensions/` 的 Rust 内核移植：插件模型与校验、注册表与执行计划、
Hook 分发与 Worker 生命周期、Skill 发现与匹配、npm 安装器。

对照数据集是冻结契约；两侧靠
对照数据集对齐，不靠人读代码。

## 模块分工

| Python | Rust | 覆盖内容 |
| --- | --- | --- |
| `plugin_models.py` | `src/models.rs` | Hook 表与策略、四种 Handler 模式、超时与阈值常量、manifest / handler / 自定义事件解析、载荷 schema 校验、JSON Patch 校验与应用、Handler 排序、`HookResult` 归一化、稳定哈希 |
| `plugin_registry.py` | `src/registry.rs` | 路径解析、注册表读写（BOM / 损坏 JSON / 结构非法的三段文案）、原子写、user/project 合并、执行计划构建、`replaces` 冲突与环检测、启停 / 卸载 / tombstone |
| `skill.py` | `src/skill.rs` | 名称与描述校验、frontmatter 解析、`SKILL.md` 与根级 `.md` 扫描、符号链接去重、同名冲突诊断、匹配打分、渐进式披露 XML 输出 |
| `plugin_protocol.py` | `src/protocol.rs` | Worker 环境白名单、NDJSON JSON-RPC 客户端（请求配对、超时、取消、stdout 协议校验、stderr 收集、Worker → Host 请求应答） |
| `plugin_manager.py` | `src/manager.rs` | `HookDispatcher`（分模式阶段执行、observe/notify 同插件串行 + 跨插件并行、熔断、审计）、`PluginManager`（bootstrap、握手校验、自定义事件投递、状态表、关闭）、`PluginRuntime`（事务式启停与工作区切换） |
| `plugin_install.py` | `src/install.rs` | 包规格解析、node/npm 探测、registry 交互、tarball 下载与解压、integrity / 内容树 / lockfile 校验、本地包安装、开发模式注册、启停、卸载与清理、回滚、列表与 doctor |
| — | `src/path.rs` | `Path.resolve()` / `expanduser()` / `home()` 的可用子集 |

## 对照工作流

```bash
cd rust && cargo test -p omnicrawl-extensions
```

`tests/fixtures/extensions_parity.json` 覆盖 23 组用例：SemVer 与包名判定、命名空间归一化、
plugins 配置解析与区间校验、Handler 声明校验、自定义事件声明、manifest 全量解析、
载荷 schema 校验、JSON Patch 校验与应用、Handler 排序、`HookResult` 归一化、稳定哈希、
模式默认超时、注册表合并、执行计划、`replaces` 冲突与环、Skill 名称 / 描述校验、
frontmatter 解析、描述推断、渐进式披露输出，以及安装器的包规格与 URL 编码。

改了实现要让冻结数据集同步更新。

### 真实链路测试

依赖真实进程与文件系统的面不走对照夹具，改由 `tests/` 下的集成测试覆盖：

| 测试 | 覆盖的链路 |
| --- | --- |
| `tests/worker_lifecycle.rs` | Worker 进程启动、`initialize` 握手校验、`hook.invoke` 真实调用、`shutdown` 回收与回收后重启；`PluginManager` 从注册表 dev-mode 引导到真实 Worker 再分发；缺 runner 时的诊断 |
| `tests/real_paths.rs` | Skill 目录扫描 `discover`（项目级 / 额外路径 / 缺失路径诊断）；注册表原子写与读盘往返（含损坏 JSON 不改写原文件） |

两个文件都用仓库里的真实 fixture 插件（`tests/fixtures/npm_plugins/`）；缺 Node.js 20+ 或
`node_runner.mjs` 时 `worker_lifecycle.rs` 整组跳过（打印搜索链后返回），不会让无 JS 运行时的
环境变红。npm 下载、`npm install` 子进程与 tarball 解压仍只能靠联网手动验证
（`ocl plugin doctor` / `ocl plugin install`），本轮未纳入自动化。

### Worker 启动路径

`WorkerLauncher::resolve()` 按固定顺序找 Node 与 `node_runner.mjs`：
`OMNICRAWL_RUNNER_DIR` → 可执行文件及其各级祖先下的 `extensions/`（载荷同级）、
`rust/assets/extensions/`（仓库检出的单一来源）与 `omnicrawl/extensions/`（旧布局）
→ 进程工作目录下的同三级。CLI 的安装期冒烟（`cli::runner_path`）、宿主的运行期启动
（`PluginManager`）与 TUI/API 的诊断共用这一份解析，失败文案里带上探测过的目录。

## 已知差异

1. **runner 路径**：Python 用 `Path(__file__).parent / "node_runner.mjs"` 定位 Worker 入口；
   Rust 侧没有 `__file__`，改由 `protocol::WorkerLauncher::resolve()` 按搜索链定位
   （`node_runner_path(dir)` 仍只拼文件名），搜索链见上节。
2. **关停语义**：`close(force=True|False)` 合并为 `close()`，一律强杀子进程；Python 在 Unix
   上先发 `SIGTERM` 再 `SIGKILL`，这条优雅窗口未保留。
3. **`str()` 转换**：解析非字符串字段时不再复现 Python 的 `str(None) == "None"`，
   `null` 一律回落空串（与会话层「已知差异」同一条约定）。
4. **集合迭代序**：Python 的 `set` 迭代受哈希随机化影响，匹配打分的子串加分与环检测的
   遍历序都不确定；Rust 侧按排序后的顺序处理，因此 `_score_match` 的浮点累加顺序固定，
   与 Python 的差异只可能在末尾 1e-16 量级。
5. **字符串数组读取**：Python 对非数组值（例如字符串）会逐字符迭代，Rust 侧统一按空数组处理。
6. **`set_enabled` 返回值**：Python 返回当前 Manager 供调用方更新引用；Rust 侧 Manager 由
   `PluginRuntime` 自己持有，只返回成功与否。
7. **日志**：Python 用 `logging` 记录 Worker stderr 与审计；Rust 侧审计走 `AuditSink` 回调，
   stderr 回调默认关闭（由调用方注入），不内置日志框架。
8. **新增依赖**：本 crate 引入 `flate2`（已在依赖图内）与 `tar`（纯 Rust 归档解析，
   随包带入 `filetime` / `xattr`），用于解 `npm pack` 的 `.tgz`。
