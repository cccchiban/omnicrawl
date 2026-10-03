//! 提示词装配：AGENTS.md 合并、Skill 索引、模式模板与 system prompt 组装。
//!
//! 对映 Python `agent/controllers/tools/building.py` 的 `_system_prompt` / `_load_agents_instructions`
//! 与 `agent/core.py` 的启动期准备。判定与文案在 [`omnicrawl_controllers`] 与
//! [`omnicrawl_extensions::skill`]，这里只做宿主侧的 I/O：读模板、读 AGENTS.md、扫 Skill 目录。
//!
//! 模板优先从磁盘目录读取（可执行文件祖先里的
//! `rust/assets/templates`（仓库检出）与 `omnicrawl/templates`（已发布载荷）），读不到时用
//! 编译期内嵌的同一份文本——脱离 Python 宿主分发时不必再带模板目录，行为与 Python 逐字一致。

use std::path::{Path, PathBuf};

use omnicrawl_config::core::runtime::{global_agents_path, ConfigEnvironment};
use omnicrawl_controllers::building::{
    advisor_guidelines_block, normalize_mode_name, system_prompt_with_mode,
};
use omnicrawl_controllers::shared::AGENTS_INSTRUCTIONS_FILE;
use omnicrawl_controllers::turn::context_messages::plugin_context_messages;
use omnicrawl_controllers::turn::prompt_context::{
    build_context_messages, build_system_prompt, ContextMessageInputs,
};
use omnicrawl_extensions::skill::{SkillManager, SkillMeta};
use serde_json::Value;

use crate::plugins::PluginHost;

/// 模板目录覆盖。
/// Agent 临时目录的默认展示路径（与 Python `_agent_temp_dir_display` 的兜底同值）。
pub const DEFAULT_AGENT_TEMP_DIR: &str = ".omnicrawl/.agent_tmp";

const EMBEDDED_SYSTEM_PROMPT: &str =
    include_str!("../../../../rust/assets/templates/system_prompt.md");
const EMBEDDED_PLAN_PROMPT: &str = include_str!("../../../../rust/assets/templates/plan.md");

/// 一次提示词装配所需的宿主输入。
pub struct PromptOptions {
    pub workspace_root: PathBuf,
    pub agent_temp_dir: String,
    pub workspace_detection_summary: String,
    /// 未给 `--system-prompt` 时用模板；给了则整段替换（与 Python 的显式覆盖同义）。
    pub system_prompt_override: Option<String>,
    /// 顾问运行期是否生效（决定是否追加顾问使用准则）。
    pub advisor_active: bool,
    pub advisor_blacklisted: bool,
    /// 启动时即生效的模式（空表示不给）。
    pub mode: String,
}

impl PromptOptions {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            agent_temp_dir: DEFAULT_AGENT_TEMP_DIR.to_string(),
            workspace_detection_summary: String::new(),
            system_prompt_override: None,
            advisor_active: false,
            advisor_blacklisted: false,
            mode: String::new(),
        }
    }
}

/// 装配好的提示词运行时：system prompt、上下文消息与当前活动模式。
pub struct PromptRuntime {
    template: String,
    advisor_block: String,
    mode_name: String,
    mode_prompt: String,
    workspace_root: PathBuf,
    agent_temp_dir: String,
    workspace_detection_summary: String,
    skill_manager: Option<SkillManager>,
    templates_dir: Option<PathBuf>,
    extra_skill_paths: Vec<String>,
    env: ConfigEnvironment,
}

impl PromptRuntime {
    /// 读模板、合并 AGENTS.md 并扫描 Skill 目录。
    pub fn load(env: &ConfigEnvironment, options: PromptOptions) -> Result<Self, String> {
        let templates_dir = locate_templates_dir(env);
        let template = match options.system_prompt_override.as_deref() {
            Some(text) if !text.trim().is_empty() => {
                build_system_prompt(text).map_err(|error| error.message().to_string())?
            }
            _ => {
                let raw = read_template(templates_dir.as_deref(), "system_prompt.md")
                    .unwrap_or_else(|| EMBEDDED_SYSTEM_PROMPT.to_string());
                build_system_prompt(&raw).map_err(|error| error.message().to_string())?
            }
        };
        let advisor_block =
            advisor_guidelines_block(options.advisor_active, options.advisor_blacklisted)
                .to_string();
        let mut runtime = Self {
            template,
            advisor_block,
            mode_name: String::new(),
            mode_prompt: String::new(),
            workspace_root: options.workspace_root,
            agent_temp_dir: options.agent_temp_dir,
            workspace_detection_summary: options.workspace_detection_summary,
            skill_manager: None,
            templates_dir,
            extra_skill_paths: Vec::new(),
            env: env.clone(),
        };
        runtime.discover_skills();
        if !options.mode.trim().is_empty() {
            runtime.activate_mode(&options.mode)?;
        }
        Ok(runtime)
    }

