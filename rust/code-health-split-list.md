# 生产行超一千的文件清单与拆分优先级

> 口径：生产行 = 剥离 `#[cfg(test)]` 块与 `#[test]` 后的物理行数（`rust/scripts/size_guard.py` 同口径）。
> 复现脚本：`.omnicrawl/.agent_tmp/scripts/show_impls.py`（列每个文件的 impl 块与函数名）+ `audit_scan.py`（行数）。
> 门禁名单：`rust/scripts/size_guard_allowlist.txt`，只减不增；拆完一个就从名单删一行。

## 2026-11-19 实测

**口径**：生产行 = 剥离 `#[cfg(test)]` 块与 `#[test]` 函数后的物理行数。总 40 个文件，全仓生产行 238,849。
**判定依据**：不看行数排座次，看**同一个文件里是否塞了互不相关的职责**。判据是读每个文件的 impl 块与函数名（脚本 `.omnicrawl/.agent_tmp/scripts/show_impls.py` 可复现）。

结论先行：40 个里**只有 6 个真正是「多功能综合文件」**，另有 4 个是「规模大但职责单一」，其余 30 个是单一职责或纯数据表——**按行数一刀切会白干**。

---

## 第一档：多功能综合文件（优先拆，6 个）

这 6 个文件内并存 ≥3 块互不相关的职责，是最该先动的。

### 1. `omnicrawl-connectors/src/qqbot/bot.rs` — 2450 行（测试仅 75）

职责块：`ReplyHandle`(25 个方法，分段/预算/配额/媒体/引用) + `ActiveBudget`(限速) + `QqBot`(55 个方法：网关循环/会话/群记忆/任务队列/审批/思考开关/工作区/附件) + `TaskTimeline` + 两个 handler impl。

**拆法**：`reply.rs`（ReplyHandle+ActiveBudget，约 700）、`timeline.rs`（与 feishu/telegram 同名文件对齐）、`media.rs`（`process_attachments` 111 行 + `quoted_*`）、`groups.rs`（群记忆 `record_group_message`/`store_group_record`/`has_group_context`）、`commands.rs`（`handle_thinking`/`handle_workspace`/`status_text`/`help_text`/`suggest_command`/`edit_distance`/`match_answer`），`bot.rs` 只留运行主线。

### 2. `omnicrawl-tui/src/ui/settings/render.rs` — 2116 行（测试 69）

52 个**全是顶层自由函数**、零个 impl；`render_*` 29 个，每个设置页各写一遍列表+表单+下拉覆盖层。混着两类东西：通用绘制原语（`draw_box`/`paint_hover`/`inset_horizontal`/`progress_bar_text`/`fit`）与 12 套页面渲染器。

**拆法**：先抽 `draw.rs`（通用原语），再按页拆 `pages/{model,embedding,decision,channel,mcp,tools,vision,subagents,tts,sessions,memory,knowledge}.rs`。**注意**：`render_decision_list`(162)、`render_embedding_list`(127)、`render_model_list`(109)、`render_channel_form`(109)、`render_decision_form`(101) 五个巨型函数要先内部拆分，否则只是搬家。

### 3. `omnicrawl-host/src/sandbox/windows.rs` — 2034 行（测试 279）

职责块：容器生命周期（`Container`/`ensure`/`destroy_container_named`）、ACL 授权（`grant`/`revoke`/`edit_ace`/`read_dacl`/`equal_sid_in_ace`/`grant_ancestors`）、环境构造（`build_environment_with` 155 行）、运行时探测（`prepare_runtimes`/`probe_runtime`/`probe_rustup`）、Job 对象与终结（`Terminator`/`create_job`/`kill`）、网络能力（`LoopbackExemption`/`CapabilitySids`/`exempt_loopback`）、子进程管道（`SandboxChild`/`spawn_pipe_reader`）、一堆 RAII `Drop`。

**拆法**：`container.rs`、`acl.rs`、`environment.rs`、`runtime.rs`、`process.rs`、`network.rs`。**这是全仓唯一需要 Win32 上下文理解的拆分，风险最高，建议放在后面做**。

### 4. `omnicrawl-tui/src/state.rs` — 1973 行（测试仅 2，已外移）

职责块：`Composer`(54 个方法：文本/历史/粘贴/图片/文档占位/键盘/选区/换行滚动) + `AppState`(20 个方法，含 `apply` 237 行) + `replay_*`(9 个事件重放) + `TextSelection`/`ReasoningReveal`/`RateEstimator`/`ToolCard`/`UserRecord` 等小类型。

**拆法**：`state/composer.rs`（Composer 独占约 600 行）、`state/replay.rs`、`state/records.rs`、`state/selection.rs`、`state.rs` 只留 `AppState` 骨架与 `apply`。**已经存在 `state/` 子模块（activity/composer_flow/replay/tests），按同构方式补即可，无需新设计**。

### 5. `omnicrawl-tui/src/app/support.rs` — 1225 行（测试 0，全自由函数）

