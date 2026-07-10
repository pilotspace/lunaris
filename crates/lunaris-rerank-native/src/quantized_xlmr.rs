//! `quantized_xlmr` — Q4_K_M XLM-RoBERTa forward pass for `bge-reranker-v2-m3`.
//!
//! Structurally mirrors `candle_transformers::models::xlm_roberta` (the FP32
//! reference path the `XlmRobertaReranker` already validates against HF).
//! Linear projections become `QMatMul`; bias terms + LayerNorm scales/biases
//! stay F32 (all stored as F32 in the GGUF — the manifest confirms 246/393
//! tensors are F32). Forward math is otherwise byte-for-byte the same:
//! 24 layers of all-global attention, learned absolute position embeddings
//! with the XLM-R position-id trick, GELU MLP, post-norm pattern, CLS pool,
//! classification head dense(1024→1024) → tanh → out_proj(1024→1) → sigmoid.
//!
//! ## XLM-RoBERTa vs ModernBERT (granite-r2)
//!
//! | Aspect            | granite-r2 (N-01.5)        | bge-reranker-v2-m3 (this) |
//! |-------------------|-----------------------------|----------------------------|
//! | Position embed    | RoPE per-layer              | Learned absolute (pad-shift)|
//! | Attention scope   | Alternating local/global    | All global                 |
//! | MLP               | GeGLU (fused gate+up)       | GELU (separate up→down)    |
//! | Norm              | LayerNorm no-bias, pre-norm | LayerNorm w/ bias, post-norm|
//! | Type embed        | none                        | Yes, single row            |
//! | Head              | none (pooled output)        | dense + tanh + out_proj    |
//! | Linear biases     | none                        | every linear has one (F32) |
//!
//! ## Position-id derivation (XLM-R-specific)
//!
//! Pads get `pad_id`, real tokens get `pad_id + 1 + offset_in_unpadded`:
//!
//! ```text
//! position_ids = (cumsum(mask, dim=1) * mask).to(U32) + pad_id
//! ```
//!
//! For `mask = [1,1,1,1,1,0,0,0,0,0]` with `pad_id = 1`:
//!     `cumsum * mask = [1,2,3,4,5,0,0,0,0,0]`
//!     `+ pad_id      = [2,3,4,5,6,1,1,1,1,1]`
//!
//! `arange(0..seq_len)` (the BERT pattern) is **wrong** here — would produce
//! position 0..seq_len-1 and silently drift the embedding lookup.
//!
//! ## Per-tensor dtype mix (from the manifest)
//!
//! GGUF Q4_K_M for bge-reranker-v2-m3 mixes types per tensor:
//! - All `*.bias` and `*_norm.{weight,bias}` → F32 (never quantized)
//! - `position_embd.weight` → F32 (already a small table on the 1024 axis)
//! - `token_types.weight` → F32 (single row)
//! - `token_embd.weight` → Q6_K
//! - `attn_{q,k,output}.weight`, `ffn_up.weight` → Q4_K
//! - `attn_v.weight`, `ffn_down.weight` → mostly Q4_K, ~25 tensors at Q6_K
//!   (llama-quantize's adaptive "important tensors" mix)
//! - `cls.weight` → Q4_K (dense classifier weight)
//! - `cls.output.weight` → F16 (1024-d row vector, kept higher precision)
//!
//! `QMatMul::from_qtensor` handles every supported quantization type
//! uniformly; we never branch on dtype. F16/F32 tensors are dequantized at
//! load and kept as plain `Tensor`s.

use std::path::Path;

use candle_core::Device;
use candle_core::quantized::{QMatMul, gguf_file};
use candle_core::{D, DType, IndexOp, Module, Tensor};
use candle_nn::{LayerNorm, VarBuilder, ops::softmax};

use crate::config::XlmRobertaRerankerConfig;

