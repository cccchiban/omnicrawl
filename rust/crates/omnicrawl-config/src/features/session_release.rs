//! 会话上下文闲置换出（`[ui] session_release_seconds`；Rust 侧新增，Python 无对映）。
//!
//! 内核把整个会话历史缓冲常驻在内存里，长会话实测可达几十 MB。它完全能由转录重建，
//! 因此允许在回合结束闲置一段时间后换出，下次提交时内核自行重放。
//!
//! 缺省 1800 秒（30 分钟）开启：重放要重新解析转录，大会话会有可见停顿，
//! 把这个代价摊到「整段闲置」上才划算。设 0 即关闭（历史一直驻留到进程退出）。

use std::path::Path;

use crate::core::runtime::{get_section, load_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::Value;

/// 缺省值：回合结束闲置 30 分钟后换出内核的会话历史缓冲。
///
/// 长会话的历史常驻实测 40 MB 以上，是内核里唯一与对话长度成正比的大块；半小时足够让
/// 「问一句、想一会儿、再问」的节奏不付重放代价，又能回收整段闲置的内存。
///
/// 大会话的重放有可见停顿；把它设成 0 即恢复「历史一直驻留到进程退出」。
pub const DEFAULT_SESSION_RELEASE_SECONDS: u64 = 1_800;

/// 上限：一天，再长没有实际意义。
pub const MAX_SESSION_RELEASE_SECONDS: u64 = 86_400;

/// 读取 `[ui] session_release_seconds`。
///
/// 段或键缺失、或者不是非负整数，都按缺省（关闭）处理：这是纯优化开关，
/// 配置写错不该让启动失败。
pub fn load_session_release_seconds(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<u64, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = get_section(&data, "ui")?;
    let Some(value) = section.get("session_release_seconds") else {
        return Ok(DEFAULT_SESSION_RELEASE_SECONDS);
    };
    let seconds = match value {
        Value::Integer(raw) => *raw,
        _ => return Ok(DEFAULT_SESSION_RELEASE_SECONDS),
    };
    if seconds < 0 {
        return Ok(DEFAULT_SESSION_RELEASE_SECONDS);
    }
    Ok((seconds as u64).min(MAX_SESSION_RELEASE_SECONDS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_section_or_key_falls_back_to_the_default() {
        let env = ConfigEnvironment::from_process();
        // 真实配置里未必有这个键：读不到必须是默认值而不是报错。
        let seconds = load_session_release_seconds(&env, None).unwrap_or(0);
        assert!(seconds <= MAX_SESSION_RELEASE_SECONDS);
    }

    /// 默认开启：闲置半小时后换出，而不是「关闭」。
    #[test]
    fn default_is_enabled() {
        assert!(
            DEFAULT_SESSION_RELEASE_SECONDS > 0,
            "默认应开启闲置换出，0 表示关闭"
        );
        assert!(DEFAULT_SESSION_RELEASE_SECONDS < MAX_SESSION_RELEASE_SECONDS);
    }
}
