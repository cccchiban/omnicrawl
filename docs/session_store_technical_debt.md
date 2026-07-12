# SessionStore 后续技术债清单

> 建立日期：2026-07-12
> 状态：待处理
> 来源：P1 SessionStore 第一阶段结构治理复核
> 约束：本文记录的问题未混入无行为变化的模块拆分，应分别设计、测试和交付。

## 1. 文档目的

`SessionStore` 第一阶段已经完成模型、PromptHistory、事件投影和 artifact 策略的结构拆分，但复核过程中发现若干涉及运行行为、数据安全或持久化协议的既有问题。此类问题不能作为“顺手修复”夹带在结构重构中，否则难以判断历史数据兼容性和故障影响。

本文作为独立技术债队列，记录问题、证据、影响、建议方案和验收标准，供后续逐项治理。

## 2. 优先级总览

| 编号 | 优先级 | 问题 | 主要影响 | 状态 |
|---|---|---|---|---|
| SESSION-DEBT-001 | P0 | JSONL 与索引并发写缺少统一锁域 | 丢计数、覆盖索引、临时文件冲突 | 单进程缓解已完成；跨进程/崩溃恢复待处理 |
| SESSION-DEBT-002 | P0 | HTML artifact 在脱敏前写盘 | 凭据可能以 HTML 原文长期保存 | 新写入修复完成；历史 artifact 不改写 |
| SESSION-DEBT-003 | P0 | PromptHistory 缺少敏感信息策略 | 用户提示和粘贴内容可能保存凭据 | 新写入修复完成；历史记录不改写 |
| SESSION-DEBT-004 | P1 | JSONL 与索引缺少崩溃恢复 | 转录与索引计数、路径可能不一致 | 待处理 |
| SESSION-DEBT-005 | P1 | 事件版本迁移与诊断缺失 | 旧版本事件可能被静默跳过 | 待处理 |
| SESSION-DEBT-006 | P1 | 损坏记录被静默忽略 | 用户无法判断历史是否完整 | 待处理 |
| SESSION-DEBT-007 | P2 | 敏感键匹配范围过宽 | 普通字段可能被误脱敏 | 已完成（精确字段匹配） |
| SESSION-DEBT-008 | P2 | Python 类型限定名发生变化 | pickle、反射和外部注册可能不兼容 | 已知限制 |
| SESSION-DEBT-009 | P2 | `SessionStore` 仍承担 index/transcript 协调 | 类体量仍较大，事务边界不清晰 | 延后评估 |

## 3. SESSION-DEBT-001：并发写一致性

### 现状

会话追加的主要顺序是：

1. 向 `<session_id>.jsonl` 追加事件；
2. 重新读取 `index.json`；
3. 更新事件数、消息数、标题和最后事件类型；
4. 写入固定的 `index.json.tmp`；
5. 用临时文件替换 `index.json`。

PromptHistory 的 `history.jsonl` 也使用直接追加，没有统一的进程内锁或跨进程锁。

### 风险

- 两个线程同时读取旧索引后分别写回，后写入者覆盖先写入者；
- `event_count`、`message_count` 或标题更新丢失；
- 多个写入者共用 `index.json.tmp`，可能互相覆盖或提前替换；
- JSONL 事件已经成功追加，但索引更新失败；
- 多进程同时运行 TUI/API 时风险进一步扩大。

### 建议方案

分两层治理：

1. **单进程一致性**：为“追加转录 + 更新索引”建立可重入锁，PromptHistory 使用独立锁；
2. **跨进程一致性**：明确项目是否支持 TUI/API 多进程同时写同一 `.agent_sessions`。若支持，引入文件锁；若不支持，应在启动时检测并给出明确错误，而不是依赖约定。

索引临时文件应使用唯一名称，并保持与目标文件位于同一目录。写入时评估 `flush`、`fsync` 和原子 `replace` 的平台差异。

### 验收标准

- [ ] 多线程并发追加不会丢失事件或计数；
- [ ] 并发索引更新不会发生临时文件冲突；
- [ ] PromptHistory 并发追加不会产生交叉或损坏记录；
- [ ] 明确并测试多进程支持策略；
- [ ] 锁等待和失败具有可诊断错误；
- [ ] 不改变现有 JSON/JSONL 字段格式。

## 4. SESSION-DEBT-002：HTML artifact 脱敏顺序

### 现状

工具返回 HTML UI artifact 时，HTML 正文会先写入 artifact 文件，之后 JSONL payload 才经过递归脱敏。因此 JSONL 中的元数据可能是安全的，但 `.html` 文件仍可能保留原始秘密。

### 风险

HTML 中可能包含：

- API Key；
- Authorization/Bearer Token；
- Cookie；
- 密码或 Secret；
- 云厂商 AK/SK；
- 写入 HTML 属性、脚本或 JSON 数据中的凭据。

### 建议方案

