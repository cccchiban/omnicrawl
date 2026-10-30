//! 可自部署的 OneJev 尺寸清单：官方发布仓库、下载量、显存提示与界面文案的唯一来源。
//!
//! 清单只描述「仓库里有什么」，驱动下载、删除、显存提示与模型名（worker 的 `--model`），
//! 界面与配置都不再各留一份硬编码表。体积取仓库实际文件总量（权重 + 分词器 + 配置），
//! 用于下载前的空间提示与进度百分比。

/// 一个可部署尺寸。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OneJevSize {
    /// 尺寸键（也是仓库后缀），如 `0.8B`。
    pub key: &'static str,
    /// 界面文案，如 `0.8B（约 2.2 GB）`。
    pub label: &'static str,
    /// Hugging Face 仓库 id。
    pub repo_id: &'static str,
    /// 仓库文件总量（字节）。
    pub total_bytes: u64,
    /// 建议的显存下限（GB，按 bfloat16 权重 + 激活 + CUDA 图预留估算）。
    pub vram_gb: u64,
}

/// 官方发布的全部尺寸（顺序即界面顺序）。
pub const ONEJEV_SIZES: [OneJevSize; 5] = [
    OneJevSize {
        key: "0.8B",
        label: "0.8B（约 2.2 GB）",
        repo_id: "OmniJev/OneJev-0.8B",
        total_bytes: 2_234_579_789,
        vram_gb: 4,
    },
    OneJevSize {
        key: "4B",
        label: "4B（约 10.4 GB）",
        repo_id: "OmniJev/OneJev-4B",
        total_bytes: 10_369_356_123,
        vram_gb: 12,
    },
    OneJevSize {
        key: "9B",
        label: "9B（约 18.9 GB）",
        repo_id: "OmniJev/OneJev-9B",
        total_bytes: 18_841_928_104,
        vram_gb: 24,
    },
    OneJevSize {
        key: "27B-FP8",
        label: "27B-FP8（约 30.4 GB）",
        repo_id: "OmniJev/OneJev-27B-FP8",
        total_bytes: 30_423_428_747,
        vram_gb: 34,
    },
    OneJevSize {
        key: "27B",
        label: "27B（约 54.7 GB）",
        repo_id: "OmniJev/OneJev-27B",
        total_bytes: 54_735_927_480,
        vram_gb: 60,
    },
];

/// 按尺寸键查清单（大小写与首尾空白不敏感）。
pub fn find_size(key: &str) -> Option<OneJevSize> {
    let key = key.trim();
    ONEJEV_SIZES
        .iter()
        .copied()
        .find(|size| size.key.eq_ignore_ascii_case(key))
}

/// 清单里的界面文案。
pub fn size_labels() -> Vec<&'static str> {
    ONEJEV_SIZES.iter().map(|size| size.label).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_size_points_at_an_official_repo() {
        for size in ONEJEV_SIZES {
            assert_eq!(size.repo_id, format!("OmniJev/OneJev-{}", size.key));
            assert!(size.total_bytes > 0, "{} 缺少体积", size.key);
            assert!(size.label.contains(size.key), "文案里应带上尺寸键");
        }
    }

    #[test]
    fn lookup_is_lenient_and_never_empty() {
        assert_eq!(find_size(" 0.8b ").map(|size| size.key), Some("0.8B"));
        assert_eq!(size_labels().len(), ONEJEV_SIZES.len());
    }
}