/// Errors raised by the quantized path. Mirrors `ForwardError` (FP32 path)
/// so callers can do uniform error mapping into
/// `NativeQuantizedRerankerError`.
#[derive(Debug, thiserror::Error)]
pub enum QuantizedRerankError {
    #[error("candle: {0}")]
    Candle(#[from] candle_core::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("gguf: {reason}")]
    Gguf { reason: String },

    #[error("forward: model returned shape {actual:?}, expected (batch, {expected})")]
    UnexpectedShape { actual: Vec<usize>, expected: usize },
}

/// One transformer layer (XLM-R encoder block). Stores all 7 linears
/// (q/k/v/attn_output/ffn_up/ffn_down) as `QMatMul` plus their F32 biases,
/// and the two post-norms (`attn_output_norm` after attention residual,
/// `layer_output_norm` after FFN residual).
struct Layer {
    attn_q: QMatMul,
    attn_q_bias: Tensor,
    attn_k: QMatMul,
    attn_k_bias: Tensor,
    attn_v: QMatMul,
    attn_v_bias: Tensor,
    attn_output: QMatMul,
    attn_output_bias: Tensor,
    /// Post-attention norm (XLM-R's `attention.output.LayerNorm`).
    attn_output_norm: LayerNorm,
    ffn_up: QMatMul,
    ffn_up_bias: Tensor,
    ffn_down: QMatMul,
    ffn_down_bias: Tensor,
    /// Post-FFN norm (XLM-R's `output.LayerNorm`).
    layer_output_norm: LayerNorm,
    num_heads: usize,
    head_dim: usize,
}

/// Classification head: dense(hidden→hidden) → tanh → out_proj(hidden→1).
///
/// `cls.weight` is loaded as `QMatMul`. `cls.output.weight` is a 1024-d F16
/// row vector — too small to bother quantizing; loaded as a plain F32
/// tensor (after the F16 dequant) and used via `matmul(&w.unsqueeze(1))` to
/// produce the `(batch, 1)` logit.
struct ClassifierHead {
    /// dense weight `[1024, 1024]` Q4_K
    dense: QMatMul,
    /// dense bias `[1024]` F32
    dense_bias: Tensor,
    /// out_proj weight `[1024, 1]` F32 (loaded from `cls.output.weight`
    /// `[1024]` F16, dequantized + unsqueezed to a column matrix at load
    /// time so the forward pass can call `matmul` without per-batch
    /// reshapes).
    out_proj_w: Tensor,
    /// out_proj bias `[1]` F32 (broadcast-added to the `(batch, 1)` logits).
    out_proj_bias: Tensor,
}

/// Q4_K_M XLM-RoBERTa cross-encoder loaded from a GGUF file. Forward pass
/// (`score`) returns sigmoid-bounded scores in `[0, 1]`, matching the FP32
/// path's output contract.
pub struct QuantizedXlmRoberta {
    cfg: XlmRobertaRerankerConfig,
    device: Device,
    /// Word embeddings — `token_embd` dequantized to F32 once at load.
    /// 250 002 × 1024 F32 = ~977 MiB resident. Same lower-RSS rationale as
    /// the embed-native Q4 path: candle's CPU `dequantize_f16` does a
    /// transient F32 alloc anyway, so F32-direct is the simpler choice.
    word_embd: Tensor,
    /// Token-type embeddings. XLM-R has `type_vocab_size = 1`, so this is
    /// a single `[1, 1024]` row that we broadcast-add to every position.
    /// Stored at F32 (manifest dtype).
    token_type_embd: Tensor,
    /// Position embeddings. Manifest shape `[1024, 8192]` (the underlying
    /// dequant layout is `[loaded_max_positions, hidden]`). Stored F32.
    position_embd: Tensor,
    /// `loaded_max_positions` from the GGUF's position table — may differ
    /// from `cfg.max_position_embeddings` by 2 (the converter sometimes
    /// trims the leading `pad_id+1` positions). We use this for runtime
    /// index_select range checks rather than `cfg.max_position_embeddings`.
    loaded_max_positions: usize,
    /// Post-embedding LayerNorm (`embeddings.LayerNorm`). F32 (manifest).
    embd_norm: LayerNorm,
    /// 24 transformer layers.
    layers: Vec<Layer>,
    /// Classification head (dense + tanh + out_proj).
    classifier: ClassifierHead,
}

impl QuantizedXlmRoberta {
    /// Load the model from a GGUF file. `cfg` MUST already be validated
    /// (use `XlmRobertaRerankerConfig::try_from_json_path`) — we do NOT
    /// re-parse the config from GGUF metadata (mirrors the embed Q4
    /// path's lesson on `pooling_type`).
    pub fn load(
        path: &Path,
        cfg: &XlmRobertaRerankerConfig,
        device: &Device,
    ) -> Result<Self, QuantizedRerankError> {
        let mut file = std::fs::File::open(path)?;
        let content = gguf_file::Content::read(&mut file)
            .map_err(|e| QuantizedRerankError::Gguf { reason: format!("read header: {e}") })?;

        // ---- helpers ----
        let qmat =
            |reader: &mut std::fs::File, name: &str| -> Result<QMatMul, QuantizedRerankError> {
                let qt = content.tensor(reader, name, device).map_err(|e| {
                    QuantizedRerankError::Gguf { reason: format!("tensor {name}: {e}") }
                })?;
                QMatMul::from_qtensor(qt).map_err(QuantizedRerankError::Candle)
            };
        let load_f32 =
            |reader: &mut std::fs::File, name: &str| -> Result<Tensor, QuantizedRerankError> {
                let qt = content.tensor(reader, name, device).map_err(|e| {
                    QuantizedRerankError::Gguf { reason: format!("tensor {name}: {e}") }
                })?;
                Ok(qt.dequantize(device)?.to_dtype(DType::F32)?)
            };
        // LayerNorm helper: GGUF stores `weight` + `bias` as separate F32
        // tensors (the XLM-R LN uses both). The candle LayerNorm
        // constructor wants the weight as the "scale" tensor and a
        // separate bias.
        let load_ln =
            |reader: &mut std::fs::File, prefix: &str| -> Result<LayerNorm, QuantizedRerankError> {
                let w = load_f32(reader, &format!("{prefix}.weight"))?;
                let b = load_f32(reader, &format!("{prefix}.bias"))?;
                // candle_nn::LayerNorm::new(weight, bias, eps) — direct ctor.
                Ok(LayerNorm::new(w, b, cfg.layer_norm_eps))
            };

        // ---- token_embd ----
        let word_qt = content.tensor(&mut file, "token_embd.weight", device).map_err(|e| {
            QuantizedRerankError::Gguf { reason: format!("tensor token_embd.weight: {e}") }
        })?;
        let word_embd = word_qt.dequantize(device)?.to_dtype(DType::F32)?;
        let we_dims = word_embd.dims().to_vec();
        // Candle's QTensor::dequantize returns the row-first natural shape
        // `[vocab_size, hidden_size]` even though the GGUF metadata view is
        // `[hidden, vocab]`. The embed-Q4 path confirmed this on granite-r2.
        if we_dims != [cfg.vocab_size, cfg.hidden_size] {
            return Err(QuantizedRerankError::Gguf {
                reason: format!(
                    "token_embd shape {we_dims:?} != [{}, {}]",
                    cfg.vocab_size, cfg.hidden_size
                ),
            });
        }

        // ---- position_embd ----
        //
        // Manifest shows shape `[1024, 8192]` (hidden, max_positions). After
        // dequant the row-first natural layout is `[loaded_max_positions,
        // hidden]`. cfg.max_position_embeddings == 8194 — the converter
        // sometimes drops the `pad_id + 1` leading rows that XLM-R never
        // uses. We bind a local `loaded_max_positions` for the range check
        // and don't assert equality with the config.
        let position_embd = load_f32(&mut file, "position_embd.weight")?;
        let pe_dims = position_embd.dims().to_vec();
        if pe_dims.len() != 2 || pe_dims[1] != cfg.hidden_size {
            return Err(QuantizedRerankError::Gguf {
                reason: format!(
                    "position_embd shape {pe_dims:?} expected [_, {}]",
                    cfg.hidden_size
                ),
            });
        }
        let loaded_max_positions = pe_dims[0];

        // ---- token_types ----
        //
        // Manifest shape `[1024]` (single row for type_vocab_size = 1). We
        // reshape to `[1, 1024]` so it can be broadcast-added across the
        // (batch, seq, hidden) embedding tensor.
        let tt = load_f32(&mut file, "token_types.weight")?;
        let token_type_embd = match tt.dims() {
            [hidden] if *hidden == cfg.hidden_size => tt.reshape((1, cfg.hidden_size))?,
            [n, hidden] if *hidden == cfg.hidden_size && *n == cfg.type_vocab_size => tt,
            other => {
                return Err(QuantizedRerankError::Gguf {
                    reason: format!(
                        "token_types shape {other:?} expected [{}] or [{}, {}]",
                        cfg.hidden_size, cfg.type_vocab_size, cfg.hidden_size
                    ),
                });
            }
        };

        let embd_norm = load_ln(&mut file, "token_embd_norm")?;

        // ---- transformer layers ----
        let num_heads = cfg.num_attention_heads;
        let head_dim = cfg.hidden_size / num_heads;
        let mut layers = Vec::with_capacity(cfg.num_hidden_layers);
        for i in 0..cfg.num_hidden_layers {
            let p = format!("blk.{i}");
            let attn_q = qmat(&mut file, &format!("{p}.attn_q.weight"))?;
            let attn_q_bias = load_f32(&mut file, &format!("{p}.attn_q.bias"))?;
            let attn_k = qmat(&mut file, &format!("{p}.attn_k.weight"))?;
            let attn_k_bias = load_f32(&mut file, &format!("{p}.attn_k.bias"))?;
            let attn_v = qmat(&mut file, &format!("{p}.attn_v.weight"))?;
            let attn_v_bias = load_f32(&mut file, &format!("{p}.attn_v.bias"))?;
            let attn_output = qmat(&mut file, &format!("{p}.attn_output.weight"))?;
            let attn_output_bias = load_f32(&mut file, &format!("{p}.attn_output.bias"))?;
            let attn_output_norm = load_ln(&mut file, &format!("{p}.attn_output_norm"))?;
            let ffn_up = qmat(&mut file, &format!("{p}.ffn_up.weight"))?;
            let ffn_up_bias = load_f32(&mut file, &format!("{p}.ffn_up.bias"))?;
            let ffn_down = qmat(&mut file, &format!("{p}.ffn_down.weight"))?;
            let ffn_down_bias = load_f32(&mut file, &format!("{p}.ffn_down.bias"))?;
            let layer_output_norm = load_ln(&mut file, &format!("{p}.layer_output_norm"))?;
            layers.push(Layer {
                attn_q,
                attn_q_bias,
                attn_k,
                attn_k_bias,
                attn_v,
                attn_v_bias,
                attn_output,
                attn_output_bias,
                attn_output_norm,
                ffn_up,
                ffn_up_bias,
                ffn_down,
                ffn_down_bias,
                layer_output_norm,
                num_heads,
                head_dim,
            });
        }

        // ---- classifier head ----
        let dense = qmat(&mut file, "cls.weight")?;
        let dense_bias = load_f32(&mut file, "cls.bias")?;
        // `cls.output.weight` is `[1024]` F16. Dequantize → F32, then
        // unsqueeze to `[1024, 1]` so the forward pass can call
        // `cls_hidden.matmul(&w_col)` to produce `(batch, 1)` logits.
        let out_proj_row = load_f32(&mut file, "cls.output.weight")?;
        let out_proj_w = match out_proj_row.dims() {
            [hidden] if *hidden == cfg.hidden_size => out_proj_row.reshape((cfg.hidden_size, 1))?,
            [hidden, one] if *hidden == cfg.hidden_size && *one == 1 => out_proj_row,
            other => {
                return Err(QuantizedRerankError::Gguf {
                    reason: format!(
                        "cls.output.weight shape {other:?} expected [{}] or [{}, 1]",
                        cfg.hidden_size, cfg.hidden_size
                    ),
                });
            }
        };
        let out_proj_bias = load_f32(&mut file, "cls.output.bias")?;
        let classifier = ClassifierHead { dense, dense_bias, out_proj_w, out_proj_bias };

        Ok(Self {
            cfg: cfg.clone(),
            device: device.clone(),
            word_embd,
            token_type_embd,
            position_embd,
            loaded_max_positions,
            embd_norm,
            layers,
            classifier,
        })
    }

