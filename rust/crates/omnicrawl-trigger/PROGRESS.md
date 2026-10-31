# 触发接口实施进度

设计稿：`designs/trigger_api_design.md`（本地设计区，被 `.gitignore` 忽略）。
用户文档：`rust/assets/docs/trigger_api.md`（内置文档，已注册进 `BUNDLED_DOCS`）。
本文件随代码提交，记录落地状态与偏差。

## 状态总览

| 步骤 | 内容 | 状态 |
|---|---|---|
| 1 | `omnicrawl-config::features::trigger_api` 段装载/归一化/save | 完成 |
| 2 | `omnicrawl-trigger`：app + routes + request + records | 完成 |
| 3 | `pool`：slot 池、回合驱动、超时/取消/终态 | 完成 |
| 4 | `notify`：终态投递（telegram / feishu；qqbot 拒绝） | 完成 |
| 5 | `server` + `main`：PID/日志就绪、ensure/stop/status | 完成 |
| 6 | `omnicrawl-entry` 加 `trigger` 子命令 | 完成 |
| 7 | 内置文档注册与系统提示词文档清单 | 完成 |
| 8 | `config.example.toml` 示例段 + 打包脚本登记 | 完成 |

## 改动清单

**新增**
- `rust/crates/omnicrawl-config/src/features/trigger_api.rs`（配置域，6 个单测）
- `rust/crates/omnicrawl-trigger/`（crate：`lib` + 二进制 + 8 个模块 + 单测）
- `rust/assets/docs/trigger_api.md`（内置文档）
- `rust/crates/omnicrawl-trigger/PROGRESS.md`（本文件）

**修改**
- `rust/Cargo.toml`：workspace members 加 `crates/omnicrawl-trigger`
- `rust/crates/omnicrawl-config/src/features/mod.rs`：注册 `trigger_api`
- `rust/crates/omnicrawl-entry/src/main.rs`：`trigger` 子命令、帮助文本、常量测试
- `rust/crates/omnicrawl-mcp/src/bundled.rs`：`BUNDLED_DOCS` 16 → 17
- `rust/crates/omnicrawl-mcp/tests/fixtures/mcp_parity.json`：文档名与哈希
- `rust/assets/templates/system_prompt.md`：文档清单加一行
- `rust/assets/config-templates/config.example.toml`：`[trigger_api]` 示例段
- `packages/cli/scripts/{build-host,install-smoke,prepare}.mjs`：载荷登记新二进制

## 与设计稿的偏差（都已回写进用户文档）

1. **QQ 不能作为投递出口**：平台自 2025-04 起关闭主动消息，所有下发必须是带 `msg_id` 的
   被动回复。外部触发没有入站消息可回复，因此 `type = "qqbot"` 在投递时直接拒绝并说明原因，
   而不是发一个注定失败的请求。设计稿原本把它列为可用通道。
2. **`job.rs` 并入 `pool.rs`**：一个 loop 的生命周期只有「装配 → 跑回合 → 收尾」三步，
   且全部由 slot 线程串起来，拆成两个模块只会多一层无收益的间接。
3. **`settings.rs` 并入 `config.rs`**：运行期设置只有 `[trigger_api]` 一段，没有第二份输入。
4. **`EVENT_CURSOR_EXPIRED` 语义收紧**：只有「游标超前于已产生的最新事件」才算过期。
   游标落在保留窗口之前不是错误——终态事件永远是最新一条（与本地 API 的 `RUN_ACTIVE`
   口径差异同理：这里不能因为没跟上增量就让整轮订阅失败）。
5. **投递失败的错误码取消**：`NOTIFY_TARGET_NOT_ALLOWED` 从 HTTP 错误码里删掉。触发在受理时
   已经跑完了，投递失败是记录上的事实（`notify.error`），不是请求错误。
6. **`system_prompt_append` 走 `options.model.system_prompt`**：`PromptRuntime` 装配完成后
   不再暴露可变入口，因此无人值守约束在装配结果之后拼接，而不是塞进模板。
7. **`session_key` 在 `per_trigger` 下是 400**：设计稿只写了「keyed 时才生效」，实现选择显式
   拒绝——静默忽略会让调用方以为会话被复用了。

## 未验证项（本机禁止构建类命令）

- `cargo build` / `cargo test` 均未运行（用户明确禁止编译类验证）。
  已完成的是逐文件人工复核：外部 API 签名（`AgentService::spawn` / `start_run` /
  `events_after` / `cancel_run` / `get_run` / `current_session_id` / `close`、
  `connector_options_from_process`、`resolve_kernel_program`、`SessionStore::read_events`、
  `SessionEvent::to_dict`、`TelegramApi::call`、`split_message`、`load_feishu_config`、
  `FeishuApi::send_message`、`load_trigger_api_configuration`）逐个对照过源文件。
- `mcp_parity.json` 的文档哈希是按当前 `rust/assets/docs/trigger_api.md` 实算的
  （sha256 `cdf4aea7…`）；若文档再改动必须同步重算，否则 `server_parity` 测试会失败。
- 运行时行为（内核启动、真实投递、并发排队）尚未实机验证。

## 后续可做

- 设置面板加「触发接口」页（当前只能手改 `config.toml`）。
- `session_mode = "keyed"` 目前只保证**同名串行**（已在 `TriggerStore::take_next` 实现并测试），
  **尚未**复用历史会话 ID：`AgentService` 的 `RunnerOptions.session.session_id` 在 `spawn` 内部
  构造，无法从 `ServiceOptions` 指定；要真复用需要给 `AgentService::spawn` 加一个会话 ID 入参。
  文档已如实写明这一点，不要当成已支持。
- 记录持久化（当前进程内内存，重启即清空）。
