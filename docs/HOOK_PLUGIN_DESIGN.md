# OmniCrawl Hook 生命周期与 NPM 插件系统设计

> 状态：已落地 V1.2（Phase 0–3 + 自定义事件总线 + `/plugins` + console script + 离线 store 安装验收；默认 `plugins.enabled=false`）  
> 适用范围：OmniCrawl Python Host、终端 CLI、NPM Hook 插件  
> 文档版本：1.3  
> 更新日期：2026-07-13

## 1. 背景与目标

OmniCrawl 当前具备 Agent 主循环、工具审批、会话、工作区切换、Skill 和 MCP 等扩展能力，但缺少统一的 Host 生命周期扩展点。若直接在 `LocalToolAgent.run_stream()`、工具执行或会话代码中散布第三方回调，会造成执行顺序不确定、安全检查被绕过、异常难以隔离和升级难以回滚。

本设计引入两层能力：

1. **Hook 生命周期层**：由 Python Host 在稳定的语义节点发布 Hook。
2. **NPM 插件层**：插件通过受版本约束的协议注册 Handler，并由 OmniCrawl CLI 完成安装、启用、更新、禁用、回滚和卸载。

目标如下：

- 为应用、工作区、会话、轮次、上下文、模型和工具定义稳定 Hook。
- 允许插件为既有 Hook **添加 Handler**。
- 允许插件在白名单字段内顺序 **修改 Hook 载荷**。
- 允许插件显式 **替换或屏蔽普通插件 Handler**。
- 允许插件在自身命名空间内 **自定义、添加和删除自定义 Hook**。
- 通过 `omnicrawl plugin ...` 安装和管理 NPM 插件。
- 保证插件不能删除核心生命周期阶段、绕过审批、路径保护、脱敏和会话一致性逻辑。
- 插件故障时可超时、熔断、禁用和回滚，不拖垮 OmniCrawl 主进程。

### 1.1 非目标

首期不实现：

- 不将 Skill、MCP 或 Python 包重新包装为 NPM 插件。
- 不允许插件动态创造 Python Host 中不存在的控制流插入点。
- 不在 Python 进程内执行 JavaScript，也不向插件暴露 Python 对象。
- 不提供插件市场、自动推荐或后台静默安装。
- 不把独立 Node 子进程宣称为 OS 级安全沙箱。
- 不允许插件替换核心审批器、工作区边界、凭据脱敏或会话持久化机制。

## 2. 术语与能力边界

| 术语 | 含义 |
|---|---|
| Hook | Python Host 发布的稳定生命周期事件，例如 `turn.start`。 |
| Handler | 某个插件注册到 Hook 上的处理器。 |
| Core Hook | Host 内置的生命周期阶段，插件只能订阅，不能删除。 |
| Custom Hook | 插件在自己的 `plugin.*` 命名空间中发布的事件。 |
| Observe | 只读观察事件，不改变 Host 流程。 |
| Transform | 返回受约束的 JSON Patch，修改可变载荷。 |
| Guard | 可拒绝操作，但不能直接批准 Host 尚未批准的操作。 |
| Tombstone | 注册表中的精确屏蔽规则，用于停用某个普通插件 Handler。 |
| Sealed | 不可替换、不可删除、不可 Patch 的安全核心处理逻辑。 |
| Plugin Worker | 单独承载一个插件的常驻 Node 子进程。 |

### 2.1 与现有扩展机制的关系

| 机制 | 谁发起调用 | 主要用途 | 是否能修改 Host 生命周期 |
|---|---|---|---|
| Skill | 模型按任务读取指令 | 提示词与工作流程 | 否 |
| MCP | 模型调用 Tool/Resource/Prompt | 外部能力接入 | 否 |
| Hook 插件 | Host 在确定节点主动分发 | 观察、校验、受控变换 | 是，限明确白名单 |

三者可以共存。Hook 插件不得借助 Skill 或 MCP 绕过 Host 权限；MCP Tool 最终仍经过现有工具审批路径。

## 3. 核心设计原则

1. **Host 控制流程**：Hook 插入点和最终安全决策由 Python Host 掌握。
2. **默认最小权限**：manifest 未声明的 Hook、模式和权限不可在运行期扩大。
3. **稳定且可重放**：Handler 使用稳定 ID，执行顺序确定，并记录版本与完整性。
4. **变换而非任意替换**：数据修改使用 RFC 6902 JSON Patch，并限制可修改路径。
5. **安全逻辑不可删除**：核心审批、路径校验、脱敏和持久化均为 sealed。
6. **每轮快照**：一个 turn 开始后使用不可变 Handler 计划，启停变更从下一轮生效。
7. **故障隔离**：一插件一 Worker，设置超时、消息上限、熔断与关闭流程。
8. **可回滚安装**：版本目录不可原地覆盖，注册表通过原子指针切换版本。

## 4. Hook 事件模型

### 4.1 命名规范

- Core Hook 使用小写点分名称：`<domain>.<action>.<phase>`。
- 推荐 phase：`before`、`after`、`error`、`cancelled`。
- 自定义 Hook 必须位于：`plugin.<规范化包名>.<event>`。
- 插件不能注册 `core.*`，也不能伪造 `tool.*`、`model.*` 等 Host Hook。
- Hook API 使用独立主版本号；不兼容变更提升 `apiVersion`。

### 4.2 统一事件信封

```ts
interface HookEvent<TPayload = unknown> {
  apiVersion: "1";
  eventId: string;
  hook: string;
  timestamp: string;
  sequence: number;
  workspace: {
    id: string;
    root?: string;
  };
  sessionId?: string;
  turnId?: string;
  payload: TPayload;
  deadlineMs: number;
  trace: {
    parentEventId?: string;
    depth: number;
  };
}
```

规则：

