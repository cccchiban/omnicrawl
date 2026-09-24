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
//   payload/rust/assets/templates/          提示词与模式模板（源在 rust/assets/templates；缺失时用内嵌副本）
//   payload/rust/assets/config-templates/   首次配置的三份 TOML 模板（源在 rust/assets/config-templates）
//   payload/rust/assets/extensions/node_runner.mjs   插件 Worker 的 JS 端点（源在 rust/assets/extensions）
//
// 资源必须放在 `rust/assets/*` 这一层：它同时是运行期搜索的第一候选
// （`omnicrawl-extensions` 的 `runner_search_dirs`、`omnicrawl-host` 的模板搜索、
// `omnicrawl-entry` 的 `locate_templates_dir` 都按「可执行文件目录 → 上级」逐级找
// `rust/assets/<资源>`），读不到时二进制里还有编译期内嵌副本。
//
// 非 Windows 目标的内核文件名就叫 `omnicrawl`（没有 `.exe`），跟旧的 `omnicrawl/<资源>` 目录
// **同名冲突**，所以旧布局只在带后缀的平台上额外补一份（Windows），保证老路径也能读到。
//
// `--legacy-python` 保留旧的 PyInstaller 路径（冻结 Python 宿主），仅用于对照与回退验证。
//
// 用法：
//   node packages/cli/scripts/build-host.mjs                    # 构建当前平台（Rust）
//   node packages/cli/scripts/build-host.mjs --target <triple>  # 交叉/指定三元组
//   node packages/cli/scripts/build-host.mjs --target <triple> --zigbuild  # 用 zig 做 C/C++ 交叉
//   node packages/cli/scripts/build-host.mjs --legacy-python    # 旧 PyInstaller 载荷
//   node packages/cli/scripts/build-host.mjs --target <triple> --platform linux-x64 --skip-build
//
// `--platform <键>`：载荷目录用哪个平台键（默认取构建机，即 `<process.platform>-<arch>`）。
//   在一台机器上交叉构建别的平台时**必须**给，否则载荷会落到构建机的键下，
//   `prepare.mjs` 按目标平台的 `hostKey` 找不到它。键要与 prepare.mjs 的 hostKey 一致：
//   win32-x64 / linux-x64 / linux-arm64。
// `--skip-build`：不调用 cargo，只按现有产物装配载荷（产物已经在 `target/<三元组>/release`
//   时用；例如用 `CARGO_TARGET_DIR` 在别的目录构建完之后，或只想重排一次载荷）。
//
// `--target` 必须与「构建内核」步骤给的三元组一致，否则找不到 cargo 产物（release
// 目录是 `<triple>/release`，与无 `--target` 的 `release/` 不是同一处）。
// `--zigbuild` 换成 `cargo zigbuild`：musl 目标的 BoringSSL 需要能编 C++ 的交叉工具链，
// 而 `musl-tools` 只给 `musl-gcc`（C），所以这类目标要传它并先在 PATH 上装好 zig。
import { cpSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { execFileSync } from 'node:child_process'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import process from 'node:process'

const here = dirname(fileURLToPath(import.meta.url))
const repoRoot = resolve(here, '../../..')
// 载荷目录的键默认按构建机给；交叉构建别的平台时用 `--platform` 指到目标平台的键。
const platformIndex = process.argv.indexOf('--platform')
const platformArg = platformIndex === -1 ? null : process.argv[platformIndex + 1]
if (platformArg && !/^(win32|linux|darwin)-(x64|ia32|arm64|arm)$/.test(platformArg)) {
  console.error(`[host] --platform 取值不合法：${platformArg}（形如 linux-x64）`)
  process.exit(1)
}
const platformKey = platformArg ?? `${process.platform}-${process.arch}`
const outRoot = join(repoRoot, 'dist', 'host', platformKey)
const payloadDir = join(outRoot, 'payload')
const legacyPython = process.argv.includes('--legacy-python')
const targetIndex = process.argv.indexOf('--target')
const rustTarget = targetIndex === -1 ? null : process.argv[targetIndex + 1]
// 交叉编译用 zig 提供的 C/C++ 工具链（cargo-zigbuild 会把它接到 CC/CXX 与链接器上）。
const zigbuild = process.argv.includes('--zigbuild')
// 只装配、不构建：产物已就位时用，避免在临时目录/自定义 CARGO_TARGET_DIR 的场景下白跑一遍 cargo。
const skipBuild = process.argv.includes('--skip-build')

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
  const cargoArgs = [zigbuild ? 'zigbuild' : 'build', '--release']
  for (const name of packages) cargoArgs.push('-p', name)
  if (rustTarget) cargoArgs.push('--target', rustTarget)
  if (skipBuild) {
    console.log(`[host] --skip-build：跳过 cargo ${cargoArgs.join(' ')}`)
  } else {
    console.log(`[host] cargo ${cargoArgs.join(' ')}`)
    execFileSync('cargo', cargoArgs, { cwd: join(repoRoot, 'rust'), stdio: 'inherit' })
  }

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
  // 资源源在 rust/assets/*（脱离 Python 包树后的单一来源），载荷里放到运行期搜索的
  // 第一候选 `<可执行文件祖先>/rust/assets/*`；读不到时运行期还有编译期内嵌副本。
  cpSync(
    join(repoRoot, 'rust', 'assets', 'extensions'),
    join(payloadDir, 'rust', 'assets', 'extensions'),
    { recursive: true },
  )
  cpSync(join(repoRoot, 'rust', 'assets', 'templates'), join(payloadDir, 'rust', 'assets', 'templates'), {
    recursive: true,
  })
  cpSync(
    join(repoRoot, 'rust', 'assets', 'config-templates'),
    join(payloadDir, 'rust', 'assets', 'config-templates'),
    { recursive: true },
  )
  // 旧布局 `omnicrawl/<资源>`：只在可执行文件名带后缀（Windows）时补一份——不带后缀的目标
  // 内核文件就叫 `omnicrawl`，再建 `omnicrawl/` 目录会直接撞名。
  if (suffix) {
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
  }

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
  // zig 交叉只在有明确三元组时才有意义：没有 `--target` 时 cargo zigbuild 就是本机构建，
  // 而载荷目录会落到 `release/`，很容易把错架构的产物当成交叉产物发出去——直接拦下。
  if (zigbuild && !rustTarget) {
    console.error('[host] --zigbuild 必须与 --target <三元组> 一起用。')
    process.exit(1)
  }
  buildRust()
}
