//! 设置与后台任务的接线：把界面事件翻成配置写盘、内核推送与后台任务。
//!
//! 界面层不碰配置，这一层负责把 `SettingsEvent` 落到磁盘与内核。

use super::*;

impl App {


    /// 把当前对话记录投影成顾问可见的工作分支（user/assistant 文本，保持顺序）。
    pub(super) fn refresh_advisor_context(&self) {
        let messages: Vec<serde_json::Value> = self
            .state
            .records
            .iter()
            .filter_map(|record| match record {
                Record::User(message) => Some(json!({"role": "user", "content": message.text})),
                Record::Assistant(text) => Some(json!({"role": "assistant", "content": text})),
                _ => None,
            })
            .collect();
        if let Ok(mut slot) = self.advisor_context.lock() {
            *slot = messages;
        }
    }


    /// 应用一次设置：先写盘并让宿主侧生效，再把变更推给内核做热更新。
    ///
    /// 写盘失败即视为「设置未完成」，值不变；内核拒绝即时更新时配置已落盘、
    /// 宿主侧已生效，只补一句「下次会话生效」，不回滚（两件事分开）。
    pub(super) fn apply_settings_change(&mut self, change: &SettingsChange) -> Result<String, String> {
        let message = match change {
            SettingsChange::Model { key } => self.apply_model_channel(key)?,
            SettingsChange::Channels { rows, default_key } => {
                self.apply_channels(rows, default_key)?
            }
            SettingsChange::DecisionModels { rows, default_key } => {
                self.apply_decision_models(rows, default_key)?
            }
            SettingsChange::EmbeddingModels { rows, default_key } => {
                self.apply_embedding_models(rows, default_key)?
            }
            SettingsChange::EmbeddingEnabled { enabled } => {
                self.apply_embedding_enabled(*enabled)?
            }
            SettingsChange::DecisionSwitch { key, enabled } => {
                self.apply_decision_switch(key, *enabled)?
            }
            SettingsChange::DecisionLocal(change) => self.apply_decision_local(change)?,
            SettingsChange::ToolApproval { mode } => self.apply_tool_approval(mode)?,
            SettingsChange::ToolSwitch { name, enabled } => {
                self.apply_tool_switch(name, *enabled)?
            }
            SettingsChange::ContextWindow { tokens } => self.apply_context_window(*tokens)?,
            SettingsChange::CompactionPercent { percent } => {
                self.apply_compaction_percent(*percent)?
            }
            SettingsChange::Reasoning { effort } => self.apply_reasoning_effort(effort)?,
            SettingsChange::ShowThinking { enabled } => self.apply_show_thinking(*enabled)?,
            SettingsChange::Feature { key, enabled } => self.apply_feature(key, *enabled)?,
            SettingsChange::Form { kind, values } => self.apply_form(*kind, values)?,
            SettingsChange::Subagent(subagent) => self.apply_subagent(subagent)?,
            SettingsChange::Mcp(change) => self.apply_mcp(change)?,
            SettingsChange::Vision(vision) => self.apply_vision(vision)?,
            SettingsChange::Tts(change) => self.apply_tts(change)?,
        };
        self.push_session_settings(change);
        Ok(message)
    }


    /// 按最新配置重建审查运行期（决策渠道或开关变更后调用）。
    pub(super) fn rebuild_review(&mut self, environment: &ConfigEnvironment) {
        self.review = build_review_options(&self.llm, environment);
    }


    /// 按最新配置重建提问托管运行期（决策渠道或开关变更后调用）。
    pub(super) fn rebuild_custody(&mut self, environment: &ConfigEnvironment) {
        self.custody = Some(omnicrawl_host::tools::custody_options_from_config(
            environment,
        ));
    }


    /// 按最新配置重建建议运行期（决策渠道或开关变更后调用）。
    ///
    /// 建议由宿主在提交前发起（它是唯一握得住「用户这一条原文」的一方），因此只需
    /// 换掉运行期，没有要下发给内核的能力声明。两类建议各有一个开关，一并重建。
    pub(super) fn rebuild_suggest(&mut self, environment: &ConfigEnvironment) {
        self.suggest = Some(omnicrawl_host::tools::suggest_options_from_config(
            environment,
        ));
        self.subagent_suggest = Some(omnicrawl_host::tools::subagent_suggest_options_from_config(
            environment,
        ));
    }


