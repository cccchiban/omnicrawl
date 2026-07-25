# OmniCrawl 上下文压缩设计

> 文档性质：设计与实施基线。第一至第四阶段已于 2026-07-20 落地。
> 设计范围：同一 Session 内的长对话压缩、缓存友好策略和可恢复上下文组装。
> 当前基线：OmniCrawl 已实现本地确定性 `/compact`、显式 `/compact --model`、结构化摘要校验、默认关闭的 70K 自动批量压缩、4 回合冷却、85% 紧急旁路、单回合大型载荷分块摘要及按事件 ID 恢复精确证据。
> 已确认策略：新功能默认关闭；开启后仅在完整回合结束时检查，预计下一次请求达到 `70,000 Token` 时批量压缩；两次模型尝试之间至少冷却 4 个完整回合，容量紧急时可绕过冷却。
> 证据恢复：开启模型压缩时注册只读工具 `recall_session_evidence`，只允许读取当前有效摘要引用的当前 Session 事件。

## 0. 先说人话：这个思路到底是什么

> 第一至第四阶段已经上线；本节用简化方式解释现有上下文压缩与按需证据恢复思路。

### 0.1 一句话说明

**完整聊天记录继续保存在本地，但每次请求模型时，不再把所有旧内容都带上，只携带一份短小、结构化的“工作便签”和最近几轮原文。**

可以把它理解为：

- 完整 JSONL 转录是一本不会丢页的“账本”。
- 上下文摘要是一张随身携带的“工作便签”。
- 最近几轮原文是当前正在处理的“桌面材料”。
- 需要核对细节时，再根据事件 ID 从账本中取回原文。

压缩不是删除历史，而是减少每次发给模型的重复内容。

### 0.2 压缩前后有什么区别

压缩前：

```text
每次请求模型
  = 项目规范
  + 第 1 轮完整对话
  + 第 2 轮完整对话
  + ...
  + 第 30 轮完整对话
  + 当前问题
```

压缩后：

```text
每次请求模型
  = 项目规范
  + 一份结构化工作摘要
  + 最近 6～10 轮完整对话
  + 当前问题
```

本地仍然保留第 1～30 轮完整转录，只是不再默认重复发送全部内容。

### 0.3 一个简单例子

假设一段长对话已经积累 `80K Token`，低成本模型把可退出热窗口的内容整理成 `6K Token` 摘要：

```text
压缩前每轮携带：80K 旧上下文
压缩后每轮携带： 6K 摘要 + 最近原文
单轮大约减少：  74K Token（这里只演示量级）
```

总结本身需要一次额外模型调用，也会改变稳定的缓存前缀。因此系统不能“每轮都总结”，而应在历史足够长或上下文容量接近阈值时批量压缩。

### 0.4 为什么不直接把所有内容永久保留在模型上下文中

即使模型支持很长的上下文，持续携带全部历史仍有四个问题：

1. 每轮都会重复计算大量旧输入 Token。
2. 缓存命中虽然便宜，但通常不是完全免费，而且有有效期和供应商差异。
3. 上下文越长，延迟通常越高，可用于新代码、日志和工具结果的空间越少。
4. 大量无关历史可能干扰当前任务，出现“重要信息埋在中间”的问题。

### 0.5 最终希望达到的效果

```text
完整历史可恢复
        +
模型每轮只看必要内容
        +
缓存前缀尽量稳定
        +
关键约束和证据不因压缩丢失
```

本文后续内容回答三个问题：什么时候压缩、压缩什么、如何避免摘要失真。

### 0.6 已确定的默认行为

```text
功能开关：默认关闭
检查时机：每个完整 user/assistant 回合结束后
触发阈值：预计下一次请求输入 >= 70,000 Token
压缩方式：一次批量合并所有可退出热窗口的完整旧回合
冷却时间：模型压缩后至少 4 个完整回合
紧急例外：达到活动模型上下文窗口的 85% 时可绕过冷却
```

“预计下一次请求”由稳定上下文、当前工作摘要、最近原文、工具定义和预留的下一条用户输入预算组成。只有功能开启后才进入这套 70K 评估流程。

开关关闭时把该功能视为不存在：不执行 70K 自动评估，不调用摘要模型，不增加单回合特殊处理、警告或拦截；继续完全沿用当前本地确定性自动压缩和普通 `/compact` 行为。即使单个回合达到 70K，也由当前普通压缩和模型请求流程按现状处理。

开关开启意味着新 service 整体接管自动压缩策略：未达到 70K 时不再运行当前按回合数触发的确定性自动压缩；只有模型摘要失败或校验失败时才调用确定性压缩降级。普通手动 `/compact` 始终保持当前行为。

## 1. 背景与现状

### 1.1 OmniCrawl 当前已有能力

当前实现已经具备以下基础：

| 能力 | 当前实现 |
|---|---|
| 完整会话转录 | `.agent_sessions/sessions/*.jsonl` 追加事件 |
| 会话恢复 | `SessionStore` 重放有效事件并重建模型消息 |
| 压缩边界 | `compact_summary` 事件 |
| 自动压缩 | 每个完整 user/assistant 回合后调用 `_compact_history(force=False)` |
| 手动压缩 | `/compact` 调用 `_compact_history(force=True)` |
| 最近窗口 | `max_history_turns`，当前默认值为 6 |
| 缓存 Token 统计 | `cached_input_tokens` 已能从多种模型协议读取 |
| 回退投影 | `/undo` 通过追加事件回退逻辑上下文，不删除原始转录 |

### 1.2 当前压缩方式的限制

当前摘要由本地确定性规则生成，不调用模型。它具有稳定、免费、离线可用的优点，但也有明显边界：

