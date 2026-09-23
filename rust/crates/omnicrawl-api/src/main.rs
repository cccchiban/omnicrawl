//! `omnicrawl-api`：本地 API 服务端二进制（对齐 `python -m omnicrawl.api`）。
//!
//! 令牌与监听地址来自 `OMNICRAWL_API_*` 环境变量或 `config.toml` 的 `api` 段；
//! 缺少令牌、地址不是回环或 CORS 配了 `*` 时拒绝启动。启动后先起内核子进程并握手，
//! 再把服务挂进状态——宿主没起来就不对外宣称服务可用。

use std::process::ExitCode;
use std::sync::Arc;

use omnicrawl_api::service::{options_from_process, resolve_kernel_program};
use omnicrawl_api::{
    load_api_config, reuse_port_supported, serve, shared_run_store_filename, AgentService,
    ApiConfig, ApiState, RunBackend, SharedRunStore, WORKER_CHILD_ENV,
};
use omnicrawl_config::core::runtime::user_config_dir;
use omnicrawl_config::core::runtime::ConfigEnvironment;

#[tokio::main]
async fn main() -> ExitCode {
    let environment = ConfigEnvironment::from_process();
    let config = match load_api_config(&environment, None) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("[api] 配置装载失败：{error}");
            return ExitCode::FAILURE;
        }
    };
    if config.workers > 1 && !is_worker_child(&environment) {
        if reuse_port_supported() {
            // 多 worker：由监督进程拉起多个子进程，各自 SO_REUSEPORT 绑定同一端口。
            return run_worker_supervisor(&config).await;
        }
        eprintln!(
            "[api] 当前平台不支持 SO_REUSEPORT，api.workers={} 退化为单进程；运行状态仍走共享存储。",
            config.workers
        );
    }
    let mut options = match options_from_process(&environment) {
        Ok(options) => options,
        Err(detail) => {
            eprintln!("[api] Agent 配置装载失败：{detail}");
            return ExitCode::FAILURE;
        }
    };
    let program = resolve_kernel_program(&environment, None);
    // 记下内核路径：运行期切换会话 / 工作区要重起内核。
    options.kernel_program = Some(program.clone());
    let mut service = match AgentService::spawn(options, &program) {
        Ok(service) => service,
        Err(detail) => {
            eprintln!("[api] 宿主启动失败：{detail}");
            return ExitCode::FAILURE;
        }
    };
    if config.workers > 1 {
        // 多 worker：每个进程有自己的 Agent 与隔离工作区，但客户端的后续请求可能落在任意进程上，
        // 因此运行记录、事件流与人工决策不能只留在进程内存里（对应 Python `shared_store.py`）。
        let path = user_config_dir(&environment)
            .join(shared_run_store_filename(&config.host, config.port));
        let shared = match SharedRunStore::new(path) {
            Ok(store) => store,
            Err(error) => {
                eprintln!(
                    "[api] 共享运行状态存储打开失败：{}",
                    error.to_api_error().message
                );
                return ExitCode::FAILURE;
            }
        };
        service = service.with_run_backend(RunBackend::Shared(shared));
    }
    let service = Arc::new(service);
    // 回合外事件泵：后台任务事件不必等到下一次回合才进会话级事件流。
    service.start_event_pump();
    let state = ApiState::new(config).with_service(Arc::clone(&service));
    let outcome = serve(state).await;
    // 正常退出（Ctrl-C）也收尾隔离工作区：apply 变更 + 按策略清理。
    service.close();
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(detail) => {
            eprintln!("[api] {detail}");
            ExitCode::FAILURE
        }
    }
}

/// 本进程是否由多 worker 监督进程拉起。
fn is_worker_child(environment: &ConfigEnvironment) -> bool {
    !environment.get_trimmed(WORKER_CHILD_ENV).is_empty()
}

/// 多 worker 监督进程：按 `api.workers` 拉起子进程，收到 Ctrl-C 后统一收尾。
///
/// 对齐 Python `uvicorn.run(..., workers=N)`：每个 worker 在独立进程里重新装载配置、
/// 起自己的内核与隔离工作区，跨进程状态经共享存储互通。子进程进程环境从父进程继承。
async fn run_worker_supervisor(config: &ApiConfig) -> ExitCode {
    let program = match std::env::current_exe() {
        Ok(program) => program,
        Err(error) => {
            eprintln!("[api] 无法定位当前可执行文件：{error}");
            return ExitCode::FAILURE;
        }
    };
    let mut children: Vec<std::process::Child> = Vec::new();
    for index in 1..=config.workers {
        // 子进程带上标记环境变量，避免它再拉起一层监督进程。
        match std::process::Command::new(&program)
            .env(WORKER_CHILD_ENV, index.to_string())
            .spawn()
        {
            Ok(child) => children.push(child),
            Err(error) => {
                eprintln!("[api] 启动 worker {index} 失败：{error}");
                stop_children(&mut children);
                return ExitCode::FAILURE;
            }
        }
    }
    println!(
        "[api] 已启动 {} 个 API worker（SO_REUSEPORT，监听 {}:{}）",
        children.len(),
        config.host,
        config.port
    );
    // 只等终止信号统一收尾（与 uvicorn 收到信号后停掉全部 worker 一致）。
    let _ = tokio::signal::ctrl_c().await;
    stop_children(&mut children);
    ExitCode::SUCCESS
}

/// 终止并回收所有 worker 子进程。
fn stop_children(children: &mut [std::process::Child]) {
    for child in children.iter_mut() {
        let _ = child.kill();
    }
    for child in children.iter_mut() {
        let _ = child.wait();
    }
}
