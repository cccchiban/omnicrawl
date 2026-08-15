"""任务思维模式路由核心逻辑（dsh-routing-suite / dsh-router-standard 的 Python 移植）。

来源：``dsh-routing-suite/preset/preset/router-core.mjs``（v0.2.0，commit 42bd08d）。

行为学结论（21 点 × n=2 官方 API 实测）：模型在 react↔spec persona 轴上不是
连续可调的，而是坍缩为三个稳定带——spec [0, 0.15]、transition 陷阱带 [0.2, 0.45]、
react [0.5, 1.0]；另有一个 weak（内部路由）模式由模型按任务自行分类。
"""

from __future__ import annotations

import re
from typing import Any


MODE_SPEC = 0
MODE_MIXED = 0.3
MODE_REACT = 1
MODE_WEAK = "weak"

SPEC_PERSONA = "You are a helpful software engineer assistant."

MIXED_PERSONA = (
    "You are a helpful software engineer assistant.\n"
    "Work directly: prefer writing or editing code over describing plans. "
    "Verify your changes by reading and running them."
)

REACT_PERSONA = (
    "You are a hands-on software engineer who delivers working output fast.\n"
    "Work directly: write or edit code, then verify it by reading and running. "
    "Keep the loop tight — produce, verify, fix — and do not build test "
    "harnesses, scaffolding, or ceremony the user did not ask for. "
    "Finish with a usable deliverable and a short summary."
)

# weak 模式 persona 是模型相关的（P11/P24）：
# - Pro：spec 句 + 分类指令（w6c, +4.67）；不加 recall/converge 锚（P24）。
# - Flash：neutral + classify + recall/converge/anti-runaway 锚（w7, +5.67）。
WEAK_PRO = (
    "You are a helpful software engineer assistant.\n"
    "Before acting, decide the task type (build or fix) and adopt the matching "
    "style: build → hands-on production; fix → inspect-and-plan."
)

WEAK_FLASH = (
    "You are a helpful assistant.\n"
    "Before acting, decide the task type (build or fix) and adopt the matching "
    "style: build → hands-on production; fix → inspect-and-plan.\n"
    "Before acting, briefly review what you have already done in this session and "
    "continue from where you left off; do not repeat completed steps. "
    "Do not run environment checks (echo, whoami, uname, node --version, date) or "
    "exhaustive grep/glob scans.\n"
    "Think deeply first, then produce."
)

# 近距引导（weak 模式）：每条真实用户消息后追加一条固定引导消息。
# v19 深度自适应：简单任务快收敛；复杂任务深度探索、信息驱动停止。
GUIDE_WEAK = (
    "\nRouter: classify this task (build or fix) now, then adopt the matching style — "
    "build: direct production; fix: inspect-first. Think deeply first, then commit and act."
)

GUIDE_DEEP = (
    "\nRouter: classify this task (build or fix) now, then adopt the matching style — "
    "build: direct production; fix: inspect-first. Think deeply about the architecture, "
    "edge cases, and integration points. Do not spend reasoning on the environment or "
    "tooling. Produce when your information is complete. End each reasoning block with "
    "a decision or an information need."
)

# 复杂度启发式：长任务或架构/设计类任务视为复杂任务。
COMPLEX_RE = re.compile(
    r"(重构|架构|全面|详细|设计|系统|优化|分析|survey|overview|architecture|refactor|comprehensive|detailed|design|system|optimize|analyze)",
    re.IGNORECASE,
)

REACT_RE = re.compile(
    r"(开发|创建|写一个|生成|从零|做一个|游戏|网页|网站|构建|新项目|搭建|实现|做出|上线|落地|脚本|工具|应用|build|create|develop|generate|implement|make a|new project)",
    re.IGNORECASE,
)

SPEC_RE = re.compile(
    r"(修复|修一下|调试|重构|维护|排查|报错|出错|崩溃|优化|审查|review|fix|debug|refactor|maintain|repair|broken|break|为什么|异常|故障|迁移|升级|兼容)",
    re.IGNORECASE,
)


def count_hits(pattern: re.Pattern[str], text: str) -> int:
    """统计正则命中次数（等价于 JS 的 matchAll().length）。"""

    return len(pattern.findall(text or ""))


def is_complex_task(text: str) -> bool:
    """长任务或含架构/设计等关键词的任务视为复杂任务。"""

    return isinstance(text, str) and (len(text) > 120 or bool(COMPLEX_RE.search(text)))


def is_flash_model(model_id: Any) -> bool:
    """Flash 家族模型走 Flash 专用 weak persona。"""

    return isinstance(model_id, str) and bool(re.search(r"flash", model_id, re.IGNORECASE))


def clamp01(value: Any) -> float:
    """把任意数值收敛到 [0, 1]；非数值按 0 处理。"""

    try:
        number = float(value)
    except (TypeError, ValueError):
        number = 0.0
    return min(1.0, max(0.0, number))


