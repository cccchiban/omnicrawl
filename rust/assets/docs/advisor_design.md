# Agent 顾问策略（Advisor）设计文档

> 文档性质：现行系统设计（已实现并运行）。本文与源码核对于 2026-09-12；如与代码、测试不一致，以代码与测试为准。
> 何时读取：需要理解、调试或修改 `advisor` 工具、`[advisor]` 配置、`/advisor` 命令、设置面板「顾问设置」页或顾问调用链路时。
> 快速落点：核心实现 `omnicrawl/agent/controllers/advisor.py`；配置 `omnicrawl/config/features/advisor.py`；测试 `tests/test_advisor.py`、`tests/test_advisor_settings.py`。

## 1. 定位与数据流

顾问策略让执行者模型（executor）在关键时刻拿到一个更强模型的「第二意见」：执行者调用零参数 `advisor` 工具，宿主把当前整段工作上下文（任务 + 工具调用 + 结果）转发给已配置的**顾问模型**（advisor model），返回 `plan` / `correction` / `stop` 三类纯文本指导，作为工具结果回到执行者回合，执行者消化后继续。

设计原则是**尽可能薄**：不引入新 Agent、后台任务或新会话，复用宿主已有的单 Agent 工具循环。顾问是一次「旁路单轮补全」，与 SubAgent 的区别：

| 维度 | SubAgent | Advisor |
|---|---|---|
| 是否独立循环 | 是（独立上下文与模型快照） | 否（旁路单轮补全） |
| 工具面 | 有（受限角色工具表） | 无（`tools=[]`） |
| 转录 / 审批 / 隔离 | 有 | 无（只读旁路，不进转录） |
| 结果形态 | 结构化工作报告 | 纯文本 plan/correction/stop |

数据流：

```text
执行者回合（AgentLoopRunner 循环）
  │ 执行者调用 advisor()（零参数）
  ▼
工具批次执行（Host 侧 _execute_tool_batch）
  │ ① 冻结顾问模型 → 构造独立 Runtime 与协议对象（tools=[]）
  │ ② 构造转发分支：当前工作消息 + 剥孤儿调用 + user 尾 + 工具清单前置
  │ ③ 单轮补全（响应取消；空响应有界重试一次；aborted/error 短路）
  ▼
ToolResult（纯文本指导 / 错误信封）
  │
  ▼
回到执行者回合 → 消化后继续（可再次申请）
```

来源：按 rpiv-advisor（Pi Agent 扩展，MIT）的顾问策略语义移植落地；本文件描述的是当前实现事实。

## 2. 行为规则（维护红线）

以下规则是顾问策略的对外语义与内部契约，修改实现时必须保持：

1. **旁路单轮补全，不是 SubAgent**：不产生会话转录、无工具、无审批、无 worktree/隔离；只在协议层发起一次模型请求（外加空响应重试），把纯文本装进 `ToolResult`。
2. **顾问模型 = 独立冻结的 LLMConfig + 独立 Runtime**：用 `apply_model_selection` 从执行者配置解析出顾问模型，复制冻结后自建 `ModelRuntimeManager` 与 `AgentLLMProtocol`；绝不复用执行者回合的 `runtime_snapshot`（不同模型/凭据）。
3. **默认关闭、零成本**：`[advisor]` 未启用（`enabled=false` 或 `model_key` 为空）或当前执行者模型命中黑名单时，`advisor` 不进工具表、system prompt 不渲染使用准则区块、`ask_user` 超时也不附加顾问托管提示——三处使用同一可用性判定（启用且已选模型、且未命中黑名单；`_advisor_is_active`）。不在工具表里保留"永远失败的 stub"。
4. **执行者黑名单语义**：`disabled_for_models` 各项按子串匹配当前执行者模型的 `catalog_key` / `profile_id` / `model`（大小写不敏感），用于表达"弱模型不能咨询强顾问"。
5. **消息协议安全**：转发前剥掉尾部 in-flight 的 `advisor` 工具调用（孤儿 toolCall 会被 OpenAI/Anthropic 等拒收），并保证尾部是 user 消息（部分模型拒绝 assistant 结尾）。
6. **转发的分支是「当前工作投影」**：使用当前回合执行者正在看的工作消息（存活于上下文压缩的投影），而不是完整会话转录或旧历史。
7. **有界重试与短路**：空响应做一次相同输入重试；aborted/error 短路不重试；整个调用响应取消（ESC 后顾问请求不继续在后台消耗费用）。
8. **错误统一转信封**：解析、初始化、请求类错误全部转换为 `ToolResult(ok=False)` 错误文案返回执行者，不打断工具批次（`AgentError` 除外）。
9. **结果与凭据隔离**：成功结果的 details 只含 `advisor_model` / `effort` / `usage`；顾问凭据解析复用既有 models.toml/profile 机制，不写入任何结果、日志或会话转录。

