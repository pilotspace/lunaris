//! `quantized_modernbert` — Q4_K_M ModernBERT forward pass for granite-r2.
//!
//! ## Numerical-equivalence contract
//!
//! Structurally mirrors `candle_transformers::models::modernbert` (the FP16
//! reference path). Linear projections become `QMatMul`, LayerNorms become
//! F32 `LayerNorm` (norm scales are stored as F32 in the GGUF — *not*
//! quantized). Forward math is otherwise byte-for-byte the same: GeGLU MLP,
//! RoPE per-layer (global θ vs local θ), alternating local/global attention
//! with a sliding-window mask of `local_attention / 2` tokens on either side.
//!
//! ## CLS pool (NOT GGUF's pooling_type)
//!
//! The GGUF metadata advertises `pooling_type = 2` (mean) because llama.cpp's
//! converter copies the top-level `config.json` field; the *sentence-embedding*
//! pipeline for granite-r2 uses CLS pooling per `1_Pooling/config.json`. The
//! FP16 path resolves the same ambiguity by hard-coding CLS in
//! `crate::modernbert::pooled_forward`. We do the same here. See
//! `conversion-evidence.md` trap #1.
//!
//! ## Per-tensor dtype mix (from the manifest)
//!
//! GGUF Q4_K_M for granite-r2 mixes types per tensor:
//! - `*_norm.weight` → F32  (LayerNorm scales — not quantized)
//! - `attn_qkv` / `attn_output` / `ffn_up` → Q4_K, some Q6_K per layer
//! - `ffn_down` → Q8_0 or Q5_0 per layer
//! - `token_embd` → Q6_K
//!
//! `QMatMul::from_qtensor` handles every supported quantization type
//! uniformly; we never branch on dtype.

use std::path::Path;
use std::sync::Arc;

use candle_core::quantized::{QMatMul, gguf_file};
use candle_core::{D, DType, Device, IndexOp, Tensor};
use candle_nn::{LayerNorm, ops::softmax};

use crate::GRANITE_R2_DIM;
use crate::config::ModernBertConfig;

/// Errors raised by the quantized path. Mirrors the structure of
/// `ForwardError` so callers can do uniform error mapping.
#[derive(Debug, thiserror::Error)]
pub enum QuantizedError {
    #[error("candle: {0}")]
    Candle(#[from] candle_core::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("gguf: {reason}")]
    Gguf { reason: String },

    #[error("forward: model returned shape {actual:?}, expected hidden size {expected}")]
    UnexpectedHidden { actual: Vec<usize>, expected: usize },
}

/// Rotary positional embedding cache (cos/sin for every position up to
/// `max_position_embeddings`). Mirrors candle-transformers'
/// `modernbert::RotaryEmbedding`.
#[derive(Clone)]
struct RotaryEmbedding {
    cos: Tensor,
    sin: Tensor,
}

impl RotaryEmbedding {
    fn new(cfg: &ModernBertConfig, rope_theta: f64, dev: &Device) -> Result<Self, QuantizedError> {
        let dim = cfg.hidden_size / cfg.num_attention_heads;
        let inv_freq: Vec<f32> = (0..dim)
            .step_by(2)
            .map(|i| 1f32 / (rope_theta.powf(i as f64 / dim as f64) as f32))
            .collect();
        let inv_freq_len = inv_freq.len();
        let inv_freq = Tensor::from_vec(inv_freq, (1, inv_freq_len), dev)?.to_dtype(DType::F32)?;
        let max_seq_len = cfg.max_position_embeddings;
        let t = Tensor::arange(0u32, max_seq_len as u32, dev)?
            .to_dtype(DType::F32)?
            .reshape((max_seq_len, 1))?;
        let freqs = t.matmul(&inv_freq)?;
        Ok(Self { cos: freqs.cos()?, sin: freqs.sin()? })
    }

