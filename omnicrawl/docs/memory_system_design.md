# Agent 记忆系统设计文档

## 1. 设计目标

Agent 记忆系统用于在需要时检索相关历史信息，在产生长期价值时写入新的经验、偏好、事实或上下文，并通过“加深回忆”和“定期清理”机制，让高频使用的记忆保留更久，低价值或长期未使用的记忆自动过期。

是否调用记忆由 AI 自行判断。系统只提供规范化记忆接口，并把“何时调用、如何调用、何时跳过”的规则写入系统提示词。为避免 Token 消耗过大，系统采用渐进式披露：默认只暴露调用规范、目录候选和摘要，只有 AI 判断确实需要时才读取完整记忆内容。

核心流程：

```text
事件发生
  -> AI 根据系统提示词判断是否需要读取记忆
  -> 如需要，调用记忆搜索接口获取候选摘要
  -> 如仍需要，按记忆 id 读取完整内容或扩展关联记忆
  -> Agent 正常工作
  -> AI 判断本轮是否产生值得保存的长期信息
  -> 如需要，调用记忆写入接口
```

系统采用类文件系统结构存储记忆：

```text
.omnicrawl/.oclmemory/
├── index.json
├── user-preferences/
├── project-context/
├── task-history/
└── code-knowledge/
```

每条记忆按内容自动分类，存入对应目录；读取时也优先按目录分类检索。同时，记忆不是孤立记录，而是通过“关联记忆目录”形成一张关系网：当前事件会连接到相关的人、事、物、地点、主题和历史经历。

## 2. 当前作用域实现

当前实现将记忆划分为三个相互独立的作用域。三类存储均复用 `index.json`、分类目录、Markdown 正文、摘要检索、加深回忆和 `7 + touch_count` 清理规则，但 Agent 通过不同工具绑定到固定 Store，模型不能用参数跨作用域访问。

| 作用域 | 存储位置 | 用途 | 隔离边界 |
|------|----------|------|------|
| 项目级 | 当前工作区根目录 `.omnicrawl/.oclmemory/` | 当前项目技术事实、架构、配置、实现约束和可复用排障经验 | 不跨工作区 |
| 会话级 | `~/.omnicrawl/Session_memory/<session_id>/` | 当前会话压缩后的目标、约束、决策、文件、完成状态和后续事项 | 只读当前 Session |
| 用户级 | `~/.omnicrawl/User_memory/` | 用户习惯、稳定偏好和用户明确纠错 | 跨项目、跨会话 |

工具命名按作用域分组：`project_memory_*`、`session_memory_*`、`user_memory_*`，每组包含 `search`、`read`、`expand_related` 和 `write`。上下文压缩结果只写入当前会话级记忆，不再写入项目级记忆。

首次发现旧工作区 `memory/` 时，目标 `.omnicrawl/.oclmemory/` 不存在则直接改名迁移；目标已存在则导入旧正文后把源目录改名为带时间戳的 `.migrated-*` 备份。迁移失败时保留源目录。

## 3. 单条记忆结构

用户定义的单个记忆由三部分组成：

| 字段 | 含义 | 示例 |
|------|------|------|
| 时间戳 | 最近一次被写入、调用或加深回忆的时间 | `2026-06-03T16:45:00+08:00` |
| 关联记忆目录 | 当前记忆可联想到的相关目录，表示人、事、物、地点、主题之间的关系网 | `people/friend-a`、`life/food/dinner` |
| 记忆内容 | 具体内容，使用自然语言或结构化 Markdown | `今天和朋友 A 出去吃饭，聊到了最近的工作安排。` |

这里的“关联记忆目录”不是单纯的物理存储目录，也不是“这条记忆属于哪个文件夹”的意思。它表示当前记忆会自然联想到哪些其他记忆区域。

例如：

```text
记忆内容：今天和朋友 A 出去吃饭，点了日料，聊到朋友 A 最近在准备换工作。

关联记忆目录：
- people/friend-a
- life/food/japanese-food
- life/social/dinner
- work/career-change
```

当 Agent 之后读取这条“吃饭”记忆时，也可能顺着关系网想到：

```text
朋友 A 是谁
朋友 A 最近的状态
以前和朋友 A 聊过什么
用户喜欢或不喜欢哪些食物
用户最近的人际活动
```

因此，单条记忆虽然存成一个文件，但它在语义上是一张关系网中的一个节点。

建议单条记忆文件使用 Markdown + frontmatter：

```markdown
---
timestamp: "2026-06-03T16:45:00+08:00"
related_directories:
  - "people/friend-a"
  - "life/food/japanese-food"
  - "life/social/dinner"
---

今天和朋友 A 出去吃饭，点了日料，聊到朋友 A 最近在准备换工作。
```