    /// 按最新配置重建工具调用淘汰运行期（决策渠道或开关变更后调用）。
    ///
    /// 淘汰由内核发起（它是唯一能看到 `messages` 与其尾部位置的一方），因此这里除了重建运行期，
    /// 还必须把新的能力声明下发给内核：关掉时内核立刻停止发起 `context.prune`。
    pub(super) fn rebuild_prune(&mut self, environment: &ConfigEnvironment) -> Result<(), String> {
        self.prune = Some(omnicrawl_host::tools::prune_options_from_config(
            environment,
        ));
        self.push_session_settings_raw(SessionSettingsParams {
            model: Some(Box::new(SessionModelSettings {
                tool_call_prune: Some(
                    self.prune
                        .as_ref()
                        .map(|prune| prune.enabled)
                        .unwrap_or(false),
                ),
                ..SessionModelSettings::default()
            })),
            compaction: None,
        });
        Ok(())
    }




    /// 把本次变更推给内核做热更新；结果在 `handle_frame` 里回填。
    ///
    /// 只有内核持有的设置在协议上有对应字段：工具声明、上下文窗口与压缩阈值、推理强度。
    /// 思考显示与功能开关属于宿主/配置面，没有帧可发（`params` 为空时直接返回）。
    pub(super) fn push_session_settings(&mut self, change: &SettingsChange) {
        let compaction = match change {
            SettingsChange::ContextWindow { .. } | SettingsChange::CompactionPercent { .. } => {
                let percent = self
                    .settings
                    .as_ref()
                    .map(|settings| settings.compaction_percent())
                    .unwrap_or(80);
                let window = self
                    .settings
                    .as_ref()
                    .map(|settings| settings.context_window_tokens())
                    .unwrap_or(128_000);
                Some(compaction_settings(window, percent))
            }
            _ => None,
        };
        let model = match change {
            // 换渠道：模型 id 与整条渠道一起下发（空串会被内核按「不改」忽略）。
            //
            // 视觉判定必须同帧下发：内核只在握手时读过一次模型能力，换模型后不带
            // `vision_capability`，带图观察仍按旧模型路由（看得见图的模型也会被摄掉图片）。
            SettingsChange::Model { .. } | SettingsChange::Channels { .. } => {
                Some(SessionModelSettings {
                    model: Some(self.llm.model.clone()),
                    provider: Some(self.llm.provider.clone()),
                    protocol: Some(self.llm.protocol.clone()),
                    base_url: Some(self.llm.base_url.clone()),
                    api_key: Some(self.llm.api_key.clone()),
                    native_vision: self.native_vision_override(),
                    // 能力未知（detected 模型没有 `capabilities.vision`）按不支持处理：
                    // 与 `model_supports_vision` 的默认值一致，显式写 false 才表达得出
                    // 「这次是模型切换」。
                    vision_capability: Some(self.llm.vision_capability.unwrap_or(false)),
                    ..SessionModelSettings::default()
                })
            }
            SettingsChange::ToolSwitch { .. } => Some(SessionModelSettings {
                tools: Some(self.registry.declarations()),
                ..SessionModelSettings::default()
            }),
            SettingsChange::ContextWindow { tokens } => Some(SessionModelSettings {
                context_window_tokens: Some(*tokens),
                ..SessionModelSettings::default()
            }),
            SettingsChange::Reasoning { effort } => Some(SessionModelSettings {
                reasoning_effort: Some(effort.clone()),
                ..SessionModelSettings::default()
            }),
            // 视觉：原生视觉开关在握手时随模型配置交给内核一次，改完要重新下发，
            // 否则带图观察仍按握手那一刻的判定走（开了原生视觉图片还是被摄掉）。
            SettingsChange::Vision(_) => Some(SessionModelSettings {
                native_vision: Some(self.native_vision_enabled()),
                ..SessionModelSettings::default()
            }),
            _ => None,
        };
        // 配置写盘已经成功，这里只为「本会话即时生效」；内核拒绝不是失败。
        let params = SessionSettingsParams {
            model: model.map(Box::new),
            compaction,
        };
        if params.model.is_none() && params.compaction.is_none() {
            return;
        }
        let id = self.send(Command::SessionSettings(Box::new(params)));
        self.settings_request = Some((id, change.clone()));
    }


    /// 内核对 `session.settings` 的响应：拒绝时在状态文本里如实补一句。
    pub(super) fn apply_settings_response(&mut self, change: &SettingsChange, frame: &Frame) {
        let Some(error) = frame.error.as_ref() else {
            return;
        };
        self.state
            .notice(format!("内核未接受设置更新：{}", error.message));
        if let Some(settings) = self.settings.as_mut() {
            settings.note_kernel_rejection(
                change,
                &format!(
                    "内核未接受即时更新（{}），将在下次会话生效。",
                    error.message
                ),
            );
        }
    }
}
