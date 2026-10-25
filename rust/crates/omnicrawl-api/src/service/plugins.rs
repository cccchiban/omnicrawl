//! 由 `service.rs` 按职责拆出，语义未变。

use super::*;

impl AgentService {

        /// 插件是否在运行期生效（对应 Python `agent._plugin_manager.enabled`）。
        pub fn plugins_enabled(&self) -> bool {
            self.options
                .plugins
                .as_ref()
                .map(|plugins| plugins.enabled())
                .unwrap_or(false)
        }

        /// 启动插件运行期并发布会话生命周期 Hook；服务装配完成后调用一次。
        pub(super) fn start_plugins(&self, session_id: &str) {
            let Some(plugins) = self.options.plugins.as_ref() else {
                return;
            };
            for line in plugins.start() {
                eprintln!("[api] 插件：{line}");
            }
            if plugins.active() {
                plugins.notify_app_started();
            }
            self.notify_plugins_session(session_id);
        }

        /// `session.start.after` / `session.resume.*`：会话标识未知（内核尚未回填）时跳过。
        pub(super) fn notify_plugins_session(&self, session_id: &str) {
            if session_id.trim().is_empty() || self.options.plugins.is_none() {
                return;
            }
            let guard = match self.runner.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let Some(runner) = guard.as_ref() else {
                return;
            };
            if let Err(error) = runner.notify_session_lifecycle(session_id) {
                eprintln!("[api] 会话生命周期 Hook 失败：{error}");
            }
        }

        /// 插件总开关热更新（`PUT /settings/features` 的 `plugins` 键）。
        ///
        /// 写盘在路由层完成，这里只切运行期：关闭时回收 Worker 与执行计划，打开时按当前
        /// 注册表重新装配（与 Python 设置面板的事务式重建同义）。
        pub fn set_plugins_enabled(&self, enabled: bool) -> Result<Vec<String>, ApiError> {
            let Some(plugins) = self.options.plugins.as_ref() else {
                return Err(ApiError::new(
                    "PLUGIN_RUNTIME_UNAVAILABLE",
                    "当前进程没有装配插件运行期，插件开关需要重启服务生效。".to_string(),
                    StatusCode::SERVICE_UNAVAILABLE,
                    None,
                ));
            };
            plugins.set_enabled(enabled).map_err(|error| {
                ApiError::new(
                    "PLUGIN_RUNTIME_FAILED",
                    format!("插件运行期切换失败：{error}"),
                    StatusCode::BAD_GATEWAY,
                    None,
                )
            })
        }

        /// 记忆开关的运行期一半：`PUT /settings/features` 写盘后调用。
        ///
        /// 记忆是一整组工具（四个 `memory_*`），开关直接决定它们进不进表；不重建的话
        /// 「设置里关了记忆」要等下次启动才从模型可见面消失。
        pub fn reload_memory_switch(&self, enabled: bool) -> Result<McpReload, ApiError> {
            let mut registry_options = self.current_registry_options();
            registry_options.memory_enabled = enabled;
            self.reload_registry_options(&registry_options, "记忆开关")
        }

        /// 插件运行态摘要：设置页与诊断用的展示来源（无运行期时全为默认值）。
        pub fn plugins_status(&self) -> Value {
            match self.options.plugins.as_ref() {
                Some(plugins) => json!({
                    "enabled": plugins.enabled(),
                    "configured": plugins.configured(),
                    "handlers": plugins.handler_count(),
                    "diagnostics": plugins.diagnostics(),
                    "plugins": plugins.status_rows(),
                }),
                None => json!({
                    "enabled": false,
                    "configured": false,
                    "handlers": 0,
                    "diagnostics": Vec::<String>::new(),
                    "plugins": Vec::<Value>::new(),
                }),
            }
        }

}
