# @omnicrawl/plugin-host

Cordis 单进程宿主，同一进程同时是**内核协议 v1 的宿主对端**：内核的 `tool.batch`（工具执行）与
`model.reply`（模型代答）由这里应答，回合与工具阶段派发成插件钩子。**进程模型：单 Cordis 宿主进程**
（原生模型——插件间可 `inject` 组合，代价是崩溃与阻塞共享事件循环）。

## 依赖与版本策略

`cordis` 固定为 **4.0.0-rc.10**（上游明示 API 未稳定，可能随时变更）。所有 Cordis 调用只出现在三处：
`src/host.js`、`src/agent-host.js` 与 `@omnicrawl/plugin-sdk` 的 `onHook` / `dispatchHook`。
上游破坏性变更时只改这三处，插件作者与内核协议不受影响。

## 分工

| 归属 | 内容 |
| --- | --- |
| Cordis | 插件加载、`inject` 依赖注入、recommended注册、依赖顺序激活、事件分发、可逆副作用、热重载 |
| 内核（Rust） | 回合循环、取消与预算、会话与 Provider 流（工具执行与模型调用的**请求方**） |
| 本宿主 | 传输与桥接、工具注册表执行、模型代答、钩子派发、npm 安装与审批（过渡期） |

## 目录

```
src/host.js          宿主骨架：建 Context、提供 omnicrawl 服务、加载/卸载插件
src/kernel-client.js 传输：起内核子进程、分帧、请求应答配对、事件与请求分流
src/agent-host.js    桥接：tool.batch 执行器、model.reply 代答、钩子派发
src/main.js          CLI：加载插件、派发一次演示钩子，或跑一个真实回合
samples/hello.js     示例插件（四种模式 + 自定义钩子 + 可逆副作用）
test/host.test.mjs        node:test：注入激活、顺序改写、guard 短路、卸载回滚、契约校验、超时跳过
test/kernel-bridge.test.mjs  node:test：驱动真内核跑通回合、拒绝、异常、取消
```

## 钩子挂点

| 时机 | 钩子 | 模式 | 效果 |
| --- | --- | --- | --- |
| 提交回合前 | `turn.start` | transform | 可改 `userText` / `tags`，改写结果送进内核 |
| 调模型前 | `model.request.before` | transform | 可改 `messages` 与采样参数 |
| 模型返回后 | `model.response.after` | observe | 只读观察 |
| 每个工具调用前 | `tool.call.before` | transform | 可改 `arguments` |
| 执行前裁决 | `tool.execute.before` | guard | `{allowed:false, reason}` → 回 `ok:false / error_code:denied` |
| 执行后 | `tool.execute.after` | transform | `{tool, ok, displayText, annotations}`；界面字段改写暂被丢弃 |
| 工具抛异常 | `tool.execute.error` | notify | `{tool, error}` |
| 回合结束 | `turn.end` | notify | 内核 `turn.finished` 的参数 |
| 回合取消/失败 | `turn.cancelled` / `turn.error` | notify | 按内核错误码分流（-32003 → cancelled） |

载荷键名与 Python 侧一致（`tool` / `arguments` / `displayText` / `annotations`），同一批插件在两个宿主上读到同一套字段。

## 运行

```bash
npm install                                   # 仓库根目录，workspaces 自动链接
node packages/plugin-host/src/main.js --plugin ./packages/plugin-host/samples/hello.js
node packages/plugin-host/src/main.js --plugin ./samples/hello.js \
  --hook plugin.hello.greet --mode transform --payload '"世界"'

# 协议 v1 模式：起内核、跑一个回合（--model 指向宿主侧模型客户端模块，默认导出 (request) => reply）
cargo build --release -p omnicrawl-cli
node packages/plugin-host/src/main.js --plugin ./samples/hello.js \
  --kernel rust/target/release/omnicrawl --model ./my-model.js --turn "你好"

cd packages/plugin-host && node --test
```

## 保留的安全策略

- 单 handler 超时按 **skip-handler** 处理（默认 5000ms，`onHook(..., { timeoutMs })` 可覆盖）：
  超时不提交改写、不给出判定，等价于该 handler 不存在。
- 契约校验：未声明的 Handler 模式与未知钩子（自定义钩子须以 `plugin.` 开头）直接报错。
- 每个内核请求都有且只有一次响应：处理器抛错或不响应时由 `kernel-client` 补 `-32603`，内核不会干等。
- 工具执行与审批权威仍在 Rust 内核，插件无法绕过。

## 进程模型的已知代价

单进程共享事件循环：一个插件崩溃或长时间阻塞会影响其他插件。需要隔离时的接缝已经留好——
`src/host.js` 的加载入口是唯一的插件装载点，将来可以按 manifest 起子进程，在子进程内跑同一个
Context（Cordis 仍是唯一框架，不需要另写一套）。

## 已知缺口（后续步骤）

1. 真实工具层与审批仍在 Python 过渡宿主；本仓库只提供工具注册表，由调用方把工具交进来。
2. TypeScript 类型：SDK 目前是纯 ESM JS，插件作者的 `.d.ts` 待补。
3. 失败熔断：`on_handler_error` / `disable_plugin_on_error` 尚未实现（当前只有超时策略）。
4. 热重载：`@cordisjs/plugin-loader` 未接入。
5. npm 安装、manifest 解析与审批仍由 Python 侧持有。
6. 自研 hook 分发（`omnicrawl/extensions/plugin_protocol.py`、`node_runner.mjs` 的 `hook.invoke`）
   已冻结、待删除。

## 与 Python 侧的一致性

钩子契约表由 `python rust/tools/gen_plugin_hook_table.py` 从
`omnicrawl/extensions/plugin_models.py` 抽取生成（`packages/plugin-sdk/src/hooks.generated.json`）。
Python 侧改了钩子就必须重新生成，否则宿主侧契约与实现脱节。协议 v1 的方法名与错误码以
`rust/docs/protocol-v1.md` 为准，JS 侧镜像在 `@omnicrawl/plugin-sdk` 的 `protocol.js`。
