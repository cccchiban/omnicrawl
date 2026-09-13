"""AI 消息脱敏设置（可内嵌右侧的 Pane + 整屏薄壳）。

对应 ``omnicrawl/docs/agent_gateway_desensitization_design.md``：编辑
config.toml 的 ``[desensitization]`` 段。该配置在构建模型运行时
（``build_runtime``）读取，保存后需切换模型或重启 TUI 生效。
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Optional

from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Container, Horizontal, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import Button, Input, Select, Static

from ....config.features.desensitization import (
    DesensitizationConfig,
    DesensitizationConfigError,
    load_desensitization_config,
    save_desensitization_config,
)
from ..terminal.theme import terminal_css, terminal_select_css
from .panes import SettingsPane

_PANE_CSS = """
#desensitization-form { height: 1fr; }
.desensitization-label { height: 1; color: $terminal-text-muted; }
.desensitization-control { height: 3; margin-bottom: 1; }
#desensitization-status { height: 2; color: $terminal-white; }
#desensitization-actions { height: 3; align-horizontal: right; }
"""


class DesensitizationSettingsPane(SettingsPane):
    """消息脱敏二级面板：编辑 ``[desensitization]`` 配置，保存后返回左侧。"""

    BINDINGS = [
        Binding("escape", "cancel", "返回", priority=True),
        Binding("ctrl+s", "save", "保存", priority=True),
    ]

    DEFAULT_CSS = terminal_css(_PANE_CSS + terminal_select_css())

    def __init__(
        self,
        config_path: str | Path | None = None,
        *,
        agent: Any | None = None,
        configuration: DesensitizationConfig | None = None,
    ) -> None:
        super().__init__(agent=agent)
        self._config_path = Path(config_path) if config_path is not None else None
        self._configuration = configuration or (
            load_desensitization_config(self._config_path)
            if self._config_path is not None
            else DesensitizationConfig()
        )

    def compose_pane(self) -> ComposeResult:
        c = self._configuration
        with VerticalScroll(id="desensitization-form"):
            yield Static("脱敏功能总开关", classes="desensitization-label")
            yield Select(
                [("停用", False), ("启用", True)],
                value=c.enabled,
                allow_blank=False,
                id="desensitization-enabled",
                classes="desensitization-control choice-select",
            )
            yield Static("屏蔽异常时中止请求（fail-closed）", classes="desensitization-label")
            yield Select(
                [("开启（不静默发送原文）", True), ("关闭（降级发送原文并告警）", False)],
                value=c.fail_closed,
                allow_blank=False,
                id="desensitization-fail-closed",
                classes="desensitization-control choice-select",
            )
            yield Static("还原缺失时中止并报错（严格还原）", classes="desensitization-label")
            yield Select(
                [("关闭（保留占位符并告警）", False), ("开启（中止并报错）", True)],
                value=c.strict_restore,
                allow_blank=False,
                id="desensitization-strict-restore",
                classes="desensitization-control choice-select",
            )
            yield Static("熵检测兜底", classes="desensitization-label")
            yield Select(
                [("开启", True), ("关闭（仅键名 / 结构匹配）", False)],
                value=c.entropy_enabled,
                allow_blank=False,
                id="desensitization-entropy-enabled",
                classes="desensitization-control choice-select",
            )
            yield Static("纯字母令牌：长度达标即脱敏（默认关闭）", classes="desensitization-label")
            yield Select(
                [("关闭", False), ("开启", True)],
                value=c.entropy_pure_letters,
                allow_blank=False,
                id="desensitization-entropy-pure-letters",
                classes="desensitization-control choice-select",
            )
            yield Static("纯数字令牌：长度达标即脱敏（默认关闭）", classes="desensitization-label")
            yield Select(
                [("关闭", False), ("开启", True)],
                value=c.entropy_pure_digits,
                allow_blank=False,
                id="desensitization-entropy-pure-digits",
                classes="desensitization-control choice-select",
            )
            for label, widget_id, value in (
                ("熵兜底长度下限", "desensitization-entropy-min-length", c.entropy_min_length),
                ("熵兜底阈值（0–8）", "desensitization-entropy-min-bits", c.entropy_min_bits),
                (
                    "追加敏感键名（逗号分隔，可留空）",
                    "desensitization-extra-keys",
                    ", ".join(c.extra_sensitive_keys),
                ),
                (
                    "豁免键名（逗号分隔，可留空）",
                    "desensitization-exempt-keys",
                    ", ".join(c.exempt_keys),
                ),
            ):
                yield Static(label, classes="desensitization-label")
                yield Input(str(value), id=widget_id, classes="desensitization-control")
            for label, widget_id, value in (
                ("PEM 私钥（BEGIN/END PRIVATE KEY）", "desensitization-detect-pem-private-key", c.detect_pem_private_key),
                ("数据库连接串", "desensitization-detect-db-connection-string", c.detect_db_connection_string),
                ("邮箱地址", "desensitization-detect-email", c.detect_email),
                ("银行卡号", "desensitization-detect-bank-card", c.detect_bank_card),
                ("内网 IP", "desensitization-detect-internal-ip", c.detect_internal_ip),
                ("外网 IP", "desensitization-detect-external-ip", c.detect_external_ip),
                ("网址（http / https / ftp）", "desensitization-detect-url", c.detect_url),
                ("MAC 地址", "desensitization-detect-mac-address", c.detect_mac_address),
                ("中国大陆车牌", "desensitization-detect-license-plate", c.detect_license_plate),
                ("gitleaks 开源规则", "desensitization-gitleaks-enabled", c.gitleaks_enabled),
            ):
                yield Static(label, classes="desensitization-label")
                yield Select(
                    [("关闭", False), ("开启", True)],
                    value=value,
                    allow_blank=False,
                    id=widget_id,
                    classes="desensitization-control choice-select",
                )
            yield Static(
                "自定义 gitleaks.toml 路径（可留空；按规则 id 覆盖 / 追加）",
                classes="desensitization-label",
            )
            yield Input(
                c.gitleaks_config_path,
                id="desensitization-gitleaks-config-path",
                classes="desensitization-control",
            )
            yield Static(
                "NER 兜底层（人名 / 地名 / 机构名；需可选依赖 torch，缺失时自动跳过）",
                classes="desensitization-label",
            )
            yield Select(
                [("关闭", False), ("开启", True)],
                value=c.ner_enabled,
                allow_blank=False,
                id="desensitization-ner-enabled",
                classes="desensitization-control choice-select",
            )
            yield Static(
                "NER 推理设备（auto 优先 CUDA、不可用回退 CPU）",
                classes="desensitization-label",
            )
            yield Select(
                [("auto", "auto"), ("cpu", "cpu"), ("cuda", "cuda")],
                value=c.ner_device,
                allow_blank=False,
                id="desensitization-ner-device",
                classes="desensitization-control choice-select",
            )
            yield Static(
                "NER 模型路径（可留空；用随包权重或环境变量 OMNICRAWL_NER_MODEL）",
                classes="desensitization-label",
            )
            yield Input(
                c.ner_model_path,
                id="desensitization-ner-model-path",
                classes="desensitization-control",
            )
            for label, widget_id, value in (
                (
                    "NER 实体类型（逗号分隔，可选 PER / ORG / LOC）",
                    "desensitization-ner-entity-types",
                    ", ".join(c.ner_entity_types),
                ),
                (
                    "NER 最小实体长度（2 规避单字地名歧义）",
                    "desensitization-ner-min-entity-chars",
                    c.ner_min_entity_chars,
                ),
                (
                    "NER 推理结果缓存容量（单位「块」，0 关闭）",
                    "desensitization-ner-cache-size",
                    c.ner_cache_size,
                ),
            ):
                yield Static(label, classes="desensitization-label")
                yield Input(str(value), id=widget_id, classes="desensitization-control")
            yield Static(
                "保存后需切换模型或重启 TUI 生效；当前运行中的模型运行时不变。",
                id="desensitization-status",
            )
        with Horizontal(id="desensitization-actions"):
            yield Static("", classes="pane-save-hint")
            yield Button("取消", id="desensitization-cancel")
            yield Button("保存", variant="primary", id="desensitization-save")

    def on_button_pressed(self, event: Button.Pressed) -> None:
        if event.button.id == "desensitization-save":
            self.action_save()
        elif event.button.id == "desensitization-cancel":
            self.action_exit()

    def activate(self) -> None:
        self.query_one("#desensitization-enabled", Select).focus()

    def action_cancel(self) -> None:
        self.request_back()

    def action_exit(self) -> None:
        """“取消”按钮：直接关闭整个设置面板。"""
        self.request_exit()

    def action_save(self) -> None:
        try:
            configuration = DesensitizationConfig(
                enabled=self._read_bool("desensitization-enabled"),
                fail_closed=self._read_bool("desensitization-fail-closed"),
                strict_restore=self._read_bool("desensitization-strict-restore"),
                extra_sensitive_keys=self._read_keys("desensitization-extra-keys"),
                exempt_keys=self._read_keys("desensitization-exempt-keys"),
                entropy_enabled=self._read_bool("desensitization-entropy-enabled"),
                entropy_min_length=self._read_int("desensitization-entropy-min-length"),
                entropy_min_bits=self._read_float("desensitization-entropy-min-bits"),
                entropy_pure_letters=self._read_bool("desensitization-entropy-pure-letters"),
                entropy_pure_digits=self._read_bool("desensitization-entropy-pure-digits"),
                detect_pem_private_key=self._read_bool("desensitization-detect-pem-private-key"),
                detect_db_connection_string=self._read_bool(
                    "desensitization-detect-db-connection-string"
                ),
                detect_email=self._read_bool("desensitization-detect-email"),
                detect_bank_card=self._read_bool("desensitization-detect-bank-card"),
                detect_internal_ip=self._read_bool("desensitization-detect-internal-ip"),
                detect_external_ip=self._read_bool("desensitization-detect-external-ip"),
                detect_url=self._read_bool("desensitization-detect-url"),
                detect_mac_address=self._read_bool("desensitization-detect-mac-address"),
                detect_license_plate=self._read_bool("desensitization-detect-license-plate"),
                gitleaks_enabled=self._read_bool("desensitization-gitleaks-enabled"),
                gitleaks_config_path=self.query_one(
                    "#desensitization-gitleaks-config-path", Input
                ).value.strip(),
                ner_enabled=self._read_bool("desensitization-ner-enabled"),
                ner_device=str(
                    self.query_one("#desensitization-ner-device", Select).value
                ),
                ner_model_path=self.query_one(
                    "#desensitization-ner-model-path", Input
                ).value.strip(),
                ner_entity_types=self._read_keys("desensitization-ner-entity-types"),
                ner_min_entity_chars=self._read_int(
                    "desensitization-ner-min-entity-chars"
                ),
                ner_cache_size=self._read_int("desensitization-ner-cache-size"),
            )
            if self._config_path is not None:
                save_desensitization_config(configuration, self._config_path)
        except (DesensitizationConfigError, OSError, ValueError) as exc:
            self.query_one("#desensitization-status", Static).update(f"保存失败：{exc}")
            return
        self.flash_save_hint()
        self.commit(configuration)

    def _read_int(self, widget_id: str) -> int:
        return int(self.query_one(f"#{widget_id}", Input).value.strip())

    def _read_float(self, widget_id: str) -> float:
        return float(self.query_one(f"#{widget_id}", Input).value.strip())

    def _read_bool(self, widget_id: str) -> bool:
        return bool(self.query_one(f"#{widget_id}", Select).value)

    def _read_keys(self, widget_id: str) -> tuple[str, ...]:
        return tuple(
            item.strip()
            for item in self.query_one(f"#{widget_id}", Input).value.split(",")
            if item.strip()
        )


class DesensitizationSettingsScreen(ModalScreen[Optional[DesensitizationConfig]]):
    """整屏薄壳：内嵌 DesensitizationSettingsPane，保存 dismiss 配置、Esc 取消。"""

    BINDINGS = [
        Binding("escape", "cancel", "取消"),
        Binding("ctrl+s", "save", "保存", priority=True),
    ]

    CSS = terminal_css("""
    DesensitizationSettingsScreen { align: center middle; background: $terminal-overlay; }
    #desensitization-dialog { width: 96; max-width: 96%; height: 52; max-height: 95%; padding: 1 2; border: round $terminal-border-strong; background: $terminal-surface; }
    #desensitization-title { height: 1; margin-bottom: 1; color: $terminal-white; text-style: bold; }
    """ + _PANE_CSS + terminal_select_css())

    def __init__(
        self,
        config_path: str | Path,
        *,
        configuration: DesensitizationConfig | None = None,
    ) -> None:
        super().__init__()
        self._config_path = Path(config_path)
        self._configuration = configuration
        self._pane: Optional[DesensitizationSettingsPane] = None

    def compose(self) -> ComposeResult:
        with Container(id="desensitization-dialog"):
            yield Static("AI 消息脱敏", id="desensitization-title")
            self._pane = DesensitizationSettingsPane(
                self._config_path,
                configuration=self._configuration,
            )
            self._pane.bind_pane_events(
                on_back=lambda: self.dismiss(None),
                on_commit=lambda result: self.dismiss(result),
                on_exit=lambda: self.dismiss(None),
            )
            yield self._pane

    def on_mount(self) -> None:
        if self._pane is not None:
            self._pane.activate()

    def action_cancel(self) -> None:
        self.dismiss(None)

    def action_save(self) -> None:
        if self._pane is not None:
            self._pane.action_save()


__all__ = ["DesensitizationSettingsPane", "DesensitizationSettingsScreen"]
