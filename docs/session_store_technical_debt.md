# SessionStore 后续技术债清单

> 建立日期：2026-07-12
> 状态：主要治理项已完成；保留历史问题、验收证据与残余限制
> 来源：P1 SessionStore 第一阶段结构治理复核
> 约束：本文记录的问题未混入无行为变化的模块拆分，应分别设计、测试和交付。

## 1. 文档目的

`SessionStore` 第一阶段已经完成模型、PromptHistory、事件投影和 artifact 策略的结构拆分，但复核过程中发现若干涉及运行行为、数据安全或持久化协议的既有问题。此类问题不能作为“顺手修复”夹带在结构重构中，否则难以判断历史数据兼容性和故障影响。

本文最初作为独立技术债队列；当前同时承担治理记录用途。各章节中的“现状/风险/建议方案”保留原问题背景，是否完成以优先级总览、章节“已落实”和验收清单为准。

## 2. 优先级总览

| 编号 | 优先级 | 问题 | 主要影响 | 状态 |
|---|---|---|---|---|
| SESSION-DEBT-001 | P0 | JSONL 与索引并发写缺少统一锁域 | 丢计数、覆盖索引、临时文件冲突 | 已完成（进程内锁 + 跨进程文件锁 + fsync） |
| SESSION-DEBT-002 | P0 | HTML artifact 在脱敏前写盘 | 凭据可能以 HTML 原文长期保存 | 新写入修复完成；历史 artifact 不改写 |
| SESSION-DEBT-003 | P0 | PromptHistory 缺少敏感信息策略 | 用户提示和粘贴内容可能保存凭据 | 新写入修复完成；历史记录不改写 |
| SESSION-DEBT-004 | P1 | JSONL 与索引缺少崩溃恢复 | 转录与索引计数、路径可能不一致 | 检查与索引重建已完成；自动修复策略仍保守 |
| SESSION-DEBT-005 | P1 | 事件版本迁移与诊断缺失 | 旧版本事件可能被静默跳过 | 已完成（内存迁移 + 可见诊断；不改写磁盘） |
| SESSION-DEBT-006 | P1 | 损坏记录被静默忽略 | 用户无法判断历史是否完整 | 已完成（结构化诊断 + API 入口） |
| SESSION-DEBT-007 | P2 | 敏感键匹配范围过宽 | 普通字段可能被误脱敏 | 已完成（精确字段匹配） |
| SESSION-DEBT-008 | P2 | Python 类型限定名发生变化 | pickle、反射和外部注册可能不兼容 | 已知限制 |
| SESSION-DEBT-009 | P2 | `SessionStore` 仍承担 index/transcript 协调 | 类体量仍较大，事务边界不清晰 | 评估完成：暂不拆分（见 §11） |

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

- [x] 多线程并发追加不会丢失事件或计数；
- [x] 并发索引更新不会发生临时文件冲突；
- [x] PromptHistory 并发追加不会产生交叉或损坏记录；
- [x] 明确并测试多进程支持策略；
- [x] 锁等待和失败具有可诊断错误；
- [x] 不改变现有 JSON/JSONL 字段格式。

### 已落实（2026-07-12）

- 新增 `omnicrawl/state/session_locking.py`：
  - `ProcessFileLock`：Windows `msvcrt.locking` / POSIX `fcntl.flock`；
  - 同一进程可重入，避免 `start_session` → `append_event` 嵌套死锁；
  - 默认超时 30s，超时抛出可诊断 `SessionStoreError`；
- 写路径顺序固定为：`thread RLock` → 跨进程文件锁；
- JSONL 追加与 `index.json` 原子写默认 `flush + fsync`；
- 目录 `fsync` 在 Windows 上不可用时降级忽略；
- 多进程策略：**允许** TUI/API 同时写同一 `.agent_sessions`，靠文件锁互斥；
- 可通过 `SessionStore(..., durable=DurableWritePolicy(...))` 调整 fsync/超时。

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

- [x] 直接读取新写入 HTML artifact 时不存在测试秘密；
- [x] 覆盖文本、属性、脚本和内嵌 JSON 中的常见凭据；
- [x] 哈希和字符数对应实际脱敏后落盘正文；
- [x] UI 历史回放仍能正常渲染；
- [x] 既有 HTML artifact 不静默改写，需用户自行清理历史敏感文件。

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