## 3. 转发内容与消息按摩

### 3.1 分支来源

- 回合内由 `_request_agent_reply` 把「模型当前正在看的分支」保存到 turn-local 槽（`_advisor_turn_messages`），advisor 工具读取它构造分支——工具在线程池执行，避免读到旧状态。
- 槽不存在时兜底读 `_history`；两者都为空时返回错误信封「当前没有可评审的工作上下文」。
- 转发内容为当前 `working_messages` 投影（含压缩后的摘要形态），不把会话存储全量倒给顾问。

### 3.2 消息按摩（`build_advisor_branch`）

1. `strip_inflight_advisor_call`：从尾部向前定位最后一条 assistant 消息，剥掉其中 `name=advisor` 的调用（它还没有对应工具结果；其余并行调用保留）；剥后无剩余则移除 `tool_calls` 字段。已配对的历史调用不受影响。
2. `ensure_user_tail`：尾部不是 user 时追加一条极简 user 消息（`请基于以上执行者工作情况给出 plan/correction/stop 指导。`）。

### 3.3 工具清单前置（`executor_tool_inventory`）

- 在分支前追加一条 user 消息：`## Available Executor Tools` + 每个工具一行 `- <name>: <description>`。
- 描述折叠为单行空白；按工具名排序，稳定序列化（供 Provider prompt 缓存对齐）。
- 目的：顾问不持有工具，但需要知道执行者能调用什么来判断「工具选择是否恰当」。清单来源为执行者当前工具表 `self._tools`。

## 4. 顾问侧调用与结果信封

`AdvisorMixin._call_advisor` 的调用序列：

1. **冻结模型**：`apply_model_selection(parent_llm, advisor.model_key)` 解析选择（支持 models.toml key/alias、`profile/model_id`、裸 model_id 三态）→ `llm_config_to_profile_and_descriptor()` → 复制冻结 `provider_options` 等可变映射。
2. **独立 Runtime**：`ModelRuntimeManager().bootstrap(profile, descriptor)`；bootstrap 失败时清理半初始化 Runtime 并返回错误信封；调用结束 `finally` 关闭。
3. **独立协议对象**（`AgentLLMProtocol`）：
   - `client=None`（统一 Runtime 路径，不创建父 client）；
   - `tools_provider` 返回 `[]`（顾问永不调工具）；
   - `system_prompt_provider` 返回内置模板 `omnicrawl/templates/advisor_system.md`；
   - `reasoning_effort_provider` 返回配置的 `display_effort`；
   - `request_retry_count=1`：空响应由协议层做一次相同输入重试；
   - 超时取执行者与顾问配置的较小值；`prompt_cache_identity` 固定为 `{workspace, advisor: "system", model: <选择>}`。
4. **单轮补全**：`request_reply`（`on_delta`/状态回调为空实现），携带执行者 `cancel_check` 以响应取消；token 用量经 `_record_advisor_usage` 记入宿主统计（供 UI/API 用，不进会话正文）。
5. **空响应**：重试后仍为空 → 错误信封「顾问连续两次返回空响应，请稍后重试。」。

结果信封：

