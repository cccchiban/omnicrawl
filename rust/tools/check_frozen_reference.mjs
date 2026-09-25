// 冻结基准的准入检查：确保「产品链路不重新引入 Python」。
//
// 背景
// ----
// `omnicrawl/`（Python）已降级为**冻结的语义基准**：它只服务于 `rust/tools/gen_*.py`
// 生成 parity fixture，以及 `tests/` 的对照测试；产品链路（Rust 内核 + Rust 宿主 +
// npm 分发）不应再依赖 Python 解释器。
//
// 冻结之所以需要机械约束：这类耦合通常以「顺手加一行」的形式回流（CI 里补一个
// `pip install`、构建脚本里调一次 `python -c` 拿版本号），review 时很不起眼。
// 本脚本把边界变成可执行断言，在 CI 上每次推送都跑。
//
// 检查三类侵蚀
// ------------
//   1. Rust **产品代码**（`crates/*/src/`）调用 Python 解释器（含 pyinstaller）；
//   2. CI workflow 安装 Python 包（armv7 的 ziglang 交叉工具链除外）；
//   3. npm 构建脚本调用 Python（无例外：产品载荷全是 Rust 二进制，构建期不碰 Python）。
//
// `crates/*/tests/` 里的 Python 调用不算侵蚀——那是 parity 对照测试，和 `rust/tools/gen_*.py`
// 同属「基准工具链」。它们会被登记为**已知开发期依赖**打印出来（信息性，不影响退出码），
// 以防有人误以为测试套件不碰 Python。
//
// 用法
// ----
//   node rust/tools/check_frozen_reference.mjs
//
// 退出码：0 通过，1 有违规（并逐条打印文件:行号与命中内容）。

