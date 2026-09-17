/**
 * 宿主桥接协议 v1 的 JS 侧：方法名、错误码、帧构造与解析。
 *
 * 语义以 `rust/docs/protocol-v1.md` 与 `rust/crates/omnicrawl-ipc/src/bridge.rs` 为准，
 * 这里只是宿主侧的名字与帧形状，不引入第二份语义。改这里必须同步改 Rust 侧那两处。
 */

export const PROTOCOL_VERSION = '1.0'
export const JSONRPC_VERSION = '2.0'
export const SUPPORTED_MAJOR = 1

/** 协议 v1 的错误码；前五个沿用 JSON-RPC 2.0 标准码。 */
export const ERROR_CODE = Object.freeze({
  PARSE_ERROR: -32700,
  INVALID_REQUEST: -32600,
  METHOD_NOT_FOUND: -32601,
  INVALID_PARAMS: -32602,
  INTERNAL_ERROR: -32603,
  UNSUPPORTED_PROTOCOL_VERSION: -32001,
  TURN_BUSY: -32002,
  TURN_CANCELLED: -32003,
  TURN_FAILED: -32004,
})

export const METHOD = Object.freeze({
  // 宿主 → 内核
  INITIALIZE: 'initialize',
  TURN_SUBMIT: 'turn.submit',
  TURN_CANCEL: 'turn.cancel',
  SHUTDOWN: 'shutdown',

  // 内核 → 宿主（请求，需要响应）
  TOOL_BATCH: 'tool.batch',
  MODEL_REPLY: 'model.reply',

  // 内核 → 宿主（notification，不需要响应）
  TURN_DELTA: 'turn.delta',
  TURN_REASONING_DELTA: 'turn.reasoning_delta',
  TURN_STATUS: 'turn.status',
  TURN_RETRY_STATUS: 'turn.retry_status',
  TURN_PROTOCOL_WAIT: 'turn.protocol_wait',
  TURN_STREAM_ROLLBACK: 'turn.stream_rollback',
  TURN_TOKEN_USAGE: 'turn.token_usage',
  TURN_FINISHED: 'turn.finished',
  TOOL_STARTED: 'tool.started',
  TOOL_FINISHED: 'tool.finished',
  TOOL_OUTPUT_UPDATE: 'tool.output_update',
  SUBAGENT_EVENT: 'subagent.event',
  TODO_UPDATE: 'todo.update',
})

/** 需要宿主回响应的内核请求方法。 */
export const KERNEL_REQUEST_METHODS = Object.freeze([METHOD.TOOL_BATCH, METHOD.MODEL_REPLY])

export function requestFrame(id, method, params = {}) {
  return { jsonrpc: JSONRPC_VERSION, id, method, params }
}

export function notificationFrame(method, params = {}) {
  return { jsonrpc: JSONRPC_VERSION, method, params }
}

export function responseFrame(id, result) {
  return { jsonrpc: JSONRPC_VERSION, id, result }
}

export function errorResponseFrame(id, code, message, data) {
  const error = { code, message }
  if (data !== undefined) error.data = data
  return { jsonrpc: JSONRPC_VERSION, id, error }
}

export function isRequest(frame) {
  return typeof frame.method === 'string' && frame.id !== undefined
}

export function isNotification(frame) {
  return typeof frame.method === 'string' && frame.id === undefined
}

export function isResponse(frame) {
  return frame.method === undefined && frame.id !== undefined
}

/** 解析一行 NDJSON；空行返回 null，形状非法直接抛错（调用方决定丢弃还是上报）。 */
export function parseFrame(line) {
  const text = String(line ?? '').trim()
  if (!text) return null
  const frame = JSON.parse(text)
  if (frame === null || typeof frame !== 'object') throw new Error('帧必须是 JSON 对象')
  if (frame.jsonrpc !== JSONRPC_VERSION) throw new Error('jsonrpc 字段必须是 "2.0"')
  if (isRequest(frame) || isNotification(frame)) {
    if (frame.result !== undefined || frame.error !== undefined) {
      throw new Error('请求与notification不得携带 result 或 error')
    }
    return frame
  }
  if (frame.id === undefined) throw new Error('响应缺少 id')
  return frame
}

/** 主版本协商，规则与内核 `version.rs` 一致：只比主号，`1` / `1.0` / `1.7.3` 等价。 */
export function negotiateVersion(hostVersion) {
  const text = String(hostVersion ?? '').trim()
  const major = text.split('.')[0]
  if (!/^\d+$/.test(major)) throw new Error(`非法协议版本：${hostVersion}`)
  if (Number(major) !== SUPPORTED_MAJOR) {
    throw new Error(`不支持宿主协议版本 ${hostVersion}（本实现支持 ${PROTOCOL_VERSION}）`)
  }
  return PROTOCOL_VERSION
}
