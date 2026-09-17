// 端到端：通过启动器驱动真实内核二进制，跑协议 v1 的完整回合、回合内取消、握手与错误码。
//
// 前置：cargo build --release -p omnicrawl-cli && node packages/cli/scripts/prepare.mjs
import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import { existsSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { test } from 'node:test'

const here = dirname(fileURLToPath(import.meta.url))
const repoRoot = resolve(here, '../../..')
const launcher = join(here, '..', 'bin', 'omnicrawl.mjs')
const binaryName = process.platform === 'win32' ? 'omnicrawl.exe' : 'omnicrawl'
const binary = join(repoRoot, 'rust', 'target', 'release', binaryName)

const SKIP = existsSync(binary)
  ? false
  : '未找到 cargo 产物（rust/target/release）：先 cargo build --release -p omnicrawl-cli'

const toolCall = {
  name: 'read_file',
  arguments: { path: 'a.py' },
  id: 'c1',
  function_name: 'read_file',
}

const observation = {
  tool_call: toolCall,
  result: { ok: true, output: 'print(1)', full_output: '', error_code: null, retryable: false },
  message: { role: 'tool', tool_call_id: 'c1', content: 'print(1)' },
  followup_messages: [],
}

/** 极小的协议客户端：按行读写，按 id 配响应，顺带记下所有入站帧。 */
class KernelClient {
  constructor(child) {
    this.child = child
    this.frames = []
    this.pending = new Map()
    this.nextId = 0
    this.cursor = 0
    this.buffer = ''
    child.stdout.setEncoding('utf8')
    child.stdout.on('data', (chunk) => this.#consume(chunk))
  }

  #consume(chunk) {
    this.buffer += chunk
    let index = this.buffer.indexOf('\n')
    while (index >= 0) {
      const line = this.buffer.slice(0, index).trim()
      this.buffer = this.buffer.slice(index + 1)
      if (line) this.#handle(JSON.parse(line))
      index = this.buffer.indexOf('\n')
    }
  }

  #handle(frame) {
    this.frames.push(frame)
    if (frame.method || frame.id === undefined) return
    const pending = this.pending.get(frame.id)
    if (!pending) return
    this.pending.delete(frame.id)
    if (frame.error) {
      pending.reject(Object.assign(new Error(frame.error.message), { frame }))
    } else {
      pending.resolve(frame.result)
    }
  }

  request(method, params = {}) {
    this.nextId += 1
    const id = this.nextId
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject })
      this.child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id, method, params })}\n`)
    })
  }

  respond(id, result) {
    this.child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id, result })}\n`)
  }

  /** 等下一条满足条件的入站帧；游标只前进，重复等待会得到后续帧。 */
  async waitFor(predicate, timeoutMs = 5000) {
    const deadline = Date.now() + timeoutMs
    while (Date.now() < deadline) {
      while (this.cursor < this.frames.length) {
        const frame = this.frames[this.cursor]
        this.cursor += 1
        if (predicate(frame)) return frame
      }
      await new Promise((resolve) => setTimeout(resolve, 10))
    }
    throw new Error(`等待帧超时；已收到：${JSON.stringify(this.frames)}`)
  }

  nextRequest(method) {
    return this.waitFor((frame) => frame.method === method && frame.id !== undefined)
  }
}

function startKernel() {
  const child = spawn(process.execPath, [launcher], {
    env: { ...process.env, OMNICRAWL_BINARY: binary },
    stdio: ['pipe', 'pipe', 'pipe'],
  })
  const logs = []
  child.stderr.setEncoding('utf8')
  child.stderr.on('data', (chunk) => logs.push(chunk))
  return { child, client: new KernelClient(child), logs }
}

const isError = (code) => (error) => {
  assert.equal(error.frame?.error?.code, code, `期望错误码 ${code}，实际 ${error.frame?.error?.code}`)
  return true
}

