// 启动自动更新：检测 npm registry 上的 `omnicrawl-cli` 最新版，落后时全局升级并重启。
//
// 版本源是 npm registry 的 `latest`，升级方式是 `npm install -g`。
//
// 流程（仅默认工作台路径，非逃生口）：
//
// 1. 读取 `[update].enabled`（缺省启用），并检查环境变量跳过开关；
// 2. 读 24h 本地缓存，过期或缺失才请求 registry（3s 超时）；
// 3. 本地版本落后时打印升级说明，执行 `npm install -g omnicrawl-cli@<latest>`；
// 4. 升级成功后重新拉起启动器（`node <launcher> <原参数>`），把子进程退出码作为本次退出码；
// 5. 任何失败（离线、npm 不可用、安装报错）都只打印原因与手动升级命令，继续用当前版本启动，
//    绝不让更新流程阻塞工具可用性。
//
// 安全护栏：
// - 源码检出 / 开发环境（祖先目录里出现 `.git`）直接跳过；
// - `OMNICRAWL_SKIP_AUTO_UPDATE=1` 可彻底关闭；
// - 逃生口 `OMNICRAWL_HOST` / `OMNICRAWL_BINARY`（协议测试与开发）不走更新检查；
// - 执行过一次升级尝试后写入 `OMNICRAWL_AUTO_UPDATE_ATTEMPTED=1`，重启后的子进程继承该标记
//   不再重复尝试，避免升级失败/版本探测异常时无限重启循环。
import { spawn } from 'node:child_process'
import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import process from 'node:process'

export const PACKAGE_NAME = 'omnicrawl-cli'
/** 彻底关闭自动更新（用户 / CI / 故障排查用）。 */
export const SKIP_AUTO_UPDATE_ENV = 'OMNICRAWL_SKIP_AUTO_UPDATE'
/** 标记「本次升级已经尝试过」，重启后的子进程据此不再重复尝试。 */
export const AUTO_UPDATE_ATTEMPTED_ENV = 'OMNICRAWL_AUTO_UPDATE_ATTEMPTED'
export const REGISTRY_URL = `https://registry.npmjs.org/${PACKAGE_NAME}/latest`
export const VERSION_CHECK_TIMEOUT_MS = 3000
export const VERSION_CHECK_CACHE_TTL_SECONDS = 24 * 60 * 60
export const VERSION_CHECK_CACHE_FILENAME = 'npm-version-check.json'
export const USER_CONFIG_DIRNAME = '.OmniCrawl'
export const CONFIG_FILENAME = 'config.toml'

/** 已识别的子命令：这些路径不做启动更新（工作台才是自动更新的入口）。 */
const DISPATCH_COMMANDS = new Set(['api', 'kernel', 'plugin'])
/** `npm install -g` 的附加参数（减少噪音与交互）。 */
const NPM_QUIET_ARGS = ['--no-fund', '--no-audit']

