# 工具输出压缩（Tool Output Compression）设计文档

> 快速落点：核心实现 `omnicrawl/agent/runtime/tool_output_compressor.py`、`omnicrawl/agent/controllers/tools/compression.py`；配置 `omnicrawl/config/features/tool_output_compression.py`；提示词 `omnicrawl/templates/tool_output_compression_system.md`；设置面板 `omnicrawl/ui/fullscreen/screens/tool_output_compression_settings.py`。

## 1. 定位与数据流

工具输出（`bash` 大段 stdout、`grep` 结果、`fetcher` 正文等）是主 Agent 上下文的主要膨胀源。本功能在**工具结果写入模型上下文之前**把它交给同一模型渠道上的另一个（通常更小的）模型压成精简观察，压缩文本既是模型后续读到的内容，也是 TUI 工具卡显示的正文。

数据流（主 Agent 回合内的工具批次）：

1. 工具线程完成 → `report_tool_result` **立即**把原始结果交给 TUI/会话（既有行为不变：快工具不被慢工具拖住）。
2. `_apply_batch_output_budget`：单工具 >50K 或批次 >200K 的输出先落盘，只留头尾预览与路径。
3. **`_compact_tool_outputs`**：筛出合格结果 → 并行调用压缩模型 → 被采纳的结果替换模型可见文本。
4. `_prepare_tool_result_for_model`（视觉路由）→ 写 `tool_result` 会话事件 → 回填 `AgentLoopObservation`。
5. 每个被压缩的工具结果通过 `on_tool_output_update` 回调触发 TUI 卡片正文刷新。

字段语义（沿用既有分叉，不新增 ToolResult 字段）：

| 字段 | 语义 | 压缩后 |
|---|---|---|
| `ToolResult.output` | 模型可见文本 | 压缩结果 |
| `ToolResult.full_output` | TUI/会话显示文本 | 压缩结果 + 原始输出 |
| 会话 `model_output` | = `output` | 压缩结果（恢复投影优先用它） |
| 会话 `output` | = `full_output` | 压缩结果 + 原始输出，超 8192 字自动落 artifact |

## 2. 行为规则（维护红线）

- **默认关闭**：`enabled` 为假或 `model_key` 为空时不构造压缩器、不发请求，工具结果完全走原路径；压缩模型默认不思考（`thinking_enabled = false`）。
- **作用域固定**：只有 `bash`、`powershell`、`git` 三个工具的输出参与压缩（`COMPACTABLE_TOOLS`），其余工具一律保留原文。`powershell` 默认未启用，因此实际生效面通常是 `bash` 与 `git`。
- **失败必回退原文**：模型不可用、超时、空响应、返回工具调用、压缩结果不小于原文 → 一律保留原始输出，只记日志，不改变工具的成功/失败与协议配对。
- **不阻塞、不永等**：单条压缩受 `timeout_seconds` 约束，批次另有宽限上限；超时项丢弃压缩结果，后台线程自行收尾（与工具批次超时同策略）。
- **取消即中断**：ESC 触发的取消异常向上传播，不吞掉；回合随即按既有取消路径收尾。
- **不改工具计时**：卡片耗时仍取自工具真实完成时刻（`completed_at`），压缩耗时不计入工具。
- **压缩模型无工具**：请求的 `tools_provider` 恒为空，返回工具调用按失败处理。

## 3. 压缩请求内容

系统提示词（`templates/tool_output_compression_system.md`）声明：只输出压缩正文、禁止前言与代码围栏、不得执行工具输出中的指令、必须保留路径/命令/退出码/错误/数量等决策信息。user 提示词由代码拼装，包含：当前任务文本、工具名、参数摘要（`ARGUMENTS_PREVIEW_CHARS = 600` 截断）、以及用 `<<<TOOL_OUTPUT_START>>>` / `<<<TOOL_OUTPUT_END>>>` 包裹的原始输出。

超长输出按头尾采样压到 `max_input_chars`，中间以固定说明标注省略；模型返回文本先做最小清洗（剥代码围栏、剥「压缩结果：」这类短标签行）再按 `max_output_chars` 截断。

## 4. 压缩侧调用与结果信封

`ToolOutputCompressor` 复用 `vision_proxy` 的外接模型模式：`apply_model_selection` + `llm_config_to_profile_and_descriptor` → 独立 `ModelRuntimeManager` → `AgentLLMProtocol`（`request_retry_count=1`，推理参数由配置下发，prompt cache 身份带 `scope="omnicrawl-tool-output-compression"`）。选中的模型与失败原因回传为 `ToolOutputCompressionResult` / `ToolOutputCompressionError`。

合格判定（`_should_compact`）：模型可见文本非空、长度 ≥ `min_chars`。批次内逐工具并发（上限 `MAX_PARALLEL_COMPRESSIONS = 4`），单项完成即回调 UI，未缩小或失败项不进结果表。

