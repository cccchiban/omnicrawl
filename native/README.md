# 原生搜索扩展（Go）

`grep`/`find` 的文本搜索核心。此前由随 wheel 分发的 ripgrep 二进制承担
（Windows/Linux/macOS 的 x86_64 与 arm64 共 5 份，解压后约 26MB）；现在改为随包
编译的 Go 原生扩展，单平台约 2.5MB，且用 abi3 稳定 ABI，一份产物覆盖 Python 3.9+。

## 组成

| 路径 | 作用 |
| --- | --- |
| `ocsearch/` | 纯 Go 搜索核心：参数解析、遍历与剪枝、逐行匹配、输出渲染 |
| `cmod/main.go` | cgo 导出层：`oc_search(JSON) -> JSON`，只做参数搬运 |
| `cmod/module.c` | CPython 侧入口：`PyInit__ocsearch`，abi3 + 释放 GIL |
| `build.py` | 构建脚本：定位 C 编译器、拼 cgo 参数、产出 `_ocsearch.pyd`/`.so` |

Go 模块零第三方依赖（只用标准库），因此离线也能构建，不需要 `go mod download`。

## 构建

```bash
# 需要 Go 1.21+ 与 cgo 可用的 C 编译器
python native/build.py --out omnicrawl/_ocsearch.pyd   # Windows
python native/build.py --out omnicrawl/_ocsearch.so    # Linux/macOS
```

`pip install .` / `python setup.py bdist_wheel` 会通过 `setup.py` 的 `build_ext`
自动调用同一份脚本。缺少 Go 或 C 编译器时构建会降级（打印 warning，不中断安装），
运行时回退到 PATH 上的 `rg`；设置 `OMNICRAWL_REQUIRE_NATIVE_SEARCH=1` 可让降级
直接失败，适合发布流水线。

C 编译器：Linux/macOS 用系统 `gcc`/`clang`；Windows 需要 MinGW-w64
（`winget install BrechtSanders.WinLibs.POSIX.UCRT`），cgo 不支持 MSVC 的 `cl.exe`。
`build.py` 会在 PATH 之外探测常见安装位置（winget 包目录、msys2、conda 等）。

## 行为对齐

Python 侧 `omnicrawl/workspace/tools.py` 仍然按 ripgrep 的参数与输出格式调用后端
（`run_search` 返回 `(stdout, 退出码)`），因此格式化、截断、落盘与安全过滤逻辑
保持不变。Go 侧实现的是项目实际用到的参数子集：

- `--json --line-number`：NDJSON 记录，`path.text`/`line_number`/`lines.text`
  都是结构化字段，非 UTF-8 行退化为 `lines.bytes`（base64），由 Python 侧替换成
  `(line is not valid UTF-8)` 占位；
- `--count --with-filename`、`--files-with-matches -m N`、`--files`；
- `--fixed-strings`、`--ignore-case`、`--hidden`、`--no-require-git`、
  `--glob`/`--glob-case-insensitive`、`--max-count`。

与 ripgrep 一致的关键行为：读取 `.gitignore`/`.ignore`/`.rgignore`（含父目录，遇到
git 仓库根停止）、忽略目录直接剪枝、`.git` 永不搜索、不跟随符号链接、二进制文件
（前 8KB 含 NUL）跳过、`^`/`$` 按行锚定。

已知差异（都不影响现有调用点）：不读取全局 gitignore 与 `.git/info/exclude`；
不支持 `--multiline`、PCRE 语法等未使用的能力；同一次调用内文件按字典序遍历
（Python 侧本来就会排序），匹配行内容与 ripgrep 一致。

`ocsearch` 包有完整单元测试：

```bash
cd native && go vet ./... && go test ./...
# 并发遍历/扫描的竞态检查（需要 cgo；Windows 上 TMP 指向含中文的路径会报
# Access is denied，改成纯 ASCII 目录即可）
CGO_ENABLED=1 go test -race ./ocsearch/
```

