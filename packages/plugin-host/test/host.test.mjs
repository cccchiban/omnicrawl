import assert from 'node:assert/strict'
import { test } from 'node:test'

import {
  API_VERSION,
  HOOKS,
  assertHookRegistration,
  onHook,
} from '@omnicrawl/plugin-sdk'

import { createHost } from '../src/host.js'

test('插件在 omnicrawl 服务就绪后激活，并能读到契约表', async () => {
  const host = createHost()
  let observed = null

  await host.load({
    name: 'contract-reader',
    inject: ['omnicrawl'],
    apply(ctx) {
      observed = {
        apiVersion: ctx.omnicrawl.apiVersion,
        hookCount: Object.keys(ctx.omnicrawl.hooks).length,
      }
    },
  })

  assert.equal(observed.apiVersion, API_VERSION)
  assert.equal(observed.hookCount, Object.keys(HOOKS).length)
  await host.dispose()
})

test('transform 钩子按注册顺序依次改写', async () => {
  const host = createHost()

  await host.load({
    name: 'first',
    apply(ctx) {
      onHook(ctx, 'turn.start', 'transform', (state) => {
        state.payload = { ...state.payload, order: [...(state.payload.order ?? []), 'first'] }
      })
    },
  })
  await host.load({
    name: 'second',
    apply(ctx) {
      onHook(ctx, 'turn.start', 'transform', (state) => {
        state.payload = { ...state.payload, order: [...(state.payload.order ?? []), 'second'] }
      })
    },
  })

  const result = await host.dispatch('turn.start', 'transform', { userText: '你好' })
  assert.deepEqual(result.order, ['first', 'second'])
  assert.equal(result.userText, '你好')
  await host.dispose()
})

test('guard 返回判定后短路该钩子的后续 handler', async () => {
  const host = createHost()
  const calls = []

  await host.load({
    name: 'denier',
    apply(ctx) {
      onHook(ctx, 'tool.execute.before', 'guard', () => {
        calls.push('denier')
        return { allowed: false, reason: '测试拒绝' }
      })
    },
  })
  await host.load({
    name: 'later-guard',
    apply(ctx) {
      onHook(ctx, 'tool.execute.before', 'guard', () => {
        calls.push('later-guard')
        return undefined
      })
    },
  })

  const decision = await host.dispatch('tool.execute.before', 'guard', { name: 'danger' })
  assert.deepEqual(calls, ['denier'])
  assert.equal(decision.allowed, false)
  await host.dispose()
})

test('卸载后钩子解绑且可逆副作用回滚', async () => {
  const host = createHost()
  let hits = 0
  let disposed = false

  const fiber = await host.load({
    name: 'rollback',
    apply(ctx) {
      onHook(ctx, 'turn.end', 'notify', () => {
        hits += 1
      })
      ctx.effect(() => () => {
        disposed = true
      })
    },
  })

  await host.dispatch('turn.end', 'notify', {})
  assert.equal(hits, 1)

  await host.unload(fiber)
  await host.dispatch('turn.end', 'notify', {})
  assert.equal(hits, 1, '卸载后钩子不应再触发')
  assert.equal(disposed, true, '卸载应执行 effect 的清理函数')
  await host.dispose()
})

test('契约校验拒绝未声明的模式与未知钩子', () => {
  assertHookRegistration('turn.end', 'notify')
  assertHookRegistration('plugin.acme.demo', 'transform')
  assert.throws(() => assertHookRegistration('turn.end', 'transform'), /turn\.end/)
  assert.throws(() => assertHookRegistration('unknown.hook', 'notify'), /未知的钩子/)
  assert.throws(() => assertHookRegistration('turn.start', 'nonsense'), /未知的 Handler 模式/)
})

test('超时 handler 按 skip-handler 策略跳过并记录警告', async () => {
  const warnings = []
  const logger = { info() {}, warn: (message) => warnings.push(message) }
  const host = createHost({ logger })

  await host.load({
    name: 'slowpoke',
    apply(ctx) {
      onHook(ctx, 'tool.execute.after', 'observe', () => new Promise(() => {}), {
        timeoutMs: 20,
        logger,
      })
    },
  })

  await host.dispatch('tool.execute.after', 'observe', { name: 'bash' })
  assert.equal(warnings.length, 1)
  assert.match(warnings[0], /slowpoke/)
  assert.match(warnings[0], /tool\.execute\.after/)
  await host.dispose()
})