- 主要截取 user/assistant 文本片段，不能可靠识别目标、约束、决策和风险。
- 摘要长度按字符限制，不按 Token 预算和模型上下文占用决定。
- 不理解代码符号、否定条件、工具结果和任务状态之间的关系。
- 旧摘要再次压缩时容易变成“摘要片段 + 新片段”的堆叠。
- 目前不会根据事件引用自动取回已退出热窗口的精确证据。

因此，新方案不是替换完整转录，而是增加一个可选的、可验证的模型辅助摘要层。

## 2. 设计目标与非目标

### 2.1 设计目标

| 目标 | 可验证结果 |
|---|---|
| 节省上下文 Token | 压缩后每轮输入 Token 明显低于未压缩估算值 |
| 保持任务连续性 | 压缩后仍能回答当前目标、约束、已完成事项和下一步 |
| 降低上下文压力 | 达到阈值时为新代码、日志和工具结果释放空间 |
| 保留可恢复性 | 完整 JSONL 不删除，可按事件 ID 回取原始证据 |
| 利用缓存 | 不因过于频繁地重写摘要而持续破坏稳定前缀 |
| 可观测 | 能看到压缩前后 Token、缓存命中和质量校验结果 |

### 2.2 非目标

第一阶段不处理：

- 不自动把一个 Session 的摘要注入所有新 Session。
- 不把会话摘要直接写入长期记忆。
- 不删除或改写原始 JSONL 事件。
- 不撤销工具已经产生的文件、命令、网络或其他外部副作用。
- 不提供模型价格、金额节省、回本回合或 ROI 报告。
- 不使用摘要替代高风险操作所需的原始证据。

跨 Session 的相关摘要检索可以作为后续能力，第一阶段先保证单 Session 内正确。

## 3. 核心设计原则

### 3.1 完整转录是事实源

`SessionStore` 的 JSONL 事件始终是事实源。摘要只是模型上下文投影，不是历史本身。

```text
JSONL 原始事件
  ├─ UI 投影
  ├─ 导出投影
  └─ 模型上下文投影
       ├─ 结构化摘要
       ├─ 最近原文
       └─ 按需证据
```

### 3.2 自动模型压缩只在完整回合边界发生

模型流式输出、工具执行或审批等待期间不得压缩。自动模型压缩只在完整 user/assistant 回合结束、用量统计和 Session 事件落盘后进行一次评估。

唯一的受控例外是：若主模型在首次请求中、任何可见文本输出和工具执行之前明确返回“上下文容量超限”，系统可为这条未完成用户消息生成一次结构化摘要，追加内部续接消息并自动重试一次。该恢复路径不适用于已有流式文本、工具副作用、审批等待或非上下文错误；摘要失败或重试仍失败时保留中断状态，不会循环重试。

用户手动执行现有 `/compact` 仍使用本地确定性摘要，不受新功能开关影响。若后续提供 `/compact --model`，则必须先开启模型压缩功能；Session 恢复本身不自动调用摘要模型。

这样可以避免截断工具调用链、把未完成状态误写成已完成事实，也避免恢复 Session 时产生意外费用。

### 3.3 保留最近原文

摘要不能覆盖全部上下文。系统必须保留最近若干完整回合，让模型看到当前语气、精确需求和即时工具结果。

推荐同时使用两个限制：

- 最近回合数，例如 6～10 轮。
- 最近原文 Token 预算，例如上下文窗口的 20%～30%。

最终取更严格的限制，避免单个超长工具结果撑爆热窗口。

### 3.4 摘要必须结构化

不使用纯散文式总结作为唯一上下文。摘要至少包含：

```yaml
objective: 当前目标
constraints: 用户约束和技术边界
decisions: 已确认方案及重要取舍
completed: 已完成事项和验证结果
current_state: 当前工作状态
open_issues: 未完成事项、风险和阻塞
artifacts: 文件、命令、测试和产物引用
exact_evidence: 必须保留原文的错误、数字和事件引用
```

### 3.5 最新用户指令优先

摘要中的旧要求不得覆盖摘要后的新用户指令。构建模型上下文时顺序固定为：

```text
系统与项目规范
结构化摘要
最近完整对话
当前用户输入
```

### 3.6 用量报告只基于 Token

系统只记录可验证的输入、输出、缓存命中和压缩前后估算 Token，不维护模型价格，也不输出金额节省、回本回合或 ROI 结论。

## 4. 目标上下文分层

| 层级 | 内容 | 默认进入模型 | 保存位置 |
|---|---|---|---|
| 热窗口 | 最近完整 user/assistant、必要工具链 | 是 | 运行时 `_history` / Session 投影 |
| 工作摘要 | 目标、约束、决策、进度、问题 | 是 | `compact_summary` 事件 |
| 按需证据 | 精确错误、长工具结果、代码片段 | 相关时进入 | JSONL / artifact |
| 完整归档 | 全部原始事件 | 否 | `.agent_sessions/` |
| 长期记忆 | 跨会话稳定知识 | 检索后进入 | 现有 memory 系统 |

设计重点是让“热窗口 + 工作摘要”稳定且小，让“按需证据”可恢复但不常驻。

## 5. Token 与缓存测量

### 5.1 Token 账本

上下文压缩只维护可验证的 Token 用量，不做价格或回本推算。每次完整回合和压缩边界记录：

- 预计下一次请求输入 Token。
- 稳定上下文、已有摘要、冷历史和最近窗口 Token。
- 主模型与摘要模型的输入、输出和缓存命中 Token。
- 本次退出热窗口和新摘要的估算 Token。
- Token 估算与供应商实际 Usage 的偏差。

这些数据用于容量诊断、测试估算准确性和排查缓存变化，不换算为金额，也不参与收益预测。

### 5.2 缓存友好约束

缓存统计用于解释上下文行为并减少不必要的前缀变化：