    /// Run the forward pass on a tokenized batch and emit `(batch,)` F32
    /// sigmoid scores in `[0, 1]`. Signature mirrors
    /// `crate::xlmr_reranker::XlmRobertaReranker::score`.
    pub fn score(
        &self,
        input_ids: &Tensor,
        attention_mask: &Tensor,
        _token_type_ids: &Tensor,
    ) -> Result<Tensor, QuantizedRerankError> {
        let dims = input_ids.dims().to_vec();
        if dims.len() != 2 {
            return Err(QuantizedRerankError::UnexpectedShape {
                actual: dims,
                expected: self.cfg.hidden_size,
            });
        }
        let batch = dims[0];
        let seq_len = dims[1];
        let device = input_ids.device();

        // Span-per-call: mask prep through the Q5 transformer stack is the
        // candle-quantized-matmul-kernel stage the §4b microscope decision
        // rule is built around — see
        // `docs/design/quantized-inference-extractor-reranker.md` §4b/§4c.
        let _forward_enter = tracing::trace_span!(
            "lunaris.rerank.forward",
            batch_size = batch,
            max_seq_len = seq_len,
            quant = "q5_k_m"
        )
        .entered();

        // ---------- attention mask (4D additive) ----------
        let attn_mask_4d = prepare_4d_attention_mask(attention_mask)?.to_device(device)?;

        // ---------- embeddings ----------
        //
        // 1. Word lookup: `index_select` against the F32 vocab table.
        let flat_ids = input_ids.flatten_all()?;
        let mut xs = self.word_embd.index_select(&flat_ids, 0)?;
        xs = xs.reshape((batch, seq_len, self.cfg.hidden_size))?;

        // 2. Token-type add. XLM-R has type_vocab_size = 1 → the single
        //    row in `token_type_embd` is broadcast-added to every position.
        //    `token_type_embd` shape: `[1, hidden]`. Broadcast to (batch, seq, hidden).
        let tt = self
            .token_type_embd
            .unsqueeze(0)? // (1, 1, hidden)
            .broadcast_as((batch, seq_len, self.cfg.hidden_size))?;
        xs = xs.broadcast_add(&tt)?;

        // 3. Position lookup with the XLM-R pad-shift trick.
        //    position_ids = (cumsum(mask, dim=1) * mask) + pad_id, then U32.
        //    Real tokens get pad_id + 1 + offset; pads get pad_id.
        //
        //    GGUF converter offset: safetensors has `[max_position_embeddings,
        //    hidden]` = `[8194, 1024]` for XLM-R-large, but the GGUF position
        //    table is `[loaded_max_positions, hidden]` = `[8192, 1024]` — the
        //    converter drops the leading `(max_position_embeddings -
        //    loaded_max_positions) = 2` rows (positions `0` and `1`, never
        //    referenced because XLM-R's pad-shift trick starts indexing at
        //    pad_id + 1 = 2 for the first real token, and pad position is
        //    attention-masked out anyway). Subtract that offset before
        //    indexing — without the subtract, every real-token position lookup
        //    is off by 2 rows, producing 1-3 logit drift even with full
        //    dequant. Discovered via CANDLE_DEQUANTIZE_ALL=1 still showing
        //    structural drift on the equivalence test.
        let pad_id = self.cfg.pad_token_id;
        let pos_offset = self.cfg.max_position_embeddings.saturating_sub(self.loaded_max_positions);
        let mask_f32 = attention_mask.to_dtype(DType::F32)?;
        let cumsum = mask_f32.cumsum(1)?;
        // (cumsum * mask) keeps pad positions at 0, real positions at 1..n
        let active = (cumsum * &mask_f32)?;
        // + pad_id (broadcast as a scalar via from_vec → broadcast_as)
        // — pos_offset (saturating subtract for non-pad lookup table parity)
        //
        // pad positions land at `pad_id - pos_offset` = `1 - 2` which would
        // underflow in U32; saturating_sub to 0. Pad embeddings are
        // attention-masked out downstream so the chosen row doesn't matter
        // (and row 0 of the table = original safetensors position 2 is no
        // worse than any other).
        let shifted_scalar_f32 = (pad_id as f32) - (pos_offset as f32);
        let bias_scalar = Tensor::from_slice(&[shifted_scalar_f32], (1,), device)?
            .broadcast_as(active.shape())?;
        let pos_f32 = (active + bias_scalar)?;
        // Clamp negative pad rows to 0 (only relevant if pos_offset > pad_id;
        // for XLM-R-large pos_offset=2 and pad_id=1, the bias is -1 — pad
        // positions get -1 + cumsum=0 = -1 which would underflow U32).
        let pos_f32 = pos_f32.maximum(&Tensor::zeros_like(&pos_f32)?)?;
        let position_ids = pos_f32.to_dtype(DType::U32)?;

        // Range-check: max value in position_ids must be < loaded_max_positions.
        // For seq_len ≤ 512 with pad_id = 1 this is always satisfied; the
        // check is cheap insurance against an unusually long batch the
        // tokenizer slipped through despite the 512-cap.
        // (We skip the runtime max() reduction since the tokenizer guarantees
        // seq_len ≤ BGE_RERANKER_V2_M3_MAX_SEQ; document the precondition.)
        let pe = self.position_embd.index_select(&position_ids.flatten_all()?, 0)?.reshape((
            batch,
            seq_len,
            self.cfg.hidden_size,
        ))?;
        xs = (xs + pe)?;

        // 4. Post-embedding LayerNorm.
        xs = xs.apply(&self.embd_norm)?;

        // ---------- transformer stack ----------
        for layer in self.layers.iter() {
            xs = layer.forward(&xs, &attn_mask_4d)?;
        }
        drop(_forward_enter);

        let _extract_enter = tracing::trace_span!(
            "lunaris.rerank.score_extract",
            batch_size = batch,
            max_seq_len = seq_len,
            quant = "q5_k_m"
        )
        .entered();

        // ---------- classifier head ----------
        //
        // 1. CLS pool at position 0: shape `(batch, hidden)`.
        let cls = xs.i((.., 0, ..))?.contiguous()?;
        // 2. Dense + bias.
        let h = cls.apply(&self.classifier.dense)?;
        let h_bias = self.classifier.dense_bias.broadcast_as(h.shape())?;
        let h = (h + h_bias)?;
        // 3. Tanh.
        let h = h.tanh()?;
        // 4. out_proj — `(batch, hidden) @ (hidden, 1) = (batch, 1)`.
        let logits = h.matmul(&self.classifier.out_proj_w)?;
        let op_bias = self.classifier.out_proj_bias.broadcast_as(logits.shape())?;
        let logits = (logits + op_bias)?;
        // 5. Squeeze trailing dim → `(batch,)`.
        let logits_squeezed = logits.i((.., 0))?.contiguous()?;
        // 6. Sigmoid in F32 — matches the FP32 reference path.
        let logits_f32 = logits_squeezed.to_dtype(DType::F32)?;
        let scores = candle_nn::ops::sigmoid(&logits_f32)?;
        Ok(scores)
    }

