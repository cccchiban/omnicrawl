# omnicrawl-config

配置域：TOML 读写、路径解析、模型与功能开关。语义基准是 `omnicrawl/config/` 下的
`core/`（runtime / settings / workspace / bootstrap）、`models/`（llm / llm_multi /
llm_client / model_store / model_catalog / channels / vision）与 `features/`（advisor /
agent_workspace / approval / context_compaction / desensitization / image_gen / run_guard /
subagents / tool_output_compression / tools / tts）。

内核要脱离宿主独立运行时，这些答案不能再由 Python 提供：配置文件在哪、用哪个模型、
走哪个协议、上下文窗口多大、开哪些功能、审批是哪种模式。

## 已搬范围

**`core/runtime`**：配置文件路径解析（显式路径 > `AI_*` 环境变量 > `~/.OmniCrawl/<name>.toml`）、
`.toml` 后缀校验、UTF-8（含 BOM）读取、空文件与遗留 `config.json` 的报错、子对象读取、
TOML 原子写回（同目录临时文件 + 替换，Windows 短暂 Access Denied 重试 8 次 × 0.05s 退避）、
旧用户目录迁移（同名冲突落 `<name>.migrated.bak`，必要时 `.migrated.N.bak`，任何失败保留旧目录）。

**`src/toml.rs`**：解析用 `toml` crate，**写回自实现**。两侧会交替读写同一份 `config.toml`，
所以文本按 `tomli_w.dumps` 的形状逐字节对齐：顶层标量在前、子表随后并以空行分隔、
中间层空壳表不占段落（只有带标量或叶子空表才写表头）、非空数组一律多行（每元素一行 +
尾逗号，缩进按层 +4）、键名只在 `[A-Za-z0-9_-]+` 时裸写、字符串按 `\b \t \n \f \r \" \\`
与 `\uXXXX`（< 0x20 或 0x7f）转义、浮点用 Python `repr` 写法。

**`models/llm`**：`LlmConfig` 运行视图与归一化校验（用户代理换行、窗口正整数、legacy 三件套、
custom 的 model/api_key）、`thinking_enabled`、`ActiveModelRef`、推理强度别名与报错文案、
`load_llm_config`（单模型与环境变量补齐）、`save_reasoning_effort`、`save_active_model_ref`。

**`models/model_store`**：`models.toml` 的解析、校验与写回。key 形状、凭据字段拒绝
（`api_key`/`token`/`cookie`/`authorization`）、别名冲突、排序（`sort_order` → `display_name`
小写 → key）、能力与顶层窗口的双向回填、`temperature` 的 `provider_options` 回退、
写回时「只写有意义的非默认能力」。

**`models/vision`**：视觉代理开关与有序模型引用（含重复检测）、`vision.models` 的逐项校验、
原生视觉三态开关（`parse_native_vision` / `resolve_native_vision` 的「模型覆盖 > 渠道覆盖 >
未配置」）、`save_native_vision` 的 model/channel 两个写入范围。

**`models/llm_multi`**：多模型 Profile 解析（`is_multi_model_section` / `parse_profiles`）、
`active_model` 与 `OMNICRAWL_MODEL` / `OMNICRAWL_PROFILE` 的来源判定、Profile 存在/启用与
协议匹配校验、凭据解析（`api_key_env` 优先）、窗口覆盖规则、模型选择
（自定义 key/别名、`profile/model_id`、裸 model_id）与 `llm_config_to_profile_and_descriptor`。

**`features/approval`**：审批模式别名表（11 条）、归一化与报错文案、中文显示名、
`load_approval_mode`（`mode` 键 > `auto_review` > `auto_approve` > 默认审查）、
`save_approval_mode`、`load_approval_review_model`。

**`features/tools`**：26 个内置工具开关的键表与界面文案、默认表（除 `powershell` 全开）、
旧版按作用域拆分的记忆工具名归一化、`load_tool_switches` / `load_disabled_tools`、
批量写回（先全校验再一次性写盘；写回顺序跟调用方给的顺序）。

**`features/context_compaction`**：`context_compaction` 段的严格校验（整数区间、比值
`0 < v < 1`、`failure_fallback` 仅支持 `deterministic`）、未知配置项拒绝、已废弃字段
`minimum_turns_between_model_compactions` 忽略。

**`features/run_guard`**：`[run_guard]` 三段（总开关 / `guard` / `continue`）的区间校验、
错误码白名单去重与上限 32、未知字段拒绝（三个前缀分别报错）、`save_run_guard_config` 写回。

## 与 Python 的差异

- **进程外信息注入**：home、平台名、`APPDATA`/`XDG_CONFIG_HOME`、`AI_*` 路径变量、模型与
  凭据环境变量都放在 `ConfigEnvironment` 里；`from_process()` 取进程环境，`new(home, platform)`
  只认显式注入（对照测试用后者，与 Python 侧清空 `os.environ` 同义）。
