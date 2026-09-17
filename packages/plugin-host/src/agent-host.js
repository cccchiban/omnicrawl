/**
 * 协议 v1 的宿主对端：内核的 `tool.batch` 与 `model.reply` 由这里应答，
 * 回合与工具阶段派发成 Cordis 插件钩子。
 *
 * 载荷键名与 Python 侧一致（`tool` / `arguments` / `displayText` / `annotations`），
 * 这样同一批插件在两个宿主上读到的是同一套字段。`tool.execute.error` 只在工具抛异常时触发，
 * 与 Python 侧一致；插件拒绝与未注册工具只记日志并回成观察。
 */

import { ERROR_CODE, METHOD } from '@omnicrawl/plugin-sdk'

export function createAgentHost({ host, kernel, model, tools = {}, onEvent, logger = console }) {
  if (typeof model !== 'function') {
    throw new Error('必须给出 model 函数：内核用 model.reply 把模型请求交给宿主。')
  }

  const registry = new Map(Object.entries(tools))
  let turnCounter = 0

  kernel.onRequest((frame, reply) => handleRequest(frame, reply))
  kernel.onEvent((frame) => handleKernelEvent(frame))

  async function handleRequest(frame, reply) {
    if (frame.method === METHOD.MODEL_REPLY) {
      reply.result(await replyModel(frame.params))
      return
    }
    if (frame.method === METHOD.TOOL_BATCH) {
      reply.result({ observations: await runBatch(frame.params) })
      return
    }
    reply.error(ERROR_CODE.METHOD_NOT_FOUND, `未知的内核请求 ${frame.method}。`)
  }

  /** 模型请求：先过 `model.request.before`（transform 可改 messages 与采样参数），再交给宿主的模型客户端。 */
  async function replyModel(params) {
    const request = await host.dispatch('model.request.before', 'transform', {
      messages: params.messages,
    })
    const reply = await model(request)
    await host.dispatch('model.response.after', 'observe', { reply })
    return reply
  }

  async function runBatch(params) {
    const observations = []
    for (const call of params.calls) {
      observations.push(await runCall(call, params))
    }
    return observations
  }

  async function runCall(call, params) {
    const call_payload = await host.dispatch('tool.call.before', 'transform', {
      tool: call.name,
      arguments: { ...call.arguments },
      step: params.step,
      turnId: params.turn_id,
    })
    const decision = await host.dispatch('tool.execute.before', 'guard', call_payload)

    if (decision?.allowed === false) {
      const reason = decision.reason ?? '未给出原因'
      logger?.warn?.(`[plugin] 工具 ${call.name} 被拒绝：${reason}`)
      return observation(call, failed('denied'), `工具 ${call.name} 被插件拒绝：${reason}`)
    }

    const handler = registry.get(call.name)
    if (!handler) {
      logger?.warn?.(`[plugin] 工具 ${call.name} 没有注册处理器`)
      return observation(call, failed('unknown_tool'), `未注册的工具 ${call.name}。`)
    }

    try {
      const outcome = (await handler(call_payload.arguments ?? {})) ?? {}
      const result = {
        ok: outcome.ok ?? true,
        output: outcome.output ?? '',
        full_output: outcome.fullOutput ?? '',
        error_code: outcome.errorCode ?? null,
        retryable: outcome.retryable ?? false,
      }
      // `tool.execute.after` 的 patch 指向界面字段；还没有界面层，改写结果先丢弃。
      await host.dispatch('tool.execute.after', 'transform', {
        tool: call.name,
        ok: result.ok,
        displayText: result.full_output || result.output,
        annotations: {},
      })
      return observation(call, result, result.output)
    } catch (error) {
      const result = failed('execution_failed')
      await host.dispatch('tool.execute.error', 'notify', { tool: call.name, error: error.message })
      return observation(call, result, `工具执行失败：${error.message}`)
    }
  }

  function failed(error_code) {
    return { ok: false, output: '', full_output: '', error_code, retryable: false }
  }

  function observation(call, result, content) {
    return {
      tool_call: call,
      result,
      message: { role: 'tool', tool_call_id: call.id, content },
      followup_messages: [],
    }
  }

  function handleKernelEvent(frame) {
    onEvent?.(frame)
    if (frame.method === METHOD.TURN_FINISHED) {
      // notify 模式不阻塞回合收尾，这里不 await。
      void host.dispatch('turn.end', 'notify', { turnId: frame.params?.turn_id, ...frame.params })
      return
    }
    if (frame.method === METHOD.TURN_STATUS) {
      logger?.info?.(`[turn] ${frame.params?.message ?? ''}`)
    }
  }

  async function runTurn(userText, { turnId, tags } = {}) {
    turnCounter += 1
    const id = turnId ?? `turn-${turnCounter}`
    const start = await host.dispatch('turn.start', 'transform', {
      userText,
      ...(tags ? { tags } : {}),
    })

    try {
      await kernel.request(METHOD.TURN_SUBMIT, {
        turn_id: id,
        user_text: start?.userText ?? userText,
      })
      return { turnId: id }
    } catch (error) {
      const code = error.frame?.error?.code
      if (code === ERROR_CODE.TURN_CANCELLED) {
        await host.dispatch('turn.cancelled', 'notify', { turnId: id })
      } else {
        await host.dispatch('turn.error', 'notify', { turnId: id, code, message: error.message })
      }
      throw error
    }
  }

  return {
    runTurn,
    cancelTurn: (turnId) => kernel.cancelTurn(turnId),
    tools: registry,
    registerTool(name, handler) {
      registry.set(name, handler)
      return () => registry.delete(name)
    },
  }
}