- 系统规范、项目提示和工具定义等稳定内容继续位于请求前缀。
- 工作摘要写入后保持稳定，直到下一次达到压缩阈值。
- 每轮重写摘要会改变前缀，因此必须设置压缩冷却期。
- 压缩后的摘要一旦稳定，后续消息只在其后追加，可重新形成稳定缓存前缀。
- 缓存有有效期，长时间暂停后继续会话时不能假设旧前缀仍然命中。

### 5.3 已确认的自动触发规则

功能开启后，自动模型压缩采用固定、可解释的容量策略：

```text
允许自动压缩 =
  context_compaction.enabled == true
  且当前完整回合已经结束
  且预计下一次请求输入 >= 70,000 Token
  且至少存在一个可退出热窗口的完整旧回合
  且（距上次模型压缩 >= 4 个完整回合
      或预计下一次请求输入 >= 活动模型上下文窗口的 85%）
```

紧急线只能绕过 4 回合冷却，不能绕过功能开关，也不能绕过完整回合边界。Token 与缓存统计只用于诊断，不改变 `70K` 触发规则。

对于上下文窗口不足以安全容纳 `70K + 输出预留` 的模型，开启功能时应拒绝该配置并给出诊断，不能等待到 70K 才处理。功能关闭时不执行这项新增校验，继续沿用当前模型请求和本地确定性压缩行为。

## 6. 压缩触发策略

### 6.1 完整回合结束后的评估流程

每个完整回合结束后按固定顺序检查：

```text
1. 确认 user_message、assistant_message 和用量事件已经落盘
2. 若 context_compaction.enabled == false，退出新功能流程并继续当前普通压缩行为
3. 估算下一次请求的输入上下文
4. 若估算值 < 70,000 Token，立即结束
5. 有可退出热窗口的旧回合时走批量历史压缩
6. 没有旧回合但当前完整回合本身达到阈值时走单回合大型载荷压缩
7. 检查距离上次模型压缩是否已满 4 个完整回合
8. 未满冷却期时，仅在估算值达到上下文窗口 85% 时继续
9. 校验摘要后追加 compact_summary 事件并重建运行时窗口
```

下一次请求估算建议包含：

```text
稳定 system/project/工具上下文
+ 当前有效结构化摘要
+ 最近完整原文窗口
+ 尚需常驻的工具结果引用
+ next_user_reserve_tokens（默认预留 4,096 Token）
```

预留下一条用户输入是为了在用户粘贴代码或日志前提前释放空间；供应商返回的上一轮实际 `input_tokens` 只作为校准依据，不能直接替代下一轮估算。

### 6.2 不应压缩的情况

- `context_compaction.enabled` 为 `false`。
- 预计下一次请求输入低于 `70,000 Token`。
- 工具调用尚未返回结果。
- 助手仍在流式输出。
- 当前正在进行人工审批。
- 既没有可退出热窗口的完整旧回合，当前完整回合也不属于单回合大型载荷场景。
- 用户要求保留逐字原文且相关内容仍在热窗口。
- 距上次模型压缩不足 4 个完整回合，且尚未达到紧急容量线。
- 摘要模型不可用或摘要校验失败；此时按降级策略处理，不能静默丢弃历史。

### 6.3 批量压缩与 4 回合冷却

```yaml
trigger_context_tokens: 70000
minimum_turns_between_model_compactions: 4
emergency_context_ratio: 0.85
```

批量压缩是指触发时一次处理所有已经退出最近窗口、且属于完整回合的旧消息，而不是每新增一个回合就重写一次摘要。压缩完成后，摘要保持不变至少 4 个完整回合，让新的稳定前缀有机会被 Prompt Cache 重复利用。

当预计下一次请求达到活动模型上下文窗口的 `85%` 时，可以绕过 4 回合冷却并再次批量压缩；但功能开关关闭时，紧急线也不得调用模型。

### 6.4 单个回合达到 70K

本节只在 `context_compaction.enabled == true` 时生效。

完整回合结束后，如果预计下一次请求达到 `70,000 Token`，系统先寻找最近窗口之外的完整旧回合：

```text
存在可退出热窗口的旧回合
  -> 按正常路径批量压缩旧回合

不存在旧回合，且当前单个完整回合本身导致达到 70K
  -> 进入单回合大型载荷压缩
```

单回合大型载荷压缩不应丢弃最新任务本身，而应保留：

- 用户目标、约束和当前问题。
- 助手已经确认的结论、决策和下一步。
- 错误、路径、命令、代码符号等关键原文证据。
- 完整大型载荷对应的 JSONL 事件 ID 或 artifact 引用。

文档、代码、日志和大型工具结果按结构分块生成摘要；低成本模型无法一次读取时，先生成分块摘要，再合并为结构化工作摘要。第一次单回合压缩不受既往冷却限制，成功后开始计算后续 4 个完整回合的冷却期。

功能关闭时不进入上述分支。单个 70K 回合不会触发任何新增摘要、警告或请求拦截，仍按当前本地确定性压缩与模型调用逻辑处理。

## 7. 滚动结构化摘要

### 7.1 不反复总结全部历史

每次压缩只读取：

```text
上一份结构化摘要
+ 本次即将退出热窗口的新增原文
+ 必要的工具结果引用
```

不重新读取完整 Session，也不只对上一份自然语言摘要再次摘要。唯一例外是功能开启后的单回合 70K 路径：它只读取该大型回合并按结构分块，不回扫其他无关历史。

### 7.2 合并规则

| 字段 | 合并方式 |
|---|---|
| `objective` | 保留当前仍有效的最终目标，新用户目标可覆盖旧目标 |
| `constraints` | 去重；明确取消的约束标记失效，不直接删除审计记录 |
| `decisions` | 记录结论、原因和来源事件 ID |
| `completed` | 追加已验证结果，避免把计划写成已完成 |
| `current_state` | 用最新状态覆盖旧状态 |
| `open_issues` | 已解决项移入 completed，未解决项保留 |
| `artifacts` | 记录路径、命令、测试数量、哈希或 artifact ID |
| `exact_evidence` | 保留原文和来源，不允许模型改写关键错误或数字 |

