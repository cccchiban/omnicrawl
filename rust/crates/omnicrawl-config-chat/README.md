# omnicrawl-config-chat

配置对话：一句自然语言改配置。语义基准是 `omnicrawl/config_chat/`（`router.py` +
`service.py`），内核侧完整搬运，不是接口占位。

## 模块对映

| Python | Rust | 说明 |
| --- | --- | --- |
| `router.py::ConfigRouter` | `src/router.rs::ConfigRouter` | 片段切分 → 两个 BIO 头解码 → 别名向量检索 → 命令列表 |
| `router.py::Model` | `src/router_weights.rs::RouterWeights` | Embedding + 2 层双向 GRU + 三个头 + `project`（dropout 推理态恒等，不参与） |
| `router.py::ConfigRouterUnavailable` | `src/router.rs::ConfigRouterError` | 拆成 `Unavailable`（缺随包权重）与 `Invalid`（资源损坏），服务层按 Python 的两条分支分别加工文案 |
| `service.py::ConfigChatCommand` / `ConfigChange` / `ConfigChatError` | 同名 | `ConfigChatCommand.score` 是 `f64`（Python 侧 `float(scores[i])`） |
| `service.py::ConfigChatService` | `src/service.rs::ConfigChatService` | 校验 → 写回 → 运行态同步 |
| `service.py` 里的 `agent.set_*`（`getattr` 探测） | `src/service.rs::ConfigChatAgent` | trait 的**默认空方法**就是 Python 的「没实现就跳过」；`()` 是 `agent=None` |
| `assets/labels.json` / `assets/aliases.json` | `data/labels.json` / `data/aliases.json` | 逐字节副本，sha256 记在对照数据集里 |

`get_section` / `resolve_subagents_path` 与 `save_feature_enabled` / `save_show_thinking` /
`save_subagent_setting` 在 Python 侧是未使用的导入，不搬；`_write_value` 只用到
`load_config_data` / `save_config_data` / `resolve_subagents_write_path`。

## 资源与权重

`router.pt` 是 zip + pickle 的 torch 存档（17 MB），**内核不实现 pickle 解析**。权重由
`rust/tools/gen_config_router_fixture.py` 转成「魔数 `OCCFG1\0\0` + 头部 JSON + f32 数据块」
的自描述二进制 `data/config_router.bin`（约 16 MB，头部含词表 / 动作表 / 配置表 / 结构参数 /
张量表）。同一脚本把两份 JSON 原样复制到 `data/`，并把三份文件的 sha256 写进对照数据集。

```bash
python rust/tools/gen_config_router_fixture.py   # 需要 torch；重新生成上文四份文件
```

装载时做**形状全校验**（`embedding` / 每层 `weight_ih_l0`、`weight_hh_l0`、两个 bias、三个头、
`project`），过检之后前向里的切片不会越界；任何缺失或形状不符都带张量名报错。

## 与 Python 的差异

- **依赖面**：Python 侧缺 torch 时抛 `ConfigRouterUnavailable`；内核没有可选依赖，只有
  `data/` 下的权重文件，缺失即同类不可用（`ConfigRouterError::Unavailable`）。
- **从句切分**：`re.split(r"[，,；;。！？!?]|然后|接着|顺便|另外|还有", text)` 换成手写扫描器
  （`split_clauses`）。分隔符集合固定、连接词首字都不在标点集合内，两者语义一致；不引入
  `regex` 依赖。
- **整数/浮点折算**：用 Rust 的 `str::parse`，不接受 Python 允许的下划线分组（`1_000`）；
  其余（`+5`、`1e3`、`inf` / `nan`）一致。
- **资源不可用文案**：两侧都保留 `配置对话资源不可用：` 前缀，但内层文字不同——Python 直接把
  `except Exception` 的 `str(exc)` 拼上去（`FileNotFoundError` 的文案里带 `[Errno 2]` 与平台
  相关路径），Rust 统一写成 `读取 <labels.json 路径> 失败：<std::io::Error>`。
- **进程外信息**：配置路径与 `AI_CONFIG_FILE` / `AI_SUBAGENTS_FILE` 由
  `omnicrawl-config` 的 `ConfigEnvironment` 注入（Python 直接读 `os.environ` / `Path.home()`）。
- **运行态同步**：`ConfigChatAgent` 是宿主注入面；Python 在函数末尾对
  `tts` / `image_gen` / `vision` / `desensitization` / `run_guard` / `agent_workspace` /
  `advisor` 段的「提前返回」本身是空操作，内核侧等价表达为「未命中 setter 就不动手」。
- **浮点**：GRU 与头部的累加顺序与 Python 的循环顺序一致，但 torch 的算子实现不同，
  逐位结果在 1e-6 量级有差异；离散结论（BIO 标签、动作 id、检索下标）在对照数据集上的
  最小 top1-top2 间距是 `1.3e-2`，远大于该量级。
- **设备**：只有 CPU。

## 对照

```bash
python rust/tools/gen_config_router_fixture.py   # 用 Python 真实现生成期望值
cd rust && cargo test -p omnicrawl-config-chat   # 同输入逐项比对
```

`tests/config_chat_parity.rs` 五组：

1. **资源快照**：三份数据文件的 sha256 / 大小与数据集一致，权重按声明形状装载；
2. **片段切分**：手写扫描器 vs `re.split`（含空串、纯标点、超长句子、连接词连用）；
3. **路由**：逐从句的取词、两个 BIO 头的**逐位标签**、动作 id、片段区间、检索下标，以及
   最终命令列表（`score` 按 1e-4 容差）；
4. **校验折算**：`prepare_command` / `coerce_value` 的报错文案与折算值（含 `OPEN` / `TOGGLE` /
   `RESET`、段开关改写、bool / int / float / str / list / dict、未知配置）；
5. **服务写回**：`apply_text` 的改动列表与 `config.toml` / `subagents.toml` 的最终文本逐字节一致
   （含「先全校验再写盘」的失败样例与 `subagents.enabled` 落独立文件）。

## 尚未接线

宿主（TUI / 本地 API）还没有调用入口：设置面板里 `config_chat` 一级项仍显示未迁移提示。
接线时宿主负责给出资源目录（随包 `data/`）、实现 `ConfigChatAgent` 的 setter 子集，
并把 `ConfigChange` 推给自己的运行态刷新路径。
