// 安装冒烟：把 dist/npm 的发布产物在干净前缀里真装一遍，验证启动器能解析平台包并给出可用命令。
//
// 用法：
//   node packages/cli/scripts/prepare.mjs          # 先暂存发布产物
//   node packages/cli/scripts/install-smoke.mjs    # 选项：--keep 保留临时目录便于排查
import { execFileSync, spawnSync } from 'node:child_process'
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readlinkSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import process from 'node:process'

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '../../..')
const staging = join(repoRoot, 'dist', 'npm')
const platformPackage = {
  'linux-x64': 'cli-linux-x64',
  'linux-arm': 'cli-linux-arm-musl',
  'linux-arm64': 'cli-linux-arm64-musl',
  'win32-x64': 'cli-win32-x64',
  'win32-ia32': 'cli-win32-ia32',
}[`${process.platform}-${process.arch}`]

function fail(message) {
  console.error(`安装冒烟失败：${message}`)
  process.exit(1)
}

function npm(args, cwd = work) {
  return execFileSync(process.platform === 'win32' ? 'npm.cmd' : 'npm', args, {
    cwd,
    encoding: 'utf8',
    shell: process.platform === 'win32',
  })
}

if (!existsSync(staging)) fail('发行产物不存在：先跑 node packages/cli/scripts/prepare.mjs')
if (!existsSync(join(staging, platformPackage))) fail(`本机平台包未暂存：${staging}/${platformPackage}`)

const work = mkdtempSync(join(tmpdir(), 'omnicrawl-smoke-'))
const packs = join(work, 'packs')
const app = join(work, 'app')

try {
  mkdirSync(packs, { recursive: true })

  const tarballs = []
  for (const name of [platformPackage, 'cli']) {
    const output = npm(['pack', join(staging, name), '--pack-destination', packs, '--json'])
    const [entry] = JSON.parse(output)
    tarballs.push(join(packs, entry.filename))
  }

  mkdirSync(app, { recursive: true })
  writeFileSync(
    join(app, 'package.json'),
    `${JSON.stringify({ name: 'omnicrawl-smoke', private: true }, null, 2)}\n`,
  )
  // --omit=optional：其余平台的包尚未发布，也不该在冒烟里被拉取；本平台的包由 tarball 显式给出。
  npm(['install', '--no-audit', '--no-fund', '--omit=optional', '--no-save', ...tarballs], app)

  const launcher = join(app, 'node_modules', 'omnicrawl-cli', 'bin', 'omnicrawl.mjs')
  if (!existsSync(launcher)) fail(`安装后找不到启动器：${launcher}`)

  const version = execFileSync(process.execPath, [launcher, '--version'], { encoding: 'utf8', cwd: app })
  const [firstLine] = version.trim().split('\n')
  if (!/^omnicrawl \d+\.\d+\.\d+$/.test(firstLine)) fail(`--version 首行不符合期望：${firstLine}`)
  if (!version.includes(`@omnicrawl/${platformPackage}`)) fail(`--version 没有报出平台包：\n${version}`)

  const help = execFileSync(process.execPath, [launcher, '--help'], { encoding: 'utf8', cwd: app })
  for (const command of ['api', 'kernel', 'plugin']) {
    if (!help.includes(command)) fail(`--help 没有列出 ${command} 指令：\n${help}`)
  }

  // .bin 层：本地安装 / npx 走的是 node_modules/.bin/omnicrawl，而不是上面那个 .mjs 路径。
  // 平台包曾经同样声明 omnicrawl 的 bin，npm 会让它抢走这个条目，于是 `omnicrawl` 变成
  // 直连内核（只打印 `omnicrawl 0.0.1`）——真实用户报障过，所以这里守住归属。
  const binDir = join(app, 'node_modules', '.bin')
  const isWindows = process.platform === 'win32'
  const launcherBin = join(binDir, isWindows ? 'omnicrawl.cmd' : 'omnicrawl')
  if (!existsSync(launcherBin)) fail(`安装后 .bin 里没有 omnicrawl：${launcherBin}`)
  const binTarget = (isWindows ? readFileSync(launcherBin, 'utf8') : readlinkSync(launcherBin)).replace(
    /\\/g,
    '/',
  )
  if (!binTarget.includes('omnicrawl-cli/bin/omnicrawl.mjs')) {
    fail(`.bin/omnicrawl 没有指向启动器，而是：${binTarget.trim()}`)
  }
  const kernelBin = join(binDir, isWindows ? 'omnicrawl-kernel.cmd' : 'omnicrawl-kernel')
  if (!existsSync(kernelBin)) fail(`.bin 里缺少内核入口 omnicrawl-kernel：${kernelBin}`)

  // 链路闭环：默认（无参数）应真的把宿主拉起来。这里没有终端，宿主应给出提示并退 2。
  const hosted = spawnSync(process.execPath, [launcher], { encoding: 'utf8', cwd: app, input: '' })
  const stderr = hosted.stderr ?? ''
  if (!stderr.includes('没有交互式终端')) {
    fail(`默认启动没有走宿主（退出码 ${hosted.status}）：\n${stderr || hosted.stdout}`)
  }
  if (hosted.status !== 2) fail(`无终端时宿主退出码应为 2，实际 ${hosted.status}`)

  console.log(`安装冒烟通过：${platformPackage}`)
  console.log(version.trim().split('\n').map((line) => `  ${line}`).join('\n'))
} finally {
  if (process.argv.includes('--keep')) console.log(`临时目录保留在：${work}`)
  else rmSync(work, { recursive: true, force: true })
}