- `eventId` 在一次分发中唯一；`sequence` 在当前 Agent 实例内单调递增。
- `workspace.root` 仅在插件获准 `workspace:metadata` 时提供，否则只提供不可逆 ID。
- 事件发送前由 Host 裁剪字段并脱敏，永不包含 API Key、Bearer Token、完整 `config.json`。
- 自定义事件最大递归深度默认为 4，超过后拒绝，防止事件环。
- 单条协议消息默认不超过 1 MiB；超限载荷截断或拒绝，并记录诊断。

### 4.3 Handler 模式

| 模式 | 能力 | 执行方式 | 默认失败策略 |
|---|---|---|---|
| `observe` | 读取裁剪后的事件，返回注解 | 不同插件可并行；同插件串行 | fail-open |
| `transform` | 对白名单路径返回 Patch | 按确定顺序串行 | fail-open，忽略非法 Patch |
| `guard` | 返回 `continue` 或 `deny` | 按确定顺序串行 | 由 Hook 策略决定 |
| `notify` | 接收终态通知 | 异步尽力而为，不阻塞主路径 | fail-open |

插件的 `deny` 可以收紧安全边界；插件的 `continue` 只表示插件不拒绝，不能替代 Host 审批。

## 5. V1 Core Hook 清单

### 5.1 Hook 矩阵

| Hook | 触发点 | 模式 | 可变字段 | 可阻断 | 超时/错误策略 |
|---|---|---|---|---|---|
| `app.start.before` | Bootstrap PluginRuntime 完成握手后、Agent 创建前 | observe/guard | 无 | 是 | 插件显式 deny 阻断启动；插件故障则禁用该插件并继续无插件启动 |
| `app.start.after` | Agent 与 UI 准备完成 | notify | 无 | 否 | 忽略插件故障 |
| `app.stop.before` | 正常关闭 Agent 资源前 | observe | 无 | 否 | 忽略插件故障 |
| `app.stop.after` | Agent/会话资源关闭后、Plugin Worker 关闭前 | notify | 无 | 否 | 尽力而为，完成后关闭 Worker |
| `workspace.switch.before` | 校验新路径后、重建子系统前 | guard | 无 | 是 | fail-closed |
| `workspace.switch.after` | 新工作区子系统完成重建 | notify | 无 | 否 | fail-open |
| `workspace.switch.error` | 切换失败 | notify | 无 | 否 | fail-open |
| `session.start.after` | 新会话已持久化 | notify | 无 | 否 | fail-open |
| `session.resume.before` | 会话存在性和工作区检查后 | guard | 无 | 是 | fail-closed |
| `session.resume.after` | 上下文恢复完成 | notify | 无 | 否 | fail-open |
| `session.resume.error` | 恢复失败 | notify | 无 | 否 | fail-open |
| `session.close.before` | 写入关闭事件前 | observe | 无 | 否 | fail-open |
| `session.close.after` | 会话关闭完成 | notify | 无 | 否 | fail-open |
| `turn.start` | `/skill:name` 解析后、模型上下文构建前 | transform/guard | `/payload/userText`、`/payload/tags` | 是 | fail-closed；Patch 非法则忽略该 Handler |
| `turn.end` | 最终回复和会话事件落盘后 | notify | 无 | 否 | fail-open |
| `turn.error` | 本轮异常已记录 | notify | 无 | 否 | fail-open |
| `turn.cancelled` | 用户取消且清理完成 | notify | 无 | 否 | fail-open |
| `context.build.before` | 构造上下文消息前 | transform | `/payload/additionalContext` | 否 | fail-open |
| `context.build.after` | 上下文构造完成、请求模型前 | observe | 无 | 否 | fail-open |
| `model.request.before` | 请求体建成、发送前 | transform/guard | `/payload/messages/*/content`、允许的采样参数 | 是 | fail-closed；不暴露凭据 |
| `model.response.after` | 完整模型响应规范化并完成流式展示后 | observe | 无 | 否 | 忽略插件故障；V1 不允许修改已展示文本 |
| `model.request.error` | 模型请求最终失败 | notify | 无 | 否 | fail-open |
| `tool.call.before` | 模型 Tool Call 规范化后 | transform/guard | 每个工具 schema 允许的参数路径 | 是 | fail-closed |
| `tool.approval.before` | Host 审批前 | observe/guard | 仅风险注解 | 是，只能拒绝 | fail-closed |
| `tool.approval.after` | 审批结果确定后 | notify | 无 | 否 | fail-open |
| `tool.execute.before` | 已获 Host 批准、执行工具前 | guard | 无 | 是 | fail-closed |
| `tool.execute.after` | 工具结果规范化并脱敏后 | observe/transform | `/payload/displayText`、`/payload/annotations` | 否 | fail-open |
| `tool.execute.error` | 工具执行失败并规范化后 | notify | 无 | 否 | fail-open |

### 5.2 Hook 失败策略

`fail-open/fail-closed` 不足以描述所有情况。每个 Hook 必须配置明确的 `HookPolicy`：

```ts
interface HookPolicy {
  onDeny: "reject-operation" | "ignore";
  onTimeout: "reject-operation" | "skip-handler";
  onProtocolError: "reject-operation" | "skip-handler";
  onHandlerError: "reject-operation" | "skip-handler";
  disablePluginOnError: boolean;
}
```

- **显式 deny** 与 **插件故障** 分开处理。例如 `app.start.before` 的 deny 阻断启动，但 Worker 崩溃只禁用该插件并继续无插件启动。
- 工具执行前、工作区切换前等高风险 guard 默认在 timeout/protocol error 时拒绝当前操作；终态 notify 一律跳过失败 Handler。
- 熔断只影响后续事件，不得把已经确定的 Host 审批结果改成允许。
- 每个 Hook 的最终策略在实现前固化为常量，并由测试逐项覆盖 deny、timeout、protocol error、handler error。

### 5.3 不可开放的 sealed 逻辑

以下逻辑不是可删除 Handler：

- `approval.mode` 决策及人工确认结果。
- 工作区路径边界与受保护路径检查。
- API Key、Token、Cookie、密码等敏感信息脱敏。
- Tool Call schema 校验和 shell 调用边界。
- 会话事件一致性、版本校验和持久化。
- Hook manifest、权限、Patch allowlist 与协议校验。

