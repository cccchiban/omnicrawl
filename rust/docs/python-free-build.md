# 纯 Rust 构建环境要求

本文件记录「从 cargo 构建整个 Rust 工作区」需要机器上具备什么，以及那些**不在**
`Cargo.toml` 里、只能由环境提供的约束。定位与冻结基准的关系见
[`frozen-reference.md`](./frozen-reference.md)。

## 1. 为什么会有 C/C++ 工具链要求

工作区的 HTTP 传输有两条链路：

| 链路 | 实现 | 用途 |
| --- | --- | --- |
| 主力 | `ureq` + `rustls`（纯 Rust，阻塞式） | llm、mcp、connectors、tts、web_search |
| 指纹 | `wreq` + `btls` + BoringSSL（C/C++，异步） | 仅 `fetcher` 工具的 `impersonate` |

第二条链路为了做**真实的浏览器 TLS 指纹（JA3/JA4）与 HTTP/2 指纹匹配**，静态链接
BoringSSL，因此构建期需要 C/C++、汇编器与 CMake。这是唯一引入 C 工具链依赖的地方，
无法用纯 Rust 替代（`rustls` 写死了扩展顺序，改密码套件列表也只能覆盖 JA3 的一个字段）。

因此：**只构建与 llm/mcp 等链路相关的 crate 时不需要 C 工具链；一旦构建
`omnicrawl-host`（及其上层 `omnicrawl-tui` / `omnicrawl-api` / `omnicrawl-cli`），就必须要。**

## 2. 各平台必需项

Linux（BoringSSL 官方列出的前置，也是 CI 安装的集合）：

```bash
sudo apt-get install -y build-essential cmake perl pkg-config libclang-dev git musl-tools
```

Windows：

| 需求 | 说明 |
| --- | --- |
| MSVC 工具链 | Visual Studio 2022（含 C++ 桌面开发） |
| CMake | 随 VS 提供，无需单独装 |
| NASM | BoringSSL 生成 x86-64 汇编；可在 VS 组件里勾选，或单独装 |
| libclang / LLVM | `btls-sys` 用 bindgen 生成 Rust 绑定，需要 `libclang.dll` |
| `LIBCLANG_PATH` | 指向 `libclang.dll` 所在目录，例如 `C:/Program Files/LLVM/bin` |
| `CMAKE_TOOLCHAIN_FILE` | 指向 `rust/tools/btls-msvc-runtime.cmake`（见下节） |

macOS：`cmake`、`perl` 与 Xcode Command Line Tools；libclang 走 `LIBCLANG_PATH`。

### 从一台机器交叉编译全部平台（本机验证用）

内核（`omnicrawl-cli`）不依赖 `btls`，因此它的交叉编译不需要 C/C++/CMake/BoringSSL，只需要目标与链接器：

```bash
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
                   armv7-unknown-linux-musleabihf i686-pc-windows-msvc

# Windows 上没有 Linux 工具链时，用 zig 自带的交叉 C 编译器与链接器：
#   zig 走 winget / 官网；cargo-zigbuild 走 cargo install
cargo zigbuild --release -p omnicrawl-cli --target x86_64-unknown-linux-musl
cargo zigbuild --release -p omnicrawl-cli --target aarch64-unknown-linux-musl
cargo zigbuild --release -p omnicrawl-cli --target armv7-unknown-linux-musleabihf

# i686 不需要 btls，也不需要目标 C 编译器（用本机 32 位 MSVC 工具集链接）：
cargo build --release -p omnicrawl-cli --target i686-pc-windows-msvc
```

本机实测版本：`zig 0.16.0`、`cargo-zigbuild`（`cargo-zigbuild --help` 自报版本）、
`rustc 1.96.1`。宿主载荷（`omnicrawl-tui` / `omnicrawl-api`）要 BoringSSL，
它本身能交叉编译；原先卡住整份 musl 载荷的 `ort-sys` 已经通过把本地推理改成可选 feature 解决
（详见第 7 节）。

## 3. Windows 专属：必须设 `CMAKE_TOOLCHAIN_FILE`

```bash
export CMAKE_TOOLCHAIN_FILE="$PWD/rust/tools/btls-msvc-runtime.cmake"
```

不设它时构建会在 CMake 的 generate 阶段失败：

```text
MSVC_RUNTIME_LIBRARY value 'MultiThreadedDLL' not known for this ASM_NASM compiler
```

原因是 BoringSSL 要求 CMake ≥ 3.22，使 CMP0091 处于 NEW 状态，而 Visual Studio
生成器把 `ASM_NASM` 归入 MSVC 家族语言、该语言的运行时库支持表为空。这与 CMake
版本新旧无关（3.29.6 复现同样错误），也不是缺 MSVC 工具链。

