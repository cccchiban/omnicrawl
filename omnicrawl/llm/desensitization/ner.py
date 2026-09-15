"""NER 兜底层：BiLSTM-CRF 语义实体（人名 / 地名 / 机构名）识别与占位。

定位（设计稿 §5.6）：结构感知（键名）、值类型规则、熵兜底之外，还有一类「形态普通、
但语义上就是敏感信息」的值——人名 / 地名 / 机构名。正则与熵都判不出来（它们既不带
敏感键名、随机性也低），只有语义模型能覆盖。本层因此作为**所有脱敏处理之后的语义兜底**
接在 ``engine.mask_text`` 的最末端：输入是已经被前几层替换过的文本（命中值已是
``｛Desensitized:n｝`` 占位符），避免邮箱 / 手机号等其它类型数据干扰模型。

关键性质：

- **复用标准占位符机制**：本层只产出「实体区间」，由 ``engine`` 用
  ``MaskContext.placeholder_for`` 分配序号并替换；还原 / 流式 / 周期注销全部沿用既有
  ``registry`` / ``stream``，本层不新增任何还原路径。
- **模型能力边界**：只识别 PER / ORG / LOC（BIO），不识别邮箱 / 手机号 / 身份证号等
  结构化敏感信息——那些由值类型规则层负责。丢弃整体未落在中文片段内的实体（非中文字符已在入口被隔离）、默认丢弃单字实体，
  避免把拉丁字母编号或「日 / 美 / 京」这类歧义单字误当实体而改坏原文。
- **CUDA 优先、CPU 兜底**：``ner_device`` 取 ``auto``（默认）时优先 CUDA、不可用回退
  CPU；显式 ``cuda`` 在不可用或运行期 CUDA 出错时同样回退，保证可用性。
- **随对话增长不劣化**：结果缓存落在**块**（``iter_chunks`` 的打包结果，≤ ``max_seq_len``）
  这一粒度上，因此「只追加」的长文本只需推理新增的块、内容相同的块还能跨文本复用；
  纯 ASCII 文本 / 块直接短路，不进模型（代码 / JSON / 日志类工具结果零推理开销）。
  缓存容量默认从 256 提到 2048，且可配（``ner_cache_size``）：整段文本当键时，历史块数
  一旦超过容量，LRU 的顺序全扫会让命中率直接掉到 0（实测 300 块历史：容量 256 时
  命中率 0%、每轮 2.4s；容量 2048 时每轮 1.8ms）。
- **零成本降级**：torch / 模型缺失、模型损坏或配置非法时 ``build_ner_layer`` 返回
  None，本层静默跳过，不影响既有脱敏链路（与 ``maybe_wrap_runtime`` 的约定一致）。

模型权重随包分发（``models/bilstm_crf_best.pt``，可用 ``ner_model_path`` 或环境变量
``OMNICRAWL_NER_MODEL`` 覆盖）。
"""

from __future__ import annotations

import os
import threading
from collections import OrderedDict
from pathlib import Path
from typing import Sequence

from .registry import PLACEHOLDER_PATTERN

#: 支持脱敏的实体类型（与模型 BIO 标签一致）。
NER_ENTITY_TYPES: tuple[str, ...] = ("PER", "ORG", "LOC")

DEVICE_AUTO = "auto"
DEVICE_CPU = "cpu"
DEVICE_CUDA = "cuda"
NER_DEVICES: tuple[str, ...] = (DEVICE_AUTO, DEVICE_CPU, DEVICE_CUDA)

#: 随包分发的默认 checkpoint（训练产出：dev F1 0.868 / test micro-F1 0.863）。
PACKAGED_MODEL_FILENAME = "bilstm_crf_best.pt"
DEFAULT_MAX_SEQ_LEN = 200
DEFAULT_BATCH_TOKENS = 12000
#: 结果缓存容量，单位为**块**（每块 ≤ ``max_seq_len`` 字符）：默认值约覆盖 400KB 文本，
#: 足够让「历史重发」的整段对话常驻；``0`` 表示关闭缓存。与
#: ``config.features.desensitization.DEFAULT_NER_CACHE_SIZE`` 保持一致（配置层不反向
#: 依赖 llm 层，故分别声明，由测试守护）。
DEFAULT_CACHE_SIZE = 2048