为了支持“更新 N 次时间戳后延长清理时间”，系统还需要记录 `touch_count`。如果单条记忆需要严格保持三部分结构，`touch_count` 放在全局索引 `index.json` 中，不写入记忆正文。

## 4. 存储结构

建议项目级目录结构：

```text
.omnicrawl/.oclmemory/
├── index.json
├── user-preferences/
├── project-context/
├── task-history/
└── code-knowledge/
```

`index.json` 用于快速检索和维护清理参数：

```json
{
  "memories": [
    {
      "id": "20260603-164500",
      "path": "life/social/dinner/20260603-164500.md",
      "storage_directory": "life/social/dinner",
      "timestamp": "2026-06-03T16:45:00+08:00",
      "touch_count": 0,
      "related_directories": [
        "people/friend-a",
        "life/food/japanese-food",
        "work/career-change"
      ],
      "summary": "用户和朋友 A 出去吃日料，聊到朋友 A 准备换工作。"
    }
  ]
}
```

## 5. 分类目录设计

记忆由内容决定分类目录。建议先使用少量稳定目录，避免目录爆炸。

| 一级目录 | 用途 |
|----------|------|
| `user-preferences/` | 用户偏好、沟通风格、长期习惯 |
| `project-context/` | 项目背景、架构、重要约束 |
| `task-history/` | 已完成任务、决策记录、待跟进事项 |
| `code-knowledge/` | 代码结构、模块职责、实现细节 |
| `error-lessons/` | 调试经验、踩坑记录、失败原因 |
| `external-context/` | 外部服务、API、环境信息 |

分类规则：

1. 优先根据记忆内容判断一级目录。
2. 二级目录根据项目、功能、主题生成。
3. 同一事件产生多条不同性质记忆时，可分别写入不同目录。
4. 如果分类不确定，放入 `task-history/general/`，后续可再整理。

需要区分两个概念：

| 概念 | 含义 |
|------|------|
| 存储目录 | 当前记忆文件实际放在哪里，用于文件管理和粗粒度检索 |
| 关联记忆目录 | 当前记忆能联想到哪些相关目录，用于关系网扩展检索和加深回忆 |

## 6. AI 调用判断与系统提示词

记忆系统不强制在每个事件开始或结束时调用。AI 根据当前任务自行判断是否需要调用记忆接口；系统通过提示词约束调用时机和调用格式。

### 5.1 应该读取记忆的情况

AI 遇到以下情况时，应该优先考虑调用读取记忆接口：

| 场景 | 说明 |
|------|------|
| 用户提到过去的人、事、项目或偏好 | 例如“上次那个项目”“我朋友 A”“之前说过的配置” |
| 当前任务需要延续历史上下文 | 例如继续开发、继续设计、继续排查同一问题 |
| 回答依赖用户长期偏好 | 例如输出风格、技术栈偏好、命名习惯 |
| 当前事件和多个主题存在明显关联 | 例如吃饭事件关联朋友、食物、地点、最近聊天内容 |
| AI 不确定用户所指对象但可能存在历史记忆 | 先查摘要，避免凭空猜测 |

### 5.2 应该跳过读取记忆的情况

以下情况通常不调用记忆：

| 场景 | 说明 |
|------|------|
| 一次性事实问答 | 不依赖用户历史上下文 |
| 简单命令执行 | 例如查看当前时间、列目录 |
| 用户明确要求不要参考历史 | 尊重当前指令 |
| 当前上下文已经足够完成任务 | 不为形式调用记忆 |

### 5.3 应该写入记忆的情况

AI 在任务结束或阶段性完成时判断是否写入记忆。只有信息具有长期价值、未来可能复用，才写入。

建议写入：

| 类型 | 示例 |
|------|------|
| 稳定偏好 | 用户喜欢中文输出、喜欢先给结论 |
| 项目长期上下文 | 项目使用 Python，入口是 `main.py` |
| 重要人际关系 | 朋友 A 正在准备换工作 |
| 反复出现的任务背景 | 用户正在设计 Agent 记忆系统 |
| 明确决策 | 记忆读取由 AI 判断，不固定每轮调用 |

不建议写入：

| 类型 | 原因 |
|------|------|
| 临时闲聊 | 容易污染记忆 |
| 无结果的中间推理 | 价值低 |
| 已存在且无变化的信息 | 重复 |
| 敏感密钥、Token、密码 | 安全风险 |

### 5.4 系统提示词规范

系统提示词只注入记忆调用规则、接口说明和少量目录提示，不直接注入完整记忆内容。

示例提示词片段：