因此，“删除 Hook”的精确定义是：

1. 删除插件自定义 Hook 的定义或订阅；
2. 卸载插件时删除该插件的 Handler；
3. 通过 tombstone 屏蔽一个明确的普通插件 Handler；
4. **不能删除 Core Hook 阶段或 sealed Host 逻辑**。

## 6. 添加、修改、替换与删除语义

### 6.1 Handler 标识与排序

Handler 唯一键：

```text
<npm-package-name>/<handler-id>
示例：@acme/omnicrawl-redactor/redact-tool-output
```

执行顺序：

1. mode 阶段：guard → transform → observe/notify；
2. `priority` 从大到小；
3. scope：project 高于 user；
4. NPM 包名按 Unicode 码点升序；
5. handler ID 升序。

最终解析结果写入 lock，避免依赖文件系统顺序。

### 6.2 添加

插件可向 Core Hook 添加 Handler：

```json
{"id":"tag-turn","hook":"turn.start","mode":"transform","priority":50}
```

插件也可静态声明并发布自定义事件：

```text
plugin.acme-omnicrawl-metrics.batch-flushed
```

V1 自定义 Hook 必须在 manifest 的 `customEvents` 中静态声明 payload schema、可见性和版本。所谓“删除自定义 Hook”是指插件升级后的 manifest 移除声明，或插件禁用/卸载后由执行计划移除该定义与订阅；V1 不支持运行时动态定义和注销 Hook。自定义事件不产生新的 Host 控制流位置，只在插件事件总线上流转。

### 6.3 修改

`transform` Handler 返回 RFC 6902 Patch：

```json
{
  "action": "patch",
  "patch": [
    {"op":"add","path":"/payload/tags/-","value":"reviewed"}
  ]
}
```

Host 必须：

1. 校验 op、路径、值类型与大小；
2. 在事件副本上应用 Patch；
3. 重新执行 Hook schema 校验；
4. 成功后把结果传给下一个 Handler；
5. 记录 before/after hash 和被修改路径，不默认记录完整正文。

首期只支持 `add`、`replace`、`remove`，且 `remove` 只能用于 Hook 明确允许的可选载荷字段。

### 6.4 显式替换

插件可声明：

```json
{
  "id": "new-redactor",
  "hook": "tool.execute.after",
  "mode": "transform",
  "replaces": ["@old/omnicrawl-redactor/redactor"]
}
```

约束：

- 只能替换普通插件 Handler，不能替换 `core/*`。
- 源和目标必须属于同一 Hook。
- 多个插件同时替换同一目标视为冲突，冲突插件不启用。
- 替换链出现循环时禁用相关插件并由 `plugin doctor` 报告。
- 不允许仅凭更高 priority 隐式覆盖其他 Handler。

### 6.5 删除与 tombstone

项目注册表可精确屏蔽用户级 Handler：

```json
{
  "disabledHandlers": [
    "@example/omnicrawl-telemetry/send-metrics"
  ]
}
```

- tombstone 只影响指定 Handler，不删除 NPM 包。
- 目标不存在时记录 warning，便于未来版本出现该 Handler 时仍保持屏蔽。
- 卸载插件会取消该插件全部 Handler 注册。
- `--purge` 仅清理能够证明没有任何注册表引用的缓存版本；由于项目注册表可分散在任意工作区，project scope 默认只解除注册，不物理删除共享 store，避免破坏其他项目引用。

### 6.6 Transform 的权威值与持久化

- transform 成功后的最终载荷是后续 Host 执行的 authoritative value；原始值只保留脱敏审计哈希。
- `turn.start` 必须在 PromptHistory 和 Session `user_message` 落盘前完成，因此修改后的 `userText` 同时用于模型输入、UI 确认信息和会话恢复。
- `tool.call.before` 修改参数后，最终参数必须重新走 schema、审批、路径和删除意图检查，并作为会话中的实际 Tool Call 保存。
- `context.build.before` 的附加上下文不改写用户原始消息，只作为独立的、带来源标记的上下文项进入当前请求。
- V1 `model.response.after` 为 observe，避免流式 UI、返回值、历史和会话文本分叉。

## 7. NPM 插件 Manifest

插件仍使用标准 `package.json`，在 `omnicrawl` 字段声明扩展元数据。

### 7.1 示例

```json
{
  "name": "@example/omnicrawl-redactor",
  "version": "1.2.3",
  "type": "module",
  "main": "dist/plugin.js",
  "files": ["dist", "README.md", "LICENSE"],
  "engines": {"node": ">=20"},
  "omnicrawl": {
    "apiVersion": "1",
    "engines": {"omnicrawl": ">=0.1 <0.2", "node": ">=20"},
    "entry": "dist/plugin.js",
    "timeoutMs": 2000,
    "permissions": [
      "hook:tool.execute.after"
    ],
    "hooks": [
      {
        "id": "redact-tool-output",
        "hook": "tool.execute.after",
        "mode": "transform",
        "priority": 100
      }
    ],
    "customEvents": [
      {
        "name": "plugin.example-omnicrawl-redactor.rules-reloaded",
        "version": 1,
        "visibility": "private",
        "schema": {"type": "object", "additionalProperties": false}
      }
    ]
  }
}
```

### 7.2 字段约束

| 字段 | 必填 | 约束 |
|---|---|---|
| `name` | 是 | 合法 NPM 包名，作为插件身份的一部分。 |
| `version` | 是 | 精确 SemVer，安装后不可原地覆盖。 |
| `type` | 建议 | 首期推荐 `module`。 |
| `omnicrawl.apiVersion` | 是 | V1 只接受字符串 `"1"`。 |
| `omnicrawl.engines.omnicrawl` | 是 | 必须覆盖当前 Host 版本。 |
| `omnicrawl.engines.node` | 是 | 首期要求 Node.js 20 LTS 或更高兼容版本。 |
| `omnicrawl.entry` | 是 | 必须位于包根目录内，规范化后不可路径逃逸。 |
| `omnicrawl.hooks` | 是 | Handler ID 在包内唯一，Hook/mode 必须有效；订阅 Custom Hook 时必须声明 `eventVersion` 或兼容范围。 |
| `omnicrawl.customEvents` | 否 | 静态声明本插件命名空间下的事件、整数版本、可见性和 JSON Schema；升级移除声明即删除。 |
| `omnicrawl.permissions` | 是 | 声明全部 Hook 与附加能力。 |
| `omnicrawl.timeoutMs` | 否 | 只能比 Host 全局上限更小。 |

