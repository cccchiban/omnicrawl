//! 工具注册表：工具表、名称/参数归一化、Schema 校验与执行分发。
//!
//! 工具表来自内核已搬好的 `omnicrawl-controllers::tool_catalog`（数据由
//! `omnicrawl/agent/toolkit/tools.py` 导出），归一化与校验来自同 crate 的
//! `tool_args`，本模块只负责「这张表里哪些工具这个宿主真的能执行」。

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_controllers::tool_args::{
    mcp_fields, mcp_prompt_arguments_error, mcp_prompt_result, mcp_resource_result,
    mcp_tool_result, normalize_tool_call, tool_validation_error_result, validate_tool_arguments,
};
use omnicrawl_controllers::tool_catalog::{
    build_agent_tools, McpItemRef, McpToolRef, ToolCatalogOptions, ToolSpec,
};
use omnicrawl_controllers::types::ToolImageAttachment;
use omnicrawl_core::{ToolCall, ToolResult};
use omnicrawl_mcp::client::McpClientManager;
use serde_json::{Map, Value};

use super::advisor::{self, AdvisorOptions};
use super::command::{CancelToken, CommandRunner, Shell, DEFAULT_COMMAND_TIMEOUT_SECONDS};
use super::config_tool;
use super::decision_search::RerankOptions;
use super::declarations::declaration;
use super::error::{command_result, text_failure, text_success, ToolError};
use super::fetcher::{self, FetcherOptions};
use super::image_gen::{self, ImageGenOptions};
use super::knowledge::{self, KnowledgeBase};
use super::memory::{self, MemoryOptions};
use super::monitor::MonitorManager;
use super::paths::WorkspacePaths;
use super::tts::TtsOptions;
use super::web_search::{self, WebSearchOptions};
use super::windows;
use super::{edit, finding, git, grep, listing, read, read_image, skills, write};

/// 本宿主已实现执行体的工具。
pub const IMPLEMENTED_TOOLS: [&str; 35] = [
    "read",
    "read_image",
    "write_file",
    "Edit_file",
    "bash",
    "powershell",
    "list",
    "find",
    "grep",
    "skill_search",
    "git",
    "monitor",
    "kb_search",
    "kb_read",
    "kb_write",
    "kb_append",
    "kb_list",
    "kb_delete",
    "memory_search",
    "memory_read",
    "memory_expand_related",
    "memory_write",
    "memory_delete",
    "web_search",
    "fetcher",
    "image_gen",
    "tts_synthesize",
    "windows_window",
    "windows_control",
    "windows_input",
    "windows_clipboard",
    "windows_screenshot",
    "advisor",
    "config_edit",
    "computer_use",
];
/// 内核自持的工具：声明进表，执行体在本宿主（`update_todos` / `ask_user` / `pause_work`）。
pub const META_RUNNERS: [&str; 3] = ["update_todos", "ask_user", "pause_work"];
/// 工具表里按 runner 绑定进表的工具：本宿主已绑定这三个。
pub const BOUND_RUNNERS: [&str; 3] = ["find", "git", "monitor"];
/// 知识库工具：runner 名即工具名，整组注册或整组不注册。
pub const KNOWLEDGE_RUNNERS: [&str; 6] = [
    "kb_search",
    "kb_read",
    "kb_write",
    "kb_append",
    "kb_list",
    "kb_delete",
];
/// 记忆工具：整组由 `memory_enabled` 控制。
pub const MEMORY_RUNNERS: [&str; 5] = [
    "memory_search",
    "memory_read",
    "memory_expand_related",
    "memory_write",
    "memory_delete",
];
/// 联网工具：runner 名即工具名，整组不依赖开关。
pub const NET_RUNNERS: [&str; 2] = ["web_search", "fetcher"];
/// Skill 检索：runner 名即工具名；索引不再注入上下文，检索入口必须始终在表里。
pub const SKILL_RUNNERS: [&str; 1] = ["skill_search"];
/// 视觉工具：`read_image` 除文本结果外还要把图片作为视觉附件交给循环。
pub const VISION_RUNNERS: [&str; 1] = ["read_image"];
/// 图像生成：与 Python 一致，工具始终进表；未启用时调用会给出明确错误。
pub const IMAGE_RUNNERS: [&str; 1] = ["image_gen"];
/// 语音合成：与 Python 一致，未启用时整组不进表（runner 名与工具名不同）。
pub const TTS_RUNNERS: [&str; 1] = ["tts"];
/// 顾问工具：只有显式启用并选中顾问模型时才进表（与 Python 的 AdvisorConfig 一致）。
pub const ADVISOR_RUNNER: &str = "advisor";
/// Computer Use 工具：由内核自持执行，只受 `[computer_use] enabled` 约束。
pub const COMPUTER_USE_RUNNER: &str = "computer_use";
/// Windows 桌面工具：整组注册或整组不注册（与 Python 的组规则一致）。
pub const WINDOWS_RUNNERS: [&str; 5] = [
    "windows_window",
    "windows_control",
    "windows_input",
    "windows_clipboard",
    "windows_screenshot",
];

/// 一次执行的产物：工具结果 + 随结果注入的视觉附件（含分析提示词）。
#[derive(Debug, Clone)]
pub struct ToolExecution {
    pub result: ToolResult,
    pub images: Vec<ToolImageAttachment>,
    pub vision_prompt: String,
}

impl Default for ToolExecution {
    fn default() -> Self {
        Self {
            result: ToolResult {
                ok: false,
                output: String::new(),
                full_output: String::new(),
                error_code: None,
                retryable: false,
            },
            images: Vec::new(),
            vision_prompt: String::new(),
        }
    }
}

#[derive(Clone, Default)]
pub struct RegistryOptions {
    /// 记忆工具是否注册。
    pub memory_enabled: bool,
    /// 记忆工具的运行期配置（三处作用域与当前会话）。
    pub memory: MemoryOptions,
    /// SubAgent 角色清单（名称 + 定义里的一句话描述）；未启用时不给这个工具。
    pub subagent_roles: Vec<(String, String)>,
    /// 会话由内核自持时，`recall_session_evidence` 由内核本地作答。
    pub session_held_by_kernel: bool,
    /// 被关闭的内置工具（`tools` 配置段里的开关）：这些工具不进表，
    /// 模型不可见即不可调用（与 Python 的 `agent.config.disabled_tools` 同义）。
    pub disabled_tools: Vec<String>,
    /// 知识库根目录；未配置时用 `~/.OmniCrawl/knowledge`。
    pub knowledge_root: Option<PathBuf>,
    /// 配置工具的配置环境；未注入时按进程环境解析 `~/.OmniCrawl`。
    pub config_env: Option<ConfigEnvironment>,
    /// 知识库检索重排（`decision_models.toml` 的 `kb_search_rerank` 开关）。
    pub knowledge_rerank: Arc<RerankOptions>,
    /// Skill 检索重排（`decision_models.toml` 的 `skill_search_rerank` 开关）。
    pub skill_rerank: Arc<RerankOptions>,
    /// 联网工具的运行期配置（传输可注入，测试用桩替换）。
    pub web_search: WebSearchOptions,
    pub fetcher: FetcherOptions,
    /// 图像生成配置（传输可注入；未启用时工具仍在表里，调用时报错）。
    pub image_gen: ImageGenOptions,
    /// 语音合成配置：未启用时为 `None`，`tts_synthesize` 不进表（与 Python 一致）。
    pub tts: Option<Arc<TtsOptions>>,
    /// 顾问运行期配置（未启用时 advisor 不进表）。
    pub advisor: AdvisorOptions,
    /// Computer Use 是否进表：只受 `[computer_use] enabled` 约束，与 `[subagents]` 无关。
    pub computer_use: bool,
    /// MCP 管理器：已发现的工具/资源/Prompt 进表，执行体走这个句柄。
    pub mcp: Option<Arc<McpClientManager>>,
    /// 复用既有的后台任务管理器：工具表重建时正在跑的后台命令不能被丢掉。
    pub monitors: Option<MonitorManager>,
    /// 复用既有的取消令牌：重建工具表不改取消语义（回合取消仍能回收本回合进程树）。
    pub cancel: Option<CancelToken>,
}