- [x] `display` 中常见凭据被处理；
- [x] `pasted_contents` 的嵌套对象和列表被处理；
- [x] 普通文本不会被大面积误删；
- [x] 空提示、截断、查询、去重行为保持；
- [x] 旧历史读取时脱敏但不隐式改写；需要永久清理时由用户删除历史文件；
- [ ] 关闭 PromptHistory 的独立产品配置尚未提供。

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

- [x] 可检测 JSONL 与索引计数不一致；
- [x] 可从有效转录重建索引核心字段；
- [x] 归档路径不一致可诊断；
- [x] 修复前有备份或可回滚机制；
- [x] 故障注入覆盖关键写入断点；
- [x] 修复过程不会删除无法判断归属的数据。

### 已落实（2026-07-12）

- 新增 `omnicrawl/state/session_consistency.py`，提供只读诊断模型
  `SessionConsistencyIssue` / `SessionConsistencyReport` 与索引重建逻辑；
- `SessionStore.check_consistency()`：扫描 `sessions/`、`archive/`、`index.json`
  与 `artifacts/`，报告计数漂移、标题/路径/归档状态不一致、孤立转录、
  缺失转录和孤立 artifact；
- `SessionStore.rebuild_index(apply=False|True)`：默认只预览；`apply=True`
  时先备份 `index.json.bak.<timestamp>`，再以磁盘转录为权威写回索引；
- 不改写 JSONL，不删除 artifact；缺失转录的索引条目会从 index 移除，
  同 ID 多份转录与不可读转录只报告、不自动裁决删除；
- 当前工作区 `.agent_sessions` 实盘扫描：`ok=True`，42 个索引与 42 份转录一致。

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

- [x] 当前版本事件继续正常读取；
- [x] 至少一个旧版本 fixture 可迁移；
- [x] 未知版本不会被无提示吞掉；
- [x] migration 不修改原始转录，除非用户明确执行升级；
- [x] 文档列出支持的版本及兼容范围。

### 已落实（2026-07-12）

- 新增 `omnicrawl/state/session_records.py`：按版本分发解码；
- 支持范围：
  - `version=1`：当前事件格式；
  - `version=0`：原型格式（内存升到 v1；兼容 `id` → `event_id`）；
  - 缺失 `version` 但具备核心字段：视为 version 字段落地前的遗留格式；
  - 其他版本：`unsupported_event_version` 诊断，不静默跳过；
- `index.json` 写入时附带 `schema_version=1`；缺失时按 1 兼容读取；
- `SessionStore.read_session_events_with_diagnostics()` 暴露结果；
- 默认不改写磁盘 JSONL。

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

- [x] 诊断包含文件和行号；
- [x] 区分 JSON 损坏、字段错误和版本不支持；
- [x] 单条损坏仍不阻断其余有效历史读取；
- [x] API/TUI 有最小可见诊断入口；
- [x] 日志内容经过脱敏。

### 已落实（2026-07-12）

- 结构化诊断码：`invalid_json` / `invalid_fields` / `unsupported_event_version` /
  `trailing_incomplete` / `legacy_event_migrated` / `invalid_prompt_history` 等；
- 中间损坏为 `error`，尾部半行为 `warning`；
- PromptHistory 同步输出诊断；snippet 经 `redact_sensitive_text`；
- API：
  - `GET /api/v1/sessions/diagnostics`
  - `GET /api/v1/sessions/{session_id}/diagnostics`
- 日志写入同样使用脱敏后的诊断消息。

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

- [x] 常见凭据字段仍被脱敏；
- [x] `keyboard`、`monkey` 等普通字段不被误删；
- [ ] 自定义敏感键配置尚未提供；
- [x] 规则变更具有正反例测试。

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

### 11.1 评估前置条件

第一阶段刻意保留 `SessionStore` 对跨文件生命周期的协调，避免在机械拆分中改变写入顺序。
评估时点要求“并发锁、崩溃恢复、版本迁移完成后再看”，当前状态：

| 前置项 | 状态 | 模块 |
|---|---|---|
| 进程内 / 跨进程写锁 + fsync | 已完成 | `session_locking.py` |
| 一致性检查与索引重建 | 已完成 | `session_consistency.py` |
| 事件版本迁移与损坏诊断 | 已完成 | `session_records.py` |
| 敏感写入策略 | 已完成 | `session_artifacts.py` / `prompt_history.py` |

前置条件已满足，可以做正式拆分评估。

### 11.2 当前职责地图（2026-07-12）