安装身份由以下信息共同确定：

```text
package name + exact version + registry/tarball source + integrity + apiVersion
```

### 7.3 权限建议

| 权限 | 说明 |
|---|---|
| `hook:<name>` | 注册指定 Core Hook。 |
| `hook:custom-emit` | 发布本插件命名空间下的自定义 Hook。 |
| `hook:custom-subscribe` | 订阅其他插件公开的自定义 Hook。 |
| `workspace:metadata` | 获取经过裁剪的工作区元数据。 |
| `session:metadata` | 获取会话 ID、标题等非正文信息。 |
| `content:model` | 获取脱敏后的模型消息内容。 |
| `content:tool` | 获取脱敏后的工具参数或结果。 |

manifest 声明与运行期注册取交集。运行期 `activate()` 不能扩大 Hook、mode、权限或超时。

### 7.4 无效插件示例

以下插件必须拒绝安装或启用：

- `entry` 为 `../../payload.js`。
- 声明未知 `apiVersion: "2"`。
- 注册 `tool.execute.after` 却没有 `hook:tool.execute.after` 权限。
- 重复 Handler ID，或同一 ID 在多个 Hook 中出现。
- `replaces` 指向 `core/approval`。
- 运行期注册 manifest 未声明的 Hook。
- 发布未在 `customEvents` 声明的事件，或跨插件订阅 private 事件。
- 包或完整生产依赖锁的 integrity 与 lock 不一致。

## 8. 插件 SDK 契约

SDK 只提供窄接口，不暴露 Python 对象、文件句柄或 Host 内部服务。

```ts
export interface PluginDefinition {
  activate(context: PluginContext): void | Promise<void>;
  deactivate?(): void | Promise<void>;
}

export interface PluginContext {
  readonly plugin: { name: string; version: string; apiVersion: "1" };
  readonly runtime: { omnicrawlVersion: string; nodeVersion: string };
  hooks: {
    // on() 只能激活 manifest 已声明的 Handler；返回值用于 deactivate 时解除运行期绑定。
    on<T>(registration: RuntimeHandlerRegistration<T>): () => void;
    // 只能发布 manifest.customEvents 中静态声明的事件。
    emitCustom<T>(event: string, payload: T): Promise<void>;
  };
  logger: {
    debug(message: string, fields?: Record<string, unknown>): void;
    info(message: string, fields?: Record<string, unknown>): void;
    warn(message: string, fields?: Record<string, unknown>): void;
    error(message: string, fields?: Record<string, unknown>): void;
  };
}

export function definePlugin(plugin: PluginDefinition): PluginDefinition;
```

响应类型：

```ts
type HookResult =
  | { action: "continue"; annotations?: Record<string, unknown> }
  | { action: "deny"; reason: string; code?: string }
  | { action: "patch"; patch: JsonPatchOperation[]; annotations?: Record<string, unknown> };
```

插件代码可使用 Node 自身 API，这也是独立 Worker **不等于安全沙箱** 的原因。首期只应安装可信来源插件；manifest permissions 只约束 Host 通过 Hook 协议提供的数据与能力，不阻止恶意包直接尝试使用 `fs` 或 `net`。Host 启动 Worker 时必须使用环境变量白名单，明确移除 API Key、Token、Cookie、代理凭据等；但在没有 OS 沙箱或独立低权限账户时，仍不能保证恶意插件无法读取当前用户可访问的配置文件。

## 9. Python Host 与 Node Worker 边界

### 9.1 进程模型

- 进程级 `PluginRuntime` 由 CLI bootstrap 在 LLM 配置和 `LocalToolAgent` 创建前初始化，用于承载 `app.*` Hook；它持有工作区级 `PluginManager`。
- 工作区级 `PluginManager` 为每个启用插件启动一个常驻 Node Worker；Agent 创建时注入该 Manager，而不是等会话启动后才创建。
- 初始会话创建/恢复必须发生在工作区 PluginManager 就绪之后，才能正确发布 `session.start.after` 和 `session.resume.*`。
- Worker 命令由 Host 固定，插件不能替换 runner：

```text
node <omnicrawl-node-runner.mjs> --plugin-root <canonical-path>
```

- stdin/stdout 使用 NDJSON 承载 JSON-RPC 2.0；stdout 只能输出协议消息。
- 插件日志经 SDK 发送结构化消息；普通 console 输出重定向到 stderr。
- stderr 按大小轮转收集，不作为协议输入。
- 同一插件请求串行处理，不同插件的只读 observe 可并行。

### 9.2 协议方法

| 方法 | 方向 | 用途 |
|---|---|---|
| `initialize` | Host → Worker | 传入 Host/API 版本、获批权限和 manifest 摘要。 |
| `initialized` | Worker → Host | 返回实际注册项和能力，供 Host 二次校验。 |
| `hook.invoke` | Host → Worker | 调用一个 Handler。 |
| `hook.cancel` | Host → Worker | 请求取消超时调用。 |
| `custom.emit` | Worker → Host | 请求发布 manifest 静态声明的插件自定义事件；Host 校验名称、版本、schema、可见性和递归深度。 |
| `ping` | Host → Worker | 健康检查。 |
| `shutdown` | Host → Worker | 正常停用与关闭。 |

### 9.3 启动时序

