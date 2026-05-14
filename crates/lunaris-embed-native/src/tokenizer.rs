//! `GraniteTokenizer` — a thin wrapper around `tokenizers::Tokenizer` configured
//! for granite-r2's XLM-R-style BPE (180k vocab, multilingual). Responsibilities:
//!
//! - Load from a HuggingFace `tokenizer.json` file.
//! - Encode a batch of `&str` with padding to longest-in-batch and truncation
//!   at `max_position_embeddings`.
//! - Emit `(input_ids, attention_mask)` as `candle_core::Tensor`s on the
//!   caller-supplied device.

use std::path::Path;

use candle_core::{DType, Device, Tensor};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams, TruncationStrategy};

use crate::config::ModernBertConfig;

/// Errors raised while tokenizing or constructing a `GraniteTokenizer`.
#[derive(Debug, thiserror::Error)]
pub enum TokenizerError {
    #[error("tokenizer: failed to load from {path}: {message}")]
    Load { path: String, message: String },

    #[error("tokenizer: encode_batch failed: {0}")]
    Encode(String),

    #[error("tokenizer: tensor build failed: {0}")]
    Tensor(#[from] candle_core::Error),
}

/// Padded + truncated batch ready to feed into [`crate::modernbert::ModernBert`].
#[derive(Debug)]
pub struct EncodedBatch {
    /// Shape: `(batch, seq_len)`, `DType::U32`.
    pub input_ids: Tensor,
    /// Shape: `(batch, seq_len)`, `DType::U32` (1 = real token, 0 = pad).
    pub attention_mask: Tensor,
}

/// Wraps `tokenizers::Tokenizer` with batch-encode helpers tuned for granite-r2.
#[derive(Debug)]
pub struct GraniteTokenizer {
    inner: Tokenizer,
    pad_id: u32,
    max_len: usize,
}

impl GraniteTokenizer {
    /// Load from `tokenizer.json` and configure padding/truncation per the
    /// model config.
    pub fn from_file<P: AsRef<Path>>(
        path: P,
        cfg: &ModernBertConfig,
    ) -> Result<Self, TokenizerError> {
        let p = path.as_ref();
        let mut tok = Tokenizer::from_file(p).map_err(|e| TokenizerError::Load {
            path: p.display().to_string(),
            message: e.to_string(),
        })?;

        // Padding: longest-in-batch, pad_id from config so the attention mask
        // marks pads correctly.
        tok.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            direction: tokenizers::PaddingDirection::Right,
            pad_to_multiple_of: None,
            pad_id: cfg.pad_token_id,
            pad_type_id: 0,
            pad_token: "[PAD]".to_string(),
        }));

        // Truncation: at config.max_position_embeddings; encoder-only, drop
        // overflow.
        tok.with_truncation(Some(TruncationParams {
            max_length: cfg.max_position_embeddings,
            strategy: TruncationStrategy::LongestFirst,
            stride: 0,
            direction: tokenizers::TruncationDirection::Right,
        }))
        .map_err(|e| TokenizerError::Encode(format!("with_truncation: {e}")))?;

        Ok(Self { inner: tok, pad_id: cfg.pad_token_id, max_len: cfg.max_position_embeddings })
    }

    /// Encode a batch. Returns tensors on `device` ready for the model.
    pub fn encode_batch(
        &self,
        inputs: &[&str],
        device: &Device,
    ) -> Result<EncodedBatch, TokenizerError> {
        if inputs.is_empty() {
            return Err(TokenizerError::Encode(
                "encode_batch called with empty input slice".into(),
            ));
        }

        let owned: Vec<String> = inputs.iter().map(|s| (*s).to_string()).collect();
        let encodings = self
            .inner
            .encode_batch(owned, /*add_special_tokens=*/ true)
            .map_err(|e| TokenizerError::Encode(e.to_string()))?;

        let batch = encodings.len();
        let seq_len = encodings.first().map(|e| e.get_ids().len()).unwrap_or(0);

        let mut ids_flat: Vec<u32> = Vec::with_capacity(batch * seq_len);
        let mut mask_flat: Vec<u32> = Vec::with_capacity(batch * seq_len);
        for enc in &encodings {
            ids_flat.extend_from_slice(enc.get_ids());
            mask_flat.extend_from_slice(enc.get_attention_mask());
        }

        let input_ids =
            Tensor::from_vec(ids_flat, (batch, seq_len), device)?.to_dtype(DType::U32)?;
        let attention_mask =
            Tensor::from_vec(mask_flat, (batch, seq_len), device)?.to_dtype(DType::U32)?;

        Ok(EncodedBatch { input_ids, attention_mask })
    }

    /// Pad-token id (forwarded from the model config).
    pub fn pad_id(&self) -> u32 {
        self.pad_id
    }

    /// Hard cap on encoded sequence length.
    pub fn max_len(&self) -> usize {
        self.max_len
    }
}

#[cfg(test)]
mod tests {
    // Offline tests run only the type-level checks; the live tokenizer roundtrip
    // belongs to the `embedder-it` integration suite where the real tokenizer.json
    // is available. Pure-unit verification of `tokenizer.json` is gated behind
    // the same env var as the numerical-equivalence test so CI without weights
    // stays green.
    use super::*;

    #[test]
    fn from_file_with_missing_path_returns_load_error() {
        let cfg = ModernBertConfig::granite_r2();
        let err = GraniteTokenizer::from_file("/tmp/__lunaris_does_not_exist.json", &cfg)
            .expect_err("missing path must fail");
        match err {
            TokenizerError::Load { .. } => {}
            other => panic!("expected Load error, got {other:?}"),
        }
    }
}
