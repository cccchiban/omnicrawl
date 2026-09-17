#!/usr/bin/env node
// OmniCrawl 启动器：按 platform/arch 选出内核二进制并转交 stdio。
//
// 它不做任何协议转换——宿主直接与内核进程对话，启动器只是平台分包的解析器。
import { spawn } from 'node:child_process'
import { existsSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import process from 'node:process'

const PLATFORM_PACKAGES = {
  'linux-x64': '@omnicrawl/cli-linux-x64',
  'linux-arm64': '@omnicrawl/cli-linux-arm64-musl',
  'win32-x64': '@omnicrawl/cli-win32-x64',
  'win32-ia32': '@omnicrawl/cli-win32-ia32',
}

function resolveBinary() {
  // 开发与测试用的逃生口：跳过分包直接指定二进制。
  if (process.env.OMNICRAWL_BINARY) return process.env.OMNICRAWL_BINARY

  const key = `${process.platform}-${process.arch}`
  const packageName = PLATFORM_PACKAGES[key]
  if (!packageName) {
    throw new Error(`不支持的平台 ${key}；已支持：${Object.keys(PLATFORM_PACKAGES).join('、')}`)
  }

  const require = createRequire(import.meta.url)
  const packageRoot = dirname(require.resolve(`${packageName}/package.json`))
  const binary = join(packageRoot, 'bin', process.platform === 'win32' ? 'omnicrawl.exe' : 'omnicrawl')
  if (!existsSync(binary)) {
    throw new Error(`平台分包 ${packageName} 里没有二进制：${binary}\n请重装该包，或用 OMNICRAWL_BINARY 指定。`)
  }
  return binary
}

let binary
try {
  binary = resolveBinary()
} catch (error) {
  console.error(`omnicrawl：${error.message}`)
  process.exit(1)
}

const child = spawn(binary, process.argv.slice(2), { stdio: 'inherit' })

child.on('error', (error) => {
  console.error(`omnicrawl：无法启动 ${binary}：${error.message}`)
  process.exit(1)
})

child.on('exit', (code, signal) => {
  if (signal) {
    console.error(`omnicrawl：内核被信号 ${signal} 终止`)
    process.exit(1)
  }
  process.exit(code ?? 0)
})
