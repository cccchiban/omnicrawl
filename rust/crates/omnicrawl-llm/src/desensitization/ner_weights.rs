//! NER 权重加载与 BiLSTM-CRF 前向。
//!
//! 权重来自 Python 侧 checkpoint（`models/bilstm_crf_best.pt`，zip + pickle 的 torch 存档），
//! 由 `rust/tools/gen_ner_fixture.py` 转成「魔数 + 头部 JSON + f32 数据块」的自描述二进制：
//! 内核不实现 pickle 解析，只读这份**与 checkpoint 逐位同源**的转换结果。
//!
//! 结构与 `omnicrawl/llm/desensitization/ner_model.py` 一致：Embedding → BiLSTM（1 层、
//! 双向）→ Linear → 带约束的线性链 CRF（Viterbi 解码）。dropout 在推理态是恒等，故不参与。

use std::collections::HashMap;
use std::path::Path;

use super::ner::NerBackend;

const MAGIC: &[u8; 8] = b"OCNER1\x00\x00";
/// CRF 约束用的「禁止」分数，与 Python 侧 `NEG_INF` 同值。
const NEG_INF: f32 = -1e4;

/// 一个张量：扁平 f32 数据（bool 掩码以 0.0/1.0 表示）。
#[derive(Debug, Clone)]
struct Tensor {
    data: Vec<f32>,
}

/// 模型结构参数。
#[derive(Debug, Clone)]
pub struct NerModelConfig {
    pub vocab_size: usize,
    pub num_tags: usize,
    pub embed_dim: usize,
    pub hidden_dim: usize,
    pub num_layers: usize,
    pub use_constraints: bool,
}

/// 加载后的权重，同时是 [`NerBackend`] 的实现。
pub struct NerWeights {
    tags: Vec<String>,
    char2idx: HashMap<char, u32>,
    unk_id: u32,
    pad_id: u32,
    config: NerModelConfig,
    tensors: HashMap<String, Tensor>,
}

