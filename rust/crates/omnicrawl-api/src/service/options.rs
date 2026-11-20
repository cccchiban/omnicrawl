//! 配置 → 运行时选项的装配。
//!
//! 这些函数把 `config` crate 的配置段折算成 `TurnRunner`/`PromptRuntime`/`McpClientManager`
//! 需要的结构。它们不碰 `AgentService` 状态，只读环境与配置，故独立成文件。

use crate::service::*;

/// 从内核 stderr 抓「会话已就绪」行里的会话 ID。
pub(super) fn parse_session_ready(line: &str) -> Option<String> {
    let marker = "会话已就绪：";
    let index = line.find(marker)?;
    let rest = line[index + marker.len()..].trim();
    let session_id: String = rest.chars().take_while(|ch| !ch.is_whitespace()).collect();
    if session_id.is_empty() {
        return None;
    }
    Some(session_id)
}
/// 内核可执行文件：显式路径 > `$OMNICRAWL_BINARY` > 与当前二进制同目录的 `omnicrawl` > PATH。
pub fn resolve_kernel_program(env: &ConfigEnvironment, explicit: Option<&Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    let from_env = env.get_trimmed("OMNICRAWL_BINARY");
    if !from_env.is_empty() {
        return PathBuf::from(from_env);
    }
    if let Ok(current) = std::env::current_exe() {
        if let Some(directory) = current.parent() {
            let sibling = directory.join(if cfg!(windows) {
                "omnicrawl.exe"
            } else {
                "omnicrawl"
            });
            if sibling.exists() {
                return sibling;
            }
        }
    }
    PathBuf::from("omnicrawl")
}
/// 从本地配置与进程环境组装服务选项（对应 Python 的 `create_default_agent`）。
///
/// 模型与审批模式读 `config.toml`（`llm` / `approval` 段），会话根取工作区下的
/// `.agent_sessions`（与 Python 同一套布局）。系统提示词与上下文消息走 `PromptRuntime`
/// 装配（模板 / AGENTS.md / Skill / 运行环境），装不出来才回落到配置文本。
pub fn options_from_process(env: &ConfigEnvironment) -> Result<ServiceOptions, String> {
    options_from_process_profile(env, true)
}
/// 连接器宿主的服务选项：与 API 同源，但**不挂隔离工作区**。
///
/// 远程连接器改的是用户当前那个工作区，会话也在同一份目录里；再套一层 worktree 会让
/// 「远程改的文件」落在隔离区、退出才 apply，与本地工作区看到的不是同一份事实。
pub fn connector_options_from_process(env: &ConfigEnvironment) -> Result<ServiceOptions, String> {
    options_from_process_profile(env, false)
}
pub(super) fn options_from_process_profile(
    env: &ConfigEnvironment,
    isolate: bool,
) -> Result<ServiceOptions, String> {
    let llm = load_llm_config(env).map_err(|error| error.to_string())?;
    if llm.model.trim().is_empty() {
        return Err(
            "未配置模型：请在 ~/.OmniCrawl/config.toml 的 llm 段或 models.toml 里选择模型。"
                .to_string(),
        );
    }
    let context = detect_project_context(env, None);
    let main_workspace = context.workspace_root;
    // 主 Agent 隔离工作区（对映 Python `api/app.py::create_default_agent`）：多个进程并行时各自在
    // 独立 worktree / 目录副本里读写，互不写穿；创建失败仅告警并回退主工作区。
    let (workspace, isolation) = if isolate {
        let workspace_config = load_agent_workspace_config(env, None).unwrap_or_default();
        let prepared = prepare_isolated_workspace(
            IsolationOptions::new(&main_workspace).with_config(workspace_config),
        );
        // 启动清扫（后台）：回收上次崩溃 / 被强杀遗留的过期隔离区。
        start_background_isolation_sweep(None);
        prepared
    } else {
        (main_workspace.clone(), None)
    };
    let approval = load_approval_mode(env, None)
        .map_err(|error| error.to_string())
        .and_then(|mode| {
            runtime_approval_mode(&mode).ok_or_else(|| format!("approval.mode 取值非法：{mode}"))
        })?;
    let system_prompt = if llm.system_prompt.trim().is_empty() {
        DEFAULT_SYSTEM_PROMPT.to_string()
    } else {
        llm.system_prompt.clone()
    };
    // 提示词装配：与 TUI 同源（模板 + AGENTS.md + Skill 索引 + 运行环境 + 模式切换）。
    // 装配成功时它以 `RunnerOptions.prompt` 覆盖 system prompt 与上下文消息；下面那份
    // 文本只作为装不出来时的兜底，避免 API 与 TUI 两头各有一套提示词口径。
    //
    // **不**把 `llm.system_prompt` 当显式覆盖：那份文案是语音客户端的默认值
    // （`config/models/llm.py` 的 `LLMConfig.system_prompt`），Python 的 Agent 从来不读它，
    // 提示词一律来自包内模板。
    let prompt = build_prompt_runtime(env, &workspace, &llm.model);
    let system_prompt = match prompt.as_ref() {
        Some(runtime) => runtime.system_prompt(),
        None => system_prompt,
    };
    let model = KernelModelConfig {
        model: llm.model.clone(),
        provider: llm.provider.clone(),
        protocol: llm.protocol.clone(),
        base_url: llm.base_url.clone(),
        api_key: llm.api_key.clone(),
        user_agent: if llm.user_agent.trim().is_empty() {
            USER_AGENT.to_string()
        } else {
            llm.user_agent.clone()
        },
        system_prompt,
        // system 之外的上下文消息由运行器按提示词装配结果覆盖。
        context_messages: Vec::new(),
        // 工具声明由运行器用工具表覆盖。
        tools: Vec::new(),
        options: generation_options(&llm),
        request_timeout_seconds: (llm.request_timeout_seconds > 0)
            .then_some(llm.request_timeout_seconds as f64),
        context_window_tokens: llm.context_window_tokens,
        // 自定义模型条目声明的 prompt 缓存能力；未声明时只对 GPT 系列回退尝试。
        prompt_cache_capable: llm.prompt_cache.unwrap_or(false),
        prompt_cache_identity: Default::default(),
        // 原生视觉的优先级由内核判定（`route_image_result` 同顺序）：为真时不走代理。
        // 传三态而不是布尔：未配置时留给内核按模型能力回落，压成 `false` 会误走代理。
        native_vision: llm.native_vision,
        vision_capability: llm.vision_capability,
        request_retry_count: llm.request_retry_count.max(1) as u32,
    };
    let mut options = ServiceOptions::new(workspace.clone(), model);
    // 会话根与 Python 一致：`~/.OmniCrawl/.agent_sessions`（不绑工作区），切换工作区不必搬会话。
    options.session_root = Some(user_config_dir(env).join(".agent_sessions"));
    options.approval = approval;
    // 宿主侧「交不交图」的判据必须与内核一致：显式开关优先，未配置时按模型能力回落。
    // 只压成 `llm.native_vision.unwrap_or(false)` 会把「模型自己支持视觉、用户没动过开关」
    // 当成不支持，于是 API 下图片根本不交给内核（TUI 走的是带回落的那条）。
    options.native_vision = native_vision_effective(&llm);
    options.prompt = prompt;
    options.env = env.clone();
    // MCP 是增量能力：配置读不到只告警；管理器建好后同时进工具表与 `/mcp` 状态。
    let mcp = build_mcp_manager(env, &options.workspace_root);
    options.registry_options.mcp = mcp.clone();
    options.mcp = mcp;
    // 后台任务管理器同理：工具表与 `/monitors` 必须看到同一批任务。
    let monitors = MonitorManager::new(options.workspace_root.clone());
    options.registry_options.monitors = Some(monitors.clone());
    // 记忆工具整组进出：与 Python `entry.py` 的 `load_feature_enabled("memory", default=True)`
    // 同源，缺省开启（本处早先没有设置，`RegistryOptions::default()` 让记忆整组永远不进表，
    // 而 `/settings` 的 `features.memory` 又报 true——两侧会自相矛盾）。
    // 作用域对齐 Python 的 `_create_memory_stores`：项目级与用户级常开；会话级由
    // `bind_session_memory` 在拿到会话 id 时打开——启动时还不知道会话 id，
    // 之后每次 `switch_session` 重起内核都会按目标会话重新绑一次。
    options.registry_options.memory_enabled =
        load_feature_enabled(env, "memory", true, None, None).unwrap_or(true);
    // 记忆目录归口：把旧的 `~/.omnicrawl/{User_memory,Session_memory}` 迁进 `memory/`。
    // 失败只告警——无头宿主没有界面可提示，记忆仍按新布局读写，不该拦启动。
    if options.registry_options.memory_enabled {
        if let Err(error) = omnicrawl_controllers::memory::ensure_memory_root(&user_data_root(
            env.home(),
        )) {
            eprintln!("[api] 记忆目录归口失败：{}", error.message());
        }
    }
    options.registry_options.memory = MemoryOptions {
        project_enabled: true,
        user_enabled: true,
        session_enabled: false,
        rerank: Arc::new(omnicrawl_host::tools::rerank_options_from_config(
            env,
            DECISION_SWITCH_MEMORY_SEARCH,
        )),
        embedding: Arc::new(
            omnicrawl_host::embedding_client::embedding_options_from_config(
                env,
                load_feature_enabled(env, "memory_embedding", false, None, None).unwrap_or(false),
                load_feature_enabled(env, "memory_rerank", false, None, None).unwrap_or(false),
            ),
        ),
        ..MemoryOptions::default()
    };
    // 知识库与 Skill 检索的第二阶段精排：与记忆召回共用同一份向量模型配置。
    options.registry_options.search_embedding = Arc::new(
        omnicrawl_host::embedding_client::embedding_options_from_config(
            env,
            load_feature_enabled(env, "memory_embedding", false, None, None).unwrap_or(false),
            load_feature_enabled(env, "memory_rerank", false, None, None).unwrap_or(false),
        ),
    );
    // 知识库检索重排：与 TUI 同源，按 `[decision_models.features] kb_search_rerank` 装配。
    options.registry_options.knowledge_rerank = Arc::new(
        omnicrawl_host::tools::rerank_options_from_config(env, DECISION_SWITCH_KB_SEARCH),
    );
    // Skill 检索重排：同一装配口径（`[decision_models.features] skill_search_rerank`）。
    options.registry_options.skill_rerank = Arc::new(
        omnicrawl_host::tools::rerank_options_from_config(env, DECISION_SWITCH_SKILL_SEARCH),
    );
    // 工具开关（`[tools]` 段）同样在启动期装载：若这里不装，`config.toml` 里关掉的
    // 内置工具（如 `powershell`）启动后仍会照旧进表，模型可见即可调用，开关形同虚设。
    options.registry_options.disabled_tools = load_disabled_tools(env, None).unwrap_or_default();
    // 知识库组整组进出（`[knowledge] enabled`）：与记忆组同源装配，不装的话配置里
    // 关掉知识库后工具仍会照旧进表，开关形同虚设。
    options.registry_options.knowledge_enabled = load_feature_enabled(
        env,
        "knowledge",
        omnicrawl_config::features::knowledge::DEFAULT_KNOWLEDGE_ENABLED,
        None,
        None,
    )
    .unwrap_or(omnicrawl_config::features::knowledge::DEFAULT_KNOWLEDGE_ENABLED);
    // 联网搜索同样在启动期装载：`RegistryOptions::default()` 的搜索选项是「默认引擎 duckduckgo +
    // 无凭据」，这里不装的话 `[web_search]` 里选的 tavily/exa/firecrawl/anysearch/tinyfish 与各自的 API Key 全部
    // 失效，工具会一直报「来源：duckduckgo」（与 TUI 同源，见 `web_search_options_from_config`）。
    // 该装配函数同时按 `[decision_models.features] web_search_rerank` 装上排序运行期。
    options.registry_options.web_search =
        omnicrawl_host::tools::web_search_options_from_config(env);
    // 图像生成同理：默认选项是「未启用 + 无凭据」，不装的话 `[image_gen]` 里配好的服务商、
    // 接口地址、模型与明文 api_key 全部失效（与 TUI 同源，见 `image_gen_options_from_config`）。
    options.registry_options.image_gen = omnicrawl_host::tools::image_gen_options_from_config(env);
    // TLS 会话恢复同理：`[tls_resumption] enabled` 为真时才建缓存，否则 `fetcher`
    // 永远拿不到 `Some`，配置段写了也没人读（会话恢复默认关闭，改的是密钥调度）。
    options.registry_options.fetcher.tls_session_cache =
        omnicrawl_host::tools::tls_session_cache_from_config(env);
    // 抓取引擎同理：`[fetcher] engine` 为 `local`（缺省）时行为与改造前完全一致，
    // 换成远程引擎后 `fetcher` 改由第三方抓取接口取正文。
    options.registry_options.fetcher.remote =
        omnicrawl_host::tools::remote_fetch_options_from_config(env);
    options.monitors = Some(monitors);
    // 审查运行期：与 TUI 同源（`approval.review_model` 为空时回落主模型，凭据与基地址
    // 沿用主渠道，脱敏旁路按 `[desensitization]` 配置构造）。
    options.review = build_review_options(&llm, env);
    // 提问托管：与 TUI 同源（`[decision_models.features] ask_user_custody`）。
    options.custody = Some(omnicrawl_host::tools::custody_options_from_config(env));
    // 工具调用淘汰：与 TUI 同源（`[decision_models.features] tool_call_prune`）。
    options.prune = Some(omnicrawl_host::tools::prune_options_from_config(env));
    // 建议：与 TUI 同源（`config.toml` 的 `[suggestion]`，检索纯本地 BM25）。
    options.skill_suggest = Some(omnicrawl_host::tools::suggest_options_from_config(env));
    options.isolation = Mutex::new(isolation);
    Ok(options)
}
/// 按配置与主模型视图装配审查运行期（与 TUI 同源：`omnicrawl_host::review`）。
///
/// 审查通道按 `[decision_models.features] tool_call_review` 在对话模型与结构化决策模型之间选；
/// 没有可用审查模型时返回 `None`（需审查的调用会 fail-closed 拒绝）。
pub(super) fn build_review_options(
    llm: &LlmConfig,
    env: &ConfigEnvironment,
) -> Option<omnicrawl_host::review::ReviewOptions> {
    omnicrawl_host::review::review_options_from_config(llm, env)
}
/// 原生视觉是否生效：显式开关优先，未配置时按模型自身声明的能力回落。
///
/// 与 TUI 的 `effective_native_vision` / 内核的 `native_vision_enabled` 同一判据：
/// 把「未配置」压成 `false` 会让支持视觉的模型（如 deepseek-v4.1）白走一趟视觉代理，
/// 甚至因为代理没配而把图片整个丢掉。
pub(super) fn native_vision_effective(llm: &LlmConfig) -> bool {
    omnicrawl_controllers::turn::turn_loop::native_vision_enabled(
        llm.native_vision,
        omnicrawl_controllers::turn::turn_loop::model_supports_vision(llm.vision_capability),
    )
}
/// `GenerationOptions` 的 JSON 形状：只带明确配置过的项。
pub(super) fn generation_options(llm: &LlmConfig) -> Value {
    let mut map = Map::new();
    if let Some(temperature) = llm.temperature {
        map.insert("temperature".to_string(), json!(temperature));
    }
    if !llm.reasoning_effort.trim().is_empty() {
        map.insert("reasoning_effort".to_string(), json!(llm.reasoning_effort));
    }
    if llm.max_output_tokens > 0 {
        map.insert(
            "max_output_tokens".to_string(),
            json!(llm.max_output_tokens),
        );
    }
    if !llm.provider_options.is_empty() {
        map.insert("provider_options".to_string(), json!(llm.provider_options));
    }
    Value::Object(map)
}
pub(super) fn runner_options(options: &ServiceOptions) -> RunnerOptions {
    RunnerOptions {
        workspace_root: options.workspace_root.clone(),
        model: options.model.clone(),
        session: options.session_root.as_ref().map(|root| {
            let (memory_root, compaction) = session_config_defaults(options);
            KernelSessionConfig {
                root: root.to_string_lossy().to_string(),
                // 新会话：内核建好后经 stderr 报 ID，本进程后续回合沿用同一个内核连接。
                session_id: String::new(),
                memory_root,
                workspace_root: Some(options.workspace_root.to_string_lossy().to_string()),
                compaction,
            }
        }),
        approval: options.approval,
        command_timeout_seconds: options.command_timeout_seconds,
        tool_timeout_seconds: options.tool_timeout_seconds,
        // 交不交图与「内核走不走代理」是两件事：原生视觉或 `[vision]` 代理任一可用都要把
        // 图片交出去，内核再按优先级决定直送主模型还是交给代理。
        attach_vision_images: options.native_vision || vision_proxy_configured(&options.env),
        client_name: "omnicrawl-api".to_string(),
        // 审查运行期与 `options_from_process` 装配的那份同源（嵌入与测试可直接填 `None`）。
        review: options.review.clone(),
        // 提问托管同源：与审查一样走决策渠道，失败一律退回人工提问。
        custody: options.custody.clone(),
        // 工具调用淘汰同源：与审查一样走决策渠道，失败一律保留原文。
        prune: options.prune.clone(),
        // 建议同源：关键词由主模型识别一次，检索本身是本地 BM25，开关在 `[suggestion]`；
        // 没有命中就原文照发。
        skill_suggest: options.skill_suggest.clone(),
        // 无头宿主不钉候选：按本轮识别出的技能词实时检索本机 Skill 索引。
        skill_suggest_pinned: None,
        // 关键词识别器：与 TUI 同源（主模型渠道），失败退化为整句检索。
        suggest_keywords: Some(omnicrawl_host::tools::keyword_client_from_config(
            &options.env,
        )),
        // 提示词装配结果：无头宿主与 TUI 共用同一套模板 / AGENTS.md / Skill 装配。
        prompt: options.prompt.clone(),
    }
}
/// 内核会话块里与配置有关的两项：记忆根与压缩策略。
///
/// 两者都必须由宿主下发：内核读不到用户配置，缺省就是 `CompactionConfig::default()`
/// （摘要预算 2000 token、记忆回写关闭），于是 `[context_compaction]` / `[memory]`
/// 就成了「配置写了却不生效」。启动与重起内核必须用同一份口径，否则切换会话或工作区后
/// 策略会悄悄退回默认值。
pub(super) fn session_config_defaults(
    options: &ServiceOptions,
) -> (Option<String>, Option<KernelCompactionConfig>) {
    let env = &options.env;
    // 开关与工具表同源（`[memory] enabled`，缺省开启）：关掉记忆就不该再写记忆目录。
    let memory_root = load_feature_enabled(env, "memory", true, None, None)
        .unwrap_or(true)
        .then(|| user_data_root(env.home()).to_string_lossy().to_string());
    let compaction = match load_context_compaction_config(env, None) {
        Ok(config) => Some(kernel_compaction_settings(
            options.model.context_window_tokens,
            &config,
        )),
        Err(error) => {
            // 无头宿主没有界面可提示，只能落到 stderr；降级为内核默认而不是让启动失败。
            eprintln!(
                "[api] 上下文压缩配置读取失败：{}（本会话按内核默认压缩策略运行）",
                error.message()
            );
            None
        }
    };
    (memory_root, compaction)
}
/// 窗口变化后要下发的压缩阈值：只带这两个字段，其余字段由内核保留现值。
pub(super) fn window_compaction_settings(
    options: &ServiceOptions,
    window: i64,
) -> Option<KernelCompactionConfig> {
    let config = load_context_compaction_config(&options.env, None).ok()?;
    let percent = config.trigger_context_percent?;
    Some(KernelCompactionConfig {
        trigger_context_tokens: Some(context_compaction_trigger_tokens(window, percent)),
        context_window_tokens: Some(window),
        ..KernelCompactionConfig::default()
    })
}
/// 提示词里的临时目录展示路径：优先用 `[agent_temp] directory` 的实际落点。
pub(super) fn agent_temp_dir_display(env: &ConfigEnvironment, workspace: &Path) -> String {
    let config =
        omnicrawl_workspace::load_agent_temp_workspace_config(env, None).unwrap_or_default();
    if !config.enabled {
        return omnicrawl_host::prompt::DEFAULT_AGENT_TEMP_DIR.to_string();
    }
    let resolved = omnicrawl_workspace::resolve_agent_temp_dir(workspace, &config.directory)
        .unwrap_or_else(|_| workspace.join(&config.directory));
    match resolved.strip_prefix(&omnicrawl_workspace::resolve_path(workspace)) {
        Ok(relative) => relative.to_string_lossy().to_string(),
        Err(_) => resolved.to_string_lossy().to_string(),
    }
}
/// 按工作区装配提示词运行时（与 TUI 的 `load_prompt_runtime` 同一套输入）。
///
/// `system_prompt_override` 恒为 `None`：API 没有「命令行提示词」这个概念，`config.toml`
/// 的 `llm.system_prompt` 也不参与 Agent 提示词（Python 侧只给语音客户端用）。
/// 装配失败时返回 `None` 并告警，由调用方沿用配置文本——无头服务不该因为提示词
/// 装不出来就起不来。
pub(super) fn build_prompt_runtime(
    env: &ConfigEnvironment,
    workspace: &Path,
    model: &str,
) -> Option<Arc<PromptRuntime>> {
    let mut options = PromptOptions::new(workspace.to_path_buf());
    // 临时目录取 `[agent_temp] directory` 的实际落点：提示词里给模型的路径必须与
    // 宿主/工具真正落盘的位置一致，否则模型按提示词去找文件会找不到。
    options.agent_temp_dir = agent_temp_dir_display(env, workspace);
    options.workspace_detection_summary = detect_project_context(env, None).detection_summary();
    options.system_prompt_override = None;
    // 顾问准则只在顾问真正可用（已启用且未命中黑名单）时追加，与 Python 同判据。
    let advisor =
        omnicrawl_config::features::advisor::load_advisor_config(env, None).unwrap_or_default();
    options.advisor_active = advisor.enabled;
    options.advisor_blacklisted = advisor.disabled_for_models.iter().any(|name| name == model);
    match PromptRuntime::load(env, options) {
        Ok(runtime) => Some(Arc::new(runtime)),
        Err(error) => {
            eprintln!("[api] 提示词装配失败，改用配置里的系统提示词：{error}");
            None
        }
    }
}
/// 按展示名派生项目目录名（与 Python `project_directory_name` 同规则）。
pub(super) fn project_directory_name(name: &str) -> String {
    let mut cleaned = String::new();
    for ch in name.trim().chars() {
        if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            cleaned.push('-');
        } else {
            cleaned.push(ch);
        }
    }
    let collapsed: String = cleaned.split_whitespace().collect::<Vec<&str>>().join("-");
    let trimmed = collapsed.trim_matches(|ch: char| ch == ' ' || ch == '.' || ch == '-');
    if trimmed.is_empty() {
        "new-project".to_string()
    } else {
        trimmed.to_string()
    }
}
/// MCP 管理器：配置读不到或未启用时按空表处理，工具表与状态面共用同一个句柄。
pub(super) fn build_mcp_manager(env: &ConfigEnvironment, workspace: &Path) -> Option<Arc<McpClientManager>> {
    let config = match omnicrawl_mcp::load_mcp_config(env, None) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("[api] MCP 配置读取失败，已跳过 MCP：{error}");
            return None;
        }
    };
    let manager = Arc::new(McpClientManager::new(config, workspace));
    // 与 TUI 同口径：有缓存清单就直接声明，首次调用某个 Server 的工具时再连接。
    if !manager.has_cached_capabilities() {
        manager.discover();
    }
    for diagnostic in manager.diagnostics() {
        let server = diagnostic.server_name.clone().unwrap_or_default();
        eprintln!(
            "[api] MCP [{}] {}: {}",
            diagnostic.severity, server, diagnostic.message
        );
    }
    Some(manager)
}
/// 连接器进程读到的 `config.toml`：读不到就按空对象处理。
///
/// 与内核连接器模式同口径（`omnicrawl-cli` 的 `connector_config_data`）：`[telegram]` /
/// `[feishu]` 段由连接器自己解析，配置读取失败只意味着「未配置」，不该让进程起不来。
pub fn connector_config_data(env: &ConfigEnvironment) -> Value {
    omnicrawl_config::core::runtime::load_config_data(env, None)
        .map(|table| omnicrawl_config::value::toml_to_json_object(&table))
        .unwrap_or_else(|_| Value::Object(serde_json::Map::new()))
}