    /// 重新扫描 Skill 目录（工作区切换后调用）。
    pub fn discover_skills(&mut self) {
        let mut manager = SkillManager::new();
        manager.discover(
            Some(self.workspace_root.as_path()),
            &self.extra_skill_paths.clone(),
        );
        self.skill_manager = Some(manager);
    }

    /// 已发现的 Skill 数量（诊断与 `/skills` 用）。
    pub fn skill_count(&self) -> usize {
        self.skill_manager
            .as_ref()
            .map(|manager| manager.list_all().len())
            .unwrap_or(0)
    }

    /// 已发现的 Skill 索引条目。
    ///
    /// prompt cache 身份的 `skill_index_hash` 直接用这份列表，因此它必须与
    /// [`Self::context_messages_with_plugins`] 拼进 Skill 索引段的来源是**同一份**
    /// （同一个 `SkillManager::list_all()`），否则身份哈希会与实际发给模型的前缀脱节。
    pub fn skill_metas(&self) -> Vec<SkillMeta> {
        self.skill_manager
            .as_ref()
            .map(|manager| manager.list_all())
            .unwrap_or_default()
    }

    /// 当前活动模式（未启用时为空串，与 Python 的 `active_mode` 同义）。
    pub fn active_mode(&self) -> &str {
        self.mode_name.as_str()
    }

