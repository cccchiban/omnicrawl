//! 配置对话服务（对映 `omnicrawl/config_chat/service.py`）：本地推理 → 类型校验 →
//! TOML 写回 → 运行态同步。
//!
//! 与 Python 的接缝只有两处，都是内核侧的常规做法：
//!
//! - **进程外信息注入**：配置路径与 `AI_*` 环境变量由 `omnicrawl-config` 的
//!   [`ConfigEnvironment`] 注入，而不是直接读 `os.environ` / `Path.home()`；
//! - **运行态目标**：Python 用 `getattr(agent, "set_*")` 探测宿主，Rust 换成
//!   [`ConfigChatAgent`] trait——默认空实现表达「宿主没实现这个方法」，`()` 表示没有宿主。

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use omnicrawl_config::core::runtime::{
    load_config_data, resolve_subagents_write_path, save_config_data, ConfigEnvironment,
};
use omnicrawl_config::toml::{Table, Value};

use crate::assets::{LabelsDocument, LABELS_FILENAME};
use crate::router::{ConfigRouter, ConfigRouterError};

/// 一条待执行的配置命令（对映 Python 的同名 dataclass）。
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigChatCommand {
    pub action: String,
    pub config: String,
    pub value: String,
    pub score: f64,
}

/// 一次成功的写回（对映 Python 的同名 dataclass）。
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigChange {
    pub path: String,
    pub value: Value,
}

/// 配置对话的输入或写回失败（对映 Python 的 `ConfigChatError(ValueError)`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigChatError {
    message: String,
}

impl ConfigChatError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ConfigChatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigChatError {}

/// 运行态同步目标（Python 侧的 agent）。
///
/// Python 用 `getattr(agent, "set_tts_enabled", lambda _value: None)` 探测：没实现就静默跳过。
/// 这里用默认空方法表达同一件事——宿主只实现它真正支持的 setter。
pub trait ConfigChatAgent {
    fn set_tts_enabled(&self, _value: &Value) {}
    fn set_memory_enabled(&self, _value: &Value) {}
    fn set_plugin_enabled(&self, _value: &Value) {}
    fn set_subagents_enabled(&self, _value: &Value) {}
    fn set_show_thinking(&self, _value: &Value) {}
    fn set_mcp_enabled(&self, _value: &Value) {}
    fn set_context_compaction_trigger_percent(&self, _value: &Value) {}
}

/// 没有宿主时的空实现（对映 Python 的 `agent=None`）。
impl ConfigChatAgent for () {}

/// 配置对话服务：懒加载路由器 + 标签表（类型校验的唯一依据）。
pub struct ConfigChatService<'a> {
    env: &'a ConfigEnvironment,
    assets_dir: PathBuf,
    /// 资源来自内嵌字节而不是磁盘目录（对映 Python 的包资源加载）。
    embedded: bool,
    agent: Option<&'a dyn ConfigChatAgent>,
    router: Option<ConfigRouter>,
    router_error: Option<String>,
    labels: HashMap<String, crate::assets::LabelEntry>,
}

impl<'a> ConfigChatService<'a> {
    /// 构造并立即装载标签表（对映 Python 的 `__init__` → `_load_schema`）。
    pub fn new(
        env: &'a ConfigEnvironment,
        assets_dir: impl Into<PathBuf>,
        agent: Option<&'a dyn ConfigChatAgent>,
    ) -> Self {
        let mut service = Self {
            env,
            assets_dir: assets_dir.into(),
            embedded: false,
            agent,
            router: None,
            router_error: None,
            labels: HashMap::new(),
        };
        service.load_schema();
        service
    }

    /// 用内嵌的内核资源构造（与 Python 的 `files("omnicrawl.config_chat.assets")` 同义）。
    ///
    /// 宿主脱离 Python 包后不再有资源目录可读，标签表、别名表与权重都随二进制分发。
    pub fn embedded(env: &'a ConfigEnvironment, agent: Option<&'a dyn ConfigChatAgent>) -> Self {
        let mut service = Self {
            env,
            assets_dir: PathBuf::new(),
            embedded: true,
            agent,
            router: None,
            router_error: None,
            labels: HashMap::new(),
        };
        service.load_schema();
        service
    }

    /// 内嵌资源：标签表 / 别名表 / 权重（与 `data/` 下的三份文件同源）。
    const EMBEDDED_LABELS: &'static str = include_str!("../data/labels.json");
    const EMBEDDED_ALIASES: &'static str = include_str!("../data/aliases.json");
    const EMBEDDED_WEIGHTS: &'static [u8] = include_bytes!("../data/config_router.bin");

