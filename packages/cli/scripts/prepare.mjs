// 组装 npm 发布产物到 dist/npm：平台分包（内核 + 宿主载荷）与启动器。
//
// 仓库里的 packages/cli*/package.json 刻意保持「本地可安装」：不带 os/cpu，也不声明平台分包依赖。
// 否则 npm install 会因平台不匹配直接 notsup 失败（工作区成员同样会被校验）。
// os/cpu 与依赖只写进发布产物，这也是 esbuild 类项目的常见做法。
//
// 宿主载荷由 packages/cli/scripts/build-host.mjs 产出（cargo release 产物 + 模板资源，
// 可 `--target` 指定三元组；产品路径全是 Rust 二进制，构建期不碰 Python）。
// hostKey 为 null 的目标只带内核，启动器会退回协议直连（当前五个平台都有宿主载荷）。
//
// 用法：
//   node packages/cli/scripts/prepare.mjs                 # 拷已构建的平台，缺的只警告
//   node packages/cli/scripts/prepare.mjs --require-all   # 缺任一内核就失败（发布前跑）
//   node packages/cli/scripts/prepare.mjs --require-host  # 缺任一宿主载荷就失败（发布前跑）
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import process from 'node:process'

const here = dirname(fileURLToPath(import.meta.url))
const repoRoot = resolve(here, '../../..')
const staging = join(repoRoot, 'dist', 'npm')

/** 宿主载荷里的可执行文件（与 build-host.mjs 的 HOST_FILES 一致，不含 .exe 后缀）。 */
const HOST_EXECUTABLES = [
  'omnicrawl-host',
  'omnicrawl',
  'omnicrawl-tui',
  'omnicrawl-api',
  'omnicrawl-decision',
  'omnicrawl-fetch',
  'omnicrawl-mcp-server',
]

const TARGETS = [
  {
    name: '@omnicrawl/cli-win32-x64',
    file: 'omnicrawl.exe',
    triple: 'x86_64-pc-windows-msvc',
    os: 'win32',
    cpu: 'x64',
    hostKey: 'win32-x64',
    description: 'OmniCrawl 平台包（Windows x64：内核 + 完整宿主）',
  },
  {
    name: '@omnicrawl/cli-win32-ia32',
    file: 'omnicrawl.exe',
    triple: 'i686-pc-windows-msvc',
    os: 'win32',
    cpu: 'ia32',
    // 宿主的 BoringSSL 在 i686 上走 btls-sys 的「Windows 交叉」分支（OPENSSL_NO_ASM），
    // 由本机 VS2022 的 32 位工具集构建，见 rust/docs/python-free-build.md。
    hostKey: 'win32-ia32',
    description: 'OmniCrawl 平台包（Windows 32 位：内核 + 完整宿主）',
  },
  {
    name: '@omnicrawl/cli-linux-x64',
    file: 'omnicrawl',
    triple: 'x86_64-unknown-linux-musl',
    os: 'linux',
    cpu: 'x64',
    hostKey: 'linux-x64',
    description: 'OmniCrawl 平台包（Linux x64：内核 + 完整宿主）',
  },
  {
    name: '@omnicrawl/cli-linux-arm-musl',
    file: 'omnicrawl',
    triple: 'armv7-unknown-linux-musleabihf',
    os: 'linux',
    cpu: 'arm',
    // 宿主载荷须交叉编译 BoringSSL；本机用 cargo zigbuild 产出，见 rust/docs/python-free-build.md。
    hostKey: 'linux-arm',
    description: 'OmniCrawl 平台包（Linux armv7，musl 静态：内核 + 完整宿主）',
  },
  {
    name: '@omnicrawl/cli-linux-arm64-musl',
    file: 'omnicrawl',
    triple: 'aarch64-unknown-linux-musl',
    os: 'linux',
    cpu: 'arm64',
    hostKey: 'linux-arm64',
    description: 'OmniCrawl 平台包（Linux arm64：内核 + 完整宿主）',
  },
]