    /// Diagnostic-only: return the post-encoder hidden state `(b, s, h)`
    /// (everything `score` does up to but NOT including the classifier head).
    /// Used by `tests/quantized_equivalence.rs` to localise drift to either
    /// the encoder or the classifier. If `stop_after_layer` is `Some(k)`,
    /// only the first `k` layers are applied (use `Some(0)` to get the post-
    /// embedding tensor, `Some(24)` or `None` for the full encoder output).
    #[doc(hidden)]
    pub fn forward_hidden_upto(
        &self,
        input_ids: &Tensor,
        attention_mask: &Tensor,
        stop_after_layer: Option<usize>,
    ) -> Result<Tensor, QuantizedRerankError> {
        let dims = input_ids.dims().to_vec();
        if dims.len() != 2 {
            return Err(QuantizedRerankError::UnexpectedShape {
                actual: dims,
                expected: self.cfg.hidden_size,
            });
        }
        let batch = dims[0];
        let seq_len = dims[1];
        let device = input_ids.device();

        let attn_mask_4d = prepare_4d_attention_mask(attention_mask)?.to_device(device)?;

        let flat_ids = input_ids.flatten_all()?;
        let mut xs = self.word_embd.index_select(&flat_ids, 0)?;
        xs = xs.reshape((batch, seq_len, self.cfg.hidden_size))?;

        let tt = self.token_type_embd.unsqueeze(0)?.broadcast_as((
            batch,
            seq_len,
            self.cfg.hidden_size,
        ))?;
        xs = xs.broadcast_add(&tt)?;

        let pad_id = self.cfg.pad_token_id;
        let pos_offset = self.cfg.max_position_embeddings.saturating_sub(self.loaded_max_positions);
        let mask_f32 = attention_mask.to_dtype(DType::F32)?;
        let cumsum = mask_f32.cumsum(1)?;
        let active = (cumsum * &mask_f32)?;
        let shifted_scalar_f32 = (pad_id as f32) - (pos_offset as f32);
        let bias_scalar = Tensor::from_slice(&[shifted_scalar_f32], (1,), device)?
            .broadcast_as(active.shape())?;
        let pos_f32 = (active + bias_scalar)?;
        let pos_f32 = pos_f32.maximum(&Tensor::zeros_like(&pos_f32)?)?;
        let position_ids = pos_f32.to_dtype(DType::U32)?;
        let pe = self.position_embd.index_select(&position_ids.flatten_all()?, 0)?.reshape((
            batch,
            seq_len,
            self.cfg.hidden_size,
        ))?;
        xs = (xs + pe)?;
        xs = xs.apply(&self.embd_norm)?;

        let n_to_run = stop_after_layer.unwrap_or(self.layers.len()).min(self.layers.len());
        for layer in self.layers.iter().take(n_to_run) {
            xs = layer.forward(&xs, &attn_mask_4d)?;
        }
        Ok(xs)
    }