- **不搬 `project_root` / `_is_development_environment`**：Python 侧已注明它们不参与默认路径
  解析（Rust 侧没有「源码目录」这个运行时概念）。
- **路径字符串化**：按 `pathlib` 的观感对齐（统一分隔符、去掉 `.` 段、合并重复分隔符、
  去掉尾随分隔符、空路径归一成 `.`），因为路径会进错误文案。`..` 段与 UNC 前缀都保留。
- **读路径与写路径同源**：Python 的 `resolve_*_write_path` 与 `resolve_*_path` 实现等价，
  Rust 侧保留两个名字以便调用点自述意图。
- **`_strip_none`**：TOML 没有 null，Rust 文档类型本身不表达 `None`，调用方构造时跳过该键。
- **迁移不跨卷**：`shutil.move` 在跨设备时会退化成 copy+delete，Rust 侧只做 `rename`；
  跨卷会报错并保留旧目录（无数据丢失），不会静默降级。
- **`models/llm_client.py` 不在本 crate 重复搬**：`OpenAIResponseLLM` 的网络调用已由
  `omnicrawl-llm` 的运行时承担，异常分类文案（`format_request_error`）与负载解析
  （`extract_stream_text` / `extract_token_usage` / `_find_usage_payload`）分别对应
  `omnicrawl-llm` 的 `errors::map_exception` 与 `usage::usage_from_openai_payload`。
- **发现模型的能力字段**：内核 `DiscoveryModel` 与 Python 对齐，另带 `capabilities`（按协议的保守默认）与 `context_window_tokens`（发现阶段为 0，目录按 Profile 默认窗口回填）；其余字段 `profile_id/provider/protocol/model_id/
  display_name`，Python 侧的 `capabilities`/`context_window_tokens` 已同样可取。
- **类型即校验**：Python 里「取值必须是 `bool`/`int`/字符串」的 `isinstance` 分支在 Rust 侧由
  类型保证，因此没有对应用例；跨字段与跨字段区间校验仍全部对照。
- **文案差异**：底层库的错误尾巴随实现不同（IO 错误、TOML 解析位置）。数据集对这类文案只
  对照到「解析失败：<路径>，」之前。
- **未对照**：`datetime` 值的写法、`NaN`/`inf` 浮点、`pathlib` 在文件名为空时 `with_suffix`
  抛 `ValueError` 的行为、`bool` 被当作 `int` 的窗口取值。

## 重复实现的收敛

`omnicrawl-controllers/src/settings.rs` 在本 crate 出现之前已搬了审批模式别名与推理强度
归一化（Python 侧这两张表只有 `config/features/approval.py` 与 `config/models/llm.py` 一份）。
本轮已把该文件里的别名表、`VALID_REASONING_EFFORTS` 与两个归一化函数改为复用本 crate 的
实现（常量 `pub use`，函数转 `AgentError`），controllers 的 30 个测试套件保持全绿。

## 已搬范围（续：其余 features / core / models）

**`features/` 其余模块**：`advisor`（默认关闭、推理档位校验、执行者黑名单归一化）、
`agent_workspace`（worktree/local 与退出策略枚举校验）、`desensitization`（27 个字段的开关、
熵参数与 NER 取值域校验）、`image_gen`（Base URL、`auto|宽x高` 尺寸、质量与格式枚举）、
`tool_output_compression`（思考档位与四个正整数预算）、`tts`（线程数枚举、设备三态、
模型目录解析）、`subagents`（独立 `subagents.toml`、环境变量紧急刹车、`models.<角色>` 覆盖）。

**`core/settings`**：`load_feature_enabled`（`subagents` 段走独立文件）、
`save_context_window_tokens`（legacy 段 / 多模型 `defaults` / `models.toml` 条目三路）、
`save_context_compaction_trigger_percent`、`save_subagent_setting`（复用设置面板白名单校验）、
`save_feature_enabled`、`load_show_thinking` / `save_show_thinking`，以及 `save_mcp_config`
（MCP 配置以 `McpConfigData` 视图注入，服务端顺序即写回顺序）。

**`core/workspace`**：`[workspace] root` 的读取与写回（路径按 `expanduser` + `abspath` 归一化）。

**`core/context`**：工作区上下文检测，对应 `omnicrawl/workspace/context.py`；启动目录优先，支持
`AI_VOICE_CHAT_LAUNCH_CWD` 覆盖，已有文件取父目录，解析失败或路径不存在回退当前目录。
TUI 与本地 API 共用该模块，避免入口之间的工作区判定漂移。

