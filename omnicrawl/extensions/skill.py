"""Agent Skill 子系统：发现 → 索引 → 匹配 → 注入。

参考 Pi（Codex Agent SDK）的 Skill 系统设计，遵循 Agent Skills 标准。
https://agentskills.io/specification
"""

from __future__ import annotations

import os
import re
import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any

# ═══════════════════════════════════════════════════════════════════════
# 常量
# ═══════════════════════════════════════════════════════════════════════

MAX_NAME_LENGTH = 64
MAX_DESCRIPTION_LENGTH = 1024
SKILL_FILE_NAME = "SKILL.md"
SKILL_MANIFEST_FILE_NAME = "manifest.json"
DEFAULT_MATCH_THRESHOLD = 0.3
DEFAULT_MAX_RESULTS = 3

# 默认扫描的作用域目录（优先级从低到高，后加载的覆盖先加载的）
SKILL_SCOPES: list[tuple[str, str]] = [
    # (scope标识, 环境变量或固定路径)
    ("enterprise", "TUI_AGENT_ENTERPRISE_DIR"),
    ("user", "~/.tui-agent/skills"),
    ("project", ".claude/skills"),
]


# ═══════════════════════════════════════════════════════════════════════
# 诊断
# ═══════════════════════════════════════════════════════════════════════

@dataclass
class SkillDiagnostic:
    """Skill 加载过程中产生的警告或冲突信息。"""
    type: str                       # "warning" | "collision"
    message: str
    path: str
    collision: dict[str, str] | None = None
    # collision 示例：{"resourceType": "skill", "name": "x", "winnerPath": "..", "loserPath": ".."}


# ═══════════════════════════════════════════════════════════════════════
# 数据模型
# ═══════════════════════════════════════════════════════════════════════

@dataclass
class SkillMeta:
    """Skill 轻量级索引条目——仅含元数据，不加载正文（渐进式披露）。

    每个 Skill 在发现阶段只解析 YAML frontmatter，占用约 200 字节。
    """
    name: str
    description: str
    source_path: Path               # SKILL.md 绝对路径
    base_dir: Path                  # Skill 根目录（解析相对路径时使用）
    scope: str                      # "project" | "user" | "enterprise"
    disable_model_invocation: bool = False


@dataclass
class Skill:
    """完整 Skill，包含加载后的指令正文。"""
    meta: SkillMeta
    body: str                       # YAML frontmatter 之后的 Markdown 正文


@dataclass
class SkillMatchResult:
    """匹配结果。"""
    skill: Skill
    score: float                    # 0.0 ~ 1.0
    reason: str


# ═══════════════════════════════════════════════════════════════════════
# 校验函数
# ═══════════════════════════════════════════════════════════════════════

def validate_skill_name(name: str) -> list[str]:
    """校验 Skill 名称，返回错误列表（空列表 = 有效）。

    规则（Agent Skills 标准）：
    - 1-64 字符
    - 仅小写字母 a-z、数字 0-9、连字符
    - 禁止首尾连字符
    - 禁止连续连字符
    """
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
    """校验描述，返回错误列表。"""
    errors: list[str] = []
    if not description or not description.strip():
        errors.append("description 是必填字段")
    elif len(description) > MAX_DESCRIPTION_LENGTH:
        errors.append(f"描述超过 {MAX_DESCRIPTION_LENGTH} 字符（当前 {len(description)} 字符）")
    return errors


# ═══════════════════════════════════════════════════════════════════════
# SkillManager
# ═══════════════════════════════════════════════════════════════════════

