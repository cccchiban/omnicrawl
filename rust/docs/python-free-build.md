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

外网指纹冒烟（默认被 `#[ignore]`，需要外网）：

```bash
cargo test -p omnicrawl-host --test wreq_fingerprint -- --ignored --nocapture
```

预期输出两族不同的 JA4 指纹，且协议段为 `h2`（浏览器档案）而非 `h1`（库指纹）。

**使用 `CARGO_TARGET_DIR` 会让三个用例受环境影响**：它们通过
`std::env::current_exe()` 向上回溯仓库结构（例如 `prompt::tests::templates_dir_prefers_rust_assets`
找 `rust/assets/templates`）。目标目录一旦移出仓库，这些用例会失败——这是环境副作用，
不是回归，判断时要先确认。

## 6. 尚未验证的面

| 面 | 状态 |
| --- | --- |
| Linux x86_64/arm64 musl 静态链接含 BoringSSL | 未在本机验证（无 Linux 与交叉工具链） |
| armv7 `cargo-zigbuild` 交叉编译 BoringSSL | 未验证；`btls-sys` 从 `CC`/`CXX` 取编译器，需确认 zig 的交叉编译器能被它识别 |
| 32 位 Windows（`i686-pc-windows-msvc`） | 未验证 BoringSSL 是否支持该目标 |
| 发布体积与运行期性能影响 | 未测量；debug 链路冷编译约 4 分钟 |

这些只能在 CI 上首次跑到时得到结论。若交叉编译失败且无法在合理成本内修复，回退方案是
把 `impersonate` 降级为「显式报错说明该平台不支持」（对齐 Python 未安装 `curl_cffi`
时的行为），而不是静默退回库指纹。