`go vet ./...` 能通过是因为 `module.c` 在未定义 `OCSEARCH_WITH_PYTHON` 时只编译
占位实现（无 Python 头文件也能构建），真正的扩展由 `build.py` 带上该宏构建。

## 性能

用 `python native/benchmark.py --rg <旧 wheel 里的 rg.exe>` 做同负载对比（两种后端
共用同一条 ripgrep 参数，差异只来自实现）。语料：`numpy`（中量）+ Python
`site-packages`（114966 文件 / 8.3GB，全站），12 核 Windows，取每个负载最快一次：

| 负载 | 首版 native | 优化后 native | ripgrep | native/rg | 输出字节 nat/rg |
| --- | --- | --- | --- | --- | --- |
| 仓库 json 字面量 | 0.088s | **0.039s** | 0.201s | 0.19x | 208850 / 310355 |
| 中量 正则（忽略大小写） | 0.283s | **0.170s** | 0.208s | 0.82x | 1562967 / 2195845 |
| 中量 `--count` | 0.210s | **0.125s** | 0.235s | 0.53x | 53884 / 53884 |
| 中量 `--files-with-matches` | 0.197s | **0.117s** | 0.232s | 0.50x | 52389 / 52389 |
| 全站 `--files` | 6.819s | **0.713s** | 1.560s | 0.46x | 3453776 / 3453776 |
| 全站 稀有字面量 | 10.257s | **3.862s** | 5.081s | 0.76x | 0 / 258 |
| 工具层 `grep(count)` | 0.451s | **0.118s** | 0.237s | 0.50x | — |

首版之所以慢，以及做了什么：

1. **每个条目都算一次相对路径 + 16 次 rune glob 匹配**（保护路径 glob 有 15~16 条）。
   现在把字面量 basename 规则降解成一次字符串比较，非字面量的单段规则只比条目名，
   只有含 `/` 的 glob 才维护相对路径段（项目调用里根本不会出现）→ 全站 `--files`
   6.82s → 2.22s。
2. **每个目录三次无果的 open** 找 `.gitignore`/`.ignore`/`.rgignore`。现在从
   `os.ReadDir` 的结果里判断哪些忽略文件存在，只读实际存在的。
3. **单线程遍历**是最大瓶颈（当时的 4.81x）。改成并发目录遍历（信号量限 16，
   派生分支前释放，避免子目录互等），完成后统一排序保持输出可复现 → 2.22s → 0.69s。
4. **每个文件新申请 64KB 扫描缓冲**、额外一次 8KB 探测读 + seek。现在用
   `sync.Pool` 复用缓冲、探测直接 `Peek` 已读入的数据，扫描并发提到 16。
5. **输出体积**：不再逐文件发 `begin`/`end` 记录（调用方只消费 `match`），
   加上不输出 `submatches`，命中密集时输出比 ripgrep 少约 30%，Python 侧解析
   也随之变快。
6. **正则字面前缀预筛**（`prefilter.go`）：Go 的 `regexp` 一开 `(?i)` 就用不上
   字面前缀优化，只能逐位置尝试折叠匹配。现在先从模式里安全抽出“每次匹配都必须
   出现”的字面量，在进引擎前把整行挡掉。逐模式对比（numpy / 全站）：

   | 模式 | native | ripgrep | native/rg | 预筛前 |
   | --- | --- | --- | --- | --- |
   | 忽略大小写 `config` | 0.132s | 0.210s | 0.63x | 0.195s |
   | 忽略大小写 `handleAuth\w*` | 0.124s | 0.213s | 0.58x | — |
   | 大小写敏感 `handleAuth\w*` | 0.116s | 0.251s | 0.46x | — |
   | 全站 忽略大小写 `config` | 11.405s | 12.153s | 0.94x | 1.12x |
   | 全站 忽略大小写 `def\s+test\w*\(` | 9.883s | 10.032s | 0.99x | 1.12x |

   预筛必须 sound（不能把正则能匹配的行挡掉），因此：只在最外层、不被零最小量词
   修饰的字面量段里取；顶层选择、命名分组、环视等无法安全解析的构造直接放弃；
   忽略大小写时只用“折叠等价类只含 ASCII”的字符（`k`/`s` 分别有 U+212A/U+017F
   折叠等价，用 ASCII 折叠搜索会漏匹配，这在 `prefilter_test.go` 里钉死）。
