//! Diagnostic: compare the post-encoder hidden state from `QuantizedXlmRoberta`
//! (with full dequant — set `CANDLE_DEQUANTIZE_ALL=1`) against the FP32 reference
//! produced by `candle_transformers::models::xlm_roberta::XLMRobertaModel`.
//!
//! Goal: localise the residual drift to encoder vs classifier. Runs only pair #0
//! (the simplest English pair, no rare-token noise) so iteration is fast.
//!
//! Env vars: same as `quantized_equivalence.rs`. Skipped if any are unset.

#![cfg(all(feature = "reranker-it", feature = "reranker-gguf"))]

use std::path::PathBuf;

use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::{Linear, Module, VarBuilder};
use candle_transformers::models::xlm_roberta::XLMRobertaModel;

use lunaris_rerank_native::{
    NativeQuantizedReranker, NativeQuantizedRerankerOpts, PairTokenizer, QuantizedXlmRoberta,
    XlmRobertaRerankerConfig,
};

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

fn l2(t: &Tensor) -> f64 {
    let v: Vec<f32> = t.flatten_all().unwrap().to_vec1().unwrap();
    v.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt()
}

fn rel_diff(a: &Tensor, b: &Tensor) -> f64 {
    let diff = (a - b).unwrap();
    l2(&diff) / l2(b)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quantized_vs_fp32_hidden_state_diff() {
    let Some(weights) = env_path("BGE_RERANKER_WEIGHTS_PATH") else {
        eprintln!("[skip] BGE_RERANKER_WEIGHTS_PATH unset");
        return;
    };
    let Some(tokenizer) = env_path("BGE_RERANKER_TOKENIZER_PATH") else {
        eprintln!("[skip] BGE_RERANKER_TOKENIZER_PATH unset");
        return;
    };
    let Some(config) = env_path("BGE_RERANKER_CONFIG_PATH") else {
        eprintln!("[skip] BGE_RERANKER_CONFIG_PATH unset");
        return;
    };
    let Some(gguf) = env_path("BGE_RERANKER_GGUF_PATH") else {
        eprintln!("[skip] BGE_RERANKER_GGUF_PATH unset");
        return;
    };

    let device = Device::Cpu;
    let cfg = XlmRobertaRerankerConfig::try_from_json_path(&config).expect("parse config");

    // ---- pair selection: defaults to pair #0; override via DIAG_PAIR_INDEX env.
    //
    // Pair #0 is the simplest English pair (no rare-token noise) — used to
    // prove forward-code correctness. Set `DIAG_PAIR_INDEX=47` to re-run the
    // discriminator on the worst-case VI pair ("mekong delta tourism") that
    // hits the Q4_K_M drift gate the hardest. If `load_from_safetensors`
    // still produces rel-L2 ≈ 0 there, the forward code is bit-exact on
    // rare-token inputs too and the residual is genuinely block-quant error.
    let tok = PairTokenizer::from_file(&tokenizer, cfg.pad_token_id).expect("tokenizer");
    let (query_owned, doc_owned): (String, String) = match std::env::var("DIAG_PAIR_INDEX") {
        Ok(s) => {
            let idx: usize = s.parse().expect("DIAG_PAIR_INDEX must be a usize");
            #[derive(serde::Deserialize)]
            struct Fx {
                pairs: Vec<Pair>,
            }
            #[derive(serde::Deserialize)]
            struct Pair {
                query: String,
                doc: String,
            }
            let fx_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join("reference_scores.json");
            let bytes = std::fs::read(&fx_path).expect("read fixture");
            let fx: Fx = serde_json::from_slice(&bytes).expect("parse fixture");
            let p = fx.pairs.into_iter().nth(idx).expect("pair index out of range");
            eprintln!(
                "[diag] using fixture pair #{idx}: q={:?} d={:?}",
                p.query.chars().take(40).collect::<String>(),
                p.doc.chars().take(40).collect::<String>()
            );
            (p.query, p.doc)
        }
        Err(_) => (
            "how to sort a list in python".to_string(),
            "Use the built-in sorted() function or list.sort() method.".to_string(),
        ),
    };
    let pair: Vec<(&str, &str)> = vec![(query_owned.as_str(), doc_owned.as_str())];
    let batch = tok.encode_pair_batch(&pair, &device).expect("encode");

    // ---- FP32 reference: candle_transformers XLMRobertaModel ----
    let bytes = std::fs::read(&weights).expect("read safetensors");
    let vb = VarBuilder::from_buffered_safetensors(bytes, DType::F32, &device)
        .expect("safetensors decode");
    let candle_cfg = cfg.to_candle();
    // The safetensors prefix is "roberta.embeddings" / "roberta.encoder" — so
    // we descend one level via vb.pp("roberta") to instantiate XLMRobertaModel.
    let model = XLMRobertaModel::new(&candle_cfg, vb.pp("roberta")).expect("XLMRobertaModel::new");
    let fp32_hidden = model
        .forward(&batch.input_ids, &batch.attention_mask, &batch.token_type_ids, None, None, None)
        .expect("FP32 forward");
    let fp32_cls = fp32_hidden.i((.., 0, ..)).unwrap().contiguous().unwrap();
    drop(model);

    // ---- Sanity check: apply the classifier manually using the same VarBuilder ----
    // If this score matches `XlmRobertaReranker::score_blocking`'s output (~0.8944),
    // the FP32 baseline in this test is wired correctly to the same path the
    // equivalence test uses.
    let classifier_vb = vb.pp("classifier");
    let dense_w = classifier_vb.get((cfg.hidden_size, cfg.hidden_size), "dense.weight").unwrap();
    let dense_b = classifier_vb.get(cfg.hidden_size, "dense.bias").unwrap();
    let out_proj_w = classifier_vb.get((1, cfg.hidden_size), "out_proj.weight").unwrap();
    let out_proj_b = classifier_vb.get(1, "out_proj.bias").unwrap();
    let dense = Linear::new(dense_w, Some(dense_b));
    let out_proj = Linear::new(out_proj_w, Some(out_proj_b));
    let h = dense.forward(&fp32_cls).unwrap();
    let h = h.tanh().unwrap();
    let logit = out_proj.forward(&h).unwrap();
    let logit_v: Vec<f32> = logit.flatten_all().unwrap().to_vec1().unwrap();
    let fp32_baseline_score = 1.0_f32 / (1.0 + (-logit_v[0]).exp());
    eprintln!(
        "[diag] FP32-baseline score (XLMRobertaModel + manual classifier) = {fp32_baseline_score:.4}  (logit {:.3})",
        logit_v[0]
    );
    eprintln!("[diag] Expected from equivalence test: ~0.8944 (logit ~2.137)");

    // ---- Q4 path ----
    let q4 = NativeQuantizedReranker::open(NativeQuantizedRerankerOpts {
        gguf_path: gguf,
        tokenizer_path: tokenizer,
        config_path: config,
        device: device.clone(),
    })
    .expect("Q4 open");

    // ---- Cross-classifier test: Q4 hidden state run through FP32 classifier ----
    // If Q4 encoder's bug is in the encoder (not the classifier), then
    // (Q4 hidden -> FP32 classifier) score is what Q4 would score if its
    // classifier were perfect. Compare to Q4 native score: any large divergence
    // implies the Q4 classifier ALSO has a bug.
    let q4_hidden_for_xfer = q4
        .inner_for_test()
        .forward_hidden_upto(&batch.input_ids, &batch.attention_mask, None)
        .expect("Q4 forward_hidden");
    let q4_cls_for_xfer = q4_hidden_for_xfer.i((.., 0, ..)).unwrap().contiguous().unwrap();
    let h_q4 = dense.forward(&q4_cls_for_xfer).unwrap();
    let h_q4 = h_q4.tanh().unwrap();
    let logit_q4 = out_proj.forward(&h_q4).unwrap();
    let logit_q4_v: Vec<f32> = logit_q4.flatten_all().unwrap().to_vec1().unwrap();
    let q4_xfer_score = 1.0_f32 / (1.0 + (-logit_q4_v[0]).exp());
    eprintln!(
        "[diag] (Q4 hidden -> FP32 classifier) score = {q4_xfer_score:.4}  (logit {:.3})",
        logit_q4_v[0]
    );
    let q4_native = q4.score_blocking(&[pair[0]]).unwrap();
    eprintln!(
        "[diag] Q4 native score = {:.4}  (logit {:.3})",
        q4_native[0],
        (q4_native[0] as f64 / (1.0 - q4_native[0] as f64)).ln()
    );
    // Per-stop diff (binary-searchable layer hunt).
    eprintln!("[diag] per-stop relative L2 diff vs FP32-final:");
    for &k in &[0usize, 1, 2, 4, 8, 12, 16, 20, 23, 24] {
        let q4_at_k = q4
            .inner_for_test()
            .forward_hidden_upto(&batch.input_ids, &batch.attention_mask, Some(k))
            .expect("Q4 forward_hidden_upto");
        let rel = rel_diff(&q4_at_k, &fp32_hidden);
        let q4_norm = l2(&q4_at_k);
        eprintln!("  stop_after_layer={k:>2}: rel L2 vs fp32 final = {rel:.4}  |q4|={q4_norm:.2}");
    }

    let q4_hidden = q4
        .inner_for_test()
        .forward_hidden_upto(&batch.input_ids, &batch.attention_mask, None)
        .expect("Q4 forward_hidden");
    let q4_cls = q4_hidden.i((.., 0, ..)).unwrap().contiguous().unwrap();

    let rel = rel_diff(&q4_hidden, &fp32_hidden);
    let rel_cls = rel_diff(&q4_cls, &fp32_cls);
    eprintln!("[diag] L2(diff)/L2(fp32) — full hidden = {rel:.6}");
    eprintln!("[diag] L2(diff)/L2(fp32) — CLS only    = {rel_cls:.6}");
    eprintln!("[diag] L2(fp32 hidden) = {:.4}", l2(&fp32_hidden));
    eprintln!("[diag] L2(q4   hidden) = {:.4}", l2(&q4_hidden));
    eprintln!("[diag] L2(fp32 cls)    = {:.4}", l2(&fp32_cls));
    eprintln!("[diag] L2(q4   cls)    = {:.4}", l2(&q4_cls));

    // ---- Hybrid: GGUF for everything except word_embd (loaded from safetensors) ----
    //
    // If score jumps to ~0.89, Q6_K on `token_embd` is the dominant drift
    // source — fix is `--token-embedding-type f16` in convert script.
    // If score stays ~0.83, the drift is in the linear weights — needs
    // imatrix-calibrated quant or a higher Q-level.
    let mut hybrid = QuantizedXlmRoberta::load(
        std::path::Path::new(std::env::var("BGE_RERANKER_GGUF_PATH").unwrap().as_str()),
        &cfg,
        &device,
    )
    .expect("hybrid GGUF load");
    hybrid.swap_word_embd_from_safetensors(&weights).expect("swap word_embd");
    let hy_hidden = hybrid
        .forward_hidden_upto(&batch.input_ids, &batch.attention_mask, None)
        .expect("hybrid forward_hidden");
    let hy_cls = hy_hidden.i((.., 0, ..)).unwrap().contiguous().unwrap();
    let h_hy = dense.forward(&hy_cls).unwrap();
    let h_hy = h_hy.tanh().unwrap();
    let logit_hy = out_proj.forward(&h_hy).unwrap();
    let logit_hy_v: Vec<f32> = logit_hy.flatten_all().unwrap().to_vec1().unwrap();
    let hy_score = 1.0_f32 / (1.0 + (-logit_hy_v[0]).exp());
    let hy_rel_cls = rel_diff(&hy_cls, &fp32_cls);
    eprintln!(
        "[diag] (hybrid: GGUF + FP32 word_embd -> FP32 classifier) score = {hy_score:.4}  (logit {:.3})",
        logit_hy_v[0]
    );
    eprintln!("[diag] hybrid vs FP32: rel L2 CLS = {hy_rel_cls:.6}");
    eprintln!(
        "[diag] Interpretation: if score ~0.89 -> Q6_K on token_embd is the culprit (--token-embedding-type f16)"
    );
    eprintln!(
        "[diag]                 if score ~0.83 -> drift is in Q4_K linear weights (needs imatrix or higher Q)"
    );

    // ---- Discriminator: same forward code, FP32 weights from safetensors ----
    //
    // If this produces 0.8944 (within ~0.001 of the candle baseline), the
    // forward arithmetic in `QuantizedXlmRoberta` is correct and the residual
    // ~8.5% L2 drift on the GGUF path is genuine quantization error
    // (addressable via convert-script knobs like --token-embedding-type f16).
    //
    // If it produces ~0.83 instead, the forward code has a structural bug
    // independent of weights — at which point a per-layer binary search
    // against candle's xlm_roberta::XLMRobertaModel is the next step.
    let sf = QuantizedXlmRoberta::load_from_safetensors(&weights, &cfg, &device)
        .expect("safetensors-fed QuantizedXlmRoberta");
    let sf_hidden = sf
        .forward_hidden_upto(&batch.input_ids, &batch.attention_mask, None)
        .expect("safetensors-fed forward_hidden");
    let sf_cls = sf_hidden.i((.., 0, ..)).unwrap().contiguous().unwrap();
    let h_sf = dense.forward(&sf_cls).unwrap();
    let h_sf = h_sf.tanh().unwrap();
    let logit_sf = out_proj.forward(&h_sf).unwrap();
    let logit_sf_v: Vec<f32> = logit_sf.flatten_all().unwrap().to_vec1().unwrap();
    let sf_score = 1.0_f32 / (1.0 + (-logit_sf_v[0]).exp());
    let sf_rel = rel_diff(&sf_hidden, &fp32_hidden);
    let sf_rel_cls = rel_diff(&sf_cls, &fp32_cls);
    eprintln!(
        "[diag] (safetensors-fed mycode -> FP32 classifier) score = {sf_score:.4}  (logit {:.3})",
        logit_sf_v[0]
    );
    eprintln!(
        "[diag] safetensors-fed mycode vs candle FP32: rel L2 hidden = {sf_rel:.6}, rel L2 CLS = {sf_rel_cls:.6}"
    );
    eprintln!("[diag] Expected: score ~0.8944, rel L2 < 1e-4 if forward code is correct");

    // Print a few CLS values side-by-side for eyeballing.
    let f: Vec<f32> = fp32_cls.flatten_all().unwrap().to_vec1().unwrap();
    let q: Vec<f32> = q4_cls.flatten_all().unwrap().to_vec1().unwrap();
    eprintln!("[diag] first 8 CLS values:");
    for i in 0..8 {
        eprintln!("  i={i}: fp32={:.4}  q4={:.4}  diff={:.4}", f[i], q[i], (f[i] - q[i]).abs());
    }
}