```text
CLI 创建进程级 PluginRuntime
  -> PluginRuntime 读取目标工作区 registry/lock
  -> 校验 manifest、版本、完整依赖锁、integrity、entry
  -> 创建工作区 PluginManager 并启动固定 Node runner
  -> initialize
  -> Worker import 插件并调用 activate()
  -> initialized 返回实际 Handler
  -> Host 校验其为 manifest 声明子集
  -> 构建不可变执行计划
  -> 插件进入 active
```

握手失败时插件保持禁用，不阻塞其他插件和无插件模式启动。

### 9.4 单次调用时序

```text
Host 创建脱敏事件副本
  -> 计算全局 deadline
  -> hook.invoke
  -> Worker 执行指定 Handler
  -> 返回 continue / deny / patch
  -> Host 校验响应和 Patch
  -> 应用结果或记录拒绝
  -> 写入审计摘要
```

### 9.5 超时、崩溃与关闭

- observe 默认 500 ms；transform/guard 默认 2 s。
- manifest 可缩短超时，不能超过 Host `max_timeout_ms`。
- 超时后先发送 `hook.cancel`；宽限期后终止 Worker。
- 连续 3 次失败进入当前 Agent 生命周期的熔断状态。
- Worker 退出、重复响应、未知 request ID、无效 JSON、stdout 噪声均视为协议失败。
- 正常关闭先调用插件 `deactivate()`，再响应 `shutdown`；超时则终止进程树。
- 关闭顺序固定为：`app.stop.before` → 关闭 Agent/会话资源 → `app.stop.after` → Worker shutdown；`app.stop.after` 不能阻止退出。

## 10. PluginManager 架构

建议新增：

```text
omnicrawl/extensions/
├── plugin_models.py       # manifest、事件、响应、注册项
├── plugin_registry.py     # project/user 合并、锁、tombstone、原子更新
├── plugin_install.py      # NPM 安装、更新、卸载、回滚
├── plugin_protocol.py     # Python 侧 Worker 协议客户端
├── plugin_manager.py      # Worker 生命周期、执行计划、调度、熔断
└── node_runner.mjs        # Node 侧单插件 runner
```

职责边界：

| 模块 | 职责 |
|---|---|
| `plugin_models.py` | 纯数据模型和 schema 校验。 |
| `plugin_registry.py` | 注册表、scope 合并、排序、replace/remove、锁与原子写。 |
| `plugin_install.py` | 下载、完整性校验、暂存、启停、版本切换与清理。 |
| `plugin_protocol.py` | NDJSON framing、请求关联、超时、取消、进程回收。 |
| `plugin_manager.py` | 进程级 PluginRuntime、工作区 PluginManager、Worker 管理、每轮快照、dispatch、审计和熔断。 |
| `node_runner.mjs` | 加载一个锁定插件，提供固定 SDK 与协议适配。 |

核心流程只调用统一的：

```python
result = plugin_manager.dispatch(hook_name, payload, policy=hook_policy)
```

不为每个 Hook 创建大量只调用一次的微型方法，也不把 manifest、协议和安装逻辑塞入 `LocalToolAgent`。

## 11. OmniCrawl CLI 设计

### 11.1 入口演进

当前 `main.py` 的 argparse 仅支持 `--resume`，并在普通启动路径加载 LLM 配置和 Textual。插件管理必须先于这些步骤路由，否则在没有 API Key 或 UI 依赖时无法安装和诊断插件。

正式入口：

```text
omnicrawl plugin <command>
```

源码兼容入口：

```text
python main.py plugin <command>
```

实现时建议新增 `omnicrawl/cli.py` 统一 parser，并在 Python 包元数据中声明 console script。原 `python main.py --resume <id>` 行为保持兼容。Windows 下必须在调用 `launch_in_powershell_window()` **之前**识别 `plugin` 子命令并直接运行 CLI；只有普通 TUI 路径允许弹出新窗口，否则脚本无法获得真实 stdout 和退出码。

### 11.2 命令契约

```text
omnicrawl plugin install <package-spec> [--project|--user] [--enable] [--yes]
omnicrawl plugin system enable|disable
omnicrawl plugin list [--project|--user|--all] [--json]
omnicrawl plugin info <name> [--json]
omnicrawl plugin enable <name> [--project|--user]
omnicrawl plugin disable <name> [--project|--user]
omnicrawl plugin update <name> [--to <version>] [--project|--user] [--yes]
omnicrawl plugin rollback <name> [--project|--user]
omnicrawl plugin uninstall <name> [--project|--user] [--purge] [--yes]
omnicrawl plugin doctor [name] [--json]
```

作用域默认值：

- 在检测到 OmniCrawl 工作区时默认 `--project`。
- 无项目上下文时默认 `--user`。
- 自动选择前必须在输出中显示目标注册表路径。
- 脚本或 CI 中建议始终显式指定作用域。

### 11.3 行为与退出码

| 退出码 | 含义 |
|---|---|
| `0` | 成功；重复安装同版本且配置一致也视为幂等成功。 |
| `2` | CLI 参数、scope 或 package spec 无效。 |
| `3` | Node/npm 缺失或版本不兼容。 |
| `4` | 包下载、离线缓存或 NPM registry 错误。 |
| `5` | manifest、权限、entry、integrity 或兼容性校验失败。 |
| `6` | 用户拒绝权限或版本变更。 |
| `7` | registry/lock 并发、原子写或恢复失败。 |
| `8` | Worker 握手或冒烟验证失败，旧版本保持 active。 |

`--json` 输出必须将诊断写入结构化字段；人类日志写 stderr，结果写 stdout。

- `plugin enable` 只改变某个 scope 的插件启用态；若全局 `plugins.enabled=false`，命令必须提示继续执行 `plugin system enable`。`install --enable` 不得静默修改全局开关。
- V1 `package-spec` 仅支持 NPM registry 的 `name`、`name@exact-version`、`name@tag`（tag 安装前解析为精确版本）。Git URL、HTTP tarball、alias 和本地目录不进入 Phase 3 安装器；本地路径仅用于 Phase 1 开发模式，不写入可回滚 store。

### 11.4 安装流水线

