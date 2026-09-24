//! 真实链路的集成测试：Node Worker 的进程启动、握手、Hook 调用与回收，
//! 以及 `PluginManager` 从注册表引导到真实 Worker 再分发的完整接线。
//!
//! 这两条链路此前只有判定层的单测覆盖，「CLI 装得上、宿主起不来」这类断裂不会被发现。
//! 用例需要 Node.js 20+ 与 `rust/assets/extensions/node_runner.mjs`（旧的
//! `omnicrawl/extensions/` 布局仍兼容）：缺任一时跳过整组用例
//! （打印原因后直接返回），因此没有 JS 运行时的构建环境不会因此变红。
//!
//! 插件包直接用仓库里的真实 fixture（`tests/fixtures/npm_plugins/`），
//! 它没有 npm 依赖，`node_runner.mjs` 能直接加载。

use std::path::{Path, PathBuf};

use omnicrawl_extensions::manager::{DispatchRequest, PluginManager};
use omnicrawl_extensions::models::parse_plugins_config;
use omnicrawl_extensions::protocol::{PluginWorkerClient, WorkerConfig, WorkerLauncher};
use serde_json::{json, Map, Value};

/// 真实 fixture 插件的包名与 Handler（与 `package.json` 的 `omnicrawl` 段一致）。
const FIXTURE_NAME: &str = "@omnicrawl-fixture/sample-observe";
const FIXTURE_HANDLER: &str = "on-turn-end";
const FIXTURE_PERMISSION: &str = "hook:turn.end";

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../tests/fixtures/npm_plugins/sample-observe")
}

