//! `config_edit`：读取与修改用户配置目录下的 TOML 配置。
//!
//! 单个工具承担两件事：`action=read` 读出配置（凭据值打码），`action=set` 按点分路径
//! 写入一个值。四个配置文件都走 `omnicrawl-config` 的路径解析与原子写盘，与设置面板
//! 写的是同一批文件、同一套 TOML 形状。
//!
//! 路径解析与凭据打码都是本模块的纯逻辑，配置环境由调用方注入（测试用隔离环境）。

use std::path::PathBuf;

use omnicrawl_config::core::runtime::{
    atomic_write_text, dump_toml_text, load_config_data, load_raw_file, resolve_config_path,
    resolve_decision_models_path, resolve_decision_models_write_path, resolve_models_path,
    resolve_models_write_path, resolve_subagents_path, resolve_subagents_write_path,
    save_config_data, ConfigEnvironment,
};
use omnicrawl_config::toml::{Table, Value};
use omnicrawl_config::value::python_repr;
use serde_json::{Map, Value as JsonValue};

use super::error::{ToolError, ToolOutcome};

/// 打码后的占位值：键名与结构照旧，只有值被替换。
const MASKED: &str = "<已隐藏>";

/// 值本身是凭据的键名（精确匹配，`api_key_env` 这类指针不算）。
const CREDENTIAL_KEYS: [&str; 7] = [
    "api_key",
    "bearer_token",
    "token",
    "cookie",
    "authorization",
    "password",
    "secret",
];

/// 可读写的配置文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// 运行配置 `config.toml`。
    Config,
    /// 模型配置 `models.toml`。
    Models,
    /// 子代理设置 `subagents.toml`。
    Subagents,
    /// 决策模型配置 `decision_models.toml`。
    DecisionModels,
}

impl Target {
    /// 省略 `file` 时读出全部配置的顺序。
    const ALL: [Target; 4] = [
        Target::Config,
        Target::Models,
        Target::Subagents,
        Target::DecisionModels,
    ];

    /// 解析 `file` 取值。
    fn parse(raw: &str) -> Result<Self, ToolError> {
        match raw.trim() {
            "config" => Ok(Target::Config),
            "models" => Ok(Target::Models),
            "subagents" => Ok(Target::Subagents),
            "decision_models" => Ok(Target::DecisionModels),
            other => Err(ToolError::new(format!(
                "file 只支持 config / models / subagents / decision_models，收到：{other}"
            ))),
        }
    }

    /// 文件名，用于报错与回执。
    fn file_name(&self) -> &'static str {
        match self {
            Target::Config => "config.toml",
            Target::Models => "models.toml",
            Target::Subagents => "subagents.toml",
            Target::DecisionModels => "decision_models.toml",
        }
    }

    /// 展示名，用于整份读取时的分节标题。
    fn label(&self) -> &'static str {
        match self {
            Target::Config => "运行配置",
            Target::Models => "模型配置",
            Target::Subagents => "子代理设置",
            Target::DecisionModels => "决策模型配置",
        }
    }

    /// 读路径。
    fn read_path(&self, env: &ConfigEnvironment) -> Result<PathBuf, omnicrawl_config::ConfigError> {
        match self {
            Target::Config => resolve_config_path(env, None),
            Target::Models => resolve_models_path(env, None),
            Target::Subagents => resolve_subagents_path(env, None),
            Target::DecisionModels => resolve_decision_models_path(env, None),
        }
    }

    /// 读出文档；文件不存在视为空表（`config.toml` 的遗留 JSON 检测在 `load_config_data` 里）。
    fn load(&self, env: &ConfigEnvironment) -> Result<Table, omnicrawl_config::ConfigError> {
        if *self == Target::Config {
            return load_config_data(env, None);
        }
        let path = self.read_path(env)?;
        if !path.exists() {
            return Ok(Table::new());
        }
        load_raw_file(&path)
    }

    /// 原子写回，返回落盘路径。
    fn save(
        &self,
        env: &ConfigEnvironment,
        data: &Table,
    ) -> Result<PathBuf, omnicrawl_config::ConfigError> {
        if *self == Target::Config {
            return save_config_data(env, data, None);
        }
        let path = match self {
            Target::Models => resolve_models_write_path(env, None),
            Target::Subagents => resolve_subagents_write_path(env, None),
            Target::DecisionModels => resolve_decision_models_write_path(env, None),
            Target::Config => unreachable!("config 已在上方分支返回"),
        }?;
        atomic_write_text(&path, &dump_toml_text(data))?;
        Ok(path)
    }
}

