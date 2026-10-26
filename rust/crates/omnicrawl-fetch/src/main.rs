//! `omnicrawl-fetch`：抓取接口的常驻服务与控制入口。
//!
//! 子命令：
//! * `serve`（默认）：前台常驻监听，直到 Ctrl-C。由 `ensure` 以脱离宿主的方式拉起。
//! * `ensure`：确保本机有一份可用的抓取接口（已就绪则复用，否则拉起并等它就绪）。
//! * `stop`：停止本机共用的抓取接口。
//! * `status`：打印配置与运行状态。
//!
//! 配置来自 `config.toml` 的 `[fetch_api]` 段；未启用时 `serve` 拒绝启动
//! （详见 `omnicrawl://docs/fetch_api.md`）。

use std::process::ExitCode;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_fetch::app::{build_router, AppState};
use omnicrawl_fetch::config::load_settings;
use omnicrawl_fetch::server::{self, EnsureOutcome};

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
                "用法：omnicrawl-fetch [serve|ensure|stop|status]\n\
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
        eprintln!("[fetch] 拒绝启动：{reason}");
        return ExitCode::FAILURE;
    }
    let address = settings.address();
    let listener = match std::net::TcpListener::bind(&address) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("[fetch] 无法绑定 {address}：{error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = listener.set_nonblocking(true) {
        eprintln!("[fetch] 设置非阻塞失败：{error}");
        return ExitCode::FAILURE;
    }
    let listener = match tokio::net::TcpListener::from_std(listener) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("[fetch] 接管监听套接字失败：{error}");
            return ExitCode::FAILURE;
        }
    };
    println!("[fetch] 抓取接口监听 http://{address}");
    let state = AppState::new(settings, &environment);
    axum::serve(listener, build_router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map(|()| ExitCode::SUCCESS)
        .unwrap_or_else(|error| {
            eprintln!("[fetch] 服务异常退出：{error}");
            ExitCode::FAILURE
        })
}

/// 打印配置与运行状态。
fn status() -> ExitCode {
    let environment = ConfigEnvironment::from_process();
    let settings = load_settings(&environment);
    println!(
        "配置：{}",
        omnicrawl_config::core::runtime::default_config_path(&environment).display()
    );
    println!("监听：{}", settings.address());
    println!(
        "接口：{}",
        if settings.api.enabled {
            "已启用"
        } else {
            "未启用"
        }
    );
    println!("鉴权：无（仅回环地址，本机任意程序均可调用）");
    println!(
        "指纹：{}",
        if settings.api.impersonate.is_empty() {
            "按请求参数（默认 chrome）".to_string()
        } else {
            format!("固定 {}", settings.api.impersonate)
        }
    );
    println!("单次 URL 上限：{}", settings.url_limit());
    println!(
        "监听中：{}",
        if server::service_ready(&environment) {
            "是"
        } else {
            "否"
        }
    );
    if let Some(reason) = settings.unavailable_reason() {
        println!("不可用原因：{reason}");
        return ExitCode::FAILURE;
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