    /// Diagnostic-only: build a `QuantizedXlmRoberta` whose weights come from
    /// the safetensors checkpoint (the FP32 reference path's source). Every
    /// linear weight is wrapped in `QMatMul::Tensor(w)` so the forward
    /// arithmetic is byte-for-byte identical to the GGUF-loaded path — the
    /// only difference is the weight values themselves.
    ///
    /// **Purpose**: discriminator for the Q4_K_M drift hunt. If the equivalence
    /// test PASSES on this constructor, the encoder code is correct and any
    /// remaining drift on the GGUF path is genuine quantization error
    /// (addressable via convert-script knobs). If it FAILS, the encoder code
    /// has a structural bug independent of weights.
    ///
    /// `safetensors_path` should be `model.safetensors` from
    /// `BAAI/bge-reranker-v2-m3` (the SAME file consumed by the FP32
    /// `XlmRobertaReranker`). Position table loads the full
    /// `max_position_embeddings` rows so `pos_offset == 0` and the pad-shift
    /// math reduces to the canonical HF formula.
    #[doc(hidden)]
    pub fn load_from_safetensors(
        safetensors_path: &Path,
        cfg: &XlmRobertaRerankerConfig,
        device: &Device,
    ) -> Result<Self, QuantizedRerankError> {
        let bytes = std::fs::read(safetensors_path)?;
        let vb = VarBuilder::from_buffered_safetensors(bytes, DType::F32, device)?;

        let r = vb.pp("roberta");
        let embd = r.pp("embeddings");

        // ---- embeddings ----
        let word_embd =
            embd.pp("word_embeddings").get((cfg.vocab_size, cfg.hidden_size), "weight")?;
        let token_type_embd = embd
            .pp("token_type_embeddings")
            .get((cfg.type_vocab_size, cfg.hidden_size), "weight")?;
        let position_embd = embd
            .pp("position_embeddings")
            .get((cfg.max_position_embeddings, cfg.hidden_size), "weight")?;
        let loaded_max_positions = cfg.max_position_embeddings;

        let ln = embd.pp("LayerNorm");
        let embd_norm = LayerNorm::new(
            ln.get(cfg.hidden_size, "weight")?,
            ln.get(cfg.hidden_size, "bias")?,
            cfg.layer_norm_eps,
        );

        // ---- transformer layers ----
        let num_heads = cfg.num_attention_heads;
        let head_dim = cfg.hidden_size / num_heads;
        let hidden = cfg.hidden_size;
        let inter = cfg.intermediate_size;
        let enc = r.pp("encoder").pp("layer");
        let mut layers = Vec::with_capacity(cfg.num_hidden_layers);
        for i in 0..cfg.num_hidden_layers {
            let lvb = enc.pp(i.to_string());
            let attn = lvb.pp("attention");
            let aself = attn.pp("self");
            let aout = attn.pp("output");

            // self-attention q/k/v: weight `[hidden, hidden]` (out, in).
            let q_w = aself.pp("query").get((hidden, hidden), "weight")?;
            let q_b = aself.pp("query").get(hidden, "bias")?;
            let k_w = aself.pp("key").get((hidden, hidden), "weight")?;
            let k_b = aself.pp("key").get(hidden, "bias")?;
            let v_w = aself.pp("value").get((hidden, hidden), "weight")?;
            let v_b = aself.pp("value").get(hidden, "bias")?;

            // attention output: dense + LN.
            let ao_w = aout.pp("dense").get((hidden, hidden), "weight")?;
            let ao_b = aout.pp("dense").get(hidden, "bias")?;
            let ao_ln = aout.pp("LayerNorm");
            let attn_output_norm = LayerNorm::new(
                ao_ln.get(hidden, "weight")?,
                ao_ln.get(hidden, "bias")?,
                cfg.layer_norm_eps,
            );

            // FFN: intermediate.dense (up), output.dense (down), output.LN.
            let up_w = lvb.pp("intermediate").pp("dense").get((inter, hidden), "weight")?;
            let up_b = lvb.pp("intermediate").pp("dense").get(inter, "bias")?;
            let lout = lvb.pp("output");
            let down_w = lout.pp("dense").get((hidden, inter), "weight")?;
            let down_b = lout.pp("dense").get(hidden, "bias")?;
            let lout_ln = lout.pp("LayerNorm");
            let layer_output_norm = LayerNorm::new(
                lout_ln.get(hidden, "weight")?,
                lout_ln.get(hidden, "bias")?,
                cfg.layer_norm_eps,
            );

            layers.push(Layer {
                attn_q: QMatMul::Tensor(q_w),
                attn_q_bias: q_b,
                attn_k: QMatMul::Tensor(k_w),
                attn_k_bias: k_b,
                attn_v: QMatMul::Tensor(v_w),
                attn_v_bias: v_b,
                attn_output: QMatMul::Tensor(ao_w),
                attn_output_bias: ao_b,
                attn_output_norm,
                ffn_up: QMatMul::Tensor(up_w),
                ffn_up_bias: up_b,
                ffn_down: QMatMul::Tensor(down_w),
                ffn_down_bias: down_b,
                layer_output_norm,
                num_heads,
                head_dim,
            });
        }

        // ---- classifier head ----
        let cls = vb.pp("classifier");
        let dense_w = cls.pp("dense").get((hidden, hidden), "weight")?;
        let dense_b = cls.pp("dense").get(hidden, "bias")?;
        // out_proj.weight is `[1, hidden]` in safetensors; we store it as
        // `[hidden, 1]` so the forward path can call `cls_hidden.matmul(&w_col)`.
        let out_proj_row = cls.pp("out_proj").get((cfg.num_labels, hidden), "weight")?;
        let out_proj_w = out_proj_row.t()?.contiguous()?;
        let out_proj_bias = cls.pp("out_proj").get(cfg.num_labels, "bias")?;

        let classifier = ClassifierHead {
            dense: QMatMul::Tensor(dense_w),
            dense_bias: dense_b,
            out_proj_w,
            out_proj_bias,
        };

        Ok(Self {
            cfg: cfg.clone(),
            device: device.clone(),
            word_embd,
            token_type_embd,
            position_embd,
            loaded_max_positions,
            embd_norm,
            layers,
            classifier,
        })
    }