### 7.3 建议摘要数据结构

`compact_summary` 事件可以继续保存可读 Markdown，同时新增结构化字段：

```json
{
  "type": "compact_summary",
  "payload": {
    "schema_version": 2,
    "content": "供模型阅读的 Markdown 摘要",
    "structured": {
      "objective": ["实现模型辅助的经济型上下文压缩"],
      "constraints": [
        {
          "text": "完整 JSONL 不得删除",
          "source_event_ids": ["01J..."]
        }
      ],
      "decisions": [],
      "completed": [],
      "current_state": ["设计阶段"],
      "open_issues": [],
      "artifacts": [],
      "exact_evidence": []
    },
    "covered_event_ids": ["01J...", "01K..."],
    "retired_token_estimate": 74000,
    "summary_input_tokens": 80000,
    "summary_output_tokens": 6000,
    "cached_input_tokens": 0,
    "summary_profile": "cheap-summary-profile",
    "reasoning_effort": "low",
    "quality": {
      "schema_valid": true,
      "source_refs_valid": true,
      "critical_facts_checked": true
    }
  }
}
```

兼容要求：旧版读取器继续使用 `payload.content`；新版读取器优先使用 `structured` 进行验证和展示。

## 8. 模块设计

### 8.1 模块职责

| 模块 | 实现文件 | 职责 |
|---|---|---|
| `ContextBudgetManager` | `context_compaction/policy.py` | 在完整回合结束后估算下一轮上下文，执行开关、70K 阈值、4 回合冷却和 85% 紧急旁路判断 |
| `ModelSummaryCompactor` | `context_compaction/summary.py` | 调用指定低成本模型生成结构化摘要 |
| `SummaryValidator` | `context_compaction/validation.py` | 校验 Schema、来源事件、关键事实和长度预算 |
| `ContextAssembler` | `context_compaction/projection.py` | 组装项目规范、摘要和最近原文 |
| `SessionEvidenceRecallService` | `context_compaction/evidence.py` | 按当前摘要授权集合恢复事件与文本 artifact，并执行 8 项/4K Token 预算 |
| `UsageLedger` | `context_compaction/ledger.py` | 记录 Token、缓存命中和压缩测量诊断 |
| `ContextCompactionService` | `context_compaction/service.py` | 编排完整回合后的自动压缩流程，不承载各模块算法细节 |
| `SessionStore` | `state/session.py` | 继续追加 `compact_summary`，保存完整事实源 |
| `DeterministicCompactor` | `agent/history.py` | 保留现有本地摘要，由组合根注入 service 作为失败降级方案 |

### 8.2 推荐调用流程

```text
完整回合结束并完成 Session 落盘
  -> 检查 context_compaction.enabled；关闭则退出新功能流程，当前普通压缩照常
  -> ContextBudgetManager 估算下一次请求输入
  -> 低于 70,000 Token 则结束
  -> 检查 4 回合冷却；达到 85% 紧急线时允许绕过
  -> UsageLedger 记录本轮 Token 与缓存测量
  -> 有旧回合：批量选择最近窗口之外的所有可压缩完整回合
  -> 无旧回合：选择当前回合中的大型载荷并建立原始引用
  -> ModelSummaryCompactor 生成结构化结果
  -> SummaryValidator 校验；失败时按确定性策略降级
  -> SessionStore 追加 compact_summary
  -> ContextAssembler 重建运行时历史
  -> UsageLedger 记录压缩前后 Token 与质量诊断
```

### 8.3 摘要模型选择

默认使用：

- 上下文窗口足够的低成本模型。
- `reasoning_effort=low` 或供应商等价设置。
- 低随机性输出。
- 严格结构化响应。

不建议默认跨供应商发送完整转录。若 `summary_profile` 与当前模型属于不同供应商，配置界面必须明确提示数据将被发送到另一个服务边界。

### 8.4 分文件设计与维护边界

上下文压缩不得继续堆入已经承担大量协调职责的 `omnicrawl/agent/core.py`，也不得新建一个包含配置、预算、模型调用、校验和 Session 写入的超级文件。实现采用独立子包，按稳定职责拆分：

```text
omnicrawl/
├─ agent/
│  ├─ context_compaction/
│  │  ├─ __init__.py
│  │  ├─ models.py
│  │  ├─ policy.py
│  │  ├─ summary.py
│  │  ├─ summary_prompt.md
│  │  ├─ validation.py
│  │  ├─ projection.py
│  │  ├─ evidence.py
│  │  ├─ ledger.py
│  │  └─ service.py
│  ├─ core.py
│  ├─ history.py
│  └─ session_facade.py
├─ config/
│  ├─ context_compaction.py
│  └─ runtime.py
└─ state/
   ├─ session.py
   └─ session_projection.py

tests/
├─ test_context_compaction_policy.py
├─ test_context_compaction_summary.py
├─ test_context_compaction_session.py
├─ test_context_compaction_evidence.py
├─ test_context_compaction_integration.py
└─ test_context_compaction_module_boundaries.py

pyproject.toml                    # 注册 summary_prompt.md 包数据
setup.cfg                         # 与现有兼容打包配置同步
```

#### 8.4.1 文件职责