61 个顶层自由函数，塞了三类：配置↔表单值互转（`*_form_values` 10 个、`*_from_config`/`*_config_from_row` 各 8 个）、配置→运行选项构造（`generation_options`/`tts_options`/`compaction_settings`/`build_review_options`）、以及杂项格式化（`feature_label`/`effective_context_window`/`is_retryable_failure`）。

**拆法**：`support/settings_values.rs`（表单值互转）、`support/options.rs`（运行选项构造）、`support/format.rs`（格式化与判定）。这是纯搬家，**风险低、收益直接**。

### 6. `omnicrawl-api/src/service.rs` — 1387 行（测试仅 2）

`impl Interactor for ServiceInteractor` 一个块装了 37 个方法：9 个 `decide`/`answer` 类核心 + `options_from_process_profile`(183 行) + 二十多个 `*_failed`/`*_unavailable` 错误构造器 + `build_mcp_manager`/`build_prompt_runtime`/`session_config_defaults` 等装配逻辑。

**拆法**：`service/errors.rs`（二十多个错误构造器，纯搬运）、`service/options.rs`（`options_from_*` 三个函数 + `generation_options`/`runner_options`）、`service/interactor.rs`（只剩 `decide`/`answer`/`question_kind` 等协议实现）。`options_from_process_profile` 183 行需内部拆分。

---

## 第二档：规模大但职责单一（4 个，拆了也没多少收益）

| 文件 | 生产行 | 为什么单职责 |
| --- | --- | --- |
| `host/src/tools/web_search.rs` | 2052 | 全是搜索引擎适配：4 个 HTML 解析 + 5 个 API 客户端 + URL 工具 + 3 套重排。**可拆但要按引擎分组，不是按职责** |
| `tui/src/ui/conversation.rs` | 1921 | `impl DisplayLine` 68 个方法全是「把记录渲染成行」，同一件事的不同阶段。测试 2441 行已外移 |
| `llm/src/desensitization/rules.rs` | 1608 | `PatternRule` 66 个方法全在写各类脱敏规则（URL/邮箱/银行卡/车牌/IP/PEM/DB URI），是规则表 + 匹配助手 |
| `controllers/src/approval.rs` | 1552 | `ApprovalMode` 66 个方法全在判「这个工具调用危险吗」：git action 表 + shell 下载执行检测 + 删除意图匹配 + 审查提示词 |

这四个如果拆，正确切法是**按数据/规则分组**（如 `rules/{url,email,bank,plate,ip,pem}.rs`），收益在可读性而非解耦。

---

## 第三档：单一职责的中大型文件（30 个，不建议本轮动）

按行数降序，这些文件内部只做一件事，拆开只会增加模块跳转成本：

`workspace/src/agent_isolation.rs` 1871（worktree 全生命周期）、`connectors/src/feishu/bot.rs` 1620、`tui/ui/fullscreen/rendering/latex.rs` 1597（LaTeX→文本）、`tls/src/session.rs` 1592（TLS 握手状态机）、`jsdom/src/browser_env/media.rs` 1488（canvas/webgl/audio 安装）、`mcp/src/client.rs` 1374、`api/src/routes/settings.rs` 1301（11 个 `put_*` 端点）、`llm/src/desensitization/engine.rs` 1300、`host/src/tools/knowledge.rs` 1293、`connectors/src/telegram/bot.rs` 1268、`session/src/store.rs` 1257、`tui/ui/settings/state.rs` 1238（只剩 `SettingsValues` 构造器）、`tui/app/mod.rs` 1222（`new` 329 行是唯一问题）、`host/src/tools/monitor.rs` 1221、`host/src/turn.rs` 1207、`jsdom/src/fetch_api.rs` 1186、`llm/src/runtime.rs` 1168（4 个 runtime 各一套，结构对称）、`controllers/src/tool_args.rs` 1155、`session/src/memory_store.rs` 1133、`ipc/src/bridge.rs` 1124（协议类型定义）、`controllers/src/subagents/tasks.rs` 1107、`host/src/tools/git_native.rs` 1100、`tui/app/settings_apply.rs` 1092（22 个 `apply_*` 对称）、`controllers/src/subagents/definitions.rs` 1073、`jsdom/src/engine.rs` 1067、`controllers/src/context_compaction/policy.rs` 1048、`api/src/shared_store.rs` 1043、`api/src/connector.rs` 1014、`host/src/review.rs` 1012、`host/src/tools/grep.rs` 1011。

---

## 建议顺序

1. `tui/app/support.rs`（纯搬家，零设计，风险最低）
2. `api/service.rs` 的错误构造器（同样纯搬家）+ `tui/state.rs` 补 `state/` 子模块（同构已有）
3. `tui/ui/settings/render.rs`（先抽 `draw.rs` 原语，再按页拆）
4. `qqbot/bot.rs`（与 feishu/telegram 对齐目录结构）
5. `sandbox/windows.rs`（Win32，最后做）

**门禁配合**：每拆完一个就从 `rust/scripts/size_guard_allowlist.txt` 删掉对应行——名单只减不增，拆不动就会被 CI 拦住，这是防止半途而废的机制。
