//! 与 Python `omnicrawl/cli.py` 参数面的对照测试。
//!
//! 数据集由 `python rust/tools/gen_plugin_cli_fixture.py` 生成：同一批 argv 喂给
//! `build_parser().parse_args()`，记录解析出的 namespace 或 argparse 的退出码；同一批 argv
//! 再喂给 `_resolve_scope`，记录三种工作区形态下的作用域结论。改了任一侧都要重跑生成脚本。
//!
//! 只对照「参数面 + 作用域」：安装 / 注册表 / 诊断要真跑 npm 与网络，由
//! `omnicrawl-extensions` 的对照测试负责。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_entry::cli::{
    parse_plugin_args, resolve_scope, run_plugin_cli, CliError, PluginArgs, EXIT_ATOMIC,
    EXIT_MANIFEST, EXIT_NODE, EXIT_OK, EXIT_REGISTRY_NET, EXIT_SMOKE, EXIT_USAGE, EXIT_USER_CANCEL,
};
use serde_json::Value;

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("plugin_cli_parity.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("读取 {} 失败：{error}", path.display()));
    serde_json::from_str(&text).expect("数据集必须是合法 JSON")
}

fn group<'a>(data: &'a Value, name: &str) -> &'a Vec<Value> {
    data.get(name)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("数据集缺少分组：{name}"))
}

fn argv_of(case: &Value) -> Vec<String> {
    case.get("argv")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| item.as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// 把 Rust 侧的解析结果摊成与 Python namespace 同名的字段值。
fn field(args: &PluginArgs, key: &str) -> Value {
    match key {
        "command" => Value::from(args.command.clone()),
        "plugin_command" => Value::from(args.plugin_command.clone()),
        "resume" => Value::from(args.resume.clone()),
        "project" => Value::from(args.project),
        "user" => Value::from(args.user),
        "enable" => Value::from(args.enable),
        "yes" => Value::from(args.yes),
        "dev" => Value::from(args.dev),
        "all" => Value::from(args.all),
        "json" => Value::from(args.json),
        "purge" => Value::from(args.purge),
        "no_activate" => Value::from(args.no_activate),
        "name" => Value::from(args.name.clone()),
        "package_spec" => Value::from(args.package_spec.clone()),
        "action" => Value::from(args.action.clone()),
        "to_version" => Value::from(args.to_version.clone()),
        other => panic!("数据集出现未映射字段：{other}"),
    }
}

fn exit_code(case: &Value) -> u64 {
    case.get("exit")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("缺少 exit：{case}"))
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut base = std::env::temp_dir();
        base.push(format!(
            "oc-plugin-cli-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&base).expect("创建临时目录");
        Self { path: base }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn parse_cases_match_python() {
    let data = fixture();
    for case in group(&data, "parse") {
        let argv = argv_of(case);
        let parsed = parse_plugin_args(&argv);
        if let Some(expected) = case.get("args").and_then(Value::as_object) {
            let args = match parsed {
                Ok(args) => args,
                Err(error) => panic!("期望解析成功，实际 {error:?}：{case}"),
            };
            // 只比对 Python namespace 里真实出现的字段：argparse 只为当前子命令声明选项，
            // 未被声明的字段在 Rust 结构体里存在（默认值），不应参与比对。
            for (key, expected) in expected {
                assert_eq!(
                    field(&args, key),
                    *expected,
                    "字段 {key} 不一致（argv={argv:?}）"
                );
            }
            continue;
        }
        let code = exit_code(case);
        match code {
            0 => assert!(
                matches!(parsed, Err(CliError::Help(_))),
                "帮助应走 Help 分支（argv={argv:?}）"
            ),
            // argparse 的参数错误一律以 2 退出；`_resolve_scope` 的 SystemExit(2) 也在这一档。
            _ => assert!(
                matches!(parsed, Err(CliError::Usage(_)) | Err(CliError::SilentUsage)),
                "参数错误应以 {EXIT_USAGE} 退出（argv={argv:?}），实际 {parsed:?}"
            ),
        }
    }
}

#[test]
fn scope_cases_match_python() {
    let data = fixture();
    let root = TempDir::new("scope");
    let workspaces = {
        let empty = root.path().join("empty");
        let agents = root.path().join("agents");
        let package = root.path().join("package");
        for path in [&empty, &agents, &package] {
            std::fs::create_dir_all(path).expect("创建工作区");
        }
        std::fs::write(agents.join("AGENTS.md"), b"# agents\n").expect("写入 AGENTS.md");
        std::fs::write(package.join("package.json"), b"{}\n").expect("写入 package.json");
        std::collections::BTreeMap::from([
            ("empty".to_string(), empty),
            ("agents".to_string(), agents),
            ("package".to_string(), package),
        ])
    };

    for case in group(&data, "scope") {
        let argv = argv_of(case);
        let shape = case
            .get("workspace")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("缺少 workspace：{case}"));
        let workspace = workspaces
            .get(shape)
            .unwrap_or_else(|| panic!("未知工作区形态：{shape}"));
        let args = parse_plugin_args(&argv)
            .unwrap_or_else(|error| panic!("数据集里该 argv 应能解析：{argv:?}，实际 {error:?}"));
        let actual = resolve_scope(&args, workspace);
        match case.get("scope").and_then(Value::as_str) {
            Some(expected) => assert_eq!(
                actual.expect("应解析出作用域"),
                expected,
                "作用域不一致（argv={argv:?}，workspace={shape}）"
            ),
            None => {
                assert_eq!(exit_code(case), EXIT_USAGE as u64);
                assert!(
                    matches!(actual, Err(CliError::SilentUsage)),
                    "`--project` 与 `--user` 同时给出应静默以 {EXIT_USAGE} 退出（argv={argv:?}）"
                );
            }
        }
    }
}

#[test]
fn exit_codes_match_python() {
    let data = fixture();
    let expected = data
        .get("exit_codes")
        .and_then(Value::as_object)
        .expect("数据集缺少 exit_codes");
    let pairs: [(&str, i32); 8] = [
        ("ok", EXIT_OK),
        ("usage", EXIT_USAGE),
        ("node", EXIT_NODE),
        ("registry_net", EXIT_REGISTRY_NET),
        ("manifest", EXIT_MANIFEST),
        ("user_cancel", EXIT_USER_CANCEL),
        ("atomic", EXIT_ATOMIC),
        ("smoke", EXIT_SMOKE),
    ];
    for (name, code) in pairs {
        assert_eq!(
            expected.get(name).and_then(Value::as_i64),
            Some(i64::from(code)),
            "退出码 {name} 不一致"
        );
    }
}

#[test]
fn non_plugin_argv_is_not_handled() {
    let environment = ConfigEnvironment::from_process();
    assert!(run_plugin_cli(&[], &environment, None).is_none());
    assert!(run_plugin_cli(&["list".to_string()], &environment, None).is_none());
    // `plugin` 只有前缀相同也算不匹配。
    assert!(run_plugin_cli(&["plugins".to_string()], &environment, None).is_none());
}