`rust/tools/btls-msvc-runtime.cmake` 里把 `CMAKE_MSVC_RUNTIME_LIBRARY` 设为空字符串
（含义是「不设置该属性」）即可绕过。**该文件只用于 Windows/MSVC**：它会让 `btls-sys`
跳过自己全部的 CMake 参数装配（含交叉编译设置），在 Linux 上设置会破坏 musl 交叉编译。

dev（`cargo check` / `cargo test`）构建建议**再导出一次目标专用变量名**：

```bash
export CMAKE_TOOLCHAIN_FILE="$PWD/rust/tools/btls-msvc-runtime.cmake"
export CMAKE_TOOLCHAIN_FILE_x86_64_pc_windows_msvc="$PWD/rust/tools/btls-msvc-runtime.cmake"
```

`btls-sys` 判断「有没有外部 toolchain 文件」时先读目标专用名（`CMAKE_TOOLCHAIN_FILE_<target>`，
连字符转下划线）再读通用名；只导出通用名时，实测 dev 配置下它仍会把
`-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreadedDLL` 传给 CMake，又在 ASM_NASM 上撞回上面那个报错
（同一命令加上目标专用名就通过，构建目录里的 `CMakeCache.txt` 会多出 `CMAKE_MSVC_RUNTIME_LIBRARY`
条目）。release 构建不受影响，所以 CI 只给通用名也能过。

## 4. 本机开发：构建目录必须是纯 ASCII 路径

BoringSSL 的 x86-64 汇编由 `nasm` 处理，MSBuild 调用它时会带 `-gcv8`。该选项会对
源文件路径做哈希，遇到非 ASCII 路径（例如中文目录名）直接失败：

```text
error: unable to hash file D:\...\Python?????\...
```

因此**在本机（仓库路径含中文时）构建时把目标目录指到纯 ASCII 路径**，例如：

```bash
export CARGO_TARGET_DIR=C:/ocl-target
```

CI 的检出路径（`D:\a\...` / `/home/runner/work/...`）是纯 ASCII，不受影响。
也可以用 `rust/.cargo/config.toml` 的 `[build] target-dir` 固定，但那会影响所有人，
所以本项目选择用环境变量而非提交配置文件。

## 5. 一次完整验证

```bash
export CARGO_TARGET_DIR=C:/ocl-target                                  # 仅 Windows 本机需要
export CMAKE_TOOLCHAIN_FILE="$PWD/rust/tools/btls-msvc-runtime.cmake"  # 仅 Windows 需要
export LIBCLANG_PATH="C:/Program Files/LLVM/bin"                       # 仅 Windows 需要

cd rust
cargo test -p omnicrawl-host
```

（若 `cargo check` / `cargo test` 在 `btls-sys` 上报 ASM_NASM 那个错，再导出一次目标专用名，
见第 3 节。）

外网指纹冒烟（默认被 `#[ignore]`，需要外网）：

```bash
cargo test -p omnicrawl-host --test wreq_fingerprint -- --ignored --nocapture
```

预期输出两族不同的 JA4 指纹，且协议段为 `h2`（浏览器档案）而非 `h1`（库指纹）。

**使用 `CARGO_TARGET_DIR` 会让三个用例受环境影响**：它们通过
`std::env::current_exe()` 向上回溯仓库结构（例如 `prompt::tests::templates_dir_prefers_rust_assets`
找 `rust/assets/templates`）。目标目录一旦移出仓库，这些用例会失败——这是环境副作用，
不是回归，判断时要先确认。

## 6. 验证状态

本地（Windows）已实测的面：

| 面 | 结果 |
| --- | --- |
| 内核交叉编译 `x86_64-unknown-linux-musl` | ✅ `cargo zigbuild --release -p omnicrawl-cli` 通过；静态链接 ELF x86-64 |
| 内核交叉编译 `aarch64-unknown-linux-musl` | ✅ 同上；静态链接 ELF ARM aarch64 |
| 内核交叉编译 `armv7-unknown-linux-musleabihf` | ✅ 同上；静态链接 ELF ARM EABI5（与 CI 同一条 zig 路径） |
| 内核编译 `i686-pc-windows-msvc` | ✅ `cargo build --release` 通过；PE32 i386（用本机 VS2022 BuildTools 的 32 位工具集） |
| Windows x64 宿主载荷（含 BoringSSL） | ✅ 五个包 `--release` 一起构建通过（`btls` / `tokio-btls` / `wreq` / `omnicrawl-host` 全部重编），5 分 58 秒 |

