# omnicrawl-entry

Rust 统一启动入口，对应 Python `omnicrawl/entry.py` 的路由与子进程生命周期，外加
`omnicrawl/cli.py` 的插件 CLI。二进制名为 `omnicrawl-host`，避免与 stdio 内核 `omnicrawl` 混淆。

## 模块对映

| Python | Rust | 说明 |
| --- | --- | --- |
| `entry.py` 的启动路由 | `src/main.rs` | `--help` / `--version`、`kernel ...`、`api ...`、默认启动 TUI |
| `entry.py` 的子进程不变量 | `src/main.rs::run_child` | 继承标准输入输出、工作区作为 CWD、透传退出码 |
| `cli.py::build_parser` | `src/cli.rs::parse_plugin_args` | `plugin` 顶层参数 + 十个子命令，逐子命令校验选项集 |
| `cli.py::_resolve_scope` | `src/cli.rs::resolve_scope` | 工作区内有 `AGENTS.md` / `package.json` 默认 project，否则 user |
| `cli.py::run_plugin_command` | `src/cli.rs::run_plugin_command` | 十个子命令分发 + 异常→退出码阶梯 |
| `cli.py::_cmd_*` | `src/cli.rs::cmd_*` | 文案、退出码与 Python 逐字一致（含 `enabled=True` 这种 Python bool 写法） |
| `cli.py::_confirm` | `src/cli.rs::confirm` | stderr 提示 + stdin 读一行；EOF / 非 `y|yes` 视为取消 |
| `plugin_install` / `plugin_registry` / `plugin_models` 的能力 | `omnicrawl-extensions` | 安装、注册表、诊断全部复用已有移植，不在这里重写 |

`main()` 在 Python 里只做一件事：`plugin ...` 之外一律交给 `run_application`；Rust 保持同一分工——
`cli.rs` 不碰 TUI、API 或内核进程，`main.rs` 只负责选择目标。

## 已迁移

- `--help` / `--version`；`--resume <id>`（透传给 TUI）；
- `kernel ...`：启动 Rust 内核并透传标准输入、输出和错误；
- `api ...`：启动 `omnicrawl-api`；
- 默认路由（`src/main.rs::run_tui`）：无交互终端直接报错退出（不静默回退）→
  `startup::bootstrap` 跑首次配置与启动诊断（配置错误退出码 1、凭据缺失 2）→
  `startup::start_connectors` 拉起已配置的 Telegram / 飞书连接器 → 起 `omnicrawl-tui`
  子进程 → 退出时先 `connectors.close()` 再返回子进程退出码；
- `OMNICRAWL_BINARY`：协议透传模式（显式指定内核时宿主只做 stdio 透传，不截走 `--help` / `api`）；
- `AI_VOICE_CHAT_LAUNCH_CWD`：通过 `omnicrawl-config::core::context` 统一工作区；
- `plugin ...`：`system` / `install` / `list` / `info` / `enable` / `disable` / `update` /
  `rollback` / `uninstall` / `doctor`，退出码 `0` / `2` / `3` / `4` / `5` / `6` / `7` / `8`
  （与 `cli.py` 的 `EXIT_*` 常量逐个对齐）。

### 启动编排（`src/startup.rs`）

| Python | Rust | 说明 |
| --- | --- | --- |
| `entry.py::run_application` 的路由与退出码 | `src/main.rs::run_tui` | 交互判定、首次配置、连接器、TUI 子进程、退出回收 |
| `entry.py::_prepare_startup` 的配置阶段 | `src/startup.rs::bootstrap` | 复用 `omnicrawl-config` 的 `initialize_user_configuration` + `format_startup_report` |
| `config/core/bootstrap.py::_ensure_template` | `startup.rs` 的 `read_template` 端口 | 模板目录（`OMNICRAWL_TEMPLATES_DIR` → 可执行文件祖先的 `omnicrawl/config/templates`）优先，缺失用编译期内嵌副本 |
| `config/core/bootstrap.py::_check_node` | `startup.rs::probe_node` | `node --version` + PATH 上的 `npm`，文案与 Python 逐条一致 |
| `config/core/bootstrap.py::_check_plugin_state` | `startup.rs::probe_plugin_rows` | 复用 `omnicrawl-extensions::install::list_plugins(scope="all")` |
| `ui/fullscreen/screens/channel_setup.py` | `src/channel_setup.rs` | 行式渠道向导（无 Textual），保存复用 `save_channel_configuration` |
| `connectors/autostart.py` | `startup.rs::start_connectors` | 复用 `omnicrawl-connectors` 的监督器与单例锁 |

