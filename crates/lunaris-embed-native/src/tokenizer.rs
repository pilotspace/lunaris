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
use unicode_normalization::UnicodeNormalization;

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

        // why: NFC-normalize every input before encoding. The granite-r2
        // tokenizer.json does NOT include an NFC/NFKC normalizer step, so a
        // string in NFD form (e.g. iOS/macOS clipboards routinely emit NFD)
        // tokenizes into a completely different id sequence from the same
        // user-visible string in NFC. See
        // `.planning/phases/N-01-step-1-modernbert-fp16/P1-VERIFICATION-RESULT.md`
        // finding #1 — `"Tiếng Việt"` produced 4 ids in NFC vs 8 ids in NFD
        // and an embedding cosine of 0.8653 between the two forms.
        // NFC is chosen over NFKC deliberately: NFKC collapses compatibility
        // characters (full-width ↔ half-width Latin, ligatures, super/sub
        // scripts) which changes semantics for code/identifier embeddings.
        // NFC only re-composes canonically-equivalent sequences, which is
        // exactly what we want: same user-visible text → same tokens.
        let owned: Vec<String> = inputs.iter().map(|s| s.nfc().collect::<String>()).collect();
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

/// Compute `(real_tokens, padded_tokens)` from an `attention_mask` tensor of
/// shape `(batch, seq_len)`, `DType::U32`, where `1` = real token, `0` = pad.
///
/// Used exclusively by the tracing instrumentation (see
/// `docs/design/quantized-inference-extractor-reranker.md` §4b — the
/// microscope workstream) to attribute batch-assembly padding waste; this is
/// NOT on any correctness path, so callers MUST treat a candle error here as
/// best-effort (skip the span fields, never fail the actual embed call).
///
/// why `sum_all()` and not a host-side loop: the mask already lives on the
/// compute device (CPU/Metal/CUDA); reducing there and pulling back a single
/// scalar is cheaper than round-tripping the whole `(batch, seq_len)` tensor
/// to a `Vec` just to sum it host-side.
pub(crate) fn mask_token_stats(mask: &Tensor) -> candle_core::Result<(usize, usize)> {
    let (batch, seq_len) = mask.dims2()?;
    let total = batch * seq_len;
    let real = mask.sum_all()?.to_dtype(DType::U32)?.to_scalar::<u32>()? as usize;
    let padded = total.saturating_sub(real);
    Ok((real, padded))
}

#[cfg(test)]
mod mask_token_stats_tests {
    use super::*;

    #[test]
    fn mask_token_stats_counts_real_and_padded() -> candle_core::Result<()> {
        let device = Device::Cpu;
        // batch=2, seq_len=4: row 0 fully real, row 1 has 2 real + 2 pad.
        let mask = Tensor::from_vec(vec![1u32, 1, 1, 1, 1, 1, 0, 0], (2, 4), &device)?
            .to_dtype(DType::U32)?;
        let (real, padded) = mask_token_stats(&mask)?;
        assert_eq!(real, 6);
        assert_eq!(padded, 2);
        Ok(())
    }

    #[test]
    fn mask_token_stats_all_pad_edge_case() -> candle_core::Result<()> {
        let device = Device::Cpu;
        let mask = Tensor::zeros((1, 8), DType::U32, &device)?;
        let (real, padded) = mask_token_stats(&mask)?;
        assert_eq!(real, 0);
        assert_eq!(padded, 8);
        Ok(())
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

    /// Regression pin for the P1-1 finding: the granite-r2 tokenizer must
    /// produce identical token-id sequences for canonically-equivalent
    /// inputs (NFC vs NFD). This is the unit-level pre-image of the IT
    /// assertion in `tests/p1_correctness.rs::p1_correctness_panel` —
    /// without `nfc()` in `encode_batch`, "Tiếng Việt" tokenizes to 4 ids
    /// in NFC and 8 ids in NFD.
    ///
    /// Env-var-skipped because it needs the real `tokenizer.json`. Same
    /// convention as the `embedder-it` integration tests.
    #[test]
    fn encode_batch_normalizes_nfc_nfd_inputs() {
        use unicode_normalization::UnicodeNormalization;

        let Some(tokenizer_path) = std::env::var_os("GRANITE_R2_TOKENIZER_PATH") else {
            eprintln!(
                "[skip] encode_batch_normalizes_nfc_nfd_inputs — \
                 GRANITE_R2_TOKENIZER_PATH unset"
            );
            return;
        };

        let cfg = ModernBertConfig::granite_r2();
        let tok = GraniteTokenizer::from_file(&tokenizer_path, &cfg)
            .expect("load granite-r2 tokenizer.json");

        let device = Device::Cpu;

        // Five strings that exercise the worst-case combining sequences:
        // tone marks + horn (ư = u + U+031B), dot-below + circumflex
        // (ộ = o + U+0323 + U+0302), and stacked diacritics (ướng).
        let cases = [
            "Tiếng Việt",
            "phở bò tái nạm",
            "Hà Nội xinh đẹp",
            "cà phê sữa đá",
            "bánh mì thịt nướng",
        ];

        for original in cases {
            let nfc: String = original.nfc().collect();
            let nfd: String = original.nfd().collect();
            // sanity: the two forms have DIFFERENT bytes (otherwise the
            // test is vacuous).
            assert_ne!(
                nfc.as_bytes(),
                nfd.as_bytes(),
                "NFC and NFD bytes must differ for {original:?}"
            );

            let enc_nfc = tok.encode_batch(&[nfc.as_str()], &device).expect("encode NFC");
            let enc_nfd = tok.encode_batch(&[nfd.as_str()], &device).expect("encode NFD");

            let ids_nfc: Vec<u32> =
                enc_nfc.input_ids.flatten_all().unwrap().to_vec1::<u32>().unwrap();
            let ids_nfd: Vec<u32> =
                enc_nfd.input_ids.flatten_all().unwrap().to_vec1::<u32>().unwrap();

            assert_eq!(
                ids_nfc, ids_nfd,
                "NFC/NFD must produce identical token ids for {original:?} \
                 (got NFC={ids_nfc:?} vs NFD={ids_nfd:?}). If this fires, \
                 the `nfc()` pre-pass in encode_batch was removed."
            );
        }
    }
}