function writeJson(path, value) {
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`)
}

function directorySize(path) {
  let total = 0
  for (const entry of readdirSync(path, { withFileTypes: true })) {
    const target = join(path, entry.name)
    total += entry.isDirectory() ? directorySize(target) : statSync(target).size
  }
  return total
}

function kernelVersion() {
  const manifest = readFileSync(join(repoRoot, 'rust', 'Cargo.toml'), 'utf8')
  const match = manifest.match(/\[workspace\.package\][\s\S]*?version\s*=\s*"([^"]+)"/)
  if (!match) throw new Error('rust/Cargo.toml 里找不到 workspace 版本。')
  return match[1]
}

function sourceCandidates(target) {
  const candidates = [join(repoRoot, 'rust', 'target', target.triple, 'release', target.file)]
  // 无 --target 的构建产物只属于宿主机架构：回退到它会把别架构的包发成错架构。
  if (target.os === process.platform && target.cpu === process.arch) {
    candidates.push(join(repoRoot, 'rust', 'target', 'release', target.file))
  }
  return candidates
}

function hostPayload(target) {
  if (!target.hostKey) return null
  const root = join(repoRoot, 'dist', 'host', target.hostKey)
  const payload = join(root, 'payload')
  const metaPath = join(root, 'host-meta.json')
  if (!existsSync(payload) || !existsSync(metaPath)) return null
  return { payload, meta: JSON.parse(readFileSync(metaPath, 'utf8')) }
}

/** 逐文件递归复制：Node 22.22 的目录级 cpSync 在本机会静默崩掉整个进程（见 stageLauncher）。 */
function copyTree(source, destination) {
  mkdirSync(destination, { recursive: true })
  for (const entry of readdirSync(source, { withFileTypes: true })) {
    const from = join(source, entry.name)
    const to = join(destination, entry.name)
    if (entry.isDirectory()) copyTree(from, to)
    else if (entry.isFile()) copyFileSync(from, to)
  }
}

function stagePlatform(target, source, version, host) {
  const packageDir = join(staging, target.name.replace('@omnicrawl/', ''))
  mkdirSync(join(packageDir, 'bin'), { recursive: true })
  copyFileSync(source, join(packageDir, 'bin', target.file))
  if (host) copyTree(host.payload, join(packageDir, 'host'))

  writeJson(join(packageDir, 'version.json'), {
    name: target.name,
    version,
    os: target.os,
    cpu: target.cpu,
    kernelVersion: kernelVersion(),
    hostVersion: host?.meta.hostVersion ?? null,
    hostPlatform: host?.meta.platform ?? null,
    builtAt: new Date().toISOString(),
  })
  writeJson(join(packageDir, 'package.json'), {
    name: target.name,
    version,
    description: target.description,
    // Trusted Publishing 要求 repository.url 与 GitHub 仓库完全一致。
    repository: {
      type: 'git',
      url: 'git+https://github.com/cccchiban/omnicrawl.git',
      directory: `packages/${target.name.replace('@omnicrawl/', '')}`,
    },
    os: [target.os],
    cpu: [target.cpu],
    // npm 打包会把未在 bin 里声明的文件权限位归一化成 0644，POSIX 下按需补回可执行位。
    //
    // 名字不能叫 omnicrawl：启动器自己也声明了 omnicrawl，而平台包在安装树里同样会生成
    // node_modules/.bin 条目，两者撞名时平台包会抢走 .bin/omnicrawl → 本地安装与 npx 下
    // `omnicrawl` 变成直连内核（只打印 `omnicrawl 0.4.2`）。改名 omnicrawl-kernel 后
    // `.bin/omnicrawl` 只归启动器，内核仍有一个显式入口（协议调试可直接用 `omnicrawl kernel`）。
    //
    // 宿主的 6 个可执行文件同样必须列进 bin，否则 `npm pack` 会把它们写成 0644，
    // 而宿主是直接用 Command::new 拉起同目录的 tui/api/内核，不看 .bin——安装后必然
    // Permission denied。列进 bin 才能让 npm 在安装时补上可执行位（已在开发板实测）。
    // 载荷里的 `omnicrawl` 是内核副本，与启动器撞名，故另取 omnicrawl-host-kernel。
    bin: {
      'omnicrawl-kernel': `bin/${target.file}`,
      ...(host
        ? Object.fromEntries(
            HOST_EXECUTABLES.map((name) => [
              name === 'omnicrawl' ? 'omnicrawl-host-kernel' : name,
              `host/${name}${target.file.endsWith('.exe') ? '.exe' : ''}`,
            ]),
          )
        : {}),
    },
    files: [...(host ? ['bin', 'host', 'version.json'] : ['bin', 'version.json'])],
    license: 'MIT',
  })
  // 二进制目录在仓库里是 gitignore 的；发布产物必须绕过 .gitignore 过滤，靠 files 决定内容。
  writeFileSync(join(packageDir, '.npmignore'), '# 发布内容由 package.json 的 files 决定。\n')
  writeFileSync(
    join(packageDir, 'README.md'),
    `# ${target.name}\n\n${target.description}。\n\n由 \`omnicrawl-cli\` 按平台自动安装，不要直接依赖。\n`,
  )

  const kernelKilobytes = Math.round(statSync(join(packageDir, 'bin', target.file)).size / 1024)
  const hostMegabytes = host ? (directorySize(join(packageDir, 'host')) / 1024 / 1024).toFixed(1) : null
  return hostMegabytes
    ? `${target.name}（内核 ${kernelKilobytes} KB + 宿主 ${hostMegabytes} MB）`
    : `${target.name}（内核 ${kernelKilobytes} KB，无宿主载荷）`
}