#: 用于长文本切块的句末标点（与训练时语料切分口径一致）。
_SENT_END_CHARS = "。！？；!?;…\n\r"

_ENV_MODEL_PATH = "OMNICRAWL_NER_MODEL"


class NerModelError(RuntimeError):
    """模型不可用（torch 缺失 / 权重文件缺失或损坏）。"""


def resolve_model_path(configured: str | Path | None = None) -> Path:
    """解析 checkpoint 路径：显式配置 > 环境变量 > 随包默认。"""

    if configured:
        return Path(configured).expanduser()
    env = os.environ.get(_ENV_MODEL_PATH, "").strip()
    if env:
        return Path(env).expanduser()
    return Path(__file__).with_name("models") / PACKAGED_MODEL_FILENAME


def resolve_device(requested: str | None = None) -> str:
    """设备选择：``cpu`` 恒为 CPU；``auto`` / ``cuda`` 优先 CUDA、不可用回退 CPU。"""

    normalized = (requested or DEVICE_AUTO).strip().lower()
    if normalized not in NER_DEVICES:
        normalized = DEVICE_AUTO
    if normalized == DEVICE_CPU:
        return DEVICE_CPU
    try:
        from . import ner_model

        if ner_model.TORCH_AVAILABLE and ner_model.torch.cuda.is_available():
            return DEVICE_CUDA
    except Exception:  # noqa: BLE001 - 探测失败一律按 CPU 处理
        pass
    return DEVICE_CPU


#: 中文姓名内部连接符：随中文片段一起进入模型，也允许出现在实体区间内。
_CHINESE_CONNECTORS = "·・"

#: 隔离用的分隔符：与被替换字符等长，保证模型返回的偏移与原文一一对应。
_CHINESE_ISOLATION_SEPARATOR = " "


def _is_chinese_char(char: str) -> bool:
    """是否属于兜底层保留的字符：汉字与中文姓名连接符。"""

    return "一" <= char <= "鿿" or char in _CHINESE_CONNECTORS


def _isolate_chinese(text: str) -> str:
    """中文片段隔离：非中文字符等长替换为分隔符，只让中文片段进入模型。

    长度不变，因此模型返回的偏移可直接用于原文；命中是否合法再由出口
    ``_is_chinese_span`` 复核，跨片段实体不会成立。
    """

    if not text:
        return text
    return "".join(
        char if _is_chinese_char(char) else _CHINESE_ISOLATION_SEPARATOR
        for char in text
    )


def _is_chinese_span(value: str) -> bool:
    """实体区间是否整体落在中文片段内（至少含一个汉字，其余只能是连接符）。"""

    if not value or not _has_cjk(value):
        return False
    return all(_is_chinese_char(char) for char in value)


def _has_cjk(text: str) -> bool:
    """是否含中日韩汉字（模型对纯拉丁片段的误报多来自邮箱 / 网址 / 编号）。"""

    return any("\u4e00" <= ch <= "\u9fff" for ch in text)


def _split_units(text: str, max_len: int) -> list[str]:
    """先按句末标点切句；超长句再按 ``max_len`` 硬切（与训练时切分口径一致）。"""

    units: list[str] = []
    start = 0
    for index, char in enumerate(text):
        if char in _SENT_END_CHARS:
            units.append(text[start : index + 1])
            start = index + 1
    if start < len(text):
        units.append(text[start:])

    out: list[str] = []
    for unit in units:
        if not unit:
            continue
        if len(unit) <= max_len:
            out.append(unit)
        else:
            out.extend(unit[k : k + max_len] for k in range(0, len(unit), max_len))
    return out