在生成哈希和写盘策略确定前先明确产品语义：

- artifact 是否必须保留模型/工具返回的原始 HTML；
- 如果必须保真，是否应加密而不是文本替换；
- 如果不要求保真，则在写盘前执行与文本 artifact 一致的脱敏；
- 哈希表示原始内容还是实际落盘内容，需要固定契约。

默认建议：对实际落盘正文脱敏，哈希对应脱敏后的落盘内容，并在元数据中明确 `redacted: true`。

### 验收标准

- [ ] 直接读取 HTML artifact 时不存在测试秘密；
- [ ] 覆盖文本、属性、脚本和内嵌 JSON 中的常见凭据；
- [ ] 哈希和字符数语义有明确文档；
- [ ] UI 历史回放仍能正常渲染；
- [ ] 既有 HTML artifact 的处理策略明确，不静默改写历史文件。

## 5. SESSION-DEBT-003：PromptHistory 敏感信息策略

### 现状

`history.jsonl` 保存用户输入的 `display` 和 `pasted_contents`。当前只执行换行规范化、空白处理和长度截断，没有敏感信息清洗。

### 风险

用户可能直接在提示中粘贴：

- `api_key=...`；
- `Authorization: Bearer ...`；
- Cookie；
- `.env` 内容；
- 密码、Token、AK/SK；
- 嵌套字典或列表形式的凭据。

PromptHistory 主要服务于输入复用，不进入模型上下文，但仍是长期明文持久化数据。

### 建议方案

先确认产品策略：

1. 默认脱敏后保存；
2. 对疑似凭据的整条提示不入历史；
3. 允许用户关闭 PromptHistory；
4. 对完整粘贴内容单独加密或不持久化。

建议默认组合：`display` 脱敏保存，`pasted_contents` 递归脱敏；配置允许关闭历史记录。不要在没有迁移策略的情况下自动重写旧历史。

### 验收标准

- [ ] `display` 中常见凭据被处理；
- [ ] `pasted_contents` 的嵌套对象和列表被处理；
- [ ] 普通文本不会被大面积误删；
- [ ] 空提示、截断、查询、去重行为保持；
- [ ] 明确历史数据清理或迁移操作；
- [ ] 配置、UI 和文档对 PromptHistory 的持久化行为描述一致。

## 6. SESSION-DEBT-004：崩溃恢复和索引重建

### 现状

JSONL 和 `index.json` 是两个独立文件，当前没有事务日志或启动修复流程。进程可能在任一步骤之间退出。

可能出现：

- JSONL 已追加，索引未更新；
- 索引已经创建，但 `session_started` 尚未写入；
- 归档事件已写入，但文件尚未移动；
- 文件已移动，但索引路径尚未更新；
- 删除文件后，索引条目尚未移除。

### 建议方案

为 SessionStore 增加显式一致性检查和修复入口：

- 根据 JSONL 重算事件数、消息数、标题和最后事件；
- 检查索引路径和实际文件位置；
- 检测孤立转录、孤立索引和孤立 artifact；
- 修复动作默认先生成报告，经确认后执行；
- 自动修复只处理可确定、幂等的情况。

### 验收标准

- [ ] 可检测 JSONL 与索引计数不一致；
- [ ] 可从有效转录重建索引核心字段；
- [ ] 归档路径不一致可诊断；
- [ ] 修复前有备份或可回滚机制；
- [ ] 故障注入覆盖关键写入断点；
- [ ] 修复过程不会删除无法判断归属的数据。

## 7. SESSION-DEBT-005：事件版本迁移

### 现状

当前只接受 `SESSION_EVENT_VERSION == 1`。其他版本会抛出 `SessionStoreError`，但转录读取层捕获后直接跳过该行，用户看不到版本不支持的诊断。

索引文件顶层也没有显式 schema 版本。

### 建议方案

- 建立按版本分发的 event decoder；
- 每个旧版本通过纯 migration 转为当前模型；
- 未知未来版本必须产生可见诊断；
- 为 index 和 PromptHistory 明确 schema/version 策略；
- 保存真实历史格式 fixture，避免只测试当前生成的数据。

### 验收标准

- [ ] 当前版本事件继续正常读取；
- [ ] 至少一个旧版本 fixture 可迁移；
- [ ] 未知版本不会被无提示吞掉；
- [ ] migration 不修改原始转录，除非用户明确执行升级；
- [ ] 文档列出支持的版本及兼容范围。

## 8. SESSION-DEBT-006：损坏记录诊断

### 现状

Session 和 PromptHistory 的 JSONL 读取会跳过非法 JSON、字段不完整或版本不支持的行。跳过尾部半行有助于崩溃恢复，但中间损坏也同样静默。

### 风险

- 用户误以为历史完整；
- 中间事件丢失导致上下文语义变化；
- 安全或磁盘问题无法追踪；
- 版本不支持和真正文件损坏无法区分。

### 建议方案