| 文件 | 单一职责 | 不应包含 |
|---|---|---|
| `context_compaction/models.py` | 配置无关的数据结构、决策枚举、预算快照、候选批次、摘要结果和小型注入协议 | 网络请求、Session I/O、金额计算 |
| `context_compaction/policy.py` | 下一请求 Token 估算、70K 判断、4 回合冷却、85% 紧急旁路、普通批次与单回合批次选择 | 模型调用、事件写入、UI 文案 |
| `context_compaction/summary.py` | 读取摘要提示词、调用低成本模型、执行单回合大型载荷的结构化分块与合并 | 触发策略、Session 投影、配置文件解析 |
| `context_compaction/summary_prompt.md` | 可审查、可版本化的摘要提示词契约 | Python 控制逻辑和运行时变量 |
| `context_compaction/validation.py` | Schema、事件引用、精确证据、工具链完整性和长度预算校验 | 重试调度、网络调用、Session 写入 |
| `context_compaction/projection.py` | 根据有效摘要和最近窗口构造新的模型历史投影 | 修改原始 JSONL、调用模型、证据读取 |
| `context_compaction/evidence.py` | 从最后一个有效摘要构造事件授权集合，恢复获准事件与 artifact，并执行输出预算 | Session 选择、任意路径读取、自动注入冷历史 |
| `context_compaction/ledger.py` | Token、缓存命中和测量诊断记录 | 决定是否达到 70K、生成摘要正文 |
| `context_compaction/service.py` | 编排一次完整回合后的压缩流程，连接 policy、summary、validation、projection 和 ledger | 具体 Token 算法、JSON Schema 校验细节、提示词正文 |
| `config/context_compaction.py` | `ContextCompactionConfig` 的默认值、解析和跨字段校验 | Agent 状态、模型调用、Session 写入 |
| `agent/core.py` | 在完整回合结束点调用 service，并在功能关闭时继续当前普通压缩 | 新功能的预算、分块和校验实现 |
| `agent/history.py` | 保留现有本地确定性普通压缩和降级实现 | 模型辅助摘要策略 |
| `agent/session_facade.py` | 为 service 提供追加摘要事件和恢复历史的窄接口 | 摘要算法和 Token 预算计算 |
| `state/session.py`、`state/session_projection.py` | 持久化 `compact_summary` 及重放有效事件 | Agent 编排和模型调用 |
| `pyproject.toml`、`setup.cfg` | 将 `agent/context_compaction/summary_prompt.md` 纳入 wheel 和 sdist | 运行时业务配置 |

#### 8.4.2 依赖方向

依赖必须保持单向，防止为了压缩功能让 Agent、Config 和 State 相互循环引用：

下图中 `A -> B` 表示 A 可以导入或调用 B：

```text
agent/core.py（组合根）
  -> context_compaction/service.py
       -> policy.py
       -> summary.py -> 现有 LLM 协议层
       -> validation.py
       -> projection.py
       -> evidence.py -> 注入的当前 Session artifact 读取回调
       -> ledger.py
       -> 注入的 DeterministicFallback 协议
       -> session_facade 的窄协议 -> state/session.py
  -> agent/history.py 的现有确定性压缩
       -> 作为 DeterministicFallback 实例注入 service.py

policy / summary / validation / projection / ledger
  -> context_compaction/models.py

config/runtime.py
  -> config/context_compaction.py

state/session.py -> state/session_projection.py
```

具体约束：

- `context_compaction` 子包不得导入 `LocalToolAgent` 或 fullscreen UI。
- `state` 包不得反向导入 `agent.context_compaction`；结构化字段以普通映射或 State 自己的事件类型持久化。
- `service.py` 通过构造参数接收模型调用器、Session 端口、时钟和 `DeterministicFallback`，不读取全局单例。
- `service.py` 不直接导入 `agent/history.py`；由 `core.py` 组合根把现有纯确定性压缩适配为回调并注入，避免 `context_compaction ↔ history` 循环依赖或重复实现。
- `core.py` 不直接导入 `policy.py`、`summary.py` 等内部模块，只依赖子包公开的 service 接口。
- `__init__.py` 只导出稳定入口和必要类型，不承载业务实现。
- 供应商差异继续封装在现有 LLM 协议层，`summary.py` 不新增第二套 HTTP 客户端。

#### 8.4.3 `core.py` 集成限制

`LocalToolAgent` 只增加初始化和完整回合结束两个集成点，概念上保持为：

```python
if self.config.context_compaction.enabled:
    result = self._context_compaction.after_complete_turn(snapshot)
    if result.history_projection is not None:
        self._history = result.history_projection
else:
    self._compact_history(force=False)
```

两个自动压缩分支必须互斥：

- 开关关闭时，service 不参与主流程，继续执行现有 `_compact_history(force=False)`。
- 开关开启时，由 service 接管自动压缩，不得先运行现有按回合数触发的确定性压缩，否则历史会在达到 70K 前被提前替换。
- 开启状态下，`DeterministicCompactor` 只在模型调用或摘要校验失败时作为降级路径使用。
- 普通手动 `/compact` 的当前行为不变。

不得把 70K 判断、分块循环、摘要提示词或校验规则直接写入 `core.py`。

#### 8.4.4 文件体量规则

分文件是为了保持职责连贯，不是为了制造只能被调用一次的微型模块：

- 新增生产模块目标控制在 `200～500` 行。
- 超过 `600` 行时必须在评审中说明为何仍属于单一职责，并优先拆分独立变化原因。
- `context_compaction` 子包内单文件超过 `800` 行视为结构缺陷，CI 应失败；自动生成文件除外。
- `core.py` 中本功能的新增集成代码目标不超过 `50` 行，不把既有大文件作为新增逻辑容器。
- 不因单个短函数单独建文件；只有当逻辑拥有独立输入输出契约、状态边界或测试维度时才拆分。
- 测试沿用项目当前扁平布局；单个测试文件超过 `700` 行时按 policy、summary、session、integration 等行为边界拆分。

#### 8.4.5 测试文件分工

| 测试文件 | 覆盖范围 |
|---|---|
| `test_context_compaction_policy.py` | 开关、69,999/70,000 边界、下一请求估算、冷却、紧急旁路和批次选择 |
| `test_context_compaction_summary.py` | 提示词输入、结构化分块、模型响应、校验失败和确定性降级 |
| `test_context_compaction_session.py` | `compact_summary` 持久化、恢复、投影、重复压缩和 `/undo` |
| `test_context_compaction_evidence.py` | 最新摘要授权、当前 Session 绑定、8 项/4K 预算、artifact 和结构化诊断 |
| `test_context_compaction_integration.py` | 完整回合入口、关闭时保持当前行为、单回合 70K 和缓存测量记录 |
| `test_context_compaction_module_boundaries.py` | 禁止反向依赖、禁止向 `core.py` 泄漏实现、文件体量守卫和摘要提示词包数据检查 |