def iter_chunks(text: str, max_len: int):
    """把句子单元贪心打包成不超过 ``max_len`` 的块，保持原文顺序。"""

    buffer = ""
    for unit in _split_units(text, max_len):
        if buffer and len(buffer) + len(unit) > max_len:
            yield buffer
            buffer = ""
        buffer += unit
    if buffer:
        yield buffer


def extract_entities(characters: str, tags: Sequence[str]) -> list[tuple[int, int, str]]:
    """BIO 标签序列 → 实体区间 ``[(start, end, type), ...]``（半开区间）。"""

    entities: list[tuple[int, int, str]] = []
    start = -1
    current = ""
    for index, tag in enumerate(tags):
        if tag.startswith("B-"):
            if current:
                entities.append((start, index, current))
            start, current = index, tag[2:]
        elif tag.startswith("I-"):
            entity_type = tag[2:]
            if current != entity_type:
                # 非法 I-x：当作新的实体起点（模型受约束后基本不会出现）。
                if current:
                    entities.append((start, index, current))
                start, current = index, entity_type
        else:
            if current:
                entities.append((start, index, current))
            start, current = -1, ""
    if current:
        entities.append((start, len(characters), current))
    return entities


class NerExtractor:
    """加载 checkpoint 并对文本做实体抽取（CUDA 优先、CPU 兜底）。"""

    def __init__(
        self,
        model_path: str | Path | None = None,
        *,
        device: str | None = None,
        max_seq_len: int = DEFAULT_MAX_SEQ_LEN,
        batch_tokens: int = DEFAULT_BATCH_TOKENS,
        cache_size: int = DEFAULT_CACHE_SIZE,
    ) -> None:
        self.model_path = resolve_model_path(model_path)
        self.requested_device = (device or DEVICE_AUTO).strip().lower()
        self.max_seq_len = max(1, int(max_seq_len))
        self.batch_tokens = max(1, int(batch_tokens))
        self._cache_size = max(0, int(cache_size))
        # 键是**块**文本（不是整段文本），值是该块内的实体区间（偏移相对块首）。
        self._cache: "OrderedDict[str, tuple[tuple[int, int, str], ...]]" = OrderedDict()
        self._cache_lock = threading.Lock()
        self._infer_lock = threading.Lock()
        self.device = DEVICE_CPU
        self.hits = 0
        self.misses = 0
        self.ascii_skips = 0
        self.inferred_chunks = 0
        self._load()

    # ── 加载 ────────────────────────────────────────────────────────────
    def _load(self) -> None:
        from . import ner_model

        if not ner_model.TORCH_AVAILABLE:
            raise NerModelError("未安装可选依赖 torch，NER 兜底层不可用。")
        if not self.model_path.is_file():
            raise NerModelError(f"NER 模型权重不存在：{self.model_path}")

        torch = ner_model.torch
        checkpoint = torch.load(
            str(self.model_path), map_location="cpu", weights_only=False
        )
        config = checkpoint["model_config"]
        self.tags: tuple[str, ...] = tuple(checkpoint.get("tags") or ner_model.DEFAULT_TAGS)
        self.char2idx: dict[str, int] = dict(checkpoint["char2idx"])
        self.meta: dict = dict(checkpoint.get("meta") or {})
        self.pad_id = int(self.char2idx.get("<pad>", 0))
        self.unk_id = int(self.char2idx.get("<unk>", 1))

        self.model = ner_model.BiLSTMCRF(
            vocab_size=int(config["vocab_size"]),
            num_tags=int(config["num_tags"]),
            tags=self.tags,
            embed_dim=int(config["embed_dim"]),
            hidden_dim=int(config["hidden_dim"]),
            num_layers=int(config["num_layers"]),
            dropout=float(config["dropout"]),
            pad_idx=self.pad_id,
            use_constraints=bool(config.get("use_constraints", True)),
        )
        self.model.load_state_dict(checkpoint["state_dict"])
        self._to_device(resolve_device(self.requested_device))

    def _to_device(self, device: str) -> None:
        """把模型搬到目标设备；CUDA 失败时回退 CPU（可用性优先）。"""

        try:
            self.model.to(device)
        except Exception:  # noqa: BLE001 - 显存不足 / 驱动异常等一律回退 CPU
            self.model.to(DEVICE_CPU)
            device = DEVICE_CPU
        self.model.eval()
        self.device = device

    # ── 抽取 ────────────────────────────────────────────────────────────
    def find_entities(self, text: str) -> list[tuple[int, int, str]]:
        """返回实体区间 ``[(start, end, type), ...]``（按出现位置排序）。

        缓存粒度是**块**而非整段文本：``iter_chunks`` 是贪心前缀打包，只追加 / 只在尾部
        增长的长文本，其已有块逐字不变，因此只有新增的块需要推理；内容相同的块还能
        跨文本复用。
        """

        if not text:
            return []
        if not _has_cjk(text):
            # 纯 ASCII 文本不可能产出本层需要的实体（过滤阶段本就丢弃不含汉字的实体）：
            # 直接短路，省掉切块、查键与前向推理。
            self.ascii_skips += 1
            return []

        entities: list[tuple[int, int, str]] = []
        missing: list[tuple[int, str]] = []
        offset = 0
        for chunk in iter_chunks(text, self.max_seq_len):
            cached = self._cache_get(chunk)
            if cached is not None:
                entities.extend(
                    (offset + start, offset + end, entity_type)
                    for start, end, entity_type in cached
                )
            elif _has_cjk(chunk):
                missing.append((offset, chunk))
            # 纯 ASCII 块同样不可能有实体：跳过且不进缓存——扫描远比推理便宜，
            # 让它们占位只会挤掉真正有价值的块。
            offset += len(chunk)

        if missing:
            inferred = self._infer_chunks([chunk for _offset, chunk in missing])
            for (chunk_offset, chunk), chunk_entities in zip(missing, inferred):
                self._cache_put(chunk, tuple(chunk_entities))
                entities.extend(
                    (chunk_offset + start, chunk_offset + end, entity_type)
                    for start, end, entity_type in chunk_entities
                )
        entities.sort(key=lambda item: item[0])
        return entities

    def _infer_chunks(self, chunks: list[str]) -> list[list[tuple[int, int, str]]]:
        """对多个块做一次批量前向，返回每块的实体（偏移相对块首，顺序与输入一致）。"""

        if not chunks:
            return []
        self.inferred_chunks += len(chunks)
        # 隔离：非中文字符等长替换为分隔符，模型只看到中文片段。
        encoded = [
            [
                self.char2idx.get(char, self.unk_id)
                for char in _isolate_chinese(chunk)
            ]
            for chunk in chunks
        ]
        results: list[list[tuple[int, int, str]]] = [[] for _ in chunks]
        with self._infer_lock:
            for batch_indices in self._iter_batches([len(ids) for ids in encoded]):
                batch = [encoded[index] for index in batch_indices]
                paths = self._run_batch(batch)
                for local, job_index in enumerate(batch_indices):
                    tags = [
                        self.tags[tag] if 0 <= tag < len(self.tags) else "O"
                        for tag in paths[local]
                    ]
                    results[job_index] = extract_entities(chunks[job_index], tags)
        return results

    def _iter_batches(self, lengths: list[int]):
        """按 token 预算动态分批（减少 padding 浪费），不改动结果顺序。"""

        batch: list[int] = []
        batch_max = 0
        for index, length in enumerate(lengths):
            new_max = max(batch_max, length)
            if batch and new_max * (len(batch) + 1) > self.batch_tokens:
                yield batch
                batch, batch_max = [], 0
                new_max = length
            batch.append(index)
            batch_max = new_max
        if batch:
            yield batch

    def _run_batch(self, batch: list[list[int]]) -> list[list[int]]:
        from . import ner_model

        torch = ner_model.torch
        lengths = [len(ids) for ids in batch]
        width = max(lengths)
        input_ids = torch.full((len(batch), width), self.pad_id, dtype=torch.long)
        mask = torch.zeros((len(batch), width), dtype=torch.float)
        for row, ids in enumerate(batch):
            input_ids[row, : len(ids)] = torch.tensor(ids, dtype=torch.long)
            mask[row, : len(ids)] = 1.0
        input_ids = input_ids.to(self.device)
        mask = mask.to(self.device)
        try:
            return self.model.predict(input_ids, mask)
        except RuntimeError as exc:
            if self.device == DEVICE_CUDA and "CUDA" in str(exc).upper():
                # 运行期 CUDA 出错（显存 / 驱动）：回退 CPU 重试一次。
                self._to_device(DEVICE_CPU)
                input_ids = input_ids.to(DEVICE_CPU)
                mask = mask.to(DEVICE_CPU)
                return self.model.predict(input_ids, mask)
            raise

    # ── 结果缓存（键是块，未变历史在后续请求中不重复推理） ──────────────
    def _cache_get(self, text: str):
        if self._cache_size <= 0:
            return None
        with self._cache_lock:
            entry = self._cache.get(text)
            if entry is None:
                self.misses += 1
                return None
            self._cache.move_to_end(text)
            self.hits += 1
            return entry

    def _cache_put(self, text: str, entities: tuple[tuple[int, int, str], ...]) -> None:
        if self._cache_size <= 0:
            return
        with self._cache_lock:
            self._cache[text] = entities
            self._cache.move_to_end(text)
            while len(self._cache) > self._cache_size:
                self._cache.popitem(last=False)

    def cache_stats(self) -> dict[str, int]:
        """缓存计数。

        ``hits`` / ``misses`` 按**块**计（含被 ASCII 短路跳过的块——它们查键未命中
        但未推理）；``inferred_chunks`` 是真正进过模型的块数（性能观测的准确口径），
        ``ascii_skips`` 是按文本计的短路次数。
        """

        with self._cache_lock:
            return {
                "entries": len(self._cache),
                "hits": self.hits,
                "misses": self.misses,
                "inferred_chunks": self.inferred_chunks,
                "ascii_skips": self.ascii_skips,
            }


