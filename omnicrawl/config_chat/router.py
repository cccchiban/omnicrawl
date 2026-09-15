"""配置对话的可选 PyTorch 向量检索后端。"""

from __future__ import annotations

import io
import json
from importlib.resources import files
from typing import Dict, List, Tuple


class ConfigRouterUnavailable(RuntimeError):
    """配置对话模型不可用。"""


class ConfigRouter:
    def __init__(self) -> None:
        try:
            import torch
            import torch.nn as nn
        except ImportError as exc:
            raise ConfigRouterUnavailable("配置对话需要可选依赖 torch，请安装 config-chat。") from exc

        class Model(nn.Module):
            def __init__(self, vocab_size: int, num_actions: int, *, embed_dim=192, hidden_dim=384, num_layers=2, dropout=0.2):
                super().__init__()
                self.embedding = nn.Embedding(vocab_size, embed_dim, padding_idx=0)
                self.dropout = nn.Dropout(dropout)
                self.encoder = nn.ModuleList()
                input_dim = embed_dim
                for _ in range(num_layers):
                    self.encoder.append(nn.GRU(input_dim, hidden_dim, batch_first=True, bidirectional=True))
                    input_dim = hidden_dim * 2
                out_dim = hidden_dim * 2
                self.config_head = nn.Linear(out_dim, 3)
                self.value_head = nn.Linear(out_dim, 3)
                self.action_head = nn.Linear(out_dim, num_actions)
                self.project = nn.Linear(out_dim, 256)

            def forward(self, input_ids, attention_mask):
                hidden = self.dropout(self.embedding(input_ids))
                for gru in self.encoder:
                    hidden, _ = gru(hidden)
                    hidden = self.dropout(hidden)
                return {
                    "config_logits": self.config_head(hidden),
                    "value_logits": self.value_head(hidden),
                    "action_logits": self.action_head(hidden),
                    "token_reps": nn.functional.normalize(self.project(hidden), dim=-1),
                }

            def encode(self, input_ids, attention_mask):
                reps = self.forward(input_ids, attention_mask)["token_reps"]
                mask = attention_mask.unsqueeze(-1).float()
                return (reps * mask).sum(dim=1) / mask.sum(dim=1).clamp(min=1.0)

        self.torch = torch
        self.model_cls = Model
        root = files("omnicrawl.config_chat.assets")
        payload = torch.load(io.BytesIO(root.joinpath("router.pt").read_bytes()), map_location="cpu")
        self.vocab: Dict[str, int] = payload["vocab"]
        self.actions: List[str] = payload["actions"]
        self.configs: List[str] = payload["configs"]
        self.model = Model(len(self.vocab), len(self.actions), **payload.get("model_config", {}))
        self.model.load_state_dict(payload["model_state"])
        self.model.eval()
        labels = json.loads(root.joinpath("labels.json").read_text(encoding="utf-8"))["keys"]
        self.sections = {item["path"] for item in labels if item["kind"] == "section"}
        self.alias_map = json.loads(root.joinpath("aliases.json").read_text(encoding="utf-8"))
        self._build_alias_index()

    def _encode(self, texts: List[str]):
        rows = [[self.vocab.get(ch, 1) for ch in text[:24]] for text in texts]
        width = max(1, max(len(row) for row in rows))
        ids = self.torch.zeros(len(rows), width, dtype=self.torch.long)
        mask = self.torch.zeros(len(rows), width, dtype=self.torch.long)
        for index, row in enumerate(rows):
            ids[index, :len(row)] = self.torch.tensor(row, dtype=self.torch.long)
            mask[index, :len(row)] = 1
        return ids, mask

    def _build_alias_index(self) -> None:
        texts, owners = [], []
        for index, config in enumerate(self.configs):
            for alias in self.alias_map.get(config, [config]):
                texts.append(alias)
                owners.append(index)
        ids, mask = self._encode(texts)
        with self.torch.no_grad():
            vectors = self.model.encode(ids, mask)
        self.alias_vectors = vectors
        self.alias_owner = self.torch.tensor(owners, dtype=self.torch.long)

    @staticmethod
    def _spans(tags: List[int]) -> List[Tuple[int, int]]:
        result, start = [], None
        for index, tag in enumerate(tags):
            if tag == 1:
                if start is not None:
                    result.append((start, index))
                start = index
            elif tag != 2 and start is not None:
                result.append((start, index))
                start = None
        if start is not None:
            result.append((start, len(tags)))
        return result

    @staticmethod
    def _value_spans(chars: List[str], tags: List[int]) -> List[str]:
        result = []
        for start, end in ConfigRouter._spans(tags):
            while end < len(chars) and chars[end] in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-/:\\":
                end += 1
            result.append("".join(chars[start:end]))
        return result

    def predict(self, text: str) -> List[dict]:
        import re
        clauses = [part.strip() for part in re.split(r"[，,；;。！？!?]|然后|接着|顺便|另外|还有", text) if part.strip()]
        commands = []
        for clause in clauses:
            chars = list(clause)[:96]
            ids = self.torch.tensor([[self.vocab.get(ch, 1) for ch in chars]], dtype=self.torch.long)
            mask = self.torch.ones_like(ids)
            with self.torch.no_grad():
                output = self.model(ids, mask)
            spans = self._spans(output["config_logits"].argmax(-1)[0].tolist())
            values = self._value_spans(chars, output["value_logits"].argmax(-1)[0].tolist())
            reps = output["token_reps"][0]
            action_logits = output["action_logits"][0]
            for index, (start, end) in enumerate(spans[:12]):
                pooled = reps[start:end].mean(0)
                pooled = pooled / pooled.norm().clamp(min=1e-6)
                similarity = self.alias_vectors @ pooled
                scores = self.torch.full((len(self.configs),), -2.0)
                scores.scatter_reduce_(0, self.alias_owner, similarity, reduce="amax")
                config_index = int(scores.argmax())
                config = self.configs[config_index]
                action = self.actions[int(action_logits[start].argmax())]
                if action in ("ENABLE", "DISABLE") and config in self.sections and f"{config}.enabled" in self.configs:
                    config = f"{config}.enabled"
                value = values[index] if index < len(values) else ""
                if action in ("ENABLE", "DISABLE"):
                    value = "true" if action == "ENABLE" else "false"
                commands.append({"action": action, "config": config, "value": value, "score": float(scores[config_index])})
        return commands