```text
解析 package spec
  -> 检查 Node/npm 版本
  -> 查询并解析精确版本
  -> 下载 tarball 到暂存目录
  -> 校验 registry integrity
  -> 读取 package.json 与 omnicrawl manifest
  -> 校验 entry、API/Host/Node 版本和权限
  -> npm install --ignore-scripts --omit=dev --no-audit --no-fund
  -> 生成并保存完整 production package-lock，校验全部传递依赖版本与 integrity
  -> 将 lockfile 哈希纳入内容寻址 store 身份
  -> 校验生产依赖树和最终文件边界
  -> 展示来源、版本、integrity、Hook 与权限差异
  -> 用户确认（除非安全允许且显式 --yes）
  -> 原子移动到内容寻址 store
  -> 在不改变 active 指针的前提下记录 candidate（初始 disabled）
  -> 对 candidate 执行 Worker 握手冒烟检查
  -> 冒烟成功后一次原子事务更新 registry/lock
  -> 新安装仅在 --enable 时把 candidate 原子切换为 active
  -> 已启用插件执行 update 时，冒烟成功后默认原子切换 active；使用 --no-activate 可只保留 candidate
```

安全要求：

- 首期始终使用 `--ignore-scripts`，不执行 `preinstall/install/postinstall`。
- 需要原生构建或 lifecycle scripts 的插件首期拒绝，不提供隐式降级。
- `--yes` 只能跳过普通交互，不能跳过来源、integrity、权限和兼容性校验。
- 升级出现新增权限时必须重新确认；CI 可用独立的预批准权限文件，不能仅靠 `--yes`。
- 安装过程中失败只清理暂存目录或 candidate，不修改当前 active 指针。
- 顶层包 integrity 不足以复现依赖树；store 与 rollback 必须同时校验 production lockfile 哈希及其中全部依赖 integrity。

## 12. 存储与注册表

### 12.1 推荐路径

```text
~/.omnicrawl/plugins/
├── store/
│   └── <escaped-name>/<version>-<integrity-prefix>/
├── registry.json
├── registry.lock
└── audit/

<workspace>/.omnicrawl/
├── plugins.json
└── plugins.lock.json
```

- store 是内容寻址的不可变版本缓存。
- 项目注册表只记录声明、启用态、配置、版本和 integrity，不把 `node_modules` 复制到源码目录。
- 项目 scope 覆盖用户 scope；冲突由稳定规则解析。
- `.omnicrawl/plugins.lock.json` 是否提交由项目决定；若需团队复现应提交，但不得包含密钥和绝对私人路径。

### 12.2 注册表示例

```json
{
  "schemaVersion": 1,
  "plugins": {
    "@example/omnicrawl-redactor": {
      "enabled": true,
      "active": {
        "version": "1.2.3",
        "integrity": "sha512-...",
        "lockfileHash": "sha256-...",
        "source": "https://registry.npmjs.org"
      },
      "candidate": null,
      "previous": {
        "version": "1.2.2",
        "integrity": "sha512-...",
        "lockfileHash": "sha256-..."
      },
      "approvedPermissions": ["hook:tool.execute.after"]
    }
  },
  "disabledHandlers": []
}
```

写入采用：文件锁 → 同目录临时文件 → flush/fsync → 原子 replace。读到损坏注册表时不得覆盖原文件，应进入无插件降级模式并由 `plugin doctor` 给出恢复路径。

## 13. 配置设计

建议在 `config.json` 增加：

```json
{
  "plugins": {
    "enabled": false,
    "default_timeout_ms": 1000,
    "max_timeout_ms": 5000,
    "failure_threshold": 3,
    "max_message_bytes": 1048576,
    "custom_event_max_depth": 4,
    "allow_network_install": false,
    "audit_log_enabled": true
  }
}
```

- 首次发布默认 `enabled: false`，由用户显式开启。
- 环境变量只允许全局禁用、超时收紧或路径覆盖，不允许静默授予权限。
- CLI 注册表写入与 `config.json` 分离，避免版本锁和普通运行配置互相覆盖。
- 插件配置中禁止存储明文密钥；未来如需 secrets，应接入独立凭据提供器。

## 14. 与当前代码的集成点

| 当前位置 | Hook/改动 |
|---|---|
| `omnicrawl/entry.py::_parse_args()` / `run_application()`，根 `main.py` launcher | 在 Windows launcher、LLM 配置和 UI 加载前路由 `plugin` 子命令；普通 TUI 路径保持原行为。 |
| `omnicrawl/cli.py` bootstrap | 在 Agent 与初始 Session 之前创建进程级 PluginRuntime/工作区 PluginManager，发布 `app.start.before`。 |
| `omnicrawl/agent/core.py::LocalToolAgent.__init__()` | 接收已就绪的 PluginManager，再创建/恢复 Session；不因单插件失败阻断 Agent。 |
| `LocalToolAgent.run_stream()` | `/skill:name` 解析后发布 `turn.start`；用 `try/except/finally` 保证 end/error/cancelled 终态恰好一个。 |
| `agent/prompt_context.py` 上下文构造点 | 发布 context Hook，只接收 schema 校验后的附加上下文或允许 Patch。 |
| 模型请求方法周围 | 发布 model Hook；发送前移除 API Key 等传输配置。V1 的流式响应完成后只允许 observe；若未来要 transform，必须先缓冲完整响应并在 Hook 后统一输出、持久化。 |
| 工具规范化、审批、执行节点 | 发布 tool Hook；审批结果由 Host 决定，插件只能拒绝或附加风险。 |
| `LocalToolAgent.switch_workspace()` | 切换前 guard；关闭旧项目 Worker；切换成功后按新 registry 重建。 |
| `LocalToolAgent.close()` | 停止接收新事件，完成关闭 Hook，回收全部 Worker 进程树。 |
| `omnicrawl/commands/slash.py` | 可增加只读 `/plugins` 状态；安装/更新/卸载不在活跃 Agent 内执行。 |
| `omnicrawl/config/runtime.py` | 复用配置读取校验风格；插件 registry 使用独立原子存储实现。 |
| SessionStore | 新增插件诊断/决策事件类型，不修改既有 user/tool 事件契约。 |
| SkillManager | 不改变 Skill 发现、手动加载和 prompt 边界；Hook 只能修改显式允许的附加上下文。 |