#### 8.4.6 摘要提示词打包

`summary.py` 使用 `importlib.resources` 从 `omnicrawl.agent.context_compaction` 包读取 `summary_prompt.md`，不依赖当前工作目录或源码树绝对路径。实现时必须同时更新当前项目已有的两个包数据清单：

```toml
# pyproject.toml：在现有列表中追加新提示词
[tool.setuptools.package-data]
omnicrawl = [
  "agent/system_prompt.md",
  "agent/subagents/builtin/*.md",
  "agent/context_compaction/summary_prompt.md",
  "extensions/node_runner.mjs",
]
```

```ini
# setup.cfg：与 pyproject.toml 保持一致
[options.package_data]
omnicrawl =
    agent/system_prompt.md
    agent/subagents/builtin/*.md
    agent/context_compaction/summary_prompt.md
    extensions/node_runner.mjs
```

打包测试必须从构建出的 wheel 安装到临时环境，再验证 `importlib.resources` 能读取提示词，避免源码环境正常而发布包缺文件。

## 9. 摘要提示词契约

摘要提示词必须强调“提取和合并”，不能鼓励自由发挥。建议约束：

```text
你是会话状态压缩器，不是任务执行者。

目标：
1. 将上一份结构化摘要与新增原始事件合并。
2. 保留用户目标、约束、已确认决策、已验证进展、未完成事项和证据引用。
3. 不把计划写成已完成，不把推测写成事实。
4. 不改写 exact_evidence 中的错误、数字、命令、路径和标识符。
5. 只输出指定 JSON Schema。
6. 新用户指令优先于旧摘要；冲突时记录被覆盖关系。
```

模型输入不需要携带无关系统提示、全部工具说明或项目 Skill 清单，只需要摘要任务所需的最小规则、上一摘要和待压缩事件，以降低摘要调用自身的 Token 成本。

## 10. 质量校验与失败处理

### 10.1 校验层级

| 校验 | 失败处理 |
|---|---|
| JSON/Schema 合法 | 重试一次；仍失败则降级本地摘要 |
| 摘要长度预算 | 要求模型压缩一次；仍超限则本地裁剪非关键字段 |
| 来源事件存在 | 拒绝写入无效引用 |
| 目标和约束未丢失 | 保留旧摘要并取消本次替换 |
| 精确证据一致 | 从原始事件回填，不接受模型改写 |
| 工具链完整 | 未完成工具链不得进入“已完成”字段 |

### 10.2 降级顺序

```text
模型结构化摘要成功
  -> 使用新摘要

模型调用或校验失败
  -> 保留旧摘要 + 最近窗口

容量仍然安全
  -> 跳过本次压缩并记录诊断

容量接近上限
  -> 使用现有 DeterministicCompactor

仍无法满足上下文上限
  -> 明确提示用户并要求选择归档、删除非关键附件或新建 Session
```

不能在摘要失败时静默丢弃旧历史。

## 11. 配置建议

```yaml
context_compaction:
  enabled: false                         # 模型辅助自动压缩默认关闭
  trigger_context_tokens: 70000          # 预计下一次请求达到此值才触发
  next_user_reserve_tokens: 4096         # 下一条用户输入的估算预留
  minimum_turns_between_model_compactions: 4
  emergency_context_ratio: 0.85          # 仅用于绕过冷却，不能绕过开关
  summary_profile: ""                    # 空值表示复用当前供应商的低成本配置
  reasoning_effort: low
  recent_turns: 8
  recent_context_ratio: 0.25
  target_summary_tokens: 6000
  preserve_exact_evidence: true
  allow_cross_provider: false
  failure_fallback: deterministic
```

配置语义：

- `enabled: false` 时整套新功能不进入主流程：不做 70K 评估、模型摘要、单回合特殊处理、新增警告或拦截。
- 全屏 TUI 的 `/settings` 面板提供“上下文压缩”总开关；切换会立即更新运行态并持久化 `context_compaction.enabled`，不在紧凑面板暴露阈值、冷却期和跨供应商等高级参数。
- 关闭时现有本地确定性自动压缩和普通 `/compact` 保持当前行为不变，同时从 Agent 工具表移除 `recall_session_evidence`。
- 开启时 service 替换现有确定性自动压缩入口；未达到 70K 不自动压缩，确定性压缩只作为模型失败降级。该行为变化必须在设置说明和测试中显式覆盖。
- 后续若增加 `/compact --model`，开关关闭时应明确拒绝并提示先启用，不能产生隐式费用；普通 `/compact` 仍可使用。

配置校验要求：

- `trigger_context_tokens` 固定默认 `70,000`，必须为正整数。
- `next_user_reserve_tokens`、`recent_turns`、Token 和冷却回合数必须为正整数。
- `0 < emergency_context_ratio < 1`。
- 活动模型上下文窗口必须大于 `trigger_context_tokens + next_user_reserve_tokens + 输出安全预留`；否则拒绝启用或暂停功能并显示诊断。
- 跨供应商默认关闭。
- 用量报告只包含 Token、缓存命中和质量诊断，不维护价格或金额字段。

## 12. 可观测性

每次压缩记录以下指标：