/// 读取或修改配置。
pub fn config_edit(env: &ConfigEnvironment, arguments: &Map<String, JsonValue>) -> ToolOutcome {
    let action = arguments
        .get("action")
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .unwrap_or_default();
    match action {
        "read" => read(env, file_argument(arguments)?),
        "set" => set(env, arguments, file_argument(arguments)?),
        "" => Err(ToolError::new("缺少必填字段 action。")),
        other => Err(ToolError::new(format!(
            "action 只支持 read / set，收到：{other}"
        ))),
    }
}

/// 解析可选参数 `file`：省略表示「按各 action 的默认范围处理」。
fn file_argument(arguments: &Map<String, JsonValue>) -> Result<Option<Target>, ToolError> {
    let Some(raw) = arguments.get("file") else {
        return Ok(None);
    };
    let Some(name) = raw.as_str() else {
        return Err(ToolError::new("file 必须是字符串。"));
    };
    if name.trim().is_empty() {
        return Err(ToolError::new(
            "file 不能为空；省略该参数表示按默认范围处理。",
        ));
    }
    Target::parse(name).map(Some)
}

/// 读出目标文件（未指定则逐份读出全部配置），凭据打码。
fn read(env: &ConfigEnvironment, target: Option<Target>) -> ToolOutcome {
    let targets = match target {
        Some(target) => vec![target],
        None => Target::ALL.to_vec(),
    };
    let mut blocks: Vec<String> = Vec::new();
    for target in targets {
        let path = target.read_path(env).map_err(config_error)?;
        let data = target.load(env).map_err(config_error)?;
        let text = dump_toml_text(&mask(&data));
        if text.trim().is_empty() {
            blocks.push(format!(
                "{}：{} 为空或不存在。",
                target.label(),
                path.display()
            ));
        } else {
            blocks.push(format!("{}：{}\n{}", target.label(), path.display(), text));
        }
    }
    Ok(blocks.join("\n"))
}

/// 按点分路径写入一个值；未指定 `file` 时写主配置 `config.toml`。
fn set(
    env: &ConfigEnvironment,
    arguments: &Map<String, JsonValue>,
    target: Option<Target>,
) -> ToolOutcome {
    let path = arguments
        .get("path")
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if path.is_empty() {
        return Err(ToolError::new("action=set 时必须提供 path。"));
    }
    let Some(raw) = arguments.get("value") else {
        return Err(ToolError::new("action=set 时必须提供 value。"));
    };
    let value = toml_value(raw)?;
    let target = target.unwrap_or(Target::Config);

    let mut data = target.load(env).map_err(config_error)?;
    let parts: Vec<&str> = path.split('.').map(str::trim).collect();
    set_path(&mut data, &parts, value.clone())?;
    target.save(env, &data).map_err(config_error)?;
    Ok(format!(
        "已更新 {} 的 {path}：{}",
        target.file_name(),
        python_repr(&value)
    ))
}

/// JSON 取值 → TOML 值；`null` 没有对应的 TOML 写法，直接拒绝。
fn toml_value(raw: &JsonValue) -> Result<Value, ToolError> {
    if raw.is_null() {
        return Err(ToolError::new("value 不能是 null。"));
    }
    if !raw.is_object() && !raw.is_array() {
        return Ok(omnicrawl_config::value::json_to_toml(raw));
    }
    Ok(Value::Table(omnicrawl_config::value::json_object_to_table(
        raw,
    )))
}

/// 按点分路径写入 TOML 文档；中间层缺失则补齐空表，遇到非表值则报错。
fn set_path(data: &mut Table, parts: &[&str], value: Value) -> Result<(), ToolError> {
    let Some((head, rest)) = parts.split_first() else {
        return Err(ToolError::new("配置路径为空。"));
    };
    if head.is_empty() {
        return Err(ToolError::new("配置路径里有空的层级。"));
    }
    if rest.is_empty() {
        data.insert((*head).to_string(), value);
        return Ok(());
    }
    if !data.contains_key(*head) {
        data.insert((*head).to_string(), Value::Table(Table::new()));
    }
    match data.get_mut(*head) {
        Some(Value::Table(section)) => set_path(section, rest, value),
        _ => Err(ToolError::new(format!("配置路径不是对象：{head}"))),
    }
}