发布体积（`--release`，`lto="thin"` + `codegen-units=1` + `panic="abort"`）：

| 产物 | 目标 | 大小 |
| --- | --- | --- |
| `omnicrawl` 内核 | `x86_64-unknown-linux-musl` | 7.0 MiB（7,298,712 B） |
| `omnicrawl` 内核 | `aarch64-unknown-linux-musl` | 5.8 MiB（6,116,808 B） |
| `omnicrawl` 内核 | `armv7-unknown-linux-musleabihf` | 5.7 MiB（5,933,008 B） |
| `omnicrawl` 内核 | `i686-pc-windows-msvc` | 5.5 MiB（5,774,336 B） |
| `omnicrawl` 内核 | `x86_64-pc-windows-msvc` | 6.6 MiB（6,908,928 B） |
| `omnicrawl-tui` | `x86_64-pc-windows-msvc` | 52.6 MiB（55,159,808 B） |
| `omnicrawl-api` | `x86_64-pc-windows-msvc` | 36.6 MiB（38,398,976 B） |
| `omnicrawl-host` | `x86_64-pc-windows-msvc` | 2.8 MiB（2,900,992 B） |
| `omnicrawl-mcp-server` | `x86_64-pc-windows-msvc` | 0.6 MiB（607,744 B） |

Windows x64 宿主载荷五项合计约 104 MiB（`.text` + `.rdata` 各占一半，exe 里没有 debug 段；
调试信息在伴随的 `.pdb` 里，不在上传清单内）。内核单发约 6–7 MiB，跨平台差异不大。
冷编译时间：单个 musl 内核约 1 分 15 秒（依赖从零编，含 `ring` 的 C 代码），Windows 全套宿主载荷约 6 分钟。

仍未验证的面：

| 面 | 状态 |
| --- | --- |
| Linux musl **完整宿主载荷** | 卡在依赖上，不是卡在工具链：见下一节「发布产物的目标选择」 |
| 运行期性能 | 未测。仓内没有基准套件，也没有可比对的 Python 基线口径；只说体积，不谈吞吐 |
| Linux 上的 `cargo test --workspace` | 未验证（本机只能跑 Windows 目标；WSL2 因缺 Hyper-V/虚拟机平台不可用） |

交叉编译的坑（已写进 CI，记下来避免重踩）：

- **musl 目标的 C 编译器要看 cc-rs 的候选表**：`x86_64-unknown-linux-musl` 在 cc-rs 里走
  `find_working_gnu_prefix(["x86_64-linux-musl", "musl"])`，会在 PATH 里先找
  `x86_64-linux-musl-gcc`、再找 `musl-gcc`，所以 `musl-tools` 的 `musl-gcc` 对它是够的；
  但 `aarch64-unknown-linux-musl` 是硬编码前缀，要的是 `aarch64-linux-musl-gcc`，
  `musl-tools` 不提供。两者都改用 `cargo zigbuild` 以后差异消失。
- **`ring` 的构建脚本要真实的 C/汇编编译器**：本机（Windows，无 Linux 工具链）直接
  `cargo build --target x86_64-unknown-linux-musl` 会报 `failed to find tool "x86_64-linux-musl-gcc"`，
  `cargo zigbuild` 等价路径则通过（zig 自带 clang 与链接器驱动）。
- **`btls-msvc-runtime.cmake` 只在 MSVC 构建需要**：Linux 目标千万不要设 `CMAKE_TOOLCHAIN_FILE`，
  它会让 `btls-sys` 跳过自己的交叉编译装配。反向也成立：从 Windows 交叉到 musl 时不能设它，
  否则会盖掉 cargo-zigbuild 自己生成的 CMake toolchain；但 `LIBCLANG_PATH` 仍要设，
  因为 bindgen 在**宿主**上跑。
- 已顺带修掉一个非 Windows 目标的 `unused_imports` 警告（`omnicrawl-controllers` 的
  `turn::environment` 里进程链缓存只在 `cfg(windows)` 分支使用），否则 CI 在 Linux/musl 上会带警告构建。
- 本机 Node 22.22.1 的 `fs.cpSync(…, {recursive:true})` 会**静默硬退（退出码 127）**，
  而工作流钉的 22.14.0 正常（用 `npx node@22.14.0` 对比确认）。`build-host.mjs` / `prepare.mjs`
  大量用 `cpSync`，本地复现发布流程要挑对 Node 版本。

## 7. 发布产物的目标选择

内核（`omnicrawl-cli`）与宿主载荷（`omnicrawl-tui` / `omnicrawl-api` / `omnicrawl-host` / `omnicrawl-mcp-server`）
在 CI 里是**两个不同的目标选择**：

