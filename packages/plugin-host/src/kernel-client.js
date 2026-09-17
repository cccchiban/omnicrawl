/**
 * 内核客户端：起 `omnicrawl` 子进程，用协议 v1 的 NDJSON 与它对话。
 *
 * 只做传输——分帧、请求应答配对、请求与事件分流；回合语义在 `agent-host.js`。
 * 每个内核请求都必须有且只有一次响应：`reply.result` / `reply.error` 幂等；
 * 处理器抛错或什么都不做时由本模块补齐错误响应，避免内核干等。
 */

import { spawn } from 'node:child_process'

import {
  ERROR_CODE,
  isRequest,
  isResponse,
  METHOD,
  negotiateVersion,
  parseFrame,
  PROTOCOL_VERSION,
  requestFrame,
  responseFrame,
  errorResponseFrame,
} from '@omnicrawl/plugin-sdk'

export function createKernelClient({ binary, args = [], env = {}, cwd, logger = console } = {}) {
  if (!binary) throw new Error('必须给出内核二进制路径。')

  const child = spawn(binary, args, {
    cwd,
    env: { ...process.env, ...env },
    stdio: ['pipe', 'pipe', 'pipe'],
  })

  const pending = new Map()
  const requestHandlers = []
  const eventHandlers = []
  let nextId = 0
  let buffer = ''
  let closed = false

  child.stdout.setEncoding('utf8')
  child.stdout.on('data', (chunk) => {
    buffer += chunk
    let index = buffer.indexOf('\n')
    while (index >= 0) {
      const line = buffer.slice(0, index)
      buffer = buffer.slice(index + 1)
      receive(line)
      index = buffer.indexOf('\n')
    }
  })
  child.stderr.setEncoding('utf8')
  child.stderr.on('data', (chunk) => logger?.info?.(`[kernel] ${String(chunk).trimEnd()}`))

  function send(frame) {
    if (closed) throw new Error('内核连接已关闭。')
    child.stdin.write(`${JSON.stringify(frame)}\n`)
  }

  function receive(line) {
    let frame
    try {
      frame = parseFrame(line)
    } catch (error) {
      logger?.warn?.(`[kernel] 丢弃无法解析的行：${error.message}`)
      return
    }
    if (frame === null) return

    if (isResponse(frame)) {
      const entry = pending.get(frame.id)
      if (!entry) {
        logger?.warn?.(`[kernel] 忽略不在等待的响应：${JSON.stringify(frame.id)}`)
        return
      }
      pending.delete(frame.id)
      if (frame.error) entry.reject(Object.assign(new Error(frame.error.message), { frame }))
      else entry.resolve(frame.result)
      return
    }

    if (isRequest(frame)) {
      void dispatchRequest(frame)
      return
    }

    for (const handler of eventHandlers) handler(frame)
  }

  async function dispatchRequest(frame) {
    let answered = false
    const reply = {
      result(value) {
        if (answered) return
        answered = true
        send(responseFrame(frame.id, value))
      },
      error(code, message, data) {
        if (answered) return
        answered = true
        send(errorResponseFrame(frame.id, code, message, data))
      },
    }

    try {
      if (!requestHandlers.length) {
        reply.error(ERROR_CODE.METHOD_NOT_FOUND, `宿主没有注册 ${frame.method} 的处理器。`)
        return
      }
      for (const handler of requestHandlers) await handler(frame, reply)
    } catch (error) {
      reply.error(ERROR_CODE.INTERNAL_ERROR, `处理 ${frame.method} 失败：${error.message}`)
      return
    }
    if (!answered) reply.error(ERROR_CODE.INTERNAL_ERROR, `宿主没有响应 ${frame.method}。`)
  }

  return {
    child,

    onRequest(handler) {
      requestHandlers.push(handler)
    },

    onEvent(handler) {
      eventHandlers.push(handler)
    },

    request(method, params = {}) {
      nextId += 1
      const id = nextId
      return new Promise((resolve, reject) => {
        pending.set(id, { resolve, reject })
        try {
          send(requestFrame(id, method, params))
        } catch (error) {
          pending.delete(id)
          reject(error)
        }
      })
    },

    /** 握手：声明版本并校验内核返回的版本。 */
    async initialize({ name = '@omnicrawl/plugin-host', version } = {}) {
      const result = await this.request(METHOD.INITIALIZE, {
        protocol_version: PROTOCOL_VERSION,
        client: { name, ...(version ? { version } : {}) },
      })
      negotiateVersion(result?.protocol_version)
      return result
    },

    /** 请求取消当前回合；内核在下一个边界收尾。 */
    cancelTurn(turnId) {
      return this.request(METHOD.TURN_CANCEL, { turn_id: turnId })
    },

    /** 要求内核退出并等它结束，返回退出码。 */
    async shutdown() {
      const exited = new Promise((resolve) => child.once('exit', (code) => resolve(code ?? 0)))
      try {
        await this.request(METHOD.SHUTDOWN, {})
      } catch {
        // 内核可能先关连接再退出，这里以进64,出为准。
      }
      closed = true
      return exited
    },

    close() {
      if (closed) return
      closed = true
      child.stdin.end()
      child.kill()
    },
  }
}
