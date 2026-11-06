//! 临时探针：真机验证 cgroup v2 限额是否真能建起来并**真正约束**子进程。
//!
//! 关键点：必须在**子进程里**验证限额。探针自己不迁入就去 fork，
//! 那 20 个进程落在原 cgroup 里，限额当然不起作用（第一版探针就是这么误报的）。
use omnicrawl_config::features::sandbox::SandboxLimits;
use omnicrawl_host::sandbox::linux::cgroup;

fn main() {
    // 子进程模式：由它自己迁入 cgroup 后试 fork（限额必须在迁入之后才生效）。
    if std::env::args().any(|arg| arg == "--child") {
        child_main();
        return;
    }
    let limits = SandboxLimits {
        max_processes: 8,
        max_memory_mb: 256,
        cpu_rate_percent: 0,
    };
    println!("--- create ---");
    let applied = match cgroup::create(&limits) {
        Ok(applied) => applied,
        Err(reason) => {
            println!("create 失败: {reason}");
            return;
        }
    };
    println!("路径: {}", applied.path.display());
    println!("已设: {:?}", applied.items);
    println!("未设: {:?}", applied.skipped);
    println!("摘要: {}", applied.describe());

    println!("--- fork 一个子进程，由它自己迁入并试 fork ---");
    let path = applied.path.clone();
    let child = std::process::Command::new(std::env::current_exe().expect("自身路径"))
        .arg("--child")
        .env("OC_CGROUP", &path)
        .spawn()
        .expect("拉起子进程");
    let mut child = child;
    let status = child.wait().expect("等待子进程");
    println!("子进程退出: {status}");

    println!("--- 清理 ---");
    cgroup::cleanup(&applied.path);
    println!("  清理后是否还在: {}", applied.path.exists());
}

/// 子进程入口：迁入 → 确认 → 试 fork 20 个。
pub fn child_main() {
    let path = std::path::PathBuf::from(std::env::var("OC_CGROUP").expect("OC_CGROUP"));
    match cgroup::join(&path) {
        Ok(()) => println!("  迁入成功"),
        Err(reason) => {
            println!("  迁入失败: {reason}");
            return;
        }
    }
    println!(
        "  自己在哪: {:?}",
        std::fs::read_to_string("/proc/self/cgroup").map(|text| text.trim().to_string())
    );
    println!("  pids.current = {:?}", std::fs::read_to_string(path.join("pids.current"))
        .map(|text| text.trim().to_string()));

    let mut spawned = 0;
    let mut first_error = None;
    let mut children = Vec::new();
    for _ in 0..20 {
        match std::process::Command::new("/bin/sleep").arg("1").spawn() {
            Ok(child) => {
                spawned += 1;
                children.push(child);
            }
            Err(error) => {
                first_error = Some(error);
                break;
            }
        }
    }
    println!("  成功 fork {spawned} 个（pids.max = 8）");
    if let Some(error) = first_error {
        println!("  第一个失败: {error}");
    }
    for mut child in children {
        let _ = child.wait();
    }
}
