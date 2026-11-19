use std::path::{Path, PathBuf};

use super::base::*;
use crate::sandbox::runtime::RuntimeRoot;

/// 重建一份最小环境：**不继承宿主环境**，避免把 `OMNICRAWL_*` 之类的凭据带进沙盒。
///
/// `TEMP`/`TMP`/`USERPROFILE`/`HOME` 全部改指向沙盒专用目录：否则子进程往真实 `%TEMP%`
/// 写临时文件会 ACL 被拒，而报错位置离真正原因很远。
pub(super) fn debug_environment(temp_dir: &Path, workspace: &Path) -> Vec<u16> {
    build_environment_with(temp_dir, workspace, &[], None)
}

pub(super) fn build_environment(
    temp_dir: &Path,
    workspace: &Path,
    runtimes: &[RuntimeRoot],
) -> Vec<u16> {
    build_environment_with(temp_dir, workspace, runtimes, None)
}

/// 宿主的 `RUSTUP_HOME`：显式设置了就用它，否则回落到 `%USERPROFILE%\.rustup`。
///
/// 只返回**确实存在**的目录：沙盒里给它一个不存在的路径，`cargo` 会报
/// 「no default toolchain」，比不给更容易误导（有值但里面是空的）。
fn rustup_home() -> Option<String> {
    let explicit = std::env::var("RUSTUP_HOME")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let path = match explicit {
        Some(value) => PathBuf::from(value),
        None => {
            let profile = std::env::var("USERPROFILE")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| std::env::var("HOME").ok())?;
            PathBuf::from(profile).join(".rustup")
        }
    };
    path.is_dir().then(|| path.to_string_lossy().to_string())
}