返回或记录结构化诊断：文件、行号、错误类型、是否可恢复。尾部截断可降级为 warning，中间损坏应明确提示。

### 验收标准

- [ ] 诊断包含文件和行号；
- [ ] 区分 JSON 损坏、字段错误和版本不支持；
- [ ] 单条损坏仍不阻断其余有效历史读取；
- [ ] API/TUI 有最小可见诊断入口；
- [ ] 日志内容经过脱敏。

## 9. SESSION-DEBT-007：敏感键误报

### 现状

递归脱敏通过键名是否包含 `"key"` 等片段判断，可能将以下普通字段误判：

- `keyboard`；
- `monkey`；
- `key_count`；
- 业务中的普通 key 标识。

### 建议方案

使用规范化后的精确字段集合和有限后缀规则，例如 `api_key`、`access_token`、`password`、`authorization`，并提供项目级扩展配置。文本匹配和结构化字段匹配应分别维护。

### 验收标准

- [ ] 常见凭据字段仍被脱敏；
- [ ] `keyboard`、`monkey` 等普通字段不被误删；
- [ ] 自定义敏感键可配置；
- [ ] 规则变更有充分正反例测试。

## 10. SESSION-DEBT-008：Python 类型限定名兼容

结构拆分后：

```text
SessionEvent.__module__ = omnicrawl.state.session_models
PromptHistoryEntry.__module__ = omnicrawl.state.prompt_history
```

从 `omnicrawl.state.session` 的兼容导入仍可用，JSON/JSONL 数据也不受影响。但依赖以下机制的外部代码可能需要迁移：

- pickle；
- 使用完整类名的注册表；
- 反射或日志断言；
- patch 原定义模块内部依赖。

### 处理建议

当前标记为已知限制。若项目确认需要读取历史 pickle，应提供兼容 unpickler 或旧路径代理；若没有 pickle 数据，不建议为假设场景增加复杂兼容层。

## 11. SESSION-DEBT-009：进一步拆分 index/transcript

第一阶段刻意保留 `SessionStore` 对跨文件生命周期的协调，避免在机械拆分中改变写入顺序。因此类本身仍约 686 行。

只有在完成并发锁、崩溃恢复和版本迁移设计后，才评估拆出：

- `SessionIndexRepository`；
- `SessionTranscriptRepository`；
- 一致性检查/修复服务。

拆分目标应是明确事务边界和状态所有权，而不是单纯降低文件行数。

## 12. 推荐处理顺序

1. SESSION-DEBT-002：HTML artifact 脱敏；
2. SESSION-DEBT-003：PromptHistory 敏感信息策略；
3. SESSION-DEBT-001：单进程并发锁与唯一临时文件；
4. SESSION-DEBT-004：一致性检查与索引重建；
5. SESSION-DEBT-005、006：版本迁移与损坏诊断；
6. SESSION-DEBT-007：降低敏感键误报；
7. SESSION-DEBT-009：重新评估 repository 拆分；
8. SESSION-DEBT-008：仅在发现真实 pickle/反射调用方时处理。

每项应单独完成需求确认、测试、实施和验证，不建议合并为一次大改。

## 13. 更新记录

### 2026-07-12：P0/P1 最小闭环

- **SESSION-DEBT-001（单进程部分）**：`SessionStore` 以会话根目录共享可重入锁，覆盖同一进程内的转录追加、索引读改写、PromptHistory 追加，以及归档/恢复、删除和空会话清理的“文件操作 + 索引更新”生命周期；`index.json` 改用同目录唯一临时文件再原子替换。跨进程文件锁、`fsync` 耐久性策略和崩溃恢复仍待单独设计。
- **SESSION-DEBT-002**：新 HTML artifact 在写盘前清理常见文本、属性、脚本和内嵌 JSON 凭据；`html_sha256` 与 `html_size_chars` 指向实际持久化正文，并写入 `redacted: true`。既有 artifact 保持原样，不进行隐式改写。
- **SESSION-DEBT-003**：新 PromptHistory 的 `display` 和嵌套 `pasted_contents` 在 JSONL 追加前脱敏；读取旧记录时仅返回脱敏展示值，不修改旧文件。关闭 PromptHistory 的产品配置另行设计。
- **SESSION-DEBT-007**：结构化敏感字段由宽泛 `key` 子串匹配收紧为精确规范化字段集合，避免误删 `keyboard`、`monkey` 和 `key_count`；同时识别常见 `X-API-Key` 风格请求头，避免 API 凭据在会话或审计嵌套参数中明文持久化。
- 同期补充 MCP 审计 `output_preview` 脱敏，避免工具输出中的凭据写入 `logs/mcp-audit.jsonl`。

### 2026-07-12：初始登记

- 建立独立技术债文档；
- 从 P1 SessionStore 结构治理中迁出行为性和数据策略问题；
- 尚未修改任何相关运行行为或历史数据。