| 模块 | 约行数 | 所有权 |
|---|---:|---|
| `session_models.py` | 271 | 事件/索引模型、ID 与时间工具 |
| `session_projection.py` | 74 | 事件 → 模型消息投影 |
| `session_artifacts.py` | 264 | 工具结果 / HTML artifact 与脱敏 |
| `prompt_history.py` | 301 | 提示历史 JSONL |
| `session_records.py` | 407 | 记录解码、版本迁移、诊断 |
| `session_consistency.py` | 611 | 一致性报告与索引重建纯逻辑 |
| `session_locking.py` | 313 | 跨进程锁与耐久写 |
| `session.py` (`SessionStore`) | 902 | **跨文件事务协调与对外门面** |

`SessionStore` 仍是唯一对外写入口；Agent / API 只依赖该门面，不直接碰 index 或 transcript 文件。

### 11.3 真实事务边界

关键写路径的真实原子单位**不是单文件**，而是跨文件组合：

1. **追加事件**：JSONL append → 读改写 `index.json`（同一把写锁）
2. **创建会话**：写 index 占位 → 追加 `session_started` → 再更新 index
3. **归档 / 恢复**：追加生命周期事件 → 移动 JSONL → 更新 index 路径与 `archived_at`
4. **删除 / 丢弃空会话**：删 JSONL + artifact 目录 + index 条目
5. **重建索引**：扫描 transcripts → 备份 index → 写新 index（不改 JSONL）

因此若拆出 `SessionIndexRepository` / `SessionTranscriptRepository`，**事务边界仍必须留在上层协调器**；仓库层只能是“带锁上下文下的文件 IO 助手”，不能各自独立提交。

### 11.4 拆分选项对比

| 选项 | 做法 | 收益 | 成本 / 风险 | 结论 |
|---|---|---|---|---|
| A. 现状保持 | `SessionStore` 继续协调 | 事务顺序集中、调用方零迁移 | 类约 900 行 | **推荐** |
| B. 内部私有仓库 | 同包抽出 `_IndexRepo` / `_TranscriptRepo`，不改公开 API | 略减 `session.py` 行数 | 多一层跳转；多数方法只调用一次 | 仅当某侧 IO 再膨胀时考虑 |
| C. 公开双仓库 | 对外暴露 index/transcript repository | 看起来边界清晰 | 调用方易绕过锁序；归档/删除事务易半完成；测试与 API 面扩大 | **不建议** |
| D. 一致性服务独立进程/包 | 把 check/rebuild 再抽成可部署服务 | 运维向扩展 | 当前无运维需求，过度设计 | **不建议** |

### 11.5 评估结论

**暂不拆分。** 理由：

1. **事务所有权已经清晰**：写锁 + fsync 在 `SessionStore._exclusive_write()`，一致性/解码/artifact/history 已是真实子模块，不再是“上帝文件堆逻辑”。
2. **行数不是拆分理由**：`session.py` 的 900 行主要是业务编排与路径校验；再拆会把一次 `append_event` 拆成多处跳转，违反“反对碎片化 / 一眼看到底”原则。
3. **公开双仓库会削弱安全边界**：锁序、路径边界、脱敏、归档移动必须单点控制；公开 repository 容易被旁路。
4. **一致性服务已存在且够用**：`session_consistency` + `check_consistency` / `rebuild_index` 已覆盖崩溃恢复入口，无需再套一层服务抽象。
5. **无新的状态所有者出现**：Agent facade / API 仍只认 `SessionStore`；没有第二个写入者需要独立 repository。

### 11.6 何时重新评估（触发条件）

满足任一条件再开专项，而不是现在预拆：

1. `session.py` 因**新状态所有者**（例如独立 attachment 生命周期、分支会话图、多根共享 store）继续显著膨胀；
2. 出现**第二种持久化后端**（SQLite / 远程对象存储）需要替换文件 IO；
3. 归档/导出/artifact 形成**可独立测试的长事务**，且当前编排已难单测；
4. 需要把一致性修复做成**离线运维命令**且与运行时写路径发布节奏分离。

若触发，优先采用 **选项 B（内部私有仓库）**：

- 保持 `SessionStore` 为唯一公开写门面；
- `_TranscriptRepository` 只负责 JSONL append/read/move；
- `_IndexRepository` 只负责 load/save/backup；
- 锁、脱敏、事件语义、归档业务仍留在 `SessionStore`；
- 不改 JSON/JSONL 格式，不改 Agent/API 调用面。

### 11.7 验收（本次评估）

- [x] 并发锁、崩溃恢复、版本迁移前置已完成后再评估；
- [x] 画出当前模块所有权与真实事务边界；
- [x] 对比公开拆分 / 内部拆分 / 保持现状；
- [x] 给出明确“做 / 不做”结论与再评估触发条件；
- [x] 不进行无行为收益的机械拆分。

