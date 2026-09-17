// 组装 npm 发布产物到 dist/npm：平台分包（带 os/cpu 声明）与启动器（带平台分包依赖）。
//
// 仓库里的 packages/cli*/package.json 刻意保持「本地可安装」：不带 os/cpu，也不声明平台分包依赖。
// 否则 npm install 会因平台不匹配直接 notsup 失败（工作区成员同样会被校验）。
// os/cpu 与依赖只写进发布产物，这也是 esbuild/生物类项目常见的做法。
//
// 用法：
//   node packages/cli/scripts/prepare.mjs                 # 拷已构建的平台，缺的只警告
//   node packages/cli/scripts/prepare.mjs --require-all   # 缺任何一个平台就失败（发布前跑）
import { copyFileSync, existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs'
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
    description: 'OmniCrawl 内核二进制（Windows x64）',
  },
  {
    name: '@omnicrawl/cli-win32-ia32',
    file: 'omnicrawl.exe',
    triple: 'i686-pc-windows-msvc',
    os: 'win32',
    cpu: 'ia32',
    description: 'OmniCrawl 内核二进制（Windows 32 位）',
  },
  {
    name: '@omnicrawl/cli-linux-x64',
    file: 'omnicrawl',
    triple: 'x86_64-unknown-linux-musl',
    os: 'linux',
    cpu: 'x64',
    description: 'OmniCrawl 内核二进制（Linux x64，musl 静态）',
  },
  {
    name: '@omnicrawl/cli-linux-arm64-musl',
    file: 'omnicrawl',
    triple: 'aarch64-unknown-linux-musl',
    os: 'linux',
    cpu: 'arm64',
    description: 'OmniCrawl 内核二进制（Linux arm64，musl 静态，嵌入式目标）',
  },
]

function writeJson(path, value) {
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`)
}

function sourceCandidates(target) {
  const candidates = [join(repoRoot, 'rust', 'target', target.triple, 'release', target.file)]
  // 无 --target 的构建产物只属于宿主机架构：回退到它会把别架构的包发成错架构。
  if (target.os === process.platform && target.cpu === process.arch) {
    candidates.push(join(repoRoot, 'rust', 'target', 'release', target.file))
  }
  return candidates
}

function stagePlatform(target, source, version) {
  const packageDir = join(staging, target.name.replace('@omnicrawl/', ''))
  mkdirSync(join(packageDir, 'bin'), { recursive: true })
  copyFileSync(source, join(packageDir, 'bin', target.file))
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
    files: ['bin'],
    license: 'MIT',
  })
  // 二进制目录在仓库里是 gitignore 的；发布产物必须绕过 .gitignore 过滤，靠 files 决定内容。
  writeFileSync(join(packageDir, '.npmignore'), '# 发布内容由 package.json 的 files 决定。\n')
  writeFileSync(join(packageDir, 'README.md'), `# ${target.name}\n\n${target.description}。\n\n由 \`omnicrawl-cli\` 按平台自动安装，不要直接依赖。\n`)
  const kilobytes = Math.round(statSync(join(packageDir, 'bin', target.file)).size / 1024)
  return `${target.name}（${kilobytes} KB）`
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
  const version = JSON.parse(
    readFileSync(join(repoRoot, 'packages', 'cli', 'package.json'), 'utf8'),
  ).version

  rmSync(staging, { recursive: true, force: true })

  const staged = []
  const missing = []
  for (const target of TARGETS) {
    const source = sourceCandidates(target).find((candidate) => existsSync(candidate))
    if (!source) {
      missing.push(target.name)
      continue
    }
    staged.push(stagePlatform(target, source, version))
  }

  if (!staged.length) {
    console.error('没有任何平台产物：先 cargo build --release -p omnicrawl-cli。')
    process.exit(1)
  }

  const platformNames = TARGETS.map((target) => target.name)
  stageLauncher(version, platformNames)

  for (const line of staged) console.log(`已暂存 ${line}`)
  for (const name of missing) console.log(`缺少 ${name}：未找到 cargo 产物，跳过`)

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
}

main()