const VERSION_PATTERN =
  /^v?(?<release>\d+(?:\.\d+)*)(?:-(?<pre>[0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$/

/** 解析常见 semver（`1.2.3`、`1.2.3-rc.1`、`v1.2.3+build`）；非法值返回 null。 */
export function parseVersion(value) {
  const text = String(value ?? '').trim()
  const match = VERSION_PATTERN.exec(text)
  if (!match) return null
  const release = match.groups.release.split('.').map((part) => Number.parseInt(part, 10))
  if (release.some((part) => !Number.isFinite(part))) return null
  const pre = match.groups.pre
  return { release, pre: pre ? pre.split('.') : null }
}

/** semver 预发布标识比较：数字段按数值，字母段按字典序，数字段低于字母段。 */
function comparePre(a, b) {
  if (a === null && b === null) return 0
  if (a === null) return 1 // 无预发布高于有预发布
  if (b === null) return -1
  const width = Math.max(a.length, b.length)
  for (let index = 0; index < width; index += 1) {
    const left = a[index]
    const right = b[index]
    if (left === undefined) return -1
    if (right === undefined) return 1
    const leftNumber = /^\d+$/.test(left) ? Number.parseInt(left, 10) : null
    const rightNumber = /^\d+$/.test(right) ? Number.parseInt(right, 10) : null
    if (leftNumber !== null && rightNumber !== null) {
      if (leftNumber !== rightNumber) return leftNumber < rightNumber ? -1 : 1
    } else if (leftNumber !== null) {
      return -1
    } else if (rightNumber !== null) {
      return 1
    } else if (left !== right) {
      return left < right ? -1 : 1
    }
  }
  return 0
}

/** 比较版本；任一侧非法（异常值）都按「不可升级」处理（Python `is_newer_version` 同口径）。 */
export function isNewerVersion(candidate, current) {
  const candidateKey = parseVersion(candidate)
  const currentKey = parseVersion(current)
  if (!candidateKey || !currentKey) return false

  const width = Math.max(candidateKey.release.length, currentKey.release.length)
  for (let index = 0; index < width; index += 1) {
    const left = candidateKey.release[index] ?? 0
    const right = currentKey.release[index] ?? 0
    if (left !== right) return left > right
  }
  return comparePre(candidateKey.pre, currentKey.pre) > 0
}

/** 从 registry 的 `latest` 响应体解析版本号；解析失败抛错（调用方降级）。 */
export function parseRegistryVersion(payload) {
  const body = typeof payload === 'string' ? JSON.parse(payload) : payload
  const version = String(body?.version ?? '').trim()
  if (!version || !parseVersion(version)) {
    throw new Error('npm registry 的 latest 响应里没有有效版本。')
  }
  return version
}

/** 默认取版本方式：直连 npm registry，带 3s 超时，失败抛错。 */
export async function fetchRegistryLatest(url = REGISTRY_URL) {
  const response = await fetch(url, {
    headers: { Accept: 'application/json', 'User-Agent': `${PACKAGE_NAME} version-check` },
    signal: AbortSignal.timeout(VERSION_CHECK_TIMEOUT_MS),
  })
  if (!response.ok) throw new Error(`npm registry 返回 ${response.status}`)
  return parseRegistryVersion(await response.json())
}

function cachePath(home) {
  return join(home, USER_CONFIG_DIRNAME, VERSION_CHECK_CACHE_FILENAME)
}

function readFreshCache(home, now) {
  try {
    const data = JSON.parse(readFileSync(cachePath(home), 'utf8'))
    const checkedAt = Number(data.checked_at)
    const latest = String(data.latest_version ?? '').trim()
    const age = Math.max(0, now / 1000 - checkedAt)
    if (age < VERSION_CHECK_CACHE_TTL_SECONDS && parseVersion(latest)) return latest
  } catch {
    return null
  }
  return null
}

function writeCache(home, now, latestVersion) {
  const target = cachePath(home)
  const temp = `${target}.${process.pid}.tmp`
  try {
    mkdirSync(dirname(target), { recursive: true })
    writeFileSync(
      temp,
      JSON.stringify({ checked_at: now / 1000, latest_version: latestVersion }),
      'utf8',
    )
    renameSync(temp, target)
  } catch {
    // 缓存失败不应抹掉已经取得的在线检查结果。
  } finally {
    rmSync(temp, { force: true })
  }
}

/** 读取 `[update].enabled`；缺省开启，配置异常也按开启处理（保底默认）。 */
export function loadUpdateEnabled(home = homedir()) {
  try {
    const text = readFileSync(join(home, USER_CONFIG_DIRNAME, CONFIG_FILENAME), 'utf8')
    const section = text.match(/(?:^|\n)\[update\]\s*\n(?<body>[\s\S]*?)(?:\n\[|$)/)
    if (!section) return true
    const match = section.groups.body.match(/^\s*enabled\s*=\s*(true|false)\s*$/m)
    if (!match) return true
    return match[1] === 'true'
  } catch {
    return true // 更新功能不能因配置问题影响启动
  }
}

/**
 * 判断是否从源码检出 / 开发环境运行（此类环境不自动更新）。
 *
 * npm 全局安装的启动器会落在 `node_modules/@omnicrawl/cli/bin` 这类目录里，祖先里没有 `.git`；
 * 仓库内开发时祖先里有 `.git`。
 */
export function isSourceCheckout(launcherPath, maxDepth = 8) {
  let current = dirname(resolve(launcherPath))
  for (let depth = 0; depth < maxDepth; depth += 1) {
    if (existsSync(join(current, '.git'))) return true
    const parent = dirname(current)
    if (parent === current) break
    current = parent
  }
  return false
}

/** 是否需要为这组参数做启动更新：只认工作台路径，已知子命令与逃生口都跳过。 */
export function shouldCheckUpdate(args, env = process.env) {
  if (env.OMNICRAWL_HOST || env.OMNICRAWL_BINARY) return false
  const first = args[0]
  if (first !== undefined && DISPATCH_COMMANDS.has(first)) return false
  return true
}

function buildInstallCommand(latestVersion) {
  const command = process.env.npm_execpath
  return command
    ? [process.execPath, command, 'install', '-g', ...NPM_QUIET_ARGS, `${PACKAGE_NAME}@${latestVersion}`]
    : ['npm', 'install', '-g', ...NPM_QUIET_ARGS, `${PACKAGE_NAME}@${latestVersion}`]
}

function runDefault(command) {
  const child = spawn(command[0], command.slice(1), { stdio: 'inherit', shell: false })
  return new Promise((resolveExit) => {
    child.on('error', () => resolveExit(1))
    child.on('exit', (code) => resolveExit(code ?? 1))
  })
}

function relaunchDefault(launcherPath, args) {
  const child = spawn(process.execPath, [launcherPath, ...args], { stdio: 'inherit' })
  return new Promise((resolveExit) => {
    child.on('error', () => resolveExit(null))
    child.on('exit', (code) => resolveExit(code ?? 0))
  })
}

function manualInstallHint(latestVersion) {
  return `可手动执行升级：npm install -g ${PACKAGE_NAME}@${latestVersion}`
}

/**
 * 启动阶段自动更新入口。
 *
 * @returns `null`：无需更新 / 已跳过 / 更新失败按用户策略继续用当前版本启动；
 *          其他整数：已升级并重启，返回新进程的退出码，调用方应直接退出。
 */
export async function runStartupUpdateIfDue({
  argv = [],
  currentVersion,
  launcherPath = new URL(import.meta.url).pathname,
  env = process.env,
  home = homedir(),
  print = (line) => console.error(line),
  now = Date.now,
  latestChecker = null,
  installer = null,
  relauncher = null,
  sourceCheckout = null,
  configEnabled = null,
} = {}) {
  if (env[SKIP_AUTO_UPDATE_ENV] === '1') return null
  if (env[AUTO_UPDATE_ATTEMPTED_ENV] === '1') return null
  if (env.OMNICRAWL_HOST || env.OMNICRAWL_BINARY) return null
  if ((sourceCheckout ?? isSourceCheckout)(launcherPath)) return null
  if (!(configEnabled ?? loadUpdateEnabled)(home)) return null

  const installed = String(currentVersion ?? '').trim()
  let latest = null
  try {
    latest = readFreshCache(home, now())
    if (latest === null) {
      latest = latestChecker
        ? await latestChecker()
        : await fetchRegistryLatest()
      if (latest) writeCache(home, now(), latest)
    }
  } catch {
    // 版本检查不能改变应用可用性；离线、代理和缓存损坏均只隐藏更新提示。
    return null
  }
  if (!latest || !isNewerVersion(latest, installed)) return null

  // 先标记尝试，避免子进程（继承环境变量）在升级后因探测不一致再次升级。
  env[AUTO_UPDATE_ATTEMPTED_ENV] = '1'
  print(`发现新版本 ${latest}（当前 ${installed}），正在自动升级…`)

  let installedOk = false
  try {
    installedOk = (await (installer
      ? installer(latest)
      : runDefault(buildInstallCommand(latest)))) === 0
  } catch {
    installedOk = false
  }
  if (!installedOk) {
    print(`自动升级失败，本次继续使用当前版本 ${installed} 启动。`)
    print(manualInstallHint(latest))
    return null
  }

  print(`升级完成（${installed} → ${latest}），正在重新启动工作台…`)
  const code = await (relauncher
    ? relauncher(launcherPath, argv)
    : relaunchDefault(launcherPath, argv))
  if (code === null) {
    print('重新启动工作台失败，请手动执行 omnicrawl 重新进入。')
    return 0
  }
  return code
}
