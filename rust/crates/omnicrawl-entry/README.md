# omnicrawl-entry

Rust 统一启动入口，对应 Python `omnicrawl/entry.py` 的路由与子进程生命周期。
二进制名为 `omnicrawl-host`，避免与 stdio 内核 `omnicrawl` 混淆。

## 模块对映

| Python | Rust | 说明 |
| --- | --- | --- |
| `entry.py` 的启动路由 | `src/main.rs` | `--help` / `--version`、`kernel ...`、`api ...`、默认启动 TUI |
| `entry.py` 的子进程不变量 | `src/main.rs::run_child` | 继承标准输入输出、工作区作为 CWD、透传退出码 |

## 已迁移

- `--help` / `--version`；`--resume <id>`（透传给 TUI）；
- `kernel ...`：启动 Rust 内核并透传标准输入、输出和错误；
- `api ...`：启动 `omnicrawl-api`；
- `decision ...` / `fetch ...`：启动对应的本地 REST 服务二进制；
- `connector <平台>` / `--connector <平台>`：启动**宿主承载**的连接器——转发给
  `omnicrawl-api connector <平台>`（真实工具表、会话与命令注册表）；`startup::connector_specs`
  在无宿主载荷的平台（armv7 等）退回内核的 `--connector`，那条路径只自持三个自持工具；
- 默认路由（`src/main.rs::run_tui`）：无交互终端直接报错退出（不静默回退）→
  `startup::bootstrap` 跑首次配置与启动诊断（配置错误退出码 1、凭据缺失 2）→
  `startup::start_connectors` 拉起已配置的 Telegram / 飞书连接器 → 起 `omnicrawl-tui`
  子进程 → 退出时先 `connectors.close()` 再返回子进程退出码；
- `OMNICRAWL_BINARY`：协议透传模式（显式指定内核时宿主只做 stdio 透传，不截走 `--help` / `api`）；
- `AI_VOICE_CHAT_LAUNCH_CWD`：通过 `omnicrawl-config::core::context` 统一工作区。

### 启动编排（`src/startup.rs`）

| Python | Rust | 说明 |
| --- | --- | --- |
| `entry.py::run_application` 的路由与退出码 | `src/main.rs::run_tui` | 交互判定、首次配置、连接器、TUI 子进程、退出回收 |
| `entry.py::_prepare_startup` 的配置阶段 | `src/startup.rs::bootstrap` | 复用 `omnicrawl-config` 的 `initialize_user_configuration` + `format_startup_report` |
| `config/core/bootstrap.py::_ensure_template` | `startup.rs` 的 `read_template` 端口 | 模板目录（可执行文件祖先的 `rust/assets/config-templates` → `omnicrawl/config/templates`）优先，缺失用编译期内嵌副本 |
| `ui/fullscreen/screens/channel_setup.py` | `src/channel_setup.rs` | 行式渠道向导（无 Textual），保存复用 `save_channel_configuration` |
| `connectors/autostart.py` | `startup.rs::start_connectors` | 复用 `omnicrawl-connectors` 的监督器与单例锁 |

渠道向导与 Python 界面屏的差异：不做多列宽表单与鼠标交互，也不做远端模型自动发现
（需要模型目录网络层）。六项输入（渠道名 / Provider / 协议 / 基地址 / 模型 ID / 凭据变量名）
足够启动，其余字段保存后可在设置面板里改。

## 与 Python 的差异

- **stdout / stderr 的分工**：与 Python 一致——数据与结果写 stdout，提示与诊断写 stderr。
- **启动诊断**：Python 的 `_check_node` 与 `_check_plugin_state` 随插件功能一起移除，
  首次启动只做模型配置检查。

## 尚未迁移

本 crate 不复制 Python 启动编排中的业务逻辑。以下内容仍由独立迁移批次负责：

- 自动更新（`maintenance/updater.py` + `version_check.py`）：Rust 宿主冻结分发下走 npm 升级路径；
- MCP 后台预热：内核侧没有同形的预热线程——能力发现被**提前**到宿主启动期
  （TUI 装配 `McpClientManager` 时调 `discover()`，见 `omnicrawl-tui/README`），
  因此不需要 Python 那种后续补发现的预热线程；
- Python Textual fallback。

未迁移的入口一律返回明确的未接入错误，不静默回退到 Python，也不执行不完整的替代逻辑。
