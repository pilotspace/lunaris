//! `PairTokenizer` — wraps `tokenizers::Tokenizer` for the XLM-R SentencePiece
//! BPE tokenizer that bge-reranker-v2-m3 ships, with batch pair-encoding tuned
//! for the cross-encoder rerank pass.
//!
//! ## Pair format
//!
//! XLM-R cross-encoders format pairs as:
//! ```text
//!   <s> query </s></s> doc </s>
//! ```
//! `tokenizers::Tokenizer::encode_batch` with `(String, String)` tuples emits
//! exactly this layout when the wrapped `tokenizer.json` ships the XLM-R
//! post-processor (which bge-reranker-v2-m3 does — verified against the live
//! `tokenizer.json` on HF Hub 2026-05-14).
//!
//! ## NFC normalization
//!
//! Both `query` and `doc` are normalized to Unicode NFC before encoding.
//! Matches `lunaris-embed-native::tokenizer::encode_batch`. Rationale: the
//! tokenizer.json does NOT include an NFC/NFKC normalizer step, so iOS/macOS
//! NFD input would tokenize into a different id sequence from the same
//! user-visible text in NFC. Cross-encoder calibration depends on the same
//! token stream the upstream training pipeline saw — that's NFC.

use std::path::Path;

use candle_core::{DType, Device, Tensor};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams, TruncationStrategy};
use unicode_normalization::UnicodeNormalization;

use crate::BGE_RERANKER_V2_M3_MAX_SEQ;

/// Errors raised while tokenizing or constructing a `PairTokenizer`.
#[derive(Debug, thiserror::Error)]
pub enum TokenizerError {
    #[error("tokenizer: failed to load from {path}: {message}")]
    Load { path: String, message: String },

    #[error("tokenizer: encode_batch failed: {0}")]
    Encode(String),

    #[error("tokenizer: tensor build failed: {0}")]
    Tensor(#[from] candle_core::Error),
}

/// Padded + truncated batch ready to feed into
/// [`crate::xlmr_reranker::XlmRobertaReranker`].
#[derive(Debug)]
pub struct EncodedPairBatch {
    /// Shape: `(batch, seq_len)`, `DType::U32`.
    pub input_ids: Tensor,
    /// Shape: `(batch, seq_len)`, `DType::U32` (1 = real token, 0 = pad).
    pub attention_mask: Tensor,
    /// Shape: `(batch, seq_len)`, `DType::U32`. XLM-R is single-segment so
    /// this is conventionally all zeros, but candle's `XLMRobertaModel`
    /// requires the tensor explicitly.
    pub token_type_ids: Tensor,
}

/// Wraps `tokenizers::Tokenizer` with pair-encode helpers tuned for the
/// bge-reranker-v2-m3 cross-encoder.
#[derive(Debug)]
pub struct PairTokenizer {
    inner: Tokenizer,
    pad_id: u32,
    max_len: usize,
}

impl PairTokenizer {
    /// Load `tokenizer.json` from disk and configure padding (BatchLongest,
    /// `pad_id` from the model config) + truncation (LongestFirst at
    /// [`BGE_RERANKER_V2_M3_MAX_SEQ`]).
    pub fn from_file<P: AsRef<Path>>(path: P, pad_id: u32) -> Result<Self, TokenizerError> {
        Self::from_file_with_max_len(path, pad_id, BGE_RERANKER_V2_M3_MAX_SEQ)
    }

