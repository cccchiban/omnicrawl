//! 配置对话权重的加载与字符级双向 GRU 前向（对映 `router.py` 里的 `Model`）。
//!
//! 权重来自 Python 侧的 `omnicrawl/config_chat/assets/router.pt`（zip + pickle 的 torch 存档），
//! 由 `rust/tools/gen_config_router_fixture.py` 转成「魔数 + 头部 JSON + f32 数据块」的自描述
//! 二进制：内核不实现 pickle 解析，只读这份**与 checkpoint 逐位同源**的转换结果。
//!
//! 结构与 Python 侧一致：Embedding（480 × 192）→ 2 层双向 GRU（隐藏 384）→
//! 三个逐 token 的线性头（config / value / action）+ `project`（256 维，L2 归一化后作为
//! token 表示）。dropout 在推理态是恒等，故不参与；设备只有 CPU。

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value as Json;

/// 内核权重文件魔数。
pub const WEIGHTS_MAGIC: &[u8; 8] = b"OCCFG1\0\0";
/// 资源目录里的权重文件名。
pub const WEIGHTS_FILENAME: &str = "config_router.bin";

/// 未登录字符的 id（对映 Python 的 `vocab.get(ch, 1)`）。
pub const UNK_ID: u32 = 1;
/// padding / `<pad>` 的 id。
pub const PAD_ID: u32 = 0;
/// `_encode` 的字符截断长度（别名索引向量；`predict` 走 96 字符的另一条路径）。
pub const ENCODE_MAX_CHARS: usize = 24;

/// config / value 两个头的类别数，与 Python 的 `nn.Linear(out_dim, 3)` 字面量一致。
const CLASSES: usize = 3;
/// `nn.functional.normalize` 的默认 `eps`。
const NORMALIZE_EPS: f32 = 1e-12;

/// 一个张量：扁平 f32 数据 + 原形状（行主序）。
#[derive(Debug, Clone)]
struct Tensor {
    data: Vec<f32>,
    shape: Vec<usize>,
}

/// 模型结构参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterModelConfig {
    pub vocab_size: usize,
    pub num_actions: usize,
    pub embed_dim: usize,
    pub hidden_dim: usize,
    pub num_layers: usize,
    pub project_dim: usize,
}

impl RouterModelConfig {
    /// 双向 GRU 的输出宽度（也是三个头的输入宽度）。
    pub fn out_dim(&self) -> usize {
        self.hidden_dim * 2
    }
}

/// 一次前向的逐 token 输出。
#[derive(Debug, Clone)]
pub struct RouterForward {
    /// `(T, 3)`：BIO 式配置片段标签。
    pub config_logits: Vec<Vec<f32>>,
    /// `(T, 3)`：BIO 式取值片段标签。
    pub value_logits: Vec<Vec<f32>>,
    /// `(T, num_actions)`：动作分类。
    pub action_logits: Vec<Vec<f32>>,
    /// `(T, project_dim)`：L2 归一化后的 token 表示。
    pub token_reps: Vec<Vec<f32>>,
}

/// 加载后的权重：词表、动作表、配置表与全部张量。
pub struct RouterWeights {
    actions: Vec<String>,
    configs: Vec<String>,
    vocab: HashMap<char, u32>,
    config: RouterModelConfig,
    tensors: HashMap<String, Tensor>,
}

