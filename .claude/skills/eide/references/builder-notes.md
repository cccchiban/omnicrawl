# EIDE unify_builder 参考

## unify_builder CLI

`unify_builder` 是 EIDE 扩展的统一构建后端，位于 VS Code 扩展目录下：

```
<vscode-extensions>/cl.eide-<version>/res/tools/win32/unify_builder/
```

### 命令

| 命令 | 说明 |
|------|------|
| `build` | 增量编译 |
| `rebuild` | 全量重建 |
| `clean` | 清理构建产物 |

### 参数

| 参数 | 说明 |
|------|------|
| `--params <path>` | builder.params 文件路径（必需） |

### builder.params

`builder.params` 是由 EIDE 从 `eide.yml` 自动生成的 JSON 文件，包含完整构建配置：

- `name`：项目名称
- `target`：配置名称（ConfigName）
- `toolchain`：工具链类型（AC5/AC6/GCC）
- `toolchainLocation`：工具链路径
- `sourceList`：源文件列表
- `incDirs`：include 路径
- `defines`：预定义宏
- `options`：编译/链接选项
- `env`：环境变量（含产物输出路径等）
- `dumpPath` / `outDir`：构建输出目录

### 构建产物

构建产物输出到 `build/<ConfigName>/` 目录：

| 文件 | 说明 |
|------|------|
| `<ProjectName>.axf` | Keil 兼容调试文件 |
| `<ProjectName>.elf` | ELF 可执行文件 |
| `<ProjectName>.hex` | Intel HEX 烧录文件 |
| `<ProjectName>.bin` | 二进制烧录文件 |
| `<ProjectName>.s19` | Motorola S19 格式 |
| `<ProjectName>.map` | 链接 Map 文件 |
| `compiler.log` | 编译日志 |
| `unify_builder.log` | 构建器日志 |
| `builder.params` | 构建参数 |

### 常见编译错误

| 错误类型 | 可能原因 |
|----------|----------|
| `error: A1023E: missing "{" after #include` | ARM 汇编文件使用了 C 风格 include |
| `error: L6218E: Undefined symbol` | 链接阶段缺少源文件或库 |
| `Fatal error: L6002U: Could not open file` | 链接脚本路径不存在 |
| `No such file or directory` | include 路径或源文件路径不正确 |
| `command not found` | 工具链未安装或路径配置错误 |

### 与 UV4 的关系

EIDE 项目可与 Keil MDK 项目共享同一套 ARM CC 工具链。两者产出的 `.axf` 和 `.hex` 文件格式完全兼容：

- EIDE `builder.params` 中的 `toolchainLocation` 指向 Keil 安装目录下的 ARM CC
- `env.KEIL_OUTPUT_DIR` 环境变量指明了 Keil 期望的输出目录
- 链接脚本（scatter file）与 Keil 工程共用同一份
