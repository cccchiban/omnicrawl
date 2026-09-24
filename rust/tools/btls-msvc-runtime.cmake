# btls-sys（BoringSSL 的 Rust 绑定）在 Windows/MSVC 上的构建补丁。
#
# ## 解决什么问题
#
# BoringSSL 要求 CMake >= 3.22，于是 CMP0091（MSVC 运行时库属性）处于 NEW 状态。
# Visual Studio 生成器把 ASM_NASM 归入 MSVC 家族语言，而该语言的运行时库支持表是
# 空的，CMake 因此在 generate 阶段直接失败：
#
#   MSVC_RUNTIME_LIBRARY value 'MultiThreadedDLL' not known for this ASM_NASM compiler
#
# 这不是 CMake 版本新旧的问题（已用 CMake 3.29.6 复现同样错误），也不是缺 MSVC
# 工具链（VS 17 2022 能被正确找到并使用），更不是 BoringSSL 自身的缺陷。
#
# ## 为什么这样修
#
# btls-sys 自己也设置 CMAKE_MSVC_RUNTIME_LIBRARY（build/main.rs 按 crt-static 在
# MultiThreaded / MultiThreadedDLL 之间选），但它留了正规出口：只要检测到
# CMAKE_TOOLCHAIN_FILE 就立即返回，不再写任何 CMake 变量。所以这里用 toolchain file
# 接管，把该属性设为空字符串——空字符串的含义是「不设置该属性」，CMake 便不再为任何
# 语言校验运行时库取值，MSVC 会退回它自己的默认值。
#
# ## 使用限制
#
# 1. 只在 Windows/MSVC 需要。Linux 与 musl 交叉编译**不要**设置它。
# 2. 这个出口会同时跳过 btls-sys 的全部 CMake 参数装配（交叉编译开关、CC/CXX、
#    CMAKE_SYSROOT、外部工具链路径等）。交叉编译若需要该出口，必须在本文件中自行
#    补齐这些设置，否则会退化成用宿主编译器构建目标平台产物。
# 3. BoringSSL 的 x86-64 汇编由 nasm 生成，而 nasm 的 -gcv8 会对源文件路径做哈希：
#    构建目录含非 ASCII 字符（例如中文路径）时汇编会失败并报 "unable to hash file"。
#    因此本机开发请把 CARGO_TARGET_DIR 指到纯 ASCII 目录。详见 rust/docs/python-free-build.md。

set(CMAKE_MSVC_RUNTIME_LIBRARY "")