impl NerWeights {
    /// 读取内核 NER 权重文件。
    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|error| format!("读取 NER 权重失败：{error}"))?;
        if bytes.len() < 12 || &bytes[..8] != MAGIC {
            return Err("不是内核 NER 权重格式（缺少魔数）。".to_string());
        }
        let header_len = u32::from_le_bytes(
            bytes[8..12]
                .try_into()
                .map_err(|_| "NER 权重头部长度无效。".to_string())?,
        ) as usize;
        let header_end = 12 + header_len;
        if bytes.len() < header_end {
            return Err("NER 权重头部被截断。".to_string());
        }
        let header: serde_json::Value = serde_json::from_slice(&bytes[12..header_end])
            .map_err(|error| format!("解析 NER 权重头部失败：{error}"))?;
        let body = &bytes[header_end..];

        let tags: Vec<String> = header["tags"]
            .as_array()
            .ok_or_else(|| "NER 权重缺少 tags。".to_string())?
            .iter()
            .map(|item| item.as_str().unwrap_or_default().to_string())
            .collect();

        let mut char2idx: HashMap<char, u32> = HashMap::new();
        let mut unk_id = 1u32;
        let mut pad_id = 0u32;
        let raw_char2idx = header["char2idx"]
            .as_object()
            .ok_or_else(|| "NER 权重缺少 char2idx。".to_string())?;
        for (key, value) in raw_char2idx {
            let id = value.as_u64().unwrap_or_default() as u32;
            match key.as_str() {
                "<unk>" => unk_id = id,
                "<pad>" => pad_id = id,
                _ => {
                    if let Some(ch) = key.chars().next() {
                        if key.chars().count() == 1 {
                            char2idx.insert(ch, id);
                        }
                    }
                }
            }
        }

        let config = &header["config"];
        let model_config = NerModelConfig {
            vocab_size: config["vocab_size"].as_u64().unwrap_or_default() as usize,
            num_tags: config["num_tags"].as_u64().unwrap_or_default() as usize,
            embed_dim: config["embed_dim"].as_u64().unwrap_or_default() as usize,
            hidden_dim: config["hidden_dim"].as_u64().unwrap_or_default() as usize,
            num_layers: config["num_layers"].as_u64().unwrap_or(1) as usize,
            use_constraints: config
                .get("use_constraints")
                .and_then(|value| value.as_bool())
                .unwrap_or(true),
        };

        let mut tensors: HashMap<String, Tensor> = HashMap::new();
        let entries = header["tensors"]
            .as_array()
            .ok_or_else(|| "NER 权重缺少张量表。".to_string())?;
        for entry in entries {
            let name = entry["name"].as_str().unwrap_or_default().to_string();
            let dtype = entry["dtype"].as_str().unwrap_or("f32");
            let offset = entry["offset"].as_u64().unwrap_or_default() as usize;
            let length = entry["length"].as_u64().unwrap_or_default() as usize;
            if offset + length > body.len() {
                return Err(format!("NER 权重数据段越界：{name}"));
            }
            let chunk = &body[offset..offset + length];
            let data = match dtype {
                "bool" => chunk
                    .iter()
                    .map(|item| if *item != 0 { 1.0 } else { 0.0 })
                    .collect(),
                _ => chunk
                    .chunks_exact(4)
                    .map(|item| f32::from_le_bytes([item[0], item[1], item[2], item[3]]))
                    .collect(),
            };
            tensors.insert(name, Tensor { data });
        }

        Ok(Self {
            tags,
            char2idx,
            unk_id,
            pad_id,
            config: model_config,
            tensors,
        })
    }

    pub fn config(&self) -> &NerModelConfig {
        &self.config
    }

    pub fn pad_id(&self) -> u32 {
        self.pad_id
    }

    fn tensor(&self, name: &str) -> Result<&Tensor, String> {
        self.tensors
            .get(name)
            .ok_or_else(|| format!("NER 权重缺少张量：{name}"))
    }

    /// 单条序列的标签路径（Viterbi 解码结果）。
    fn predict_one(&self, ids: &[u32]) -> Result<Vec<usize>, String> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let input_dim = self.config.embed_dim;
        let hidden = self.config.hidden_dim;
        let embedding = self.tensor("embedding.weight")?;

        let inputs: Vec<Vec<f32>> = ids
            .iter()
            .map(|id| {
                let start = *id as usize * input_dim;
                embedding.data[start..start + input_dim].to_vec()
            })
            .collect();

        let forward = self.lstm_direction(
            &inputs,
            "lstm.weight_ih_l0",
            "lstm.weight_hh_l0",
            "lstm.bias_ih_l0",
            "lstm.bias_hh_l0",
            hidden,
            true,
        )?;
        let backward = self.lstm_direction(
            &inputs,
            "lstm.weight_ih_l0_reverse",
            "lstm.weight_hh_l0_reverse",
            "lstm.bias_ih_l0_reverse",
            "lstm.bias_hh_l0_reverse",
            hidden,
            false,
        )?;

        let fc_weight = self.tensor("fc.weight")?;
        let fc_bias = self.tensor("fc.bias")?;
        let num_tags = self.config.num_tags;
        let combined_dim = hidden * 2;
        let mut emissions: Vec<Vec<f32>> = Vec::with_capacity(ids.len());
        for step in 0..ids.len() {
            let mut combined = Vec::with_capacity(combined_dim);
            combined.extend_from_slice(&forward[step]);
            combined.extend_from_slice(&backward[step]);
            let mut row = Vec::with_capacity(num_tags);
            for tag in 0..num_tags {
                let weights = &fc_weight.data[tag * combined_dim..(tag + 1) * combined_dim];
                let mut value = fc_bias.data[tag];
                for (index, weight) in weights.iter().enumerate() {
                    value += weight * combined[index];
                }
                row.push(value);
            }
            emissions.push(row);
        }

        self.viterbi(&emissions)
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::needless_range_loop)]
    fn lstm_direction(
        &self,
        inputs: &[Vec<f32>],
        weight_ih: &str,
        weight_hh: &str,
        bias_ih: &str,
        bias_hh: &str,
        hidden: usize,
        forward: bool,
    ) -> Result<Vec<Vec<f32>>, String> {
        let input_dim = self.config.embed_dim;
        let w_ih = self.tensor(weight_ih)?;
        let w_hh = self.tensor(weight_hh)?;
        let b_ih = self.tensor(bias_ih)?;
        let b_hh = self.tensor(bias_hh)?;

        let mut hidden_state = vec![0.0f32; hidden];
        let mut cell_state = vec![0.0f32; hidden];
        let mut gates = vec![0.0f32; hidden * 4];
        let mut outputs = vec![vec![0.0f32; hidden]; inputs.len()];
        let order: Vec<usize> = if forward {
            (0..inputs.len()).collect()
        } else {
            (0..inputs.len()).rev().collect()
        };
        for index in order {
            let input = &inputs[index];
            for row in 0..hidden * 4 {
                // PyTorch 的门顺序是输入门、遗忘门、候选、输出门。
                let mut value = b_ih.data[row] + b_hh.data[row];
                let input_weights = &w_ih.data[row * input_dim..(row + 1) * input_dim];
                for (slot, weight) in input_weights.iter().enumerate() {
                    value += weight * input[slot];
                }
                let recurrent_weights = &w_hh.data[row * hidden..(row + 1) * hidden];
                for (slot, weight) in recurrent_weights.iter().enumerate() {
                    value += weight * hidden_state[slot];
                }
                gates[row] = value;
            }
            for slot in 0..hidden {
                let input_gate = sigmoid(gates[slot]);
                let forget_gate = sigmoid(gates[hidden + slot]);
                let candidate = gates[hidden * 2 + slot].tanh();
                let output_gate = sigmoid(gates[hidden * 3 + slot]);
                cell_state[slot] = forget_gate * cell_state[slot] + input_gate * candidate;
                hidden_state[slot] = output_gate * cell_state[slot].tanh();
            }
            outputs[index] = hidden_state.clone();
        }
        Ok(outputs)
    }

    #[allow(clippy::needless_range_loop)]
    fn viterbi(&self, emissions: &[Vec<f32>]) -> Result<Vec<usize>, String> {
        let transitions = self.tensor("crf.transitions")?;
        let start = self.tensor("crf.start_transitions")?;
        let end = self.tensor("crf.end_transitions")?;
        let trans_mask = self.tensor("crf.trans_mask")?;
        let start_mask = self.tensor("crf.start_mask")?;
        let end_mask = self.tensor("crf.end_mask")?;
        let constrained = self.config.use_constraints;
        let num_tags = self.config.num_tags;

        let mask = |value: f32, forbidden: f32| -> f32 {
            if constrained && forbidden > 0.5 {
                NEG_INF
            } else {
                value
            }
        };

        let mut score: Vec<f32> = (0..num_tags)
            .map(|tag| mask(start.data[tag], start_mask.data[tag]) + emissions[0][tag])
            .collect();
        let mut history: Vec<Vec<usize>> = Vec::with_capacity(emissions.len());
        for step in 1..emissions.len() {
            let mut best_scores = vec![0.0f32; num_tags];
            let mut best_tags = vec![0usize; num_tags];
            for tag in 0..num_tags {
                let mut best = f32::NEG_INFINITY;
                let mut best_previous = 0usize;
                for previous in 0..num_tags {
                    let transition = mask(
                        transitions.data[previous * num_tags + tag],
                        trans_mask.data[previous * num_tags + tag],
                    );
                    let candidate = score[previous] + transition;
                    if candidate > best {
                        best = candidate;
                        best_previous = previous;
                    }
                }
                best_scores[tag] = best + emissions[step][tag];
                best_tags[tag] = best_previous;
            }
            score = best_scores;
            history.push(best_tags);
        }
        for tag in 0..num_tags {
            score[tag] += mask(end.data[tag], end_mask.data[tag]);
        }

        let mut last = 0usize;
        let mut best = f32::NEG_INFINITY;
        for (tag, value) in score.iter().enumerate() {
            if *value > best {
                best = *value;
                last = tag;
            }
        }
        let mut path = vec![last];
        let mut cursor = last;
        for step in (1..emissions.len()).rev() {
            cursor = history[step - 1][cursor];
            path.push(cursor);
        }
        path.reverse();
        Ok(path)
    }
}

impl NerBackend for NerWeights {
    fn tags(&self) -> &[String] {
        &self.tags
    }

    fn char_id(&self, ch: char) -> u32 {
        self.char2idx.get(&ch).copied().unwrap_or(self.unk_id)
    }

    fn unk_id(&self) -> u32 {
        self.unk_id
    }

    fn predict(&self, batch: &[Vec<u32>]) -> Vec<Vec<usize>> {
        batch
            .iter()
            .map(|ids| self.predict_one(ids).unwrap_or_default())
            .collect()
    }
}

fn sigmoid(value: f32) -> f32 {
    1.0 / (1.0 + (-value).exp())
}
