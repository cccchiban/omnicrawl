# 纯 Rust 构建环境要求

本文件记录「从 cargo 构建整个 Rust 工作区」需要机器上具备什么，以及那些**不在**
`Cargo.toml` 里、只能由环境提供的约束。

## 1. 现状：整条链路都是纯 Rust

工作区的 HTTP 传输有两层实现：

| 链路 | 实现 | 用途 |
| --- | --- | --- |
| 主力 | `ureq` + `rustls`（纯 Rust，阻塞式） | llm、mcp、connectors、tts、image_gen |
| 指纹 | `omnicrawl-tls`（本仓库自研，纯 Rust，阻塞式） | `fetcher` 与 `web_search` 的浏览器档案 |

第二条链路要的是**真实浏览器的 TLS 指纹（JA3/JA4）与 HTTP/2 连接指纹**。它曾经用过
`wreq` + `btls` + BoringSSL，代价是整条依赖树要求 C/C++ 工具链、CMake、NASM 与 libclang，
交叉编译处处受限，而且指纹数值不透明（换个库版本线上指纹就变了）。

现在这条链路由 `crates/omnicrawl-tls` 提供：ClientHello 的扩展顺序与密码套件排列自己
构造，记录层与握手编排自己写，密码学原语交 `ring`，X.509 路径判定交 `rustls-webpki`。
档案数值来自对真实浏览器的抓包校准（见该 crate 的 `profile` 文档）。

**因此构建本工作区只需要 `cargo`。** 不需要 MSVC C++ 工具链、CMake、NASM、perl 或
libclang。

## 2. 各平台必需项

| 平台 | 需求 |
| --- | --- |
| Linux | Rust 工具链；交叉到 musl 时外加对应 target 与链接器 |
| Windows | Rust（MSVC 或 GNU 均可）；链接用系统自带的链接器 |
| macOS | Rust 与 Xcode Command Line Tools（链接器） |

`ring` 自带预生成汇编，通过 `cc` 编译，不需要 NASM。

## 3. 交叉编译

全部目标都不再受 C 工具链牵制。Linux musl 目标用 zig 提供的链接器即可：

```bash
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
                   armv7-unknown-linux-musleabihf i686-pc-windows-msvc

# zig 走 winget / 官网；cargo-zigbuild 走 cargo install
cargo zigbuild --release -p omnicrawl-cli --target x86_64-unknown-linux-musl
cargo zigbuild --release -p omnicrawl-cli --target aarch64-unknown-linux-musl
cargo zigbuild --release -p omnicrawl-cli --target armv7-unknown-linux-musleabihf

cargo build --release -p omnicrawl-cli --target i686-pc-windows-msvc
```

宿主载荷同样可以交叉，五个平台都有：x64 与 arm64 在各自的原生 runner 上构建，
ARMv7 在 x64 上直接 zig 交叉（`build-host.mjs` 自己就带 `omnicrawl-cli`，无需单独预编内核）：

```bash
node packages/cli/scripts/build-host.mjs \
  --target armv7-unknown-linux-musleabihf --platform linux-arm --zigbuild
```

## 4. 路径含非 ASCII 字符

仓库路径里有中文时，Cargo 的构建产物目录会带上这些字符。部分链接器与工具对此敏感，
遇到难以解释的链接失败时把目标目录指到纯 ASCII 路径：

```bash
export CARGO_TARGET_DIR=C:/ocl-target
```

## 5. 需要外网的验证

以下几组用例默认 `#[ignore]`，因为它们依赖外网可用性。发布前手动跑一遍：

```bash
# TLS 协议栈：真实站点连通性
cargo test -p omnicrawl-tls --test live_sites -- --ignored --test-threads=1

# 指纹传输：JA4 分族、HTTP 头与 TLS 档案同源、真实站点抓取
cargo test -p omnicrawl-host --test tls_fingerprint -- --ignored --test-threads=1
```

## 6. 历史：曾经的 BoringSSL 约束

删掉的那条链路留下的经验，记在这里避免重复踩：

- `btls-sys` 的 bindgen 需要 `LIBCLANG_PATH` 指向 `libclang.dll`；该路径被系统清理后
  `cargo check` 会在构建脚本阶段失败，错误信息看起来与代码无关。
- Windows/MSVC 下 BoringSSL 要求 CMake ≥ 3.22，使 `CMP0091` 处于 NEW 状态，而 Visual
  Studio 的 `ASM_NASM` 语言不认识 `MSVC_RUNTIME_LIBRARY`，需要在 toolchain file 里把它
  设为空串绕过。
- `cargo zigbuild` 会注入自己的 `CMAKE_TOOLCHAIN_FILE`，与上面那份互相覆盖，交叉编译
  时只能二选一。

这些约束随 `wreq`/`btls` 一起移除了。若将来要重新引入原生 TLS 库，需要重新评估。
