//! `XlmRobertaRerankerConfig` — deserialization of bge-reranker-v2-m3's
//! `config.json`.
//!
//! Thin wrapper around `candle_transformers::models::xlm_roberta::Config` that
//! adds:
//! - a JSON-from-disk loader (`try_from_json_path`),
//! - lightweight invariant validation,
//! - a default constructor pinned to the upstream
//!   `BAAI/bge-reranker-v2-m3` hyperparameters (so unit tests don't need a
//!   `config.json` fixture to exercise downstream types).
//!
//! Why wrap instead of re-export? Mirrors `lunaris-embed-native::config` —
//! Lunaris is the public API surface; we don't want crate consumers to
//! transitively depend on `candle-transformers` for a type signature. The
//! wrapper also lets us fail-fast on `hidden_size % num_attention_heads != 0`
//! before handing the config to the model loader (a deep candle panic
//! otherwise).

use std::path::Path;

use candle_nn::Activation;
use serde::Deserialize;

/// Errors raised while parsing or validating an `XlmRobertaRerankerConfig`.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config: failed to read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("config: failed to parse JSON: {0}")]
    Parse(#[from] serde_json::Error),

    #[error("config: invariant violated: {0}")]
    Invariant(&'static str),
}

/// XLM-RoBERTa architecture hyperparameters as published in the upstream
/// `config.json`. Unknown fields are tolerated so future model revisions (e.g.,
/// additional fine-tuning metadata) don't fail at parse time.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct XlmRobertaRerankerConfig {
    pub vocab_size: usize,
    pub hidden_size: usize,
    pub num_hidden_layers: usize,
    pub num_attention_heads: usize,
    pub intermediate_size: usize,
    pub max_position_embeddings: usize,
    pub layer_norm_eps: f64,
    pub pad_token_id: u32,
    pub type_vocab_size: usize,
    #[serde(default = "default_hidden_act")]
    pub hidden_act: Activation,
    #[serde(default = "default_position_embedding_type")]
    pub position_embedding_type: String,
    #[serde(default)]
    pub attention_probs_dropout_prob: f32,
    #[serde(default)]
    pub hidden_dropout_prob: f32,
    /// `num_labels` from the HF SequenceClassification head config. bge-reranker
    /// ships `num_labels = 1` (single relevance logit through sigmoid).
    #[serde(default = "default_num_labels")]
    pub num_labels: usize,
}

fn default_hidden_act() -> Activation {
    Activation::Gelu
}

fn default_position_embedding_type() -> String {
    "absolute".to_string()
}

fn default_num_labels() -> usize {
    1
}

impl XlmRobertaRerankerConfig {
    /// Upstream `BAAI/bge-reranker-v2-m3` defaults, mirrored from the live
    /// `config.json` on the HF Hub (verified 2026-05-14). Production loads MUST
    /// go through `try_from_json_path` — this constructor exists for tests
    /// that don't have a config fixture on disk yet.
    #[doc(hidden)]
    pub fn bge_reranker_v2_m3() -> Self {
        Self {
            vocab_size: 250_002,
            hidden_size: 1024,
            num_hidden_layers: 24,
            num_attention_heads: 16,
            intermediate_size: 4096,
            max_position_embeddings: 8194,
            layer_norm_eps: 1e-5,
            pad_token_id: 1,
            type_vocab_size: 1,
            hidden_act: Activation::Gelu,
            position_embedding_type: "absolute".to_string(),
            attention_probs_dropout_prob: 0.1,
            hidden_dropout_prob: 0.1,
            num_labels: 1,
        }
    }