/// 同 [`build_environment`]，但可注入出网代理。
///
/// `proxy` 是 `http://127.0.0.1:<port>`：注入 `HTTP_PROXY`/`HTTPS_PROXY` 后，
/// `curl`/`pip`/`npm`/`git http(s)`/`cargo`/`go mod` 都会走宿主代理，
/// 由它按 `[sandbox.net]` 判定。**不给值时也会清空代理变量**——不能让容器
/// 继承宿主的代理设置，否则出网绕过我们的判定。
pub(super) fn build_environment_with(
    temp_dir: &Path,
    workspace: &Path,
    runtimes: &[RuntimeRoot],
    proxy: Option<&str>,
) -> Vec<u16> {
    let temp = temp_dir.to_string_lossy().to_string();
    let home = temp_dir.join("home").to_string_lossy().to_string();
    let workspace = workspace.to_string_lossy().to_string();

    let mut vars: Vec<(String, String)> = Vec::new();
    let host = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    };
    for name in [
        "PATHEXT",
        "SYSTEMROOT",
        "SYSTEMDRIVE",
        "WINDIR",
        "COMSPEC",
        "PROGRAMFILES",
        "PROGRAMFILES(X86)",
        "PROGRAMW6432",
        "PROGRAMDATA",
        "NUMBER_OF_PROCESSORS",
        "PROCESSOR_ARCHITECTURE",
        "OS",
        // 这一项**不能少**：AppContainer profile 落在 `%LOCALAPPDATA%\Packages`，
        // `CreateProcessW` 从调用方环境解析它；缺了就报 `ERROR_ENVVAR_NOT_FOUND`(203)，
        // 一个完全不指向真实原因的错误码（设计文档 §16 记录了这次排查）。
        "LOCALAPPDATA",
        "APPDATA",
        // PowerShell 靠它找 cmdlet 与模块。**不能原样继承**：宿主的值里常含用户目录下的
        // 路径（沙盒读不到），而 pwsh 在模块目录都解析不了时会连内置 cmdlet 都加载失败。
        // 处理见下方 `sandboxed_module_path`。
        "USERNAME",
        "USERDOMAIN",
        // 控制台代码页与区域：缺了之后中文输出变成乱码，`whoami` 之类的系统程序
        // 还会在 DLL 初始化阶段以 0xC0000142 退出。
        // `windir` 已在上面（`WINDIR`）列过一次：Windows 变量名**不区分大小写**，
        // 重复项会让 PowerShell 的 `Env:` 驱动器报「已添加了具有相同键的项」，
        // 于是 `$env:FOO` 与 `Get-ChildItem Env:` 全部失效。
        "COMPUTERNAME",
        "PROCESSOR_IDENTIFIER",
        "PROCESSOR_LEVEL",
        "PROCESSOR_REVISION",
        "TZ",
    ] {
        if let Some(value) = host(name) {
            vars.push((name.to_string(), value));
        }
    }

    vars.push(("TEMP".to_string(), temp.clone()));
    vars.push(("TMP".to_string(), temp));
    // `USERPROFILE` 指向沙盒内的家目录：真实用户目录读不到，留着真实路径只会让程序
    // 拿到一个打不开的位置后行为诡异（`whoami` 曾因此以 0xC0000142 崩溃）。
    vars.push(("USERPROFILE".to_string(), home.clone()));
    vars.push(("HOME".to_string(), home));
    vars.push(("HOMEDRIVE".to_string(), "C:".to_string()));
    vars.push(("HOMEPATH".to_string(), "\\".to_string()));
    // PowerShell 的 `~\` 与部分工具从这两个变量取当前目录。
    vars.push(("PWD".to_string(), workspace.clone()));
    vars.push(("OMNICRAWL_SANDBOX".to_string(), "1".to_string()));
    // Rust：`HOME`/`USERPROFILE` 已改指向沙盒内的空家目录，而 rustup 的 shim
    // （`~/.cargo/bin/cargo.exe`）靠 `USERPROFILE` 找 `~/.rustup`。不显式给
    // `RUSTUP_HOME` 就必然报「rustup could not choose a version to run, because one
    // wasn't specified explicitly, and no default is configured」——这条报错完全
    // 不指向「家目录被换掉了」，实测最容易误判成权限问题。
    //
    // 只给 `RUSTUP_HOME`（toolchain 定位），`CARGO_HOME` 另指沙盒内缓存（见下）：
    // `cargo` 只是 shim，真正的编译驱动在 `~/.rustup/toolchains/<tc>/bin` 下，
    // 那两处都按只读授权。
    if let Some(rustup_home) = rustup_home() {
        vars.push(("RUSTUP_HOME".to_string(), rustup_home));
    }
    // 沙盒进程的输出不走控制台，而是管道；代码页仍是 OEM（中文系统上是 936），
    // 按 UTF-8 解会是乱码。`PYTHONUTF8` 让 Python 自己吐 UTF-8，其余程序由
    // `tools/command.rs` 在读取时按 OEM 代码页解码。
    vars.push(("PYTHONUTF8".to_string(), "1".to_string()));
    vars.push(("LANG".to_string(), "C.UTF-8".to_string()));
    vars.push(("LC_ALL".to_string(), "C.UTF-8".to_string()));
    vars.push(("PYTHONIOENCODING".to_string(), "utf-8".to_string()));
    // `PSModulePath` 必须补上 pwsh 自己的模块目录：PowerShell 7 的内置 cmdlet 是模块
    // （`Microsoft.PowerShell.*`），只在自己安装目录的 `Modules` 下找。宿主的值里那一项
    // 可能指向用户目录（容器读不到），于是 pwsh 起来了却连 `Write-Output` 都不认——
    // 症状是「脚本里每个命令都不存在」，与权限错误毫无相似之处（真机实测）。
    vars.push((
        "PSModulePath".to_string(),
        sandboxed_module_path(&powershell_module_roots()),
    ));
    // `PATH` 必须按**容器内实际可读**重建，不能继承宿主那份。
    // 宿主 PATH 里混着大量用户目录（`.cargo/bin`、`WindowsApps`、各类 SDK），容器读不到；
    // 更要紧的是 Windows 的「当前目录 + 每个 PATH 项」查找语义：只要排在前面的项不可访问，
    // `cmd` 起 `npm.cmd` 这类包装脚本就会在解析阶段失败，而报错说的是"不是内部或外部命令"，
    // 完全不指向真实原因（实测踩过）。
    vars.push(("PATH".to_string(), sandboxed_path(&workspace, runtimes)));

    // 缓存目录必须在可写范围内，否则 npm/pip 每次都要重下（设计 §14.3）。
    vars.push((
        "npm_config_cache".to_string(),
        format!("{workspace}\\.omnicrawl\\.agent_tmp\\npm"),
    ));
    vars.push((
        "PIP_CACHE_DIR".to_string(),
        format!("{workspace}\\.omnicrawl\\.agent_tmp\\pip"),
    ));
    // npm 的全局前缀默认是 `%APPDATA%\npm`，容器读不到。`npm.ps1` 会拿
    // `npm-prefix.js` 的输出做一次 `Test-Path`，对不可读路径直接抛
    // `Access is denied`——在 `$ErrorActionPreference = 'Stop'` 下这是一次**致命**错误，
    // 于是 `npm --version` 报的是 `Test-Path : Access is denied`，而不是版本号（实测）。
    // 指到工作区内既让这次探测有确定结果，也让 `npm i -g` 有可写落点。
    vars.push((
        "npm_config_prefix".to_string(),
        format!("{workspace}\\.omnicrawl\\.agent_tmp\\npm-global"),
    ));
    // `CARGO_HOME` 指到沙盒内可写目录：它同时是 registry 缓存落点，指回真实 `~/.cargo`
    // 会要求写权限（那里只有只读授权）。toolchain 的定位不走它——`cargo.exe` 是 rustup
    // 的 shim，靠 `RUSTUP_HOME` 找 toolchain（上面已显式给出）。
    vars.push((
        "CARGO_HOME".to_string(),
        format!("{workspace}\\.omnicrawl\\.agent_tmp\\cargo"),
    ));

    // 网络：容器没有 `internetClient`（内核层出不了网），唯一通路是宿主回环代理。
    // 注入这两个变量，让尊重代理的客户端（curl/pip/npm/git/cargo）走代理；
    // `NO_PROXY` 留给回环自身，避免访问本地 dev server 时被绕一圈。
    //
    // **不注入就是完全不注入**：宿主的代理变量一律不继承——否则容器可能直接
    // 打到用户配置的代理上去，我们的判定就被跳过了。
    if let Some(proxy) = proxy {
        vars.push(("HTTP_PROXY".to_string(), proxy.to_string()));
        vars.push(("HTTPS_PROXY".to_string(), proxy.to_string()));
        // 小写形式：部分工具（curl、git、pip）只认小写。
        vars.push(("http_proxy".to_string(), proxy.to_string()));
        vars.push(("https_proxy".to_string(), proxy.to_string()));
        vars.push((
            "NO_PROXY".to_string(),
            "127.0.0.1,localhost,::1".to_string(),
        ));
        vars.push((
            "no_proxy".to_string(),
            "127.0.0.1,localhost,::1".to_string(),
        ));
    }

    let mut block: Vec<u16> = Vec::new();
    for (key, value) in dedupe_vars(vars) {
        block.extend(wide(&format!("{key}={value}")));
    }
    block.push(0);
    block
}

