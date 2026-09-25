# omnicrawl-cli

OmniCrawl 启动器：按 `platform-arch` 选平台分包，默认启动**完整宿主**（工作台 / `api` / 插件 CLI）；
`kernel` 子命令把 stdio 交给内核二进制（协议调试与 e2e）。

## 分发形态

| npm 包 | 内容 |
| --- | --- |
| `omnicrawl-cli` | JS 启动器（`bin/omnicrawl.mjs`） |
| `@omnicrawl/cli-win32-x64` | 内核二进制 + 宿主载荷（Windows x64） |
| `@omnicrawl/cli-linux-x64` | 内核二进制 + 宿主载荷（Linux x64，内核为 musl 静态） |
| `@omnicrawl/cli-linux-arm64-musl` | 内核二进制 + 宿主载荷（Linux arm64） |
| `@omnicrawl/cli-win32-ia32` | 仅内核（32 位 Windows 缺运行期轮子） |
| `@omnicrawl/cli-linux-arm-musl` | 仅内核（armv7 嵌入式目标） |

宿主载荷是 Rust 二进制（`host/`）：`omnicrawl-host` / `omnicrawl-tui` / `omnicrawl-api` / `omnicrawl-mcp-server` 与内核同架构，`host-meta.json` 里 `hostKind: "rust"`；
用户机器不需要 Python 或 pip。没有宿主载荷的平台由启动器退回内核直连，并打印这一边界。

**平台分包、`os`/`cpu` 与平台依赖只出现在发布产物里**（`dist/npm/`）。仓库内的 `packages/cli*/package.json`
刻意不带 `os`/`cpu`、也不声明平台分包依赖：npm 安装时会校验工作区成员的 `os`/`cpu`，带上就直接
`notsup` 失败。发布产物由 `scripts/prepare.mjs` 生成。

## 命令面

```
omnicrawl                 启动工作台（TUI）
omnicrawl api             启动本地 HTTP 服务（无头/服务器场景）
omnicrawl plugin ...      插件管理
omnicrawl kernel ...      直连内核进程（协议调试）
omnicrawl --version       打印启动器、平台包、宿主与内核版本
```

无交互式终端时宿主会直接给出改用 `api` 的提示并以 2 退出，不会停在协议等待状态。

## 启动自动更新

默认工作台路径（无子命令）启动时，启动器会检查 npm registry 上 `omnicrawl-cli` 的最新版：
落后就执行 `npm install -g omnicrawl-cli@<latest>`，成功后用原参数重新拉起启动器，
沿用新进程的退出码。语义与旧 Python 的 `maintenance/updater.py` 一路对齐，只是版本源换成
npm registry、升级方式换成 `npm install -g`。

- 版本结果缓存 24 小时（`~/.OmniCrawl/npm-version-check.json`），过期才请求 registry（3 秒超时）。
- `[update].enabled = false`（`~/.OmniCrawl/config.toml`）可关闭；缺省开启。
- 护栏：源码检出（祖先目录含 `.git`）、`OMNICRAWL_HOST` / `OMNICRAWL_BINARY` 逃生口、
  已知子命令（`api` / `kernel` / `plugin`）都不做更新检查。
- `OMNICRAWL_SKIP_AUTO_UPDATE=1` 彻底关闭；升级尝试后写 `OMNICRAWL_AUTO_UPDATE_ATTEMPTED=1`
  让重启的子进程不再重复尝试，避免升级失败时无限重启。
- 任何失败（离线、npm 不可用、安装报错）只打印原因与手动升级命令，继续用当前版本启动。

## 本地开发

```bash
cargo build --release -p omnicrawl-cli        # 产物：rust/target/release/omnicrawl[.exe]
node packages/cli/scripts/build-host.mjs      # 宿主载荷：dist/host/<platform>/payload
node packages/cli/scripts/build-host.mjs --target x86_64-pc-windows-msvc  # 产物目录跟 --target 走
node packages/cli/scripts/prepare.mjs         # 暂存发布产物（缺的平台只警告）
node packages/cli/scripts/host-smoke.mjs      # 宿主冒烟：无终端提示 + 退出码
node packages/cli/scripts/install-smoke.mjs   # 安装冒烟：干净前缀里真装真跑
npm test -w omnicrawl-cli                     # e2e：真二进制 + 协议 v1
```

宿主载荷是纯 Rust 产物，按构建机的目标构建即可（`omnicrawl-tts` 的 ONNX 运行时已改成可选
feature，所以 musl 静态宿主载荷在 Windows 上交叉编译也能过，本机实测见
`rust/docs/python-free-build.md` 第 7 节）：Windows 侧用 `--target` 指到与内核相同的三元组目录，
Linux 侧目前仍按 runner 原生 gnu 目标构建（aarch64 musl 宿主在 arm runner 上还没验证过）。
若目标无法在构建机上构建（或宿主没接线），启动器会退回协议直连内核——内核可以单独交叉编译成
musl 静态。

平台包里的内核二进制声明成 `omnicrawl-kernel`，**不能**叫 `omnicrawl`：启动器自己也声明了
`omnicrawl`，而平台包在安装树里同样会生成 `node_modules/.bin` 条目，撞名会让平台包抢走
`.bin/omnicrawl`，本地安装与 `npx` 下 `omnicrawl` 就变成直连内核（只打印 `omnicrawl 0.0.1`）。
安装冒烟里有断言守住 `.bin/omnicrawl` 必须指向启动器。

两个逃生口：`OMNICRAWL_HOST` 直接指定宿主、`OMNICRAWL_BINARY` 直接指定内核（此时启动器是纯透传）。

## 发布

1. 升 `packages/cli/package.json` 版本（平台包与启动器同版本）。
2. 推 `npm-v<版本>` tag（CI 会校验 tag 与包版本一致）或手动触发 workflow：
   矩阵各平台构建内核（+ 宿主）→ 宿主冒烟 → 暂存并上传产物；
   发布任务下载全部产物 → 暂存启动器 → 安装冒烟 → 平台包在前、启动器在后依次发布。

发布前自检：`cargo test --workspace`、`npm test --workspaces`、`prepare.mjs --require-all --require-host`。
