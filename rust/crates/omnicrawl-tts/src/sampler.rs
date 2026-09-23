//! 采样与随机数：`omnicrawl/tts/onnx_runtime.py` 的采样部分。
//!
//! 随机数用 PCG64（XSL-RR 128/64，与 numpy 的 `default_rng` 同族算法），播种走
//! splitmix64；`sample_mode = greedy` 的路径不使用随机数，因此可与 Python 逐 token
//! 对齐，`fixed` 模式的采样序列则与 Python 不逐位相同。

use std::cmp::Ordering;
use std::collections::HashSet;

pub const SAMPLE_MODE_GREEDY: &str = "greedy";
pub const SAMPLE_MODE_FIXED: &str = "fixed";
pub const SAMPLE_MODE_FULL: &str = "full";

const PCG_MULTIPLIER: u128 = 0x2360_ED05_1FC6_5DA4_4385_DF64_9FCC_F645;

/// PCG64 随机数发生器（`rng.random()` 与 numpy 一样取 `[0, 1)` 的 53 位 double）。
#[derive(Debug, Clone)]
pub struct Pcg64 {
    state: u128,
    inc: u128,
}

impl Pcg64 {
    pub fn seeded(seed: i64) -> Self {
        let mut mixer = seed as u64;
        let first = splitmix64(&mut mixer) as u128;
        let second = splitmix64(&mut mixer) as u128;
        let third = splitmix64(&mut mixer) as u128;
        let fourth = splitmix64(&mut mixer) as u128;
        let init_state = (first << 64) | second;
        let init_seq = (third << 64) | fourth;
        let inc = (init_seq << 1) | 1;
        let mut rng = Self { state: 0, inc };
        rng.step();
        rng.state = rng.state.wrapping_add(init_state);
        rng.step();
        rng
    }

    fn step(&mut self) {
        self.state = self
            .state
            .wrapping_mul(PCG_MULTIPLIER)
            .wrapping_add(self.inc);
    }

    pub fn next_u64(&mut self) -> u64 {
        self.step();
        let xorshifted = ((self.state >> 64) as u64) ^ (self.state as u64);
        let rotation = (self.state >> 122) as u32;
        xorshifted.rotate_right(rotation)
    }

    /// `[0, 1)` 均匀分布；与 numpy 的 `Generator.random()` 取位方式一致。
    pub fn random(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / 9_007_199_254_740_992.0)
    }
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// 采样模式归一化：未知值按 `do_sample` 落到 greedy / fixed。
pub fn normalize_sample_mode(raw_sample_mode: &str, do_sample: bool) -> String {
    let normalized = raw_sample_mode.trim();
    if [SAMPLE_MODE_GREEDY, SAMPLE_MODE_FIXED, SAMPLE_MODE_FULL].contains(&normalized) {
        return normalized.to_string();
    }
    if normalized == "mixed3" {
        return if do_sample {
            SAMPLE_MODE_FIXED.to_string()
        } else {
            SAMPLE_MODE_GREEDY.to_string()
        };
    }
    if do_sample {
        SAMPLE_MODE_FIXED.to_string()
    } else {
        SAMPLE_MODE_GREEDY.to_string()
    }
}

/// `np.argmax`：返回第一个最大值的下标。
pub fn argmax(values: &[f32]) -> usize {
    let mut best_index = 0usize;
    let mut best_value = f32::NEG_INFINITY;
    for (index, value) in values.iter().enumerate() {
        if *value > best_value {
            best_value = *value;
            best_index = index;
        }
    }
    best_index
}

/// 与 Python `_softmax` 一致：f32 上做平移，再以 f64 求指数与归一化。
pub fn softmax(values: &[f32]) -> Vec<f64> {
    let max_value = values.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f64> = values
        .iter()
        .map(|value| f64::from(*value - max_value).exp())
        .collect();
    let total: f64 = exps.iter().sum();
    exps.iter().map(|value| value / total).collect()
}

fn sort_ascending(values: &[f32]) -> Vec<f32> {
    let mut sorted = values.to_vec();
    sorted.sort_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal));
    sorted
}

/// 与 Python `_sample_from_scores` 一致：temperature 缩放 → top-k → top-p → 轮盘赌。
pub fn sample_from_scores(
    values: &[f32],
    do_sample: bool,
    temperature: f64,
    top_k: i64,
    top_p: f64,
    rng: &mut Pcg64,
) -> Result<usize, String> {
    if !do_sample {
        return Ok(argmax(values));
    }
    // 与 Python 的 `not (temperature > 0)` 一致：NaN 也按非法处理，不改成 `<= 0`。
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    if !(temperature > 0.0) {
        return Err("temperature must be positive when do_sample=True".to_string());
    }

    let mut scores: Vec<f32> = values
        .iter()
        .map(|value| *value / temperature as f32)
        .collect();
    if top_k > 0 && (top_k as usize) < scores.len() {
        let sorted = sort_ascending(&scores);
        let threshold = sorted[sorted.len() - top_k as usize];
        for score in scores.iter_mut() {
            if *score < threshold {
                *score = f32::NEG_INFINITY;
            }
        }
    }
    if top_p > 0.0 && top_p < 1.0 {
        let mut indexed: Vec<(usize, f32)> = scores.iter().copied().enumerate().collect();
        indexed.sort_by(|left, right| right.1.partial_cmp(&left.1).unwrap_or(Ordering::Equal));
        let sorted_scores: Vec<f32> = indexed.iter().map(|item| item.1).collect();
        let sorted_probs = softmax(&sorted_scores);
        let mut remove_mask = vec![false; indexed.len()];
        let mut cumulative = 0.0f64;
        for (index, probability) in sorted_probs.iter().enumerate() {
            cumulative += *probability;
            if cumulative > top_p {
                remove_mask[index] = true;
            }
        }
        for index in (1..remove_mask.len()).rev() {
            remove_mask[index] = remove_mask[index - 1];
        }
        if let Some(first) = remove_mask.first_mut() {
            *first = false;
        }
        for (index, remove) in remove_mask.iter().enumerate() {
            if *remove {
                scores[indexed[index].0] = f32::NEG_INFINITY;
            }
        }
    }

    let probabilities = softmax(&scores);
    let mut random_value = rng.random();
    for (index, probability) in probabilities.iter().enumerate() {
        random_value -= *probability;
        if random_value <= 0.0 {
            return Ok(index);
        }
    }
    Ok(argmax(&scores))
}