class SkillManager:
    """Skill 全生命周期管理：发现 → 索引 → 匹配 → 注入。

    参考 Pi 两层架构：
    - coding-agent 层（本类）：路径管理、来源标记、去重冲突
    - 本模块底层函数：纯文件解析、校验
    """

    def __init__(self) -> None:
        self._index: dict[str, SkillMeta] = {}
        self._body_cache: dict[str, str] = {}   # name → body（避免二次读取）
        self._diagnostics: list[SkillDiagnostic] = []
        self._real_paths: set[str] = set()
        self._last_cwd: Path | None = None
        self._last_extra_paths: list[str] = []

    # ── 发现 ──────────────────────────────────────────

    def discover(
        self,
        cwd: Path | None = None,
        extra_paths: list[str] | None = None,
    ) -> None:
        """遍历所有作用域目录，构建 Skill 索引。

        加载顺序（低优先级先，高优先级覆盖）：
        1. 企业级  — $TUI_AGENT_ENTERPRISE_DIR
        2. 个人级  — ~/.tui-agent/skills/
        3. 项目级  — <cwd>/.claude/skills/
        4. 额外路径 — extra_paths（CLI --skill、settings.json）

        同名 Skill 保留先加载的（即更高优先级的），记录碰撞诊断。
        符号链接去重：同一 real path 只加载一次。
        """
        self._index.clear()
        self._body_cache.clear()
        self._diagnostics.clear()
        self._real_paths.clear()
        self._last_cwd = (cwd or Path.cwd()).resolve()
        self._last_extra_paths = list(extra_paths or [])
        work_dir = self._last_cwd
        home_dir = Path.home()

        for scope, path_spec in SKILL_SCOPES:
            if scope == "enterprise":
                env_val = os.getenv(path_spec, "").strip()
                if env_val:
                    self._scan_directory(Path(env_val).expanduser().resolve(), scope)
            elif scope == "user":
                user_skills = home_dir / ".tui-agent" / "skills"
                self._scan_directory(user_skills, scope)
            elif scope == "project":
                project_skills = work_dir / ".claude" / "skills"
                self._scan_directory(project_skills, scope)

        for raw_path in (extra_paths or []):
            resolved = Path(raw_path).expanduser()
            if not resolved.is_absolute():
                resolved = work_dir / resolved
            resolved = resolved.resolve()
            if not resolved.exists():
                self._diagnostics.append(SkillDiagnostic(
                    type="warning",
                    message="Skill 路径不存在",
                    path=str(resolved),
                ))
                continue
            if resolved.is_dir():
                self._scan_directory(resolved, "path")
            elif resolved.is_file() and resolved.suffix == ".md":
                parsed = self._parse_skill_file(resolved, "path", self._diagnostics)
                if parsed is not None:
                    self._add_skill(*parsed)
            else:
                self._diagnostics.append(SkillDiagnostic(
                    type="warning",
                    message="Skill 路径不是目录或 .md 文件",
                    path=str(resolved),
                ))

    def _scan_directory(self, dir_path: Path, scope: str) -> None:
        """递归扫描目录下的 SKILL.md 和根级 .md 文件。

        规则（来自 Pi 实现）：
        - 找到 SKILL.md → 解析为 Skill → 不再递归进入该子目录
        - 跳过 . 开头目录
        - 根目录下的 .md 文件（非 SKILL.md）也作为独立 Skill 发现
        """
        if not dir_path.is_dir():
            return

        try:
            entries = sorted(dir_path.iterdir(), key=lambda e: e.name.lower())
        except OSError:
            return

        for entry in entries:
            if entry.name.startswith("."):
                continue

            if entry.is_symlink():
                try:
                    entry = entry.resolve(strict=True)
                except OSError:
                    continue

            if entry.is_dir():
                skill_md = entry / SKILL_FILE_NAME
                if skill_md.is_file():
                    parsed = self._parse_skill_file(skill_md, scope, self._diagnostics)
                    if parsed is not None:
                        self._add_skill(*parsed)
                    # 找到 SKILL.md → 不再递归深入
                    continue
                # 递归扫描子目录
                self._scan_directory(entry, scope)

            elif entry.is_file() and entry.suffix == ".md" and entry.name != SKILL_FILE_NAME:
                # 根目录下的 .md 文件作为独立 Skill
                parsed = self._parse_skill_file(entry, scope, self._diagnostics)
                if parsed is not None:
                    self._add_skill(*parsed)

    def _add_skill(self, meta: SkillMeta, body: str) -> None:
        """添加 Skill 到索引，处理符号链接去重和同名冲突。"""
        # 符号链接去重
        try:
            real_key = str(meta.source_path.resolve(strict=True))
        except OSError:
            real_key = str(meta.source_path)
        if real_key in self._real_paths:
            return
        self._real_paths.add(real_key)

        # 同名冲突：保留先加载的（高优先级）
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
        self._body_cache[meta.name] = body

    # ── 解析 ──────────────────────────────────────────

    @classmethod
    def _parse_skill_file(
        cls,
        file_path: Path,
        scope: str,
        diagnostics: list[SkillDiagnostic] | None = None,
    ) -> tuple[SkillMeta, str] | None:
        """解析 SKILL.md 的 YAML frontmatter。

        返回 (SkillMeta, body) 或 None（缺少必填字段/读取失败）。
        校验警告会追加到 diagnostics 列表中。
        """
        diags = diagnostics if diagnostics is not None else []

        try:
            raw = file_path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            return None

        frontmatter, body = cls._parse_frontmatter(raw)
        manifest = cls._read_manifest(file_path.parent)
        name = (
            frontmatter.get("name", "").strip()
            or str(manifest.get("skill_name", "")).strip()
            or file_path.parent.name
        )
        normalized_name = cls._normalize_skill_name(name)
        if normalized_name != name:
            diags.append(SkillDiagnostic(
                type="warning",
                message=f'Skill 名称 "{name}" 已兼容为 "{normalized_name}"',
                path=str(file_path),
            ))
            name = normalized_name

        description = frontmatter.get("description", "").strip()
        if not description and file_path.name == SKILL_FILE_NAME:
            description = cls._infer_description(raw, body)
            if description:
                diags.append(SkillDiagnostic(
                    type="warning",
                    message="Skill 缺少 description，已从 Markdown 内容推断",
                    path=str(file_path),
                ))

        # 校验 name
        for error in validate_skill_name(name):
            diags.append(SkillDiagnostic(
                type="warning",
                message=error,
                path=str(file_path),
            ))

        # 校验 description
        for error in validate_skill_description(description):
            diags.append(SkillDiagnostic(
                type="warning",
                message=error,
                path=str(file_path),
            ))

        # 缺少 description → 不加载
        if not description:
            return None

        disable_model = frontmatter.get("disable-model-invocation", False)
        if isinstance(disable_model, str):
            disable_model = disable_model.lower() == "true"

        meta = SkillMeta(
            name=name,
            description=description,
            source_path=file_path.resolve(),
            base_dir=file_path.parent.resolve(),
            scope=scope,
            disable_model_invocation=bool(disable_model),
        )
        return meta, body

    @staticmethod
    def _parse_frontmatter(content: str) -> tuple[dict[str, Any], str]:
        """解析 YAML frontmatter。

        返回 (frontmatter_dict, body_text)。
        无 frontmatter 时返回 ({}, content)。
        """
        normalized = content.replace("\r\n", "\n").replace("\r", "\n")
        if not normalized.startswith("---"):
            return {}, normalized

        end_idx = normalized.find("\n---", 3)
        if end_idx == -1:
            return {}, normalized

        yaml_str = normalized[4:end_idx]
        body = normalized[end_idx + 4:].strip()

        result: dict[str, Any] = {}
        for line in yaml_str.split("\n"):
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            m = re.match(r"^([a-zA-Z][\w-]*):\s*(.*)", line)
            if not m:
                continue
            key = m.group(1)
            value = m.group(2).strip()
            # 布尔值
            if value.lower() in ("true", "false"):
                value = value.lower() == "true"
            # 去除引号
            elif len(value) >= 2 and value[0] == value[-1] and value[0] in ('"', "'"):
                value = value[1:-1]
            result[key] = value

        return result, body

    @staticmethod
    def _read_manifest(skill_dir: Path) -> dict[str, Any]:
        manifest_path = skill_dir / SKILL_MANIFEST_FILE_NAME
        if not manifest_path.is_file():
            return {}

        try:
            data = json.loads(manifest_path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError):
            return {}
        return data if isinstance(data, dict) else {}

    @staticmethod
    def _normalize_skill_name(name: str) -> str:
        normalized = re.sub(r"[^a-z0-9-]+", "-", name.strip().lower())
        normalized = re.sub(r"-{2,}", "-", normalized).strip("-")
        return normalized or name

    @staticmethod
    def _infer_description(raw: str, body: str) -> str:
        source = body or raw
        lines = [line.strip() for line in source.splitlines()]

        for line in lines:
            if not line.startswith("#"):
                continue
            heading = line.lstrip("#").strip()
            if heading:
                return heading[:MAX_DESCRIPTION_LENGTH]

        for line in lines:
            if not line or line.startswith("---"):
                continue
            clean = re.sub(r"^[>\-\*\d、.()\s]+", "", line).strip()
            if clean:
                return clean[:MAX_DESCRIPTION_LENGTH]
        return ""

    # ── 匹配 ──────────────────────────────────────────

    def match(
        self,
        user_input: str,
        max_results: int = DEFAULT_MAX_RESULTS,
        threshold: float = DEFAULT_MATCH_THRESHOLD,
    ) -> list[SkillMatchResult]:
        """基于用户输入与 Skill description 进行关键词匹配。

        返回得分 ≥ threshold 的 Skill，按得分降序。
        disable_model_invocation=True 的技能不参与自动匹配。
        """
        input_lower = user_input.lower()
        results: list[SkillMatchResult] = []

        for meta in self._index.values():
            if meta.disable_model_invocation:
                continue
            score, reason = self._score_match(input_lower, meta)
            if score >= threshold:
                skill = self._load_skill(meta)
                results.append(SkillMatchResult(skill=skill, score=score, reason=reason))

        results.sort(key=lambda r: r.score, reverse=True)
        return results[:max_results]

    def match_by_name(self, name: str) -> Skill | None:
        """按名称精确获取 Skill（用于 /skill:name 命令）。

        索引未命中时自动 re-discover 一次，覆盖 AI 在当前会话内安装
        新 Skill 的场景。
        """
        meta = self._index.get(name)
        if meta is None:
            # 可能是 AI 在本次会话中新安装的 Skill，重新扫描
            self.reload()
            meta = self._index.get(name)
        if meta is None:
            return None
        return self._load_skill(meta)

    def reload(self) -> None:
        """重新扫描所有作用域目录，捕获新增/变更的 Skill。

        保留当前 cwd，与 __init__ 后的 discover 行为一致。
        """
        self.discover(cwd=self._last_cwd, extra_paths=self._last_extra_paths)

    def _score_match(self, input_lower: str, meta: SkillMeta) -> tuple[float, str]:
        """对单个 Skill 计算匹配得分。

        算法（多级打分）：
        1. Skill name 整体命中输入 → 1.0
        2. name 分词命中 → 每部分 +0.25
        3. description 关键词子串匹配 → 每个 +0.15
        4. Jaccard 相似度 → 关键词集合交集/并集
        """
        reasons: list[str] = []

        # 1. 名称整体命中
        name_parts = meta.name.replace("-", " ")
        if name_parts in input_lower:
            return 1.0, f"Skill 名称命中：{meta.name}"

        # 2. 名称分词命中
        name_keywords = set(meta.name.replace("-", " ").split())
        name_hits = {kw for kw in name_keywords if len(kw) >= 2 and kw in input_lower}
        name_bonus = len(name_hits) * 0.25
        if name_hits:
            reasons.append(f"名称分词命中：{', '.join(sorted(name_hits))}")

        # 3. 描述匹配
        desc_lower = meta.description.lower()
        desc_keywords = self._extract_keywords(desc_lower)
        input_keywords = self._extract_keywords(input_lower)

        if not desc_keywords and not name_keywords:
            return 0.0, "无有效关键词"

        # Jaccard 相似度
        intersection = desc_keywords & input_keywords
        union = desc_keywords | input_keywords
        jaccard = len(intersection) / len(union) if union else 0.0

        # 子串匹配加分
        substring_bonus = 0.0
        for kw in desc_keywords:
            if len(kw) >= 2 and kw in input_lower:
                substring_bonus += 0.15

        score = min(jaccard + substring_bonus + name_bonus, 1.0)
        if intersection:
            reasons.append(f"关键词命中：{', '.join(sorted(intersection))}")

        return score, "; ".join(reasons) if reasons else "低相关度"

    @staticmethod
    def _extract_keywords(text: str) -> set[str]:
        """从文本中提取中文和英文关键词。"""
        cn_words = re.findall(r"[一-鿿]{2,4}", text)
        en_words = re.findall(r"[a-zA-Z]{3,}", text)
        return set(cn_words) | {w.lower() for w in en_words}

    # ── 渐进式披露（Pi 风格）──────────────────────────

    @staticmethod
    def format_skills_for_prompt(skills: list[SkillMeta]) -> str:
        """将所有 Skill 的元数据以 XML 格式输出，供上下文消息使用。

        Pi 风格渐进式披露：只列出 name + description + location，
        AI 自己用 read_file 工具加载需要的 SKILL.md 全文。
        disable_model_invocation=True 的技能不列出。
        """
        visible = [s for s in skills if not s.disable_model_invocation]
        if not visible:
            return ""

        lines = [
            "",
            "以下 Skill 提供了针对特定任务的专用指令。"
            "当你判断当前任务匹配某个 Skill 的描述时，"
            "请使用 read_file 工具加载对应的 SKILL.md 文件，"
            "然后严格遵循其中的指令执行。"
            "Skill 文件中引用的相对路径应相对于 SKILL.md 所在目录解析。",
            "",
            "<available_skills>",
        ]
        for s in visible:
            lines.append("  <skill>")
            lines.append(f"    <name>{SkillManager._escape_xml(s.name)}</name>")
            lines.append(f"    <description>{SkillManager._escape_xml(s.description)}</description>")
            lines.append(f"    <location>{SkillManager._escape_xml(str(s.source_path))}</location>")
            lines.append("  </skill>")
        lines.append("</available_skills>")
        return "\n".join(lines)

    # ── 注入（手动调用 /skill:name 时使用）─────────────

    def inject(self, matches: list[SkillMatchResult], system_prompt: str) -> str:
        """兼容旧调用：不再把 Skill 指令注入 system prompt。

        Skill 元数据和正文现在由 `agent_prompt_context` 构造为独立上下文消息，
        避免污染稳定 system prompt。保留此方法是为了外部旧补丁点不报错。
        """
        _ = matches
        return system_prompt

    @staticmethod
    def _escape_xml(text: str) -> str:
        return text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;") \
            .replace('"', "&quot;").replace("'", "&apos;")

    # ── 加载 ──────────────────────────────────────────

    def _load_skill(self, meta: SkillMeta) -> Skill:
        """获取完整 Skill 正文，优先使用缓存的 body。"""
        body = self._body_cache.get(meta.name, "")
        return Skill(meta=meta, body=body)

    # ── 查询 ──────────────────────────────────────────

    def list_all(self) -> list[SkillMeta]:
        """返回所有已索引的 Skill 元数据，供 /skills 命令使用。"""
        return sorted(self._index.values(), key=lambda m: m.name)

    def get(self, name: str) -> Skill | None:
        """按名称获取完整 Skill。"""
        meta = self._index.get(name)
        if meta is None:
            return None
        return self._load_skill(meta)

    def get_diagnostics(self) -> list[SkillDiagnostic]:
        """返回发现阶段收集的诊断信息。"""
        return list(self._diagnostics)

    @property
    def count(self) -> int:
        return len(self._index)