/// Node 与 runner 都在位时返回启动路径；否则打印原因并让调用方跳过。
fn launcher() -> Option<WorkerLauncher> {
    match WorkerLauncher::resolve() {
        Ok(launcher) => Some(launcher),
        Err(error) => {
            eprintln!("跳过真实 Worker 用例：{error}");
            None
        }
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let root =
        std::env::temp_dir().join(format!("omnicrawl-ext-{tag}-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("临时目录创建失败");
    root
}

/// 握手参数：与 `PluginManager::start_worker` 同形（apiVersion / 版本 / 已批准权限）。
fn handshake_params(permissions: &[&str]) -> Map<String, Value> {
    let mut params = Map::new();
    params.insert(
        "apiVersion".to_string(),
        Value::from(omnicrawl_extensions::models::HOOK_API_VERSION),
    );
    params.insert(
        "omnicrawlVersion".to_string(),
        Value::from(omnicrawl_extensions::models::OMNICRAWL_VERSION),
    );
    params.insert(
        "permissions".to_string(),
        Value::Array(permissions.iter().map(|item| Value::from(*item)).collect()),
    );
    params
}

/// 构造 `turn.end` 事件的载荷（与管理器构造的事件形状同义）。
fn turn_end_event() -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("turnId".to_string(), Value::from("turn-1"));
    let mut event = Map::new();
    event.insert("hook".to_string(), Value::from("turn.end"));
    event.insert("eventId".to_string(), Value::from("evt-1"));
    event.insert("timestamp".to_string(), Value::from("2026-01-01T00:00:00Z"));
    event.insert("payload".to_string(), Value::Object(payload));
    event
}

/// 注册表文档：把一个 dev-mode 插件指向真实 fixture 目录。
fn registry_document(plugin_root: &Path) -> Value {
    json!({
        "schemaVersion": 1,
        "plugins": {
            FIXTURE_NAME: {
                "enabled": true,
                "devMode": true,
                "localPath": plugin_root.to_string_lossy(),
                "approvedPermissions": [FIXTURE_PERMISSION],
            }
        },
        "disabledHandlers": [],
    })
}

#[test]
fn worker_starts_handshakes_invokes_and_recycles() {
    let Some(launcher) = launcher() else {
        return;
    };
    let root = fixture_root();
    assert!(root.is_dir(), "缺少 fixture 插件：{}", root.display());

    let client = PluginWorkerClient::new(WorkerConfig {
        plugin_root: root,
        plugin_name: FIXTURE_NAME.to_string(),
        timeout_ms: Some(8000),
        max_message_bytes: None,
        node_executable: Some(launcher.node_executable.clone()),
        runner_path: Some(launcher.runner_path.clone()),
        env: None,
        on_stderr: None,
        on_host_request: None,
    })
    .expect("客户端构造失败");
    assert!(!client.alive(), "未 start 时不应有子进程");

    client.start().expect("Worker 进程启动失败");
    assert!(client.alive(), "start 之后进程必须在运行");

    let handshake = client
        .initialize(&handshake_params(&[FIXTURE_PERMISSION]), Some(8000))
        .expect("握手失败");
    let handlers = handshake
        .get("handlers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        handlers.len(),
        1,
        "握手应回显 manifest 声明的 Handler：{handshake:?}"
    );
    assert_eq!(
        handlers[0].get("id").and_then(Value::as_str),
        Some(FIXTURE_HANDLER)
    );
    assert_eq!(
        handlers[0].get("hook").and_then(Value::as_str),
        Some("turn.end")
    );

    // 真实调用一次 Handler：fixture 返回 continue 并带注解。
    let outcome = client
        .invoke_handler(FIXTURE_HANDLER, &turn_end_event(), 8000)
        .expect("hook.invoke 失败");
    assert_eq!(
        outcome.get("action").and_then(Value::as_str),
        Some("continue"),
        "Handler 返回值：{outcome:?}"
    );

    client.shutdown(2000);
    assert!(!client.alive(), "shutdown 之后子进程必须已回收");
}

#[test]
fn client_restarts_worker_after_shutdown() {
    let Some(launcher) = launcher() else {
        return;
    };
    let client = PluginWorkerClient::new(WorkerConfig {
        plugin_root: fixture_root(),
        plugin_name: FIXTURE_NAME.to_string(),
        timeout_ms: Some(8000),
        node_executable: Some(launcher.node_executable.clone()),
        runner_path: Some(launcher.runner_path.clone()),
        ..WorkerConfig::default()
    })
    .expect("客户端构造失败");
    client.start().expect("首次启动失败");
    client.shutdown(2000);
    assert!(!client.alive());

    // 回收之后同一个客户端应能按需重启（`request` 里的惰性 start 路径）。
    let handshake = client
        .initialize(&handshake_params(&[FIXTURE_PERMISSION]), Some(8000))
        .expect("重启后握手失败");
    assert!(client.alive(), "重启后进程必须在运行");
    assert_eq!(
        handshake
            .get("handlers")
            .and_then(Value::as_array)
            .map(|items| items.len()),
        Some(1)
    );
    client.shutdown(2000);
    assert!(!client.alive());
}

#[test]
fn manager_bootstraps_and_dispatches_through_real_worker() {
    let Some(launcher) = launcher() else {
        return;
    };
    let workspace = temp_dir("manager-ws");
    let plugin_root = fixture_root();
    let user_registry = workspace.join("plugins.json");
    std::fs::write(&user_registry, registry_document(&plugin_root).to_string())
        .expect("注册表写入失败");

    let config =
        parse_plugins_config(Some(&json!({ "enabled": true }))).expect("plugins 配置解析失败");
    let manager = PluginManager::new(
        &workspace,
        config,
        Some(user_registry),
        Some(workspace.join("project-plugins.json")),
        Some(workspace.join("store")),
    )
    .with_launcher(Some(launcher));

    let diagnostics = manager.bootstrap();
    assert!(
        diagnostics
            .iter()
            .any(|line| line.contains("已加载 1 个插件")),
        "引导诊断：{diagnostics:?}"
    );
    assert_eq!(manager.current_plan_len(), 1, "执行计划应包含 1 个 Handler");

    let status = manager.list_status();
    assert_eq!(status.len(), 1);
    assert_eq!(
        status[0].get("active").and_then(Value::as_bool),
        Some(true),
        "Worker 应处于活跃状态：{:?}",
        status[0].get("lastError")
    );

    let mut payload = Map::new();
    payload.insert("turnId".to_string(), Value::from("turn-1"));
    let outcome = manager
        .dispatch(
            "turn.end",
            DispatchRequest {
                payload,
                ..DispatchRequest::default()
            },
        )
        .expect("分发失败");
    assert!(!outcome.denied, "turn.end 是通知类 Hook，不应被拒绝");
    assert_eq!(outcome.results.len(), 1, "应有一个 Handler 参与分发");
    assert_eq!(outcome.results[0].status, "continue");
    assert_eq!(
        outcome.results[0].handler_key,
        format!("{FIXTURE_NAME}/{FIXTURE_HANDLER}")
    );

    manager.close();
    assert!(
        manager.list_status().is_empty(),
        "close 之后不应残留 Worker"
    );
    let _ = std::fs::remove_dir_all(&workspace);
}

#[test]
fn missing_runner_reports_search_chain_instead_of_handshake_failure() {
    let Some(launcher) = launcher() else {
        return;
    };
    let workspace = temp_dir("missing-runner");
    let user_registry = workspace.join("plugins.json");
    std::fs::write(
        &user_registry,
        registry_document(&fixture_root()).to_string(),
    )
    .expect("注册表写入失败");

    let config =
        parse_plugins_config(Some(&json!({ "enabled": true }))).expect("plugins 配置解析失败");
    // 显式给出一个不存在的 runner：必须在启动插件之前一次性失败，
    // 而不是逐个插件报「握手失败」。
    let manager = PluginManager::new(
        &workspace,
        config,
        Some(user_registry),
        Some(workspace.join("project-plugins.json")),
        Some(workspace.join("store")),
    )
    .with_launcher(Some(WorkerLauncher::new(
        launcher.node_executable.clone(),
        workspace.join("不存在").join("node_runner.mjs"),
    )));

    let diagnostics = manager.bootstrap();
    assert!(
        diagnostics
            .iter()
            .any(|line| line.contains("缺少 Node runner")),
        "应给出 runner 缺失的诊断：{diagnostics:?}"
    );
    assert_eq!(manager.current_plan_len(), 0);
    manager.close();
    let _ = std::fs::remove_dir_all(&workspace);
}