impl RouterWeights {
    /// 读取内核权重文件。
    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path)
            .map_err(|error| format!("读取配置对话权重失败：{}，{error}", path.display()))?;
        Self::from_bytes(&bytes)
    }

    /// 解析内核权重二进制。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 12 || &bytes[..8] != WEIGHTS_MAGIC {
            return Err("不是内核配置对话权重格式（缺少魔数）。".to_string());
        }
        let header_len = u32::from_le_bytes(
            bytes[8..12]
                .try_into()
                .map_err(|_| "配置对话权重头部长度无效。".to_string())?,
        ) as usize;
        let header_end = 12 + header_len;
        if bytes.len() < header_end {
            return Err("配置对话权重头部被截断。".to_string());
        }
        let header: Json = serde_json::from_slice(&bytes[12..header_end])
            .map_err(|error| format!("解析配置对话权重头部失败：{error}"))?;
        let body = &bytes[header_end..];

        let actions = string_list(&header, "actions")?;
        let configs = string_list(&header, "configs")?;

        // 词表按字符索引：`<pad>` / `<unk>` 这类多字符键永远不会被单字符查询命中。
        let raw_vocab = header["vocab"]
            .as_object()
            .ok_or_else(|| "配置对话权重缺少 vocab。".to_string())?;
        let mut vocab: HashMap<char, u32> = HashMap::new();
        for (key, value) in raw_vocab {
            let id = value.as_u64().unwrap_or_default() as u32;
            let mut characters = key.chars();
            if let (Some(ch), None) = (characters.next(), characters.next()) {
                vocab.insert(ch, id);
            }
        }

        let model_config = &header["model_config"];
        let vocab_size = model_config["vocab_size"]
            .as_u64()
            .ok_or_else(|| "配置对话权重缺少 model_config.vocab_size。".to_string())?
            as usize;
        let config = RouterModelConfig {
            vocab_size,
            num_actions: actions.len(),
            embed_dim: model_config["embed_dim"].as_u64().unwrap_or(192) as usize,
            hidden_dim: model_config["hidden_dim"].as_u64().unwrap_or(384) as usize,
            num_layers: model_config["num_layers"].as_u64().unwrap_or(2) as usize,
            project_dim: 0,
        };

        let mut tensors: HashMap<String, Tensor> = HashMap::new();
        let entries = header["tensors"]
            .as_array()
            .ok_or_else(|| "配置对话权重缺少张量表。".to_string())?;
        for entry in entries {
            let name = entry["name"].as_str().unwrap_or_default().to_string();
            let dtype = entry["dtype"].as_str().unwrap_or("f32");
            let shape: Vec<usize> = entry["shape"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .map(|item| item.as_u64().unwrap_or_default() as usize)
                        .collect()
                })
                .unwrap_or_default();
            let offset = entry["offset"].as_u64().unwrap_or_default() as usize;
            let length = entry["length"].as_u64().unwrap_or_default() as usize;
            if offset + length > body.len() {
                return Err(format!("配置对话权重数据段越界：{name}"));
            }
            let chunk = &body[offset..offset + length];
            let data: Vec<f32> = match dtype {
                // 布尔张量在数据块里按 1 字节存，**每个元素占 1 字节**（长度按字节算）。
                "bool" => chunk
                    .iter()
                    .map(|item| if *item != 0 { 1.0f32 } else { 0.0f32 })
                    .collect(),
                _ => chunk
                    .chunks_exact(4)
                    .map(|item| f32::from_le_bytes([item[0], item[1], item[2], item[3]]))
                    .collect(),
            };
            if shape.iter().product::<usize>() != data.len() {
                return Err(format!("配置对话权重张量形状与数据长度不符：{name}"));
            }
            tensors.insert(name, Tensor { data, shape });
        }

        let project_dim = tensors
            .get("project.weight")
            .and_then(|tensor| tensor.shape.first().copied())
            .ok_or_else(|| "配置对话权重缺少张量：project.weight".to_string())?;
        let config = RouterModelConfig {
            project_dim,
            ..config
        };
        let weights = Self {
            actions,
            configs,
            vocab,
            config,
            tensors,
        };
        weights.validate()?;
        Ok(weights)
    }

    pub fn config(&self) -> &RouterModelConfig {
        &self.config
    }

    /// 动作表，顺序即动作 id。
    pub fn actions(&self) -> &[String] {
        &self.actions
    }

    /// 配置路径表，顺序即检索下标。
    pub fn configs(&self) -> &[String] {
        &self.configs
    }

    pub fn project_dim(&self) -> usize {
        self.config.project_dim
    }

    /// 字符 → 词表 id；未登录（含多字符键）一律 [`UNK_ID`]。
    pub fn char_id(&self, ch: char) -> u32 {
        self.vocab.get(&ch).copied().unwrap_or(UNK_ID)
    }

    /// `_encode` 的取词：按 [`ENCODE_MAX_CHARS`] 截断（别名索引向量专用）。
    pub fn encode_ids(&self, text: &str) -> Vec<u32> {
        text.chars()
            .take(ENCODE_MAX_CHARS)
            .map(|ch| self.char_id(ch))
            .collect()
    }

    /// `predict` 的取词：不截断，调用方已按 96 字符切好。
    pub fn token_ids(&self, characters: &[char]) -> Vec<u32> {
        characters.iter().map(|ch| self.char_id(*ch)).collect()
    }

    /// 别名索引向量：前向 + 逐 token 均值（对映 `Model.encode` 的掩码均值）。
    pub fn encode_text(&self, text: &str) -> Result<Vec<f32>, String> {
        let ids = self.encode_ids(text);
        if ids.is_empty() {
            return Ok(vec![0.0; self.config.project_dim]);
        }
        let output = self.forward(&ids)?;
        Ok(mean_pool(&output.token_reps))
    }

    /// 批量版：宿主算别名索引时一次跑完（逐条语义与 [`Self::encode_text`] 完全一致）。
    ///
    /// 不做批内并行：GRU 前向本身是纯计算，并行会让缓存构建期间的 CPU 曲线更难解释，
    /// 且离线缓存已经把这笔开销从每次启动摊到一次。
    pub fn encode_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for text in texts {
            vectors.push(self.encode_text(text)?);
        }
        Ok(vectors)
    }

    /// 一次完整前向：Embedding → 逐层双向 GRU → 四个头。
    pub fn forward(&self, ids: &[u32]) -> Result<RouterForward, String> {
        if ids.is_empty() {
            return Err("配置对话前向需要非空输入。".to_string());
        }
        let embed_dim = self.config.embed_dim;
        let embedding = self.tensor("embedding.weight")?;
        let mut hidden: Vec<Vec<f32>> = Vec::with_capacity(ids.len());
        for id in ids {
            let start = (*id as usize) * embed_dim;
            hidden.push(embedding.data[start..start + embed_dim].to_vec());
        }

        for layer in 0..self.config.num_layers {
            let forward = self.gru_direction(&hidden, layer, false)?;
            let backward = self.gru_direction(&hidden, layer, true)?;
            let mut merged: Vec<Vec<f32>> = Vec::with_capacity(hidden.len());
            for (forward_row, backward_row) in forward.iter().zip(backward.iter()) {
                let mut row = Vec::with_capacity(self.config.out_dim());
                row.extend_from_slice(forward_row);
                row.extend_from_slice(backward_row);
                merged.push(row);
            }
            hidden = merged;
        }

        let mut output = RouterForward {
            config_logits: Vec::with_capacity(hidden.len()),
            value_logits: Vec::with_capacity(hidden.len()),
            action_logits: Vec::with_capacity(hidden.len()),
            token_reps: Vec::with_capacity(hidden.len()),
        };
        for row in &hidden {
            output.config_logits.push(self.linear("config_head", row)?);
            output.value_logits.push(self.linear("value_head", row)?);
            output.action_logits.push(self.linear("action_head", row)?);
            let projected = self.linear("project", row)?;
            output.token_reps.push(normalize(&projected, NORMALIZE_EPS));
        }
        Ok(output)
    }

    fn tensor(&self, name: &str) -> Result<&Tensor, String> {
        self.tensors
            .get(name)
            .ok_or_else(|| format!("配置对话权重缺少张量：{name}"))
    }

    /// 逐 token 线性层：`weight` 形状 `(out, in)`。
    fn linear(&self, prefix: &str, input: &[f32]) -> Result<Vec<f32>, String> {
        let weight = self.tensor(&format!("{prefix}.weight"))?;
        let bias = self.tensor(&format!("{prefix}.bias"))?;
        let out = bias.data.len();
        let width = input.len();
        let mut result = vec![0.0f32; out];
        for (row, value) in result.iter_mut().enumerate() {
            let mut total = bias.data[row];
            let weights = &weight.data[row * width..(row + 1) * width];
            for (slot, weight) in weights.iter().enumerate() {
                total += weight * input[slot];
            }
            *value = total;
        }
        Ok(result)
    }

    /// 单向 GRU。PyTorch 的门顺序是重置门、更新门、候选门（`W_ir | W_iz | W_in`），
    /// 更新式为 `h' = (1 - z) * n + z * h`，`n` 里乘重置门的是整条 `W_hn h + b_hn`。
    fn gru_direction(
        &self,
        inputs: &[Vec<f32>],
        layer: usize,
        reverse: bool,
    ) -> Result<Vec<Vec<f32>>, String> {
        let hidden = self.config.hidden_dim;
        let input_dim = if layer == 0 {
            self.config.embed_dim
        } else {
            self.config.out_dim()
        };
        let suffix = if reverse { "_reverse" } else { "" };
        let weight_ih = self.tensor(&format!("encoder.{layer}.weight_ih_l0{suffix}"))?;
        let weight_hh = self.tensor(&format!("encoder.{layer}.weight_hh_l0{suffix}"))?;
        let bias_ih = self.tensor(&format!("encoder.{layer}.bias_ih_l0{suffix}"))?;
        let bias_hh = self.tensor(&format!("encoder.{layer}.bias_hh_l0{suffix}"))?;

        let gates = hidden * 3;
        let mut state = vec![0.0f32; hidden];
        let mut input_gates = vec![0.0f32; gates];
        let mut hidden_gates = vec![0.0f32; gates];
        let mut outputs = vec![vec![0.0f32; hidden]; inputs.len()];
        let order: Vec<usize> = if reverse {
            (0..inputs.len()).rev().collect()
        } else {
            (0..inputs.len()).collect()
        };
        for index in order {
            let input = &inputs[index];
            for (row, gate) in input_gates.iter_mut().enumerate().take(gates) {
                let mut total = bias_ih.data[row];
                for (slot, weight) in weight_ih.data[row * input_dim..(row + 1) * input_dim]
                    .iter()
                    .enumerate()
                {
                    total += weight * input[slot];
                }
                *gate = total;
            }
            for (row, gate) in hidden_gates.iter_mut().enumerate().take(gates) {
                let mut total = bias_hh.data[row];
                for (slot, weight) in weight_hh.data[row * hidden..(row + 1) * hidden]
                    .iter()
                    .enumerate()
                {
                    total += weight * state[slot];
                }
                *gate = total;
            }
            let mut next = vec![0.0f32; hidden];
            for slot in 0..hidden {
                let reset = sigmoid(input_gates[slot] + hidden_gates[slot]);
                let update = sigmoid(input_gates[hidden + slot] + hidden_gates[hidden + slot]);
                let candidate = (input_gates[hidden * 2 + slot]
                    + reset * hidden_gates[hidden * 2 + slot])
                    .tanh();
                next[slot] = (1.0 - update) * candidate + update * state[slot];
            }
            // 反向方向按倒序推进，但输出写回原位置（PyTorch 的 `_reverse` 语义）。
            outputs[index] = next.clone();
            state = next;
        }
        Ok(outputs)
    }

    /// 张量齐全性与形状校验：过了这一关，前向里的切片都不会越界。
    fn validate(&self) -> Result<(), String> {
        let embed_dim = self.config.embed_dim;
        let hidden = self.config.hidden_dim;
        let out_dim = self.config.out_dim();
        if self.config.vocab_size == 0
            || embed_dim == 0
            || hidden == 0
            || self.config.num_layers == 0
        {
            return Err("配置对话权重结构参数无效。".to_string());
        }
        self.expect_shape("embedding.weight", &[self.config.vocab_size, embed_dim])?;
        for layer in 0..self.config.num_layers {
            let input_dim = if layer == 0 { embed_dim } else { out_dim };
            for suffix in ["", "_reverse"] {
                self.expect_shape(
                    &format!("encoder.{layer}.weight_ih_l0{suffix}"),
                    &[hidden * 3, input_dim],
                )?;
                self.expect_shape(
                    &format!("encoder.{layer}.weight_hh_l0{suffix}"),
                    &[hidden * 3, hidden],
                )?;
                self.expect_shape(
                    &format!("encoder.{layer}.bias_ih_l0{suffix}"),
                    &[hidden * 3],
                )?;
                self.expect_shape(
                    &format!("encoder.{layer}.bias_hh_l0{suffix}"),
                    &[hidden * 3],
                )?;
            }
        }
        for prefix in ["config_head", "value_head"] {
            self.expect_shape(&format!("{prefix}.weight"), &[CLASSES, out_dim])?;
            self.expect_shape(&format!("{prefix}.bias"), &[CLASSES])?;
        }
        self.expect_shape("action_head.weight", &[self.config.num_actions, out_dim])?;
        self.expect_shape("action_head.bias", &[self.config.num_actions])?;
        self.expect_shape("project.weight", &[self.config.project_dim, out_dim])?;
        self.expect_shape("project.bias", &[self.config.project_dim])?;
        Ok(())
    }

    fn expect_shape(&self, name: &str, expected: &[usize]) -> Result<(), String> {
        let tensor = self.tensor(name)?;
        if tensor.shape != expected {
            return Err(format!(
                "配置对话权重张量形状不符：{name}，期望 {expected:?}，实际 {:?}",
                tensor.shape
            ));
        }
        Ok(())
    }
}