impl crate::host::ToolFacts for ToolRegistry {
    fn facts(&self, tool: &str) -> Option<(String, String)> {
        self.approval_facts(tool)
    }
}

pub struct ToolRegistry {
    paths: WorkspacePaths,
    commands: CommandRunner,
    monitors: MonitorManager,
    knowledge: KnowledgeBase,
    knowledge_rerank: Arc<RerankOptions>,
    skill_rerank: Arc<RerankOptions>,
    memory: MemoryOptions,
    web_search: WebSearchOptions,
    fetcher: FetcherOptions,
    image_gen: ImageGenOptions,
    tts: Option<Arc<TtsOptions>>,
    advisor: AdvisorOptions,
    mcp: Option<Arc<McpClientManager>>,
    /// 配置工具的配置环境（构造时定下，之后不随工作区切换而变）。
    config_env: ConfigEnvironment,
    /// MCP 能力名：工具用逻辑名，资源/Prompt 用各自的适配工具名。
    mcp_tool_names: BTreeSet<String>,
    mcp_resource_names: Vec<(String, String)>,
    mcp_prompt_names: Vec<(String, String)>,
    /// SubAgent 角色清单（名称 + 定义里的一句话描述）：与 `subagent` 工具的角色枚举同源，
    /// 子代理建议的候选直接取这里，不需要第二处发现逻辑。
    subagent_roles: Vec<(String, String)>,
    specs: Vec<ToolSpec>,
    cancel: CancelToken,
}

