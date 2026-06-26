# Agent Skill 系统实现文档

> 基于 Pi（Codex Agent SDK）Skill 系统设计，遵循 [Agent Skills 标准](https://agentskills.io/specification)，为 TUI Agent 设计的 Python Skill 子系统实现方案。

---

## 1. 架构总览

```
┌──────────────────────────────────────────────────────────────┐
│                     终端 / 全屏 TUI                           │
├──────────────────────────────────────────────────────────────┤
│                    LocalToolAgent                            │
│  ┌──────────┐  ┌──────────┐  ┌───────────────────────────┐  │
│  │ 工具系统  │  │ LLM 对话  │  │  Skill 子系统              │  │
│  │          │  │          │  │ ┌───────────────────────┐  │  │
│  │list_files │  │_system_  │  │ │ SkillManager           │  │  │
│  │read_file  │  │ prompt() │──│ │ - discover() 扫描目录  │  │  │
│  │run_cmd    │  │          │  │ │ - match() 匹配用户意图  │  │  │
│  │...        │  │          │  │ │ - inject() 注入提示词   │  │  │
│  └──────────┘  └──────────┘  │ │ - list_all() 查询列表   │  │  │
│                               │ └───────────────────────┘  │  │
│                               └───────────────────────────┘  │
└──────────────────────────────────────────────────────────────┘
```

**核心设计思路**：Skill 子系统作为独立模块，不修改工具协议和对话流程。发现阶段只解析元数据（渐进式披露），匹配阶段基于关键词打分，注入阶段将完整 Skill 正文插入 system prompt 头部。

---

## 2. 参考来源与对比

本文档设计吸收了 Pi (`packages/coding-agent/src/core/skills.ts` & `packages/agent/src/harness/skills.ts`) 的以下精华：

| 特性 | Pi 实现 | 本文档采用 | 说明 |
|------|---------|-----------|------|
| SKILL.md 格式 | YAML frontmatter + Markdown 正文 | 同 | 遵循 Agent Skills 标准 |
| 名字校验 | 64字符、`[a-z0-9-]`、禁止首尾/连续连字符 | 同 | 标准校验规则 |
| 描述校验 | 必填，最长 1024 字符 | 同 | 缺少描述不加载 |
| `disable-model-invocation` | 支持 | 同 | 隐藏技能，仅 `/skill:name` 调用 |
| 忽略文件 | `.gitignore` / `.ignore` / `.fdignore` | 同 | 受保护路径额外排除 |
| 冲突诊断 | 同名保留先发现者，记录碰撞 | 同 | 含 winner/loser 路径 |
| 符号链接去重 | canonicalPath 去重 | 同 | 避免同一文件重复加载 |
| XML 注入格式 | `<available_skills>` / `<skill>` 标签 | 同 | 符合集成标准 |
| 作用域优先级 | user → project → path → （无 enterprise） | 扩展为 project > user > enterprise | 项目级覆盖个人级 |
| 匹配策略 | LLM 自行选择（全部展示） | **关键词预匹配 + LLM 自行选择（双模式）** | 省 token + 保留灵活性 |
| `/skill:name` 命令 | 支持 | 同 | `:` 分隔符更清晰 |
| 热加载 | 监听文件变更 | 暂缓 | 第二期 |

---

## 3. Skill 存储结构

### 3.1 多级作用域

```
优先级从高到低（同名 Skill 高优先级覆盖低优先级）：

  1（最高）  .claude/skills/          项目级 — 仅当前项目生效
  2         ~/.tui-agent/skills/      个人级 — 当前用户所有项目
  3（最低）  $TUI_AGENT_ENTERPRISE_DIR 企业级 — 管理员统一配置
```

Pi 仅有 user / project / path 三级；本文档扩展企业级，支持 `TUI_AGENT_ENTERPRISE_DIR` 环境变量。

### 3.2 目录结构

```
skills/
└── python-code-review/        # 目录名用 kebab-case
    ├── SKILL.md               # 必需：核心指令文件（YAML frontmatter + Markdown）
    ├── scripts/               # 可选：辅助脚本
    │   └── lint_check.py
    ├── references/            # 可选：按需加载的参考文档
    │   └── pep8_guide.md
    └── assets/                # 可选：模板、图片等资源
        └── report_template.md
```

**发现规则**（来自 Pi）：
1. 目录包含 `SKILL.md` → 视为技能根目录，**不再递归深入**子目录
2. `.` 开头的目录和 `node_modules` 自动跳过
3. `.gitignore` / `.ignore` / `.fdignore` 中的规则自动生效

### 3.3 SKILL.md 格式

```yaml
---
name: python-code-review
description: 对 Python 代码进行 PEP8 规范审查、漏洞检测、可读性优化。适用于后端代码提交前校验。
disable-model-invocation: false   # 可选，true 时仅 /skill:name 手动调用
---

# Python 代码审查技能

## 执行目标
严格遵循 PEP8 规范，检测代码语法漏洞、冗余逻辑、命名不规范问题，输出结构化审查报告。

## 执行步骤
1. 读取待审查 Python 代码，解析语法结构
2. 校验变量命名、缩进、注释、空行等规范
3. 检测潜在 bug（如未捕获异常、资源泄漏）
4. 给出优化建议与修改后的代码片段
5. 生成 Markdown 格式审查报告

## 输出要求
报告需包含：问题等级（高危/中危/低危）、问题位置、违规规范、优化方案。
```

### 3.4 元数据字段规范

| 字段 | 必需 | 约束 | 说明 |
|------|------|------|------|
| `name` | 是 | 1-64字符，`[a-z0-9-]+`，禁止首尾连字符，禁止连续连字符 | 唯一标识 |
| `description` | 是 | 必填，最长1024字符，**不可空白** | AI 匹配 Skill 的主要依据 |
| `disable-model-invocation` | 否 | 布尔值 | `true` 时不注入 prompt，仅 `/skill:name` 手动调用 |

**名称校验规则**（来自 Pi，遵循 Agent Skills 标准）：

| 规则 | 有效示例 | 无效示例 |
|------|---------|---------|
| 仅小写字母、数字、连字符 | `pdf-tools`, `code-review` | `PDF-Tools`, `code_review` |
| 不超过64字符 | `my-skill` | 65+ 字符的名称 |
| 不以连字符开头/结尾 | `brave-search` | `-pdf`, `pdf-` |
| 不含连续连字符 | `data-analysis` | `data--analysis` |

与 Pi 相同：**不强制** name 与父目录名一致（Pi 认为该标准要求对共享技能目录过于严格）。

---

## 4. 核心模块设计

### 4.1 数据模型（`omnicrawl/skill.py`）

```python
from __future__ import annotations

import re
import yaml
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


# ── 常量 ──────────────────────────────────────────────

MAX_NAME_LENGTH = 64
MAX_DESCRIPTION_LENGTH = 1024
SKILL_FILE_NAME = "SKILL.md"
IGNORE_FILE_NAMES = [".gitignore", ".ignore", ".fdignore"]


# ── 诊断 ──────────────────────────────────────────────

@dataclass
class SkillDiagnostic:
    """Skill 加载过程中产生的警告/冲突信息。"""
    type: str                      # "warning" | "collision"
    message: str
    path: str
    # 仅碰撞类型
    collision: dict | None = None  # {"resourceType": "skill", "name": str, "winnerPath": str, "loserPath": str}


# ── 数据类 ────────────────────────────────────────────

@dataclass
class SkillMeta:
    """Skill 轻量级索引条目——仅含元数据，不加载正文。（渐进式披露）"""
    name: str
    description: str
    source_path: Path              # SKILL.md 绝对路径
    base_dir: Path                 # Skill 根目录（用于解析相对路径）
    scope: str                     # "project" | "user" | "enterprise" | "path"
    disable_model_invocation: bool = False


@dataclass
class Skill:
    """完整 Skill，含加载后的指令正文。"""
    meta: SkillMeta
    body: str                      # YAML frontmatter 之后的 Markdown 正文


@dataclass
class SkillMatchResult:
    """匹配结果。"""
    skill: Skill
    score: float                   # 0.0 ~ 1.0
    reason: str                    # 匹配原因，便于调试


# ── 校验函数 ──────────────────────────────────────────

def validate_skill_name(name: str) -> list[str]:
    """校验 Skill 名称，返回错误信息列表（空列表 = 有效）。"""
    errors: list[str] = []
    if len(name) > MAX_NAME_LENGTH:
        errors.append(f"名称超过 {MAX_NAME_LENGTH} 字符（当前 {len(name)} 字符）")
    if not re.fullmatch(r"[a-z0-9-]+", name):
        errors.append("名称只能包含小写字母 a-z、数字 0-9、连字符")
    if name.startswith("-") or name.endswith("-"):
        errors.append("名称不能以连字符开头或结尾")
    if "--" in name:
        errors.append("名称不能包含连续连字符")
    return errors


def validate_skill_description(description: str | None) -> list[str]:
    """校验描述，返回错误信息列表。"""
    errors: list[str] = []
    if not description or not description.strip():
        errors.append("description 是必填字段")
    elif len(description) > MAX_DESCRIPTION_LENGTH:
        errors.append(f"描述超过 {MAX_DESCRIPTION_LENGTH} 字符（当前 {len(description)} 字符）")
    return errors
```

### 4.2 SkillManager 核心类

```python
class SkillManager:
    """Skill 全生命周期管理：发现 → 索引 → 匹配 → 注入。

    参考 Pi 的两层架构设计：
    - coding-agent 层（本类）：路径管理、来源标记、去重冲突检测
    - agent 层（_parse_frontmatter 等）：纯文件解析、校验
    """

    def __init__(self) -> None:
        self._index: dict[str, SkillMeta] = {}    # name → SkillMeta
        self._diagnostics: list[SkillDiagnostic] = []
        self._real_paths: set[str] = set()         # 符号链接去重

    # ── 发现 ──────────────────────────────────────────

    def discover(self, cwd: Path | None = None, extra_paths: list[str] | None = None) -> None:
        """遍历所有作用域目录，构建 Skill 索引。

        加载顺序（低优先级先加载，高优先级覆写）：
        1. 企业级 — $TUI_AGENT_ENTERPRISE_DIR
        2. 个人级 — ~/.tui-agent/skills/
        3. 项目级 — <cwd>/.claude/skills/
        4. 额外路径 — extra_paths（CLI --skill 参数、settings.json skills 字段）

        同名 Skill 处理：保留先加载的（即优先级更高的），记录 collision 诊断。
        符号链接去重：同一 real path 只加载一次。
        """

    def _scan_directory(
        self,
        dir_path: Path,
        scope: str,
        ignore_patterns: list[str] | None = None,
        include_root_md: bool = True,
    ) -> None:
        """递归扫描目录下的 SKILL.md 文件。

        - 遇到 SKILL.md → 解析 → 添加到索引 → 不再递归深入
        - 跳过 . 开头目录
        - 遵循 ignore_patterns（来自 .gitignore 等）
        - include_root_md=True 时同时发现根目录下的单独 .md 文件（Pi 模式）
        """

    def _parse_frontmatter(self, file_path: Path) -> tuple[dict[str, Any], str]:
        """解析 SKILL.md 的 YAML frontmatter 和正文。

        返回 (frontmatter_dict, body_text)。
        解析失败时抛出 SkillError，由调用方记录诊断。
        """

    def _add_skill(self, meta: SkillMeta) -> None:
        """将 SkillMeta 添加到索引，处理冲突和去重。"""

        # 符号链接去重（参考 Pi 的 canonicalPath 机制）
        try:
            real_path = meta.source_path.resolve()
        except OSError:
            real_path = meta.source_path
        real_key = str(real_path)
        if real_key in self._real_paths:
            return
        self._real_paths.add(real_key)

        # 同名冲突检测
        existing = self._index.get(meta.name)
        if existing is not None:
            self._diagnostics.append(SkillDiagnostic(
                type="collision",
                message=f'Skill 名称 "{meta.name}" 冲突',
                path=str(meta.source_path),
                collision={
                    "resourceType": "skill",
                    "name": meta.name,
                    "winnerPath": str(existing.source_path),
                    "loserPath": str(meta.source_path),
                },
            ))
            return

        self._index[meta.name] = meta

    # ── 匹配 ──────────────────────────────────────────

    def match(self, user_input: str, max_results: int = 3) -> list[SkillMatchResult]:
        """基于用户输入匹配 Skill。

        支持两种模式：
        1. 自动匹配（默认）：关键词 Jaccard 相似度 + 子串命中加分
        2. 手动调用：以 /skill:name 开头的输入直接加载对应 Skill

        返回得分 ≥ 阈值的 Skill，按得分降序排列。
        """

    def match_by_name(self, name: str) -> Skill | None:
        """按名称精确获取 Skill（用于 /skill:name 命令）。"""

    # ── 匹配算法 ──────────────────────────────────────

    def _score_match(self, input_lower: str, meta: SkillMeta) -> tuple[float, str]:
        """对单个 Skill 计算匹配得分。

        算法：
        1. Skill name 精确命中 → 1.0 分
        2. keyword 子串匹配：description 中的关键词出现在用户输入中 → 每个 +0.15
        3. Jaccard 相似度：两侧关键词集合的交集/并集

        关键词提取：中文 2-4 字词组 + 英文 3+ 字母单词。
        """

    @staticmethod
    def _extract_keywords(text: str) -> list[str]:
        """从文本中提取关键词。"""
        cn_words = re.findall(r"[一-鿿]{2,4}", text)
        en_words = re.findall(r"[a-zA-Z]{3,}", text)
        return cn_words + [w.lower() for w in en_words]

    # ── 注入 ──────────────────────────────────────────

    def inject(self, matches: list[SkillMatchResult], system_prompt: str) -> str:
        """将匹配到的 Skill 指令注入系统提示词头部。

        注入格式遵循 Agent Skills 集成标准（参考 Pi 的 formatSkillsForPrompt）：

        <available_skills>
          <skill>
            <name>python-code-review</name>
            <description>对 Python 代码进行 PEP8 规范审查...</description>
            <location>/path/to/SKILL.md</location>
          </skill>
        </available_skills>

        随后拼接完整 Skill 正文：

        <skill name="python-code-review">
        # Python 代码审查技能
        ...
        </skill>

        注意：disable_model_invocation=True 的 Skill 不会通过 match() 自动匹配，
        只能通过 /skill:name 手动调用。
        """

    # ── 辅助方法 ──────────────────────────────────────

    def load_skill(self, meta: SkillMeta) -> Skill:
        """按需加载 SKILL.md 正文，从 meta 构造完整 Skill。"""

    def list_all(self) -> list[SkillMeta]:
        """返回所有已索引 Skill 元数据，供 /skills 命令使用。"""

    def get(self, name: str) -> Skill | None:
        """按名称获取完整 Skill。"""

    def get_diagnostics(self) -> list[SkillDiagnostic]:
        """返回发现阶段收集的诊断信息。"""

    # ── 忽略文件 ──────────────────────────────────────

    @staticmethod
    def _load_ignore_patterns(dir_path: Path) -> list[str]:
        """加载目录下 .gitignore / .ignore / .fdignore 的规则。

        参考 Pi 的 prefixIgnorePattern：自动为嵌套目录规则添加相对路径前缀。
        """

    @staticmethod
    def _is_ignored(rel_path: str, patterns: list[str], is_dir: bool = False) -> bool:
        """判断路径是否被忽略规则匹配。"""
```

### 4.3 匹配策略对比

本文档提供两种匹配策略，可在初始化时切换：

```python
class MatchStrategy(Enum):
    KEYWORD = "keyword"      # 关键词预匹配：Python 端打分，省 token
    LLM_CHOICE = "llm"       # LLM 自行选择：类似 Pi，全部注入 prompt
```

| 维度 | KEYWORD（默认） | LLM_CHOICE（Pi 风格） |
|------|----------------|----------------------|
| 上下文消耗 | 低，只注入匹配的 Skill | 高，所有 Skill 描述都在 prompt 中 |
| 准确性 | 依赖关键词提取质量 | 依赖 LLM 判断能力 |
| 灵活性 | 中文分词需调优 | 天然支持多语言 |
| 适用场景 | Skill 数量多（10+） | Skill 数量少（<10） |

**默认使用 KEYWORD**，因为当前项目 Skill 数量预计不多。若后续扩展技能市场，可切换为 LLM_CHOICE。

---

## 5. 集成点

### 5.1 修改 `AgentConfig`——增加 skills 配置

```python
@dataclass
class AgentConfig:
    # ... 现有字段 ...
    skills_enabled: bool = True         # 是否启用 Skill 子系统
    skill_paths: list[str] = field(default_factory=list)  # 额外的 Skill 路径（来自 settings.json 或 CLI）
    skill_match_strategy: str = "keyword"  # "keyword" | "llm"
```

### 5.2 修改 `LocalToolAgent.__init__`

```python
def __init__(self, config=None, confirm=None):
    # ... 现有初始化 ...
    self._skill_manager: SkillManager | None = None
    if self.config.skills_enabled:
        self._skill_manager = SkillManager()
        cwd = self.workspace_root
        self._skill_manager.discover(
            cwd=cwd,
            extra_paths=self.config.skill_paths,
        )
    self._active_skills: list[SkillMatchResult] = []
```

### 5.3 修改 `_system_prompt()`——注入 Skill 指令

```python
def _system_prompt(self) -> str:
    # ... 构造基础 system_prompt ...
    if self._skill_manager is not None and self._active_skills:
        system_prompt = self._skill_manager.inject(self._active_skills, system_prompt)
    if self._agents_instructions:
        system_prompt = f"{self._agents_instructions}\n\n---\n\n{system_prompt}"
    return system_prompt
```

### 5.4 修改 `run_stream()`——每轮任务开始时匹配

```python
def run_stream(self, user_text, on_delta, on_status=None):
    # 处理 /skill:name 命令
    if self._skill_manager is not None:
        if user_text.startswith("/skill:"):
            skill_name = user_text[len("/skill:"):].strip().split()[0]
            skill = self._skill_manager.match_by_name(skill_name)
            if skill:
                self._active_skills = [SkillMatchResult(skill=skill, score=1.0, reason=f"手动调用: {skill_name}")]
            # 不需要 fallback ——返回"未知 Skill"提示
        else:
            self._active_skills = self._skill_manager.match(user_text)
    # ... 继续现有执行流程 ...
```

### 5.5 添加 `/skills` 命令（在 `main.py` 中处理）

全屏 TUI 和行内 UI 分别在输入处理循环中拦截 `/skills`：

```python
if user_input.strip() == "/skills":
    if agent._skill_manager is not None:
        metas = agent._skill_manager.list_all()
        if metas:
            display_lines = ["已加载的 Skill："]
            for meta in metas:
                disabled = " [手动]" if meta.disable_model_invocation else ""
                display_lines.append(f"  {meta.name}{disabled}: {meta.description}")
            # TUI: tui.add_system_message("\n".join(display_lines))
            # 行内: print("\n".join(display_lines))
        else:
            # TUI/行内: 提示"当前没有已加载的 Skill"
    continue
```

### 5.6 CLI 参数扩展

```python
# main.py argparse 或手动解析
# --skill <path>     可重复，指定额外 Skill 文件或目录
# --no-skills        禁用 Skill 子系统
```

---

## 6. 渐进式披露流程

```
应用启动
  │
  ├─ SkillManager.discover()
  │    ├─ 遍历企业级/个人级/项目级技能目录
  │    ├─ 遵循 .gitignore /.ignore /.fdignore
  │    ├─ 仅解析 YAML frontmatter → 构建 SkillMeta 索引
  │    ├─ 符号链接去重 + 同名冲突诊断
  │    │  （内存占用：每个 Skill ~200 字节，不加载正文）
  │    └─ 日志输出：加载了 N 个 Skill，W 个诊断
  │
  ▼
用户输入
  │
  ├─ 是 /skill:name？ → match_by_name(name) → 强制加载该 Skill
  │
  ├─ 是 /skills ？    → list_all() → 展示 Skill 列表
  │
  ├─ 普通输入 → match(user_input)
  │    └─ 关键词提取 → Jaccard + 子串匹配 → 得分排序 → 返回 Top-N
  │
  ├─ SkillManager.inject(matches, system_prompt)
  │    └─ load_skill() 加载 SKILL.md 正文 → 注入 System Prompt 头部
  │
  ▼
LLM 请求
  │
  └─ System Prompt 包含 Skill 指令
     （仅本轮消耗 token，下一轮重新匹配）
```

**关键设计决策**：
- **启动时仅索引元数据** → 零 token 预热成本
- **每次用户输入时动态匹配** → Skill 集合随任务变化
- **注入位置在 system prompt 头部** → 确保 LLM 优先读取 Skill 指令
- **disable_model_invocation 技能不参与自动匹配** → 仅 `/skill:name` 触发（参考 Pi）

---

## 7. 文件清单

| 文件 | 作用 | 行数估算 |
|------|------|----------|
| `omnicrawl/skill.py` | SkillMeta / Skill / SkillMatchResult / SkillDiagnostic + SkillManager + 校验函数 | ~350 行 |
| `skills/` 目录 | 内置 Skill（如 `code-review/`） | 按需 |
| `omnicrawl/agent.py`（修改） | AgentConfig 增加字段 + `__init__` + `run_stream` + `_system_prompt` 集成 | +40 行 |
| `main.py`（修改） | `/skills` / `/skill:name` 命令 + CLI 参数解析 | +35 行 |
| `docs/skill_system_impl.md` | 本文档 | — |

---

## 8. 实现步骤

| 步骤 | 内容 | 涉及文件 | 预计行数 |
|------|------|----------|----------|
| 1 | 创建 `omnicrawl/skill.py`：数据类（SkillMeta, Skill, SkillMatchResult, SkillDiagnostic）+ 校验函数 | omnicrawl/skill.py | ~80 行 |
| 2 | 实现 `SkillManager._parse_frontmatter()` — YAML 头部解析 | omnicrawl/skill.py | ~30 行 |
| 3 | 实现 `SkillManager._scan_directory()` — 目录遍历 + 忽略规则 | omnicrawl/skill.py | ~50 行 |
| 4 | 实现 `SkillManager.discover()` — 多作用域遍历 + _add_skill 去重冲突 | omnicrawl/skill.py | ~60 行 |
| 5 | 实现 `SkillManager._score_match()` + `match()` — 关键词匹配算法 | omnicrawl/skill.py | ~50 行 |
| 6 | 实现 `SkillManager.inject()` — XML 格式注入 | omnicrawl/skill.py | ~30 行 |
| 7 | 实现 `SkillManager.list_all()` / `get()` / `match_by_name()` — 查询接口 | omnicrawl/skill.py | ~30 行 |
| 8 | 修改 `AgentConfig` — 增加 skills 配置字段 | omnicrawl/agent.py | +10 行 |
| 9 | 修改 `LocalToolAgent.__init__` / `_system_prompt()` / `run_stream()` | omnicrawl/agent.py | +30 行 |
| 10 | 修改 `main.py` — `/skills` / `/skill:name` 命令 + CLI 参数 | main.py | +35 行 |
| 11 | 创建示例 Skill（`skills/python-code-review/SKILL.md`）验证端到端流程 | skills/ | ~30 行 |
| 12 | 手工测试：启动 → `/skills` 查看 → 输入任务 → 验证 Skill 注入与执行 | — | — |

---

## 9. 设计取舍与决策记录

| 决策点 | 选择 | 理由 |
|--------|------|------|
| 匹配时机 | 每次 `run_stream()` 调用时 | Skill 随任务变化，不跨轮缓存 |
| 默认匹配算法 | 关键词 Jaccard + 子串加分 | 简单透明、无需外部依赖、可控性好 |
| 注入位置 | System Prompt 头部 | 确保 LLM 优先读取 Skill 指令 |
| 多 Skill 处理 | 返回 Top-N 全部注入 | 让 LLM 自行判断用哪个 |
| 手动调用格式 | `/skill:name`（`:` 分隔） | 与 Pi / Claude Code 兼容，避免与文件名连字符混淆 |
| 名称目录一致性 | **不强制**一致 | 共享技能目录被多个 agent 共用时更灵活（同 Pi 立场） |
| 存储 | 文件系统 + SKILL.md | 零依赖、可直接编辑、Git 可追踪 |
| 热加载 | 暂不实现 | `reload` 后可添加自动检测文件变更 |
| 企业级作用域 | 环境变量控制 | 简单且符合 12-factor |

---

## 10. 扩展方向（非本期）

- **LLM_CHOICE 模式**：类似 Pi，将所有 Skill 描述注入 prompt，由 LLM 自行选择—适合 Skill 数量少时
- **热加载**：用 `watchdog` 监听 Skill 目录变更，自动重新 discover，无需重启
- **Skill 依赖**：`SKILL.md` frontmatter 中声明 `requires: [other-skill]`，加载时自动引入
- **用户级匹配规则覆盖**：`~/.tui-agent/skill-rules.json` 允许用户自定义特定关键词到 Skill 的映射
- **Skill 市场/远程仓库**：从 GitHub 拉取社区 Skill 包，类似 Pi Skills 仓库
- **Skill 模板生成**：`/skill:new <name>` 交互式创建新 Skill 脚手架