    /// Same as [`Self::from_file`] but with a caller-supplied max sequence
    /// length. Used in tests to verify truncation behavior at smaller caps.
    pub fn from_file_with_max_len<P: AsRef<Path>>(
        path: P,
        pad_id: u32,
        max_len: usize,
    ) -> Result<Self, TokenizerError> {
        let p = path.as_ref();
        let mut tok = Tokenizer::from_file(p).map_err(|e| TokenizerError::Load {
            path: p.display().to_string(),
            message: e.to_string(),
        })?;

        // Padding: longest-in-batch with the model's pad id (1 for XLM-R).
        // Right-pad so position-0 (CLS) always lines up at index 0 regardless
        // of input length — the classification head reads position 0.
        tok.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            direction: tokenizers::PaddingDirection::Right,
            pad_to_multiple_of: None,
            pad_id,
            pad_type_id: 0,
            pad_token: "<pad>".to_string(),
        }));

        // Truncation: LongestFirst — for pair encoding this drops tokens from
        // the *longer* of the two segments first, preserving as much of the
        // shorter side as possible. This matches HF's default for cross-encoder
        // pair inputs (sentence-transformers `CrossEncoder` uses the same).
        tok.with_truncation(Some(TruncationParams {
            max_length: max_len,
            strategy: TruncationStrategy::LongestFirst,
            stride: 0,
            direction: tokenizers::TruncationDirection::Right,
        }))
        .map_err(|e| TokenizerError::Encode(format!("with_truncation: {e}")))?;

        Ok(Self { inner: tok, pad_id, max_len })
    }

    /// Encode a single `(query, doc)` pair. Convenience wrapper around
    /// [`Self::encode_pair_batch`] for callers that only need one pair.
    pub fn encode_pair(
        &self,
        query: &str,
        doc: &str,
        device: &Device,
    ) -> Result<EncodedPairBatch, TokenizerError> {
        self.encode_pair_batch(&[(query, doc)], device)
    }

    /// Encode a batch of `(query, doc)` pairs. NFC-normalizes each side
    /// individually before joining.
    pub fn encode_pair_batch(
        &self,
        pairs: &[(&str, &str)],
        device: &Device,
    ) -> Result<EncodedPairBatch, TokenizerError> {
        if pairs.is_empty() {
            return Err(TokenizerError::Encode(
                "encode_pair_batch called with empty pair slice".into(),
            ));
        }

        // why: NFC-normalize both halves of every pair before encoding.
        // Mirrors `lunaris-embed-native::tokenizer::encode_batch` (see that
        // module's docstring + N-01.5 finding #1). The bge-reranker-v2-m3
        // tokenizer.json does NOT include an NFC normalizer step, so an
        // NFD-form input tokenizes into a completely different id sequence
        // from the equivalent NFC-form. NFC (not NFKC) is chosen so we re-
        // compose canonically-equivalent sequences without collapsing
        // compatibility characters that change semantics (full-width Latin,
        // ligatures, super/subscripts) — important for code/identifier docs.
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(q, d)| (q.nfc().collect::<String>(), d.nfc().collect::<String>()))
            .collect();

        let encodings = self
            .inner
            .encode_batch(owned, /*add_special_tokens=*/ true)
            .map_err(|e| TokenizerError::Encode(e.to_string()))?;

        let batch = encodings.len();
        let seq_len = encodings.first().map(|e| e.get_ids().len()).unwrap_or(0);

        let mut ids_flat: Vec<u32> = Vec::with_capacity(batch * seq_len);
        let mut mask_flat: Vec<u32> = Vec::with_capacity(batch * seq_len);
        let mut type_flat: Vec<u32> = Vec::with_capacity(batch * seq_len);
        for enc in &encodings {
            ids_flat.extend_from_slice(enc.get_ids());
            mask_flat.extend_from_slice(enc.get_attention_mask());
            type_flat.extend_from_slice(enc.get_type_ids());
        }

        let input_ids =
            Tensor::from_vec(ids_flat, (batch, seq_len), device)?.to_dtype(DType::U32)?;
        let attention_mask =
            Tensor::from_vec(mask_flat, (batch, seq_len), device)?.to_dtype(DType::U32)?;
        let token_type_ids =
            Tensor::from_vec(type_flat, (batch, seq_len), device)?.to_dtype(DType::U32)?;

        Ok(EncodedPairBatch { input_ids, attention_mask, token_type_ids })
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
/// Mirrors `lunaris_embed_native::tokenizer::mask_token_stats` byte-for-byte
/// (same fallback policy on both sides of the embed/rerank split — see that
/// crate's doc comment). Used exclusively by the tracing instrumentation
/// (`docs/design/quantized-inference-extractor-reranker.md` §4b); a candle
/// error here MUST be treated as best-effort by the caller (skip the span
/// fields, never fail the actual rerank call).
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
    use super::*;

    #[test]
    fn from_file_with_missing_path_returns_load_error() {
        let err = PairTokenizer::from_file("/tmp/__lunaris_rerank_no_tokenizer.json", 1)
            .expect_err("missing path must fail");
        match err {
            TokenizerError::Load { .. } => {}
            other => panic!("expected Load error, got {other:?}"),
        }
    }

    /// Regression pin: when the real bge-reranker tokenizer is available, NFC
    /// vs NFD inputs MUST produce identical token-id sequences. Mirrors the
    /// embedder-side regression test. Env-var-skipped so CI without the
    /// tokenizer.json stays green.
    #[test]
    fn encode_pair_normalizes_nfc_nfd_inputs() {
        let Some(tokenizer_path) = std::env::var_os("BGE_RERANKER_TOKENIZER_PATH") else {
            eprintln!(
                "[skip] encode_pair_normalizes_nfc_nfd_inputs — \
                 BGE_RERANKER_TOKENIZER_PATH unset"
            );
            return;
        };

        let tok = PairTokenizer::from_file(&tokenizer_path, 1)
            .expect("load bge-reranker-v2-m3 tokenizer.json");
        let device = Device::Cpu;

        // Vietnamese diacritics (stacked combining sequences) are the worst
        // case for NFC/NFD disagreement.
        let cases = [
            ("Tiếng Việt là gì?", "Tiếng Việt là một ngôn ngữ"),
            ("phở bò ngon nhất ở đâu?", "phở bò tái nạm ở Hà Nội"),
            ("cà phê sữa đá", "cà phê sữa đá Việt Nam nổi tiếng"),
        ];

        for (q, d) in cases {
            let nfc_q: String = q.nfc().collect();
            let nfc_d: String = d.nfc().collect();
            let nfd_q: String = q.nfd().collect();
            let nfd_d: String = d.nfd().collect();
            assert_ne!(
                nfc_q.as_bytes(),
                nfd_q.as_bytes(),
                "NFC and NFD bytes must differ for query {q:?}"
            );

            let enc_nfc =
                tok.encode_pair(nfc_q.as_str(), nfc_d.as_str(), &device).expect("encode NFC pair");
            let enc_nfd =
                tok.encode_pair(nfd_q.as_str(), nfd_d.as_str(), &device).expect("encode NFD pair");

            let ids_nfc: Vec<u32> =
                enc_nfc.input_ids.flatten_all().unwrap().to_vec1::<u32>().unwrap();
            let ids_nfd: Vec<u32> =
                enc_nfd.input_ids.flatten_all().unwrap().to_vec1::<u32>().unwrap();
            assert_eq!(
                ids_nfc, ids_nfd,
                "NFC/NFD must produce identical token ids for ({q:?}, {d:?}) — \
                 if this fires, the `nfc()` pre-pass in encode_pair_batch was removed."
            );
        }
    }
}