| 指标 | 用途 |
|---|---|
| `retired_tokens` | 本次移出热窗口的估算 Token |
| `summary_input_tokens` | 摘要模型实际输入 |
| `summary_output_tokens` | 摘要模型实际输出 |
| `summary_cached_tokens` | 摘要调用缓存命中 |
| `token_estimation_error` | 估算 Token 与供应商实际 Usage 的偏差 |
| `quality_fallbacks` | 校验失败和降级次数 |
| `evidence_recall_count` | 按需证据恢复工具调用次数 |
| `evidence_recall_diagnostics` | 未授权、缺失、不可读和预算截断诊断 |

建议 `/compact --model` 返回简洁结果：

```text
压缩完成：退出热窗口约 74K Token，摘要 6K Token。
完整转录仍保留在当前 Session。
```

## 13. 缓存友好策略

为了同时获得摘要收益和 Prompt Cache 收益：

1. 项目规范、工具定义等稳定内容继续位于请求前缀。
2. 工作摘要写入后保持稳定，直到下一次达到压缩阈值。
3. 最近消息只在摘要之后追加。
4. 不因一条新消息就重写摘要。
5. 记录缓存命中率，用于诊断稳定前缀是否按预期复用。
6. 供应商支持显式缓存断点时，在稳定规范和工作摘要后设置断点。

需要接受的取舍：压缩发生后的第一次请求通常要建立新的缓存前缀；后续请求才能复用新的稳定前缀。

## 14. 隐私与安全

- 完整转录可能包含密钥、Cookie、个人信息和本地路径，摘要前继续应用现有敏感值脱敏。
- `exact_evidence` 不应保存明文密钥，即使用户原文中存在。
- 跨供应商摘要必须由用户显式开启。
- 摘要模型只能读取当前 Session 中被选中的事件，不得扫描其他项目或 Session。
- `recall_session_evidence` 不接受 Session ID 或 artifact 路径，只接受当前摘要展示的事件 ID。
- artifact 路径只能从获准事件 payload 中派生，并继续由 SessionStore 校验当前 Session 归属和目录边界。
- 摘要不自动写入长期记忆，避免把临时敏感上下文扩大传播范围。
- 导出用量诊断时只包含 Token 和质量元数据，不包含会话正文。

## 15. 与 `/undo`、恢复和长期记忆的关系

### 15.1 `/undo`

如果被回退轮次已经被某个摘要覆盖：

- 原始 JSONL 继续追加 `turn_undone`。
- 旧摘要在活动投影中失效。
- 从回退后的有效事件重新构建摘要或退回最近一个仍有效的摘要边界。
- 文件修改、命令和网络副作用仍不会自动撤销。

### 15.2 Session 恢复

恢复顺序：

```text
读取原始事件
  -> 应用 turn_undone 等活动投影
  -> 选择最后一个有效 compact_summary
  -> 加载摘要后的最近原文
  -> 校验上下文预算
  -> 必要时在下一轮前再次压缩
```

### 15.3 长期记忆

会话摘要回答“这个任务目前做到哪里”；长期记忆回答“未来其他任务也值得知道什么”。两者不能自动互相替代。

只有稳定偏好、长期项目决策和可复用踩坑经验，才应通过现有 memory 流程单独写入长期记忆。

## 16. 实施阶段

### 第一阶段：只测量，不调用摘要模型

- 先建立 `agent/context_compaction/` 子包和 `config/context_compaction.py`，不把测量逻辑追加到 `core.py`。
- 增加 Token 估算和上下文预算快照。
- 记录旧历史 Token、最近窗口 Token、缓存命中率。
- 模拟计算“如果现在压缩，可以减少多少 Token”。
- 不改变现有 `_history` 行为。

验收：至少收集多段真实长会话数据，能够比较估算 Token、实际 Usage 和缓存命中情况。

### 第二阶段：结构化摘要原型

- 新增 Schema 和 `SummaryValidator`。
- 仅通过手动 `/compact --model` 触发。
- 默认同供应商、`low` 思考强度。
- 模型失败时回退现有确定性摘要。

验收：目标、约束、决策、进度和原始证据引用通过固定测试集。

### 第三阶段：默认关闭的 70K 自动批量压缩

- 增加 `context_compaction.enabled` 开关，默认值为 `false`。
- 仅在完整回合结束后估算下一次请求输入。
- 开启后，估算值达到 `70,000 Token` 才批量压缩。
- 两次模型压缩至少间隔 4 个完整回合。
- 达到活动模型上下文窗口 `85%` 时只允许绕过冷却，不允许绕过开关和回合边界。
- 保留最近窗口，批量合并其余所有可压缩完整回合。
- 单个回合独自达到 70K 且没有旧回合可压缩时，仅在功能开启状态下处理该回合的大型载荷。

验收：开关关闭时整套新功能不进入主流程，行为与当前版本一致；开启后仅在完整回合结束且达到 70K 时触发；工具链和恢复路径保持正确。

### 第四阶段：按需证据恢复（已完成）

- 摘要保留事件 ID 和 artifact 引用。
- 模型通过只读工具 `recall_session_evidence` 提交当前摘要中的事件 ID。
- 每次调用只读取最后一个有效摘要授权的当前 Session 事件；`/undo` 后失效摘要不再授权。
- artifact 路径从获准事件中派生，文本内容可恢复，二进制或不可读内容只返回安全元数据与诊断。
- 单次最多 8 个唯一事件 ID，合计约 4,000 Token；超出时截断并允许分批读取。
- 不默认恢复整个冷历史，不允许模型指定 Session ID 或任意 artifact 路径。

验收：摘要遗漏的精确错误或工具结果可以从转录定位并进入当前工具回路；未授权、缺失、跨 Session、二进制和超预算场景不阻断回合且返回结构化诊断。

## 17. 测试与验收

