//! 运行期装配：`[trigger_api]` 段 → 池配置、会话根、内核程序。

use std::path::{Path, PathBuf};

use omnicrawl_config::core::runtime::{ConfigEnvironment, user_config_dir};
use omnicrawl_config::features::trigger_api::{TriggerApiConfig, load_trigger_api_configuration};
use serde_json::Value;

/// 会话根目录名（放在用户配置目录下）。
pub const SESSION_DIRNAME: &str = "trigger_sessions";

/// 触发接口的运行期配置。
#[derive(Debug, Clone)]
pub struct TriggerSettings {
    pub api: TriggerApiConfig,
}

impl TriggerSettings {
    /// 服务能否真正监听。
    pub fn ready(&self) -> bool {
        self.api.usable()
    }

    /// 不能监听时的可读原因；可用时返回 `None`。
    pub fn unavailable_reason(&self) -> Option<String> {
        if !self.api.enabled {
            return Some(
                "触发接口未启用：请在 config.toml 的 [trigger_api] 段设置 enabled = true。"
                    .to_string(),
            );
        }
        None
    }

    pub fn address(&self) -> String {
        self.api.address()
    }

    /// 触发接口专用会话根：与 TUI / 本地 API / 连接器都不共享。
    pub fn session_root(&self, env: &ConfigEnvironment) -> PathBuf {
        user_config_dir(env).join(SESSION_DIRNAME)
    }
}

/// 按配置装配运行期；缺段与坏值都按默认值回落。
pub fn load_settings(environment: &ConfigEnvironment) -> TriggerSettings {
    TriggerSettings {
        api: load_trigger_api_configuration(environment, None),
    }
}

/// `config.toml` 的 JSON 视图：连接器配置段（`[telegram]` / `[feishu]`）由连接器自己解析。
///
/// 与本地 API 同口径：读失败只意味着「未配置」，不该让投递路径 panic。
pub fn config_data(env: &ConfigEnvironment) -> Value {
    omnicrawl_config::core::runtime::load_config_data(env, None)
        .map(|table| omnicrawl_config::value::toml_to_json_object(&table))
        .unwrap_or_else(|_| Value::Object(serde_json::Map::new()))
}

/// 内核可执行文件：显式路径优先，否则复用本地 API 的解析链。
pub fn kernel_program(env: &ConfigEnvironment, explicit: Option<&Path>) -> PathBuf {
    omnicrawl_api::service::resolve_kernel_program(env, explicit)
}
