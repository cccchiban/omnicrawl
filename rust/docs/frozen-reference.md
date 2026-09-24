# 冻结的语义基准（`omnicrawl/`）

本文件说明 Python 侧 `omnicrawl/` 包在 Rust 迁移完成后的定位、边界与操作规则。

## 一、定位

`omnicrawl/` **不再是产品实现**，而是 **parity 语义基准**。

产品链路（内核、宿主、TUI、本地 API、MCP、连接器、TTS、扩展、工作区、会话存储、脱敏）
全部在 `rust/crates/`，由 npm 分发。仓库根的 `main.py` / `python -m omnicrawl` /
控制台脚本 `ocl` 只是 `omnicrawl/compat.py` 的薄垫片，把命令行转发给 Rust 二进制。

Python 侧只剩两个职责：

| 职责 | 消费者 | 产物 |
| --- | --- | --- |
| **期望值来源** | `rust/tools/gen_*_fixture.py` | `rust/crates/*/tests/fixtures/*_parity.json` |
| **对照基准** | `tests/`（144 个文件） | Python 侧的回归断言 |

`cargo test` 本身**不依赖 Python**：fixture 已提交进仓库，测试读的是 JSON。

## 二、三条硬规则

### 规则 1：不新增产品功能

功能缺口补齐到 `rust/crates/`，不要改 Python。

反例：为了让某个边界行为「看起来对」，在 `omnicrawl/` 里加分支——这会让两侧契约分叉，
而 Rust 侧并不知道这个新分支存在。

### 规则 2：Rust 产品代码不依赖 Python 解释器

`crates/*/src/` 不得出现 `Command::new("python")` / `pyinstaller` 一类进程调用。
CI 上由 `rust/tools/check_frozen_reference.mjs` 强制检查（`.github/workflows/frozen-check.yml`）。

`crates/*/tests/` 里的 Python 调用**允许**——那是 parity 对照测试（如
`omnicrawl-session/tests/lock_cross_process.rs` 验证 Python 持锁时内核取锁超时），
与 `gen_*.py` 同属基准工具链。这类测试必须保持「未设置 `OMNICRAWL_PYTHON` 时跳过」的门控，
以免在没装 Python 的环境里报假失败。

### 规则 3：改语义必须同步重生成 fixture

改动 `omnicrawl/` 里任何被 `gen_*.py` 读取的语义实现后，必须：

```bash
python rust/tools/gen_<对应>_fixture.py     # 重新取期望值
cd rust && cargo test -p <对应 crate>        # 确认 Rust 侧仍然一致
```

两侧都提交。只改一侧而不重跑生成脚本，会让 fixture 停留在旧契约上——测试照样全绿，
但两侧已经不一致。

## 三、`FROZEN-ALLOW` 标记

`check_frozen_reference.mjs` 通过 `FROZEN-ALLOW` 标记区分两类情况：

- **已计划的回退路径**（标了标记）：如 `packages/cli/scripts/build-host.mjs` 的
  `buildLegacyPython()`——它整段是待下线的 PyInstaller 冻结路径，产品默认路径不经过它。
- **意外回流**（没标标记）：CI 里补一个 `pip install`、构建脚本里 `python -c` 拿版本号。
  这类会在 CI 上失败。

标记的生效范围是「同一行」或「上方 8 行内的连续注释」。**不要用它绕过规则 2**——
它只用于标注已计划下线的路径，且必须写明下线条件。

## 四、尚未下线的 Python 残留

以下几项仍留在仓库里，属于待清理项而非冻结基准：

| 残留 | 位置 | 下线条件 |
| --- | --- | --- |
| Textual 全屏 UI（约 18,900 行） | `omnicrawl/ui/` + `omnicrawl/entry.py` | Rust TUI 的 Markdown 逐 token 高亮、设置面板点击、跨栏鼠标交互补齐后 |
| PyInstaller 冻结路径 | `packaging/pyinstaller/` + `buildLegacyPython()` | Rust 侧功能缺口补齐、不再需要视觉对照后 |
| 转发垫片 | `omnicrawl/compat.py` + `main.py` | 确认没有外部脚本依赖 `python main.py` 后 |
| 打包元数据 | `setup.py` / `MANIFEST.in` | 同上 |

`pyproject.toml` 的 `dependencies` 仍包含 `textual` / `fastapi` / `onnxruntime` 等——
它们被 parity 工具链间接需要（`gen_*.py` 会 import `omnicrawl.api` / `omnicrawl.tts`），
不随 Textual UI 一并移除。

## 五、验证

```bash
# 边界检查（CI 上每次推送都跑）
node rust/tools/check_frozen_reference.mjs

# 全量 parity（需要 Python 环境）
cd rust && cargo test --workspace
```