def _placeholder_spans(text: str) -> list[tuple[int, int]]:
    """既有占位符的区间（保护前几层的替换结果不被本层改写）。"""

    return [
        (match.start(), match.end()) for match in PLACEHOLDER_PATTERN.finditer(text)
    ]


class NerLayer:
    """在抽取结果之上做「过滤 + 占位符保护」的兜底层。

    过滤规则（宁少勿滥，避免改坏原文）：

    - 只保留配置允许的实体类型；
    - 丢弃整体未落在中文片段内的实体（入口隔离后模型只见中文，含拉丁字母的误报不成立）；
    - 丢弃长度小于 ``min_entity_chars`` 的实体（默认 2，规避单字地名歧义）；
    - 丢弃与已有 ``｛Desensitized:n｝`` 占位符重叠的实体（前几层的结果不参与改写）。
    """

    def __init__(
        self,
        extractor: NerExtractor,
        *,
        entity_types: Sequence[str] = NER_ENTITY_TYPES,
        min_entity_chars: int = 2,
    ) -> None:
        self._extractor = extractor
        selected = tuple(
            item.strip().upper()
            for item in entity_types
            if str(item).strip()
        )
        self._types = frozenset(selected) or frozenset(NER_ENTITY_TYPES)
        self._min_entity_chars = max(1, int(min_entity_chars))

    @property
    def device(self) -> str:
        return self._extractor.device

    @property
    def extractor(self) -> NerExtractor:
        return self._extractor

    def find_spans(self, text: str) -> list[tuple[int, int]]:
        """返回需要占位的实体区间（已过滤、已剔除占位符重叠）。

        占位符扫描是**惰性**的：无候选实体时完全不扫（绝大多数文本块走这条路径）；
        实体与占位符都按位置有序，重叠判定用单指针线性推进，不退回 O(实体×占位符)。
        """

        if not text:
            return []
        entities = self._extractor.find_entities(text)
        if not entities:
            return []
        spans: list[tuple[int, int]] = []
        placeholders: list[tuple[int, int]] | None = None
        pointer = 0
        for start, end, entity_type in entities:
            if entity_type not in self._types:
                continue
            if end <= start or end > len(text):
                continue
            value = text[start:end]
            if len(value) < self._min_entity_chars or not _is_chinese_span(value):
                continue
            if placeholders is None:
                placeholders = _placeholder_spans(text)
            while pointer < len(placeholders) and placeholders[pointer][1] <= start:
                pointer += 1
            if (
                pointer < len(placeholders)
                and placeholders[pointer][0] < end
                and start < placeholders[pointer][1]
            ):
                continue
            spans.append((start, end))
        return spans


