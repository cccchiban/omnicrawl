# omnicrawl-extensions

Skill 子系统：`omnicrawl/extensions/skill.py` 的 Rust 内核移植。覆盖 Skill 全生命周期
「发现 → 索引 → 匹配 → 渐进式披露」，不涉及插件。

## 模块分工

| Python | Rust | 覆盖内容 |
| --- | --- | --- |
| `skill.py` | `src/skill.rs` | 名称与描述校验、frontmatter 解析、`SKILL.md` 与根级 `.md` 扫描、符号链接去重、同名冲突诊断、匹配打分、按作用域与关键词的索引检索 |
| — | `src/bm25.rs` | Skill 索引的 BM25 打分（正文进索引、中文二字切分、长度归一） |
| — | `src/path.rs` | `Path.resolve()` / `expanduser()` / `home()` 的可用子集 |

## 边界

插件子系统已于 2026-10-30 从产品中移除：模型与校验、注册表、Worker 协议、npm 安装器与
Hook 分发一并删除，`node_runner.mjs` 与 `packages/plugin-*` 也不再随产品分发。

## 测试

```bash
cd rust && cargo test -p omnicrawl-extensions
```

| 测试 | 覆盖的链路 |
| --- | --- |
| `tests/skill_discovery.rs` | 企业级作用域、额外路径、同名冲突与 frontmatter 块标量 |
| `tests/skill_bm25_search.rs` | 正文进索引、名称序截断、中文二字切分与长度归一 |

两个文件都只碰临时目录，不写项目文件。

## 已知差异

1. **`str()` 转换**：解析非字符串字段时不复现 Python 的 `str(None) == "None"`，
   `null` 一律回落空串（与会话层「已知差异」同一条约定）。
2. **集合迭代序**：Python 的 `set` 迭代受哈希随机化影响，匹配打分的子串加分与环检测的
   遍历序都不确定；Rust 侧按排序后的顺序处理，因此 `_score_match` 的浮点累加顺序固定，
   与 Python 的差异只可能在末尾 1e-16 量级。
3. **字符串数组读取**：Python 对非数组值（例如字符串）会逐字符迭代，Rust 侧统一按空数组处理。
