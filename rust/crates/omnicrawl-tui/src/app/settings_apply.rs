//! `App` 的设置应用：把设置面板的一次变更写进配置并让宿主侧生效。
//!
//! 每个 `apply_*` 对应设置面板的一页/一行。共同约定：写盘失败即「设置未完成」，
//! 值不变；内核拒绝即时更新时配置已落盘、宿主侧已生效，只补一句「下次会话生效」，
//! 不回滚（两件事分开）。
//!
//! 拆成独立 `impl App` 块：这里只依赖 `App` 自身状态与配置层，不参与界面编排。

use super::*;

impl App {
    /// 切换模型渠道：写 `llm.active_model`（legacy 配置写 `llm.model`），并同步运行视图。
    ///
    /// 渠道信息（Provider/协议/基地址/凭据变量名、模型 id）整条换掉，随后的
    /// `session.settings` 会把它推给内核，本会话即刻生效。
    pub(super) fn apply_model_channel(&mut self, key: &str) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let configuration = load_channel_configuration(&environment, None, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let Some(channel) = configuration
            .channels
            .iter()
            .find(|channel| channel.key == key)
        else {
            return Err(format!("设置未完成：配置里没有渠道 {key}。"));
        };
        let reference = ActiveModelRef {
            source: "custom".to_string(),
            key: channel.key.clone(),
            profile: channel.profile_id.clone(),
            model_id: channel.model_id.clone(),
            protocol: channel.protocol.clone(),
        };
        let path = save_active_model_ref(&environment, &reference, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 写盘后重新解析一次整条模型视图：上下文窗口、生成选项、Provider、基地址与凭据
        // 变量名都跟着新渠道走（读不出来时退回渠道记录里的那几个字段）。
        match load_llm_config(&environment) {
            Ok(refreshed) => self.llm = refreshed,
            Err(error) => {
                self.state.notice(format!(
                    "切换渠道后重新解析模型配置失败，沿用渠道记录：{error}"
                ));
                self.llm.model = channel.model_id.clone();
                self.llm.provider = channel.provider.clone();
                self.llm.protocol = channel.protocol.clone();
                self.llm.base_url = channel.base_url.clone();
                self.llm.profile_id = channel.profile_id.clone();
                self.llm.catalog_key = channel.key.clone();
                self.llm.model_source = "custom".to_string();
            }
        }
        self.state.model = self.llm.model.clone();
        Ok(format!(
            "模型已切到 {}（{}），已保存到 {}。",
            channel.name,
            self.llm.model,
            path.display()
        ))
    }

    /// 保存整份渠道配置：写 config.toml 与 models.toml，并让当前渠道跟着默认渠道走。
    ///
    /// 面板里填了新密钥（非空）就用新值，留空则按 key 从磁盘上的同一条渠道继承——
    /// 既能录入内联 `api_key`（Python 渠道编辑器同能力），也不会因为没碰这一行把它抹掉
    /// （`save_channel_configuration` 对空 `api_key` 会移除该字段）。
    pub(super) fn apply_channels(&mut self, rows: &[ChannelRow], default_key: &str) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let existing = load_channel_configuration(&environment, None, None)
            .map(|configuration| configuration.channels)
            .unwrap_or_default();
        let channels: Vec<ChannelConfig> = rows
            .iter()
            .map(|row| {
                let mut channel = channel_config_from_row(row);
                if channel.api_key.is_empty() {
                    if let Some(previous) = existing.iter().find(|item| item.key == row.key) {
                        channel.api_key = previous.api_key.clone();
                    }
                }
                channel
            })
            .collect();
        let configuration = ChannelConfiguration {
            channels,
            default_key: default_key.to_string(),
        };
        let (config_path, models_path) =
            save_channel_configuration(&environment, &configuration, None, None)
                .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 配置侧会规范默认渠道与凭据变量名：重新解析一次模型视图并读回磁盘上的渠道。
        match load_llm_config(&environment) {
            Ok(refreshed) => self.llm = refreshed,
            Err(error) => self
                .state
                .notice(format!("保存渠道后重新解析模型配置失败：{error}")),
        }
        self.state.model = self.llm.model.clone();
        let (reloaded, reloaded_default) = self.channel_views(&environment);
        if let Some(settings) = self.settings.as_mut() {
            settings.sync_channels(reloaded, &reloaded_default);
        }
        Ok(format!(
            "渠道配置已保存到 {} 与 {}；当前渠道 {}。",
            config_path.display(),
            models_path.display(),
            self.llm.model
        ))
    }

    /// 推理强度：写 config.toml 的 `llm.reasoning_effort`，并把档位推给内核的生成选项。
    pub(super) fn apply_reasoning_effort(&mut self, effort: &str) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let path = save_reasoning_effort(&environment, effort, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        Ok(format!(
            "推理强度已设为 {}，已保存到 {}。",
            reasoning_label(effort),
            path.display()
        ))
    }