## 12. 推荐处理顺序

1. SESSION-DEBT-002：HTML artifact 脱敏（已完成）；
2. SESSION-DEBT-003：PromptHistory 敏感信息策略（已完成）；
3. SESSION-DEBT-001：并发锁与 fsync 耐久性（已完成，含跨进程）；
4. SESSION-DEBT-004：一致性检查与索引重建（已完成）；
5. SESSION-DEBT-005、006：版本迁移与损坏诊断（已完成）；
6. SESSION-DEBT-009：repository 拆分评估（已完成：暂不拆分）；
7. SESSION-DEBT-008：仅在发现真实 pickle/反射调用方时处理。

SESSION-DEBT-007 已在前序闭环中完成。

每项应单独完成需求确认、测试、实施和验证，不建议合并为一次大改。

## 13. 更新记录

### 2026-07-12：SESSION-DEBT-009 repository 拆分评估

- 前置（锁 / 一致性 / 版本诊断）已齐，完成正式评估；
- 结论：**暂不拆分** `SessionIndexRepository` / `SessionTranscriptRepository`；
- `SessionStore` 继续作为跨文件事务唯一协调门面；
- 记录再评估触发条件；若将来拆分，仅允许内部私有仓库（选项 B）。

### 2026-07-12：SESSION-DEBT-001 跨进程锁与 fsync

- 新增 `session_locking.py`：跨平台文件锁、耐久追加、原子写；
- `SessionStore` 写路径统一走 `_exclusive_write()`；
- PromptHistory 追加接入 fsync；
- 新增 `tests/test_session_locking.py`（可重入、超时、多进程追加）；
- 全量测试 340 项通过。

### 2026-07-12：SESSION-DEBT-005/006 版本迁移与损坏诊断

- 新增 `session_records.py`：事件解码、legacy 迁移、结构化诊断；
- `SessionStore` / `PromptHistoryStore` 读取路径接入诊断；
- index 写入补充 `schema_version`；
- Agent/API 暴露 `load_session_diagnostics` 与两个 diagnostics 路由；
- 新增 `tests/test_session_records.py`；全量测试 335 项通过。

### 2026-07-12：SESSION-DEBT-004 一致性检查与索引重建

- 新增 `session_consistency` 真实模块，并由 `SessionStore` 暴露
  `check_consistency` / `rebuild_index`；
- 覆盖事件/消息计数、标题、最后事件、路径、归档状态、孤立转录、
  缺失索引、孤立 artifact 等诊断；
- `rebuild_index(apply=True)` 备份后仅重写 `index.json`；
- 新增 `tests/test_session_consistency.py`，边界测试纳入 `session_consistency`；
- 全量测试 328 项通过；实盘 42 会话一致性扫描通过。

### 2026-07-12：P0/P1 最小闭环

- **SESSION-DEBT-001（单进程部分）**：`SessionStore` 以会话根目录共享可重入锁，覆盖同一进程内的转录追加、索引读改写、PromptHistory 追加，以及归档/恢复、删除和空会话清理的“文件操作 + 索引更新”生命周期；`index.json` 改用同目录唯一临时文件再原子替换。跨进程文件锁与 `fsync` 已在后续闭环完成。
- **SESSION-DEBT-002**：新 HTML artifact 在写盘前清理常见文本、属性、脚本和内嵌 JSON 凭据；`html_sha256` 与 `html_size_chars` 指向实际持久化正文，并写入 `redacted: true`。既有 artifact 保持原样，不进行隐式改写。
- **SESSION-DEBT-003**：新 PromptHistory 的 `display` 和嵌套 `pasted_contents` 在 JSONL 追加前脱敏；读取旧记录时仅返回脱敏展示值，不修改旧文件。关闭 PromptHistory 的产品配置另行设计。
- **SESSION-DEBT-007**：结构化敏感字段由宽泛 `key` 子串匹配收紧为精确规范化字段集合，避免误删 `keyboard`、`monkey` 和 `key_count`；同时识别常见 `X-API-Key` 风格请求头，避免 API 凭据在会话或审计嵌套参数中明文持久化。
- 同期补充 MCP 审计 `output_preview` 脱敏，避免工具输出中的凭据写入 `logs/mcp-audit.jsonl`。

### 2026-07-12：初始登记

- 建立独立技术债文档；
- 从 P1 SessionStore 结构治理中迁出行为性和数据策略问题；
- 尚未修改任何相关运行行为或历史数据。
