#!/usr/bin/env node
// OmniCrawl 启动器：按 platform/arch 选出平台分包，默认启动完整宿主（工作台 / api / plugin 指令），
// `kernel` 子命令把 stdio 交给内核二进制（协议调试与 e2e）。
//
// 两个逃生口（开发与协议测试用）：
//   OMNICRAWL_HOST=<可执行文件>   直接指定宿主，跳过平台分包
//   OMNICRAWL_BINARY=<可执行文件> 直接指定内核；此时启动器退化为纯透传，便于协议测试
import { spawn } from 'node:child_process'
import { existsSync, readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import process from 'node:process'

const PLATFORM_PACKAGES = {
  'linux-x64': '@omnicrawl/cli-linux-x64',
  'linux-arm': '@omnicrawl/cli-linux-arm-musl',
  'linux-arm64': '@omnicrawl/cli-linux-arm64-musl',
  'win32-x64': '@omnicrawl/cli-win32-x64',
  'win32-ia32': '@omnicrawl/cli-win32-ia32',
}

const IS_WINDOWS = process.platform === 'win32'
const HOST_FILE = IS_WINDOWS ? 'omnicrawl-host.exe' : 'omnicrawl-host'
const KERNEL_FILE = IS_WINDOWS ? 'omnicrawl.exe' : 'omnicrawl'
const KERNEL_COMMAND = 'kernel'

const launcherVersion = JSON.parse(
  readFileSync(new URL('../package.json', import.meta.url), 'utf8'),
).version

function fail(message) {
  console.error(message)
  process.exit(1)
}

/** 解析平台分包根目录；拿不到分包时抛出可直接展示的错误。 */
function resolvePlatform() {
  const key = `${process.platform}-${process.arch}`
  const packageName = PLATFORM_PACKAGES[key]
  if (!packageName) {
    throw new Error(`不支持的平台 ${key}；已支持：${Object.keys(PLATFORM_PACKAGES).join('、')}`)
  }
  const require = createRequire(import.meta.url)
  return { packageName, root: dirname(require.resolve(`${packageName}/package.json`)) }
}

/** 目标可执行文件：宿主优先，内核兜底；两个环境变量各自覆盖。 */
function resolveTargets() {
  if (process.env.OMNICRAWL_HOST) {
    return { host: process.env.OMNICRAWL_HOST, kernel: process.env.OMNICRAWL_BINARY ?? null, passthrough: false }
  }
  if (process.env.OMNICRAWL_BINARY) {
    return { host: null, kernel: process.env.OMNICRAWL_BINARY, passthrough: true }
  }
  const { packageName, root } = resolvePlatform()
  return {
    host: join(root, 'host', HOST_FILE),
    kernel: join(root, 'bin', KERNEL_FILE),
    packageName,
    root,
    passthrough: false,
  }
}

function readPayloadMeta(root) {
  if (!root) return null
  try {
    return JSON.parse(readFileSync(join(root, 'version.json'), 'utf8'))
  } catch {
    return null
  }
}

function printVersion(targets) {
  const meta = readPayloadMeta(targets.root)
  const lines = [`omnicrawl ${launcherVersion}`]
  lines.push(
    targets.packageName
      ? `${targets.packageName} ${meta?.version ?? '未知'}（${process.platform}-${process.arch} 平台包）`
      : `omnicrawl-cli ${launcherVersion}（启动器）`,
  )
  if (meta?.hostVersion) lines.push(`omnicrawl-agent ${meta.hostVersion}（宿主）`)
  if (meta?.kernelVersion) lines.push(`omnicrawl ${meta.kernelVersion}（内核）`)
  console.log(lines.join('\n'))
}

function printHelp() {
  console.log(
    `用法：omnicrawl [选项] [指令]\n\n\
  无参数        启动工作台（TUI）\n\
  api           启动本地 HTTP 服务（无头/服务器场景）\n\
  plugin ...    插件管理\n\
  kernel ...    直连内核进程（协议调试）\n\n\
  --resume <会话 ID>   启动时恢复指定会话\n\
  --version, -V        打印启动器、平台包、宿主与内核版本\n\
  --help, -h           打印本说明`,
  )
}

/** 把 stdio 交给目标可执行文件，并把它的退出码/信号作为本次退出状态。 */
function run(executable, args) {
  if (!executable || !existsSync(executable)) {
    fail(`omnicrawl：找不到可执行文件 ${executable ?? '(未解析到)'}。请重装对应的平台包。`)
  }
  const child = spawn(executable, args, { stdio: 'inherit' })

  child.on('error', (error) => {
    console.error(`omnicrawl：无法启动 ${executable}：${error.message}`)
    process.exit(1)
  })

  child.on('exit', (code, signal) => {
    if (signal) {
      console.error(`omnicrawl：${executable} 被信号 ${signal} 终止`)
      process.exit(1)
    }
    process.exit(code ?? 0)
  })
}

function main() {
  let targets
  try {
    targets = resolveTargets()
  } catch (error) {
    fail(`omnicrawl：${error.message}`)
  }

  const args = process.argv.slice(2)

  // 内核透传模式（OMNICRAWL_BINARY）：启动器不介入 stdio 上的协议帧。
  if (targets.passthrough) return run(targets.kernel, args)

  if (args[0] === KERNEL_COMMAND) return run(targets.kernel, args.slice(1))
  if (args.includes('--version') || args.includes('-V')) return printVersion(targets)
  if (args.includes('--help') || args.includes('-h')) return printHelp()

  if (targets.host && existsSync(targets.host)) return run(targets.host, args)

  // 嵌入式目标（armv7 等）没有宿主载荷：退回内核直连，并说明这一平台的边界。
  console.error(
    `omnicrawl：平台包 ${targets.packageName ?? '(未指定)'} 没有宿主载荷，退回内核直连；` +
      '该平台只支持协议模式（用 omnicrawl kernel 显式进入）。',
  )
  return run(targets.kernel, args)
}

main()