    /// 加载并启用模式模板；失败时保持原状态（与 Python 的先读后写一致）。
    pub fn activate_mode(&mut self, mode: &str) -> Result<String, String> {
        let normalized = normalize_mode_name(mode).map_err(|error| error.message().to_string())?;
        let file_name = format!("{normalized}.md");
        let prompt = match read_template(self.templates_dir.as_deref(), &file_name) {
            Some(text) => text,
            None if normalized == "plan" => EMBEDDED_PLAN_PROMPT.to_string(),
            None => {
                // 与 Python 的文案同形：包内模板确实缺这个文件。
                let path = self
                    .templates_dir
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("templates"))
                    .join(&file_name);
                return Err(format!(
                    "读取模式模板 {normalized}.md 失败：{} 不存在。",
                    path.display()
                ));
            }
        };
        let prompt = prompt.trim().to_string();
        if prompt.is_empty() {
            return Err(format!("模式模板 {normalized}.md 不能为空。"));
        }
        self.mode_name = normalized.clone();
        self.mode_prompt = prompt;
        Ok(normalized)
    }

    /// 基础 system prompt + 顾问准则 + 活动模式提示词。
    pub fn system_prompt(&self) -> String {
        system_prompt_with_mode(
            &self.template,
            &self.advisor_block,
            &self.mode_name,
            &self.mode_prompt,
        )
    }

    /// system 之外的上下文消息：项目规范、Skill、工具能力说明与运行环境。
    ///
    /// `has_tools` 由调用方按当前工具表给出（工具开关会改这张表）。
    /// 不带插件钩子，等价于 [`Self::context_messages_with_plugins`] 传 `None`。
    pub fn context_messages(&self, has_tools: bool) -> Result<Vec<Value>, String> {
        self.context_messages_with_plugins(has_tools, None, None, None)
    }

    /// 上下文消息装配的完整路径（含插件 Hook）。
    ///
    /// 与 Python `_context_messages` 同序：先跑 `context.build.before` 取插件附加上下文，
    /// 装配稳定/动态消息后追加插件消息，最后发 `context.build.after` 报最终条数。
    /// 插件缺失或 Hook 被拒（fail-open）时与 `context_messages` 结果一致。
    pub fn context_messages_with_plugins(
        &self,
        has_tools: bool,
        plugins: Option<&PluginHost>,
        session_id: Option<&str>,
        turn_id: Option<&str>,
    ) -> Result<Vec<Value>, String> {
        let skill_index_section = self
            .skill_manager
            .as_ref()
            .map(|manager| SkillManager::format_skills_for_prompt(&manager.list_all()));
        let project_instructions = self.project_instructions()?;
        let mut messages = build_context_messages(&ContextMessageInputs {
            workspace_root: &self.workspace_root.to_string_lossy(),
            project_instructions: &project_instructions,
            skill_index_section: skill_index_section.as_deref(),
            active_skill_context: None,
            has_tools,
            agent_temp_dir: &self.agent_temp_dir,
            workspace_detection_summary: &self.workspace_detection_summary,
        });
        if let Some(plugins) = plugins {
            let additional = plugins.context_build_before(session_id, turn_id);
            messages.extend(plugin_context_messages(additional.as_ref()));
            plugins.context_build_after(messages.len(), session_id, turn_id);
        }
        Ok(messages)
    }

    /// 合并用户级与项目级 AGENTS.md；项目级排在后面并优先。
    ///
    /// 读文件失败（非 UTF-8 或权限）按 Python 同文案报错，由调用方决定是否阻断启动。
    pub fn project_instructions(&self) -> Result<String, String> {
        let paths: [(String, PathBuf); 2] = [
            ("用户级".to_string(), global_agents_path(&self.env)),
            (
                "项目级".to_string(),
                self.workspace_root.join(AGENTS_INSTRUCTIONS_FILE),
            ),
        ];
        let mut sections: Vec<String> = Vec::new();
        for (scope, path) in paths {
            if !path.is_file() {
                continue;
            }
            let bytes = std::fs::read(&path)
                .map_err(|error| format!("读取{scope} {AGENTS_INSTRUCTIONS_FILE} 失败：{error}"))?;
            let content = String::from_utf8(bytes)
                .map_err(|_| format!("{scope} {AGENTS_INSTRUCTIONS_FILE} 必须是 UTF-8 文本。"))?;
            let content = content.trim();
            if !content.is_empty() {
                sections.push(format!("【{scope} AGENTS.md】\n{content}"));
            }
        }
        if sections.is_empty() {
            return Ok(String::new());
        }
        Ok(format!(
            "用户级规则提供默认协作约束；项目级规则针对当前工作区，项目级规则优先。\n\n{}",
            sections.join("\n\n")
        ))
    }
}

