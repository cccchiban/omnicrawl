//! 扩展子系统的错误面。
//!
//! Python 侧是一棵 `PluginError` 继承树（manifest / protocol / dispatch / install 四个子类），
//! 文案直接进 UI、注册表与模型上下文，因此 Rust 侧同样按字符串保真搬运：
//! 每个子类一个独立类型，只承载文案，不做分类。

use std::fmt;

macro_rules! plugin_error {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name {
            message: String,
        }

        impl $name {
            pub fn new(message: impl Into<String>) -> Self {
                Self {
                    message: message.into(),
                }
            }

            pub fn message(&self) -> &str {
                &self.message
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.message)
            }
        }

        impl std::error::Error for $name {}
    };
}

plugin_error!(PluginError, "插件子系统通用错误。");
plugin_error!(PluginManifestError, "manifest / package.json 校验失败。");
plugin_error!(PluginProtocolError, "Host ↔ Worker 协议错误。");
plugin_error!(PluginDispatchError, "Hook 分发失败且策略要求拒绝当前操作。");
plugin_error!(PluginInstallError, "安装 / 更新 / 卸载失败。");
plugin_error!(PluginRegistryError, "注册表读写或合并失败。");

/// 读注册表、解析配置这类场景要向上层汇总多种插件错误，统一收敛成通用错误。
impl From<PluginManifestError> for PluginError {
    fn from(value: PluginManifestError) -> Self {
        Self::new(value.message)
    }
}

impl From<PluginProtocolError> for PluginError {
    fn from(value: PluginProtocolError) -> Self {
        Self::new(value.message)
    }
}

impl From<PluginDispatchError> for PluginError {
    fn from(value: PluginDispatchError) -> Self {
        Self::new(value.message)
    }
}

impl From<PluginInstallError> for PluginError {
    fn from(value: PluginInstallError) -> Self {
        Self::new(value.message)
    }
}

impl From<PluginRegistryError> for PluginError {
    fn from(value: PluginRegistryError) -> Self {
        Self::new(value.message)
    }
}
