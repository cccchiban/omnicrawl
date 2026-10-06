/**
 * OmniCrawl 插件 SDK：在 Cordis 之上提供 OmniCrawl 的钩子契约与事件命名空间。
 *
 * 分工：Cordis 负责插件加载、依赖注入、事件分发与可逆副作用；本模块只负责 OmniCrawl 的契约层
 * ——钩子白名单、Handler 模式约束、模式到 Cordis 分发方式的映射，以及跨墙超时策略。
 * 钩子表在 `hooks.generated.json` 里，不在本文件手写，避免与内核侧漂移。
 */

import { readFileSync } from 'node:fs'

const table = JSON.parse(
  readFileSync(new URL('./hooks.generated.json', import.meta.url), 'utf8'),
)

export const API_VERSION = table.api_version

/** Handler 模式到 Cordis 事件分发方式的映射。 */
export const MODE_DISPATCH = Object.freeze({
  transform: 'serial',
  guard: 'bail',
  observe: 'parallel',
  notify: 'emit',
})

export const HOOKS = Object.freeze(table.hooks)

export function hookModes(hook) {
  return HOOKS[hook]?.modes ?? null
}

/** 自定义钩子必须以 `plugin.` 开头，与 Python 侧 is_custom_hook 规则一致。 */
export function isCustomHook(hook) {
  return hook.startsWith('plugin.')
}

export function isAllowed(hook, mode) {
  if (!Object.hasOwn(MODE_DISPATCH, mode)) return false
  if (isCustomHook(hook)) return true
  return hookModes(hook)?.includes(mode) ?? false
}

export function assertHookRegistration(hook, mode) {
  if (!Object.hasOwn(MODE_DISPATCH, mode)) {
    throw new Error(`未知的 Handler 模式：${mode}`)
  }
  if (isAllowed(hook, mode)) return
  const modes = hookModes(hook)
  if (!modes) {
    throw new Error(`未知的钩子：${hook}（自定义钩子需以 plugin. 开头）`)
  }
  throw new Error(`钩子 ${hook} 不允许 ${mode} 模式，允许：${modes.join(', ')}`)
}

/** 事件名；加前缀以免与 Cordis 内部事件重名。 */
export function hookEvent(hook) {
  return `omnicrawl/${hook}`
}

export * from './protocol.js'

const TIMED_OUT = Symbol('omnicrawl.timeout')

/** 单 handler 超时包装：超时按 skip-handler 策略处理（变换退回入参，判定退回无判定）。 */
function withTimeout(listener, { timeoutMs, name, hook, mode, logger }) {
  if (!timeoutMs) return listener
  return async (...args) => {
    const result = await Promise.race([
      Promise.resolve().then(() => listener(...args)),
      new Promise((resolve) => {
        setTimeout(() => resolve(TIMED_OUT), timeoutMs)
      }),
    ])
    if (result !== TIMED_OUT) return result
    logger?.warn?.(
      `[plugin] ${name} 的 ${hook}(${mode}) 超过 ${timeoutMs}ms，已跳过该 handler。`,
    )
    // skip-handler：不提交改写、也不给出判定，等价于该 handler 不存在。
    return undefined
  }
}

/** 按契约注册钩子；返回解绑函数（Cordis 的 effect 也会在插件卸载时自动撤销）。 */
export function onHook(ctx, hook, mode, listener, options = {}) {
  assertHookRegistration(hook, mode)
  const { timeoutMs = 5000, logger = console } = options
  const name = ctx.fiber?.name ?? 'anonymous'
  return ctx.on(
    hookEvent(hook),
    withTimeout(listener, { timeoutMs, name, hook, mode, logger }),
  )
}

/** 由宿主调用：按钩子声明的模式派发。 */
export async function dispatchHook(ctx, hook, mode, payload, options = {}) {
  const { logger = console } = options
  assertHookRegistration(hook, mode)
  const event = hookEvent(hook)
  switch (MODE_DISPATCH[mode]) {
    case 'emit':
      ctx.emit(event, payload)
      return undefined
    case 'parallel':
      await ctx.parallel(event, payload)
      return undefined
    case 'bail':
      return ctx.bail(event, payload)
    case 'serial': {
      // transform：顺序折叠。监听器收到同一个 state，读 state.payload 并写回改写结果；
      // 声明顺序即应用顺序，且包装层已统一丢弃返回值，插件无法误让链条短路。
      const state = { payload }
      await ctx.serial(event, state)
      return state.payload
    }
    default:
      throw new Error(`未知的 Handler 模式：${mode}`)
  }
}
