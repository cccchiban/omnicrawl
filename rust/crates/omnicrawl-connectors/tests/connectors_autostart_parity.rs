//! 与 Python `omnicrawl/connectors/autostart.py` 真实现的对照测试。
//!
//! 数据集由 `python rust/tools/gen_connectors_autostart_fixture.py` 生成：同一批「配置文件 +
//! 环境变量 + 采集日志开关」喂给真监督器（`Popen` 换成记录用的假实现），这里用假启动器重放
//! Rust 监督器并逐字段比对。改了任一侧都要重跑生成脚本。
//!
//! 不比对的部分（见 crate `README.md`）：子进程命令（Python 是 `python -m <模块>`，Rust 是
//! 宿主注入的「当前可执行文件 + `connector <平台>`」，这里只断言参数尾部对得上），以及
//! `PYTHONPATH` 的具体取值（Python 固定塞包父目录，Rust 由宿主注入；这里把 `pythonpath_root`
//! 设成临时根目录，只比对「是否注入」，取值语义由 `child_env_cases` 覆盖）。

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_connectors::autostart::{
    auto_start_decision, child_environment, default_connector_specs, start_configured_connectors,
    ConnectorManagerOptions, ConnectorProcessManager, ConnectorSpec, OutputTarget, ProcessSpawner,
    SpawnRequest, AUTO_START_ENV, CONNECTOR_LOG_DIRNAME, CONNECTOR_SUBCOMMAND, DISABLED_VALUES,
    ENABLED_VALUES, FEISHU_PLATFORM, TELEGRAM_PLATFORM,
};
use omnicrawl_workspace::process_control::{ManagedProcess, ProcessState};
use serde_json::Value;

/// 假进程的 PID：取一个不可能存在的极大值，避免 Windows 上 `taskkill` 打歪到真实进程。
const FAKE_PID: u32 = 4_294_967_280;
const ROOT_TOKEN: &str = "<ROOT>";
const WORKSPACE_TOKEN: &str = "<WORKSPACE>";

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("connectors_autostart_parity.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("读取 {} 失败：{error}", path.display()));
    serde_json::from_str(&text).expect("数据集必须是合法 JSON")
}

fn group<'a>(data: &'a Value, name: &str) -> &'a Vec<Value> {
    data.get(name)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("数据集缺少分组：{name}"))
}

fn text_of(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("缺少字符串字段 {key}：{value}"))
        .to_string()
}

fn bool_of(value: &Value, key: &str) -> bool {
    value
        .get(key)
        .and_then(Value::as_bool)
        .unwrap_or_else(|| panic!("缺少布尔字段 {key}：{value}"))
}

fn strings_of(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("缺少字符串数组字段 {key}：{value}"))
        .iter()
        .map(|item| item.as_str().expect("数组元素必须是字符串").to_string())
        .collect()
}

fn slash(text: &str) -> String {
    text.replace('\\', "/")
}

/// 记录启动请求、返回假进程的启动器。
#[derive(Default)]
struct FakeSpawner {
    requests: Mutex<Vec<SpawnRequest>>,
}

impl FakeSpawner {
    fn requests(&self) -> Vec<SpawnRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|item| item.into_inner())
            .clone()
    }
}

impl ProcessSpawner for FakeSpawner {
    fn spawn(&self, request: &SpawnRequest) -> io::Result<Box<dyn ManagedProcess>> {
        self.requests
            .lock()
            .unwrap_or_else(|item| item.into_inner())
            .push(request.clone());
        Ok(Box::new(FakeProcess::new()))
    }
}

/// 假子进程：状态从运行变成已退出，只在被终止时。
struct FakeProcess {
    state: ProcessState,
}

impl FakeProcess {
    fn new() -> Self {
        Self {
            state: ProcessState::Running,
        }
    }
}

impl ManagedProcess for FakeProcess {
    fn process_id(&self) -> u32 {
        FAKE_PID
    }

    fn state(&mut self) -> ProcessState {
        self.state
    }

    fn terminate(&mut self) {
        self.state = ProcessState::Exited(0);
    }