| 场景 | 结果 |
|---|---|
| 成功 | `ToolResult(ok=True, output=纯文本, full_output=纯文本, ui_artifact={"advisor": {advisor_model, effort, usage?}})`，usage 含 `input_tokens`/`output_tokens`/`cached_input_tokens` |
| 未启用 / 黑名单 / 无上下文 | `ToolResult(ok=False, output=可操作错误文案)` |
| 模型无法解析 / Runtime 初始化失败 / 请求失败 | 同上，文案含失败环节 |

顾问系统提示（`advisor_system.md`）的核心约束：只返回 `plan`/`correction`/`stop` 三类之一（英文单词开头）；纯文本、不调工具、不输出 JSON 或代码围栏；只面向执行者、绝不产生面向最终用户的展示文本；点名具体文件、函数、行号或工具名；不加前言道歉、不复述执行者已知内容。

## 5. 使用准则（执行者引导区块）

`building.py::_advisor_guidelines_block()` 仅在 advisor 真正可用时，向基础 system prompt 之后、活动模式区块之前追加「顾问策略（advisor）使用准则」：

- 适合调用：重大实质工作（写代码、下结论）之前；反复失败或方案不收敛（卡住）时；换方向之前；长任务承诺方案前至少一次、声明完成前至少一次（先落盘再调用）。
- 不适合调用：短任务且下一步由刚读到的工具输出直接决定时；只做定向探索时。
- 向用户提问超时或用户未作答（`ask_user` 超时/返回失败）时，可调用一次 `advisor` 代替用户评估并给出合理决策方向，避免任务空等；顾问决策不得代替高危或需审批操作的明确用户授权。
- 收到指导后给其实质权重；若与自身观察到的证据冲突，用一次 `advisor` 把冲突摆给顾问做 reconcile，不盲从也不盲弃。
- 调用后必须在**下一条对用户可见的回复**中转述关键指导（工具卡片对用户是折叠的）。

`ask_user` 超时提示由 `shared.py::ask_user_advisor_hint` 生成，与工具表注册、指引区块使用同一可用性判定。

## 6. 配置、命令与界面

### 6.1 `[advisor]` 配置（config.toml）

| 字段 | 默认 | 说明 |
|---|---|---|
| `enabled` | `false` | 显式启用；未启用时 advisor 不进工具表 |
| `model_key` | `""` | 顾问模型引用：models.toml key/alias、`profile/model_id` 或裸 model_id |
| `effort` | `high` | 推理档位：`none` / `low` / `medium` / `high` / `xhigh` / `max` |
| `disabled_for_models` | `[]` | 执行者黑名单（子串匹配，见 §2-4） |

- 读 / 写 / 清除：`load_advisor_config` / `save_advisor_config` / `clear_advisor_config`；写回时保留 config.toml 其他段。
- `AdvisorConfig.active = enabled 且 model_key 非空`；effort 非法值在构造时即报错。

### 6.2 `/advisor` 命令

| 用法 | 行为 |
|---|---|
| `/advisor` | 查看当前顾问与用法；未启用时列出可用模型候选 |
| `/advisor <model_key> [effort]` | 校验选择可解析后保存；缺省 effort=`high` |
| `/advisor off`（`none`/`clear`/`no` 同义） | 清除选择并关闭；工具即时剥离 |
| `/advisor help`（`-h`/`--help`） | 帮助信息 |

保存后同步内存 `config.advisor` 并重建工具表，使 advisor 工具即时出现/消失；工具表重建失败时报告已保存但刷新失败。

### 6.3 设置面板「顾问设置」页

- 位置：`/settings` 左侧列表"模型渠道"之后（`_SETTING_ORDER` 中 `advisor`）。
- 组成：启用开关 + effort 下拉（中文标签）+ 当前顾问摘要 + 内嵌双列模型选择器。
- 模型选择器复用主模型同款 `ModelPickerPane`，但 `selection_only=True` 且 `current_override` 指向已选顾问：**只选择不切换主模型**；启用但未选模型时拒绝保存。
- 保存语义：先写 config.toml `[advisor]` → 调 `agent.set_advisor_configuration`（类型校验 + 事务式更新 `config.advisor` + 重建工具表；失败回滚磁盘与内存）。