def band_of(mode: Any) -> str:
    """把模式量化到四个实测行为带之一。"""

    if mode == MODE_WEAK:
        return "weak"
    value = clamp01(mode)
    if value < 0.2:
        return "spec"
    if value < 0.5:
        return "transition"
    return "react"


def persona_for(mode: Any, model_id: Any = None) -> str:
    """返回模式对应的 persona 文本。"""

    band = band_of(mode)
    if band == "spec":
        return SPEC_PERSONA
    if band == "transition":
        return MIXED_PERSONA
    if band == "weak":
        return WEAK_FLASH if is_flash_model(model_id) else WEAK_PRO
    return REACT_PERSONA


def core_for(mode: Any) -> list[str]:
    """首轮核心工具面（Host 目录过滤用）。

    weak 带使用 RL-shape 面：str_replace_editor + shell（shell 由运行时按平台加入）。
    """

    band = band_of(mode)
    if band == "spec":
        return ["read", "edit", "glob", "grep"]
    if band == "transition":
        return ["read", "edit", "write", "glob", "grep"]
    if band == "weak":
        return ["str_replace_editor"]
    return ["read", "write", "edit"]


def band_for(mode: Any) -> str:
    """人类可读带名；transition 显示为 mixed（陷阱带，仅显式 opt-in）。"""

    band = band_of(mode)
    return "mixed" if band == "transition" else band


def testiness_for(mode: Any) -> str:
    """测试抑制强度（信息性）。"""

    band = band_of(mode)
    if band == "react":
        return "suppressed"
    if band == "spec":
        return "normal"
    return "light"


def classify_task(text: str) -> int | str:
    """按关键词计数分类任务：react=1 / spec=0 / 打平或无命中=weak。"""

    react = count_hits(REACT_RE, text)
    spec = count_hits(SPEC_RE, text)
    if react > spec:
        return MODE_REACT
    if spec > react:
        return MODE_SPEC
    return MODE_WEAK


def extract_user_text(data: Any) -> str:
    """防御性解包 user/message 事件 payload。

    兼容 ``data.message.content`` 嵌套形状（issue #1），content 元素可以是字符串
    或 ``{text: ...}`` 对象。
    """

    if not data:
        return ""
    payload = data
    if isinstance(data, dict) and isinstance(data.get("message"), dict):
        payload = data["message"]
    if isinstance(payload, dict):
        content = payload.get("content")
    else:
        content = None
    if not isinstance(content, list):
        return ""
    parts: list[str] = []
    for item in content:
        if isinstance(item, str):
            parts.append(item)
        elif isinstance(item, dict):
            parts.append(str(item.get("text") or ""))
    return " ".join(part for part in parts if part)


def parse_mode(token: Any) -> int | float | str | None:
    """解析用户/Agent 提供的模式 token。

    接受带名（spec/spec-lean/weak/router/mixed/balanced/react/react-lean）、
    0-100 整数、0.0-1.0 小数、auto（清除 override）。
    """

    if token is None:
        return None
    raw = str(token).strip().lower()
    if not raw:
        return None
    if raw == "auto":
        return "auto"
    if raw in {"weak", "router"}:
        return MODE_WEAK
    if raw in {"spec", "spec-lean"}:
        return MODE_SPEC
    if raw in {"balanced", "mixed"}:
        return MODE_MIXED
    if raw in {"react", "react-lean"}:
        return MODE_REACT
    try:
        number = float(raw)
    except ValueError:
        return None
    if not (number == number):  # NaN
        return None
    if "." in raw:
        return clamp01(number)
    return clamp01(number / 100)


def fmt_mode(mode: Any) -> str:
    """显示用模式文本。"""

    return str(mode) if isinstance(mode, str) else f"{mode:.2f}"


def mode_from_session_events(events: Any) -> int | str:
    """从持久会话事件推导会话模式（resume-safe）。

    ``events`` 接受可迭代对象，元素需有 ``type`` 与 ``payload``/``data`` 属性；
    兼容 OmniCrawl ``SessionEvent``（payload 为 dict）。
    """

    for event in events or ():
        event_type = getattr(event, "type", None)
        if event_type != "user_message":
            continue
        payload = getattr(event, "payload", None)
        if isinstance(payload, dict) and payload.get("content"):
            return classify_task(str(payload["content"]))
    return MODE_WEAK


__all__ = [
    "COMPLEX_RE",
    "GUIDE_DEEP",
    "GUIDE_WEAK",
    "MODE_MIXED",
    "MODE_REACT",
    "MODE_SPEC",
    "MODE_WEAK",
    "REACT_PERSONA",
    "REACT_RE",
    "SPEC_PERSONA",
    "SPEC_RE",
    "WEAK_FLASH",
    "WEAK_PRO",
    "band_for",
    "band_of",
    "classify_task",
    "clamp01",
    "core_for",
    "count_hits",
    "extract_user_text",
    "fmt_mode",
    "is_complex_task",
    "is_flash_model",
    "mode_from_session_events",
    "parse_mode",
    "persona_for",
    "testiness_for",
]
