#!/usr/bin/env node
/**
 * Cordis 宿主入口：加载插件、按需派发一次钩子演示、收到 SIGINT 后干净卸载。
 *
 * 用法：
 *   node packages/plugin-host/src/main.js --plugin ./samples/hello.js
 *   node packages/plugin-host/src/main.js --plugin ./samples/hello.js --hook turn.start
 *   node packages/plugin-host/src/main.js --plugin ./samples/hello.js \
 *     --kernel rust/target/release/omnicrawl --model ./my-model.js --turn "你好"
 *
 * 给了 `--kernel` 就进入协议 v1 模式：起内核子进程、握手、跑一个回合、退出。
 * `--model` 是宿主侧的模型客户端模块，默认导出 `(request) => reply`（过渡期模型由宿主代答）。
 */

import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'

import { createAgentHost } from './agent-host.js'
import { createHost } from './host.js'
import { createKernelClient } from './kernel-client.js'

function parseArgs(argv) {
  const options = {
    plugins: [],
    hook: null,
    mode: null,
    payload: '{}',
    kernel: null,
    turn: null,
    model: null,
  }
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index]
    if (arg === '--plugin') options.plugins.push(argv[++index])
    else if (arg === '--hook') options.hook = argv[++index]
    else if (arg === '--mode') options.mode = argv[++index]
    else if (arg === '--payload') options.payload = argv[++index]
    else if (arg === '--kernel') options.kernel = argv[++index]
    else if (arg === '--turn') options.turn = argv[++index]
    else if (arg === '--model') options.model = argv[++index]
    else throw new Error(`未知参数：${arg}`)
  }
  return options
}

async function importPlugin(specifier) {
  if (specifier.startsWith('.') || specifier.startsWith('/')) {
    return import(pathToFileURL(resolve(specifier)).href)
  }
  return import(specifier)
}

async function main() {
  const options = parseArgs(process.argv.slice(2))
  if (!options.plugins.length) throw new Error('至少要有一个 --plugin。')

  const host = createHost()
  const load = async (specifier) => {
    const module = await importPlugin(specifier)
    const fiber = await host.load(module.default ?? module)
    console.log(`[host] 已加载插件 ${module.name ?? fiber.name}`)
    return fiber
  }

  for (const specifier of options.plugins) await load(specifier)

  if (options.hook) {
    const mode = options.mode ?? 'observe'
    const result = await host.dispatch(options.hook, mode, JSON.parse(options.payload))
    console.log(`[host] ${options.hook}(${mode}) 返回：${JSON.stringify(result)}`)
  }

  if (options.kernel || options.turn || options.model) {
    if (!(options.kernel && options.turn && options.model)) {
      throw new Error('--kernel、--turn、--model 必须同时给出。')
    }

    let finished = null
    const kernel = createKernelClient({ binary: resolve(options.kernel) })
    const bridge = createAgentHost({
      host,
      kernel,
      model: (await importPlugin(options.model)).default,
      onEvent: (frame) => {
        if (frame.method === 'turn.finished') finished = frame.params
      },
    })

    await kernel.initialize({ name: '@omnicrawl/plugin-host' })
    const { turnId } = await bridge.runTurn(options.turn)
    console.log(`[host] 回合 ${turnId} 结束：${finished?.final_text ?? ''}`)
    await kernel.shutdown()
    await host.dispose()
    return
  }

  const shutdown = async () => {
    await host.dispose()
    console.log('[host] 已卸载全部插件')
    process.exit(0)
  }
  process.on('SIGINT', shutdown)
  process.on('SIGTERM', shutdown)
  console.log('[host] 就绪，Ctrl+C 退出。')
}

main().catch((error) => {
  console.error(`[host] 启动失败：${error.message}`)
  process.exit(1)
})