# ── 共享实例与构建入口 ────────────────────────────────────────────────────
#: 共享实例键含缓存容量：改容量即换一个抽取器，避免旧容量静默生效。
_EXTRACTORS: dict[tuple[str, str, int], NerExtractor] = {}
_EXTRACTOR_LOCK = threading.Lock()


def get_shared_extractor(
    model_path: str | Path | None = None,
    device: str | None = None,
    cache_size: int = DEFAULT_CACHE_SIZE,
) -> NerExtractor:
    """按 (路径, 设备, 缓存容量) 复用抽取器：模型只在首次启用时加载一次。"""

    key = (
        str(resolve_model_path(model_path)),
        (device or DEVICE_AUTO).strip().lower(),
        max(0, int(cache_size)),
    )
    with _EXTRACTOR_LOCK:
        extractor = _EXTRACTORS.get(key)
        if extractor is None:
            extractor = NerExtractor(model_path, device=device, cache_size=cache_size)
            _EXTRACTORS[key] = extractor
        return extractor


def build_ner_layer(config: object) -> NerLayer | None:
    """按脱敏配置构建 NER 兜底层；未启用或环境不满足时返回 None（静默降级）。"""

    try:
        if not getattr(config, "ner_enabled", False):
            return None
        from . import ner_model

        if not ner_model.TORCH_AVAILABLE:
            return None
        extractor = get_shared_extractor(
            getattr(config, "ner_model_path", "") or None,
            getattr(config, "ner_device", DEVICE_AUTO),
            getattr(config, "ner_cache_size", DEFAULT_CACHE_SIZE),
        )
        return NerLayer(
            extractor,
            entity_types=getattr(config, "ner_entity_types", NER_ENTITY_TYPES)
            or NER_ENTITY_TYPES,
            min_entity_chars=getattr(config, "ner_min_entity_chars", 2),
        )
    except Exception:  # noqa: BLE001 - 模型相关异常不得影响脱敏链路可用性
        return None


def clear_ner_cache() -> None:
    """清空共享抽取器与其结果缓存（测试 / 配置变更后使用）。"""

    with _EXTRACTOR_LOCK:
        _EXTRACTORS.clear()


__all__ = [
    "DEFAULT_BATCH_TOKENS",
    "DEFAULT_CACHE_SIZE",
    "DEFAULT_MAX_SEQ_LEN",
    "DEVICE_AUTO",
    "DEVICE_CPU",
    "DEVICE_CUDA",
    "NER_DEVICES",
    "NER_ENTITY_TYPES",
    "PACKAGED_MODEL_FILENAME",
    "NerExtractor",
    "NerLayer",
    "NerModelError",
    "build_ner_layer",
    "clear_ner_cache",
    "extract_entities",
    "get_shared_extractor",
    "iter_chunks",
    "resolve_device",
    "resolve_model_path",
]
