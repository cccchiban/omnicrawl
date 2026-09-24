// 构建宿主载荷：默认打包 **Rust 宿主**（cargo release 产物 + 模板资源）。
//
// 产物：dist/host/<platform>/payload/（宿主目录内容）+ host-meta.json（版本信息）。
// payload 的内容会被 prepare.mjs 直接搬进平台包的 `host/`，因此 payload 里放的就是
// 「宿主二进制 + 它依赖的同目录二进制 + 可编辑的模板资源」：
//
//   payload/omnicrawl-host[.exe]   统一入口（默认路由到 TUI，另有 api / plugin / kernel）
//   payload/omnicrawl[.exe]        内核（协议 v1 服务端，宿主按同目录查找）
//   payload/omnicrawl-tui[.exe]    终端工作台
//   payload/omnicrawl-api[.exe]    本地 HTTP/SSE 服务
//   payload/omnicrawl-mcp-server[.exe]
//   payload/omnicrawl/templates/   提示词与模式模板（源在 rust/assets/templates；缺失时用内嵌副本）
//   payload/omnicrawl/config/templates/   首次配置的三份 TOML 模板（源在 rust/assets/config-templates）
//   payload/omnicrawl/extensions/node_runner.mjs   插件 Worker 的 JS 端点（源在 rust/assets/extensions）
//
// `--legacy-python` 保留旧的 PyInstaller 路径（冻结 Python 宿主），仅用于对照与回退验证。
//
// 用法：
//   node packages/cli/scripts/build-host.mjs                    # 构建当前平台（Rust）
//   node packages/cli/scripts/build-host.mjs --target <triple>  # 交叉/指定三元组
//   node packages/cli/scripts/build-host.mjs --legacy-python    # 旧 PyInstaller 载荷
import { cpSync, existsSync, mkdirSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { execFileSync } from 'node:child_process'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import process from 'node:process'

const here = dirname(fileURLToPath(import.meta.url))
const repoRoot = resolve(here, '../../..')
const platformKey = `${process.platform}-${process.arch}`
const outRoot = join(repoRoot, 'dist', 'host', platformKey)
const payloadDir = join(outRoot, 'payload')
const legacyPython = process.argv.includes('--legacy-python')
const targetIndex = process.argv.indexOf('--target')
const rustTarget = targetIndex === -1 ? null : process.argv[targetIndex + 1]

/** 载荷里必须存在的可执行文件（平台后缀按目标平台给）。 */
const HOST_FILES = ['omnicrawl-host', 'omnicrawl', 'omnicrawl-tui', 'omnicrawl-api', 'omnicrawl-mcp-server']

function directorySize(path) {
  let total = 0
  for (const entry of readdirSync(path, { withFileTypes: true })) {
    const target = join(path, entry.name)
    total += entry.isDirectory() ? directorySize(target) : statSync(target).size
  }
  return total
}

function writeMeta(meta) {
  writeFileSync(join(outRoot, 'host-meta.json'), `${JSON.stringify(meta, null, 2)}\n`)
  const megabytes = (directorySize(payloadDir) / 1024 / 1024).toFixed(1)
  console.log(`[host] 载荷就绪：${payloadDir}（${megabytes} MB，omnicrawl ${meta.hostVersion}）`)
}

/** 按目标三元组判断可执行文件后缀。 */
function executableSuffix() {
  if (rustTarget) return rustTarget.includes('windows') ? '.exe' : ''
  return process.platform === 'win32' ? '.exe' : ''
}

/** Rust 宿主载荷：cargo release 产物 + 模板资源。 */
function buildRust() {
  const packages = [
    'omnicrawl-entry',
    'omnicrawl-cli',
    'omnicrawl-tui',
    'omnicrawl-api',
    'omnicrawl-mcp',
  ]
  const cargoArgs = ['build', '--release']
  for (const name of packages) cargoArgs.push('-p', name)
  if (rustTarget) cargoArgs.push('--target', rustTarget)
  console.log(`[host] cargo ${cargoArgs.join(' ')}`)
  execFileSync('cargo', cargoArgs, { cwd: join(repoRoot, 'rust'), stdio: 'inherit' })

  const releaseDir = rustTarget
    ? join(repoRoot, 'rust', 'target', rustTarget, 'release')
    : join(repoRoot, 'rust', 'target', 'release')
  const suffix = executableSuffix()

  rmSync(outRoot, { recursive: true, force: true })
  mkdirSync(payloadDir, { recursive: true })
  for (const name of HOST_FILES) {
    const candidate = join(releaseDir, `${name}${suffix}`)
    if (!existsSync(candidate)) {
      console.error(`[host] 缺少 cargo 产物：${candidate}`)
      process.exit(1)
    }
    cpSync(candidate, join(payloadDir, `${name}${suffix}`))
  }
  // 模板源在 rust/assets/templates（脱离 Python 包树后的单一来源）；载荷内仍走运行期磁盘路径
  // `<可执行文件祖先>/omnicrawl/templates`，缺失时用编译期内嵌副本。
  // 插件 Worker 的 JS 端点随载荷分发：宿主按 `<可执行文件祖先>/omnicrawl/extensions/` 找到它。
  cpSync(join(repoRoot, 'rust', 'assets', 'extensions'), join(payloadDir, 'omnicrawl', 'extensions'), {
    recursive: true,
  })
  cpSync(join(repoRoot, 'rust', 'assets', 'templates'), join(payloadDir, 'omnicrawl', 'templates'), {
    recursive: true,
  })
  cpSync(
    join(repoRoot, 'rust', 'assets', 'config-templates'),
    join(payloadDir, 'omnicrawl', 'config', 'templates'),
    { recursive: true },
  )

  // 版本号取自启动器包（`packages/cli/package.json`）：发布 tag 校验与 npm 包版本都以它为准。
  // 根 `package.json` 是私有 workspace 清单，没有 `version` 字段，早先读它会得到 undefined。
  const version = JSON.parse(
    readFileSync(join(repoRoot, 'packages', 'cli', 'package.json'), 'utf8'),
  ).version
  writeMeta({
    platform: platformKey,
    hostVersion: version,
    hostKind: 'rust',
    rustTarget,
    builtAt: new Date().toISOString(),
  })
}

// 旧路径：PyInstaller one-dir 冻结 Python 宿主（对照与回退验证用）。
//
// **待下线**：产品默认路径（`buildRust`）已完全不碰 Python。本函数保留到 Rust 侧
// 功能缺口补齐、Textual UI 对照价值耗尽为止，之后连同 `packaging/pyinstaller/` 一起移除。
// 它触发的 Python 调用逐处标了 `FROZEN-ALLOW`，供 `rust/tools/check_frozen_reference.mjs`
// 区分「已计划的回退路径」与「意外回流」。
function buildLegacyPython() {
  const specPath = join(repoRoot, 'packaging', 'pyinstaller', 'omnicrawl-host.spec')
  const distPath = join(outRoot, 'dist')
  const workPath = join(outRoot, 'build')
  const python = process.env.OMNICRAWL_PYTHON ?? (process.platform === 'win32' ? 'python' : 'python3')
  const run = (args, options = {}) =>
    // FROZEN-ALLOW：legacy 回退路径（待下线），产品默认路径不经过此处。
    execFileSync(python, args, { cwd: repoRoot, encoding: 'utf8', ...options })

  try {
    run(['-m', 'PyInstaller', '--version'], { stdio: ['ignore', 'pipe', 'pipe'] })
  } catch {
    console.error(`缺少 PyInstaller：${python} -m pip install pyinstaller`)
    process.exit(1)
  }

  rmSync(outRoot, { recursive: true, force: true })
  mkdirSync(outRoot, { recursive: true })
  run(
    [
      '-m', 'PyInstaller', '--noconfirm', '--clean', '--log-level', 'WARN',
      '--distpath', distPath, '--workpath', workPath, specPath,
    ],
    { stdio: 'inherit' },
  )
  const builtDir = join(distPath, 'omnicrawl-host')
  cpSync(builtDir, payloadDir, { recursive: true })
  writeMeta({
    platform: platformKey,
    hostVersion: run(['-c', 'import importlib.metadata as m; print(m.version("omnicrawl-agent"))']).trim(),
    hostKind: 'python',
    pythonVersion: run(['-c', 'import platform; print(platform.python_version())']).trim(),
    builtAt: new Date().toISOString(),
  })
}

if (legacyPython) {
  buildLegacyPython()
} else {
  buildRust()
}