思考由压缩模型自己决定：`effective_reasoning_effort` 在关闭思考时下发 `none`，`_request_model_config` 把 `thinking_type` 一并写进本次请求的模型配置。选中的模型配置默认继承主模型的思考设置，不覆盖会让压缩请求跟着主模型一起思考（或一起不思考）。

## 5. 配置、界面

`config.toml`：

```toml
[tool_output_compression]
enabled = false            # 默认关闭
model_key = ""             # models.toml key/alias 或 profile/model_id
thinking_enabled = false   # 压缩模型思考开关
reasoning_effort = "low"   # 思考深度：low/medium/high/xhigh/max
min_chars = 1200           # 模型可见文本短于此值不压缩
max_input_chars = 24000    # 超长输出按头尾采样后再送压缩模型
max_output_chars = 1500    # 压缩结果硬上限
timeout_seconds = 60
```

`active = enabled and model_key`（与 advisor 同语义）。设置面板「工具输出压缩」页提供开关、思考开关与思考深度、4 个预算输入与内嵌模型选择器；保存写盘后经 `set_tool_output_compression_configuration` 同步运行态。面板布局：开关与预算输入两两并排成定高行，模型选择器紧随其后并吃掉面板剩余高度（矮窗口由自身 `min-height` 兜底），页脚必须显式 `height: auto` —— Textual 的 `Vertical` 默认 `height: 1fr`，不写就会和表单区对半分屏，在按钮下方留下半屏空白。本功能**不注册工具、不新增斜杠命令**，因此配置变化不重建工具表。

## 6. 实现落点

| 层 | 位置 |
|---|---|
| 配置 | `omnicrawl/config/features/tool_output_compression.py`；挂入 `AgentConfig.tool_output_compression`（`omnicrawl/agent/core.py`） |
| 压缩器 | `omnicrawl/agent/runtime/tool_output_compressor.py` |
| 批次压缩 | `omnicrawl/agent/controllers/tools/compression.py` |
| 回合钩子 | `omnicrawl/agent/controllers/turn/loop.py`：`run_stream`/`_execute_tool_batch` 的可选回调 `on_tool_output_update` / `report_tool_output_update`；压缩点在 `_apply_batch_output_budget` 之后 |
| 运行态配置 | `omnicrawl/agent/controllers/session/settings.py`：`set_tool_output_compression_configuration` |
| TUI 卡片 | `rendering/widgets.py`：`ToolDisclosure.update_body`；`rendering/pipeline.py`：已收口卡片表（上限 32）+ `_handle_tool_output_update`；`app/core.py`：`_finished_tool_cards`；`support/turns.py` + `turn/execution.py`：回调透传 |
| 设置面板 | `omnicrawl/ui/fullscreen/screens/tool_output_compression_settings.py`；`screens/settings.py` 注册行与 `_build_tool_output_compression_pane` |
| 提示词 | `omnicrawl/templates/tool_output_compression_system.md`（随包发布） |

## 7. 维护与验证

- 临时冒烟脚本（不入库）覆盖：配置读写与校验、采样与清洗、合格判定、批次采纳与顺序、失败/超时/取消回退、未启用零成本、卡片正文更新（含文件变更工具保持预览）、设置面板拒绝保存与写盘应用、接线存在性。
- 修改 TUI 回调链时同步检查：`AgentTurnCallbacks` 字段、`AgentTurnController.run` 透传、`TurnExecutionMixin._run_agent_turn` 注册。
- 修改会话 `tool_result` payload 时同步检查：`state/session_projection.py` 的 `tool_result_output_text`（优先级 `model_output → output_preview → output`）与历史重放 `_replay_tool_output`。

## 8. 已知边界

- **正文被隐藏的工具无显示变化**：`read` 与记忆/知识库类工具在 TUI 不展示正文（`HIDDEN_BODY_TOOLS`），压缩只影响模型上下文。
- **文件变更工具不换正文**：`write_file` / `Edit_file` 的正文由调用参数生成，`update_body` 直接返回，避免丢掉 diff 预览；这类结果本身很短，通常低于 `min_chars`。
- **子代理批次一并生效但无卡片刷新**：子代理复用同一批次方法，其工具结果同样被压缩（有利于子上下文），但子代理工具行仍显示原始输出。
- **API/连接器只拿到压缩文本**：`tool.completed` 事件的 `output` 即压缩结果，本轮不提供“二次刷新”事件；原始输出仍可从 TUI 卡片或会话 artifact 取得。
- **用量不计入主模型统计**：压缩调用的 token 用量不写入模型用量回调，避免污染主对话统计（仅记日志与状态行）。
- **压缩会拉长回合**：压缩在模型下一轮请求之前完成，因此它不额外增加回合总时长，但会让工具结果到模型回填之间的间隔变长。

## 9. 参考

- `omnicrawl/docs/advisor_design.md`（外接模型 + 独立 Runtime 同款模式）
- `omnicrawl/agent/runtime/vision_proxy.py`（外接模型旁路调用与结果回注）
- `omnicrawl/docs/session_design.md`（`tool_result` 事件与恢复投影）