    /// 标签表装载失败时只记下原因：路由器仍可尝试加载，`available` 才会给出结论。
    fn load_schema(&mut self) {
        let path = self.assets_dir.join(LABELS_FILENAME);
        let loaded = if self.embedded {
            LabelsDocument::parse(Self::EMBEDDED_LABELS)
        } else {
            std::fs::read_to_string(&path)
                .map_err(|error| format!("读取 {} 失败：{error}", path.display()))
                .and_then(|text| LabelsDocument::parse(&text))
        };
        match loaded {
            Ok(document) => self.labels = document.by_path(),
            Err(error) => {
                self.router_error = Some(format!("配置对话资源不可用：{error}"));
                self.labels = HashMap::new();
            }
        }
    }

    /// 是否可用（对映 Python 的 `available`：装载成功，或尚未失败过）。
    pub fn available(&self) -> bool {
        self.router.is_some() || self.router_error.is_none()
    }

    /// 已装载的配置路径 → 标签（弹层用它判断目录是否真的装载成功）。
    pub fn labels(&self) -> &HashMap<String, crate::assets::LabelEntry> {
        &self.labels
    }

    /// 不可用原因（对映 Python 的 `unavailable_reason`）。
    pub fn unavailable_reason(&self) -> String {
        self.router_error
            .clone()
            .unwrap_or_else(|| "配置对话模型尚未加载。".to_string())
    }

    fn router(&mut self) -> Result<&ConfigRouter, ConfigChatError> {
        if self.router.is_none() {
            let loaded = if self.embedded {
                ConfigRouter::from_sources(
                    Self::EMBEDDED_WEIGHTS,
                    Self::EMBEDDED_LABELS,
                    Self::EMBEDDED_ALIASES,
                )
            } else {
                ConfigRouter::load(&self.assets_dir)
            };
            match loaded {
                Ok(router) => self.router = Some(router),
                Err(error) => {
                    let message = match error {
                        // 权重不可用：文案直接进 UI（对映 `except ConfigRouterUnavailable`）。
                        ConfigRouterError::Unavailable(message) => message,
                        // 其余失败再加一层前缀（对映 `except Exception`）。
                        ConfigRouterError::Invalid(message) => {
                            format!("配置对话模型加载失败：{message}")
                        }
                    };
                    self.router_error = Some(message.clone());
                    return Err(ConfigChatError::new(message));
                }
            }
        }
        Ok(self.router.as_ref().expect("刚刚装载或此前已装载路由器"))
    }

    /// 一句话 → 命令列表；空白输入直接返回空表。
    pub fn predict(&mut self, text: &str) -> Result<Vec<ConfigChatCommand>, ConfigChatError> {
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let router = self.router()?;
        router
            .predict(text)
            .map_err(|error| ConfigChatError::new(format!("配置对话推理失败：{error}")))
    }

    /// 一句话 → 写回结果：先**全部**校验（任一条不合法则什么都不写），再逐条写盘并同步运行态。
    pub fn apply_text(&mut self, text: &str) -> Result<Vec<ConfigChange>, ConfigChatError> {
        let commands = self.predict(text)?;
        if commands.is_empty() {
            return Err(ConfigChatError::new("没有识别到可修改的配置。"));
        }
        let mut prepared: Vec<(ConfigChatCommand, Value)> = Vec::with_capacity(commands.len());
        for command in commands {
            let value = self.prepare_command(&command)?;
            prepared.push((command, value));
        }
        let mut changes: Vec<ConfigChange> = Vec::with_capacity(prepared.len());
        for (command, value) in prepared {
            self.write_value(&command.config, &value)?;
            self.sync_runtime(&command.config, &value);
            changes.push(ConfigChange {
                path: command.config,
                value,
            });
        }
        Ok(changes)
    }

    /// 校验一条命令是否可以执行，并折算成写入值（对映 Python 的 `_prepare_command`）。
    pub fn prepare_command(&self, command: &ConfigChatCommand) -> Result<Value, ConfigChatError> {
        if !self.labels.contains_key(&command.config) {
            return Err(ConfigChatError::new(format!(
                "不允许修改未知配置：{}",
                command.config
            )));
        }
        match command.action.as_str() {
            "OPEN" => {
                return Err(ConfigChatError::new(format!(
                    "“{}”是查看请求，不是修改请求。",
                    command.config
                )))
            }
            "TOGGLE" | "RESET" => {
                return Err(ConfigChatError::new(format!(
                    "暂不支持“{}”操作：{}",
                    command.action, command.config
                )))
            }
            _ => {}
        }
        self.coerce_value(&command.config, &command.value, &command.action)
    }

