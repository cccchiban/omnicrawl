/**
 * Cordis 宿主的进程内骨架：一个 Context 承载多个插件。
 *
 * 进程模型为「单 Cordis 宿主进程」（原生模型：插件间可 inject 组合，代价是崩溃与阻塞共享事件循环）。
 * 与之配套的验收项与安全边界见 `README.md`。
 */

import { Context } from 'cordis'
import { API_VERSION, HOOKS, dispatchHook } from '@omnicrawl/plugin-sdk'

export const SERVICE_NAME = 'omnicrawl'

export function createHost({ logger = console, timeoutMs = 5000 } = {}) {
  const ctx = new Context()
  const plugins = []

  ctx.provide(SERVICE_NAME, {
    apiVersion: API_VERSION,
    hooks: HOOKS,
    timeoutMs,
    logger,
    dispatch: (hook, mode, payload) => dispatchHook(ctx, hook, mode, payload, { logger }),
  })

  return {
    ctx,

    async load(plugin) {
      const fiber = await ctx.plugin(plugin)
      plugins.push(fiber)
      return fiber
    },

    async unload(fiber) {
      const index = plugins.indexOf(fiber)
      if (index >= 0) plugins.splice(index, 1)
      await fiber.dispose()
    },

    async dispose() {
      while (plugins.length) await plugins.pop().dispose()
    },

    dispatch(hook, mode, payload) {
      return dispatchHook(ctx, hook, mode, payload, { logger })
    },
  }
}
