"""BiLSTM-CRF 模型结构（PER / ORG / LOC），NE 兜底层的推理内核。

移植自 ``NER_BiLSTM_CRF/src/model.py``（人民日报语料、字符级、手写 CRF）；差别：

- 标签集从 checkpoint 读取（``tags``）而非全局 ``config``，因此本模块不依赖外部
  工程目录，可随包分发；
- 本模块**只在真的要用模型时才被导入**（``ner.py`` 内部延迟导入）。torch 是可选
  依赖，缺失时本模块导入即失败，而 ``ner.py`` 会据此把 NER 兜底层静默跳过。

CRF 为手写实现，包含前向归一化（log-sum-exp）、Viterbi 解码与非法 BIO 转移约束；
正确性由 ``tests/test_desensitization_ner.py`` 回归守护。
"""

from __future__ import annotations

from typing import Sequence

try:  # pragma: no cover - 取决于运行环境是否安装可选依赖 torch
    import torch
    import torch.nn as nn

    TORCH_AVAILABLE = True
except Exception:  # noqa: BLE001 - torch 缺失只是「本层不可用」，不是错误
    torch = None  # type: ignore[assignment]
    nn = None  # type: ignore[assignment]
    TORCH_AVAILABLE = False


NEG_INF = -1e4

# 与 checkpoint ``tags`` 一致的 BIO 标签集（顺序即标签 id）。
DEFAULT_TAGS: tuple[str, ...] = (
    "O",
    "B-PER",
    "I-PER",
    "B-ORG",
    "I-ORG",
    "B-LOC",
    "I-LOC",
)


if TORCH_AVAILABLE:

    class CRF(nn.Module):
        """线性链 CRF（一阶）；``transitions[i][j]`` 表示标签 i → j 的分数。"""

        def __init__(
            self,
            num_tags: int,
            tags: Sequence[str] = DEFAULT_TAGS,
            use_constraints: bool = True,
        ) -> None:
            super().__init__()
            self.num_tags = num_tags
            self.tags = tuple(tags)
            self.use_constraints = use_constraints

            self.transitions = nn.Parameter(torch.empty(num_tags, num_tags))
            self.start_transitions = nn.Parameter(torch.empty(num_tags))
            self.end_transitions = nn.Parameter(torch.empty(num_tags))
            nn.init.uniform_(self.transitions, -0.1, 0.1)
            nn.init.uniform_(self.start_transitions, -0.1, 0.1)
            nn.init.uniform_(self.end_transitions, -0.1, 0.1)

            # 预计算非法转移掩码（True 表示禁止）：序列不能以 I-x 开头、不能以 B-x
            # 结尾；I-x 只能接在同类型的 B-x / I-x 之后。显式登记为 buffer 才能与
            # checkpoint 的 state_dict 对齐。
            trans_mask = torch.zeros(num_tags, num_tags, dtype=torch.bool)
            start_mask = torch.zeros(num_tags, dtype=torch.bool)
            end_mask = torch.zeros(num_tags, dtype=torch.bool)
            if use_constraints:
                for i, tag_i in enumerate(self.tags):
                    if tag_i.startswith("I-"):
                        start_mask[i] = True
                    if tag_i.startswith("B-"):
                        end_mask[i] = True
                    for j, tag_j in enumerate(self.tags):
                        if tag_j.startswith("I-"):
                            entity_type = tag_j[2:]
                            legal = tag_i.startswith(("B-", "I-")) and tag_i[2:] == entity_type
                            if not legal:
                                trans_mask[i, j] = True
            self.register_buffer("trans_mask", trans_mask)
            self.register_buffer("start_mask", start_mask)
            self.register_buffer("end_mask", end_mask)

        # ── 约束后的分数 ────────────────────────────────────────────────
        def _constrained_transitions(self):
            if not self.use_constraints:
                return self.transitions
            return self.transitions.masked_fill(self.trans_mask, NEG_INF)

        def _constrained_start(self):
            if not self.use_constraints:
                return self.start_transitions
            return self.start_transitions.masked_fill(self.start_mask, NEG_INF)

        def _constrained_end(self):
            if not self.use_constraints:
                return self.end_transitions
            return self.end_transitions.masked_fill(self.end_mask, NEG_INF)

        @torch.no_grad()
        def decode(self, emissions, mask) -> list[list[int]]:
            """Viterbi 解码，返回 ``List[List[int]]``（已按真实长度截断）。"""

            batch, seq_len, num_tags = emissions.shape
            score = self._constrained_start() + emissions[:, 0]
            history = []
            trans = self._constrained_transitions().unsqueeze(0)

            for i in range(1, seq_len):
                m = mask[:, i].unsqueeze(1)
                scores = score.unsqueeze(2) + trans
                best_score, best_tag = scores.max(dim=1)
                best_score = best_score + emissions[:, i]
                score = torch.where(m.bool(), best_score, score)
                history.append(
                    torch.where(m.bool(), best_tag, torch.zeros_like(best_tag))
                )

            score = score + self._constrained_end()
            best_last = score.argmax(dim=1)
            lengths = mask.sum(1).long().clamp(min=1).tolist()

            results: list[list[int]] = []
            for b, length in enumerate(lengths):
                last = int(best_last[b].item())
                path = [last]
                j = length - 1
                while j > 0:
                    last = int(history[j - 1][b][last].item())
                    path.append(last)
                    j -= 1
                path.reverse()
                results.append(path)
            return results


    class BiLSTMCRF(nn.Module):
        """Embedding → BiLSTM → Linear → CRF。"""

        def __init__(
            self,
            vocab_size: int,
            num_tags: int,
            tags: Sequence[str] = DEFAULT_TAGS,
            embed_dim: int = 128,
            hidden_dim: int = 256,
            num_layers: int = 1,
            dropout: float = 0.5,
            pad_idx: int = 0,
            use_constraints: bool = True,
        ) -> None:
            super().__init__()
            self.embedding = nn.Embedding(vocab_size, embed_dim, padding_idx=pad_idx)
            nn.init.normal_(self.embedding.weight, mean=0.0, std=0.1)
            with torch.no_grad():
                self.embedding.weight[pad_idx].zero_()

            self.drop = nn.Dropout(dropout)
            self.lstm = nn.LSTM(
                input_size=embed_dim,
                hidden_size=hidden_dim,
                num_layers=num_layers,
                bidirectional=True,
                batch_first=True,
                dropout=dropout if num_layers > 1 else 0.0,
            )
            self.fc = nn.Linear(hidden_dim * 2, num_tags)
            self.crf = CRF(num_tags, tags=tags, use_constraints=use_constraints)

        def _emissions(self, input_ids, mask):
            emb = self.drop(self.embedding(input_ids))
            # 用 pack_padded_sequence 跳过 padding，缩短 CRF 需要展开的时间步。
            lengths = mask.sum(1).long().clamp(min=1).cpu()
            packed = nn.utils.rnn.pack_padded_sequence(
                emb, lengths, batch_first=True, enforce_sorted=False
            )
            outputs, _ = self.lstm(packed)
            outputs, _ = nn.utils.rnn.pad_packed_sequence(
                outputs, batch_first=True, total_length=emb.size(1)
            )
            return self.fc(self.drop(outputs))

        @torch.no_grad()
        def predict(self, input_ids, mask) -> list[list[int]]:
            """预测标签索引（``List[List[int]]``）。"""

            return self.crf.decode(self._emissions(input_ids, mask), mask)


__all__ = [
    "DEFAULT_TAGS",
    "NEG_INF",
    "TORCH_AVAILABLE",
    "BiLSTMCRF",
    "CRF",
]