**`core/bootstrap`**：首次启动编排——模板落盘、Profile 与模型配置读取、API Key 收集、
三项启动检查与启动提示文案。模板资源、渠道向导、API Key 输入、Node 探测与插件注册表都走
`StartupPorts` 注入；`check_node` / `check_plugin_state` 是公开的判定入口。

**`models/channels`**：渠道集合的成对读写（`config.toml` 的 profile ＋ `models.toml` 的条目）、
`unique_channel_key`、渠道校验（key 形状、协议与 Provider 匹配、Base URL、User-Agent 换行、
Profile 复用冲突）、写回后失效 profile 与条目的清理、`models.toml` 凭据剔除、
两次写盘任一失败的**回滚**，以及凭据完备性判定。

**`models/model_catalog`**：双列目录（custom / detected / diagnostics / current）、
`/models` 探测的请求构造与全部失败分支文案、发现缓存的 TTL 与 LRU 上限、
`save_llm_model` 的三条写回路径（单模型段 / 自定义条目别名 / Profile 目标）。
网络与时钟由 `ModelListFetch`、`CatalogPorts` 注入。

## 仍未搬（交回用户）

`models/llm_client.py` 按上文说明不重复搬。`config/` 其余部分已全部落地。

## 对照数据集

生成器：`rust/tools/gen_config_runtime_fixture.py`、`rust/tools/gen_config_models_fixture.py`、
`rust/tools/gen_config_features_fixture.py`、`rust/tools/gen_config_features_extra_fixture.py`、
`rust/tools/gen_config_core_fixture.py`、`rust/tools/gen_config_channels_fixture.py`、
`rust/tools/gen_config_catalog_fixture.py`（期望值全部来自 Python 真实现）。

- `tests/fixtures/config_runtime_parity.json`：路径解析 26 例、用户目录与旧目录 5 例、
  `get_section` 10 例、`load_config_data` 15 例、写回文本 22 例、迁移 5 例 + 冲突递增 1 例。
- `tests/fixtures/config_models_parity.json`：推理强度 21 例、`LlmConfig` 归一化 20 例、
  思考开关 8 例、模型引用 3 例、模型目录解析 36 例、写回 4 例、别名解析 5 例、
  加载 8 例 + 写回 5 例、视觉 16 例 + 三态解析 6 例 + 解析覆盖 6 例 + 写回 8 例、
  多模型加载 20 例 + 模型选择 8 例 + Profile 解析 3 例 + 描述转换 1 例。
- `tests/fixtures/config_features_parity.json`：审批归一化 15 例 + 标签 4 例 + 读取 12 例 +
  写回 3 例、工具名校验 9 例 + 键表/默认表/文案表、工具开关读取 6 例 + 写回 4 例、
  上下文压缩 26 例、运行护栏 24 例 + 写回 1 例。

验证：`cargo test -p omnicrawl-config`（10 个套件 / 59 项全绿，
含新增的 `features_extra_parity` 14 项、`core_parity` 13 项、`channels_parity` 7 项、
`catalog_parity` 5 项）、
`cargo test -p omnicrawl-config -p omnicrawl-controllers`（30 套件 / 201 项全绿）、
`cargo fmt -p omnicrawl-config -p omnicrawl-controllers -- --check`、
`cargo clippy -p omnicrawl-config -p omnicrawl-controllers --all-targets -- -D warnings`。

新增的数据集：

- `tests/fixtures/config_features_extra_parity.json`：advisor 读取 10 例 + 写回 2 例、
  tts 读取 10 例 + 写回 1 例、agent_workspace 读取 8 例 + 写回 1 例、
  image_gen 读取 14 例 + 写回 1 例、tool_output_compression 读取 9 例 + 写回 2 例、
  desensitization 读取 15 例 + 写回 1 例。
- `tests/fixtures/config_core_parity.json`：workspace 读取 5 例 + 写回 1 例、
  设置开关 6 例、窗口写回（配置 3 例 / 模型目录 3 例）、压缩阈值 5 例、
  显示思考 3 例、SubAgent 参数 4 例、MCP 写回 1 例、Node 判定 6 例、
  插件状态 5 例、启动提示 5 例、首次启动编排 7 例。
- `tests/fixtures/config_channels_parity.json`：默认草稿 4 例、key 归一化 7 例、
  标签 9 例、渠道读取 7 例、写回文本 2 例、渠道校验 11 例、凭据判定 4 例。
- `tests/fixtures/config_catalog_parity.json`：`/models` 探测 18 例（含请求头与 endpoint）、
  provider 分类 16 例、候选补全 3 例、列表渲染 3 例、环境变量接管 4 例、
  描述转换 2 例、模型写回 6 例、目录构建 4 例（含缓存命中与刷新两条路径）。
