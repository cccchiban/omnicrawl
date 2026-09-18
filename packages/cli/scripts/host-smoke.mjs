// 宿主载荷冒烟：在无终端环境下，冻结宿主必须给出可执行的提示，而不是崩溃或静默退出。
//
// 用法：node packages/cli/scripts/host-smoke.mjs（需要先跑 build-host.mjs）
import { spawnSync } from 'node:child_process'
import { existsSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import process from 'node:process'

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '../../..')
const platformKey = `${process.platform}-${process.arch}`
const entry = join(
  repoRoot,
  'dist',
  'host',
  platformKey,
  'payload',
  process.platform === 'win32' ? 'omnicrawl-host.exe' : 'omnicrawl-host',
)

if (!existsSync(entry)) {
  console.error(`找不到宿主载荷入口：${entry}\n先跑 node packages/cli/scripts/build-host.mjs`)
  process.exit(1)
}

// 管道让 stdin/stdout 都不是终端：正是服务器与 CI 的形态。
const result = spawnSync(entry, [], { encoding: 'utf8', input: '' })
const stderr = result.stderr ?? ''

if (!stderr.includes('没有交互式终端')) {
  console.error(`宿主没有给出无终端提示（退出码 ${result.status}）：\n${stderr}`)
  process.exit(1)
}
if (result.status !== 2) {
  console.error(`无终端时的退出码应为 2，实际为 ${result.status}`)
  process.exit(1)
}

console.log(`宿主冒烟通过（${platformKey}）：无终端提示与退出码符合预期。`)
