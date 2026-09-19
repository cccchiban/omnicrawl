//! 工具注册表：工具表、名称/参数归一化、Schema 校验与执行分发。
//!
//! 工具表来自内核已搬好的 `omnicrawl-controllers::tool_catalog`（数据由
//! `omnicrawl/agent/toolkit/tools.py` 导出），归一化与校验来自同 crate 的
//! `tool_args`，本模块只负责「这张表里哪些工具这个宿主真的能执行」。

use std::collections::BTreeSet;
use std::path::PathBuf;

use omnicrawl_controllers::tool_args::{
    normalize_tool_call, tool_validation_error_result, validate_tool_arguments,
};
use omnicrawl_controllers::tool_catalog::{build_agent_tools, ToolCatalogOptions, ToolSpec};
use omnicrawl_controllers::types::ToolImageAttachment;
use omnicrawl_core::{ToolCall, ToolResult};
use serde_json::{Map, Value};

use super::advisor::{self, AdvisorOptions};
use super::command::{CancelToken, CommandRunner, Shell, DEFAULT_COMMAND_TIMEOUT_SECONDS};
use super::declarations::declaration;
use super::error::{command_result, text_failure, text_success, ToolError};
use super::fetcher::{self, FetcherOptions};
use super::image_gen::{self, ImageGenOptions};
use super::knowledge::{self, KnowledgeBase};
use super::memory::{self, MemoryOptions};
use super::monitor::MonitorManager;
use super::paths::WorkspacePaths;
use super::web_search::{self, WebSearchOptions};
use super::windows;
use super::{edit, finding, git, grep, listing, read, read_image, write};

/// 本宿主已实现执行体的工具。
pub const IMPLEMENTED_TOOLS: [&str; 29] = [
    "read",
    "read_image",
    "write_file",
    "Edit_file",
    "bash",
    "powershell",
    "list",
    "find",
    "grep",
    "git",
    "monitor",
    "kb_search",
    "kb_read",
    "kb_write",
    "kb_append",
    "kb_list",
    "memory_search",
    "memory_read",
    "memory_expand_related",
    "memory_write",
    "web_search",
    "fetcher",
    "image_gen",
    "windows_window",
    "windows_control",
    "windows_input",
    "windows_clipboard",
    "windows_screenshot",
    "advisor",
];
/// 内核自持的工具：声明进表，执行体在本宿主（`update_todos` / `ask_user` / `pause_work`）。
pub const META_RUNNERS: [&str; 3] = ["update_todos", "ask_user", "pause_work"];
/// 工具表里按 runner 绑定进表的工具：本宿主已绑定这三个。
pub const BOUND_RUNNERS: [&str; 3] = ["find", "git", "monitor"];
/// 知识库工具：runner 名即工具名，整组注册或整组不注册。
pub const KNOWLEDGE_RUNNERS: [&str; 5] =
    ["kb_search", "kb_read", "kb_write", "kb_append", "kb_list"];
/// 记忆工具：整组由 `memory_enabled` 控制。
pub const MEMORY_RUNNERS: [&str; 4] = [
    "memory_search",
    "memory_read",
    "memory_expand_related",
    "memory_write",
];
/// 联网工具：runner 名即工具名，整组不依赖开关。
pub const NET_RUNNERS: [&str; 2] = ["web_search", "fetcher"];
/// 视觉工具：`read_image` 除文本结果外还要把图片作为视觉附件交给循环。
pub const VISION_RUNNERS: [&str; 1] = ["read_image"];
/// 图像生成：与 Python 一致，工具始终进表；未启用时调用会给出明确错误。
pub const IMAGE_RUNNERS: [&str; 1] = ["image_gen"];
/// 顾问工具：只有显式启用并选中顾问模型时才进表（与 Python 的 AdvisorConfig 一致）。
pub const ADVISOR_RUNNER: &str = "advisor";
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
    /// SubAgent 角色枚举（未启用时不给这个工具）。
    pub subagent_types: Vec<String>,
    /// 会话由内核自持时，`recall_session_evidence` 由内核本地作答。
    pub session_held_by_kernel: bool,
    /// 知识库根目录；未配置时用 `~/.OmniCrawl/knowledge`。
    pub knowledge_root: Option<PathBuf>,
    /// 模型原生支持视觉：为真时才会把 `read_image` 注册进工具表（否则图片无处可用）。
    pub native_vision: bool,
    /// 联网工具的运行期配置（传输可注入，测试用桩替换）。
    pub web_search: WebSearchOptions,
    pub fetcher: FetcherOptions,
    /// 图像生成配置（传输可注入；未启用时工具仍在表里，调用时报错）。
    pub image_gen: ImageGenOptions,
    /// 顾问运行期配置（未启用时 advisor 不进表）。
    pub advisor: AdvisorOptions,
}