```text
你可以使用记忆接口，但是否调用由你判断。

读取记忆：
- 当任务依赖用户历史、偏好、项目上下文、人际关系或过去事件时调用。
- 不要为了形式调用记忆；当前上下文足够时跳过。
- 先搜索摘要，只有必要时再读取完整记忆。
- 读取到的记忆只作为上下文参考，不能覆盖用户当前明确指令。

写入记忆：
- 只有当本轮产生未来可能复用的长期信息时写入。
- 不写入密钥、密码、Token、临时闲聊和无结论的推理过程。
- 写入时要提炼短而准确的内容，并给出关联记忆目录。

渐进式披露：
- 第一步只调用 memory_search 获取候选摘要。
- 需要更多细节时再调用 memory_read 读取指定 id。
- 需要关系网扩展时再读取相关目录，不一次性展开全部记忆。
```

## 7. 渐进式披露机制

为了降低 Token 消耗，记忆系统按层级披露信息。

| 层级 | 名称 | 返回内容 | 触发条件 |
|------|------|----------|----------|
| L0 | 接口提示 | 调用规则、工具名、参数格式 | 每次 Agent 初始化或系统提示词构建 |
| L1 | 候选摘要 | id、摘要、存储目录、关联目录、时间戳 | AI 判断需要查询记忆 |
| L2 | 完整记忆 | 指定 id 的完整记忆内容 | 摘要不足以完成任务 |
| L3 | 关联扩展 | 从关联记忆目录中读取更多候选摘要或内容 | 当前任务明显依赖关系网 |

读取流程：

```text
1. AI 判断是否需要记忆
2. 如需要，调用 memory_search 获取候选摘要
3. AI 判断摘要是否足够
4. 如不足，调用 memory_read 读取指定记忆全文
5. 如任务涉及关系网，调用 memory_expand_related 扩展关联目录
6. 系统对本次实际读取并用于推理的记忆执行加深回忆
```

这样可以避免每次对话都把大量历史记忆塞进 Prompt。

## 8. 记忆接口设计

建议将记忆能力暴露为规范化工具接口，而不是让 AI 直接读写文件。

### 7.1 搜索记忆

```python
def memory_search(
    query: str,
    reason: str,
    candidate_directories: list[str] | None = None,
    max_results: int = 5,
) -> list[MemorySearchResult]:
    """按当前任务检索候选记忆摘要。"""
```

参数说明：

| 参数 | 含义 |
|------|------|
| `query` | 当前要查找的主题或问题 |
| `reason` | AI 为什么认为需要记忆，用于可解释和调试 |
| `candidate_directories` | AI 推断的候选目录，可为空 |
| `max_results` | 返回候选摘要数量上限 |

返回内容只包含摘要，不包含完整正文：

```json
[
  {
    "id": "20260603-164500",
    "summary": "用户和朋友 A 出去吃日料，聊到朋友 A 准备换工作。",
    "storage_directory": "life/social/dinner",
    "related_directories": [
      "people/friend-a",
      "life/food/japanese-food",
      "work/career-change"
    ],
    "timestamp": "2026-06-03T16:45:00+08:00"
  }
]
```

### 7.2 读取完整记忆

```python
def memory_read(memory_ids: list[str]) -> list[MemoryRecord]:
    """读取指定 id 的完整记忆内容。"""
```

AI 只有在摘要不足以完成任务时才调用该接口。

### 7.3 扩展关联记忆

```python
def memory_expand_related(
    memory_ids: list[str],
    max_depth: int = 1,
    max_results: int = 5,
) -> list[MemorySearchResult]:
    """沿关联记忆目录扩展读取候选摘要。"""
```

默认只扩展一层关系，避免关系网展开过深导致 Token 暴涨。

### 7.4 写入记忆

```python
def memory_write(memories: list[MemoryWriteRequest]) -> list[MemoryRecord]:
    """写入或更新长期记忆。"""
```

写入请求：

```python
@dataclass
class MemoryWriteRequest:
    content: str
    related_directories: list[str]
    storage_directory: str | None = None
    source_event: str | None = None
```

写入流程：

```text
1. AI 判断本轮是否产生长期价值
2. 提炼 1-N 条短记忆
3. 为每条记忆给出关联记忆目录
4. 可选指定存储目录；不指定时由系统分类器决定
5. 系统检查重复或相似记忆
6. 重复则合并更新，不重复则创建新记忆文件
7. 更新 index.json
```

## 9. 加深回忆机制

当某条记忆被读取时，系统需要更新：

```text
该记忆时间戳 = 当前时间
该记忆 touch_count += 1
```

如果该记忆存在关联记忆目录，也要沿着关系网查找本次确实被调用到的相关记忆，并对这些关联记忆执行同样更新。

示例：