function stageLauncher(version, platformNames) {
  const template = JSON.parse(readFileSync(join(repoRoot, 'packages', 'cli', 'package.json'), 'utf8'))
  const cliDir = join(staging, 'cli')
  // bin/ 下的入口不止一个：`omnicrawl.mjs` 静态 import 同目录的 `update.mjs`，只拷一个文件
  // 会让装出来的启动器在加载阶段直接 ERR_MODULE_NOT_FOUND（0.2.1 线上包就是这样）。
  // 逐个文件拷而不是 cpSync 目录：Node 22.22 的目录 cpSync 会静默崩掉整个进程。
  const binSource = join(repoRoot, 'packages', 'cli', 'bin')
  mkdirSync(join(cliDir, 'bin'), { recursive: true })
  for (const entry of readdirSync(binSource, { withFileTypes: true })) {
    if (!entry.isFile()) continue
    copyFileSync(join(binSource, entry.name), join(cliDir, 'bin', entry.name))
  }
  copyFileSync(join(repoRoot, 'packages', 'cli', 'README.md'), join(cliDir, 'README.md'))
  writeJson(join(cliDir, 'package.json'), {
    ...template,
    version,
    optionalDependencies: Object.fromEntries(platformNames.map((name) => [name, version])),
  })
  return 'omnicrawl-cli'
}

function main() {
  const requireAll = process.argv.includes('--require-all')
  const requireHost = process.argv.includes('--require-host')
  const launcherOnly = process.argv.includes('--launcher-only')
  const version = JSON.parse(
    readFileSync(join(repoRoot, 'packages', 'cli', 'package.json'), 'utf8'),
  ).version

  // CI 的发布任务只重排启动器：平台包由各平台 runner 单独构建并作为产物上传。
  if (launcherOnly) {
    rmSync(join(staging, 'cli'), { recursive: true, force: true })
    stageLauncher(version, TARGETS.map((target) => target.name))
    console.log('已暂存 omnicrawl-cli（仅启动器）\n\n发布：\n  npm publish dist/npm/cli')
    return
  }

  rmSync(staging, { recursive: true, force: true })

  const staged = []
  const missing = []
  const missingHosts = []
  for (const target of TARGETS) {
    const source = sourceCandidates(target).find((candidate) => existsSync(candidate))
    if (!source) {
      missing.push(target.name)
      continue
    }
    const host = hostPayload(target)
    if (target.hostKey && !host) missingHosts.push(target.name)
    staged.push(stagePlatform(target, source, version, host))
  }

  if (!staged.length) {
    console.error('没有任何平台产物：先 cargo build --release -p omnicrawl-cli。')
    process.exit(1)
  }

  const platformNames = TARGETS.map((target) => target.name)
  stageLauncher(version, platformNames)

  for (const line of staged) console.log(`已暂存 ${line}`)
  for (const name of missing) console.log(`缺少 ${name}：未找到 cargo 产物，跳过`)
  for (const name of missingHosts) {
    console.log(`缺少 ${name} 的宿主载荷：先 node packages/cli/scripts/build-host.mjs（只能与构建机同架构）`)
  }

  console.log(`\n发布产物在 dist/npm/，按顺序发布：`)
  for (const name of staged.map((line) => line.split('（')[0]).reverse()) {
    console.log(`  npm publish dist/npm/${name.replace('@omnicrawl/', '')}`)
  }
  console.log(`  npm publish dist/npm/cli`)

  if (missing.length && requireAll) {
    console.error(
      `\n发布门槛未通过：${missing.join('、')} 没有二进制。\n` +
        '交叉编译示例：cargo build --release -p omnicrawl-cli --target x86_64-unknown-linux-musl',
    )
    process.exit(1)
  }
  // 宿主载荷与内核分开找：两者是不同 crate 的产物，缺少宿主的平台包只带内核。
  if (missingHosts.length && requireHost) {
    console.error(
      `\n发布门槛未通过：${missingHosts.join('、')} 没有宿主载荷。\n` +
        '先构建宿主载荷：node packages/cli/scripts/build-host.mjs（可加 --target <三元组>）。',
    )
    process.exit(1)
  }
}

main()