    fn apply(&self, q: &Tensor, k: &Tensor) -> Result<(Tensor, Tensor), QuantizedError> {
        let q_embed = candle_nn::rotary_emb::rope(&q.contiguous()?, &self.cos, &self.sin)?;
        let k_embed = candle_nn::rotary_emb::rope(&k.contiguous()?, &self.cos, &self.sin)?;
        Ok((q_embed, k_embed))
    }
}

/// One transformer layer (attention + MLP + their pre-norms). Layer 0 has no
/// `attn_norm` (the embeddings' `token_embd_norm` plays that role).
struct Layer {
    attn_qkv: QMatMul,
    attn_output: QMatMul,
    ffn_up: QMatMul,
    ffn_down: QMatMul,
    /// Pre-attention norm. `None` for layer 0 (matches FP16 path: the
    /// embeddings' `norm` is the de-facto layer-0 attn_norm).
    attn_norm: Option<LayerNorm>,
    /// Pre-FFN norm.
    ffn_norm: LayerNorm,
    /// Shared per-layer RoPE cache (cheap `Arc` clone — never mutated).
    rotary: Arc<RotaryEmbedding>,
    /// True iff this layer uses the 128-token sliding-window mask.
    uses_local_attention: bool,
    /// Per-head dim cached for the attention split.
    num_heads: usize,
    head_dim: usize,
}

/// Quantized ModernBERT model loaded from a GGUF file. The forward pass
/// produces the same `(batch, hidden)` CLS-pooled, fp32-normalized output as
/// `crate::modernbert::pooled_forward`.
pub struct QuantizedModernBert {
    cfg: ModernBertConfig,
    device: Device,
    /// Word embeddings — `token_embd` dequantized to F32 once at load time.
    /// We dequantize rather than wrap in `QMatMul` because the embedding op
    /// is an index lookup, not a matmul. F32 (not F16) because candle's
    /// CPU `dequantize_f16` does a transient F32 alloc then casts — same
    /// peak as F32 during load, plus per-batch F16→F32 casts in the hot
    /// path. F32-direct is the simpler and lower-peak-RSS choice.
    word_embd: Tensor,
    /// Post-embedding norm (= ModernBert's `embeddings.norm`). Always present.
    embd_norm: LayerNorm,
    /// 22 transformer layers.
    layers: Vec<Layer>,
    /// Final norm before the head.
    final_norm: LayerNorm,
}

impl QuantizedModernBert {
    /// Load the model from a GGUF file. The `cfg` MUST already be validated
    /// (use `ModernBertConfig::try_from_json_path`) — we do NOT re-parse
    /// `config.json` from GGUF metadata because (a) the FP16 path's
    /// `try_from_json_path` is the canonical loader and (b) the GGUF
    /// `pooling_type` field is misleading (says MEAN; sentence-transformers
    /// uses CLS).
    pub fn load(
        path: &Path,
        cfg: &ModernBertConfig,
        device: &Device,
    ) -> Result<Self, QuantizedError> {
        let mut file = std::fs::File::open(path)?;
        let content = gguf_file::Content::read(&mut file)
            .map_err(|e| QuantizedError::Gguf { reason: format!("read header: {e}") })?;

        // Helper: load a quantized tensor by name and wrap in QMatMul.
        let qmat = |reader: &mut std::fs::File, name: &str| -> Result<QMatMul, QuantizedError> {
            let qt = content
                .tensor(reader, name, device)
                .map_err(|e| QuantizedError::Gguf { reason: format!("tensor {name}: {e}") })?;
            QMatMul::from_qtensor(qt).map_err(QuantizedError::Candle)
        };

        // Helper: load an F32 weight by name and wrap as a no-bias LayerNorm.
        // GGUF stores norm weights with the GGML type `F32` (the manifest
        // confirms this), so `dequantize` is a no-op materialization.
        let load_ln =
            |reader: &mut std::fs::File, name: &str| -> Result<LayerNorm, QuantizedError> {
                let qt = content
                    .tensor(reader, name, device)
                    .map_err(|e| QuantizedError::Gguf { reason: format!("tensor {name}: {e}") })?;
                let w = qt.dequantize(device)?.to_dtype(DType::F32)?;
                // ModernBert uses `layer_norm_no_bias` with `cfg.layer_norm_eps`.
                Ok(LayerNorm::new_no_bias(w, cfg.layer_norm_eps))
            };

        // ---------- token_embd ----------
        //
        // GGUF stores `token_embd.weight` with shape `[hidden, vocab]` in the
        // manifest's logical view, but candle's `Embedding::new(t, hidden)`
        // expects `[vocab, hidden]`. The QTensor we get back already has the
        // shape candle wants (`QTensor::shape()` is in the row-first
        // [n_rows, n_cols] convention regardless of llama.cpp's transpose
        // bookkeeping for matmul). We verify the inner dim after dequant.
        let word_qt = content.tensor(&mut file, "token_embd.weight", device).map_err(|e| {
            QuantizedError::Gguf { reason: format!("tensor token_embd.weight: {e}") }
        })?;
        // why: keep the vocab table at F32. `dequantize_f16` on CPU does a
        // transient F32 alloc + cast to F16 (see candle 0.10.2's
        // `QTensor::dequantize_f16`), so peak RSS during load is the SAME
        // either way; the F16 path then leaks a steady ~384 MiB F16 table
        // plus per-batch F32 casts. F32-direct keeps steady-state RSS lower.
        // The 262_152×768 F32 table is ~768 MiB resident; combined with the
        // Q4 layer weights (~120 MiB packed) the Q4 path's steady-state RSS
        // stays well under the FP16 path's (FP16 carries the same word
        // table dequantized to F32 plus ~600 MiB of F32 layer weights).
        let word_embd = word_qt.dequantize(device)?.to_dtype(DType::F32)?;
        // Shape sanity — must be `[vocab_size, hidden_size]`.
        let we_dims = word_embd.dims().to_vec();
        if we_dims != [cfg.vocab_size, cfg.hidden_size] {
            return Err(QuantizedError::Gguf {
                reason: format!(
                    "token_embd shape {we_dims:?} != [{}, {}]",
                    cfg.vocab_size, cfg.hidden_size
                ),
            });
        }

        let embd_norm = load_ln(&mut file, "token_embd_norm.weight")?;
        let final_norm = load_ln(&mut file, "output_norm.weight")?;

        // Build per-layer rope caches (one for global θ, one for local θ).
        // Shared by reference across layers via `Arc`.
        let global_rotary = Arc::new(RotaryEmbedding::new(cfg, cfg.global_rope_theta, device)?);
        let local_rotary = Arc::new(RotaryEmbedding::new(cfg, cfg.local_rope_theta, device)?);

        let num_heads = cfg.num_attention_heads;
        let head_dim = cfg.hidden_size / num_heads;

        let mut layers = Vec::with_capacity(cfg.num_hidden_layers);
        for i in 0..cfg.num_hidden_layers {
            let p = format!("blk.{i}");

            // FP16 convention: layer i is LOCAL iff `i % global_attn_every_n_layers != 0`.
            // i.e. layer 0,3,6,... are GLOBAL, the rest are LOCAL (128-window).
            let uses_local_attention = !i.is_multiple_of(cfg.global_attn_every_n_layers);
            let rotary =
                if uses_local_attention { local_rotary.clone() } else { global_rotary.clone() };

            let attn_qkv = qmat(&mut file, &format!("{p}.attn_qkv.weight"))?;
            let attn_output = qmat(&mut file, &format!("{p}.attn_output.weight"))?;
            let ffn_up = qmat(&mut file, &format!("{p}.ffn_up.weight"))?;
            let ffn_down = qmat(&mut file, &format!("{p}.ffn_down.weight"))?;
            let ffn_norm = load_ln(&mut file, &format!("{p}.ffn_norm.weight"))?;
            // Layer 0 has no attn_norm in this GGUF (matches the FP16 path's
            // `attn_norm: Option<LayerNorm>` invariant — embeddings.norm
            // covers that position).
            let attn_norm = if i == 0 {
                None
            } else {
                Some(load_ln(&mut file, &format!("{p}.attn_norm.weight"))?)
            };

            layers.push(Layer {
                attn_qkv,
                attn_output,
                ffn_up,
                ffn_down,
                attn_norm,
                ffn_norm,
                rotary,
                uses_local_attention,
                num_heads,
                head_dim,
            });
        }

        Ok(Self {
            cfg: cfg.clone(),
            device: device.clone(),
            word_embd,
            embd_norm,
            layers,
            final_norm,
        })
    }