/// 逐 token 的掩码均值（对映 Python 的 `(reps * mask).sum(1) / mask.sum(1).clamp(min=1.0)`）。
pub fn mean_pool(rows: &[Vec<f32>]) -> Vec<f32> {
    let Some(first) = rows.first() else {
        return Vec::new();
    };
    let mut total = vec![0.0f32; first.len()];
    for row in rows {
        for (slot, value) in row.iter().enumerate() {
            total[slot] += value;
        }
    }
    let divisor = rows.len().max(1) as f32;
    for value in &mut total {
        *value /= divisor;
    }
    total
}

/// L2 归一化，范数下限 `min_norm`（对映 `nn.functional.normalize` 与 `pooled.norm().clamp(min=…)`）。
pub fn normalize(vector: &[f32], min_norm: f32) -> Vec<f32> {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    let divisor = norm.max(min_norm);
    vector.iter().map(|value| value / divisor).collect()
}

fn string_list(header: &Json, key: &str) -> Result<Vec<String>, String> {
    header[key]
        .as_array()
        .ok_or_else(|| format!("配置对话权重缺少 {key}。"))
        .map(|items| {
            items
                .iter()
                .map(|item| item.as_str().unwrap_or_default().to_string())
                .collect()
        })
}

fn sigmoid(value: f32) -> f32 {
    1.0 / (1.0 + (-value).exp())
}