    /// Parse a `config.json` from disk and validate invariants.
    pub fn try_from_json_path<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let p = path.as_ref();
        let bytes = std::fs::read(p)
            .map_err(|source| ConfigError::Read { path: p.display().to_string(), source })?;
        let cfg: XlmRobertaRerankerConfig = serde_json::from_slice(&bytes)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Hidden invariants the model loader expects.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.hidden_size == 0 || self.num_attention_heads == 0 {
            return Err(ConfigError::Invariant("hidden_size and num_attention_heads must be > 0"));
        }
        if !self.hidden_size.is_multiple_of(self.num_attention_heads) {
            return Err(ConfigError::Invariant(
                "hidden_size must be divisible by num_attention_heads",
            ));
        }
        if self.num_hidden_layers == 0 {
            return Err(ConfigError::Invariant("num_hidden_layers must be > 0"));
        }
        if self.num_labels == 0 {
            return Err(ConfigError::Invariant("num_labels must be >= 1"));
        }
        if self.type_vocab_size == 0 {
            return Err(ConfigError::Invariant("type_vocab_size must be >= 1"));
        }
        Ok(())
    }

    /// Bridge to the candle-transformers config that the model loader consumes.
    pub(crate) fn to_candle(&self) -> candle_transformers::models::xlm_roberta::Config {
        candle_transformers::models::xlm_roberta::Config {
            hidden_size: self.hidden_size,
            layer_norm_eps: self.layer_norm_eps,
            attention_probs_dropout_prob: self.attention_probs_dropout_prob,
            hidden_dropout_prob: self.hidden_dropout_prob,
            num_attention_heads: self.num_attention_heads,
            position_embedding_type: self.position_embedding_type.clone(),
            intermediate_size: self.intermediate_size,
            hidden_act: self.hidden_act,
            num_hidden_layers: self.num_hidden_layers,
            vocab_size: self.vocab_size,
            max_position_embeddings: self.max_position_embeddings,
            type_vocab_size: self.type_vocab_size,
            pad_token_id: self.pad_token_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bge_defaults_validate() {
        let c = XlmRobertaRerankerConfig::bge_reranker_v2_m3();
        c.validate().expect("defaults must pass validation");
        assert_eq!(c.hidden_size, 1024);
        assert_eq!(c.num_hidden_layers, 24);
        assert_eq!(c.num_attention_heads, 16);
        assert_eq!(c.vocab_size, 250_002);
        assert_eq!(c.pad_token_id, 1);
        assert_eq!(c.num_labels, 1);
    }

    #[test]
    fn head_dim_must_divide_hidden() {
        let mut c = XlmRobertaRerankerConfig::bge_reranker_v2_m3();
        c.num_attention_heads = 7; // 1024 % 7 != 0
        assert!(c.validate().is_err());
    }

    #[test]
    fn num_labels_zero_rejected() {
        let mut c = XlmRobertaRerankerConfig::bge_reranker_v2_m3();
        c.num_labels = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn parse_minimal_config_json() {
        let json = r#"{
            "vocab_size": 250002,
            "hidden_size": 1024,
            "num_hidden_layers": 24,
            "num_attention_heads": 16,
            "intermediate_size": 4096,
            "max_position_embeddings": 8194,
            "layer_norm_eps": 1e-5,
            "pad_token_id": 1,
            "type_vocab_size": 1,
            "hidden_act": "gelu",
            "attention_probs_dropout_prob": 0.1,
            "hidden_dropout_prob": 0.1,
            "num_labels": 1,
            "some_extra_field": "tolerated"
        }"#;
        let c: XlmRobertaRerankerConfig = serde_json::from_str(json).expect("parse");
        c.validate().expect("validate");
        assert_eq!(c, XlmRobertaRerankerConfig::bge_reranker_v2_m3());
    }

    #[test]
    fn candle_bridge_round_trip() {
        let c = XlmRobertaRerankerConfig::bge_reranker_v2_m3();
        let candle_cfg = c.to_candle();
        assert_eq!(candle_cfg.hidden_size, c.hidden_size);
        assert_eq!(candle_cfg.num_attention_heads, c.num_attention_heads);
        assert_eq!(candle_cfg.vocab_size, c.vocab_size);
        assert_eq!(candle_cfg.pad_token_id, c.pad_token_id);
    }
}
