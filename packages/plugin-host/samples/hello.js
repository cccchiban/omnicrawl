import { onHook } from '@omnicrawl/plugin-sdk'

export const name = 'hello'
export const inject = ['omnicrawl']

export function apply(ctx, config = {}) {
  const greeting = config.greeting ?? '你好'

  // transform：读 state.payload、写回改写结果；声明顺序即应用顺序。
  onHook(ctx, 'turn.start', 'transform', (state) => {
    state.payload = {
      ...state.payload,
      tags: [...(state.payload.tags ?? []), 'hello'],
    }
  })

  // notify：纯通知，返回值被忽略。
  onHook(ctx, 'turn.end', 'notify', (payload) => {
    ctx.logger?.info?.(`${greeting}：回合 ${payload?.turnId ?? '(未命名)'} 结束`)
  })

  // guard：返回非 null/false/undefined 即短路该钩子的后续 handler。
  onHook(ctx, 'tool.execute.before', 'guard', (payload) => {
    if (payload?.name === 'danger') {
      return { allowed: false, reason: '示例插件拒绝该工具' }
    }
    return undefined
  })

  // 自定义钩子：名字必须以 plugin. 开头。
  onHook(ctx, 'plugin.hello.greet', 'transform', (state) => {
    state.payload = `${greeting}，${state.payload}`
  })

  // 可逆副作用：插件卸载时由 Cordis 自动撤销。
  ctx.effect(() => () => {
    ctx.logger?.info?.(`${greeting}：hello 插件已卸载，副作用已回滚`)
  })
}
