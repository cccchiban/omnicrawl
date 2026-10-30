//! Hook 策略：每个 Hook 允许的 Handler 模式、补丁白名单与封印前缀。
//!
//! 从 `models.rs` 按职责拆出。

use super::*;

/// 单个 Core Hook 的失败与 deny 策略（对应 Python 的 `HookPolicy`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookPolicy {
    pub on_deny: &'static str,
    pub on_timeout: &'static str,
    pub on_protocol_error: &'static str,
    pub on_handler_error: &'static str,
    pub disable_plugin_on_error: bool,
}

impl Default for HookPolicy {
    fn default() -> Self {
        Self {
            on_deny: "reject-operation",
            on_timeout: "skip-handler",
            on_protocol_error: "skip-handler",
            on_handler_error: "skip-handler",
            disable_plugin_on_error: false,
        }
    }
}

impl HookPolicy {
    /// 校验四个策略字段的取值；越界文案与 Python 的 `__post_init__` 一致。
    pub fn validate(self) -> Result<Self, PluginError> {
        for (field, value) in [
            ("on_deny", self.on_deny),
            ("on_timeout", self.on_timeout),
            ("on_protocol_error", self.on_protocol_error),
            ("on_handler_error", self.on_handler_error),
        ] {
            if !POLICY_VALUES.contains(&value) {
                return Err(PluginError::new(format!(
                    "HookPolicy.{field} 非法：{value}"
                )));
            }
        }
        Ok(self)
    }
}

/// 设计文档 §5.2：显式 deny 与故障分开处理。未列出的 Hook 没有策略（与 Python 的查表一致）。
pub fn hook_policy(hook: &str) -> Option<HookPolicy> {
    let reject_all = HookPolicy {
        on_deny: "reject-operation",
        on_timeout: "reject-operation",
        on_protocol_error: "reject-operation",
        on_handler_error: "reject-operation",
        disable_plugin_on_error: false,
    };
    let ignore_deny = HookPolicy {
        on_deny: "ignore",
        on_timeout: "skip-handler",
        on_protocol_error: "skip-handler",
        on_handler_error: "skip-handler",
        disable_plugin_on_error: false,
    };
    Some(match hook {
        "app.start.before" => HookPolicy {
            on_deny: "reject-operation",
            disable_plugin_on_error: true,
            ..ignore_deny
        },
        "app.start.after" | "app.stop.before" | "app.stop.after" => ignore_deny,
        "workspace.switch.before" | "session.resume.before" => reject_all,
        "workspace.switch.after"
        | "workspace.switch.error"
        | "session.start.after"
        | "session.resume.after"
        | "session.resume.error"
        | "session.close.before"
        | "session.close.after"
        | "turn.end"
        | "turn.error"
        | "turn.cancelled"
        | "context.build.before"
        | "context.build.after"
        | "context.compaction.after_turn"
        | "model.response.after"
        | "model.request.error"
        | "tool.approval.after"
        | "tool.execute.after"
        | "tool.execute.error" => ignore_deny,
        "turn.start"
        | "model.request.before"
        | "tool.call.before"
        | "tool.approval.before"
        | "tool.execute.before" => reject_all,
        _ => return None,
    })
}

/// 每个 Hook 允许的 Handler 模式；未知 Hook 返回空（与 Python 的 `frozenset()` 回落一致）。
pub fn hook_allowed_modes(hook: &str) -> &'static [&'static str] {
    HOOK_ALLOWED_MODES
        .iter()
        .find(|(name, _)| *name == hook)
        .map(|(_, modes)| *modes)
        .unwrap_or(&[])
}

/// Hook 的 transform 白名单；未知 Hook 或未声明白名单时返回空。
pub fn hook_patch_allowlist(hook: &str) -> &'static [&'static str] {
    HOOK_PATCH_ALLOWLIST
        .iter()
        .find(|(name, _)| *name == hook)
        .map(|(_, paths)| *paths)
        .unwrap_or(&[])
}