pub struct ToolRegistry {
    paths: WorkspacePaths,
    commands: CommandRunner,
    monitors: MonitorManager,
    knowledge: KnowledgeBase,
    memory: MemoryOptions,
    web_search: WebSearchOptions,
    fetcher: FetcherOptions,
    image_gen: ImageGenOptions,
    advisor: AdvisorOptions,
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
            .chain(IMAGE_RUNNERS.iter())
            .chain(WINDOWS_RUNNERS.iter())
            .map(|name| (*name).to_string())
            .collect();
        if options.advisor.active() {
            available.push(ADVISOR_RUNNER.to_string());
        }
        // 视觉工具只在模型能看图时进表：否则客户端会读出图片却无处可送。
        if options.native_vision {
            available.extend(VISION_RUNNERS.iter().map(|name| (*name).to_string()));
        }
        if options.session_held_by_kernel {
            available.push("evidence_recall".to_string());
        }
        // `subagent` 由内核自持执行（子回合需要模型运行时），宿主只把它声明给模型；
        // 角色为空时不进表，避免模型看到一个必然失败的入口。
        if !options.subagent_types.is_empty() {
            available.push("subagent".to_string());
        }
        let available_refs: Vec<&str> = available.iter().map(String::as_str).collect();
        let catalog_options = ToolCatalogOptions {
            available: &available_refs,
            memory_enabled: options.memory_enabled,
            subagent_types: &options.subagent_types,
            mcp_tools: &[],
            mcp_resources: &[],
            mcp_prompts: &[],
            disabled_tools: &[],
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
        let monitors = MonitorManager::new(paths.root());
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
            memory,
            web_search: options.web_search.clone(),
            fetcher: options.fetcher.clone(),
            image_gen,
            advisor,
            specs,
            cancel: CancelToken::new(),
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

    pub fn is_implemented(&self, name: &str) -> bool {
        IMPLEMENTED_TOOLS.contains(&name)
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
        if name == "windows_screenshot" {
            return Some(match windows::windows_screenshot(&self.paths, &arguments) {
                Ok(outcome) => ToolExecution {
                    result: text_success(outcome.output),
                    images: outcome.images,
                    vision_prompt: String::new(),
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
        let result = match name.as_str() {
            "read" => outcome_result(read::read(&self.paths, &arguments)),
            "write_file" => outcome_result(write::write_file(&self.paths, &arguments)),
            "Edit_file" => outcome_result(edit::edit_file(&self.paths, &arguments)),
            "bash" => self.run_command(&arguments, Shell::Bash),
            "powershell" => self.run_command(&arguments, Shell::PowerShell),
            "list" => outcome_result(listing::list_files(&self.paths, &arguments)),
            "find" => outcome_result(finding::find_files(&self.paths, &arguments)),
            "grep" => outcome_result(grep::grep(&self.paths, &arguments)),
            "git" => git::git_tool(&self.paths, &arguments),
            "monitor" => self.run_monitor(&arguments),
            "kb_search" => outcome_result(knowledge::kb_search(&self.knowledge, &arguments)),
            "kb_read" => outcome_result(knowledge::kb_read(&self.knowledge, &arguments)),
            "kb_write" => outcome_result(knowledge::kb_write(&self.knowledge, &arguments)),
            "kb_append" => outcome_result(knowledge::kb_append(&self.knowledge, &arguments)),
            "kb_list" => outcome_result(knowledge::kb_list(&self.knowledge, &arguments)),
            "memory_search" => outcome_result(memory::memory_search(&self.memory, &arguments)),
            "memory_read" => outcome_result(memory::memory_read(&self.memory, &arguments)),
            "memory_expand_related" => {
                outcome_result(memory::memory_expand_related(&self.memory, &arguments))
            }
            "memory_write" => outcome_result(memory::memory_write(&self.memory, &arguments)),
            "web_search" => outcome_result(web_search::web_search(&self.web_search, &arguments)),
            "fetcher" => outcome_result(fetcher::fetcher(&self.fetcher, &arguments)),
            "image_gen" => outcome_result(image_gen::image_gen(&self.image_gen, &arguments)),
            "windows_window" => outcome_result(windows::windows_window(&arguments)),
            "windows_control" => outcome_result(windows::windows_control(&arguments)),
            "windows_input" => outcome_result(windows::windows_input(&arguments)),
            "windows_clipboard" => outcome_result(windows::windows_clipboard(&arguments)),
            "advisor" => outcome_result(advisor::advisor(&self.advisor, &arguments)),
            _ => return None,
        };
        Some(ToolExecution {
            result,
            ..ToolExecution::default()
        })
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
        ] {
            assert!(names.contains(expected), "缺少 {expected}：{names:?}");
        }
        for hidden in ["subagent", "memory_search", "read_image"] {
            assert!(!names.contains(hidden), "{hidden} 不该出现：{names:?}");
        }
        // 内核自持的三个工具仍在表里（界面负责交互）。
        for meta in META_RUNNERS {
            assert!(names.contains(meta), "缺少 {meta}");
        }
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
    }

    /// 联网工具的传输桩：搜索端点返回结果页，其余返回普通页面。
    struct StubTransport;

    impl WebTransport for StubTransport {
        fn send(&self, request: &WebRequest) -> Result<WebResponse, WebError> {
            let body = if request.url.contains("bing.com") {
                r#"<li class="b_algo"><h2><a href="https://example.com/1">标题一</a></h2><p>摘要一</p></li>"#
            } else if request.url.contains("/images/generations") {
                r#"{"data": [{"b64_json": "aGVsbG8=", "output_format": "png"}]}"#
            } else {
                "<html><head><title>示例页</title></head><body><main><p>正文内容</p></main></body></html>"
            };
            Ok(WebResponse {
                status: 200,
                final_url: request.url.clone(),
                body: body.as_bytes().to_vec(),
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
            searched.output.contains("来源：bing"),
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
        let options = RegistryOptions {
            native_vision: true,
            ..RegistryOptions::default()
        };
        let registry = ToolRegistry::new(&root, &options, 360).expect("工具表应当构建成功");
        // 未开启原生视觉时这张表里没有 read_image：图片读出来也无处可送。
        let default_registry =
            ToolRegistry::new(&root, &RegistryOptions::default(), 360).expect("工具表应当构建成功");
        assert!(!declared_names(&default_registry).contains("read_image"));

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
}
