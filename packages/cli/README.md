# omnicrawl-cli

OmniCrawl 启动器：按 `platform-arch` 选包node_modules的内核二进制，把 stdio 转交给它。
它不做协议转换——宿主直接与内核进程对话（协议见 `rust/docs/protocol-v1.md`）。

## 分发形态

| npm 包 | 内容 |
| --- | --- |
| `omnicrawl-cli` | JS 启动器（`bin/omnicrawl.mjs`） |
| `@omnicrawl/cli-linux-x64` | 内核二进制（Linux x64，musl 静态） |
| `@omnicrawl/cli-linux-arm64-musl` | 内核二进制（Linux arm64，嵌入式目标） |
| `@omnicrawl/cli-win32-x64` | 内核二进制（Windows x64） |

**平台分包、`os`/`cpu` 与平台依赖只出现在发布产物里**（`dist/npm/`）。仓库内的 `packages/cli*/package.json`
刻意不带 `os`/`cpu`、也不声明平台分包依赖：npm 安装时会校验工作区成员的 `os`/`cpu`，带上就直接
`notsup` 失败。发布产物由 `scripts/prepare.mjs` 生成。

## 本地开发

```bash
cargo build --release -p omnicrawl-cli      # 产物：rust/target/release/omnicrawl[.exe]
node packages/cli/scripts/prepare.mjs       # 暂存发布产物（缺的平台只警告）
npm test -w omnicrawl-cli                  # e2e：真二进制 + 协议 v1
OMNICRAWL_BINARY=/path/to/omnicrawl node packages/cli/bin/omnicrawl.mjs --version
```

工作区里 `@omnicrawl/cli-*` 都是 workspace 成员，启动器靠 `createRequire` 解析到它们；
`OMNICRAWL_BINARY` 是直接指定二进制的逃生口（e2e 用的就是它）。e2e 需要 `rust/target/release`
里有产物，否则跳过并打印构建命令。

## 发布

1. 交叉编译三个平台（Linux 走 musl 静态，嵌入式目标优先）：

   ```bash
   cargo build --release -p omnicrawl-cli --target x86_64-pc-windows-msvc
   cargo build --release -p omnicrawl-cli --target x86_64-unknown-linux-musl
   cargo build --release -p omnicrawl-cli --target aarch64-unknown-linux-musl
   ```

2. 门槛检查（缺任何平台会失败并列出缺失平台）：

   ```bash
   node packages/cli/scripts/prepare.mjs --require-all
   ```

3. 按顺序发布暂存产物：平台包在前，启动器在后（启动器的 `optionalDependencies` 是精确版本，顺序反了会装不到）：

   ```bash
   npm publish dist/npm/cli-linux-x64
   npm publish dist/npm/cli-linux-arm64-musl
   npm publish dist/npm/cli-win32-x64
   npm publish dist/npm/cli
   ```

   脚本跑完会把这几行命令直接打出来。

4. 版本reproducibility：四个包同版本；平台包只装二进制，升级它们就是换内核。

## 发布前自检

`cargo test --workspace`、`npm test --workspaces`，以及 `prepare --require-all` 门槛。
