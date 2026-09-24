# AI Skill 安装使用规范

本文档说明如何为本项目安装、编写和使用 AI Skill。目标是用渐进式披露控制上下文成本：启动时只索引 Skill 元数据，只有任务命中或用户手动调用时才读取完整 `SKILL.md`。

## 1. 先知道怎么用

Skill 是一组面向特定任务的专用指令。它不会替代工具权限确认，也不会自动执行高风险操作；它只告诉 Agent 在某类任务里应该按什么流程思考和交付。

常用命令：

```text
/skills
/skill:<skill-name> 你的任务描述
```

- `/skills`：查看当前已加载的 Skill。
- `/skill:<skill-name>`：手动指定本轮使用某个 Skill。
- 普通输入：Agent 会根据已索引的 `name` 和 `description` 判断是否加载匹配的 Skill。

## 2. 安装到哪里

按作用域选择安装目录：

| 作用域 | 目录 | 适用场景 |
|------|------|----------|
| 项目级 | `.omnicrawl/skills/<skill-name>/SKILL.md` | 只给当前项目使用，推荐优先使用 |
| 个人级 | `~/.omnicrawl/skills/<skill-name>/SKILL.md` | 当前用户多个项目复用 |
| 企业级 | `$OMNICRAWL_ENTERPRISE_DIR/<skill-name>/SKILL.md` | 管理员统一分发 |

同名 Skill 按当前实现由后加载作用域覆盖前面作用域：项目级优先于个人级，个人级优先于企业级。额外路径 `skill_paths` 的优先级最高。

个人级 `~/.omnicrawl/skills/` 与项目级 `.omnicrawl/skills/` 目录会在 OmniCrawl 启动（`discover`）时自动创建，无需手工 `mkdir`。企业级目录由环境变量显式指定，不自动创建。

推荐目录结构：

```text
.omnicrawl/skills/
└── python-code-review/
    ├── SKILL.md
    ├── references/
    │   └── checklist.md
    └── scripts/
        └── verify.py
```

## 3. 最小可用 Skill

`SKILL.md` 必须包含 YAML frontmatter。名称使用小写字母、数字和连字符，描述要写清楚触发场景。

```markdown
---
name: python-code-review
description: 审查 Python 代码的正确性、边界处理、测试覆盖和可维护性；适用于用户要求 review 或检查改动时。
disable-model-invocation: false
---

# Python Code Review

## 何时使用

当用户要求代码审查、检查改动、找风险或评估测试覆盖时使用。

## 工作流程

1. 先读取相关代码和 diff。
2. 优先列出 bug、回归风险和缺失测试。
3. 用文件路径和行号定位问题。
4. 如果没有发现问题，明确说明剩余风险。

## 输出要求

先给发现项，再给测试缺口，最后给简短总结。
```

字段规范：

| 字段 | 必填 | 规则 |
|------|------|------|
| `name` | 是 | 1-64 字符；仅 `a-z`、`0-9`、`-`；不能首尾为 `-`；不能连续 `--` |
| `description` | 是 | 不为空，最长 1024 字符；用于自动匹配 |
| `disable-model-invocation` | 否 | `true` 时不会自动匹配，只能 `/skill:name` 手动调用 |

## 4. 渐进式披露规则

Agent 对 Skill 的读取分三层：

| 层级 | 何时发生 | 读取内容 | 目的 |
|------|----------|----------|------|
| L1 索引 | 程序启动或重新发现时 | `name`、`description`、位置、开关 | 低成本知道有哪些 Skill |
| L2 匹配 | 用户输入一轮任务时 | 已索引元数据 | 判断是否需要某个 Skill |
| L3 加载 | 自动命中或 `/skill:name` 时 | 完整 `SKILL.md` 正文 | 注入专用流程和约束 |

编写 Skill 时也要遵循这个思想：

- `description` 写触发条件，不写长流程。
- `SKILL.md` 正文只写执行当前技能必须知道的流程。
- 大段参考资料放到 `references/`，在正文里说明“需要时再读取哪个文件”。
- 脚本放到 `scripts/`，不要把长脚本粘进正文。
- 模板、图片等资源放到 `assets/`，正文只说明用途和路径。

## 5. 安装流程

### 5.0 默认决策

当用户已经给出 GitHub URL、仓库地址或本地路径，并明确要求“安装 Skill”时，Agent 不再询问安装位置、安装方式或安装后动作，按以下默认值执行：

| 项目 | 默认值 |
|------|--------|
| 安装位置 | 项目级 `.omnicrawl/skills/` |
| 安装方式 | 先检查仓库结构和 `SKILL.md` 元数据，再复制必要的 Skill 目录 |
| 单 Skill 仓库 | 只有 Skill 名称、描述与用户目标一致时才直接安装 |
| 泛称仓库 | 仓库名为 `skills`、`claude-skills`、`agent-skills` 等泛称时，必须检查 README、分支和 tag |
| 多 Skill 仓库 | 如果用户未指定名称，只询问要安装哪一个 |
| 安装后动作 | 只做下载、复制、校验和 `/skills` 可见性验证，不自动执行 Skill 任务 |