test('完整回合：模型与工具批次经协议往返', { skip: SKIP }, async (t) => {
  const { child, client } = startKernel()
  t.after(() => child.kill())

  const initialized = await client.request('initialize', {
    protocol_version: '1.0',
    client: { name: 'e2e' },
  })
  assert.equal(initialized.protocol_version, '1.0')

  const submitted = client.request('turn.submit', { turn_id: 't1', user_text: '看看 a.py' })

  const first = await client.nextRequest('model.reply')
  assert.equal(first.params.turn_id, 't1')
  assert.deepEqual(first.params.messages, [{ role: 'user', content: '看看 a.py' }])
  client.respond(first.id, {
    message: { role: 'assistant', content: '', tool_calls: [toolCall] },
    content: '',
    tool_calls: [toolCall],
    reasoning: '',
    content_streamed: false,
  })

  const batch = await client.nextRequest('tool.batch')
  assert.equal(batch.params.turn_id, 't1')
  assert.equal(batch.params.step, 1)
  assert.deepEqual(batch.params.calls, [toolCall])
  client.respond(batch.id, { observations: [observation] })

  const second = await client.nextRequest('model.reply')
  assert.equal(second.params.messages.length, 3, '用户消息 + assistant 工具调用 + 工具观察')
  assert.equal(second.params.messages[2].role, 'tool')
  client.respond(second.id, {
    message: { role: 'assistant', content: '读完了' },
    content: '读完了',
    tool_calls: [],
    reasoning: '先看文件',
    content_streamed: false,
  })

  const finished = await client.waitFor((frame) => frame.method === 'turn.finished')
  assert.equal(finished.params.turn_id, 't1')
  assert.equal(finished.params.final_text, '读完了')
  assert.equal(finished.params.reasoning, '先看文件')
  assert.equal(finished.params.model_turns, 2)
  assert.equal(finished.params.tool_calls, 1)
  assert.equal(finished.params.paused, false)

  assert.deepEqual(await submitted, {}, 'turn.submit 只表示回合已结束，不重复结果')
})

test('回合内取消：立即中止该回合并回 -32003，随后仍能开新回合', { skip: SKIP }, async (t) => {
  const { child, client } = startKernel()
  t.after(() => child.kill())

  await client.request('initialize', { protocol_version: '1.0' })
  const submitted = client.request('turn.submit', { turn_id: 't2', user_text: '慢活儿' })
  await client.nextRequest('model.reply')

  assert.deepEqual(await client.request('turn.cancel', { turn_id: 't2' }), {})
  await assert.rejects(
    submitted,
    isError(-32003),
  )
  assert.equal(
    client.frames.find((frame) => frame.method === 'turn.finished'),
    undefined,
    '被取消的回合不应发 turn.finished',
  )

  const retry = client.request('turn.submit', { turn_id: 't3', user_text: '再来一次' })
  const modelReply = await client.nextRequest('model.reply')
  assert.equal(modelReply.params.turn_id, 't3')
  client.respond(modelReply.id, {
    message: { role: 'assistant', content: '好的' },
    content: '好的',
    tool_calls: [],
    reasoning: '',
    content_streamed: false,
  })
  await client.waitFor((frame) => frame.method === 'turn.finished')
  assert.deepEqual(await retry, {})
})

test('握手、错误码与 shutdown', { skip: SKIP }, async (t) => {
  const { child, client } = startKernel()
  t.after(() => child.kill())

  await assert.rejects(
    client.request('turn.submit', { turn_id: 'x', user_text: 'hi' }),
    isError(-32600),
    '未握手前其他请求回 -32600',
  )
  await assert.rejects(client.request('initialize', { protocol_version: '2.0' }), isError(-32001))
  assert.equal((await client.request('initialize', { protocol_version: '1.0' })).protocol_version, '1.0')
  await assert.rejects(client.request('does.not.exist'), isError(-32601))
  await assert.rejects(client.request('turn.submit', { turn_id: 'x' }), isError(-32602))

  assert.deepEqual(await client.request('shutdown'), {})
  const code = await new Promise((resolve) => child.on('exit', resolve))
  assert.equal(code, 0, 'shutdown 后内核应正常退出')
})

test('启动器转交参数：--version', { skip: SKIP }, async () => {
  const output = await new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [launcher, '--version'], {
      env: { ...process.env, OMNICRAWL_BINARY: binary },
    })
    let text = ''
    child.stdout.setEncoding('utf8')
    child.stdout.on('data', (chunk) => {
      text += chunk
    })
    child.on('error', reject)
    // 'exit' 可能先于 stdout 读完触发，用 'close' 才能拿到全部输出。
    child.on('close', () => resolve(text.trim()))
  })
  assert.match(output, /^omnicrawl \d+\.\d+\.\d+$/)
})