    fn kill(&mut self) {
        self.state = ProcessState::Exited(0);
    }

    fn raw_process_handle(&self) -> Option<isize> {
        None
    }
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut base = std::env::temp_dir();
        base.push(format!(
            "oc-autostart-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&base).expect("创建临时目录");
        Self {
            path: omnicrawl_workspace::resolve_path(&base),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 数据集里的 Python 模块 → Rust 侧缺省子命令尾段。
fn expected_subcommand(dataset_module: &str) -> String {
    if dataset_module.ends_with("telegram") {
        "telegram".to_string()
    } else {
        "feishu".to_string()
    }
}

#[test]
fn constants_match_python() {
    let data = fixture();
    let constants = data.get("constants").expect("缺少 constants");

    assert_eq!(AUTO_START_ENV, text_of(constants, "auto_start_env"));
    assert_eq!(
        CONNECTOR_LOG_DIRNAME,
        text_of(constants, "connector_log_dirname")
    );
    let mut disabled: Vec<String> = DISABLED_VALUES
        .iter()
        .map(|item| item.to_string())
        .collect();
    disabled.sort();
    assert_eq!(disabled, strings_of(constants, "disabled_values"));
    let mut enabled: Vec<String> = ENABLED_VALUES.iter().map(|item| item.to_string()).collect();
    enabled.sort();
    assert_eq!(enabled, strings_of(constants, "enabled_values"));

    let names = strings_of(constants, "platform_names");
    assert_eq!(names, vec![TELEGRAM_PLATFORM, FEISHU_PLATFORM]);
    let modules = strings_of(constants, "platform_modules");
    assert_eq!(modules.len(), 2, "数据集应当记录两个平台模块作为参考");

    let specs = default_connector_specs("/tmp/omnicrawl-connector");
    assert_eq!(specs.len(), 2);
    for (index, spec) in specs.iter().enumerate() {
        assert_eq!(spec.name, names[index]);
        // Python 是 `[python, "-m", <模块>]`，Rust 是 `[宿主程序, connector, <平台>]`：
        // 命令行本身不同源（见模块文档），这里只锁住「平台 → 子命令尾段」的对应关系。
        assert_eq!(
            spec.args,
            vec![
                CONNECTOR_SUBCOMMAND.to_string(),
                expected_subcommand(&modules[index])
            ],
            "缺省参数形状不一致：{}",
            spec.name
        );
        assert_eq!(spec.command_line().len(), 3);
    }

    // 日志路径：`<用户配置目录>/logs/<平台>.log`。
    for entry in group(&data, "log_paths") {
        let platform = text_of(entry, "platform");
        let root = TempDir::new("log-path");
        let env = ConfigEnvironment::new(root.path(), std::env::consts::OS);
        let path = omnicrawl_connectors::autostart::connector_log_path(
            &omnicrawl_connectors::autostart::default_log_root(&env),
            &platform,
        );
        assert_eq!(
            path.file_name()
                .map(|item| item.to_string_lossy().to_string()),
            Some(text_of(entry, "filename")),
            "日志文件名不一致：{platform}"
        );
        assert_eq!(
            path.parent()
                .and_then(|item| item.file_name())
                .map(|item| item.to_string_lossy().to_string()),
            Some(CONNECTOR_LOG_DIRNAME.to_string())
        );
    }
}

#[test]
fn auto_start_decisions_match_python() {
    let data = fixture();
    for case in group(&data, "auto_start_decisions") {
        let raw = case.get("raw").and_then(Value::as_str);
        let mut env = ConfigEnvironment::new(std::env::temp_dir(), std::env::consts::OS);
        if let Some(value) = raw {
            env = env.with_env_value(AUTO_START_ENV, value);
        }
        let decision = auto_start_decision(&env);
        // 环境变量开关已移除：注入任何取值都不改变结论，也不再告警。
        assert!(
            decision.enabled,
            "自动启动恒为启用：{raw:?}"
        );
        assert!(
            decision.warning.is_none(),
            "不再有环境变量开关，也就没有告警：{raw:?}"
        );
    }
}

#[test]
fn child_environment_matches_python() {
    let data = fixture();
    for case in group(&data, "child_env_cases") {
        let name = text_of(case, "name");
        let base: BTreeMap<String, String> = case
            .get("base")
            .and_then(Value::as_object)
            .expect("缺少 base")
            .iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    value.as_str().expect("base 值是字符串").to_string(),
                )
            })
            .collect();
        let base_pairs: Vec<(String, String)> = base
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();

        let root = TempDir::new("child-env");
        let actual: BTreeMap<String, String> = child_environment(&base_pairs, Some(root.path()))
            .into_iter()
            .map(|(key, value)| {
                (
                    key,
                    value.replace(&root.path().to_string_lossy().to_string(), ROOT_TOKEN),
                )
            })
            .collect();
        let expected: BTreeMap<String, String> = case
            .get("pairs")
            .and_then(Value::as_array)
            .expect("缺少 pairs")
            .iter()
            .map(|pair| {
                let pair = pair.as_array().expect("pairs 元素是数组");
                (
                    pair[0].as_str().expect("环境变量名").to_string(),
                    pair[1].as_str().expect("环境变量值").to_string(),
                )
            })
            .collect();
        assert_eq!(actual, expected, "子进程环境不一致：{name}");
        // 未注入根目录时不碰 PYTHONPATH（Rust 侧的宿主注入点，Python 没有这个模式）。
        let untouched = child_environment(&base_pairs, None);
        assert!(
            untouched.iter().all(|(key, value)| key != "PYTHONPATH"
                || Some(value.as_str()) == base.get("PYTHONPATH").map(|item| item.as_str())),
            "缺省不应改写 PYTHONPATH：{name}"
        );
    }

    // 既有 PYTHONPATH 时原地更新而不是追加到末尾（与 Python 的 dict 写入位置一致）。
    let root = TempDir::new("child-env-order");
    let pairs = child_environment(
        &[
            ("PYTHONPATH".to_string(), "/old".to_string()),
            ("KEEP".to_string(), "1".to_string()),
        ],
        Some(root.path()),
    );
    assert_eq!(pairs.len(), 2);
    assert_eq!(pairs[0].0, "PYTHONPATH");
    assert!(pairs[0].1.ends_with(";/old") || pairs[0].1.ends_with(":/old"));
    assert_eq!(pairs[1], ("KEEP".to_string(), "1".to_string()));
}

#[test]
fn scenarios_match_python() {
    let data = fixture();
    let root = TempDir::new("scenarios");

    for case in group(&data, "scenarios") {
        let name = text_of(case, "name");
        let workspace = root.join("workspaces").join(&name);
        std::fs::create_dir_all(&workspace).expect("创建工作区");

        let config_path = root.join("configs").join(format!("{name}.toml"));
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent).expect("创建配置目录");
        }
        match case.get("config") {
            Some(Value::String(content)) => {
                std::fs::write(&config_path, content.as_bytes()).expect("写配置")
            }
            _ => {
                let _ = std::fs::remove_file(&config_path);
            }
        }