    /// 思考显示：只影响本机界面（消息流里要不要显示思考段），写 `ui.show_thinking`。
    pub(super) fn apply_show_thinking(&mut self, enabled: bool) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let path = save_show_thinking(&environment, enabled, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        self.state.set_show_thinking(enabled);
        Ok(format!(
            "思考显示已{}，已保存到 {}。",
            if enabled { "开启" } else { "关闭" },
            path.display()
        ))
    }

    /// 功能开关：`memory` 立即重建工具表（记忆整组进/出表）。
    pub(super) fn apply_feature(&mut self, key: &str, enabled: bool) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let path = save_feature_enabled(&environment, key, enabled, None, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let label = feature_label(key);
        match key {
            "memory" => {
                self.registry_options.memory_enabled = enabled;
                self.rebuild_registry()?;
                if let Some(settings) = self.settings.as_mut() {
                    settings.refresh_tools(tool_switch_rows(&self.registry));
                }
                Ok(format!(
                    "{label}已{}，已保存到 {}。",
                    if enabled { "开启" } else { "关闭" },
                    path.display()
                ))
            }
            // 面板上只有「记忆」一个功能开关，该支已即时重建运行期；
            // 走到这里说明面板加了新的开关行却没有配套的重建逻辑，如实报告而不是假装已生效。
            other => Err(format!(
                "设置未完成：功能开关 {other} 已写盘，但本进程没有配套的重建逻辑，需要补齐后再试。"
            )),
        }
    }

    /// 上下文长度：写窗口 + 按当前百分比重算阈值，两者都写盘。
    ///
    /// 第二部分失败时把磁盘上的窗口值改回旧值——否则会留下「新窗口 + 旧百分比」
    /// 的自相矛盾配置（与 Python 的 `_apply_setting_value("context")` 一致）。
    pub(super) fn apply_context_window(&mut self, tokens: i64) -> Result<String, String> {
        let percent = self
            .settings
            .as_ref()
            .map(|settings| settings.compaction_percent())
            .unwrap_or(80);
        let environment = ConfigEnvironment::from_process();
        let llm = load_llm_config(&environment)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let previous = effective_context_window(self.options.context_window_tokens, &llm);
        let path = save_context_window_tokens(
            &environment,
            tokens,
            &llm.model_source,
            &llm.catalog_key,
            None,
            None,
        )
        .map_err(|error| format!("设置未完成：{}", error.message()))?;
        if let Err(error) =
            save_context_compaction_trigger_percent(&environment, percent, tokens, None)
        {
            let _ = save_context_window_tokens(
                &environment,
                previous,
                &llm.model_source,
                &llm.catalog_key,
                None,
                None,
            );
            return Err(format!("设置未完成：{}", error.message()));
        }
        self.state.telemetry.context_window = Some(tokens.max(0) as u64);
        let threshold = context_compaction_trigger_tokens(tokens, percent);
        Ok(format!(
            "上下文长度已设为 {}K，已保存到 {}；压缩阈值已同步为 {percent}%（{threshold} Token）。",
            tokens / 1000,
            path.display()
        ))
    }