    /// Run the forward pass on a tokenized batch and emit CLS-pooled,
    /// L2-normalized 768-d vectors. Signature mirrors
    /// `crate::modernbert::pooled_forward`.
    pub fn pooled_forward(
        &self,
        input_ids: &Tensor,
        attention_mask: &Tensor,
    ) -> Result<Tensor, QuantizedError> {
        let dims = input_ids.dims().to_vec();
        if dims.len() != 2 {
            return Err(QuantizedError::UnexpectedHidden {
                actual: dims,
                expected: GRANITE_R2_DIM,
            });
        }
        let seq_len = dims[1];
        let batch = dims[0];

        // ---------- forward: masks + embedding + Q4 transformer stack ------
        //
        // Everything from mask prep through `final_norm` lives inside one
        // span — this is the candle-quantized-matmul-kernel stage the §4b
        // microscope decision rule is built around (see
        // `docs/design/quantized-inference-extractor-reranker.md` §4b/§4c).
        let hidden = {
            let _enter = tracing::trace_span!(
                "lunaris.embed.forward",
                batch_size = batch,
                max_seq_len = seq_len,
                quant = "q4_k_m"
            )
            .entered();

            // ---------- masks ----------
            let global_attention_mask = prepare_4d_attention_mask(attention_mask, DType::F32)?
                .to_device(input_ids.device())?;
            let local_attention_mask = get_local_attention_mask(
                seq_len,
                self.cfg.local_attention / 2,
                input_ids.device(),
            )?;

            // ---------- embedding + post-embedding norm ----------
            //
            // `Embedding::forward` would require constructing a `candle_nn::Embedding`
            // every call (or holding one on Self); the cheaper path is a direct
            // `index_select` against the F32 weight matrix.
            let mut xs = self.word_embd.index_select(&input_ids.flatten_all()?, 0)?;
            xs = xs.reshape((batch, seq_len, self.cfg.hidden_size))?;
            xs = xs.apply(&self.embd_norm)?;

            // ---------- transformer stack ----------
            for layer in self.layers.iter() {
                xs = layer.forward(&xs, &global_attention_mask, &local_attention_mask)?;
            }

            // ---------- final norm ----------
            xs.apply(&self.final_norm)?
        };

        let h_dims = hidden.dims().to_vec();
        if h_dims.len() != 3 || h_dims[2] != GRANITE_R2_DIM {
            return Err(QuantizedError::UnexpectedHidden {
                actual: h_dims,
                expected: GRANITE_R2_DIM,
            });
        }

        let _pool_enter = tracing::trace_span!(
            "lunaris.embed.pooling",
            batch_size = batch,
            max_seq_len = seq_len,
            quant = "q4_k_m"
        )
        .entered();

        // CLS pool — first token. why: granite-r2's sentence-embedding
        // pipeline uses CLS pooling (`1_Pooling/config.json`), NOT the
        // GGUF metadata's `pooling_type = 2` (mean). The FP16 path
        // resolves the same ambiguity the same way.
        let cls = hidden.i((.., 0, ..))?.contiguous()?;
        let pooled = cls.to_dtype(DType::F32)?;
        let sq = pooled.sqr()?.sum_keepdim(D::Minus1)?;
        let eps = Tensor::new(&[1e-12_f32], pooled.device())?
            .to_dtype(DType::F32)?
            .broadcast_as(sq.shape())?;
        let norm = sq.maximum(&eps)?.sqrt()?;
        let unit = pooled.broadcast_div(&norm)?;
        Ok(unit)
    }