仍会保留工具级安全确认：联网下载、写入文件、执行命令时，终端会展示具体工具和参数供用户确认。这是执行安全边界，不属于低价值方案确认。

### 5.0.1 目标核对规则

安装外部 Skill 时，目标核对优先于复制动作。即使仓库默认分支只有一个 `SKILL.md`，也不能把“发现一个 Skill”直接等同于“用户想安装这个 Skill”。

必须按以下顺序判断：

1. 读取用户输入中的显式目标，例如 Skill 名称、用途关键词、仓库分支、README 指向或用户补充说明。
2. 在临时目录拉取或展开来源仓库，读取默认分支的 `SKILL.md` frontmatter，至少核对 `name` 和 `description`。
3. 读取 README 摘要，判断仓库主描述与默认分支 Skill 是否一致。
4. 如果仓库名是 `skills`、`claude-skills`、`agent-skills` 等泛称，或用户只给了泛称仓库 URL，没有给明确 Skill 名称，必须继续检查远程分支、tag 和子目录。
5. 如果默认分支 Skill 与用户目标在名称或用途上不一致，必须用用户目标关键词搜索分支、tag、目录名、README 和 `SKILL.md` 元数据。
6. 只有找到唯一且语义匹配的候选时，才可以继续安装；如果存在多个候选、候选不唯一或无法判断，必须暂停并让用户选择。

典型误判场景：

- 用户给出 `https://github.com/xxx/skills`，默认分支中存在 `baoyu-design`，但用户实际目标是 `web-fetcher`。
- 默认分支 `SKILL.md` 可用，但 README 或分支名显示目标 Skill 在 `feat-web-fetcher` 等其他分支。
- 同一仓库多个分支分别维护不同 Skill，默认分支不能代表用户目标。

### 5.1 本地创建

1. 在 `.omnicrawl/skills/<skill-name>/` 下创建 `SKILL.md`。
2. 填写 `name`、`description` 和正文。
3. 重启程序，或在下一次精确调用 `/skill:<skill-name>` 时让系统重新发现。
4. 输入 `/skills` 确认已加载。
5. 用 `/skill:<skill-name> 测试任务` 做一次端到端验证。

### 5.2 从外部仓库安装

当前项目没有内置联网安装器。规范做法是先把外部 Skill 目录复制到目标作用域，再按本项目字段规范检查：

1. 确认来源可信，避免安装未知脚本。
2. 在临时目录检查默认分支的 `SKILL.md`、README、目录结构和可安装候选。
3. 对泛称仓库或语义不一致场景，继续检查远程分支、tag 和子目录，直到找到唯一目标或需要用户确认。
4. 检查目标 Skill 的 frontmatter 字段是否符合本项目规则。
5. 检查 `scripts/` 是否会联网、写文件或执行高风险命令。
6. 只复制目标 Skill 的必要文件到 `.omnicrawl/skills/<skill-name>/`、`~/.omnicrawl/skills/<skill-name>/` 或 `$OMNICRAWL_ENTERPRISE_DIR/<skill-name>/`，不要把完整仓库、临时脚本或无关示例复制到正式目录。
7. 重启程序并执行 `/skills` 验证。
8. 交付说明中写明 Skill 名称、来源仓库或本地路径、来源分支或 commit、安装目录、核心文件验证结果；如排除过不匹配候选，也要说明排除原因。

## 6. 验证清单

安装后至少检查：

| 检查项 | 通过标准 |
|--------|----------|
| `/skills` 可见 | 列表中出现 Skill 名称、作用域和描述 |
| 手动调用 | `/skill:<name> 测试任务` 能加载该 Skill |
| 自动匹配 | 普通任务命中 description 关键词时能按 Skill 流程回答 |
| 资源路径 | 正文中引用的 `references/`、`scripts/` 路径相对 `SKILL.md` 所在目录可解析 |
| 安全边界 | Skill 没有要求跳过用户确认或绕过工具安全限制 |

常见问题：

- `/skills` 看不到：检查目录层级是否是 `<skill-name>/SKILL.md`，或是否放错作用域。
- 名称不合法：改成 kebab-case，例如 `python-code-review`。
- 自动不触发：优化 `description`，写清楚任务关键词和适用场景。
- 只想手动触发：设置 `disable-model-invocation: true`。

## 7. 维护约定

- 项目专用 Skill 放 `.omnicrawl/skills/`，可随项目提交。
- 个人习惯类 Skill 放 `~/.omnicrawl/skills/`，不写入项目仓库。
- 修改 Skill 后用 `/skills` 和 `/skill:name` 做最小验证。
- 不在 Skill 中保存密钥、Token、私人路径或真实生产数据。
- 不让 Skill 承诺“自动安装依赖”“自动执行命令”；涉及安装、联网、删除、写入等操作仍必须走用户确认。