渠道向导与 Python 界面屏的差异：不做多列宽表单与鼠标交互，也不做远端模型自动发现
（需要模型目录网络层）。六项输入（渠道名 / Provider / 协议 / 基地址 / 模型 ID / 凭据变量名）
足够启动，其余字段保存后可在设置面板里改。

## 与 Python 的差异

- **参数解析**：手写解析器只覆盖本 CLI 声明的选项，**不接受** argparse 的长选项前缀缩写
  （`--pro` 这类）；报错文案是 `omnicrawl plugin: error: …`，不是 argparse 的 usage 全文。
  成功路径（字段与默认值）与未知选项 / 缺失位置参数的**退出码**都与 argparse 一致。
  帮助输出是自写的简短说明，不是 argparse 的自动排版。
- **runner 路径**：Python 从 `plugin_protocol.__file__` 推 `node_runner.mjs` 的位置；内核侧没有
  Python 包目录，改由 `OMNICRAWL_RUNNER_DIR` 环境变量，其次可执行文件同级的 `extensions/`
  注入（`runner_dir` / `runner_path`）。
- **配置来源**：`plugins` 段经 `omnicrawl-config` 的 `ConfigEnvironment` 注入，不读进程全局；
  Python 侧直接读默认配置路径。
- **stdout / stderr 的分工**：与 Python 一致——数据与结果写 stdout，提示与诊断写 stderr。

## Worker 启动路径

插件 Worker 靠 Node 拉起（`node_runner.mjs`），入口与路径在三个地方要对上：npm 启动器
（`packages/cli/bin/omnicrawl.mjs`）、宿主运行期（`omnicrawl-host` 的 `PluginHost`）与本 crate 的安装命令。

`cli::runner_dir()` / `cli::runner_path()` 现在委派给 `omnicrawl-extensions` 的同一份搜索链
（`OMNICRAWL_RUNNER_DIR` → 可执行文件及其祖先下的 `extensions/` 与 `omnicrawl/extensions/`
→ 工作目录），失败时带回探测过的目录；`cli::worker_launcher()` 给出 Node + runner 两条路径，
供需要自己起 Worker 的调用方复用。因此「`ocl plugin install` 装得上」与「宿主起得来」
用的是同一个 runner，不再各找一份。

## 尚未迁移

本 crate 不复制 Python 启动编排中的业务逻辑。以下内容仍由独立迁移批次负责：

- 自动更新（`maintenance/updater.py` + `version_check.py`）：Rust 宿主冻结分发下走 npm 升级路径；
- MCP 后台预热：内核侧没有同形的预热线程，工具发现由 TUI 的 MCP 管理器承担；
- Python Textual fallback。

未迁移的入口一律返回明确的未接入错误，不静默回退到 Python，也不执行不完整的替代逻辑。

## 对照

```bash
python rust/tools/gen_plugin_cli_fixture.py   # 期望值来自 omnicrawl/cli.py 的 argparse 面
cd rust && cargo test -p omnicrawl-entry
```

`tests/plugin_cli_parity.rs` 三组（数据集 `tests/fixtures/plugin_cli_parity.json`）：

1. **参数解析**（44 例）：成功时逐字段比对 argparse 的 namespace（只比对当前子命令真实声明
   的字段），帮助按退出码 0、参数错误按退出码 2 比对；
2. **作用域**（10 例）：空工作区 / 有 `AGENTS.md` / 有 `package.json`，以及 `--project` 与
   `--user` 同时给出时的静默 `2`；
3. **退出码**：八个 `EXIT_*` 常量与 Python 侧逐个一致。

安装 / 注册表 / 诊断要真跑 `npm` 与网络，不在本批对照范围，由 `omnicrawl-extensions` 的
`tests/extensions_parity.rs` 与它自己的真进程测试负责。
