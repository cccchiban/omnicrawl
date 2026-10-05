//! `omnicrawl-decision`：决策接口的常驻服务与控制入口。
//!
//! 子命令：
//! * `serve`（默认）：前台常驻监听，直到 Ctrl-C。由 `ensure` 以脱离宿主的方式拉起。
//! * `ensure`：确保本机有一份可用的决策接口（已在监听则复用，否则拉起并等就绪）。
//! * `stop`：停止本机共用的决策接口。
//! * `status`：打印配置与监听状态，并做一次上游连通性检查。
//!
//! 配置来自 `decision_models.toml` 的 `[api]` 段与默认决策渠道；未启用或没有可用渠道时
//! `serve` 拒绝启动（详见 `omnicrawl://docs/decision_api.md`）。

use std::process::ExitCode;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_decision::app::{build_router, AppState};
use omnicrawl_decision::config::load_settings;
use omnicrawl_decision::server::{self, EnsureOutcome};

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
        "status" => status().await,
        other => {
            eprintln!(
                "用法：omnicrawl-decision [serve|ensure|stop|status]\n\
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
        eprintln!("[decision] 拒绝启动：{reason}");
        return ExitCode::FAILURE;
    }
    let address = settings.address();
    let listener = match std::net::TcpListener::bind(&address) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("[decision] 无法绑定 {address}：{error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = listener.set_nonblocking(true) {
        eprintln!("[decision] 设置非阻塞失败：{error}");
        return ExitCode::FAILURE;
    }
    let listener = match tokio::net::TcpListener::from_std(listener) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("[decision] 接管监听套接字失败：{error}");
            return ExitCode::FAILURE;
        }
    };
    println!("[decision] 决策接口监听 http://{address}");
    let state = AppState::new(settings, &environment);
    axum::serve(listener, build_router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map(|()| ExitCode::SUCCESS)
        .unwrap_or_else(|error| {
            eprintln!("[decision] 服务异常退出：{error}");
            ExitCode::FAILURE
        })
}

/// 打印配置与运行状态，并打一次上游（决策服务）连通性检查。
async fn status() -> ExitCode {
    let environment = ConfigEnvironment::from_process();
    let settings = load_settings(&environment);
    println!("配置：{}", omnicrawl_config::core::runtime::default_decision_models_path(&environment).display());
    println!("监听：{}", settings.address());
    println!(
        "接口：{}",
        if settings.api.enabled { "已启用" } else { "未启用" }
    );
    println!("鉴权：无（仅回环地址，本机任意程序均可调用）");
    match settings.channel.as_ref() {
        Some(channel) => println!(
            "渠道：mode={} model={} base_url={} 凭据={}",
            channel.mode,
            channel.model,
            channel.base_url,
            if channel.resolve_api_key().is_empty() {
                "未配置"
            } else {
                "已配置"
            }
        ),
        None => println!("渠道：没有可用渠道"),
    }
    let listening = server::health(&settings.api.host, settings.api.port as u16);
    println!("监听中：{}", if listening { "是" } else { "否" });
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