/// 逐层复制文档并把凭据值换成占位符；键名与结构保持不变。
fn mask(data: &Table) -> Table {
    let mut masked = Table::new();
    for (key, value) in data {
        let next = match value {
            Value::Table(inner) => Value::Table(mask(inner)),
            _ if is_credential(key) => Value::String(MASKED.to_string()),
            value => value.clone(),
        };
        masked.insert(key.clone(), next);
    }
    masked
}

/// 键名是否代表一个凭据值。
fn is_credential(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    CREDENTIAL_KEYS.contains(&lowered.as_str())
}

fn config_error(error: omnicrawl_config::ConfigError) -> ToolError {
    ToolError::new(error.message())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 建一个隔离的 `~/.OmniCrawl` 并写入给定文件。
    fn home(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join(".OmniCrawl");
        std::fs::create_dir_all(&dir).expect("建配置目录");
        for (file, text) in files {
            std::fs::write(dir.join(file), text).expect("写测试配置");
        }
        root
    }

    fn args(value: JsonValue) -> Map<String, JsonValue> {
        value.as_object().cloned().unwrap_or_default()
    }

    fn read_file(root: &PathBuf, file: &str) -> String {
        std::fs::read_to_string(root.join(".OmniCrawl").join(file)).expect("读回配置")
    }

    #[test]
    fn read_masks_credentials_and_keeps_the_rest() {
        let root = home(
            "omnicrawl-config-tool-read",
            &[(
                "config.toml",
                "[llm.profiles.channel]\napi_key = \"sk-secret\"\napi_key_env = \"OPENAI_API_KEY\"\n\n[approval]\nmode = \"review\"\n",
            )],
        );
        let environment = ConfigEnvironment::new(&root, "windows");
        let output = config_edit(
            &environment,
            &args(json!({"action": "read", "file": "config"})),
        )
        .expect("读取成功");
        assert!(output.contains("api_key = \"<已隐藏>\""), "{output}");
        assert!(
            output.contains("api_key_env = \"OPENAI_API_KEY\""),
            "环境变量名不是凭据：{output}"
        );
        assert!(output.contains("mode = \"review\""), "{output}");
        assert!(
            !output.contains("sk-secret"),
            "凭据不得出现在输出里：{output}"
        );
    }

    #[test]
    fn read_without_file_covers_every_configuration() {
        let root = home(
            "omnicrawl-config-tool-read-all",
            &[
                ("config.toml", "[approval]\nmode = \"review\"\n"),
                (
                    "models.toml",
                    "[llm.defaults]\ncontext_window_tokens = 128000\n",
                ),
                ("subagents.toml", "[subagents]\nenabled = false\n"),
                (
                    "decision_models.toml",
                    "[decision_model]\napi_key = \"dv-secret\"\n",
                ),
            ],
        );
        let environment = ConfigEnvironment::new(&root, "windows");
        let output = config_edit(&environment, &args(json!({"action": "read"}))).expect("读取成功");
        for expected in [
            "运行配置",
            "模型配置",
            "子代理设置",
            "决策模型配置",
            "context_window_tokens = 128000",
        ] {
            assert!(output.contains(expected), "{expected} 缺失：{output}");
        }
        assert!(
            !output.contains("dv-secret"),
            "决策模型的凭据同样要打码：{output}"
        );
    }

    #[test]
    fn set_creates_missing_layers_and_reports_the_value() {
        let root = home("omnicrawl-config-tool-set", &[]);
        let environment = ConfigEnvironment::new(&root, "windows");
        let output = config_edit(
            &environment,
            &args(
                json!({"action": "set", "path": "llm.defaults.reasoning_effort", "value": "high"}),
            ),
        )
        .expect("写入成功");
        assert_eq!(
            output,
            "已更新 config.toml 的 llm.defaults.reasoning_effort：'high'"
        );
        let written = read_file(&root, "config.toml");
        assert!(written.contains("reasoning_effort = \"high\""), "{written}");
        // 再读一次能读回刚写的值（读与写是同一份文件）。
        let read_back =
            config_edit(&environment, &args(json!({"action": "read"}))).expect("读取成功");
        assert!(
            read_back.contains("reasoning_effort = \"high\""),
            "{read_back}"
        );
    }

    #[test]
    fn set_targets_each_file_by_name() {
        let root = home(
            "omnicrawl-config-tool-targets",
            &[
                ("config.toml", "[approval]\nmode = \"review\"\n"),
                (
                    "models.toml",
                    "[llm.defaults]\ncontext_window_tokens = 128000\n",
                ),
                ("subagents.toml", "[subagents]\nenabled = false\n"),
                ("decision_models.toml", "[features]\nreview = false\n"),
            ],
        );
        let environment = ConfigEnvironment::new(&root, "windows");
        for (file, path, value) in [
            (
                "models",
                "llm.defaults.context_window_tokens",
                json!(200000),
            ),
            ("subagents", "subagents.enabled", json!(true)),
            ("decision_models", "features.review", json!(true)),
        ] {
            let output = config_edit(
                &environment,
                &args(json!({"action": "set", "file": file, "path": path, "value": value})),
            )
            .expect("写入成功");
            assert!(
                output.starts_with(&format!("已更新 {file}.toml 的 {path}")),
                "{output}"
            );
        }
        // 只改目标文件：其余三份保持原样。
        assert_eq!(
            read_file(&root, "config.toml"),
            "[approval]\nmode = \"review\"\n"
        );
        assert!(read_file(&root, "models.toml").contains("context_window_tokens = 200000"));
        assert!(read_file(&root, "subagents.toml").contains("enabled = true"));
        assert!(read_file(&root, "decision_models.toml").contains("review = true"));
    }

    #[test]
    fn set_refuses_an_unknown_file() {
        let root = home("omnicrawl-config-tool-bad-file", &[]);
        let environment = ConfigEnvironment::new(&root, "windows");
        let error = config_edit(
            &environment,
            &args(json!({"action": "read", "file": "secrets"})),
        )
        .expect_err("未知 file 应当报错");
        assert!(error.message.contains("file 只支持"), "{}", error.message);
        assert!(
            config_edit(&environment, &args(json!({"action": "read", "file": ""})))
                .expect_err("空 file 应当报错")
                .message
                .contains("file 不能为空")
        );
    }

    #[test]
    fn set_refuses_a_path_that_crosses_a_scalar() {
        let root = home(
            "omnicrawl-config-tool-cross",
            &[("config.toml", "[approval]\nmode = \"review\"\n")],
        );
        let environment = ConfigEnvironment::new(&root, "windows");
        let error = config_edit(
            &environment,
            &args(json!({"action": "set", "path": "approval.mode.deep", "value": 1})),
        )
        .expect_err("跨标量写入应当报错");
        assert!(
            error.message.contains("配置路径不是对象"),
            "{}",
            error.message
        );
    }

    #[test]
    fn missing_or_unknown_action_is_reported() {
        let environment = ConfigEnvironment::new("unused", "windows");
        assert!(config_edit(&environment, &args(json!({})))
            .expect_err("缺 action 应当报错")
            .message
            .contains("缺少必填字段 action"));
        assert!(
            config_edit(&environment, &args(json!({"action": "delete"})))
                .expect_err("未知 action 应当报错")
                .message
                .contains("只支持 read / set")
        );
    }

    #[test]
    fn boolean_and_null_values_are_handled() {
        let root = home("omnicrawl-config-tool-bool", &[]);
        let environment = ConfigEnvironment::new(&root, "windows");
        let output = config_edit(
            &environment,
            &args(json!({"action": "set", "path": "memory.enabled", "value": true})),
        )
        .expect("写入成功");
        assert_eq!(output, "已更新 config.toml 的 memory.enabled：True");
        assert!(config_edit(
            &environment,
            &args(json!({"action": "set", "path": "memory.enabled", "value": null}))
        )
        .expect_err("null 应当被拒绝")
        .message
        .contains("不能是 null"));
    }
}