| 测试场景 | 验收标准 |
|---|---|
| 功能默认状态 | 未配置或 `enabled: false` 时，自动模型摘要调用数为 0，且主流程行为与当前版本一致 |
| 短会话 | 即使功能开启，预计下一次请求低于 70K 时也不调用摘要模型，也不运行当前按回合数触发的确定性自动压缩 |
| 70K 边界 | `69,999` 不触发，`70,000` 在完整回合结束后触发一次批量压缩 |
| 非完整回合 | 流式输出、工具执行和审批期间达到 70K 也不触发 |
| 冷却期 | 压缩后 4 个完整回合内不再次压缩，除非达到 85% 紧急线 |
| 紧急旁路 | 只能绕过冷却；开关关闭时调用数仍为 0 |
| 批量范围 | 一次覆盖最近窗口之外所有可压缩完整回合，不逐回合重写摘要 |
| 单回合 70K（开启） | 没有旧回合可压缩时，对当前回合大型载荷分块摘要并保留任务骨架和原始引用 |
| 单回合 70K（关闭） | 不进入新路径，不新增摘要、警告或拦截，继续当前普通压缩行为 |
| 长会话多轮继续 | 相对当前生产基线减少配置要求的上下文 Token，并保持目标与约束完整 |
| 高缓存命中 | 压缩冷却期间摘要保持稳定，缓存命中统计持续可用 |
| Token 估算 | 在支持精确 Usage 的基准集上，中位绝对百分比误差不高于 10%，P95 不高于 20% |
| 模型摘要失败 | 保留旧上下文或降级确定性摘要，不丢历史 |
| 摘要 Schema 错误 | 拒绝写入并记录诊断 |
| 否定约束 | “不要删除文件”等约束压缩后仍存在 |
| 精确错误 | 错误文本和事件引用保持一致 |
| 多轮滚动压缩 | 不出现明显摘要漂移或重复堆叠 |
| `/undo` 跨摘要边界 | 活动摘要与回退后的事件投影一致 |
| Session 恢复 | 恢复后目标、进度和最近原文完整 |
| 工具调用中 | 不触发压缩，不截断工具链 |
| 跨供应商配置 | 未明确允许时拒绝发送转录 |
| 证据授权 | 只能恢复最后一个有效摘要引用的当前 Session 事件 |
| 证据预算 | 单次最多 8 个唯一事件 ID，序列化结果不超过约 4,000 Token |
| artifact 恢复 | 文本内容可读取；二进制、缺失、越界只返回元数据和结构化诊断 |
| 证据注入 | 仅工具主动调用时进入模型上下文，不默认恢复冷历史 |
| 缓存前缀 | 压缩冷却期间摘要保持稳定 |
| 文件边界 | 新功能逻辑位于 `context_compaction` 子包，`core.py` 只保留薄集成点 |
| 文件体量 | 子包生产文件不超过 800 行；超过 600 行必须有单一职责说明 |
| 依赖方向 | `state` 不反向依赖 Agent，子包不导入 `LocalToolAgent`、`agent/history.py` 或 fullscreen UI；确定性降级由组合根注入 |
| 提示词打包 | wheel 安装后可通过 `importlib.resources` 读取 `summary_prompt.md` |

建议建立一组固定“摘要事实测试”：每段原始会话预先标注必须保留、允许省略和禁止推断的事实，然后分别测试不同摘要模型。

## 18. 关键取舍

| 选择 | 收益 | 成本或放弃项 |
|---|---|---|
| 完整 JSONL + 短上下文投影 | 可恢复且节省每轮输入 | 本地存储量不会减少 |
| 结构化摘要 | 易验证、可合并、漂移较少 | 比自然语言摘要更严格，需要 Schema |
| 低成本模型 | 摘要调用便宜 | 能力过弱时可能遗漏复杂约束 |
| 保留最近原文 | 保持当前任务精度 | 不能把上下文压到理论最小 |
| 冷却后批量压缩 | 保护缓存并减少调用次数 | 上下文会在阈值间继续增长 |
| 同供应商默认 | 数据边界更清晰 | 可能不是价格最低的摘要模型 |
| 固定容量阈值 | 行为明确且可测试 | 不针对不同价格动态调整阈值 |

## 19. 已确认与待确认决策

### 19.1 已确认

| 决策 | 结果 |
|---|---|
| 功能开关 | 模型辅助自动压缩可开关，默认关闭；关闭时整套新功能视为不存在并完全沿用当前普通压缩行为 |
| 自动检查时机 | 每个完整 user/assistant 回合结束后 |
| 自动触发阈值 | 预计下一次请求输入达到 `70,000 Token` |
| 压缩方式 | 批量合并最近窗口之外所有可压缩完整回合 |
| 冷却期 | 两次模型压缩至少间隔 4 个完整回合 |
| 紧急例外 | 达到活动模型上下文窗口 85% 时可绕过冷却，但不能绕过开关或回合边界 |

### 19.2 已落地实施决策

| 决策 | 结果 |
|---|---|
| 第一阶段 | 先实现 Token 与缓存测量，不调用摘要模型 |
| 摘要模型 | 默认复用当前供应商，可通过 `summary_profile` 显式选择模型 |
| 最近窗口 | 沿用 6 个完整回合，并受最近原文 Token 预算约束 |
| 手动命令 | `/compact` 保持确定性；`/compact --model` 显式使用模型摘要 |

第一至第四阶段已经落地，`enabled` 仍保持默认关闭；用户可以先观察 Token 与缓存测量，再主动开启自动压缩和按需证据恢复。

## 20. 最终结论

OmniCrawl 采用固定容量策略管理长会话上下文，不引入价格、金额收益、回本预测或 ROI 阶段。目标是减少重复上下文、保持缓存前缀稳定，并确保摘要不会破坏任务连续性：

```text
完整 JSONL 事实源
+ 滚动结构化工作摘要
+ 最近完整原文
+ 按需恢复证据
+ Token/缓存实测
```

第一至第四阶段已经实现：默认关闭的 `70K + 4 回合冷却` 自动批量压缩，以及受当前摘要授权、按事件 ID 调用的精确证据恢复。证据工具不改变既有压缩触发规则，也不默认注入整个冷历史。