/// 按**大小写不敏感**去重环境变量，保留首次出现的那条。
///
/// Windows 的环境块里 `WINDIR` 与 `windir` 是两个条目，但所有读取方（PowerShell 的
/// `Env:` 驱动器、.NET 的 `Environment`、大多数 CRT）都按大小写不敏感建字典，
/// 遇到同名两条就抛「已添加了具有相同键的项」——一条环境变量重复能让整个
/// `$env:FOO` 机制失效，报错还完全不指向重复项。
pub(super) fn dedupe_vars(vars: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut seen: Vec<String> = Vec::with_capacity(vars.len());
    let mut kept: Vec<(String, String)> = Vec::with_capacity(vars.len());
    for (key, value) in vars {
        let folded = key.to_ascii_uppercase();
        if seen.contains(&folded) {
            continue;
        }
        seen.push(folded);
        kept.push((key, value));
    }
    kept
}

/// 沙盒内可用的 `PATH`：只保留容器**确实能读**的目录。
///
/// 三类来源，顺序即查找优先级：
///
/// 1. 工作区自己的 `node_modules/.bin` 等（相对项按工作区解析）；
/// 2. 已授只读执行权的解释器目录（[`crate::sandbox::runtime`] 发现的那批）；
/// 3. 宿主 `PATH` 里的系统目录（`C:\Windows`、`Program Files`，容器默认可读）。
///
/// 不在这三类里的一律剔除：宿主 PATH 里的 `.cargo/bin`、`WindowsApps`、各类 SDK 都对容器
/// 不可访问，留着只会让程序查找在解析阶段失败（而且报错极具误导性）。
pub(super) fn sandboxed_path(workspace: &str, runtimes: &[RuntimeRoot]) -> String {
    let mut entries: Vec<String> = Vec::new();
    let mut push = |value: &str| {
        let value = value.trim().trim_end_matches('\\');
        if value.is_empty() {
            return;
        }
        if entries
            .iter()
            .any(|entry| entry.eq_ignore_ascii_case(value))
        {
            return;
        }
        entries.push(value.to_string());
    };

    push(workspace);
    push(&format!("{workspace}\\.omnicrawl\\.agent_tmp\\npm"));
    // `npm i -g` 把可执行文件放进 prefix 目录（`build_environment` 指向这里），
    // 放进 PATH 才能被后续命令找到。
    push(&format!("{workspace}\\.omnicrawl\\.agent_tmp\\npm-global"));
    // Rust：`~/.cargo/bin` 是唯一放着 `cargo.exe`/`rustc.exe`/`rustup.exe` 的地方。
    // 只有**已授权**的目录才会出现在 `runtimes` 里，因此这里不会把一个读不到的
    // 用户目录塞进 PATH（那会让 cmd 在解析阶段就失败，报"不是内部或外部命令"）。
    for runtime in runtimes {
        push(&runtime.path.to_string_lossy());
    }
    match std::env::var("PATH") {
        Ok(host) => {
            for entry in host.split(';') {
                if is_system_path(entry) {
                    push(entry);
                }
            }
        }
        Err(_) => {}
    }
    entries.join(";")
}

