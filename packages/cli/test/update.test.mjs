// 启动自动更新（`bin/update.mjs`）的单元测试：版本比较、缓存、配置开关与护栏。
//
// 全部依赖注入，不触网、不动真实 HOME：`latestChecker` / `installer` / `relauncher` 都由用例提供。
import assert from 'node:assert/strict'
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { test } from 'node:test'

import {
  AUTO_UPDATE_ATTEMPTED_ENV,
  SKIP_AUTO_UPDATE_ENV,
  VERSION_CHECK_CACHE_FILENAME,
  isNewerVersion,
  isSourceCheckout,
  loadUpdateEnabled,
  parseRegistryVersion,
  parseVersion,
  runStartupUpdateIfDue,
  shouldCheckUpdate,
} from '../bin/update.mjs'

function tempHome() {
  return mkdtempSync(join(tmpdir(), 'omnicrawl-update-'))
}

/** 一次「有新版、安装成功、重启成功」的最小调用；用例按需覆盖字段。 */
function callUpdate(overrides = {}) {
  return runStartupUpdateIfDue({
    argv: [],
    currentVersion: '0.2.0',
    launcherPath: join(tmpdir(), 'global-node-modules', 'bin', 'omnicrawl.mjs'),
    env: {},
    home: overrides.home ?? tempHome(),
    print: () => {},
    now: () => Date.UTC(2026, 0, 1),
    sourceCheckout: () => false,
    configEnabled: () => true,
    latestChecker: async () => '0.3.0',
    installer: async () => 0,
    relauncher: async () => 0,
    ...overrides,
  })
}

test('parseVersion 接受常见 semver 写法，非法值返回 null', () => {
  assert.deepEqual(parseVersion('1.2.3'), { release: [1, 2, 3], pre: null })
  assert.deepEqual(parseVersion('v1.2.3-rc.1'), { release: [1, 2, 3], pre: ['rc', '1'] })
  assert.deepEqual(parseVersion('1.2.3+build.5'), { release: [1, 2, 3], pre: null })
  assert.equal(parseVersion('not-a-version'), null)
  assert.equal(parseVersion(''), null)
})

test('isNewerVersion 按数值与预发布顺序比较，异常值按不可升级处理', () => {
  assert.equal(isNewerVersion('0.3.0', '0.2.1'), true)
  assert.equal(isNewerVersion('1.0.0', '0.9.9'), true)
  assert.equal(isNewerVersion('0.2.1', '0.2.1'), false)
  assert.equal(isNewerVersion('0.2.0', '0.2.1'), false)
  // 预发布低于同版本正式版：正式版 > rc > 更早的 rc。
  assert.equal(isNewerVersion('0.3.0', '0.3.0-rc.1'), true)
  assert.equal(isNewerVersion('0.3.0-rc.1', '0.3.0'), false)
  assert.equal(isNewerVersion('0.3.0-rc.2', '0.3.0-rc.1'), true)
  // 补零：1.2 == 1.2.0。
  assert.equal(isNewerVersion('1.2', '1.2.0'), false)
  assert.equal(isNewerVersion('bad', '1.0.0'), false)
  assert.equal(isNewerVersion('1.0.0', 'bad'), false)
})

test('parseRegistryVersion 读 latest 响应体，缺版本时抛错', () => {
  assert.equal(parseRegistryVersion({ version: '1.4.0' }), '1.4.0')
  assert.equal(parseRegistryVersion('{"version":"1.4.0"}'), '1.4.0')
  assert.throws(() => parseRegistryVersion({}), /没有有效版本/)
  assert.throws(() => parseRegistryVersion({ version: 'nope' }), /没有有效版本/)
})

test('loadUpdateEnabled 缺省开启，只认 [update] 段的 enabled', () => {
  const home = tempHome()
  try {
    assert.equal(loadUpdateEnabled(home), true, '没有配置文件时默认开启')
    const dir = join(home, '.OmniCrawl')
    mkdirSync(dir, { recursive: true })
    const config = join(dir, 'config.toml')
    writeFileSync(config, '[update]\nenabled = false\n\n[tools]\nenabled = false\n')
    assert.equal(loadUpdateEnabled(home), false)
    // 别的段的 enabled 不影响 update。
    writeFileSync(config, '[tools]\nenabled = false\n')
    assert.equal(loadUpdateEnabled(home), true)
    writeFileSync(config, '[update]\n# enabled 缺失\n')
    assert.equal(loadUpdateEnabled(home), true)
  } finally {
    rmSync(home, { recursive: true, force: true })
  }
})

test('shouldCheckUpdate 只认工作台路径，逃生口与已知子命令都跳过', () => {
  assert.equal(shouldCheckUpdate([], {}), true)
  assert.equal(shouldCheckUpdate(['--resume', 's1'], {}), true)
  assert.equal(shouldCheckUpdate(['api'], {}), false)
  assert.equal(shouldCheckUpdate(['kernel', '--version'], {}), false)
  assert.equal(shouldCheckUpdate(['plugin', 'list'], {}), false)
  assert.equal(shouldCheckUpdate([], { OMNICRAWL_BINARY: '/tmp/x' }), false)
  assert.equal(shouldCheckUpdate([], { OMNICRAWL_HOST: '/tmp/x' }), false)
})