impl ToolRegistry {
    pub fn new(
        root: impl Into<PathBuf>,
        options: &RegistryOptions,
        command_timeout_seconds: i64,
    ) -> Result<Self, ToolError> {
        let paths = WorkspacePaths::new(root);
        let mut available: Vec<String> = META_RUNNERS
            .iter()
            .chain(BOUND_RUNNERS.iter())
            .chain(KNOWLEDGE_RUNNERS.iter())
            .chain(MEMORY_RUNNERS.iter())
            .chain(NET_RUNNERS.iter())
            .chain(SKILL_RUNNERS.iter())
            .chain(IMAGE_RUNNERS.iter())
            .chain(WINDOWS_RUNNERS.iter())
            .map(|name| (*name).to_string())
            .collect();
        if options.tts.is_some() {
            available.extend(TTS_RUNNERS.iter().map(|name| (*name).to_string()));
        }
        if options.advisor.active() {
            available.push(ADVISOR_RUNNER.to_string());
        }
        // 视觉工具始终进表（与 Python 的 `_build_tools` 一致）：图片的去向由路由决定——
        // 原生视觉直送主模型，否则交给 `[vision]` 代理，两者都没有时只把图片元数据给模型。
        available.extend(VISION_RUNNERS.iter().map(|name| (*name).to_string()));
        if options.session_held_by_kernel {
            available.push("evidence_recall".to_string());
        }
        // `subagent` 由内核自持执行（子回合需要模型运行时），宿主只把它声明给模型；
        // 角色为空时不进表，避免模型看到一个必然失败的入口。
        if !options.subagent_roles.is_empty() {
            available.push("subagent".to_string());
        }
        // `computer_use` 同样由内核自持（子回合需要模型运行时），只受自己的开关约束：
        // 它不受 `[subagents] enabled` 影响，关掉普通子代理不该连坐关掉桌面操作。
        if options.computer_use {
            available.push(COMPUTER_USE_RUNNER.to_string());
        }
        let available_refs: Vec<&str> = available.iter().map(String::as_str).collect();
        // MCP 能力来自管理器的注册表快照：工具直接以 `server.tool` 进表，资源与 Prompt
        // 各生成一个适配工具（与 Python 的 `build_mcp_tools` 一致）。发现时机在宿主启动时，
        // 因此这张表在声明给内核之后不会再变。
        let mcp_tools: Vec<McpToolRef> = options
            .mcp
            .as_ref()
            .map(|manager| {
                manager
                    .tools()
                    .iter()
                    .map(|meta| McpToolRef {
                        logical_name: meta.logical_name.clone(),
                        server_name: meta.server_name.clone(),
                        description: meta.description.clone(),
                        argument_schema: meta.argument_schema(),
                        requires_confirmation: meta.requires_confirmation,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mcp_resources: Vec<McpItemRef> = options
            .mcp
            .as_ref()
            .map(|manager| {
                manager
                    .resources()
                    .iter()
                    .map(|meta| McpItemRef {
                        logical_name: meta.logical_uri.clone(),
                        server_name: meta.server_name.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mcp_prompts: Vec<McpItemRef> = options
            .mcp
            .as_ref()
            .map(|manager| {
                manager
                    .prompts()
                    .iter()
                    .map(|meta| McpItemRef {
                        logical_name: meta.logical_name.clone(),
                        server_name: meta.server_name.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mcp_tool_names: BTreeSet<String> = mcp_tools
            .iter()
            .map(|meta| meta.logical_name.clone())
            .collect();
        let mcp_resource_names: Vec<(String, String)> = mcp_resources
            .iter()
            .map(|meta| {
                (
                    format!("mcp_read_resource__{}", meta.logical_name),
                    meta.logical_name.clone(),
                )
            })
            .collect();
        let mcp_prompt_names: Vec<(String, String)> = mcp_prompts
            .iter()
            .map(|meta| {
                (
                    format!("mcp_get_prompt__{}", meta.logical_name),
                    meta.logical_name.clone(),
                )
            })
            .collect();
        let subagent_types: Vec<String> = options
            .subagent_roles
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        let catalog_options = ToolCatalogOptions {
            available: &available_refs,
            memory_enabled: options.memory_enabled,
            subagent_types: &subagent_types,
            mcp_tools: &mcp_tools,
            mcp_resources: &mcp_resources,
            mcp_prompts: &mcp_prompts,
            disabled_tools: &options.disabled_tools,
        };
        let specs = build_agent_tools(&catalog_options)
            .map_err(|error| ToolError::new(error.message().to_string()))?;
        let commands = CommandRunner::new(
            paths.root(),
            if command_timeout_seconds <= 0 {
                DEFAULT_COMMAND_TIMEOUT_SECONDS
            } else {
                command_timeout_seconds
            },
        );
        let monitors = match options.monitors.clone() {
            Some(monitors) => monitors,
            None => MonitorManager::new(paths.root()),
        };
        let knowledge = KnowledgeBase::new(
            options
                .knowledge_root
                .clone()
                .unwrap_or_else(knowledge::default_root),
        );
        let memory = MemoryOptions {
            workspace_root: paths.root().to_path_buf(),
            ..options.memory.clone()
        };
        let image_gen = ImageGenOptions {
            workspace_root: paths.root().to_path_buf(),
            ..options.image_gen.clone()
        };
        let advisor = AdvisorOptions {
            workspace_root: paths.root().to_path_buf(),
            ..options.advisor.clone()
        };
        Ok(Self {
            paths,
            commands,
            monitors,
            knowledge,
            knowledge_rerank: options.knowledge_rerank.clone(),
            skill_rerank: options.skill_rerank.clone(),
            memory,
            web_search: options.web_search.clone(),
            fetcher: options.fetcher.clone(),
            image_gen,
            tts: options.tts.clone(),
            advisor,
            mcp: options.mcp.clone(),
            config_env: options
                .config_env
                .clone()
                .unwrap_or_else(ConfigEnvironment::from_process),
            mcp_tool_names,
            mcp_resource_names,
            mcp_prompt_names,
            subagent_roles: options.subagent_roles.clone(),
            specs,
            cancel: options.cancel.clone().unwrap_or_default(),
        })
    }

    pub fn paths(&self) -> &WorkspacePaths {
        &self.paths
    }

    pub fn monitors(&self) -> &MonitorManager {
        &self.monitors
    }

    /// 把本回合内启动的后台任务挂到该回合：回合取消时只回收自己的任务。
    pub fn set_monitor_scope(&self, scope: Option<&str>) {
        self.monitors
            .set_scope(scope.map(|value| value.to_string()));
    }

    /// 取消一个回合的后台任务；返回被回收的任务 id。
    pub fn stop_monitors_in_scope(&self, scope: &str, reason: &str) -> Vec<String> {
        self.monitors.stop_scope(scope, reason)
    }

    /// 宿主退出：回收仍在运行的后台进程，避免留下孤儿。
    pub fn close_monitors(&self) {
        self.monitors.close();
    }

    /// 宿主退出：关闭 MCP 连接，不给系统留下孤儿子进程。
    pub fn close_mcp(&self) {
        if let Some(manager) = self.mcp.as_ref() {
            manager.close();
        }
    }

    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// provider 声明（`initialize.model.tools`）。
    pub fn declarations(&self) -> Vec<Value> {
        self.specs.iter().map(declaration).collect()
    }

    /// 审批判定所需的两条事实：工具说明与参数 schema 文本。
    ///
    /// `review` 模式的删除意图识别要用它们（`is_delete_behavior_tool_call`）；
    /// 工具不在表里时返回 `None`，判定方按空串处理。
    pub fn approval_facts(&self, name: &str) -> Option<(String, String)> {
        self.specs
            .iter()
            .find(|spec| spec.name == name)
            .map(|spec| (spec.description.clone(), spec.argument_schema.clone()))
    }

    pub fn tool_pairs(&self) -> Vec<(&str, &str)> {
        self.specs
            .iter()
            .map(|spec| (spec.name.as_str(), spec.argument_schema.as_str()))
            .collect()
    }

    /// 执行者可用工具面（工具名 + 说明），供 advisor 了解执行者手段。
    pub fn tool_inventory(&self) -> Vec<(String, String)> {
        self.specs
            .iter()
            .map(|spec| (spec.name.clone(), spec.description.clone()))
            .collect()
    }

    /// SubAgent 角色清单（名称 + 描述）：与 `subagent` 工具的角色枚举同源。
    pub fn subagent_roles(&self) -> &[(String, String)] {
        &self.subagent_roles
    }

    pub fn is_implemented(&self, name: &str) -> bool {
        IMPLEMENTED_TOOLS.contains(&name)
            || self.mcp_tool_names.contains(name)
            || self.mcp_resource_names.iter().any(|(tool, _)| tool == name)
            || self.mcp_prompt_names.iter().any(|(tool, _)| tool == name)
    }

    /// MCP 管理器（未配置时为 `None`）。
    pub fn mcp(&self) -> Option<&Arc<McpClientManager>> {
        self.mcp.as_ref()
    }

    /// 记忆作用域选项。
    ///
    /// 这里的 `workspace_root` 已在构造时按工作区补全（与 `paths.root()` 同源），
    /// 因此调用方拿到的是可直接 `store(scope)` 的选项，而不是注册表外的原始入参。
    pub fn memory(&self) -> &MemoryOptions {
        &self.memory
    }

    /// 该名称是不是 MCP Server 提供的 **Tool**（Resource / Prompt 的适配工具不算）。
    ///
    /// 审批拒绝的审计只用这张表：Python 的 `_approve_tool_call` 用
    /// `tool.name in mcp_manager.registry.tools` 判定，Resource / Prompt 不在其中。
    pub fn is_mcp_tool(&self, name: &str) -> bool {
        self.mcp_tool_names.contains(name)
    }

    /// 执行一次工具调用。
    ///
    /// 返回 `None` 表示这个宿主不认识该工具（调用方按「不可用」回观察），
    /// 参数校验失败会返回结构化的 `invalid_arguments` 结果供模型修正。
    pub fn execute(&self, call: &ToolCall) -> Option<ToolResult> {
        self.execute_with_vision(call)
            .map(|execution| execution.result)
    }

    /// 执行一次工具调用，并带回需要随结果注入的视觉附件（目前只有 `read_image` 用得上）。
    pub fn execute_with_vision(&self, call: &ToolCall) -> Option<ToolExecution> {
        let pairs = self.tool_pairs();
        let (name, arguments) = normalize_tool_call(&call.name, &call.arguments, &pairs);
        let spec = self.specs.iter().find(|spec| spec.name == name)?;
        if !self.is_implemented(&name) {
            return None;
        }
        let issues =
            validate_tool_arguments(&spec.argument_schema, &Value::Object(arguments.clone()));
        if !issues.is_empty() {
            let rejected = tool_validation_error_result(&spec.name, &spec.argument_schema, &issues);
            return Some(ToolExecution {
                result: from_controllers(rejected),
                ..ToolExecution::default()
            });
        }
        if matches!(
            name.as_str(),
            "windows_screenshot" | "windows_input" | "windows_control" | "windows_window"
        ) {
            let outcome = match name.as_str() {
                "windows_screenshot" => windows::windows_screenshot(&self.paths, &arguments),
                "windows_input" => windows::windows_input(&self.paths, &arguments),
                "windows_control" => windows::windows_control(&self.paths, &arguments),
                _ => windows::windows_window(&self.paths, &arguments),
            };
            // 操作类工具自带确认截图：点明它来自刚才的动作，别让模型以为还要再截一次。
            return Some(match outcome {
                Ok(outcome) => ToolExecution {
                    vision_prompt: if name == "windows_screenshot" || outcome.images.is_empty() {
                        String::new()
                    } else {
                        windows::snapshot_vision_prompt()
                    },
                    result: text_success(outcome.output),
                    images: outcome.images,
                },
                Err(error) => ToolExecution {
                    result: text_failure(&error),
                    ..ToolExecution::default()
                },
            });
        }
        if name == "read_image" {
            return Some(match read_image::read_image(&self.paths, &arguments) {
                Ok(outcome) => ToolExecution {
                    result: text_success(outcome.output),
                    images: outcome.images,
                    vision_prompt: outcome.prompt,
                },
                Err(error) => ToolExecution {
                    result: text_failure(&error),
                    ..ToolExecution::default()
                },
            });
        }
        if self.mcp_tool_names.contains(&name) {
            return Some(self.call_mcp_tool(&name, &arguments));
        }
        if let Some((_, logical_uri)) = self
            .mcp_resource_names
            .iter()
            .find(|(tool, _)| tool == &name)
        {
            return Some(self.read_mcp_resource(logical_uri));
        }
        if let Some((_, logical_name)) =
            self.mcp_prompt_names.iter().find(|(tool, _)| tool == &name)
        {
            return Some(self.get_mcp_prompt(logical_name, &arguments));
        }
        let result = match name.as_str() {
            "read" => outcome_result(read::read(&self.paths, &arguments)),
            "write_file" => outcome_result(write::write_file(&self.paths, &arguments)),
            "Edit_file" => outcome_result(edit::edit_file(&self.paths, &arguments)),
            "bash" => self.run_command(&arguments, Shell::Bash),
            "powershell" => self.run_command(&arguments, Shell::PowerShell),
            "list" => outcome_result(listing::list_files(&self.paths, &arguments)),
            "find" => outcome_result(finding::find_files(&self.paths, &arguments)),
            "grep" => outcome_result(grep::grep(&self.paths, &arguments)),
            "skill_search" => outcome_result(skills::skill_search(
                self.paths.root(),
                &self.config_env,
                &self.skill_rerank,
                &arguments,
            )),
            "git" => git::git_tool(&self.paths, &arguments),
            "monitor" => self.run_monitor(&arguments),
            "kb_search" => outcome_result(knowledge::kb_search(
                &self.knowledge,
                &self.knowledge_rerank,
                &arguments,
            )),
            "kb_read" => outcome_result(knowledge::kb_read(&self.knowledge, &arguments)),
            "kb_write" => outcome_result(knowledge::kb_write(&self.knowledge, &arguments)),
            "kb_append" => outcome_result(knowledge::kb_append(&self.knowledge, &arguments)),
            "kb_list" => outcome_result(knowledge::kb_list(&self.knowledge, &arguments)),
            "kb_delete" => outcome_result(knowledge::kb_delete(&self.knowledge, &arguments)),
            "memory_search" => outcome_result(memory::memory_search(&self.memory, &arguments)),
            "memory_read" => outcome_result(memory::memory_read(&self.memory, &arguments)),
            "memory_expand_related" => {
                outcome_result(memory::memory_expand_related(&self.memory, &arguments))
            }
            "memory_write" => outcome_result(memory::memory_write(&self.memory, &arguments)),
            "memory_delete" => outcome_result(memory::memory_delete(&self.memory, &arguments)),
            "web_search" => outcome_result(web_search::web_search(&self.web_search, &arguments)),
            "fetcher" => outcome_result(fetcher::fetcher(&self.fetcher, &arguments)),
            "image_gen" => outcome_result(image_gen::image_gen(&self.image_gen, &arguments)),
            "tts_synthesize" => match self.tts.as_ref() {
                Some(tts) => tts.tts_synthesize(&arguments),
                None => text_failure(&ToolError::new("TTS 未启用。")),
            },
            "windows_clipboard" => outcome_result(windows::windows_clipboard(&arguments)),
            "advisor" => outcome_result(advisor::advisor(&self.advisor, &arguments)),
            "config_edit" => outcome_result(config_tool::config_edit(&self.config_env, &arguments)),
            _ => return None,
        };
        Some(ToolExecution {
            result,
            ..ToolExecution::default()
        })
    }

    fn call_mcp_tool(&self, name: &str, arguments: &Map<String, Value>) -> ToolExecution {
        let Some(manager) = self.mcp.as_ref() else {
            return ToolExecution::default();
        };
        let result = manager.call_tool(name, arguments);
        ToolExecution {
            result: from_controllers(mcp_tool_result(mcp_fields(
                result.ok,
                &result.server_name,
                &result.tool_name,
                &result.audit_id,
                result.duration_ms,
                result.error_code.as_deref().unwrap_or_default(),
                result.retryable,
                &result.output,
                &result.full_output,
            ))),
            ..ToolExecution::default()
        }
    }

    fn read_mcp_resource(&self, logical_uri: &str) -> ToolExecution {
        let Some(manager) = self.mcp.as_ref() else {
            return ToolExecution::default();
        };
        let result = manager.read_resource(logical_uri);
        ToolExecution {
            result: from_controllers(mcp_resource_result(mcp_fields(
                result.ok,
                &result.server_name,
                &result.uri,
                "",
                result.duration_ms,
                result.error_code.as_deref().unwrap_or_default(),
                result.retryable,
                &result.output,
                &result.full_output,
            ))),
            ..ToolExecution::default()
        }
    }

    fn get_mcp_prompt(&self, logical_name: &str, arguments: &Map<String, Value>) -> ToolExecution {
        let Some(manager) = self.mcp.as_ref() else {
            return ToolExecution::default();
        };
        // 与 Python 一致：`arguments` 缺失按空对象处理，给了非对象直接拒绝。
        let prompt_arguments = match arguments.get("arguments") {
            None => Some(Map::new()),
            Some(Value::Object(map)) => Some(map.clone()),
            Some(_) => None,
        };
        let Some(prompt_arguments) = prompt_arguments else {
            return ToolExecution {
                result: from_controllers(mcp_prompt_arguments_error()),
                ..ToolExecution::default()
            };
        };
        let result = manager.get_prompt(logical_name, Some(&prompt_arguments));
        ToolExecution {
            result: from_controllers(mcp_prompt_result(mcp_fields(
                result.ok,
                &result.server_name,
                &result.prompt_name,
                "",
                result.duration_ms,
                result.error_code.as_deref().unwrap_or_default(),
                result.retryable,
                &result.output,
                &result.full_output,
            ))),
            ..ToolExecution::default()
        }
    }

    fn run_command(&self, arguments: &Map<String, Value>, shell: Shell) -> ToolResult {
        match self.commands.run_shell(arguments, shell, &self.cancel) {
            Ok(outcome) => command_result(outcome.ok, outcome.output),
            Err(error) => text_failure(&error),
        }
    }

    fn run_monitor(&self, arguments: &Map<String, Value>) -> ToolResult {
        match self.monitors.run(arguments) {
            Ok(outcome) => command_result(outcome.ok, outcome.output),
            Err(error) => text_failure(&error),
        }
    }
}

fn outcome_result(outcome: super::error::ToolOutcome) -> ToolResult {
    match outcome {
        Ok(output) => text_success(output),
        Err(error) => text_failure(&error),
    }
}

/// 控制器域的结果类型 → 内核工具结果（去掉 UIKit 侧字段）。
fn from_controllers(result: omnicrawl_controllers::types::ToolResult) -> ToolResult {
    ToolResult {
        ok: result.ok,
        output: result.output,
        full_output: result.full_output,
        error_code: result.error_code,
        retryable: result.retryable,
    }
}

/// 声明里的工具名集合（供测试与诊断）。
pub fn declared_names(registry: &ToolRegistry) -> BTreeSet<String> {
    registry
        .specs()
        .iter()
        .map(|spec| spec.name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::web_transport::{WebError, WebRequest, WebResponse, WebTransport};
    use serde_json::json;
    use std::sync::Arc;

    fn registry(name: &str) -> (ToolRegistry, PathBuf) {
        let root = std::env::temp_dir().join(format!("omnicrawl-tui-registry-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        let registry =
            ToolRegistry::new(&root, &RegistryOptions::default(), 360).expect("工具表应当构建成功");
        (registry, root)
    }

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            name: name.to_string(),
            arguments: arguments.as_object().cloned().unwrap_or_default(),
            id: "c1".to_string(),
            function_name: name.to_string(),
        }
    }

    /// 关掉的内置工具必须真的不进声明表：模型不可见才不可调用。
    ///
    /// 回归的是「设置里关掉了、启动后仍然可调」：`RegistryOptions::disabled_tools`
    /// 若在装配时漏填，`build_agent_tools` 的过滤就退化成不过滤（空表即全放行）。
    #[test]
    fn disabled_tools_stay_out_of_the_table() {
        let root = std::env::temp_dir().join("omnicrawl-tui-registry-disabled");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        let options = RegistryOptions {
            disabled_tools: vec!["powershell".to_string(), "web_search".to_string()],
            ..RegistryOptions::default()
        };
        let registry = ToolRegistry::new(&root, &options, 360).expect("工具表应当构建成功");
        let names = declared_names(&registry);
        assert!(
            !names.contains("powershell"),
            "关掉的工具不得进表：{names:?}"
        );
        assert!(
            !names.contains("web_search"),
            "关掉的工具不得进表：{names:?}"
        );
        assert!(names.contains("read"), "其余工具照旧进表：{names:?}");
    }

    #[test]
    fn table_declares_only_tools_this_host_can_run() {
        let (registry, _root) = registry("table");
        let names = declared_names(&registry);
        for expected in [
            "read",
            "write_file",
            "Edit_file",
            "bash",
            "powershell",
            "list",
            "find",
            "grep",
            "skill_search",
            "git",
            "monitor",
            "kb_search",
            "kb_read",
            "kb_write",
            "kb_append",
            "kb_list",
            "web_search",
            "fetcher",
            "image_gen",
            "windows_window",
            "windows_control",
            "windows_input",
            "windows_clipboard",
            "windows_screenshot",
            // 视觉工具始终进表（与 Python `_build_tools` 一致），图片去向由内核路由。
            "read_image",
            "config_edit",
        ] {
            assert!(names.contains(expected), "缺少 {expected}：{names:?}");
        }
        for hidden in ["subagent", "memory_search"] {
            assert!(!names.contains(hidden), "{hidden} 不该出现：{names:?}");
        }
        // 内核自持的三个工具仍在表里（界面负责交互）。
        for meta in META_RUNNERS {
            assert!(names.contains(meta), "缺少 {meta}");
        }
    }

    /// `config_edit` 真的经工具表读配置：读出来的是注入环境指到的文件，不是进程家目录。
    ///
    /// 顺带钉住两点：写操作在声明里标了需要确认；`RegistryOptions.config_env` 一旦漏接，
    /// 工具就会去读真实用户配置，这个用例会立刻发现。
    #[test]
    fn config_edit_reads_and_writes_through_the_registry() {
        let root = std::env::temp_dir().join("omnicrawl-config-edit-registry");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".OmniCrawl")).expect("建配置目录");
        std::fs::write(
            root.join(".OmniCrawl").join("config.toml"),
            "[approval]
mode = \"review\"
",
        )
        .expect("写测试配置");
        let options = RegistryOptions {
            config_env: Some(omnicrawl_config::core::runtime::ConfigEnvironment::new(
                &root, "windows",
            )),
            ..RegistryOptions::default()
        };
        let registry = ToolRegistry::new(&root, &options, 360).expect("工具表应当构建成功");
        let names = declared_names(&registry);
        assert!(names.contains("config_edit"), "配置工具应当进表：{names:?}");
        let spec = registry
            .specs()
            .iter()
            .find(|spec| spec.name == "config_edit")
            .expect("config_edit 声明");
        assert!(spec.requires_confirmation, "写配置必须走确认/审查");

        let read = registry
            .execute(&call("config_edit", json!({"action": "read"})))
            .expect("宿主认识这个工具");
        assert!(read.ok, "{}", read.output);
        assert!(read.output.contains("mode = \"review\""), "{}", read.output);

        let written = registry
            .execute(&call(
                "config_edit",
                json!({"action": "set", "path": "memory.enabled", "value": true}),
            ))
            .expect("宿主认识这个工具");
        assert!(written.ok, "{}", written.output);
        let text =
            std::fs::read_to_string(root.join(".OmniCrawl").join("config.toml")).expect("读回配置");
        assert!(
            text.contains("[memory]") && text.contains("enabled = true"),
            "新段应当写进去：{text}"
        );
        assert!(
            text.contains("mode = \"review\""),
            "其余配置段必须保留：{text}"
        );

        // 显式 file 落到对应文件，且不碰主配置。
        let models = registry
            .execute(&call(
                "config_edit",
                json!({"action": "set", "file": "models", "path": "llm.defaults.context_window_tokens", "value": 200000}),
            ))
            .expect("宿主认识这个工具");
        assert!(models.ok, "{}", models.output);
        let models_text = std::fs::read_to_string(root.join(".OmniCrawl").join("models.toml"))
            .expect("读回模型配置");
        assert!(
            models_text.contains("context_window_tokens = 200000"),
            "{models_text}"
        );
        let config_text =
            std::fs::read_to_string(root.join(".OmniCrawl").join("config.toml")).expect("读回配置");
        assert!(
            !config_text.contains("context_window_tokens"),
            "写 models.toml 不得污染 config.toml：{config_text}"
        );
    }

    #[test]
    fn declarations_carry_real_parameter_schemas() {
        let (registry, _root) = registry("declarations");
        let declarations = registry.declarations();
        let read = declarations
            .iter()
            .find(|item| item["function"]["name"] == "read")
            .expect("read 声明");
        assert_eq!(read["type"], "function");
        assert_eq!(read["function"]["parameters"]["type"], "object");
        assert_eq!(
            read["function"]["parameters"]["properties"]["path"]["type"],
            "string"
        );
        assert_eq!(read["function"]["parameters"]["minProperties"], 1);
        let description = read["function"]["description"].as_str().unwrap_or_default();
        assert!(!description.contains('\n'), "描述应当折叠空白");

        let bash = declarations
            .iter()
            .find(|item| item["function"]["name"] == "bash")
            .expect("bash 声明");
        assert_eq!(bash["function"]["parameters"]["required"][0], "command");
    }

    #[test]
    fn read_executes_through_the_registry() {
        let (registry, root) = registry("read");
        std::fs::write(root.join("a.txt"), "内容\n").expect("写测试文件");
        let result = registry
            .execute(&call("read", json!({"path": "a.txt"})))
            .expect("read 应当被本宿主处理");
        assert!(result.ok, "{}", result.output);
        assert_eq!(result.output, "1: 内容\n(End of file - total 1 lines)");
    }

    #[test]
    fn argument_aliases_are_normalized_before_execution() {
        let (registry, root) = registry("aliases");
        std::fs::write(root.join("a.txt"), "x\n").expect("写测试文件");
        // maxLines / startLine 是工具表声明的别名。
        let result = registry
            .execute(&call(
                "read",
                json!({"path": "a.txt", "startLine": 1, "maxLines": 5}),
            ))
            .expect("别名参数应当被归一化");
        assert!(result.ok, "{}", result.output);
        assert!(result.output.starts_with("1: x"));
    }

    #[test]
    fn invalid_arguments_return_structured_error() {
        let (registry, _root) = registry("invalid");
        let result = registry
            .execute(&call("bash", json!({"timeout_seconds": 9999})))
            .expect("bash 应当被本宿主处理");
        assert!(!result.ok);
        // 结构化错误信封把码与可重试标记写进 JSON 载荷（与 Python `_error_result` 一致）。
        assert!(
            result.output.contains("\"code\": \"invalid_arguments\""),
            "{}",
            result.output
        );
        assert!(
            result.output.contains("\"retryable\": true"),
            "{}",
            result.output
        );
        assert!(result.output.contains("command"), "{}", result.output);
    }

    #[test]
    fn unknown_tools_are_left_to_the_caller() {
        let (registry, _root) = registry("unknown");
        assert!(registry
            .execute(&call("advisor", json!({"question": "?"})))
            .is_none());
        assert!(registry.execute(&call("update_todos", json!({}))).is_none());
        assert!(registry.execute(&call("no_such_tool", json!({}))).is_none());
    }

    #[test]
    fn monitor_starts_and_reports_through_the_registry() {
        let (registry, _root) = registry("monitor");
        let result = registry
            .execute(&call("monitor", json!({"action": "list"})))
            .expect("monitor 应当被本宿主处理");
        assert!(result.ok, "{}", result.output);
        assert_eq!(result.output, "当前没有后台任务。");

        let empty = registry
            .execute(&call(
                "monitor",
                json!({"action": "start", "command": "  "}),
            ))
            .expect("monitor 应当被本宿主处理");
        assert!(!empty.ok);
        assert_eq!(empty.output, "启动监控时 command 不能为空。");
    }

    #[test]
    fn edit_executes_and_reports_through_the_registry() {
        let (registry, root) = registry("edit");
        std::fs::write(root.join("a.txt"), "old\n").expect("写测试文件");
        let result = registry
            .execute(&call(
                "Edit_file",
                json!({"path": "a.txt", "old_text": "old", "new_text": "new"}),
            ))
            .expect("Edit_file 应当被本宿主处理");
        assert!(result.ok, "{}", result.output);
        assert!(
            result.output.starts_with("已修改 a.txt，替换 1 处。"),
            "{}",
            result.output
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).expect("读回"),
            "new\n"
        );
    }

    /// `computer_use` 只受自己的开关控制：普通子代理关闭时它照样进表，且不会把
    /// `subagent` 一起带出来（注册条件看的是角色清单，不是总开关）。
    #[test]
    fn computer_use_registers_independently_from_subagents() {
        let root = std::env::temp_dir().join("omnicrawl-tui-registry-computer-use");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");

        // 桌面开、子代理关：只有 computer_use，且不出现 subagent。
        let options = RegistryOptions {
            computer_use: true,
            subagent_roles: Vec::new(),
            ..RegistryOptions::default()
        };
        let registry = ToolRegistry::new(&root, &options, 360).expect("工具表应当构建成功");
        let names = declared_names(&registry);
        assert!(names.contains("computer_use"), "{names:?}");
        assert!(!names.contains("subagent"), "{names:?}");

        // 桌面关：不进表。
        let options = RegistryOptions {
            computer_use: false,
            ..RegistryOptions::default()
        };
        let registry = ToolRegistry::new(&root, &options, 360).expect("工具表应当构建成功");
        assert!(!declared_names(&registry).contains("computer_use"));
    }

    #[test]
    fn tts_tool_registers_only_when_enabled() {
        let (disabled, _root) = registry("tts-disabled");
        assert!(!declared_names(&disabled).contains("tts_synthesize"));

        let root = std::env::temp_dir().join("omnicrawl-tui-registry-tts");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        let options = RegistryOptions {
            tts: Some(Arc::new(TtsOptions::new(
                omnicrawl_config::features::tts::TtsConfiguration {
                    enabled: true,
                    auto_play: false,
                    ..Default::default()
                },
                crate::tools::tts::tts_backend(
                    &omnicrawl_config::features::tts_cloud::TtsCloudConfiguration::default(),
                    omnicrawl_config::features::tts_cloud::TtsApiConfiguration::default(),
                    omnicrawl_config::features::tts_cloud::GeminiTtsConfiguration::default(),
                ),
                root.clone(),
            ))),
            ..RegistryOptions::default()
        };
        let registry = ToolRegistry::new(&root, &options, 360).expect("工具表应当构建成功");
        assert!(declared_names(&registry).contains("tts_synthesize"));

        let result = registry
            .execute(&call("tts_synthesize", json!({"text": "   "})))
            .expect("tts_synthesize 应当被本宿主处理");
        assert!(!result.ok);
        assert_eq!(result.output, r#"{"ok":false,"error":"text 不能为空。"}"#);
    }

    #[test]
    fn knowledge_tools_run_through_the_registry() {
        let root = std::env::temp_dir().join("omnicrawl-tui-registry-knowledge");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        let knowledge_root = root.join("kb");
        let options = RegistryOptions {
            knowledge_root: Some(knowledge_root.clone()),
            ..RegistryOptions::default()
        };
        let registry = ToolRegistry::new(&root, &options, 360).expect("工具表应当构建成功");

        let written = registry
            .execute(&call(
                "kb_write",
                json!({
                    "path": "projects/demo/note",
                    "content": "正文",
                    "title": "示例",
                    "mode": "create",
                }),
            ))
            .expect("kb_write 应当被本宿主处理");
        assert!(written.ok, "{}", written.output);
        assert!(knowledge_root.join("projects/demo/note.md").is_file());
        assert!(knowledge_root.join("INDEX.md").is_file());

        let listed = registry
            .execute(&call("kb_list", json!({})))
            .expect("kb_list 应当被本宿主处理");
        assert!(listed.ok, "{}", listed.output);
        assert!(
            listed
                .output
                .contains("\"path\": \"projects/demo/note.md\""),
            "{}",
            listed.output
        );

        let searched = registry
            .execute(&call("kb_search", json!({"query": "正文"})))
            .expect("kb_search 应当被本宿主处理");
        assert!(searched.ok, "{}", searched.output);
        assert!(
            searched.output.contains("\"score\":"),
            "{}",
            searched.output
        );
    }

    /// `skill_search` 经工具表可取到工作区内的 Skill（默认不开重排）。
    #[test]
    fn skill_search_runs_through_the_registry() {
        let root = std::env::temp_dir().join("omnicrawl-tui-registry-skill");
        let _ = std::fs::remove_dir_all(&root);
        let skill_dir = root.join(".omnicrawl").join("skills").join("demo");
        std::fs::create_dir_all(&skill_dir).expect("建 skill 目录");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: demo\ndescription: 演示用 Skill\n---\n\n正文\n",
        )
        .expect("写 SKILL.md");

        let registry =
            ToolRegistry::new(&root, &RegistryOptions::default(), 360).expect("工具表应当构建成功");
        let found = registry
            .execute(&call(
                "skill_search",
                json!({"scope": "project", "query": "demo"}),
            ))
            .expect("skill_search 应当被本宿主处理");
        assert!(found.ok, "{}", found.output);
        assert!(
            found.output.contains("\"name\": \"demo\""),
            "{}",
            found.output
        );
        assert!(
            found.output.contains("\"scope\": \"project\""),
            "{}",
            found.output
        );
    }

    #[test]
    fn memory_tools_register_and_run_when_enabled() {
        let root = std::env::temp_dir().join("omnicrawl-tui-registry-memory");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        let options = RegistryOptions {
            memory_enabled: true,
            memory: MemoryOptions {
                project_directory: ".omnicrawl/.oclmemory".to_string(),
                project_enabled: true,
                ..MemoryOptions::default()
            },
            ..RegistryOptions::default()
        };
        let registry = ToolRegistry::new(&root, &options, 360).expect("工具表应当构建成功");
        let names = declared_names(&registry);
        for expected in MEMORY_RUNNERS {
            assert!(names.contains(expected), "缺少 {expected}：{names:?}");
        }

        let written = registry
            .execute(&call(
                "memory_write",
                json!({
                    "memories": [{
                        "content": "内核工具已经用 Rust 化。",
                        "related_directories": ["rust/crates"],
                    }],
                }),
            ))
            .expect("memory_write 应当被本宿主处理");
        assert!(written.ok, "{}", written.output);

        let searched = registry
            .execute(&call(
                "memory_search",
                json!({"query": "Rust 化", "reason": "确认写入生效"}),
            ))
            .expect("memory_search 应当被本宿主处理");
        assert!(searched.ok, "{}", searched.output);
        assert!(searched.output.contains("\"id\":"), "{}", searched.output);

        // 删除走同一条分发：先取 id 再删，删完检索不到。
        let found: serde_json::Value = serde_json::from_str(&searched.output).expect("结果是 JSON");
        let memory_id = found[0]["id"].as_str().expect("有 id").to_string();
        let deleted = registry
            .execute(&call("memory_delete", json!({"memory_ids": [memory_id]})))
            .expect("memory_delete 应当被本宿主处理");
        assert!(deleted.ok, "{}", deleted.output);
        assert!(deleted.output.contains("\"deleted_count\": 1"), "{}", deleted.output);
    }

    /// 联网工具的传输桩：搜索端点返回结果页，其余返回普通页面。
    struct StubTransport;

    impl WebTransport for StubTransport {
        fn send(&self, request: &WebRequest) -> Result<WebResponse, WebError> {
            let body = if request.url.contains("duckduckgo.com") {
                r#"<a class="result__a" href="https://example.com/1">标题一</a>
<a class="result__snippet">摘要一</a>"#
            } else if request.url.contains("/images/generations") {
                r#"{"data": [{"b64_json": "aGVsbG8=", "output_format": "png"}]}"#
            } else {
                "<html><head><title>示例页</title></head><body><main><p>正文内容</p></main></body></html>"
            };
            Ok(WebResponse {
                status: 200,
                final_url: request.url.clone(),
                body: body.as_bytes().to_vec(),
                headers: Vec::new(),
            })
        }
    }

    #[test]
    fn network_tools_run_through_the_registry() {
        let root = std::env::temp_dir().join("omnicrawl-tui-registry-network");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        let transport = Arc::new(StubTransport);
        let options = RegistryOptions {
            web_search: WebSearchOptions {
                transport: transport.clone(),
                // 显式禁用代理：测试不读系统代理、不起网络。
                proxy: Some(String::new()),
                ..WebSearchOptions::default()
            },
            fetcher: FetcherOptions {
                transport,
                ..FetcherOptions::default()
            },
            ..RegistryOptions::default()
        };
        let registry = ToolRegistry::new(&root, &options, 360).expect("工具表应当构建成功");

        let searched = registry
            .execute(&call("web_search", json!({"query": "rust 工具"})))
            .expect("web_search 应当被本宿主处理");
        assert!(searched.ok, "{}", searched.output);
        assert!(
            searched.output.contains("来源：duckduckgo"),
            "{}",
            searched.output
        );
        assert!(searched.output.contains("标题一"), "{}", searched.output);

        // 内网地址直连，不触发系统代理检测。
        let fetched = registry
            .execute(&call(
                "fetcher",
                json!({"urls": "http://127.0.0.1/page", "parallel": false}),
            ))
            .expect("fetcher 应当被本宿主处理");
        assert!(fetched.ok, "{}", fetched.output);
        assert!(
            fetched.output.contains("网页抓取完成（1 个 URL"),
            "{}",
            fetched.output
        );
        assert!(
            fetched.output.contains("标题: 示例页"),
            "{}",
            fetched.output
        );
        assert!(
            fetched.output.contains("内容: 正文内容"),
            "{}",
            fetched.output
        );
    }

    #[test]
    fn read_image_returns_a_vision_attachment_through_the_registry() {
        let root = std::env::temp_dir().join("omnicrawl-tui-registry-image");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        std::fs::write(root.join("pic.png"), b"\x89PNG\r\n\x1a\n0000").expect("写测试图片");
        let registry =
            ToolRegistry::new(&root, &RegistryOptions::default(), 360).expect("工具表应当构建成功");
        // 图片的去向不由工具表决定：默认表里也有 read_image（与 Python `_build_tools` 一致），
        // 未开原生视觉时由 `[vision]` 代理接手，两者都没有时模型只收到图片元数据。
        let default_registry =
            ToolRegistry::new(&root, &RegistryOptions::default(), 360).expect("工具表应当构建成功");
        assert!(declared_names(&default_registry).contains("read_image"));

        let execution = registry
            .execute_with_vision(&call(
                "read_image",
                json!({"path": "pic.png", "prompt": "描述这张图"}),
            ))
            .expect("read_image 应当被本宿主处理");
        assert!(execution.result.ok, "{}", execution.result.output);
        assert!(
            execution
                .result
                .output
                .contains("\"vision_attachment\":true"),
            "{}",
            execution.result.output
        );
        assert_eq!(execution.images.len(), 1);
        assert_eq!(execution.images[0].media_type, "image/png");
        assert_eq!(execution.images[0].filename, "pic.png");
        assert_eq!(execution.vision_prompt, "描述这张图");

        // 纯文本执行体的附件恒为空。
        let plain = registry
            .execute_with_vision(&call("read", json!({"path": "pic.png"})))
            .expect("read 应当被本宿主处理");
        assert!(plain.images.is_empty());
        assert!(plain.vision_prompt.is_empty());
    }

    /// 桌面操作工具的确认截图必须走宿主同一条视觉附件通路，否则模型仍然看不到画面。
    #[cfg(windows)]
    #[test]
    fn windows_input_returns_a_snapshot_through_the_registry() {
        let (registry, root) = registry("windows-snapshot");
        // release 未按下时幂等：不产生任何输入事件，却能覆盖「操作后截图」这条通路。
        let execution = registry
            .execute_with_vision(&call("windows_input", json!({"action": "release"})))
            .expect("windows_input 应当被本宿主处理");
        assert!(execution.result.ok, "{}", execution.result.output);
        assert!(
            execution.result.output.contains("\"screenshot\""),
            "结果里要有截图的路径与区域：{}",
            execution.result.output
        );
        assert_eq!(execution.images.len(), 1, "确认截图应当作为视觉附件交出去");
        assert_eq!(execution.images[0].media_type, "image/png");
        assert!(execution.images[0].filename.ends_with(".png"));
        assert!(
            execution.vision_prompt.contains("windows_screenshot"),
            "提示词要拦掉多余的补截：{}",
            execution.vision_prompt
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn image_gen_writes_files_through_the_registry() {
        let root = std::env::temp_dir().join("omnicrawl-tui-registry-image-gen");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        let options = RegistryOptions {
            image_gen: ImageGenOptions {
                transport: Arc::new(StubTransport),
                enabled: true,
                api_key: "test-key".to_string(),
                ..ImageGenOptions::default()
            },
            ..RegistryOptions::default()
        };
        let registry = ToolRegistry::new(&root, &options, 360).expect("工具表应当构建成功");

        let result = registry
            .execute(&call("image_gen", json!({"prompt": "一只猫"})))
            .expect("image_gen 应当被本宿主处理");
        assert!(result.ok, "{}", result.output);
        assert!(
            result
                .output
                .starts_with("已生成 1 张图片（模型 gpt-image-2）："),
            "{}",
            result.output
        );

        let images = root.join(".omnicrawl").join(".agent_tmp").join("images");
        let entries: Vec<std::fs::DirEntry> = std::fs::read_dir(&images)
            .expect("默认图片目录应当存在")
            .flatten()
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            std::fs::read(entries[0].path()).expect("读生成的图片"),
            b"hello"
        );

        // 未启用时同一个工具给出明确错误（与 Python 一致：工具始终进表）。
        let disabled =
            ToolRegistry::new(&root, &RegistryOptions::default(), 360).expect("工具表应当构建成功");
        let result = disabled
            .execute(&call("image_gen", json!({"prompt": "一只猫"})))
            .expect("image_gen 应当被本宿主处理");
        assert!(!result.ok);
        assert!(
            result.output.starts_with("图像生成未启用："),
            "{}",
            result.output
        );
    }

    /// 脚本化 MCP 连接：验证能力进表与三类执行体的分发（不依赖真实子进程）。
    struct FakeConnection;

    impl omnicrawl_mcp::client::McpConnection for FakeConnection {
        fn discover(
            &self,
        ) -> Result<
            omnicrawl_mcp::client::DiscoveredCapabilities,
            omnicrawl_mcp::jsonrpc::McpCallError,
        > {
            let maps = |value: Value| value.as_object().cloned().unwrap_or_default();
            Ok(omnicrawl_mcp::client::DiscoveredCapabilities {
                tools: vec![maps(json!({
                    "name": "read",
                    "description": "读一个文件",
                    "inputSchema": {"type": "object", "required": ["path"]},
                }))],
                resources: vec![maps(json!({"uri": "file:///a", "name": "a"}))],
                prompts: vec![maps(json!({"name": "review", "description": "审查"}))],
            })
        }

        fn call_tool(
            &self,
            _name: &str,
            _arguments: &Map<String, Value>,
        ) -> Result<Map<String, Value>, omnicrawl_mcp::jsonrpc::McpCallError> {
            Ok(json!({"content": [{"type": "text", "text": "内容"}]})
                .as_object()
                .cloned()
                .unwrap_or_default())
        }

        fn read_resource(
            &self,
            uri: &str,
        ) -> Result<Map<String, Value>, omnicrawl_mcp::jsonrpc::McpCallError> {
            Ok(json!({"contents": [{"uri": uri, "text": "正文"}]})
                .as_object()
                .cloned()
                .unwrap_or_default())
        }

        fn get_prompt(
            &self,
            name: &str,
            _arguments: Option<&Map<String, Value>>,
        ) -> Result<Map<String, Value>, omnicrawl_mcp::jsonrpc::McpCallError> {
            Ok(json!({"messages": [{"role": "user", "content": {"type": "text", "text": format!("{name} 模板")}}]})
                .as_object()
                .cloned()
                .unwrap_or_default())
        }

        fn close(&self) {}
    }

    fn mcp_registry(name: &str) -> (ToolRegistry, PathBuf) {
        use omnicrawl_mcp::client::McpConnection;
        use omnicrawl_mcp::config::{McpConfig, McpPolicyConfig, McpServerConfig};

        let root = std::env::temp_dir().join(format!("omnicrawl-tui-mcp-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        let mut server = McpServerConfig::new("files", "stdio");
        server.command = Some("npx".to_string());
        let manager = Arc::new(
            McpClientManager::new(
                McpConfig {
                    enabled: true,
                    default_timeout_seconds: 30,
                    servers: vec![(server.name.clone(), server)],
                    policy: McpPolicyConfig::default(),
                },
                &root,
            )
            .with_connector(Arc::new(|_server, _root| {
                Ok(Arc::new(FakeConnection) as Arc<dyn McpConnection>)
            })),
        );
        manager.discover();
        let options = RegistryOptions {
            mcp: Some(manager),
            ..RegistryOptions::default()
        };
        let registry = ToolRegistry::new(&root, &options, 360).expect("工具表应当构建成功");
        (registry, root)
    }

    #[test]
    fn mcp_capabilities_enter_the_table_and_run_through_the_manager() {
        let (registry, root) = mcp_registry("wiring");
        let names = declared_names(&registry);
        for expected in [
            "files.read",
            "mcp_read_resource__files:file:///a",
            "mcp_get_prompt__files.review",
        ] {
            assert!(names.contains(expected), "缺少 {expected}：{names:?}");
        }

        let tool = registry
            .execute(&call("files.read", json!({"path": "/tmp/a"})))
            .expect("MCP 工具应当由本宿主执行");
        assert!(tool.ok, "{}", tool.output);
        assert!(
            tool.output.contains("MCP Tool：files.read"),
            "{}",
            tool.output
        );

        let resource = registry
            .execute(&call("mcp_read_resource__files:file:///a", json!({})))
            .expect("MCP 资源读取器应当由本宿主执行");
        assert!(resource.ok, "{}", resource.output);
        assert!(resource.output.contains("正文"), "{}", resource.output);

        let prompt = registry
            .execute(&call(
                "mcp_get_prompt__files.review",
                json!({"arguments": {"path": "a"}}),
            ))
            .expect("MCP Prompt 读取器应当由本宿主执行");
        assert!(prompt.ok, "{}", prompt.output);
        assert!(prompt.output.contains("review 模板"), "{}", prompt.output);

        // 参数不是对象时按 Python 的文案拒绝，不落到管理器。
        let invalid = registry
            .execute(&call(
                "mcp_get_prompt__files.review",
                json!({"arguments": 7}),
            ))
            .expect("MCP Prompt 读取器应当由本宿主执行");
        assert!(
            invalid.output.contains("arguments 必须是 JSON 对象。"),
            "{}",
            invalid.output
        );

        registry.close_mcp();
        let _ = std::fs::remove_dir_all(root);
    }
}
