// 端到端：插件宿主当协议 v1 的宿主对端，驱动真实内核二进制跑完回合。
//
// 前置：cargo build --release -p omnicrawl-cli
import assert from 'node:assert/strict'
import { existsSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { test } from 'node:test'

import { onHook } from '@omnicrawl/plugin-sdk'

import { createAgentHost } from '../src/agent-host.js'
import { createHost } from '../src/host.js'
import { createKernelClient } from '../src/kernel-client.js'

const here = dirname(fileURLToPath(import.meta.url))
const repoRoot = resolve(here, '../../..')
const binary = join(
  repoRoot,
  'rust',
  'target',
  'release',
  process.platform === 'win32' ? 'omnicrawl.exe' : 'omnicrawl',
)

const SKIP = existsSync(binary)
  ? false
  : '未找到内核二进制：先 cargo build --release -p omnicrawl-cli'

function toolCall(name, args, id) {
  return { name, arguments: args, id, function_name: name }
}

function assistant(content, toolCalls = []) {
  return {
    message: toolCalls.length
      ? { role: 'assistant', content, tool_calls: toolCalls }
      : { role: 'assistant', content },
    content,
    tool_calls: toolCalls,
    reasoning: '',
    content_streamed: false,
  }
}

/** 脚本化模型：按顺序吐出预设回复，并记下每次收到的请求。 */
function scriptedModel(replies) {
  const seen = []
  return {
    seen,
    model: async (request) => {
      seen.push(request.messages)
      const next = replies.shift()
      if (!next) throw new Error('脚本化模型没有更多回复了。')
      return next
    },
  }
}

async function waitFor(predicate, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    const value = predicate()
    if (value) return value
    await new Promise((done) => setTimeout(done, 10))
  }
  throw new Error('等待条件超时')
}

/** 观察型监听器在宿主按 transform 派发时收到的是 state，取值要兼容两种形状。 */
const valueOf = (received) => received?.payload ?? received

async function startBridge({ host, model, tools }) {
  const kernel = createKernelClient({ binary })
  const bridge = createAgentHost({ host, kernel, model, tools })
  await kernel.initialize({ name: 'plugin-host-test' })
  return { kernel, bridge }
}

test('回合跑通：turn.start 改写、工具由注册表执行、turn.end 收到结果', { skip: SKIP }, async (t) => {
  const host = createHost()
  const executed = []
  const ends = []
  const hookNames = []

  await host.load({
    name: 'bridge-probe',
    apply(ctx) {
      onHook(ctx, 'turn.start', 'transform', (state) => {
        state.payload = { ...state.payload, userText: `${state.payload.userText}（插件改写）` }
      })
      onHook(ctx, 'tool.call.before', 'transform', (state) => {
        state.payload = {
          ...state.payload,
          arguments: { ...state.payload.arguments, extra: true },
        }
      })
      onHook(ctx, 'tool.execute.after', 'observe', (received) => {
        hookNames.push(valueOf(received).tool)
      })
      onHook(ctx, 'turn.end', 'notify', (payload) => {
        ends.push(payload)
      })
    },
  })

  const echo = toolCall('echo', { text: '你好' }, 'c1')
  const scripted = scriptedModel([assistant('', [echo]), assistant('完成')])
  const { kernel, bridge } = await startBridge({
    host,
    model: scripted.model,
    tools: {
      echo: async (args) => {
        executed.push(args)
        return { ok: true, output: `echo:${args.text}` }
      },
    },
  })
  t.after(() => kernel.close())

  const { turnId } = await bridge.runTurn('你好')

  assert.deepEqual(scripted.seen[0], [{ role: 'user', content: '你好（插件改写）' }])
  assert.deepEqual(executed, [{ text: '你好', extra: true }])
  assert.deepEqual(hookNames, ['echo'])
  assert.equal(scripted.seen[1].length, 3, '用户消息 + assistant 工具调用 + 工具观察')
  assert.equal(scripted.seen[1][2].content, 'echo:你好')

  const ended = await waitFor(() => ends.find((payload) => payload.turnId === turnId))
  assert.equal(ended.final_text, '完成')
  assert.equal(ended.model_turns, 2)
  assert.equal(ended.tool_calls, 1)
  await host.dispose()
})

test('插件拒绝工具：模型看到拒绝原因，工具未执行', { skip: SKIP }, async (t) => {
  const host = createHost()
  const guardCalls = []

  await host.load({
    name: 'denier',
    apply(ctx) {
      onHook(ctx, 'tool.execute.before', 'guard', (payload) => {
        guardCalls.push(payload.tool)
        return { allowed: false, reason: '测试拒绝' }
      })
    },
  })

  const executed = []
  const echo = toolCall('echo', { text: '你好' }, 'c1')
  const scripted = scriptedModel([assistant('', [echo]), assistant('换个办法')])
  const { kernel, bridge } = await startBridge({
    host,
    model: scripted.model,
    tools: {
      echo: async (args) => {
        executed.push(args)
        return { ok: true, output: '不该执行到这里' }
      },
    },
  })
  t.after(() => kernel.close())

  await bridge.runTurn('试试被拒的工具')

  assert.deepEqual(guardCalls, ['echo'])
  assert.deepEqual(executed, [], '被拒绝的工具不应执行')
  assert.equal(scripted.seen[1][2].content, '工具 echo 被插件拒绝：测试拒绝')
  await host.dispose()
})

test('未注册工具与抛异常都回成观察，不中断回合', { skip: SKIP }, async (t) => {
  const host = createHost()
  const errors = []

  await host.load({
    name: 'error-probe',
    apply(ctx) {
      onHook(ctx, 'tool.execute.error', 'notify', (payload) => {
        errors.push(payload)
      })
    },
  })

  const unknown = toolCall('nope', {}, 'c1')
  const boom = toolCall('boom', {}, 'c2')
  const scripted = scriptedModel([assistant('', [unknown, boom]), assistant('知道了')])
  const { kernel, bridge } = await startBridge({
    host,
    model: scripted.model,
    tools: {
      boom: async () => {
        throw new Error('炸了')
      },
    },
  })
  t.after(() => kernel.close())

  await bridge.runTurn('用两个坏工具')

  const observed = scripted.seen[1].slice(2).map((message) => message.content)
  assert.deepEqual(observed, ['未注册的工具 nope。', '工具执行失败：炸了'])
  const reported = await waitFor(() => errors.find((payload) => payload.tool === 'boom'))
  assert.equal(reported.error, '炸了')
  await host.dispose()
})

test('回合内取消：runTurn 抛 -32003 并触发 turn.cancelled', { skip: SKIP }, async (t) => {
  const host = createHost()
  const cancelled = []

  await host.load({
    name: 'cancel-probe',
    apply(ctx) {
      onHook(ctx, 'turn.cancelled', 'notify', (payload) => {
        cancelled.push(payload)
      })
    },
  })

  const hung = scriptedModel([])
  hung.model = async (request) => {
    hung.seen.push(request.messages)
    return new Promise(() => {})
  }

  const { kernel, bridge } = await startBridge({ host, model: hung.model, tools: {} })
  t.after(() => kernel.close())

  const pending = bridge.runTurn('慢活儿', { turnId: 't-cancel' })
  await waitFor(() => hung.seen.length === 1)
  await bridge.cancelTurn('t-cancel')

  await assert.rejects(pending, (error) => {
    assert.equal(error.frame?.error?.code, -32003)
    assert.equal(error.frame?.error?.data?.kind, 'cancelled')
    return true
  })
  await waitFor(() => cancelled.find((payload) => payload.turnId === 't-cancel'))
  await host.dispose()
})
