// 组装 npm 发布产物到 dist/npm：平台分包（内核 + 宿主载荷）与启动器。
//
// 仓库里的 packages/cli*/package.json 刻意保持「本地可安装」：不带 os/cpu，也不声明平台分包依赖。
// 否则 npm install 会因平台不匹配直接 notsup 失败（工作区成员同样会被校验）。
// os/cpu 与依赖只写进发布产物，这也是 esbuild 类项目的常见做法。
//
// 宿主载荷由 packages/cli/scripts/build-host.mjs 产出（PyInstaller one-dir），只能与构建机同架构：
// hostKey 为 null 的目标（32 位 Windows、armv7 嵌入式）只带内核，启动器会退回协议直连。
//
// 用法：
//   node packages/cli/scripts/prepare.mjs                 # 拷已构建的平台，缺的只警告
//   node packages/cli/scripts/prepare.mjs --require-all   # 缺任一内核就失败（发布前跑）
//   node packages/cli/scripts/prepare.mjs --require-host  # 缺任一宿主载荷就失败（发布前跑）
import {
  copyFileSync,
  cpSync,
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
    // 32 位 Windows 缺 numpy / onnxruntime 等运行期轮子，只发内核。
    hostKey: null,
    description: 'OmniCrawl 内核二进制（Windows 32 位）',
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
    // 嵌入式目标只跑静态内核（宿主需要 CPython 与各原生轮子，armv7 无可用产物）。
    hostKey: null,
    description: 'OmniCrawl 内核二进制（Linux armv7，musl 静态，嵌入式目标）',
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

function stagePlatform(target, source, version, host) {
  const packageDir = join(staging, target.name.replace('@omnicrawl/', ''))
  mkdirSync(join(packageDir, 'bin'), { recursive: true })
  copyFileSync(source, join(packageDir, 'bin', target.file))
  if (host) cpSync(host.payload, join(packageDir, 'host'), { recursive: true })

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
    // npm 打包时会把 bin 指向的文件写成 0755。不声明 bin，Windows 上打出的
    // tarball 里内核二进制就是 0644，Linux/macOS 安装后 spawn 会 EACCES。
    bin: { omnicrawl: `bin/${target.file}` },
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
  mkdirSync(join(cliDir, 'bin'), { recursive: true })
  copyFileSync(
    join(repoRoot, 'packages', 'cli', 'bin', 'omnicrawl.mjs'),
    join(cliDir, 'bin', 'omnicrawl.mjs'),
  )
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
  if (missingHosts.length && requireHost) {
    console.error(
      `\n发布门槛未通过：${missingHosts.join('、')} 没有宿主载荷。\n` +
        '宿主载荷必须在该平台的原生 runner 上构建（见 .github/workflows/publish-npm.yml）。',
    )
    process.exit(1)
  }
}

main()