    /// Diagnostic-only: replace the word-embedding table with the FP32 one
    /// loaded from safetensors. Used by the layerwise drift hunt to test the
    /// hypothesis "Q6_K on `token_embd` is the dominant drift source on
    /// rare-token inputs". If pair #0's score jumps to ~0.89 after this swap,
    /// `--token-embedding-type f16` in the convert script is the fix; if it
    /// stays at ~0.83, the drift is in the linear weights and needs an
    /// imatrix-calibrated requant or a different Q-level.
    #[doc(hidden)]
    pub fn swap_word_embd_from_safetensors(
        &mut self,
        safetensors_path: &Path,
    ) -> Result<(), QuantizedRerankError> {
        let bytes = std::fs::read(safetensors_path)?;
        let vb = VarBuilder::from_buffered_safetensors(bytes, DType::F32, &self.device)?;
        let w = vb
            .pp("roberta")
            .pp("embeddings")
            .pp("word_embeddings")
            .get((self.cfg.vocab_size, self.cfg.hidden_size), "weight")?;
        self.word_embd = w;
        Ok(())
    }

    /// Accessor for the loaded config.
    pub fn config(&self) -> &XlmRobertaRerankerConfig {
        &self.cfg
    }

    /// Accessor for the model's device.
    pub fn device(&self) -> &Device {
        &self.device
    }