7. **Python 侧开销**：cProfile 显示工具层 44~52% 的时间花在 `nt._getfinalpathname`
   —— `relative_path()` 每条匹配都调一次 `Path.resolve()`（Windows 上单次几十
   微秒）。改成纯字符串的前缀比较（同时预存调用方传入的原始根形式，8.3 短路径/
   符号链接这类未解析输入不必再走 `resolve()`；仍不命中才回退 `os.path.relpath`），并把
   “保护路径/include/exclude 过滤”从每行一次改成每文件一次（匹配记录按文件连续
   输出）：

   | 工具层场景 | 优化前 | 优化后 | 其中后端耗时 |
   | --- | --- | --- | --- |
   | `grep` matches | 1.620s | **0.169s** | 0.113s |
   | `grep` count | 0.293s | **0.113s** | 0.093s |
   | `find` files | 0.284s | **0.100s** | 0.079s |

结构性的优势来自**没有进程启动开销**：每次 `grep` 省掉一次约 60ms 的 spawn，
小仓库场景因此快约 5 倍。

## 还能再快吗

1. **预筛的覆盖范围**：必需字面量里包含折叠不安全的短段时（如 `test` 只剩
   `te`）会放弃预筛。要再揠回来得做多 needle 搜索（ASCII 折叠 needle + `ſ`/`K`
   的 UTF-8 形式），收益局限在少数模式上。
2. **扫描吞吐**：目前是 64KB 缓冲读，没有 mmap 与 SIMD 字面量搜索。大文件
   （数 MB 以上）考虑 mmap 会有明显收益，小文件收益有限。
3. **JSON 解析**：命中密集时每条记录一次 `json.loads`（约 5µs）已是 Python 侧
   主要剩余开销；若以后不再需要兼容 rg 的 NDJSON（丢掉 PATH 回退），可以换成
   更紧凑的分隔格式。注意 `should_skip_path` 是安全边界，不要为了快把它移到
   Go 侧。
4. 并发遍历可以做成 work-stealing（现在 16 个槽已经快过 rg，收益边际）。

## 注意事项

- **GIL**：`module.c` 在调用 Go 期间释放 GIL，长搜索不会阻塞 TUI/API 线程。
- **进程内运行时**：Go 运行时与 CPython 共处一个进程。Windows 上无额外依赖
  （产物只依赖 KERNEL32/UCRT 与 `python3.dll`）；Linux/macOS 上 Go 会注册自己的
  信号处理器，这是 cgo 扩展的固有代价，如有信号相关需求需单独评估。
- **fork**：项目只用 `subprocess`（exec 语义）起子进程，不存在 fork 后带着已初始化的
  Go 运行时继续运行的场景；另外原生扩展是首次搜索时才惰性加载的。
- **平台产物**：目前只在 Windows x86_64 上验证过。Linux x86_64/arm64 与 macOS
  需在对应平台各构建一次并验证 `import omnicrawl._ocsearch`；macOS 上
  `-buildmode=c-shared` 产物按 `.so` 命名给 Python 加载。
- 单次调用的匹配结果上限仍由 Python 侧 `SEARCH_PARSE_LINE_CAP` 控制，原生侧为了
  与 ripgrep 的输出逐条对齐不做截断；如需进一步限制病态大结果的内存占用，可在
  `ocsearch.Run` 里增加上限。
