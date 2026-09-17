# @omnicrawl/plugin-sdk

Cordis 之上的 OmniCrawl 契约层：钩子白名单、Handler 模式约束、模式到 Cordis 分发方式的映射、
跨 handler 超时策略。**不含框架逻辑**——加载、依赖注入、事件分发、可逆副作用全部由 Cordis 提供。

## 导出

| 导出 | 作用 |
| --- | --- |
| `API_VERSION` | 插件契约版本（当前 `"1"`） |
| `HOOKS` | 钩子表：每个钩子允许的 Handler 模式与 JSON Patch 白名单 |
| `MODE_DISPATCH` | Handler 模式 → Cordis 分发方式 |
| `hookModes(hook)` | 某钩子允许的模式；未知钩子返回 `null` |
| `isCustomHook(hook)` | 是否自定义钩子（须以 `plugin.` 开头） |
| `isAllowed(hook, mode)` / `assertHookRegistration(hook, mode)` | 契约校验 |
| `onHook(ctx, hook, mode, listener, options?)` | 按契约注册钩子，含超时包装 |
| `dispatchHook(ctx, hook, mode, payload, options?)` | 宿主｛Desensitized:1362｝派发（通常不必直接调用） |

## 四种 Handler 模式

| 模式 | 语义 | Cordis 分发 |
| --- | --- | --- |
| `transform` | 顺序改写载荷：声明顺序即应用顺序 | `serial` + 状态对象 |
| `guard` | 判定：任一 handler 给出判定即短路后续 | `bail` |
| `observe` | 并发观察：全部执行，错误聚合，不短路 | `parallel` |
| `notify` | 纯通知：不 await、返回值忽略 | `emit` |

```js
import { onHook } from '@omnicrawl/plugin-sdk'

export const name = 'example'
export const inject = ['omnicrawl']

export function apply(ctx, config) {
  onHook(ctx, 'turn.start', 'transform', (state) => {
    state.payload = { ...state.payload, tags: [...(state.payload.tags ?? []), 'example'] }
  })

  onHook(ctx, 'tool.execute.before', 'guard', (payload) => {
    if (payload.name === 'danger') return { allowed: false, reason: '不允许该工具' }
    return undefined
  })

  onHook(ctx, 'turn.end', 'notify', (payload) => {
    ctx.logger.info(`回合结束：${payload.turnId}`)
  })

  onHook(ctx, 'plugin.example.demo', 'transform', (state) => {
    state.payload = `示例：${state.payload}`
  })
}
```

## 两个容易踩的点

1. **`transform` 不用 Cordis 的 `waterfall`**。`waterfall` 是 Koa 式中间件，`next()` 不接收参数，
   改写只能发生在返回链上，顺序与 registration顺序相反、且容易静默丢数据。因此 `transform`
   映射到 `serial`：包装层把一个 `state` 对象传给每个 handler，handler 读 `state.payload` 并写回改写结果；
   包装层统一丢弃 handler 返回值，插件无法误让链条短路。
2. **`guard` 的短路条件是「返回非 `null`/`false`/`undefined`」**（Cordis 的 `isBailed` 语义），
   不是抛异常。返回 `{ allowed: false, reason }` 即终止该钩子的后续 handler。

## 超时

`onHook(..., { timeoutMs })` 默认 5000ms。超时按 skip-handler 处理：不提交改写、不给出判定，
并记一条警告（含插件名、钩子名与模式）。

## 契约来源

`src/hooks.generated.json` 由 `python rust/tools/gen_plugin_hook_table.py` 从
`omnicrawl/extensions/plugin_models.py` 生成，请勿手改。