    /// Accessor for the position-embedding table's loaded row count
    /// (may differ from `cfg.max_position_embeddings` by 2; see module
    /// docs / load() comment).
    pub fn loaded_max_positions(&self) -> usize {
        self.loaded_max_positions
    }
}

impl Layer {
    /// Forward pass of one XLM-R encoder block. Post-norm pattern: the
    /// LayerNorms are applied AFTER the residual add (not before, as in
    /// ModernBERT/granite-r2).
    fn forward(&self, xs: &Tensor, attn_mask_4d: &Tensor) -> Result<Tensor, QuantizedRerankError> {
        // ---------- attention block ----------
        let (b, s, _d) = xs.dims3()?;

        // Project q, k, v separately. Each is `(b, s, hidden)`.
        let q = bias_linear(&xs.apply(&self.attn_q)?, &self.attn_q_bias)?;
        let k = bias_linear(&xs.apply(&self.attn_k)?, &self.attn_k_bias)?;
        let v = bias_linear(&xs.apply(&self.attn_v)?, &self.attn_v_bias)?;

        // Reshape + permute to `(b, heads, s, head_dim)`.
        let q = q
            .reshape((b, s, self.num_heads, self.head_dim))?
            .permute((0, 2, 1, 3))?
            .contiguous()?;
        let k = k
            .reshape((b, s, self.num_heads, self.head_dim))?
            .permute((0, 2, 1, 3))?
            .contiguous()?;
        let v = v
            .reshape((b, s, self.num_heads, self.head_dim))?
            .permute((0, 2, 1, 3))?
            .contiguous()?;

        // Scaled dot-product attention.
        let scale = (self.head_dim as f64).powf(-0.5);
        let att = q.matmul(&k.transpose(D::Minus2, D::Minus1)?)?;
        let att = (att * scale)?;
        let att = att.broadcast_add(attn_mask_4d)?;
        let att = softmax(&att, D::Minus1)?;

        // Apply attention weights → reshape back to `(b, s, hidden)`.
        let ctx = att.matmul(&v)?;
        let ctx = ctx
            .transpose(1, 2)? // (b, s, heads, head_dim)
            .reshape((b, s, self.num_heads * self.head_dim))?
            .contiguous()?;

        // attn_output projection + bias.
        let attn_out = bias_linear(&ctx.apply(&self.attn_output)?, &self.attn_output_bias)?;

        // Post-norm: LayerNorm(attn_out + residual).
        let xs2 = (attn_out + xs)?;
        let xs2 = xs2.apply(&self.attn_output_norm)?;

        // ---------- MLP block ----------
        // ffn_up + bias → GELU → ffn_down + bias.
        let ffn_h = bias_linear(&xs2.apply(&self.ffn_up)?, &self.ffn_up_bias)?;
        // XLM-R uses standard GELU (cfg.hidden_act = Activation::Gelu). candle's
        // `Activation::Gelu` is the erf-form, matching HF transformers'
        // `nn.functional.gelu` (the default).
        let ffn_h = candle_nn::Activation::Gelu.forward(&ffn_h)?;
        let ffn_out = bias_linear(&ffn_h.apply(&self.ffn_down)?, &self.ffn_down_bias)?;

        // Post-norm: LayerNorm(ffn_out + xs2).
        let xs3 = (ffn_out + xs2)?;
        let xs3 = xs3.apply(&self.layer_output_norm)?;

        Ok(xs3)
    }
}

/// Helper: broadcast-add a 1D bias `[d]` to the trailing dimension of an
/// N-D tensor `(..., d)`. The candle expression is `t + b.broadcast_as(t)`,
/// but `broadcast_as` requires building the target shape — wrap it once
/// here.
fn bias_linear(t: &Tensor, bias: &Tensor) -> Result<Tensor, QuantizedRerankError> {
    let b = bias.broadcast_as(t.shape())?;
    Ok((t + b)?)
}

/// Build the additive `(batch, 1, tgt, src)` attention bias from a
/// `(batch, src)` U32 mask. Mirrors `candle_transformers::models::
/// xlm_roberta::prepare_4d_attention_mask` (squeezed for our caller —
/// `tgt_len == src_len` always in encoder-only inference).
fn prepare_4d_attention_mask(mask: &Tensor) -> Result<Tensor, QuantizedRerankError> {
    let bsz = mask.dim(0)?;
    let src_len = mask.dim(1)?;
    let tgt_len = src_len;
    let expanded = mask
        .unsqueeze(1)?
        .unsqueeze(2)?
        .expand((bsz, 1, tgt_len, src_len))?
        .to_dtype(DType::F32)?;
    let inverted = (1.0 - expanded)?;
    Ok((inverted * f32::MIN as f64)?.to_dtype(DType::F32)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_with_missing_path_errors() {
        let cfg = XlmRobertaRerankerConfig::bge_reranker_v2_m3();
        let device = Device::Cpu;
        let got = QuantizedXlmRoberta::load(
            std::path::Path::new("/tmp/__lunaris_no_rerank_gguf_file"),
            &cfg,
            &device,
        );
        assert!(got.is_err(), "load must fail on missing gguf");
    }

    #[test]
    fn prepare_4d_attention_mask_inverts_pad_to_neg_inf() -> Result<(), QuantizedRerankError> {
        let device = Device::Cpu;
        // batch=1, seq=3, real-real-pad.
        let m = Tensor::from_slice(&[1u32, 1, 0], (1, 3), &device)?;
        let out = prepare_4d_attention_mask(&m)?;
        assert_eq!(out.dims(), &[1, 1, 3, 3]);
        let flat = out.squeeze(0)?.squeeze(0)?; // (3, 3)
        let v: Vec<Vec<f32>> = flat.to_vec2()?;
        assert!(v[0][2] < -1e30, "pad column must be NEG_INF-ish");
        assert!(v[1][1].abs() < 1e-6, "real token column must be 0.0");
        Ok(())
    }
}