import { readFileSync, readdirSync, statSync } from 'node:fs'
import { dirname, join, relative, resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'
import process from 'node:process'

const here = dirname(fileURLToPath(import.meta.url))
const repoRoot = resolve(here, '..', '..')

const violations = []
const devOnlyPythonUse = []

function report(rule, file, line, text) {
  violations.push({ rule, file: relative(repoRoot, file).split(sep).join('/'), line, text: text.trim() })
}

function noteDevOnly(file, line, text) {
  devOnlyPythonUse.push({
    file: relative(repoRoot, file).split(sep).join('/'),
    line,
    text: text.trim(),
  })
}

/** 递归收集匹配后缀的文件，跳过常见产物目录。 */
function collect(dir, suffixes, skipDirs) {
  const found = []
  const walk = (current) => {
    let entries
    try {
      entries = readdirSync(current, { withFileTypes: true })
    } catch {
      return
    }
    for (const entry of entries) {
      const full = join(current, entry.name)
      if (entry.isDirectory()) {
        if (skipDirs.has(entry.name)) continue
        walk(full)
      } else if (suffixes.some((suffix) => entry.name.endsWith(suffix))) {
        found.push(full)
      }
    }
  }
  if (statSync(dir, { throwIfNoEntry: false })?.isDirectory()) walk(dir)
  return found
}

/**
 * 该行是否为行注释（Rust / JS 的 `//`、YAML 的 `#`）。
 *
 * 注释里的举例文字（如「CI 里补 pip install」）不是可执行语句，不能算作依赖回流；
 * 本检查器最早就是被自己的文档注释误伤的。
 */
function isCommentLine(line) {
  return /^\s*(\/\/|\*|\/\*|#)/.test(line)
}

/**
 * YAML 中所有处于 `run:` 块内的行号。
 *
 * `pip install` 只有写在 `run:` 里才会真的执行；`name:` / `if:` 一类字段里的同样字串只是文案。
 * 块范围按缩进判定：`run:` 行自身 + 其后缩进更深的行（含空行）。
 */
function yamlRunBlockLines(lines) {
  const inside = new Set()
  let blockIndent = null
  lines.forEach((line, index) => {
    const declaration = /^(\s*)run:\s*(.*)$/.exec(line)
    if (declaration) {
      blockIndent = declaration[1].length
      inside.add(index)
      return
    }
    if (blockIndent === null) return
    if (line.trim() === '') {
      inside.add(index)
      return
    }
    if (line.match(/^\s*/)[0].length > blockIndent) {
      inside.add(index)
      return
    }
    blockIndent = null
  })
  return inside
}

// ── 规则 1：Rust 产品代码不得调用 Python 解释器 ────────────────────────────────
// 命中 `Command::new("python")` / `Command::new("python3")` / `Command::new("pyinstaller")`
// 一类进程调用。Rust 侧出现的 `python_str` / `python_dumps_compact` 只是「模拟 Python 语义」
// 的函数名，不是进程调用，因此正则锚定在进程启动上。
//
// 范围限定 `crates/*/src/`：`tests/` 下的 Python 用法用于 parity 对照（见 `noteDevOnly`），
// 且通常带 `OMNICRAWL_PYTHON` 门控，不属于产品依赖。
const RUST_PYTHON_CALL = /\b(Command::new|Command::new_async|process::Command)\s*\(\s*&?\s*["']?(python|python3|pyinstaller)\b/i
const IS_PARITY_TEST_DIR = /(^|\/)tests(\/|$)/  // 本行只是路径判定

for (const file of collect(join(repoRoot, 'rust', 'crates'), ['.rs'], new Set(['target', 'node_modules']))) {
  const lines = readFileSync(file, 'utf8').split('\n')
  const isParityTest = IS_PARITY_TEST_DIR.test(relative(repoRoot, file).split(sep).join('/'))
  lines.forEach((text, index) => {
    if (!RUST_PYTHON_CALL.test(text)) return
    if (isCommentLine(text)) return
    if (isParityTest) {
      noteDevOnly(file, index + 1, text)
      return
    }
    report('rust-spawns-python', file, index + 1, text)
  })
}

// ── 规则 2：CI workflow 不得安装 Python 包 ────────────────────────────────────
// armv7 的 Zig 交叉工具链走 `pip install --user ziglang`，与产品链路无关，放行。
const PIP_INSTALL = /\bpip3?\s+install\b/

for (const file of collect(join(repoRoot, '.github', 'workflows'), ['.yml', '.yaml'], new Set([]))) {
  const lines = readFileSync(file, 'utf8').split('\n')
  // 只看 `run:` 块：字段里的文案（`name:` / `if:`）与注释都不执行命令。
  const executable = yamlRunBlockLines(lines)
  lines.forEach((text, index) => {
    if (!executable.has(index)) return
    if (!PIP_INSTALL.test(text)) return
    if (/\bziglang\b/.test(text)) return
    report('ci-installs-python-packages', file, index + 1, text)
  })
}

// ── 规则 3：npm 构建脚本不得调用 Python ──────────────────────────────────────
const NPM_PYTHON_CALL = /\b(execFileSync|execSync|spawnSync|spawn)\s*\(\s*[`'"$]?.*\b(python|python3|pyinstaller)\b/i

for (const file of collect(join(repoRoot, 'packages'), ['.mjs', '.js'], new Set(['node_modules', 'dist']))) {
  const lines = readFileSync(file, 'utf8').split('\n')
  lines.forEach((text, index) => {
    if (!NPM_PYTHON_CALL.test(text)) return
    if (isCommentLine(text)) return
    report('npm-script-spawns-python', file, index + 1, text)
  })
}

// ── 输出 ────────────────────────────────────────────────────────────────────
if (violations.length === 0) {
  console.log('冻结基准准入检查通过：产品链路没有重新引入 Python 依赖。')
  console.log('（`omnicrawl/` 仍作为 parity 基准服务 rust/tools/gen_*.py，这是预期状态。）')
  if (devOnlyPythonUse.length > 0) {
    console.log('\n已知开发期依赖（parity 对照测试调用 Python，不影响产品运行）：')
    for (const item of devOnlyPythonUse) {
      console.log(`  · ${item.file}:${item.line}  ${item.text.slice(0, 100)}`)
    }
    console.log('  这类测试应保持「未设置 OMNICRAWL_PYTHON 时跳过」的门控。')
  }
  process.exit(0)
}

console.error(`冻结基准准入检查失败：发现 ${violations.length} 处 Python 依赖回流。\n`)
for (const item of violations) {
  console.error(`  [${item.rule}] ${item.file}:${item.line}`)
  console.error(`      ${item.text.slice(0, 160)}`)
}
console.error('\n处理方式：')
console.error('  · 产品链路确实需要 Python → 说明设计决策，不要静默绕过检查；')
console.error('  · 属于 parity 基准工具链（gen_*.py / tests/）→ 不属于本检查范围，无需处理。')
process.exit(1)