/// 沙盒内可用的 `PSModulePath`：**剔除用户目录下的路径**，并补上各候选解释器自带的模块目录。
///
/// pwsh 启动时会逐个解析模块目录；只要存在一个打不开的项，它就把整份模块搜索路径
/// 判为不可用，连 `Write-Output` 这类内置 cmdlet 都加载不出来（真机实测现象）。
/// 系统目录下的模块对容器可读（`ALL APPLICATION PACKAGES: RX`），保留它们即可。
///
/// 补解释器自己的 `Modules` 是**必要条件但非充分条件**：PowerShell 7 的内置 cmdlet 是
/// 模块（`Microsoft.PowerShell.*`），只在自己安装目录下找；实测容器内 `$PSHOME` 与
/// `PSModulePath` 都正确时它仍加载不出任何 cmdlet，最终由
/// [`Isolation::powershell`] 的实测探测决定是否回退到 5.1。
fn sandboxed_module_path(powershell_roots: &[PathBuf]) -> String {
    let host = std::env::var("PSModulePath").unwrap_or_default();
    let mut kept: Vec<String> = std::env::split_paths(&host)
        .map(|path| path.to_string_lossy().to_string())
        .filter(|path| !path.trim().is_empty() && is_system_path(path))
        .collect();
    for modules in powershell_roots {
        let text = modules.to_string_lossy().to_string();
        if !kept.iter().any(|entry| entry.eq_ignore_ascii_case(&text)) {
            kept.insert(0, text);
        }
    }
    if kept.is_empty() {
        // 宿主没有可用值时给出系统默认，避免空串让 pwsh 彻底找不到模块。
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| String::from("C:/Windows"));
        return format!("{root}/system32/WindowsPowerShell/v1.0/Modules");
    }
    kept.join(";")
}

/// 各候选 PowerShell 自带的模块目录（存在才收）。
///
/// 环境块在**探测之前**就要组装好（探测本身也要用它），所以这里不区分「最终选中哪个」：
/// 把所有候选的模块目录都放进去，选中谁都能加载自己的 cmdlet。用不到的项多几个目录，
/// 代价是 pwsh 启动时多扫几处不存在的路径——比选错解释器安全得多。
fn powershell_module_roots() -> Vec<PathBuf> {
    let env = |name: &str| std::env::var(name).ok();
    crate::sandbox::runtime::powershell_candidates(&env)
        .iter()
        .filter_map(|executable| executable.parent())
        .map(|directory| directory.join("Modules"))
        .filter(|modules| modules.is_dir())
        .collect()
}

/// 路径是否在系统范围内（`SystemRoot`、`Program Files` 系、`ProgramData`）。
///
/// 按**路径组件**比较，不用字符串前缀：带盘符的路径以 `C:` 开头，`starts_with("program files")`
/// 永远不成立——这个写法曾经让「系统目录」判定事实上只覆盖 `SystemRoot`。
pub(super) fn is_system_path(path: &str) -> bool {
    let candidate = normalize_path(path);
    if candidate.is_empty() {
        return false;
    }
    for name in [
        "SystemRoot",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "ProgramData",
        "windir",
    ] {
        let Ok(value) = std::env::var(name) else {
            continue;
        };
        let base = normalize_path(&value);
        if base.is_empty() {
            continue;
        }
        if candidate == base || candidate.starts_with(&format!("{base}\\")) {
            return true;
        }
    }
    false
}

/// 比较用路径：小写、正斜杠统一成反斜杠、去掉结尾分隔符。
pub(super) fn normalize_path(path: &str) -> String {
    path.trim()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}
