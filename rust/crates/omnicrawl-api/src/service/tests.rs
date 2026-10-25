//! 由 `service.rs` 迁出：服务装配与解析用例。

use super::*;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 隔离出一个「读不到用户配置」的家目录：走的就是 `[context_compaction]` / `[memory]` 的默认值。
    fn isolated_environment(label: &str) -> ConfigEnvironment {
        let home =
            std::env::temp_dir().join(format!("omnicrawl-api-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&home).expect("临时主目录");
        ConfigEnvironment::new(home, std::env::consts::OS.to_string())
    }

    /// 连接器宿主必须拿到**真实工具表**：内核自带的 `--connector` 只自持三个工具，
    /// 其余一律 `tool_unavailable`，远程用户看到的就是「我无法访问你的项目仓库」。
    /// 这条断言钉住修复本身——工具表非空，且文件/命令类工具真的在表里。
    #[test]
    fn connector_options_declare_the_real_tool_table() {
        let environment = isolated_environment("connector-tools");
        let home = environment.home().to_path_buf();
        std::fs::create_dir_all(home.join(".OmniCrawl")).expect("用户配置目录");
        std::fs::write(
            home.join(".OmniCrawl/config.toml"),
            r#"
[llm]
api_key = "test-secret"
base_url = "https://config.example/v1"
model = "test-model"
"#,
        )
        .expect("写 config.toml");

        let env = ConfigEnvironment::new(home.clone(), std::env::consts::OS.to_string());
        let options = connector_options_from_process(&env).expect("连接器选项应能装配");
        let workspace = home.join("workspace");
        std::fs::create_dir_all(&workspace).expect("工作区目录");

        let registry = omnicrawl_host::tools::ToolRegistry::new(
            workspace,
            &options.registry_options,
            options
                .model
                .request_timeout_seconds
                .map(|v| v as i64)
                .unwrap_or(600),
        )
        .expect("工具表应能构建");
        let names: Vec<String> = registry
            .declarations()
            .iter()
            .filter_map(|item| {
                item.get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();

        assert!(!names.is_empty(), "连接器宿主必须有工具表");
        for expected in ["read", "write_file", "Edit_file", "bash", "grep", "list"] {
            assert!(
                names.iter().any(|name| name == expected),
                "连接器宿主缺少 {expected}：{names:?}"
            );
        }
    }

    /// 连接器宿主与 API 的差别只在「不挂隔离工作区」：远程改的是用户当前那个工作区。
    #[test]
    fn connector_options_skip_the_isolated_workspace() {
        let environment = isolated_environment("connector-workspace");
        let home = environment.home().to_path_buf();
        std::fs::create_dir_all(home.join(".OmniCrawl")).expect("用户配置目录");
        std::fs::write(
            home.join(".OmniCrawl/config.toml"),
            r#"
[llm]
api_key = "test-secret"
base_url = "https://config.example/v1"
model = "test-model"
"#,
        )
        .expect("写 config.toml");

        let env = ConfigEnvironment::new(home.clone(), std::env::consts::OS.to_string());
        let options = connector_options_from_process(&env).expect("连接器选项应能装配");
        assert!(
            options
                .isolation
                .lock()
                .map(|slot| slot.is_none())
                .unwrap_or(false),
            "连接器宿主不应创建隔离工作区"
        );
    }

    /// 启动时的会话块必须带记忆根与整段压缩策略：缺了这两项，用户配置就「写了却不生效」
    /// ——内核会回落到 `CompactionConfig::default()`（摘要预算 2000 token、记忆回写关闭）。
    #[test]
    fn runner_options_deliver_compaction_and_memory_root() {
        let environment = isolated_environment("session-block");
        let home = environment.home().to_path_buf();
        let model: KernelModelConfig = serde_json::from_value(json!({
            "model": "test-model",
            "context_window_tokens": 1_000,
        }))
        .expect("模型视图");
        let mut options = ServiceOptions::new(home.join("workspace"), model);
        options.env = environment;
        options.session_root = Some(home.join(".agent_sessions"));

        let runner = runner_options(&options);
        let session = runner.session.expect("给了 session_root 就有会话块");
        let memory_root = session
            .memory_root
            .expect("记忆开关缺省开启，必须下发记忆根");
        assert!(
            memory_root.replace('\\', "/").ends_with(".omnicrawl"),
            "记忆根应当是用户数据根：{memory_root}"
        );
        let compaction = session.compaction.expect("压缩策略必须整段下发");
        assert_eq!(
            compaction.target_summary_tokens,
            Some(6_000),
            "配置默认的摘要预算"
        );
        assert_eq!(
            compaction.trigger_context_tokens,
            Some(800),
            "默认 80% × 当前窗口"
        );
        assert_eq!(compaction.context_window_tokens, Some(1_000));
        assert_eq!(compaction.archive_compacted_events, Some(true));
        assert_eq!(compaction.auto_memory_recall, Some(true));

        let _ = std::fs::remove_dir_all(&home);
    }

    /// 窗口变化时同步阈值（「窗口的百分之几时压缩」是配置关系），且只带这两个字段，
    /// 避免把握手时下发的摘要预算等字段覆盖回空值。
    #[test]
    fn window_changes_recompute_the_trigger() {
        let environment = isolated_environment("window-change");
        let home = environment.home().to_path_buf();
        let mut options = ServiceOptions::new(
            home.join("workspace"),
            serde_json::from_value(json!({"model": "test-model"})).expect("模型视图"),
        );
        options.env = environment;

        let settings = window_compaction_settings(&options, 2_000).expect("默认配置带百分比");
        assert_eq!(settings.trigger_context_tokens, Some(1_600));
        assert_eq!(settings.context_window_tokens, Some(2_000));
        assert_eq!(
            settings.target_summary_tokens, None,
            "热更新只带本次要改的两个字段"
        );

        let _ = std::fs::remove_dir_all(&home);
    }
}