        let mut env = ConfigEnvironment::new(root.path(), std::env::consts::OS);
        if let Some(overrides) = case.get("env_overrides").and_then(Value::as_object) {
            for (key, value) in overrides {
                env = env.with_env_value(key, value.as_str().expect("环境变量值是字符串"));
            }
        }

        let spawner = Arc::new(FakeSpawner::default());
        let options = ConnectorManagerOptions::new(env)
            .with_spawner(Arc::clone(&spawner) as Arc<dyn ProcessSpawner>)
            .with_capture_logs(bool_of(case, "capture_logs"))
            .with_config_path(Some(config_path))
            .with_pythonpath_root(Some(root.path().to_path_buf()));
        let manager = ConnectorProcessManager::new(workspace.as_path(), options);

        assert_eq!(
            manager.capture_logs(),
            bool_of(case, "capture_logs_flag"),
            "采集日志开关不一致：{name}"
        );
        let started = manager.start();
        assert_eq!(
            started,
            strings_of(case, "started"),
            "启动的平台不一致：{name}"
        );
        assert_eq!(
            manager.diagnostics(),
            strings_of(case, "diagnostics"),
            "诊断文案不一致：{name}"
        );

        let expected_spawns = group(case, "spawns");
        let requests = spawner.requests();
        assert_eq!(
            requests.len(),
            expected_spawns.len(),
            "启动请求数量不一致：{name}"
        );
        for (request, expected) in requests.iter().zip(expected_spawns) {
            assert_eq!(
                slash(&request.cwd.to_string_lossy()),
                slash(&text_of(expected, "cwd"))
                    .replace(WORKSPACE_TOKEN, &slash(&root.path().to_string_lossy())),
                "子进程工作目录不一致：{name}"
            );
            assert_eq!(
                request.stdin_null,
                bool_of(expected, "stdin_is_devnull"),
                "stdin 不一致：{name}"
            );
            match &request.stdout {
                OutputTarget::Null => assert!(
                    bool_of(expected, "stdout_is_devnull"),
                    "stdout 应当是 DEVNULL：{name}"
                ),
                OutputTarget::Append(path) => assert_eq!(
                    path.file_name()
                        .map(|item| item.to_string_lossy().to_string()),
                    expected
                        .get("log_name")
                        .and_then(Value::as_str)
                        .map(|item| item.to_string()),
                    "日志文件名不一致：{name}"
                ),
                OutputTarget::MergeIntoStdout => panic!("stdout 不该并入自身：{name}"),
            }
            if bool_of(expected, "stderr_merged_into_stdout") {
                assert_eq!(
                    request.stderr,
                    OutputTarget::MergeIntoStdout,
                    "stderr 应当并入 stdout：{name}"
                );
            } else {
                assert!(
                    bool_of(expected, "stderr_is_devnull"),
                    "数据集里 stderr 只有「并入 stdout」与「丢弃」两种"
                );
                assert_eq!(request.stderr, OutputTarget::Null, "stderr 不一致：{name}");
            }
            assert_eq!(
                request.new_process_group,
                bool_of(expected, "new_process_group"),
                "独立进程组不一致：{name}"
            );
            assert_eq!(
                request.env_value("PYTHONPATH").is_some(),
                bool_of(expected, "pythonpath_present"),
                "PYTHONPATH 注入与否不一致：{name}"
            );
            // 尾部参数与平台对应（命令本身两侧不同源，见模块文档）。
            let module = text_of(expected, "module");
            assert_eq!(
                request.args,
                vec![
                    CONNECTOR_SUBCOMMAND.to_string(),
                    expected_subcommand(&module)
                ],
                "子命令尾部不一致：{name}"
            );
        }