    /// 压缩阈值：按当前上下文长度换算 Token 后写盘。
    pub(super) fn apply_compaction_percent(&mut self, percent: i64) -> Result<String, String> {
        let window = self
            .settings
            .as_ref()
            .map(|settings| settings.context_window_tokens())
            .unwrap_or(128_000);
        let environment = ConfigEnvironment::from_process();
        let path = save_context_compaction_trigger_percent(&environment, percent, window, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let tokens = context_compaction_trigger_tokens(window, percent);
        Ok(format!(
            "上下文压缩阈值已设为 {percent}%（{tokens} Token），已保存到 {}。",
            path.display()
        ))
    }

    /// 工具开关：写盘 → 重建宿主工具表 → 刷新设置面板里的注册标记。
    ///
    /// 开关即「模型可见性」：禁用的工具不进声明，模型也就调不到它。
    pub(super) fn apply_tool_switch(&mut self, name: &str, enabled: bool) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let path = save_tool_switch(&environment, name, enabled, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let disabled = load_disabled_tools(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        self.registry_options.disabled_tools = disabled;
        self.rebuild_registry()?;
        if let Some(settings) = self.settings.as_mut() {
            settings.refresh_tools(tool_switch_rows(&self.registry));
        }
        Ok(format!(
            "{}已{}，已保存到 {}。",
            tool_label(name),
            if enabled { "启用" } else { "关闭" },
            path.display()
        ))
    }

    /// 联网搜索页：写 `config.toml [web_search]`（字段顺序与 `WEB_SEARCH_FIELDS` 一致），
    /// 再换掉宿主侧搜索运行期。
    ///
    /// 五把凭据只在非空时写回：界面留空或仍显示掩码都表示「不改动磁盘原值」，
    /// 否则用户只想改默认引擎就会顺手把已存密钥抹掉。
    pub(super) fn apply_web_search_form(&mut self, values: &[FieldValue]) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let mut configuration = load_web_search_configuration(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 写盘前留一份磁盘原值：重建工具表失败时要把它写回去（这份还没被覆盖）。
        let previous = configuration.clone();
        configuration.engine = field_text_or(values, 0, "duckduckgo");
        if let Some(key) = keep_or_replace(&field_text(values, 1)) {
            configuration.tavily_api_key = key;
        }
        if let Some(key) = keep_or_replace(&field_text(values, 2)) {
            configuration.exa_api_key = key;
        }
        if let Some(key) = keep_or_replace(&field_text(values, 3)) {
            configuration.firecrawl_api_key = key;
        }
        if let Some(key) = keep_or_replace(&field_text(values, 4)) {
            configuration.anysearch_api_key = key;
        }
        if let Some(key) = keep_or_replace(&field_text(values, 5)) {
            configuration.tinyfish_api_key = key;
        }
        if let Some(key) = keep_or_replace(&field_text(values, 6)) {
            configuration.github_token = key;
        }
        let path = save_web_search_configuration(&environment, &configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 搜索运行期就在宿主侧：写盘后立即换掉。但执行体读的是工具表里那一份克隆
        // （`ToolRegistry::new` 会 clone `registry_options.web_search`），只改选项不重建
        // 的话正在跑的表还握着启动时的引擎与凭据——现象是「设置里选了 tavily，搜索仍报
        // 来源：duckduckgo」，直到重启。重建失败时磁盘与运行期一起回滚（同 `apply_advisor_form`）。
        let previous_options = self.registry_options.web_search.clone();
        self.registry_options.web_search =
            omnicrawl_host::tools::web_search_options_from_config(&environment);
        if let Err(error) = self.rebuild_registry() {
            if let Err(rollback) = save_web_search_configuration(&environment, &previous, None) {
                self.state
                    .notice(format!("联网搜索设置回滚写盘失败：{rollback}"));
            }
            self.registry_options.web_search = previous_options;
            return Err(error);
        }
        Ok(format!(
            "联网搜索已设为 {}，已保存到 {}（本会话即刻生效）。",
            configuration.engine,
            path.display()
        ))
    }

    /// 写回工具调用审查模式（`[approval] mode`），并同步本会话的审批模式。
    pub(super) fn apply_tool_approval(&mut self, mode: &str) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let path =
            omnicrawl_config::features::approval::save_approval_mode(&environment, mode, None)
                .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let parsed = ApprovalMode::parse(mode).map_err(|error| format!("设置未完成：{error}"))?;
        // 审批模式是宿主侧执行层的定调依据，本会话即刻生效（不下发内核）。
        self.options.approval = parsed;
        Ok(format!(
            "工具调用审查模式已设为 {}，已保存到 {}。",
            parsed.label(),
            path.display()
        ))
    }

    /// 读 MCP 设置页的初值：全局开关/策略 + Server 列表；读不出来用保守默认值。
    pub(super) fn mcp_settings_values(&self, environment: &ConfigEnvironment) -> McpSettingsValues {
        match load_mcp_config(environment, None) {
            Ok(config) => mcp_values_from_config(&config),
            Err(error) => {
                diagnostics::warn(format!(
                    "[tui] MCP 配置读取失败，MCP 设置页用默认值：{error}"
                ));
                McpSettingsValues::default()
            }
        }
    }

    /// 应用一次 MCP 设置：把界面变更写回 `[mcp]` 段，再重建 MCP 连接与工具表。
    ///
    /// MCP 能力注册进工具表，所以保存后必须重建注册表；旧管理器先 `close()`，
    /// 不给系统留下孤儿子进程（与退出路径同一处理）。
    ///
    /// 写盘前后各有一次把关：写前用读取器的名称规则预检（字符集、重名），写后把整份配置
    /// 回读一遍——传输、地址、超时边界只有读取器全覆盖，它才是合法性的唯一真相。
    /// 回读失败即按字节还原写前快照，磁盘不会停在读取器拒绝的内容上。
    pub(super) fn apply_mcp(&mut self, change: &McpChange) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let config = load_mcp_config(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let mut data = mcp_config_data(&config);
        match change {
            McpChange::Globals {
                enabled,
                allow_external_network_tools,
                require_confirmation_for_write,
                require_confirmation_for_command,
                audit_log_enabled,
                timeout_seconds,
            } => {
                data.enabled = *enabled;
                data.default_timeout_seconds = *timeout_seconds;
                data.policy = McpPolicyData {
                    require_confirmation_for_write: *require_confirmation_for_write,
                    require_confirmation_for_command: *require_confirmation_for_command,
                    allow_external_network_tools: *allow_external_network_tools,
                    audit_log_enabled: *audit_log_enabled,
                };
            }
            McpChange::SaveServer(draft) => {
                let name = draft.name.trim().to_string();
                if name.is_empty() {
                    return Err("设置未完成：Server 名称不能为空。".to_string());
                }
                // 名称字符集由读取器定义，预检一次让错误当面就说清楚，而不是等到下次读取。
                if !valid_server_name(&name) {
                    return Err(format!(
                        "设置未完成：Server 名称只能包含小写字母、数字、下划线和连字符：{name}。"
                    ));
                }
                let original = draft.original_name.as_deref();
                let duplicate = data.servers.iter().any(|(key, _)| key == &name)
                    && original != Some(name.as_str());
                if duplicate {
                    return Err("设置未完成：Server 名称不能重复。".to_string());
                }
                let previous = original
                    .and_then(|key| data.servers.iter().find(|(name, _)| name == key))
                    .map(|(_, server)| server.clone());
                let server = mcp_server_data(draft.as_ref(), previous.as_ref())?;
                if let Some(original) = original {
                    if original != name {
                        data.servers.retain(|(key, _)| key != original);
                    }
                }
                match data.servers.iter_mut().find(|(key, _)| key == &name) {
                    Some(entry) => entry.1 = server,
                    None => data.servers.push((name, server)),
                }
            }
            McpChange::SetServerEnabled { name, enabled } => {
                if let Some(entry) = data.servers.iter_mut().find(|(key, _)| key == name) {
                    entry.1.enabled = *enabled;
                }
            }
            McpChange::DeleteServer { name } => {
                data.servers.retain(|(key, _)| key != name);
            }
        }
        // 写前把配置文件原样快照一份：回读校验失败时按字节还原，被拒的保存不留下任何痕迹
        // （与 `PUT /settings/mcp` 同一处理，也不能靠重写 `[mcp]` 段「回滚」——那会把用户
        // 文件里的其他写法一起规范化掉）。
        let config_file = resolve_config_path(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let snapshot = std::fs::read(&config_file).ok();
        let path = save_mcp_config(&environment, &data, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 回读校验：传输、地址与超时边界只有读取器全覆盖，它才是「配置合法」的唯一真相。
        if let Err(error) = load_mcp_config(&environment, None) {
            restore_config_file(&config_file, snapshot.as_deref());
            return Err(format!("设置未完成：{}", error.message()));
        }
        // 旧连接先关，再按新配置重新发现并重建工具表。
        if let Some(manager) = self.registry.mcp() {
            manager.close();
        }
        self.registry_options.mcp = mcp_manager(&self.workspace);
        self.rebuild_registry()?;
        Ok(format!("MCP 设置已保存：{}。", path.display()))
    }

    /// 保存一页表单：按表单页把字段值拼回配置结构，写盘后再让宿主侧生效。
    pub(super) fn apply_form(&mut self, kind: FormKind, values: &[FieldValue]) -> Result<String, String> {
        match kind {
            FormKind::Advisor => self.apply_advisor_form(values),
            FormKind::ToolOutputCompression => self.apply_compression_form(values),
            FormKind::Desensitization => self.apply_desensitization_form(values),
            FormKind::RunGuard => self.apply_run_guard_form(values),
            FormKind::AgentWorkspace => self.apply_agent_workspace_form(values),
            FormKind::ImageGen => self.apply_image_gen_form(values),
            FormKind::WebSearch => self.apply_web_search_form(values),
            FormKind::ComputerUse => self.apply_computer_use_form(values),
        }
    }

    /// Computer Use 设置：写 `config.toml [computer_use]`。
    ///
    /// 工具是否进表取决于这个总开关，而执行体读的是工具表里那一份克隆，
    /// 因此保存后必须重建工具表，否则要等重启才生效。
    pub(super) fn apply_computer_use_form(&mut self, values: &[FieldValue]) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let mut configuration = load_computer_use_config(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 写盘前留一份磁盘原值：重建工具表失败时要把它写回去。
        let previous = configuration.clone();
        configuration.enabled = values.first().map(FieldValue::flag).unwrap_or(false);
        configuration.timeout_seconds = positive_int(values, 1, "单次时间预算（秒）")? as f64;
        configuration.screenshot_max_dimension = positive_int(values, 2, "截图长边上限")?;
        configuration.thinking_effort = {
            let effort = field_text(values, 3).trim().to_string();
            if effort.is_empty() {
                DEFAULT_THINKING_EFFORT.to_string()
            } else {
                effort
            }
        };
        configuration.model = field_text(values, 4).trim().to_string();
        let path = save_computer_use_config(&environment, &configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let previous_options = self.registry_options.computer_use;
        self.registry_options.computer_use = configuration.active();
        if let Err(error) = self.rebuild_registry() {
            if let Err(rollback) = save_computer_use_config(&environment, &previous, None) {
                self.state
                    .notice(format!("Computer Use 设置回滚写盘失败：{rollback}"));
            }
            self.registry_options.computer_use = previous_options;
            return Err(error);
        }
        let state = if configuration.enabled {
            "已启用"
        } else {
            "已关闭"
        };
        Ok(format!(
            "Computer Use {state}，已保存到 {}（本会话即刻生效）。",
            path.display()
        ))
    }

    /// 顾问设置：写 `config.toml [advisor]`，再用新配置重建顾问工具。
    ///
    /// 顾问工具是否注册取决于「启用且选了模型」以及当前模型是否命中黑名单，
    /// 所以保存后必须重建工具表，让 advisor 工具即时出现/消失
    /// （与 Python 的 `set_advisor_configuration` 同义）。
    pub(super) fn apply_advisor_form(&mut self, values: &[FieldValue]) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let previous = load_advisor_config(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let enabled = values.first().map(|value| value.flag()).unwrap_or(false);
        let effort = field_text(values, 1);
        // 字段表：启用 / effort / 顾问渠道 / 顾问模型。
        let channels: Vec<ChannelRow> = self
            .settings
            .as_ref()
            .map(|settings| settings.channel_rows().to_vec())
            .unwrap_or_default();
        let model_key = form_model_token(values, 2, 3, &channels);
        if enabled && model_key.is_empty() {
            return Err("设置未完成：启用顾问前请先选择顾问渠道与模型。".to_string());
        }
        let configuration = AdvisorConfig {
            enabled,
            model_key,
            effort,
            // 黑名单不在界面里编辑，按磁盘上的原值保留。
            disabled_for_models: previous.disabled_for_models.clone(),
        };
        let path = save_advisor_config(&environment, &configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 与 Python 页面同义：配置已写盘，但宿主侧生效失败时把磁盘与运行期一起回滚，
        // 不留「磁盘是新值、运行期还是旧值」的错配。
        let previous_options = self.registry_options.advisor.clone();
        self.sync_advisor_options(&environment, &configuration);
        if let Err(error) = self.rebuild_registry() {
            if let Err(rollback) = save_advisor_config(&environment, &previous, None) {
                self.state
                    .notice(format!("顾问设置回滚写盘失败：{rollback}"));
            }
            self.registry_options.advisor = previous_options;
            return Err(error);
        }
        Ok(format!(
            "顾问设置已保存到 {}（{}）。",
            path.display(),
            if configuration.active() {
                "本会话即刻生效"
            } else {
                "顾问已停用"
            }
        ))
    }

    /// 把顾问配置映射到顾问工具的运行期选项：模型项按渠道解析出接口与凭据。
    pub(super) fn sync_advisor_options(&mut self, environment: &ConfigEnvironment, config: &AdvisorConfig) {
        let mut advisor = self.registry_options.advisor.clone();
        advisor.enabled = config.active();
        advisor.effort = config.display_effort();
        advisor.disabled_for_models = config.disabled_for_models.clone();
        match channel_for_key(environment, &config.model_key) {
            Some(channel) => {
                advisor.model = channel.model_id.clone();
                advisor.base_url = if channel.base_url.trim().is_empty() {
                    self.options.base_url.clone()
                } else {
                    channel.base_url.clone()
                };
                // 顾问凭据只认渠道里的明文密钥（与运行期 `resolve_api_key` 同口径）。
                advisor.api_key = channel.api_key.clone();
            }
            // 没配渠道（单模型 legacy 配置）时，顾问模型就是配置里的引用本身。
            None => {
                advisor.model = config.model_key.clone();
                advisor.base_url = self.options.base_url.clone();
                advisor.api_key = String::new();
            }
        }
        self.registry_options.advisor = advisor;
    }

    /// `/advisor` 的运行期落地：把命令层已写盘的顾问配置同步到工具选项并重建工具表。
    ///
    /// 与设置页（[`App::apply_advisor_form`]）不同的是，写盘由命令处理器负责
    /// （它先调 `save_advisor_config` 再调这里），所以宿主这一侧只做两件事：
    /// 把配置映射成运行期选项，再重建工具表让 advisor 工具即时出现/消失。
    ///
    /// 重建失败时把运行期选项回滚成原值再报错：调用方会看到「已保存但未生效」，
    /// 而不是「磁盘是新值、运行期还是旧值」这种看不见的错配。
    ///
    /// 公开是为了让集成测试能直接驱动这条落地路径（`/advisor <model>` 的写盘会碰
    /// 真实 `config.toml`，不经写盘就验证「选项同步 + 重建工具表」只能走这里）。
    pub fn command_apply_advisor_config(&mut self, config: &AdvisorConfig) -> Result<(), String> {
        let environment = ConfigEnvironment::from_process();
        let previous_options = self.registry_options.advisor.clone();
        self.sync_advisor_options(&environment, config);
        if let Err(error) = self.rebuild_registry() {
            self.registry_options.advisor = previous_options;
            return Err(error);
        }
        Ok(())
    }

    /// 工具输出压缩：写 `config.toml [tool_output_compression]`。
    ///
    /// 压缩发生在内核侧的工具批次收口时，宿主这边没有运行期句柄，所以这里只写盘，
    /// 并在状态文本里说明生效时机。
    pub(super) fn apply_compression_form(&mut self, values: &[FieldValue]) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let enabled = values.first().map(|value| value.flag()).unwrap_or(false);
        // 字段表：启用 / 思考开关 / 思考深度 / 四个预算 / 压缩渠道 / 压缩模型。
        let channels: Vec<ChannelRow> = self
            .settings
            .as_ref()
            .map(|settings| settings.channel_rows().to_vec())
            .unwrap_or_default();
        let model_key = form_model_token(values, 7, 8, &channels);
        if enabled && model_key.is_empty() {
            return Err("设置未完成：启用压缩前请先选择压缩渠道与模型。".to_string());
        }
        let configuration = ToolOutputCompressionConfig {
            enabled,
            model_key,
            thinking_enabled: values.get(1).map(|value| value.flag()).unwrap_or(false),
            reasoning_effort: field_text(values, 2),
            min_chars: positive_int(values, 3, "最小压缩字符数")?,
            max_input_chars: positive_int(values, 4, "单次压缩输入上限")?,
            max_output_chars: positive_int(values, 5, "压缩结果上限")?,
            timeout_seconds: positive_int(values, 6, "单条压缩超时（秒）")?,
        };
        configuration
            .clone()
            .validate()
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let path = save_tool_output_compression_config(&environment, &configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        Ok(format!(
            "工具输出压缩设置已保存到 {}（压缩在内核侧随每轮工具批次重读，下一轮起生效）。",
            path.display()
        ))
    }

    /// 消息脱敏：写 `config.toml` 的 `[desensitization]` 段。
    ///
    /// 该段在内核构建模型运行时读取，而构建键里带了这段配置的指纹（见内核侧
    /// `RuntimeBuildKey`），因此改完只要下一回合取运行时就按新配置重建，本会话即刻生效。
    pub(super) fn apply_desensitization_form(&mut self, values: &[FieldValue]) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let mut configuration = load_desensitization_config(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 下标与 `form.rs` 的 `DESENSITIZATION_FIELDS` 一一对应。
        configuration.enabled = field_flag(values, 0);
        configuration.fail_closed = field_flag(values, 1);
        configuration.strict_restore = field_flag(values, 2);
        configuration.entropy_enabled = field_flag(values, 3);
        configuration.entropy_pure_letters = field_flag(values, 4);
        configuration.entropy_pure_digits = field_flag(values, 5);
        configuration.entropy_min_length = positive_int(values, 6, "熵兜底长度下限")?;
        configuration.entropy_min_bits = field_float(values, 7, "熵兜底阈值")?;
        configuration.extra_sensitive_keys = field_list(values, 8, ',');
        configuration.exempt_keys = field_list(values, 9, ',');
        configuration.detect_pem_private_key = field_flag(values, 10);
        configuration.detect_db_connection_string = field_flag(values, 11);
        configuration.detect_email = field_flag(values, 12);
        configuration.detect_bank_card = field_flag(values, 13);
        configuration.detect_internal_ip = field_flag(values, 14);
        configuration.detect_external_ip = field_flag(values, 15);
        configuration.detect_url = field_flag(values, 16);
        configuration.detect_mac_address = field_flag(values, 17);
        configuration.detect_license_plate = field_flag(values, 18);
        configuration.gitleaks_enabled = field_flag(values, 19);
        configuration.gitleaks_config_path = field_text(values, 20).trim().to_string();
        let path = save_desensitization_config(&environment, &configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        Ok(format!(
            "消息脱敏设置已保存到 {}（本会话下一回合起生效）。",
            path.display()
        ))
    }

    /// 持续运转：写 `config.toml` 的 `[run_guard]` 段。
    ///
    /// 与 Python 一致：保存后从下一个 Agent 回合起生效，正在跑的回合不变。
    pub(super) fn apply_run_guard_form(&mut self, values: &[FieldValue]) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let mut configuration = load_run_guard_config(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 下标与 `form.rs` 的 `RUN_GUARD_FIELDS` 一一对应。
        configuration.enabled = field_flag(values, 0);
        configuration.guard.enabled = field_flag(values, 1);
        configuration.guard.window_chars = positive_int(values, 2, "窗口字符数")?;
        configuration.guard.substr_len = positive_int(values, 3, "重复子串长度")?;
        configuration.guard.repeat_ratio = field_float(values, 4, "重复率阈值")?;
        configuration.guard.check_every = positive_int(values, 5, "检查间隔")?;
        configuration.guard.max_blocks = positive_int(values, 6, "推理块上限")?;
        configuration.guard.max_chars = positive_int(values, 7, "推理字符上限")?;
        configuration.guard.max_guard_retries = non_negative_int(values, 8, "护栏重试次数")?;
        configuration.guard.auto_retry_errors = field_list(values, 9, ',');
        configuration.continuation.enabled = field_flag(values, 10);
        configuration.continuation.max_auto_followups =
            non_negative_int(values, 11, "续跑次数上限")?;
        let path = save_run_guard_config(&environment, &configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        Ok(format!(
            "持续运转设置已保存到 {}（本会话下一个回合起生效）。",
            path.display()
        ))
    }

    /// 隔离工作区：写 `config.toml` 的 `[agent_workspace]` 段。
    ///
    /// 隔离区在启动时创建一次，运行中的进程不换区：这里的改动对**下一个**隔离会话生效。
    /// 收尾策略（`apply_on_exit` / `cleanup_on_exit`）在退出时才读，所以它当场就生效。
    /// 「基线分支」不在界面里，保留磁盘原值。
    pub(super) fn apply_agent_workspace_form(&mut self, values: &[FieldValue]) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let mut configuration = load_agent_workspace_config(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 下标与 `form.rs` 的 `AGENT_WORKSPACE_FIELDS` 一一对应。
        configuration.enabled = field_flag(values, 0);
        configuration.mode = field_text(values, 1);
        let base_ref = field_text(values, 2).trim().to_string();
        configuration.base_ref = if base_ref.is_empty() {
            "HEAD".to_string()
        } else {
            base_ref
        };
        configuration.detached = field_flag(values, 3);
        configuration.sync_uncommitted = field_flag(values, 4);
        configuration.apply_on_exit = field_flag(values, 5);
        configuration.cleanup_on_exit = field_text(values, 6);
        configuration.copy_dirs = field_list(values, 7, ',');
        configuration.env_scripts = field_list(values, 8, ';');
        let path = save_agent_workspace_config(&environment, &configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        Ok(format!(
            "隔离工作区设置已保存到 {}（收尾策略即刻生效；建区参数对下一个隔离会话生效）。",
            path.display()
        ))
    }

    /// 图像生成：写 `config.toml [image_gen]`，再按新配置重建图像生成工具。
    ///
    /// 凭据字段只显示掩码，留空＝保留磁盘原值；重建工具表失败时磁盘与运行期一起回滚。
    pub(super) fn apply_image_gen_form(&mut self, values: &[FieldValue]) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let mut configuration = load_image_gen_configuration(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 写盘前再读一份原始值：生效失败时要把它写回去（这一份还没被覆盖）。
        let previous = load_image_gen_configuration(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 下标与 `form.rs` 的 `IMAGE_GEN_FIELDS` 一一对应。
        configuration.enabled = field_flag(values, 0);
        configuration.provider = field_text_or(values, 1, "openai");
        configuration.base_url = field_text_or(values, 2, "https://api.openai.com/v1");
        let api_key = field_text(values, 3);
        if !api_key.trim().is_empty() {
            configuration.api_key = api_key.trim().to_string();
        }
        configuration.model = field_text_or(values, 4, "gpt-image-2");
        configuration.size = field_text(values, 5);
        configuration.quality = field_text(values, 6);
        configuration.output_format = field_text(values, 7);
        configuration.aspect_ratio = field_text(values, 8);
        configuration.image_size = field_text(values, 9);
        configuration.n = non_negative_int(values, 10, "默认张数")?;
        configuration.timeout_seconds = non_negative_int(values, 11, "请求超时")?;
        // 归一化会把「切到 Gemini 但仍是 OpenAI 默认端点/模型」折算成 Gemini 的默认值，
        // 折算结果要同时进磁盘、运行期与界面草稿，三者不能各执一份。
        let saved = configuration
            .clone()
            .normalize()
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let path = save_image_gen_configuration(&environment, &saved, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let previous_options = self.registry_options.image_gen.clone();
        self.sync_image_gen_options(&saved);
        if let Err(error) = self.rebuild_registry() {
            if let Err(rollback) = save_image_gen_configuration(&environment, &previous, None) {
                self.state
                    .notice(format!("图像生成设置回滚写盘失败：{rollback}"));
            }
            self.registry_options.image_gen = previous_options;
            return Err(error);
        }
        Ok(format!(
            "图像生成设置已保存到 {}（本会话即刻生效）。",
            path.display()
        ))
    }

    /// 把图像生成配置映射到运行期选项（图像生成工具按它发请求）。
    pub(super) fn sync_image_gen_options(&mut self, config: &ImageGenConfiguration) {
        let mut image_gen = self.registry_options.image_gen.clone();
        image_gen.enabled = config.enabled;
        image_gen.provider = config.provider.clone();
        image_gen.base_url = config.base_url.clone();
        image_gen.model = config.model.clone();
        image_gen.api_key = config.api_key.clone();
        image_gen.size = config.size.clone();
        image_gen.quality = config.quality.clone();
        image_gen.output_format = config.output_format.clone();
        image_gen.aspect_ratio = config.aspect_ratio.clone();
        image_gen.image_size = config.image_size.clone();
        image_gen.n = config.n;
        image_gen.timeout_seconds = config.timeout_seconds as f64;
        self.registry_options.image_gen = image_gen;
    }

    /// 子任务设置：总开关走功能开关，高级参数写 `subagents.toml`。
    ///
    /// 开关写盘后重建宿主工具表（决定 `subagent` 是否进表），高级参数写盘后立即生效
    /// （内核侧每次调用现读 `subagents.toml`）。
    pub(super) fn apply_subagent(&mut self, change: &SubagentChange) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        match change {
            SubagentChange::Enabled(enabled) => {
                let path = save_feature_enabled(&environment, "subagents", *enabled, None, None)
                    .map_err(|error| format!("设置未完成：{}", error.message()))?;
                // 开关决定 `subagent` 是否进表（宿主按角色清单声明它），所以要重建工具表；
                // 内核侧的子任务运行期本来就按每次调用现读 `subagents.toml`，不必重起。
                self.registry_options.subagent_roles = subagent_roles();
                self.rebuild_registry()?;
                if let Some(settings) = self.settings.as_mut() {
                    settings.refresh_tools(tool_switch_rows(&self.registry));
                }
                Ok(format!(
                    "子任务功能已{}，已保存到 {}（本会话即刻生效）。",
                    if *enabled { "开启" } else { "关闭" },
                    path.display()
                ))
            }
            SubagentChange::Advanced { key, value } => {
                let spec = SUBAGENT_ADVANCED_SPECS
                    .iter()
                    .find(|spec| spec.key == key.as_str())
                    .ok_or_else(|| format!("设置未完成：未知的子任务参数 {key}。"))?;
                // 子任务超时在配置里是浮点：写成整数会被类型校验拒绝。
                let raw = if spec.is_float {
                    omnicrawl_config::toml::Value::Float(*value as f64)
                } else {
                    omnicrawl_config::toml::Value::Integer(*value)
                };
                validate_subagent_advanced_setting(key, &raw)
                    .map_err(|error| format!("设置未完成：{}", error.message()))?;
                let path = save_subagent_setting(&environment, key, &raw, None)
                    .map_err(|error| format!("设置未完成：{}", error.message()))?;
                Ok(format!(
                    "{}已设为 {value}，已保存到 {}。",
                    spec.label,
                    path.display()
                ))
            }
        }
    }

    /// 视觉设置：写 `config.toml [vision]`；模型原生视觉有改动时连带写回 models.toml 或渠道。
    ///
    /// 与 Python 一致：启用但列表为空时拒绝保存；模型原生视觉先写（它自身失败会回滚），
    /// 再写代理配置。
    pub(super) fn apply_vision(&mut self, change: &VisionChange) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        if change.enabled && change.models.is_empty() {
            return Err("设置未完成：启用视觉模型代理前，至少添加一个视觉模型。".to_string());
        }
        if let Some(native) = change.native {
            let (scope, label) = vision_native_target(&self.llm);
            if label.is_empty() {
                return Err(
                    "设置未完成：当前模型缺少渠道或模型标识，无法保存模型原生视觉。".to_string(),
                );
            }
            // 自定义模型按 key 写 models.toml，其余按渠道标识写 config.toml。
            let (catalog_key, profile_id) = if scope == "model" {
                (label.as_str(), "")
            } else {
                ("", label.as_str())
            };
            save_native_vision(
                &environment,
                native,
                scope,
                catalog_key,
                profile_id,
                None,
                None,
            )
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        }
        let configuration = VisionConfiguration {
            enabled: change.enabled,
            models: change.models.iter().map(vision_ref_to_config).collect(),
        };
        let path = save_vision_configuration(&environment, &configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 代理是否可用、模型原生视觉是什么，都是按当前配置现算的，本会话即刻生效；
        // 重新解析模型视图让 `native_vision_enabled` 立刻读到新值，随后的
        // `push_session_settings` 会把新开关下发给内核。
        self.refresh_llm_view();
        Ok(format!(
            "视觉设置已保存到 {}（{} 个故障转移模型，本会话即刻生效）。",
            path.display(),
            change.models.len()
        ))
    }

    /// 重新解析整条模型视图（渠道、原生视觉与生成选项都跟着配置走）。
    ///
    /// 读不出来时保留当前视图：调用点多在「设置刚写盘」之后，此时磁盘是合法的，
    /// 真读不出来也只该沿用旧值，不该把设置面板的保存结果回滚掉。
    pub(super) fn refresh_llm_view(&mut self) {
        let environment = ConfigEnvironment::from_process();
        match load_llm_config(&environment) {
            Ok(refreshed) => self.llm = refreshed,
            Err(error) => {
                self.state
                    .notice(format!("重新解析模型配置失败，沿用当前视图：{error}"));
            }
        }
    }

    /// 按当前 `registry_options` 重建工具表。
    ///
    /// 只换工具表：正在跑的后台命令与取消令牌从旧表带过去，否则一次开关
    /// 会把 `monitor` 启动的后台进程从宿主的账上抹掉。
    pub(super) fn rebuild_registry(&mut self) -> Result<(), String> {
        let mut options = self.registry_options.clone();
        options.monitors = Some(self.registry.monitors().clone());
        options.cancel = Some(self.registry.cancel_token());
        let registry = ToolRegistry::new(
            &self.workspace,
            &options,
            self.options.command_timeout_seconds,
        )
        .map_err(|error| format!("设置未完成：{}", error.message))?;
        self.registry = Arc::new(registry);
        Ok(())
    }
}