### 6.4 状态上报

调用顾问期间通过当前工具批次的 status 通道上报 `正在咨询顾问（<model_key>，effort=…）…`，结束清空；上报失败不得中断顾问调用。

## 7. 实现落点

| 职责 | 文件 |
|---|---|
| 核心 Mixin 与纯函数（分支构造 / 按摩 / 工具清单 / 调用） | `omnicrawl/agent/controllers/advisor.py` |
| 配置定义与读写 | `omnicrawl/config/features/advisor.py` |
| 顾问系统提示模板 | `omnicrawl/templates/advisor_system.md` |
| 工具定义（零参数、无审批、有界输出） | `omnicrawl/agent/toolkit/tools.py`（`ADVISOR_TOOL_NAME`、`_meta_tool_definitions`） |
| 工具注册/剥离 + 使用准则区块 | `omnicrawl/agent/controllers/tools/building.py`（`_build_tools`、`_advisor_guidelines_block`） |
| 回合接线（工作消息快照、状态上报、ask_user 超时提示） | `omnicrawl/agent/controllers/turn/loop.py`、`omnicrawl/agent/controllers/shared.py` |
| Agent 配置字段与 Mixin 组合 | `omnicrawl/agent/core.py` |
| `/advisor` 命令 | `omnicrawl/commands/slash.py`（`handle_advisor_command`） |
| 设置面板与路由 | `omnicrawl/ui/fullscreen/screens/advisor_settings.py`、`settings.py`、`model_picker.py` |
| 运行时配置 setter | `omnicrawl/agent/controllers/session/settings.py`（`set_advisor_configuration`） |
| 连接器命令分发 | `omnicrawl/connectors/fsapp.py`、`omnicrawl/connectors/telegram.py` |
| 测试 | `tests/test_advisor.py`、`tests/test_advisor_settings.py` |

## 8. 维护与验证

修改顾问策略后按下表检查一致性：

| # | 检查项 | 期望 |
|---|---|---|
| 1 | 未启用 / 黑名单命中 | 工具表无 `advisor`；system prompt 无准则区块；`ask_user` 超时无托管提示 |
| 2 | 工具注册链路 | `tools.py` 定义、`building.py` 注册、Mixin runner 三处接线完整（新增工具按同模式） |
| 3 | 消息按摩 | 转发分支无孤儿 advisor 调用、尾部为 user |
| 4 | 错误路径 | 解析/初始化/请求失败均返回错误信封而非抛异常 |
| 5 | 生命周期 | 每次调用自建 Runtime 并在结束时关闭；不写会话转录、不触发审批 |

测试与回归：

```bash
python -m pytest tests/test_advisor.py tests/test_advisor_settings.py
```

关联回归范围：`agent` / `commands` / `tui` / `config` / `subagent` / `model_runtime` 相关用例。

## 9. 已知边界

- **成本放大**：每次调用都把整个工作分支按顾问模型计费；靠默认关闭、提示词纪律与黑名单控制，不做硬性次数限制（reconcile 场景需要灵活度）。
- **顾问无工具**：默认形态即「只思考、只输出指导」。若未来需要"顾问也能动手"，应让其退化为受限 SubAgent，而不是给旁路补全加工具面。
- **effort 支持**：档位是否真正生效取决于具体模型与网关支持情况。
- **转发范围**：只含当前工作投影（压缩后形态），不含更早的完整历史原文。
- **费用与 undo**：advisor 调用不加入 undo 只读集合（与 subagent 一致，调用会产生外部模型费用）。

## 10. 参考

- rpiv-advisor 源码（MIT）：`juicesharp/rpiv-mono/packages/rpiv-advisor`。
- 工具系统与调用协议：`omnicrawl/docs/TOOL_CALLING.md`。
- 主要实现文件见 §7；内置文档 URI：`omnicrawl://docs/advisor_design.md`。
