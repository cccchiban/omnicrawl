//! 沙盒内的 `bash`：brush（纯 Rust 的 bash/POSIX 兼容 shell）的可执行入口。
//!
//! 存在的理由与 `git_native` 同构：AppContainer 下 Git Bash 是 MSYS 程序，启动时要打开
//! `/dev/null`（即 `NUL`），而 `\Device\Null` 的 NULL DACL 对受限令牌一律拒绝，于是
//! `bash.exe` 在 DLL 初始化阶段就以 `0xC0000142` 退出——连 `--version` 都跑不出来。
//!
//! 这里换的是**执行形态**而不是放宽隔离：本程序仍是独立进程，由沙盒以容器身份 spawn，
//! 它自己 spawn 的外部命令（`cargo`、`node`、`python` …）同样继承容器令牌。绝不能改成
//! 在宿主进程内解释脚本——那会让脚本里的每条命令都以宿主权限执行，等于没有沙盒。
//!
//! 参数形状与 Git Bash 保持一致（`-o pipefail -lc <script>`），因此 `tools/command.rs`
//! 里只有「用哪个可执行文件」这一处不同。

fn main() {
    brush_shell::entry::run();
}