/// 与 Python `_apply_repetition_penalty` 一致：重复 token 按符号缩放 logits。
pub fn apply_repetition_penalty(
    values: &[f32],
    previous_token_ids: &[i32],
    repetition_penalty: f64,
) -> Vec<f32> {
    let mut result = values.to_vec();
    if previous_token_ids.is_empty() || repetition_penalty == 1.0 {
        return result;
    }
    let unique: HashSet<i32> = previous_token_ids.iter().copied().collect();
    for token_id in unique {
        if token_id < 0 || token_id as usize >= result.len() {
            continue;
        }
        let value = result[token_id as usize];
        result[token_id as usize] = if value < 0.0 {
            value * repetition_penalty as f32
        } else {
            value / repetition_penalty as f32
        };
    }
    result
}

/// 与 Python `_argmax_with_repetition_penalty` 一致（不分配惩罚后的整段 logits）。
pub fn argmax_with_repetition_penalty(
    values: &[f32],
    previous_token_set: &HashSet<i32>,
    repetition_penalty: f64,
) -> usize {
    let mut best_index = 0usize;
    let mut best_value = f64::NEG_INFINITY;
    let apply_penalty = !previous_token_set.is_empty() && repetition_penalty != 1.0;
    for (index, value) in values.iter().enumerate() {
        let mut score = f64::from(*value);
        if apply_penalty && previous_token_set.contains(&(index as i32)) {
            score = if score < 0.0 {
                score * repetition_penalty
            } else {
                score / repetition_penalty
            };
        }
        if score > best_value {
            best_value = score;
            best_index = index;
        }
    }
    best_index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greedy_ignores_the_rng() {
        let mut rng = Pcg64::seeded(1234);
        let values = [0.1f32, 0.9, 0.5];
        assert_eq!(
            sample_from_scores(&values, false, 1.0, 0, 1.0, &mut rng).unwrap(),
            1
        );
        assert_eq!(argmax(&values), 1);
    }

    #[test]
    fn top_k_keeps_only_the_highest_scored_entries() {
        let mut rng = Pcg64::seeded(7);
        let values = [1.0f32, 5.0, 3.0, 2.0];
        for _ in 0..32 {
            let sampled = sample_from_scores(&values, true, 1.0, 1, 1.0, &mut rng).unwrap();
            assert_eq!(sampled, 1, "top_k=1 时只能取到最高分");
        }
    }

    #[test]
    fn repetition_penalty_matches_python_sign_handling() {
        let values = [-2.0f32, 4.0, 0.0];
        let penalized = apply_repetition_penalty(&values, &[0, 1, 1], 2.0);
        assert_eq!(penalized, vec![-4.0, 2.0, 0.0]);

        let disabled = apply_repetition_penalty(&values, &[0, 1], 1.0);
        assert_eq!(disabled, values.to_vec());
    }

    #[test]
    fn sample_mode_normalization_follows_python() {
        assert_eq!(normalize_sample_mode("mixed3", true), SAMPLE_MODE_FIXED);
        assert_eq!(normalize_sample_mode("mixed3", false), SAMPLE_MODE_GREEDY);
        assert_eq!(normalize_sample_mode("greedy", true), SAMPLE_MODE_GREEDY);
        assert_eq!(normalize_sample_mode("unknown", false), SAMPLE_MODE_GREEDY);
        assert_eq!(normalize_sample_mode("unknown", true), SAMPLE_MODE_FIXED);
    }

    #[test]
    fn softmax_sums_to_one_and_survives_negative_infinity() {
        let probabilities = softmax(&[1.0f32, f32::NEG_INFINITY, 2.0]);
        assert!((probabilities.iter().sum::<f64>() - 1.0).abs() < 1e-12);
        assert_eq!(probabilities[1], 0.0);
    }

    #[test]
    fn random_stream_is_reproducible_for_a_seed() {
        let mut first = Pcg64::seeded(42);
        let mut second = Pcg64::seeded(42);
        let values: Vec<f64> = (0..4).map(|_| first.random()).collect();
        let repeated: Vec<f64> = (0..4).map(|_| second.random()).collect();
        assert_eq!(values, repeated);
        assert!(values.iter().all(|value| (0.0..1.0).contains(value)));
    }
}
