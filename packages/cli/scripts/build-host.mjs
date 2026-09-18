// 构建宿主载荷：用 PyInstaller one-dir 把 Python 运行时、依赖、代码与资源打成一个目录。
//
// 产物：dist/host/<platform>/omnicrawl-host/（PyInstaller 目录）+ host-meta.json（版本信息）。
// 平台只能与构建机同架构：载荷里含 CPython 运行时的原生扩展，交叉构建不成立。
//
// 用法：
//   node packages/cli/scripts/build-host.mjs                    # 构建当前平台
//   node packages/cli/scripts/build-host.mjs --install-tools    # 先 pip 装 PyInstaller
//   OMNICRAWL_PYTHON=/path/to/python node .../build-host.mjs
import { cpSync, existsSync, mkdirSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { execFileSync } from 'node:child_process'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import process from 'node:process'

const here = dirname(fileURLToPath(import.meta.url))
const repoRoot = resolve(here, '../../..')
const specPath = join(repoRoot, 'packaging', 'pyinstaller', 'omnicrawl-host.spec')
const platformKey = `${process.platform}-${process.arch}`
const outRoot = join(repoRoot, 'dist', 'host', platformKey)
const distPath = join(outRoot, 'dist')
const workPath = join(outRoot, 'build')

const python = process.env.OMNICRAWL_PYTHON ?? (process.platform === 'win32' ? 'python' : 'python3')

function run(args, options = {}) {
  return execFileSync(python, args, { cwd: repoRoot, encoding: 'utf8', ...options })
}

function ensurePyInstaller() {
  try {
    run(['-m', 'PyInstaller', '--version'], { stdio: ['ignore', 'pipe', 'pipe'] })
  } catch {
    if (!process.argv.includes('--install-tools')) {
      console.error(
        `缺少 PyInstaller：${python} -m pip install pyinstaller\n` +
          '（或加 --install-tools 让本脚本代装。）',
      )
      process.exit(1)
    }
    console.log('[host] 安装 PyInstaller 与运行期依赖…')
    run(['-m', 'pip', 'install', '--disable-pip-version-check', '-r', 'requirements.txt', 'pyinstaller'], {
      stdio: 'inherit',
    })
  }
}

function packageVersion() {
  return run([
    '-c',
    'import importlib.metadata as m; print(m.version("omnicrawl-agent"))',
  ]).trim()
}

function directorySize(path) {
  let total = 0
  for (const entry of readdirSync(path, { withFileTypes: true })) {
    const target = join(path, entry.name)
    total += entry.isDirectory() ? directorySize(target) : statSync(target).size
  }
  return total
}

ensurePyInstaller()

rmSync(outRoot, { recursive: true, force: true })
mkdirSync(outRoot, { recursive: true })

console.log(`[host] 平台 ${platformKey}，Python：${run(['--version']).trim()}`)
console.log('[host] PyInstaller 打包中（one-dir）…')

run(
  [
    '-m',
    'PyInstaller',
    '--noconfirm',
    '--clean',
    '--log-level',
    'WARN',
    '--distpath',
    distPath,
    '--workpath',
    workPath,
    specPath,
  ],
  { stdio: 'inherit' },
)

const builtDir = join(distPath, 'omnicrawl-host')
if (!existsSync(join(builtDir, process.platform === 'win32' ? 'omnicrawl-host.exe' : 'omnicrawl-host'))) {
  console.error(`[host] 打包结果不符合预期：缺少入口可执行文件（${builtDir}）。`)
  process.exit(1)
}

// 冻结目录就是发布内容：把内容平铺到 dist/host/<platform>/payload/，prepare.mjs 直接搬。
const payloadDir = join(outRoot, 'payload')
cpSync(builtDir, payloadDir, { recursive: true })

const meta = {
  platform: platformKey,
  hostVersion: packageVersion(),
  pythonVersion: run(['-c', 'import platform; print(platform.python_version())']).trim(),
  builtAt: new Date().toISOString(),
}
writeFileSync(join(outRoot, 'host-meta.json'), `${JSON.stringify(meta, null, 2)}\n`)

const megabytes = (directorySize(payloadDir) / 1024 / 1024).toFixed(1)
console.log(`[host] 载荷就绪：dist/host/${platformKey}/payload（${megabytes} MB，omnicrawl-agent ${meta.hostVersion}）`)
