//! sentencepiece 分词：与 Python `spm.SentencePieceProcessor.encode` 逐 token 一致。
//!
//! 模型自带 `tokenizer.model`（sentencepiece BPE + nmt_nfkc 归一化 + byte fallback）。
//! 这里用纯 Rust 实现（`sentencepiece-rs`）：与 ONNX Runtime 共存时不会像 C++ 静态库
//! 那样与 ORT 自带 protobuf 撞符号，也免去 C++ 工具链。

use std::path::Path;

use sentencepiece_rs::SentencePieceProcessor;

pub struct TtsTokenizer {
    processor: SentencePieceProcessor,
}

impl TtsTokenizer {
    pub fn open(path: &Path) -> Result<Self, String> {
        let processor = SentencePieceProcessor::open(path)
            .map_err(|error| format!("加载分词器 {} 失败：{error}", path.display()))?;
        Ok(Self { processor })
    }

    pub fn vocab_size(&self) -> usize {
        self.processor.model().vocab_size()
    }

    /// 文本 → token id 序列。
    pub fn encode(&self, text: &str) -> Result<Vec<i32>, String> {
        let ids = self
            .processor
            .encode_to_ids(text)
            .map_err(|error| format!("分词失败：{error}"))?;
        Ok(ids.into_iter().map(|id| id as i32).collect())
    }

    pub fn count(&self, text: &str) -> Result<usize, String> {
        Ok(self.encode(text)?.len())
    }
}