        manager.close();
        assert_eq!(
            manager.started_connectors(),
            strings_of(case, "started_after_close"),
            "关闭后已启动平台列表不一致：{name}"
        );
        assert_eq!(
            manager.diagnostics(),
            strings_of(case, "diagnostics_after_close"),
            "关闭后诊断文案不一致：{name}"
        );
    }
}

#[test]
fn production_entry_enables_log_capture() {
    // `start_configured_connectors`（生产入口）默认把子进程输出落盘；这里用假启动器验证它
    // 真的把开关打开并拉起「已配置」的平台，不碰真实进程。
    let root = TempDir::new("entry");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("创建工作区");
    let spawner = Arc::new(FakeSpawner::default());
    let env = ConfigEnvironment::new(root.path(), std::env::consts::OS);
    let options = ConnectorManagerOptions::new(env)
        .with_spawner(Arc::clone(&spawner) as Arc<dyn ProcessSpawner>)
        .with_config_path(Some(root.join("config.toml")))
        .with_specs(vec![ConnectorSpec::new(
            "Custom",
            root.join("connector"),
            vec![CONNECTOR_SUBCOMMAND.to_string(), "custom".to_string()],
        )]);

    let manager = start_configured_connectors(options, workspace.as_path());
    assert!(manager.capture_logs());
    assert_eq!(manager.started_connectors(), vec!["Custom".to_string()]);
    assert_eq!(spawner.requests().len(), 1);
    manager.close();
    assert_eq!(manager.started_connectors(), vec!["Custom".to_string()]);
}
