use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::os::windows::io::FromRawHandle;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use windows_sys::Win32::Foundation::PSID;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    FreeSid, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectCpuRateControlInformation,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, TerminateProcess, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

use super::acl::*;
use super::base::*;
use super::environment::*;
use super::network::*;
use super::process::*;
use super::toolchain::*;
use omnicrawl_config::features::sandbox::SandboxLimits;
use crate::sandbox::Sandbox;

/// 一个 AppContainer profile 及其 package SID。
///
/// 名字里带工作区哈希：不同工作区用不同容器，因此给某个工作区加的 ACE 不会顺带把
/// 另一个工作区暴露给本条沙盒。
pub struct Container {
    pub(super) name: String,
    pub(super) sid: *mut c_void,
}

impl Container {
    /// 取或建容器。已存在时只派生 SID，不改任何既有设置。
    pub fn ensure(name: &str) -> Result<Self, String> {
        let name_wide = wide(name);
        let display = wide("OmniCrawl 沙盒");
        let description = wide("OmniCrawl 以 AppContainer 身份执行的命令");

        // 建容器时不给能力：容器在内核层出不了网，出网只能经宿主回环代理（P3）。
        let mut sid: *mut c_void = std::ptr::null_mut();
        // SAFETY: 三个字符串指针在调用期间有效；capabilities 为 0 个时可以为空。
        let created = unsafe {
            CreateAppContainerProfile(
                name_wide.as_ptr(),
                display.as_ptr(),
                description.as_ptr(),
                std::ptr::null(),
                0,
                &mut sid,
            )
        };
        if created >= 0 && !sid.is_null() {
            return Ok(Self {
                name: name.to_string(),
                sid,
            });
        }

        // 已存在是常态（第二次启动起），改为派生同一个 SID。
        let already_exists = created == (0x8007_0000u32 | ERROR_ALREADY_EXISTS) as i32;
        if !already_exists && !sid.is_null() {
            // 其他失败也可能已经填了 SID：以 SID 为准，泄漏前先释放。
            // SAFETY: SID 由 userenv 分配，FreeSid 是配对的释放方式。
            unsafe { FreeSid(sid) };
        }
        let mut derived: *mut c_void = std::ptr::null_mut();
        // SAFETY: 同上，名字指针在调用期间有效。
        let result =
            unsafe { DeriveAppContainerSidFromAppContainerName(name_wide.as_ptr(), &mut derived) };
        if result < 0 || derived.is_null() {
            return Err(format!(
                "无法建立 AppContainer 容器 `{name}`（HRESULT 0x{created:08X}/0x{result:08X}）"
            ));
        }
        Ok(Self {
            name: name.to_string(),
            sid: derived,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub(super) fn sid(&self) -> *mut c_void {
        self.sid
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        if !self.sid.is_null() {
            // SAFETY: SID 由 `CreateAppContainerProfile` / `Derive…` 分配，配对的释放是 FreeSid。
            unsafe { FreeSid(self.sid) };
        }
    }
}

// SAFETY: `Container` 只持有一个 SID 指针，SID 是进程范围内的只读内存，跨线程共享安全。
unsafe impl Send for Container {}

unsafe impl Sync for Container {}

/// 一条沙盒命令所需的全部上下文：容器身份、专用目录与限额。
pub struct Isolation {
    pub(super) container: Container,
    pub(super) temp: PathBuf,
    pub(super) workspace: PathBuf,
    pub(super) limits: SandboxLimits,
    pub(super) runtimes: Vec<crate::sandbox::runtime::RuntimeRoot>,
    pub(super) skipped: Vec<(PathBuf, String)>,
    /// 允许的网络能力名（空表示完全断网）。
    pub(super) capabilities: Vec<String>,
    /// 已解析的能力 SID 与其底层数组：必须活到 `CreateProcessW` 返回。
    pub(super) capability_owned: Vec<CapabilitySids>,
    /// 出网代理地址（`http://127.0.0.1:<port>`）：`None` 表示不注入，容器无出网通路。
    pub(super) proxy_url: std::sync::Mutex<Option<String>>,
    /// 容器内实测可用的 PowerShell（`None` 表示尚未探测或一个都不通）。探测要起进程，
    /// 一次约 1–2 秒，因此结果缓存。
    pub(super) powershell: std::sync::Mutex<Option<Option<PathBuf>>>,
}

impl Isolation {
    /// 准备容器与目录，并给工作区、沙盒临时目录与配置里额外声明的路径授权。
    ///
    /// 任何一步失败都返回错误——调用方据此**拒绝执行**，不放行到宿主身份（失败即关闭）。
    pub fn prepare(sandbox: &Sandbox) -> Result<Arc<Self>, String> {
        let workspace = sandbox.root().to_path_buf();
        let temp = sandbox.sandbox_temp();
        std::fs::create_dir_all(temp.join("home"))
            .map_err(|error| format!("无法创建沙盒临时目录 {}：{error}", temp.display()))?;
        // `npm_config_prefix` 指到这里（见 `build_environment`）：先建出来，
        // 否则 `npm i -g` 要自己造目录树，而 `npm.ps1` 的前缀探测会先撞上不存在。
        let npm_global = workspace
            .join(".omnicrawl")
            .join(".agent_tmp")
            .join("npm-global");
        std::fs::create_dir_all(&npm_global)
            .map_err(|error| format!("无法创建 {}：{error}", npm_global.display()))?;

        let name = container_name(&workspace);
        let container = Container::ensure(&name)?;
        let sid = container.sid();

        let mut granted: Vec<PathBuf> = Vec::new();
        for path in [&workspace, &temp] {
            grant_ancestors(path, sid, &mut granted);
            grant_ancestors(&temp.join("home"), sid, &mut granted);
            grant(path, sid, FULL_CONTROL_FOR_SANDBOX, true)
                .map_err(|error| format!("无法给 {} 授予沙盒访问权：{error}", path.display()))?;
        }
        // 系统盘根必须可遍历，否则 PowerShell 7 **一个 cmdlet 都加载不出来**。
        //
        // 根因是 pwsh 启动时要建立 `C:`（系统盘）驱动器：它用
        // `Directory.Exists("C:\\")` 探路，而 AppContainer 对没有 ACE 的卷根返回 False，
        // 于是以「The specified drive root "C:\" either does not exist」失败，`C:` 驱动器
        // 没建起来；接着它自己的内置模块（`Microsoft.PowerShell.*`）全部导入失败
        // （`Cannot find drive. A drive with the name 'C' does not exist.`），最终连
        // `Write-Output` 都报「无法识别」。Windows PowerShell 5.1 不需要这一步，所以同一
        // 容器里 5.1 一切正常——这正是「只有 pwsh 坏掉」的真正原因。
        //
        // 工作区在 D: 时 `grant_ancestors` 只覆盖 D: 那一支，系统盘根不在祖先链上，
        // 于是这条 ACE 必须单独补。只授遍历（`TRAVERSE_ONLY`，不可继承），代价极小：
        // 卷根本来就有大量同类 ACE，且这一条不改写继承。
        for root in system_drive_roots() {
            if !granted.iter().any(|entry| entry == &root) && !can_traverse(&root, sid) {
                // 失败不致命：容器内 PowerShell 会退回到 5.1，命令仍能跑。但必须留下
                // 原因——否则症状只是「pwsh 莫名其妙不能用」，与权限拒绝毫无相似之处。
                if let Err(error) = grant(&root, sid, TRAVERSE_ONLY, false) {
                    eprintln!(
                        "[sandbox] 无法给系统盘根 {} 授予遍历权：{error}",
                        root.display()
                    );
                }
            }
            granted.push(root);
        }

        let config = sandbox.config();
        for entry in config.writable.iter().chain(config.readable.iter()) {
            let resolved = if Path::new(entry).is_absolute() {
                PathBuf::from(entry)
            } else {
                workspace.join(entry)
            };
            if !resolved.exists() {
                continue;
            }
            let access = if config.writable.contains(entry) {
                FULL_CONTROL_FOR_SANDBOX
            } else {
                READ_EXECUTE
            };
            grant_ancestors(&resolved, sid, &mut granted);
            grant(&resolved, sid, access, resolved.is_dir()).map_err(|error| {
                format!("无法给 {} 授予沙盒访问权：{error}", resolved.display())
            })?;
        }

        // 解释器根目录：`node`/`npm`/`python`/`cargo` 常装在用户目录下，容器默认读不到。
        //
        // 成本已实测：改一次 DACL 会让系统重算整棵子树的继承，`D:\ProgramData\Anaconda3`
        // （22 万项）单次 18.4 秒，`C:\Program Files\nodejs`（0.29 万项）0.26 秒。
        // 所以这里按规模分档：小目录自动授权（代价可忽略，用户开箱可用），大目录跳过并在
        // `/sandbox` 里如实报告——宁可让用户知道「这个解释器沙盒内不可用」，也不让启动
        // 默默卡 20 秒。已经授过的（容器 SID 有权限）零成本，直接进 PATH。
        // 授权与判定都在 `prepare_runtimes` 内部完成（先授、再按可读性分可用/跳过）。
        let (runtimes, skipped) = prepare_runtimes(sid);

        Ok(Arc::new(Self {
            container,
            temp,
            workspace,
            limits: config.limits,
            runtimes,
            skipped,
            capabilities: Vec::new(),
            capability_owned: Vec::new(),
            proxy_url: std::sync::Mutex::new(None),
            powershell: std::sync::Mutex::new(None),
        }))
    }

    /// 设定容器可用的网络能力（空列表 = 完全断网）。
    ///
    /// 只有 `internetClient`/`internetClientServer` 两个名字有意义：AppContainer 的
    /// 出网权限就是能力 SID，没能力连回环都会被拒（实测超时）。因此「给网络」与
    /// 「给回环」是两件事：能出网靠能力，能连本机代理靠回环豁免。
    pub fn set_capabilities(&mut self, names: &[String]) -> Result<(), String> {
        let mut owned = Vec::with_capacity(names.len());
        for name in names {
            owned.push(capability_sid(name)?);
        }
        self.capabilities = names.to_vec();
        self.capability_owned = owned;
        Ok(())
    }

    /// 当前生效的网络能力名。
    pub fn capabilities(&self) -> &[String] {
        &self.capabilities
    }

    /// 设定出网代理地址（注入 `HTTP_PROXY` 等）。
    ///
    /// 传 `None` 表示**不注入任何代理变量**：容器既没有网络能力、也没有代理通路，
    /// 那就是彻底断网。这正是 `offline` 策略与「豁免失败」时的降级形态。
    pub fn set_proxy(&self, proxy_url: Option<String>) {
        let mut guard = self
            .proxy_url
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = proxy_url;
    }

    /// 当前注入的代理地址。
    pub fn proxy_url(&self) -> Option<String> {
        self.proxy_url
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 容器内可用的 PowerShell：候选按「7.* 最新 → 5.1」排序，逐个在容器里实测。
    ///
    /// 为什么必须实测而不是按版本挑：PowerShell 7 在 AppContainer 里能否加载 cmdlet，
    /// 取决于它的模块目录是否真的可读——能启动、能报版本号，但 `Write-Output` 都不认，
    /// 症状是「脚本里每个命令都不存在」。这类失败只能在容器里跑一次才看得见。
    ///
    /// 探测用 `Write-Output` 并核对输出：退出码 0 但报错文本说明 cmdlet 没加载，同样算不通。
    pub fn powershell(&self) -> Option<PathBuf> {
        let mut cache = self
            .powershell
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(selected) = cache.as_ref() {
            return selected.clone();
        }
        let env = |name: &str| std::env::var(name).ok();
        let selected = crate::sandbox::runtime::powershell_candidates(&env)
            .into_iter()
            .find(|candidate| self.powershell_works(candidate));
        *cache = Some(selected.clone());
        selected
    }

    /// 在容器里跑一次最小脚本，判断该解释器是否真能用。
    ///
    /// 判据只看「输出里有没有探针字符串」：PowerShell 7 在 AppContainer 里启动正常、
    /// `$PSHOME` 与 `$env:PSModulePath` 都正确，却**一个 cmdlet 都加载不出来**，
    /// 连 `Write-Output` 都报「无法识别」——此时它照样可能以退出码 0 结束，因此退出码
    /// 不能作为判据（真机实测）。
    pub(super) fn powershell_works(&self, executable: &Path) -> bool {
        let args = vec![
            executable.to_string_lossy().to_string(),
            "-NoLogo".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            "Write-Output '__omnicrawl_probe__'".to_string(),
        ];
        let Ok(spawned) = self.spawn(&args) else {
            return false;
        };
        let mut spawned = spawned;
        // 探测也要有上限：真坏掉时它可能一直挂着，不能把首次执行拖死。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if spawned.child.try_wait().is_some() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                spawned.child.kill();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let stdout = String::from_utf8_lossy(&spawned.stdout.join().unwrap_or_default()).to_string();
        stdout.contains("__omnicrawl_probe__")
    }

    /// 能力 SID 指针表，供 `SECURITY_CAPABILITIES.Capabilities` 使用。
    pub(super) fn capability_attributes(&self) -> Vec<SID_AND_ATTRIBUTES> {
        self.capability_owned
            .iter()
            .flat_map(|owned| owned.sids.iter().copied())
            .map(|sid| SID_AND_ATTRIBUTES {
                Sid: sid as PSID,
                // SE_GROUP_ENABLED = 0x4：能力必须处于启用状态才生效。
                Attributes: 0x0000_0004,
            })
            .collect()
    }

    /// 已可在容器内读取的解释器目录（会进沙盒 `PATH`）。
    pub fn runtimes(&self) -> &[crate::sandbox::runtime::RuntimeRoot] {
        &self.runtimes
    }

    /// 因目录过大而**未授权**的解释器目录，附原因（`/sandbox` 与日志如实展示）。
    pub fn skipped_runtimes(&self) -> &[(PathBuf, String)] {
        &self.skipped
    }

    pub fn container_name(&self) -> &str {
        self.container.name()
    }

    /// 销毁本工作区的容器 profile（退出清理）。
    ///
    /// 安全性来自 SID 的确定性：容器名由「用户 + 工作区」派生，同名永远得到同一个
    /// SID（实测建 profile 前后、删除后重建都是同一个 `S-1-15-2-…`）。因此磁盘上那些
    /// 给该 SID 的 ACE 不会变成悬空引用——下次启动重建 profile 后它们继续有效，
    /// 也就不必重付一次可继承授权的代价（大目录单次可达数十秒）。
    ///
    /// 删不掉不算错误：profile 残留只会让 `%LOCALAPPDATA%\\Packages` 多一项，
    /// 而容器名稳定，后续启动会复用同一个 profile。因此这里只返回是否真的删除了。
    pub fn destroy_container_named(name: &str) -> bool {
        let name_wide = wide(name);
        // SAFETY: 名字指针在调用期间有效；只按名字删除，不持有其他资源。
        let result = unsafe { DeleteAppContainerProfile(name_wide.as_ptr()) };
        result >= 0
    }

    /// 本容器的 SID（测试与诊断用）。
    pub fn sid_for_test(&self) -> *mut c_void {
        self.container.sid()
    }

    /// 本容器 SID 的字符串形式（`S-1-15-2-...`），回环豁免按它登记。
    pub fn sid_string(&self) -> String {
        format_sid(self.container.sid())
    }

    /// 给本容器加回环豁免，返回的守卫在析构时撤销。
    ///
    /// 为什么必须做：AppContainer 不给网络能力时**连宿主回环都被拒**（实测超时）。
    /// 沙盒的出网通道是宿主侧代理，它就在回环上，因此没有豁免代理形同虚设。
    ///
    /// 为什么用 `-p=<SID>` 而不是 `-n=<名字>`：`-n` 要的是**包名**
    /// （如 `microsoft.net.native.framework.2.1_8wekyb3d8bbwe`），AppContainer 名不是包名，
    /// 传它一律返回 87（参数错误）——这是实测踩出来的。
    ///
    /// 加不上就是加不上：调用方据此把网络强制为 `offline`，绝不放宽成「有
    /// `internetClient` 但代理不通」那种更危险的中间态。
    #[cfg(windows)]
    pub fn exempt_loopback(&self) -> Result<LoopbackExemption, String> {
        let sid = self.sid_string();
        // 幂等：先撤一次再设，避免上次崩溃残留的重复条目累积。
        let _ = run_checknetisolation("-d", &sid);
        run_checknetisolation("-a", &sid)
            .map_err(|error| format!("回环豁免不可用，网络已禁（{error}）"))?;
        Ok(LoopbackExemption { sid })
    }

    /// 撤掉本容器 SID 在 `path` 上的 ACE（清理由本沙盒写入的权限）。
    ///
    /// 只供测试与诊断使用：正常运行时 ACE 会保留，下次启动复用（重新授予很贵）。
    pub fn revoke(&self, path: &Path) -> Result<(), String> {
        revoke(path, self.container.sid())
    }

    /// 以容器身份拉起 `args`（`args[0]` 是可执行文件路径）。
    ///
    /// 返回的 [`SandboxChild`] 已经归属 Job：后代无法逃离，宿主崩溃时由操作系统递归回收。
    /// 输出一次性读全——`monitor` 需要增量读，用 [`Self::spawn_streamed`]。
    pub fn spawn(&self, args: &[String]) -> io::Result<Spawned> {
        let (child, stdout_read, stderr_read) = self.spawn_raw(args)?;
        Ok(Spawned {
            child,
            stdout: spawn_pipe_reader(stdout_read.into_inner()),
            stderr: spawn_pipe_reader(stderr_read.into_inner()),
        })
    }

    /// 以容器身份拉起 `args`，输出管道交给调用方逐行读取（`monitor` 的后台任务）。
    ///
    /// 与 [`Self::spawn`] 的差别只在输出怎么消费：这里把两个读端交出去，不占线程。
    /// 另附一个 [`Terminator`]：子进程交给等待线程后，停止路径仍要能终止它。
    pub fn spawn_streamed(&self, args: &[String]) -> io::Result<Streamed> {
        let (child, stdout_read, stderr_read) = self.spawn_raw(args)?;
        let terminator = Terminator::duplicate(child.process_handle())?;
        // SAFETY: 两个读端由本函数独占，转换后由 `File` 负责关闭，不会重复关闭。
        let stdout = unsafe { std::fs::File::from_raw_handle(stdout_read.into_inner() as _) };
        // SAFETY: 同上。
        let stderr = unsafe { std::fs::File::from_raw_handle(stderr_read.into_inner() as _) };
        Ok(Streamed {
            child,
            stdout,
            stderr,
            terminator,
        })
    }

    /// 真正的一次 `CreateProcessW`：返回子进程与两个**原始**读端句柄。
    pub(super) fn spawn_raw(&self, args: &[String]) -> io::Result<(SandboxChild, OwnedHandle, OwnedHandle)> {
        if args.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "缺少可执行文件",
            ));
        }
        let executable = wide(&args[0]);
        let mut command_line = build_command_line(args);
        let directory = wide(&self.workspace.to_string_lossy());
        let proxy = self.proxy_url();
        let mut environment = build_environment_with(
            &self.temp,
            &self.workspace,
            &self.runtimes,
            proxy.as_deref(),
        );

        let (stdout_read, stdout_write) = pipe().map_err(other)?;
        let (stderr_read, stderr_write) = pipe().map_err(other)?;
        let stdin = null_input().map_err(other)?;
        {
            let dbg = std::env::var("DBG_PIN").is_ok();
            if dbg {
                eprintln!(
                    "[dbg] handles in={} out={} err={} stdout_r={}",
                    stdin.get(),
                    stdout_write.get(),
                    stderr_write.get(),
                    stdout_read.get()
                );
            }
        }

        let job = self.create_job().map_err(other)?;
        // 能力结构体必须活到 `CreateProcessW` 返回：属性列表只存指针，不复制内容。
        // 无能力（`capabilities` 为空）时 `CapabilityCount = 0`，容器完全断网。
        let mut capability_attrs = self.capability_attributes();
        let capability = SECURITY_CAPABILITIES {
            AppContainerSid: self.container.sid(),
            Capabilities: if capability_attrs.is_empty() {
                std::ptr::null_mut()
            } else {
                capability_attrs.as_mut_ptr()
            },
            CapabilityCount: capability_attrs.len() as u32,
            Reserved: 0,
        };
        let attributes = AttributeList::new(
            &capability,
            &[stdin.get(), stdout_write.get(), stderr_write.get()],
        )?;

        let mut info: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        // 必须是 `sizeof(STARTUPINFOEXW)`：传 104（`STARTUPINFOW` 的大小）时系统会认为
        // 扩展区不在可读范围内，直接以 `ERROR_INVALID_PARAMETER` 失败。
        info.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        info.StartupInfo.hStdInput = stdin.get();
        info.StartupInfo.hStdOutput = stdout_write.get();
        info.StartupInfo.hStdError = stderr_write.get();
        info.lpAttributeList = attributes.pointer();

        let mut process_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: 所有指针在 `CreateProcessW` 期间有效（`capability` 与 `attributes` 都在本栈帧）。
        // 必须把整个 `STARTUPINFOEXW` 传进去——属性列表在扩展区的 `lpAttributeList`，
        // 只传首字段 `StartupInfo` 会让扩展信息整份失效（进程会以宿主身份起来）。
        let created = unsafe {
            CreateProcessW(
                executable.as_ptr(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
                environment.as_mut_ptr() as *const c_void,
                directory.as_ptr(),
                &info as *const STARTUPINFOEXW as *const STARTUPINFOW,
                &mut process_info,
            )
        };
        // 子进程已持有自己的副本，父进程的三个句柄可以立刻关闭。
        // `GetLastError` 会被后续任何一次 Win32 调用覆盖，所以必须先取错误码再关句柄。
        let create_error = if created == 0 { last_error() } else { 0 };
        // SAFETY: 三个句柄都由本函数独占，各关闭一次。
        unsafe {
            CloseHandle(stdin.into_inner());
            CloseHandle(stdout_write.into_inner());
            CloseHandle(stderr_write.into_inner());
        }
        if created == 0 {
            return Err(describe_create_error(create_error));
        }

        // 线程句柄用不上；进程句柄与 Job 由 `SandboxChild` 持有。
        // SAFETY: 两个句柄都刚由 CreateProcessW 输出，各关闭一次。
        unsafe { CloseHandle(process_info.hThread) };
        let process = OwnedHandle::new(process_info.hProcess)
            .ok_or_else(|| io::Error::other("CreateProcessW 未返回进程句柄"))?;

        // 实测 `PROC_THREAD_ATTRIBUTE_JOB_LIST` 会让 `CreateProcessW` 以
        // `ERROR_INVALID_HANDLE` 失败，因此统一改为创建后纳入。风险窗口是创建到 `Assign`
        // 之间的几微秒：此时进程已在容器里（读不到用户目录），只是尚未受 Job 限额约束。
        if let Some(job) = job.as_ref() {
            // SAFETY: 两个句柄均有效。
            let assigned = unsafe { AssignProcessToJobObject(job.get(), process.get()) };
            if assigned == 0 {
                let code = last_error();
                // 起得来但管不住的后代比不隔离更危险：直接拒绝这条命令。
                // SAFETY: 进程已创建，终止后句柄仍归本函数关闭。
                unsafe { TerminateProcess(process.get(), 1) };
                return Err(other(io_error("把沙盒进程纳入 Job", code)));
            }
        }

        Ok((
            SandboxChild {
                process,
                job,
                pid: process_info.dwProcessId,
                exited: None,
            },
            stdout_read,
            stderr_read,
        ))
    }

    pub(super) fn create_job(&self) -> Result<Option<OwnedHandle>, String> {
        // SAFETY: 名字与属性都为空，得到匿名 Job。
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        let Some(job) = OwnedHandle::new(raw) else {
            return Err(io_error("创建 Job 对象", last_error()));
        };

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if self.limits.max_processes > 0 {
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            info.BasicLimitInformation.ActiveProcessLimit = self.limits.max_processes;
        }
        if self.limits.max_memory_mb > 0 {
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
            info.JobMemoryLimit = (self.limits.max_memory_mb as usize) * 1024 * 1024;
        }
        // SAFETY: 结构体按 Win32 定义零值初始化后逐字段赋值；大小按类型给出。
        let ok = unsafe {
            SetInformationJobObject(
                job.get(),
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast::<c_void>(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(io_error("设置 Job 限额", last_error()));
        }

        if self.limits.cpu_rate_percent > 0 {
            let mut rate: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = unsafe { std::mem::zeroed() };
            rate.ControlFlags =
                JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
            // 单位是 1/100 百分比：50% → 5000。
            rate.Anonymous.CpuRate = self.limits.cpu_rate_percent.min(100) * 100;
            // SAFETY: 同上。CPU 速率不被支持时（部分容器/虚拟化环境）不致命，忽略即可。
            unsafe {
                SetInformationJobObject(
                    job.get(),
                    JobObjectCpuRateControlInformation,
                    (&rate as *const JOBOBJECT_CPU_RATE_CONTROL_INFORMATION).cast::<c_void>(),
                    size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
                )
            };
        }
        Ok(Some(job))
    }
}