    /// Accessor for the loaded config.
    pub fn config(&self) -> &ModernBertConfig {
        &self.cfg
    }

    /// Accessor for the model's device.
    pub fn device(&self) -> &Device {
        &self.device
    }
}

impl Layer {
    fn forward(
        &self,
        xs: &Tensor,
        global_attention_mask: &Tensor,
        local_attention_mask: &Tensor,
    ) -> Result<Tensor, QuantizedError> {
        let residual = xs.clone();

        // ---------- attention block (pre-norm) ----------
        let mut h = xs.clone();
        if let Some(an) = &self.attn_norm {
            h = h.apply(an)?;
        }
        let attention_mask = if self.uses_local_attention {
            &global_attention_mask.broadcast_add(local_attention_mask)?
        } else {
            global_attention_mask
        };
        let attn_out = self.attention(&h, attention_mask)?;
        let xs = (attn_out + residual)?;

        // ---------- MLP block (pre-norm, GeGLU) ----------
        let mlp_in = xs.apply(&self.ffn_norm)?;
        let gate_up = mlp_in.apply(&self.ffn_up)?;
        // chunk(2, -1): first half is gate, second half is up — matches
        // candle-transformers' `(gate.gelu_erf() * up)` ordering.
        let parts = gate_up.chunk(2, D::Minus1)?;
        let gated = (parts[0].gelu_erf()? * &parts[1])?;
        let mlp_out = gated.apply(&self.ffn_down)?;
        let xs = (xs + mlp_out)?;
        Ok(xs)
    }