/// 模板目录：可执行文件祖先里的 `rust/assets/templates`（仓库检出）
/// 与 `omnicrawl/templates`（已发布载荷）。
pub fn locate_templates_dir(_env: &ConfigEnvironment) -> Option<PathBuf> {
    let mut base = std::env::current_exe().ok();
    while let Some(path) = base {
        base = path.parent().map(PathBuf::from);
        let Some(directory) = base.as_ref() else {
            break;
        };
        // 仓库检出里的新家优先（脱离 Python 包树后的单一来源），再落到已发布载荷的旧布局。
        let candidate = directory.join("rust").join("assets").join("templates");
        if candidate.join("system_prompt.md").is_file() {
            return Some(candidate);
        }
        let candidate = directory.join("omnicrawl").join("templates");
        if candidate.join("system_prompt.md").is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 读模板文件；目录缺失或文件不存在时返回 `None`（由调用方决定回落内嵌文本）。
fn read_template(dir: Option<&Path>, file_name: &str) -> Option<String> {
    let dir = dir?;
    let path = dir.join(file_name);
    let bytes = std::fs::read(path).ok()?;
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnicrawl_extensions::models::PluginsConfig;

    fn options() -> PromptOptions {
        let mut options = PromptOptions::new(std::env::temp_dir().join("oc-prompt-ws"));
        options.agent_temp_dir = ".omnicrawl/.agent_tmp".to_string();
        options
    }

    /// 仓库检出里模板目录的优先级：`rust/assets/templates` 必须先于旧的 `omnicrawl/templates`
    /// （后者只剩「已发布载荷」的兼容角色，脱离 Python 包树后会被删掉）。
    #[test]
    fn templates_dir_prefers_rust_assets() {
        // 隔离环境：不读进程变量，避免本机环境把用例带偏。
        let env = ConfigEnvironment::new("bundle", "test");
        let found = locate_templates_dir(&env).expect("仓库检出里应能找到模板目录");
        assert!(found.join("system_prompt.md").is_file(), "{found:?}");
        assert!(
            found
                .to_string_lossy()
                .replace('\\', "/")
                .ends_with("rust/assets/templates"),
            "应优先命中 rust/assets/templates：{found:?}"
        );
    }

    #[test]
    fn system_prompt_keeps_embedded_body_and_appends_mode() {
        let env = ConfigEnvironment::from_process();
        let mut runtime = PromptRuntime::load(&env, options()).expect("装配提示词");
        let base = runtime.system_prompt();
        // 模板正文里本来就有“提示词末尾可能追加 <active_mode_prompt …>”这句规范，
        // 因此只断言末尾没有真的追加区块。
        assert!(base.contains("OmniCrawl"), "内嵌模板应可用：{base}");
        assert!(!base.trim_end().ends_with("</active_mode_prompt>"));

        runtime.activate_mode("plan").expect("启用计划模式");
        let with_mode = runtime.system_prompt();
        assert!(with_mode.starts_with(base.as_str()));
        assert!(with_mode.contains("<active_mode_prompt name=\"plan\">"));
        assert!(with_mode.trim_end().ends_with("</active_mode_prompt>"));
    }

    #[test]
    fn advisor_block_only_when_active() {
        let env = ConfigEnvironment::from_process();
        let mut inactive = options();
        inactive.advisor_active = false;
        assert!(!PromptRuntime::load(&env, inactive)
            .expect("装配")
            .system_prompt()
            .contains("使用准则"));

        let mut active = options();
        active.advisor_active = true;
        assert!(PromptRuntime::load(&env, active)
            .expect("装配")
            .system_prompt()
            .contains("顾问策略（advisor）使用准则"));
    }

    #[test]
    fn context_messages_carry_runtime_context_without_agents_file() {
        let env = ConfigEnvironment::from_process();
        let runtime = PromptRuntime::load(&env, options()).expect("装配");
        let messages = runtime.context_messages(true).expect("上下文消息");
        let contents: Vec<String> = messages
            .iter()
            .map(|message| message["content"].as_str().unwrap().to_string())
            .collect();
        // 运行环境始终排在最后；项目规范只在工作区/用户级有 AGENTS.md 时出现
        // （测试用的临时工作区没有，用户级文件是否存在取决于本机）。
        let last = contents.last().expect("至少一条上下文消息");
        assert!(last.starts_with("<runtime_context"), "{last}");
        assert!(last.contains(".omnicrawl/.agent_tmp"));
        // 没有工具面时不注入工具能力说明。
        let without_tools = runtime.context_messages(false).expect("上下文消息");
        assert_eq!(without_tools.len(), messages.len() - 1);
    }

    #[test]
    fn context_messages_without_plugins_matches_plain_path() {
        let env = ConfigEnvironment::from_process();
        let runtime = PromptRuntime::load(&env, options()).expect("装配");
        let plain = runtime.context_messages(true).expect("上下文消息");
        let hooked = runtime
            .context_messages_with_plugins(true, None, None, None)
            .expect("上下文消息");
        assert_eq!(plain, hooked);
    }

    #[test]
    fn disabled_plugin_host_injects_nothing() {
        let env = ConfigEnvironment::from_process();
        let runtime = PromptRuntime::load(&env, options()).expect("装配");
        // 未启用（无 Manager）的插件宿主走 fail-open：附加上下文为空，消息集合不变。
        let plugins = PluginHost::new(
            &std::env::temp_dir().join("oc-prompt-ws"),
            PluginsConfig::default(),
        );
        let plain = runtime.context_messages(true).expect("上下文消息");
        let hooked = runtime
            .context_messages_with_plugins(true, Some(&plugins), Some("session-1"), Some("turn-1"))
            .expect("上下文消息");
        assert_eq!(plain, hooked);
    }

    #[test]
    fn invalid_mode_name_is_rejected_with_python_wording() {
        let env = ConfigEnvironment::from_process();
        let mut runtime = PromptRuntime::load(&env, options()).expect("装配");
        let error = runtime.activate_mode("Plan Mode").expect_err("非法模式名");
        assert_eq!(error, "模式名称只能使用小写字母、数字和单连字符。");
        assert_eq!(runtime.active_mode(), "");
    }
}
