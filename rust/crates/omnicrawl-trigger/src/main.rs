//! `omnicrawl-trigger`：触发接口的常驻服务与控制入口。
//!
//! 子命令：
//! * `serve`（默认）：前台常驻监听，直到 Ctrl-C。由 `ensure` 以脱离宿主的方式拉起。
//! * `ensure`：确保本机有一份可用的触发接口（已就绪则复用，否则拉起并等它就绪）。
//! * `stop`：停止本机共用的触发接口。
//! * `status`：打印配置与运行状态。
//!
//! 配置来自 `config.toml` 的 `[trigger_api]` 段；未启用时 `serve` 拒绝启动。

use std::process::ExitCode;
use std::sync::Arc;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_trigger::app::{AppState, build_router};
use omnicrawl_trigger::config::{kernel_program, load_settings};
use omnicrawl_trigger::pool::{PoolConfig, TriggerPool};
use omnicrawl_trigger::records::TriggerStore;
use omnicrawl_trigger::server::{self, EnsureOutcome};

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("serve");
    match command {
        "serve" => serve().await,
        "ensure" => {
            let environment = ConfigEnvironment::from_process();
            let settings = load_settings(&environment);
            let outcome = server::ensure_running(&environment, &settings, true);
            print_outcome(&outcome);
            exit_code(outcome)
        }
        "stop" => {
            let environment = ConfigEnvironment::from_process();
            let settings = load_settings(&environment);
            let outcome = server::stop(&environment, &settings);
            println!("{}", outcome.message);
            ExitCode::SUCCESS
        }
        "status" => status(),
        other => {
            eprintln!(
                "用法：omnicrawl-trigger [serve|ensure|stop|status]\n\
                 未知子命令：{other}"
            );
            ExitCode::FAILURE
        }
    }
}

/// 前台常驻监听；配置不齐时拒绝启动并给出可读原因。
async fn serve() -> ExitCode {
    let environment = ConfigEnvironment::from_process();
    let settings = load_settings(&environment);
    if let Some(reason) = settings.unavailable_reason() {
        eprintln!("[trigger] 拒绝启动：{reason}");
        return ExitCode::FAILURE;
    }
    let address = settings.address();
    let listener = match std::net::TcpListener::bind(&address) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("[trigger] 无法绑定 {address}：{error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = listener.set_nonblocking(true) {
        eprintln!("[trigger] 设置非阻塞失败：{error}");
        return ExitCode::FAILURE;
    }
    let listener = match tokio::net::TcpListener::from_std(listener) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("[trigger] 接管监听套接字失败：{error}");
            return ExitCode::FAILURE;
        }
    };
    println!("[trigger] 触发接口监听 http://{address}");
    // 自己登记 PID：`ensure` 拉起的进程会由调用方写一次，但 `serve` 也可能被直接启动
    // （前台调试、被别处托起）。只有服务自己写，`stop` / 就绪判定才不会因缺记录而失明——
    // 那种失灵的表现是「stop 说未运行，服务其实仍在监听，ensure 再拉一份却绑不上端口」。
    omnicrawl_trigger::server::write_pid(&environment, std::process::id());

    let store = Arc::new(TriggerStore::new(
        settings.api.queue_limit.max(1) as usize,
        settings.api.retention_hours,
    ));
    let pool = Arc::new(TriggerPool::new(
        store,
        PoolConfig::from_config(&settings.api, settings.session_root(&environment)),
        environment.clone(),
        kernel_program(&environment, None),
    ));
    pool.start();

    let shutdown_pool = Arc::clone(&pool);
    let result = axum::serve(listener, build_router(AppState::new((*pool).clone())))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            // 先停受理、再取消排队与运行中的触发：返回前让 slot 线程收尾，避免留下孤立内核。
            shutdown_pool.shutdown();
        })
        .await;
    if let Err(error) = result {
        eprintln!("[trigger] 服务异常退出：{error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// 等待 Ctrl-C（Unix 另接 SIGTERM）。
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(handle) => handle,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// `status`：配置与运行状态。
fn status() -> ExitCode {
    let environment = ConfigEnvironment::from_process();
    let settings = load_settings(&environment);
    let ready = server::service_ready(&environment);
    println!("触发接口配置：{}", settings.address());
    println!("启用：{}", settings.api.enabled);
    println!("并发上限：{}", settings.api.max_concurrency);
    println!("排队上限：{}", settings.api.queue_limit);
    println!("会话模式：{}", settings.api.session_mode);
    println!("工具白名单：{}", settings.api.tools.join(", "));
    println!("会话根：{}", settings.session_root(&environment).display());
    println!("运行状态：{}", if ready { "已就绪" } else { "未运行" });
    println!("日志：{}", server::log_path(&environment).display());
    if let Some(reason) = settings.unavailable_reason() {
        println!("不可用原因：{reason}");
    }
    ExitCode::SUCCESS
}

fn print_outcome(outcome: &EnsureOutcome) {
    println!("{}", outcome.message);
}

fn exit_code(outcome: EnsureOutcome) -> ExitCode {
    if outcome.ready() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