### 14.1 turn 终态约束

每个 `turn.start` 必须对应且只对应以下之一：

- `turn.end`
- `turn.error`
- `turn.cancelled`

即使插件、模型或工具异常，也应先完成 Host 自身状态清理和会话记录，再发送终态通知。通知失败不得覆盖原始异常。

## 15. 审批、安全与威胁模型

### 15.1 审批关系

- 安装/更新审批关注：来源、精确版本、integrity、Hook、权限和权限增量。
- 运行时工具审批继续使用现有 `manual`、`auto`、`review` 模式。
- `tool.approval.before` 插件可以 `deny`，不能返回“代表用户批准”。
- 插件修改后的 Tool 参数必须重新通过工具 schema、路径边界和删除意图检查。
- 插件不能修改审批结果事件。
- “插件不能读取凭据”的 V1 保证仅限于 Hook 协议不提供凭据、Worker 不继承敏感环境变量；没有 OS 沙箱时不承诺阻止恶意插件使用当前用户文件权限。

### 15.2 主要威胁与措施

| 威胁 | 设计措施 | 剩余风险 |
|---|---|---|
| 恶意 NPM lifecycle script | 强制 `--ignore-scripts` | 插件运行后仍拥有当前用户的 Node 进程权限。 |
| 供应链版本替换 | 精确版本、source、integrity lock、不可变 store | 上游首次发布即恶意仍无法自动识别。 |
| entry 路径逃逸 | canonical path 必须位于包根目录 | Node 依赖本身仍可能访问外部路径。 |
| stdout 协议注入 | 固定 runner 接管 stdout，日志走 stderr；严格 JSON-RPC request ID | 恶意插件可能直接写 fd 1，需视为协议失败并终止。 |
| 凭据泄露 | 字段裁剪、脱敏、不给完整配置；Worker 环境白名单移除敏感变量 | 插件仍可通过 Node `fs` 读取当前用户可访问文件；严格隔离需 OS 沙箱/低权限账户。 |
| 审批绕过 | sealed 审批，插件只可拒绝；修改参数后重新审批/校验 | Host 接入遗漏 Hook 后重校验会形成漏洞，需集成测试。 |
| 事件递归/风暴 | 自定义命名空间、深度限制、事件数与消息大小限制 | 大量合法小事件仍可能耗时，需速率限制。 |
| 拒绝服务 | 超时、取消、熔断、一插件一进程、进程树回收 | 同一用户权限下的恶意 Worker 可主动消耗系统资源。 |
| 日志泄密 | 只记录摘要、哈希和 Patch 路径，复用凭据清理 | 插件自己的 stderr 仍可能输出秘密，日志目录需限权和轮转。 |
| Handler 劫持 | 显式 replaces、tombstone、冲突检测、sealed core | 用户主动批准恶意替换仍可能改变非核心行为。 |

首期产品文案必须明确：**插件隔离用于故障边界，不构成恶意代码沙箱，只安装可信插件。**

## 16. 审计与可观测性

每次 Hook 调用至少记录：

- event ID、Hook 名称和时间；
- 插件名、版本、integrity 前缀、Handler ID；
- mode、priority、scope、耗时；
- `continue/deny/patch/error/timeout/circuit-open`；
- Patch 路径列表与前后哈希；
- 错误分类和 Worker 重启/熔断状态。

默认不记录完整 prompt、Tool 参数和 Tool 输出。审计写入前使用项目现有敏感信息清理策略。`plugin doctor` 汇总最近握手、协议、超时、冲突、权限和 integrity 诊断。

## 17. 升级、回滚与恢复

### 17.1 安装/升级状态机

```text
downloaded
  -> verified
  -> staged
  -> candidate-recorded（不改变 active）
  -> smoke-tested
  -> active（原子切换）
```

- 任一步失败，删除暂存并保留旧 active 版本。
- 新版本不覆盖旧 store；注册表使用 `active`、`candidate`、`previous` 三个独立指针。只有 candidate 冒烟成功后才通过一次原子事务切换 `active`。
- 新安装默认保持 disabled，只有 `--enable` 才激活；已启用插件执行 update 默认在冒烟成功后切换 active，可用 `--no-activate` 只准备 candidate。
- 权限增加必须重新审批。
- 冒烟握手失败时返回退出码 8，不切换 active。

### 17.2 回滚

`plugin rollback <name>`：

1. 校验 `previous` 版本、顶层 integrity、production lockfile 哈希及完整依赖树仍正确；
2. 启动 Worker 完成握手；
3. 原子交换 `active` 与 `previous`；
4. 下一轮使用新执行计划；
5. 记录审计。

### 17.3 中断恢复

| 场景 | 恢复结果 |
|---|---|
| 下载/安装失败 | 清理 staging，registry 不变。 |
| 更新后握手失败 | 旧版本继续 active，新版本保留为诊断对象或清理。 |
| 运行期 Worker 崩溃 | 当前 Handler 按 Hook 策略失败；达到阈值后熔断，Host 主流程继续。 |
| registry 损坏 | 保留损坏文件，只禁用插件启动；`doctor` 从 lock/备份建议恢复。 |
| 卸载中断 | 注册表原子写保证仍注册或已解除注册，不出现半条记录。 |
| 工作区切换中断 | 关闭已启动的新 Worker，恢复旧工作区状态或报告切换失败。 |

## 18. 测试策略

测试不得依赖公网 NPM、真实凭据或全局插件目录。使用临时 registry、fixture tarball 和假 Worker。