test('isSourceCheckout 祖先里出现 .git 即判定为源码检出', () => {
  const root = tempHome()
  try {
    const nested = join(root, 'repo', 'packages', 'cli', 'bin')
    mkdirSync(nested, { recursive: true })
    writeFileSync(join(nested, 'omnicrawl.mjs'), '')
    assert.equal(isSourceCheckout(join(nested, 'omnicrawl.mjs')), false)
    mkdirSync(join(root, 'repo', '.git'))
    assert.equal(isSourceCheckout(join(nested, 'omnicrawl.mjs')), true)
  } finally {
    rmSync(root, { recursive: true, force: true })
  }
})

test('无需更新时不安装、不重启', async () => {
  let installed = 0
  let relaunched = 0
  const code = await callUpdate({
    latestChecker: async () => '0.2.0',
    installer: async () => {
      installed += 1
      return 0
    },
    relauncher: async () => {
      relaunched += 1
      return 0
    },
  })
  assert.equal(code, null)
  assert.equal(installed, 0)
  assert.equal(relaunched, 0)
})

test('有新版时安装并重启，返回子进程退出码并写下尝试标记', async () => {
  const env = {}
  const lines = []
  const calls = []
  const code = await callUpdate({
    env,
    print: (line) => lines.push(line),
    installer: async (latest) => {
      calls.push(['install', latest])
      return 0
    },
    relauncher: async (launcher, argv) => {
      calls.push(['relaunch', launcher, argv])
      return 7
    },
  })
  assert.equal(code, 7)
  assert.deepEqual(calls[0], ['install', '0.3.0'])
  assert.equal(calls[1][0], 'relaunch')
  assert.equal(env[AUTO_UPDATE_ATTEMPTED_ENV], '1', '升级尝试后写入标记防循环')
  assert.ok(lines.some((line) => line.includes('发现新版本 0.3.0')))
  assert.ok(lines.some((line) => line.includes('正在重新启动工作台')))
})

test('安装失败只打印原因与手动命令，继续用当前版本启动', async () => {
  const lines = []
  let relaunched = 0
  const code = await callUpdate({
    print: (line) => lines.push(line),
    installer: async () => 1,
    relauncher: async () => {
      relaunched += 1
      return 0
    },
  })
  assert.equal(code, null)
  assert.equal(relaunched, 0)
  assert.ok(lines.some((line) => line.includes('自动升级失败')))
  assert.ok(lines.some((line) => line.includes('npm install -g omnicrawl-cli@0.3.0')))
})

test('重启失败返回 0，让本次调用以成功退出', async () => {
  const lines = []
  const code = await callUpdate({
    print: (line) => lines.push(line),
    relauncher: async () => null,
  })
  assert.equal(code, 0)
  assert.ok(lines.some((line) => line.includes('重新启动工作台失败')))
})

test('护栏：跳过开关、尝试标记、逃生口、源码检出与配置关闭都直接返回', async () => {
  let checked = 0
  const latestChecker = async () => {
    checked += 1
    return '9.9.9'
  }
  const cases = [
    { name: '跳过开关', overrides: { env: { [SKIP_AUTO_UPDATE_ENV]: '1' } } },
    { name: '尝试标记', overrides: { env: { [AUTO_UPDATE_ATTEMPTED_ENV]: '1' } } },
    { name: '逃生口', overrides: { env: { OMNICRAWL_BINARY: '/tmp/kernel' } } },
    { name: '源码检出', overrides: { sourceCheckout: () => true } },
    { name: '配置关闭', overrides: { configEnabled: () => false } },
  ]
  for (const testCase of cases) {
    const code = await callUpdate({ latestChecker, ...testCase.overrides })
    assert.equal(code, null, testCase.name)
  }
  assert.equal(checked, 0, '被护栏拦下时不应请求版本')
})

test('24h 缓存命中时不请求 registry，过期后重新请求', async () => {
  const home = tempHome()
  try {
    const dir = join(home, '.OmniCrawl')
    mkdirSync(dir, { recursive: true })
    writeFileSync(
      join(dir, VERSION_CHECK_CACHE_FILENAME),
      JSON.stringify({ checked_at: 1000, latest_version: '0.3.0' }),
    )
    let checked = 0
    const latestChecker = async () => {
      checked += 1
      return '0.3.0'
    }

    // 缓存 1 小时后仍新鲜。
    const cached = await callUpdate({
      home,
      latestChecker,
      now: () => 1001_000,
      relauncher: async () => 0,
      installer: async () => 0,
    })
    assert.equal(cached, 0)
    assert.equal(checked, 0, '缓存新鲜时不请求')

    // 缓存超过 24h 后重新请求，并把新结果写回。
    const refreshed = await callUpdate({
      home,
      latestChecker,
      now: () => 1000_000 + 25 * 60 * 60 * 1000,
    })
    assert.equal(refreshed, 0)
    assert.equal(checked, 1)
  } finally {
    rmSync(home, { recursive: true, force: true })
  }
})

test('registry 出错或缓存损坏时静默降级，不影响启动', async () => {
  const home = tempHome()
  try {
    const dir = join(home, '.OmniCrawl')
    mkdirSync(dir, { recursive: true })
    writeFileSync(join(dir, VERSION_CHECK_CACHE_FILENAME), '{ 不是 JSON')
    const code = await callUpdate({
      home,
      latestChecker: async () => {
        throw new Error('离线')
      },
    })
    assert.equal(code, null)
  } finally {
    rmSync(home, { recursive: true, force: true })
  }
})