    /// 取值折算（对映 Python 的 `_coerce_value`）。
    ///
    /// 未知路径回落到 `str`：Python 侧此处会 `KeyError`，但服务层总是先过
    /// [`Self::prepare_command`] 的标签校验，正常路径不会走到。
    pub fn coerce_value(
        &self,
        path: &str,
        raw: &str,
        action: &str,
    ) -> Result<Value, ConfigChatError> {
        let kind = self
            .labels
            .get(path)
            .map(|item| item.type_name.as_str())
            .unwrap_or("str");
        match action {
            "ENABLE" => return Ok(Value::Boolean(true)),
            "DISABLE" => return Ok(Value::Boolean(false)),
            _ => {}
        }
        let value = raw.trim();
        if kind == "bool" {
            let lowered = value.to_lowercase();
            if matches!(
                lowered.as_str(),
                "true" | "1" | "on" | "开" | "开启" | "启用" | "打开"
            ) {
                return Ok(Value::Boolean(true));
            }
            if matches!(
                lowered.as_str(),
                "false" | "0" | "off" | "关" | "关闭" | "禁用"
            ) {
                return Ok(Value::Boolean(false));
            }
            return Err(ConfigChatError::new(format!(
                "{path} 需要布尔值，收到：{raw}"
            )));
        }
        if kind == "int" {
            return value
                .parse::<i64>()
                .map(Value::Integer)
                .map_err(|_| ConfigChatError::new(format!("{path} 需要 {kind}，收到：{raw}")));
        }
        if kind == "float" {
            return value
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| ConfigChatError::new(format!("{path} 需要 {kind}，收到：{raw}")));
        }
        if kind == "list" || kind == "dict" {
            return Err(ConfigChatError::new(format!(
                "暂不支持直接修改复合配置：{path}"
            )));
        }
        Ok(Value::String(value.to_string()))
    }

    /// 写回一条配置：`subagents.*` 落到子代理设置文件，其余落到运行配置。
    fn write_value(&self, path: &str, value: &Value) -> Result<(), ConfigChatError> {
        let target = if path.starts_with("subagents.") {
            Some(
                resolve_subagents_write_path(self.env, None)
                    .map_err(|error| ConfigChatError::new(error.to_string()))?,
            )
        } else {
            None
        };
        let mut data = load_config_data(self.env, target.as_deref())
            .map_err(|error| ConfigChatError::new(error.to_string()))?;
        let parts: Vec<&str> = path.split('.').collect();
        set_path(&mut data, &parts, value.clone())?;
        save_config_data(self.env, &data, target.as_deref())
            .map_err(|error| ConfigChatError::new(error.to_string()))?;
        Ok(())
    }

    /// 运行态同步：只有命中 setter 才动手。
    ///
    /// Python 侧在最后还对 `tts` / `image_gen` / `vision` / `desensitization` / `run_guard` /
    /// `agent_workspace` / `advisor` 段显式提前返回，但函数本身到此结束——语义等价于
    /// 「未命中 setter 就不做运行态同步」，复杂段由下次启动或设置页完整编辑器加载。
    fn sync_runtime(&self, path: &str, value: &Value) {
        let Some(agent) = self.agent else {
            return;
        };
        match path {
            "tts.enabled" => agent.set_tts_enabled(value),
            "memory.enabled" => agent.set_memory_enabled(value),
            "plugins.enabled" => agent.set_plugin_enabled(value),
            "subagents.enabled" => agent.set_subagents_enabled(value),
            "ui.show_thinking" => agent.set_show_thinking(value),
            "mcp.enabled" => agent.set_mcp_enabled(value),
            "context_compaction.trigger_context_percent" => {
                agent.set_context_compaction_trigger_percent(value)
            }
            _ => {}
        }
    }
}

/// 按点分路径写入 TOML 文档；中间层缺失则补齐空表，非表则报错（对映 Python 的 `_write_value` 导航段）。
fn set_path(data: &mut Table, parts: &[&str], value: Value) -> Result<(), ConfigChatError> {
    let Some((head, rest)) = parts.split_first() else {
        return Err(ConfigChatError::new("配置路径为空。"));
    };
    if rest.is_empty() {
        data.insert((*head).to_string(), value);
        return Ok(());
    }
    if !data.contains_key(*head) {
        data.insert((*head).to_string(), Value::Table(Table::new()));
    }
    match data.get_mut(*head) {
        Some(Value::Table(section)) => set_path(section, rest, value),
        _ => Err(ConfigChatError::new(format!("配置路径不是对象：{head}"))),
    }
}