| 测试层 | 关键用例 |
|---|---|
| Manifest | 合法字段、未知 API 版本、SemVer、entry 逃逸、重复 ID、权限不足、sealed replace。 |
| Registry | project/user 合并、稳定排序、tombstone、replace 冲突、循环、原子写、并发锁、rollback。 |
| Installer | 固定 `--ignore-scripts`、integrity 不符、权限增量、幂等安装、staging 失败、purge 引用检查。 |
| Protocol | 握手、NDJSON 分帧、噪声、未知 ID、超时、cancel、崩溃、重复响应、超大消息。 |
| Dispatcher | observe 并行、transform 串行、Patch allowlist、guard deny、熔断、每轮不可变快照。 |
| Agent 集成 | 正常 end、模型 error、用户 cancelled、工具批准/拒绝/失败、工作区切换和 close。 |
| 安全回归 | 插件不能批准工具、不能 Patch sealed 字段、不能通过 Hook 协议或继承环境取得凭据、修改参数后重新校验；另验证并记录无 OS 沙箱时的剩余文件权限风险。 |
| 兼容回归 | 无 `plugins` 配置、plugins disabled、Node 缺失时，TUI、Skill、MCP、Session、`--resume` 行为不变。 |

建议未来测试文件：

```text
tests/test_plugin_manifest.py
tests/test_plugin_registry.py
tests/test_plugin_install.py
tests/test_plugin_protocol.py
tests/test_plugin_manager.py
tests/test_agent_hooks.py
tests/fixtures/npm_plugins/
```

项目级验证命令：

```powershell
python -m unittest discover -s tests
```

Node runner 协议可同时使用 Python 假 Worker 做快速单测，以及仓库锁定 Node 版本做少量端到端测试。

## 19. 分阶段实施计划

### Phase 0：内部 HookDispatcher

- 只实现 Core Hook、事件 schema、终态约束和审计。
- 不加载外部插件。
- 验收：无 Handler 时性能和行为与当前版本一致。

### Phase 1：本地插件与 observe

- 支持显式开发模式的本地路径插件、Worker 协议、`list/info/doctor/enable/disable`；本地路径不进入可回滚 store。
- 只开放 observe/notify。
- 验收：插件崩溃、超时不会破坏 Agent 主流程。

### Phase 2：受限修改与删除

- 开放 transform/guard、JSON Patch allowlist、replace、tombstone、每轮快照。
- 验收：审批和 sealed 安全测试全部通过；执行顺序可复现。

### Phase 3：NPM 安装与回滚

- 开放 install/update/uninstall/rollback。
- 实现 integrity lock、权限审批、原子 store/registry。
- 验收：安装失败和升级失败均能保持旧版本可用。

### Phase 4：安全增强（可选）

- 评估可信 registry、包签名、组织 allowlist、OS 沙箱和资源配额。
- 是否开放更多 Hook 必须逐项评审载荷、Patch 路径和失败策略。

全阶段保留全局回滚开关：

```json
{"plugins":{"enabled":false}}
```

## 20. 关键设计取舍

| 决策 | 选择 | 原因与代价 |
|---|---|---|
| JS 执行位置 | 一插件一 Node Worker | 故障隔离和协议清晰；增加进程与启动成本。 |
| 插件安装载体 | NPM 包 + `package.json.omnicrawl` | 复用版本、依赖和分发生态；引入供应链风险。 |
| 数据修改方式 | 受限 JSON Patch | 可审计、可排序、易校验；不如任意回调灵活。 |
| 删除语义 | tombstone/卸载/自定义 Hook 删除 | 保证核心生命周期稳定；不允许删除 Host 控制流。 |
| npm scripts | 首期全部禁用 | 降低安装期代码执行风险；不支持需构建的插件。 |
| 运行时启停 | 下一轮生效 | 避免半轮执行计划变化；状态反馈不是瞬时生效。 |
| 安全定位 | 可信插件 + 能力约束，不称沙箱 | 符合真实边界；恶意 Node 代码仍需 OS 隔离解决。 |
| 插件管理入口 | 进程级 CLI | 可在无 API Key/UI 时管理，避免活跃 Agent 内改依赖。 |

## 21. 验收标准

设计落地后至少满足：

1. `omnicrawl plugin install <spec> --project` 能锁定精确版本和 integrity，默认不执行 npm scripts。
2. 插件能添加 Core Hook Handler，并按稳定顺序执行。
3. transform 只能修改 Hook 白名单字段，非法 Patch 不进入 Host 状态。
4. guard 可拒绝工具或轮次，但不能批准 Host 未批准的 Tool Call。
5. 插件能通过 manifest 静态定义自己命名空间内的自定义 Hook；升级移除声明、禁用或卸载后，下一轮执行计划删除其定义与订阅。
6. replace/tombstone 能修改或屏蔽普通插件 Handler，不能作用于 sealed core。
7. Worker 超时、崩溃和协议污染可被隔离、审计和熔断。
8. 更新握手失败时旧版本继续可用，`rollback` 可原子恢复上一版本。
9. 工作区切换会关闭旧项目 Worker，并从新项目 registry 重建。
10. `plugins.enabled=false`、未安装 Node 或无插件配置时，不影响现有 TUI、Skill、MCP、Session 与 API 主路径。
11. 所有 transform Hook 的权威值与持久化顺序一致，不出现模型输入、UI、历史和 Session 恢复内容分叉。

## 22. 已冻结 / 仍待产品确认

已冻结（V1.2）：

1. 正式包名 `omnicrawl`，SemVer `0.1.0`，console script：`omnicrawl = omnicrawl.__main__:main`（见 `pyproject.toml` / `setup.py`）。
2. Node.js 最低版本固定为 **20 LTS**（`detect_node_npm` 强制）。
3. 入口兼容：`python main.py plugin ...`、`python -m omnicrawl plugin ...`、`omnicrawl plugin ...`。

仍待产品确认：

1. 项目级 `.omnicrawl/plugins.lock.json` 是否默认纳入 Git。
2. 首批开放的 transform Patch 路径是否进一步收紧。
3. 各 guard Hook 的 `deny`、`timeout`、`protocol_error`、`handler_error` 四类策略矩阵是否再细分。
4. 是否允许组织级可信 registry/包名 allowlist（Phase 4）。
