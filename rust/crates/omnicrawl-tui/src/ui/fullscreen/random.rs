//! 轮播解密特效与留言抽取用的随机源。
//!
//! Python 侧直接用标准库 `random.Random`（测试可注入固定种子获取可复现输出）；
//! Rust 侧用内部确定性 PRNG（xorshift64*）顶替：字符集、概率与抽样语义对齐，
//! 随机序列本身与 Mersenne Twister 不同（见 crate README 的已知差异）。

use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    pub fn from_entropy() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0);
        Self::new(nanos ^ (std::process::id() as u64) << 32)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// 均匀分布于 `[0, 1)` 的浮点值，对映 Python 的 `Random.random()`。
    pub fn random(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// 均匀索引，对映 Python 的 `Random.randbelow`。
    pub fn index(&mut self, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        (self.next_u64() % len as u64) as usize
    }

    /// 等概率抽样，对映 Python 的 `Random.choice`。
    pub fn choice<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.index(items.len())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_seed_reproduces_sequence() {
        let mut left = Rng::new(7);
        let mut right = Rng::new(7);
        let left_values: Vec<u64> = (0..8).map(|_| left.next_u64()).collect();
        let right_values: Vec<u64> = (0..8).map(|_| right.next_u64()).collect();
        assert_eq!(left_values, right_values);
        assert!(left_values.iter().any(|value| *value != left_values[0]));
    }

    #[test]
    fn random_stays_in_unit_interval() {
        let mut rng = Rng::new(11);
        for _ in 0..200 {
            let value = rng.random();
            assert!((0.0..1.0).contains(&value), "越界：{value}");
        }
    }

    #[test]
    fn choice_and_index_stay_in_range() {
        let items = ['a', 'b', 'c'];
        let mut rng = Rng::new(13);
        for _ in 0..50 {
            assert!(items.contains(rng.choice(&items)));
            assert!(rng.index(items.len()) < items.len());
        }
        assert_eq!(rng.index(0), 0);
    }
}