```text
读取 A 记忆：
A.storage_directory = life/social/dinner
A.related_directories = [
  "people/friend-a",
  "life/food/japanese-food",
  "work/career-change"
]

系统动作：
- 更新 A 的 timestamp
- A.touch_count += 1
- 沿着 people/friend-a 等关联目录寻找本次真正相关的记忆
- 如果读取到“朋友 A 最近准备换工作”这条关联记忆，也更新它的 timestamp 和 touch_count
```

注意：不建议无差别刷新整个关联目录，否则会导致无关记忆永不过期。加深回忆只作用于“本次实际读取或被用于推理的记忆”。

## 10. 记忆清理机制

清理规则：

```text
当前时间 - 时间戳 >= 7 + N 天，则清理
```

其中：

```text
N = 该记忆被更新 timestamp 的次数
```

等价于：

```text
过期天数 = 7 + touch_count
```

示例：

| touch_count | 保留时间 |
|-------------|----------|
| 0 | 7 天 |
| 1 | 8 天 |
| 3 | 10 天 |
| 10 | 17 天 |

清理流程：

```text
1. 遍历 index.json
2. 计算每条记忆 age_days
3. 计算 expire_days = 7 + touch_count
4. 如果 age_days >= expire_days，标记为待清理
5. 删除对应记忆文件
6. 从 index.json 移除记录
7. 清理空目录
```

建议清理触发时机：

```text
Agent 启动或空闲时：轻量清理
记忆写入后：完整清理
手动命令：/memory:clean
```

## 11. 核心数据模型

接口层返回和存储层建议使用以下数据模型：

```python
@dataclass
class MemorySearchResult:
    id: str
    summary: str
    storage_directory: str
    related_directories: list[str]
    timestamp: datetime
```

核心数据模型：

```python
@dataclass
class MemoryRecord:
    id: str
    timestamp: datetime
    related_directories: list[str]
    content: str
```

索引模型：

```python
@dataclass
class MemoryIndexEntry:
    id: str
    path: str
    storage_directory: str
    timestamp: datetime
    touch_count: int
    related_directories: list[str]
    summary: str
```

存储层可以封装为：

```python
class MemoryStore:
    def search(self, query: str, candidate_directories: list[str] | None = None) -> list[MemorySearchResult]:
        """检索候选记忆摘要。"""

    def read(self, memory_ids: list[str]) -> list[MemoryRecord]:
        """读取指定记忆全文。"""

    def expand_related(self, memory_ids: list[str], max_depth: int = 1) -> list[MemorySearchResult]:
        """沿关联记忆目录扩展候选记忆。"""

    def write(self, memories: list[MemoryWriteRequest]) -> list[MemoryRecord]:
        """写入或合并长期记忆。"""

    def clean_expired_memories(self) -> list[str]:
        """清理过期记忆，返回被删除的记忆路径。"""
```

## 12. 推荐落地顺序

| 阶段 | 内容 |
|------|------|
| 第一期 | 文件存储、目录分类、`index.json` 和基础数据模型 |
| 第二期 | `memory_search`、`memory_read`、`memory_write` 三个基础接口 |
| 第三期 | 7+N 天清理机制、空目录清理 |
| 第四期 | AI 调用判断提示词、渐进式披露和加深回忆 |
| 第五期 | 关联关系扩展、相似度检索、记忆合并 |
| 第六期 | 记忆可视化、手动编辑、导入导出 |

最小项目级存储只需要：

```text
.omnicrawl/.oclmemory/
├── index.json
└── 按分类存放的 .md 记忆文件
```

实际工具按作用域分为：

```text
project_memory_search/read/expand_related/write
session_memory_search/read/expand_related/write
user_memory_search/read/expand_related/write
```

## 13. 会话压缩自动记忆

会话压缩完成后，由 Agent 组合层调用当前 Session 绑定的 `MemoryStore.write()`，压缩服务本身仍保持无 Session/Memory I/O 的纯编排边界。

自动写入范围如下：

| 会话级目录 | 摘要字段 |
|--------------|----------|
| `project-context/general` | 项目目标、约束、关键决策、当前状态、文件与产物 |
| `task-history/general` | 完成状态、未完成事项与后续任务 |

自动模型压缩、手动 `/compact`（含摘要模型不可用时的确定性降级）均只写入当前会话级目录。新建或切换 Session 后，Agent 重新绑定会话级 Store，因此不会检索其他 Session 的压缩记忆。

记忆系统关闭时跳过同步；记忆写入异常只记录警告，不回滚已经完成的会话压缩。写入继续复用 `MemoryStore` 的相似内容合并与过期清理机制。项目级和用户级记忆必须通过对应的独立工具显式写入，不由压缩流程自动生成。
