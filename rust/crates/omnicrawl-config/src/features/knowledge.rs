//! 知识库开关配置（`config.toml` 的 `[knowledge]` 段）。
//!
//! 只有一项 `enabled`：知识库工具组（`kb_search` 到 `kb_delete`）整组进表或整组不进表。
//! 读走 `core::settings::load_feature_enabled`，写走 `save_feature_enabled`——与记忆、
//! 子代理等既有功能开关同一条链路，这里只补默认值与归一。
//!
//! 默认开启：知识库是 Agent 的既有能力，升级后不该悄悄消失。

/// 缺省是否开启：`config.toml` 里没有 `[knowledge]` 段时按开启处理。
pub const DEFAULT_KNOWLEDGE_ENABLED: bool = true;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_enabled() {
        // 关掉知识库是用户的显式选择，不是缺省行为。
        assert!(DEFAULT_KNOWLEDGE_ENABLED);
    }
}