    fn attention(&self, h: &Tensor, mask: &Tensor) -> Result<Tensor, QuantizedError> {
        let (b, s, d) = h.dims3()?;
        // Fused qkv projection: (b, s, 3*hidden).
        let qkv = h
            .apply(&self.attn_qkv)?
            .reshape((b, s, 3, self.num_heads, self.head_dim))?
            .permute((2, 0, 3, 1, 4))?; // (3, b, heads, s, head_dim)
        let q = qkv.get(0)?;
        let k = qkv.get(1)?;
        let v = qkv.get(2)?;

        let (q, k) = self.rotary.apply(&q, &k)?;

        let scale = (self.head_dim as f64).powf(-0.5);
        let q = (q * scale)?;
        let att = q.matmul(&k.transpose(D::Minus2, D::Minus1)?)?;
        let att = att.broadcast_add(mask)?;
        let att = softmax(&att, D::Minus1)?;

        let xs = att.matmul(&v)?;
        let xs = xs.transpose(1, 2)?.reshape((b, s, d))?.contiguous()?;
        let xs = xs.apply(&self.attn_output)?;
        Ok(xs)
    }
}

/// Expand a `(batch, seq)` U32 mask to the additive `(batch, 1, tgt, src)`
/// attention bias. Mirrors candle-transformers'
/// `modernbert::prepare_4d_attention_mask`.
fn prepare_4d_attention_mask(mask: &Tensor, dtype: DType) -> Result<Tensor, QuantizedError> {
    let bsz = mask.dim(0)?;
    let src_len = mask.dim(1)?;
    let tgt_len = src_len;
    let expanded =
        mask.unsqueeze(1)?.unsqueeze(2)?.expand((bsz, 1, tgt_len, src_len))?.to_dtype(dtype)?;
    let inverted = (1.0 - expanded)?;
    Ok((inverted * f32::MIN as f64)?.to_dtype(dtype)?)
}

/// Build the (seq, seq) additive bias that zeroes out attention scores beyond
/// `max_distance` tokens (in either direction). Matches FP16 reference.
fn get_local_attention_mask(
    seq_len: usize,
    max_distance: usize,
    device: &Device,
) -> Result<Tensor, QuantizedError> {
    let mask: Vec<f32> = (0..seq_len)
        .flat_map(|i| {
            (0..seq_len).map(move |j| {
                if (j as i32 - i as i32).abs() > max_distance as i32 {
                    f32::NEG_INFINITY
                } else {
                    0.0
                }
            })
        })
        .collect();
    Ok(Tensor::from_slice(&mask, (seq_len, seq_len), device)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_with_missing_path_errors() {
        // After the port body lands, `load` must fail on a missing file
        // (it now touches disk). The previous scaffold test asserted the
        // opposite — flipped here per CLAUDE.md TDD discipline.
        let cfg = ModernBertConfig::granite_r2();
        let device = Device::Cpu;
        let got = QuantizedModernBert::load(
            std::path::Path::new("/tmp/__lunaris_no_gguf_file"),
            &cfg,
            &device,
        );
        assert!(got.is_err(), "load must fail on missing gguf");
    }

    #[test]
    fn local_attention_mask_zero_inside_window_neg_inf_outside() -> Result<(), QuantizedError> {
        let device = Device::Cpu;
        let m = get_local_attention_mask(5, 1, &device)?;
        let v: Vec<Vec<f32>> = m.to_vec2()?;
        // Row 2 with window=1: allowed positions are {1,2,3}; outside is -inf.
        assert_eq!(v[2][0], f32::NEG_INFINITY);
        assert_eq!(v[2][1], 0.0);
        assert_eq!(v[2][2], 0.0);
        assert_eq!(v[2][3], 0.0);
        assert_eq!(v[2][4], f32::NEG_INFINITY);
        Ok(())
    }

    #[test]
    fn prepare_4d_attention_mask_inverts_pad_to_neg_inf() -> Result<(), QuantizedError> {
        let device = Device::Cpu;
        // batch=1, seq=3, real-real-pad.
        let m = Tensor::from_slice(&[1u32, 1, 0], (1, 3), &device)?;
        let out = prepare_4d_attention_mask(&m, DType::F32)?;
        assert_eq!(out.dims(), &[1, 1, 3, 3]);
        // Squeeze the bsz=1 / broadcast dim so we get a 3-D view we can dump.
        let flat = out.squeeze(0)?.squeeze(0)?; // (3, 3)
        let v: Vec<Vec<f32>> = flat.to_vec2()?;
        assert!(v[0][2] < -1e30, "pad column must be NEG_INF-ish");
        assert!(v[1][1].abs() < 1e-6, "real token column must be 0.0");
        Ok(())
    }
}