- **内核**：一律按矩阵里的三元组交叉（Linux 侧三个都是 musl，静态链接）。上游依赖只有 `ring`
  这类 C 代码，zig 能完整处理；本机已逐个实测通过。
- **宿主载荷**：Windows 用 `--target x86_64-pc-windows-msvc`（与内核同一个
  `target/<三元组>/release/`）；**Linux 侧不带 `--target`**，按构建机原生 gnu 目标构建，
  因此发布包依赖 runner 的 glibc（Ubuntu 22.04 → glibc 2.35）。

Linux 宿主载荷为什么不跟着走 musl：`omnicrawl-tts` 原本硬依赖 `ort`（ONNX Runtime 绑定），
而 `ort-sys` 只提供部分目标的预编译库，musl 不在其中：

```text
error: ort-sys@2.0.0-rc.13: no prebuilt binaries available for target x86_64-unknown-linux-musl
```

（BoringSSL 那一段能过——`libcrypto.a` / `libssl.a` 都正常链接出来——所以这不是 BoringSSL
的交叉编译能力问题。）现在 TTS 改成以**接口合成为主、本地 ONNX 为可选 feature**
（`omnicrawl-tts` 的 `onnx`，默认关闭），`ort` 因此退出了默认依赖树，musl 宿主载荷可以构建。
如果将来要让某个平台带本地推理，可选的下一步是换 `ort` 的 `load-dynamic` 模式
（运行期找 `libonnxruntime.so`，代价：目标机器要自己装）或自建 ORT 的 musl 静态库
（官方不保证支持 musl）。

`build-host.mjs` 已经支持 `--target <三元组>` 与 `--zigbuild`（后者换成 `cargo zigbuild`，
并且必须与 `--target` 同时出现）。**现在 Linux 那两个 job 可以直接用
`--target ${{ matrix.target }} --zigbuild` 发 musl 宿主载荷**（本机已实测，见下表）；
在此之前 CI 只能用原生 gnu。

### 本机实测（Windows 主机交叉到 x86_64-unknown-linux-musl）

```bash
cd rust   # 注意：CARGO_TARGET_DIR 不要设，产物要落到 target/<三元组>/release/
LIBCLANG_PATH=<libclang 目录> cargo zigbuild --release \
  -p omnicrawl-entry -p omnicrawl-cli -p omnicrawl-tui -p omnicrawl-api -p omnicrawl-mcp \
  --target x86_64-unknown-linux-musl
```

全部静态链接，均无警告：

| 产物 | 字节 | 类型 |
| --- | --- | --- |
| `omnicrawl`（内核） | 7,492,464 | ELF x86-64 静态链接 |
| `omnicrawl-host` | 3,247,072 | 同上 |
| `omnicrawl-tui` | 32,606,520 | 同上 |
| `omnicrawl-api` | 15,848,760 | 同上 |
| `omnicrawl-mcp-server` | 846,960 | 同上 |

耗时：首次（含 BoringSSL 全量交叉编译）3 分 08 秒，增量 1 分 35 秒。
对照：Windows x64 宿主载荷是 104 MiB，musl 反而更小（32.6 MiB 的 TUI vs 55 MiB）——
ELF 不带 PDB，且 `strip` 后不保留符号表。

两个只有在非 Windows 目标上才会暴露的编译问题（已修，走 musl 载荷必须过这两关）：

- `crates/omnicrawl-host/src/tools/windows/mod.rs` 的 `#[cfg(not(windows))]` 占位函数把返回类型
  写成了 `super::screenshot::ScreenshotOutcome`，在非 Windows 上没有任何 `tools::screenshot` 模块，
  直接 E0433；现在该目标下自己定义一个同形状的 `ScreenshotOutcome`。
- `crates/omnicrawl-host/src/tools/web_transport.rs` 无条件 `use std::process::Command`，
  而它只被 Windows 的注册表读代理用到，非 Windows 上是未使用导入警告。

历史上还有一个真实缺陷（已修）：`build-host.mjs` 用了 `readFileSync` 却没从 `node:fs` 引入，
任何平台都会在载荷拷完、写 `host-meta.json` 之前以
`ReferenceError: readFileSync is not defined` 退出，`prepare.mjs` 会因此认为缺宿主载荷。

若将来 BoringSSL 在某个目标上真的编不出来，回退方案仍是把 `impersonate` 降级为
「显式报错说明该平台不支持」（对齐 Python 未安装 `curl_cffi` 时的行为），
而不是静默退回库指纹。
