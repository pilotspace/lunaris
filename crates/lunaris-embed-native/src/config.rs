//! `ModernBertConfig` — deserialization of granite-r2's `config.json`.
//!
//! This is a thin wrapper around `candle_transformers::models::modernbert::Config`
//! that adds:
//! - validation (`new` / `try_from_json_path`),
//! - a default constructor matching upstream granite-r2 hyperparameters
//!   (so unit tests don't need a config.json fixture to exercise downstream
//!   types).
//!
//! Why wrap instead of re-export? Two reasons:
//! 1. Lunaris is the public API surface; we don't want crate consumers to
//!    transitively depend on `candle-transformers` for a type signature.
//! 2. We want a fail-fast validator (`hidden_size % num_attention_heads == 0`,
//!    `global_attn_every_n_layers >= 1`, etc.) before handing the config to
//!    the model loader, so misconfigurations surface as `NativeEmbedderError`
//!    rather than a deep candle panic.

use std::path::Path;

use serde::Deserialize;

/// Errors raised while parsing or validating a `ModernBertConfig`.
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

/// ModernBERT architecture hyperparameters. Wire-format matches HuggingFace's
/// `config.json` shape; unknown fields are tolerated so future model revisions
/// (e.g., additional fine-tuning metadata) don't fail at parse time.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ModernBertConfig {
    pub vocab_size: usize,
    pub hidden_size: usize,
    pub num_hidden_layers: usize,
    pub num_attention_heads: usize,
    pub intermediate_size: usize,
    pub max_position_embeddings: usize,
    pub layer_norm_eps: f64,
    pub pad_token_id: u32,
    /// "Every Nth layer is a GLOBAL attention layer; the rest are LOCAL with a
    /// sliding window of `local_attention` tokens." Upstream granite-r2 = 3.
    pub global_attn_every_n_layers: usize,
    /// RoPE theta for GLOBAL attention layers (large — handles 32k context).
    pub global_rope_theta: f64,
    /// Sliding-window size for LOCAL attention layers (granite-r2 = 128).
    pub local_attention: usize,
    /// RoPE theta for LOCAL attention layers (smaller, tuned for the window).
    pub local_rope_theta: f64,
}

impl ModernBertConfig {
    /// Upstream `ibm-granite/granite-embedding-311m-multilingual-r2` defaults,
    /// mirrored verbatim from the live `config.json` (HF revision as of
    /// 2026-05-14). Production loads MUST go through `try_from_json_path` —
    /// this constructor exists for tests that don't have a config fixture on
    /// disk yet. Note the unusual `local_rope_theta > global_rope_theta`
    /// relationship: that's how granite-r2 ships; do not "correct" it.
    #[doc(hidden)]
    pub fn granite_r2() -> Self {
        Self {
            vocab_size: 262_152,
            hidden_size: 768,
            num_hidden_layers: 22,
            num_attention_heads: 12,
            intermediate_size: 1_152,
            max_position_embeddings: 8_192,
            layer_norm_eps: 1e-12,
            pad_token_id: 0,
            global_attn_every_n_layers: 3,
            global_rope_theta: 150_000.0,
            local_attention: 128,
            local_rope_theta: 160_000.0,
        }
    }

    /// Parse a `config.json` from disk and validate invariants.
    pub fn try_from_json_path<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let p = path.as_ref();
        let bytes = std::fs::read(p)
            .map_err(|source| ConfigError::Read { path: p.display().to_string(), source })?;
        let cfg: ModernBertConfig = serde_json::from_slice(&bytes)?;
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
        if self.global_attn_every_n_layers == 0 {
            return Err(ConfigError::Invariant("global_attn_every_n_layers must be >= 1"));
        }
        if self.local_attention == 0 {
            return Err(ConfigError::Invariant("local_attention window must be > 0"));
        }
        Ok(())
    }

    /// Bridge to the candle-transformers config that the model loader consumes.
    pub(crate) fn to_candle(&self) -> candle_transformers::models::modernbert::Config {
        candle_transformers::models::modernbert::Config {
            vocab_size: self.vocab_size,
            hidden_size: self.hidden_size,
            num_hidden_layers: self.num_hidden_layers,
            num_attention_heads: self.num_attention_heads,
            intermediate_size: self.intermediate_size,
            max_position_embeddings: self.max_position_embeddings,
            layer_norm_eps: self.layer_norm_eps,
            pad_token_id: self.pad_token_id,
            global_attn_every_n_layers: self.global_attn_every_n_layers,
            global_rope_theta: self.global_rope_theta,
            local_attention: self.local_attention,
            local_rope_theta: self.local_rope_theta,
            classifier_config: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn granite_r2_defaults_validate() {
        let c = ModernBertConfig::granite_r2();
        c.validate().expect("defaults must pass validation");
        assert_eq!(c.hidden_size, 768);
        assert_eq!(c.num_hidden_layers, 22);
        assert_eq!(c.num_attention_heads, 12);
        assert_eq!(c.global_attn_every_n_layers, 3);
        assert_eq!(c.local_attention, 128);
    }

    #[test]
    fn head_dim_must_divide_hidden() {
        let mut c = ModernBertConfig::granite_r2();
        c.num_attention_heads = 7; // 768 % 7 != 0
        assert!(c.validate().is_err());
    }

    #[test]
    fn parse_minimal_config_json() {
        // Matches the live granite-r2 config.json (HF revision 2026-05-14).
        let json = r#"{
            "vocab_size": 262152,
            "hidden_size": 768,
            "num_hidden_layers": 22,
            "num_attention_heads": 12,
            "intermediate_size": 1152,
            "max_position_embeddings": 8192,
            "layer_norm_eps": 1e-12,
            "pad_token_id": 0,
            "global_attn_every_n_layers": 3,
            "global_rope_theta": 150000.0,
            "local_attention": 128,
            "local_rope_theta": 160000.0,
            "some_unrelated_extra_field": "tolerated"
        }"#;
        let c: ModernBertConfig = serde_json::from_str(json).expect("parse");
        c.validate().expect("validate");
        assert_eq!(c, ModernBertConfig::granite_r2());
    }

    #[test]
    fn candle_bridge_round_trip() {
        let c = ModernBertConfig::granite_r2();
        let candle_cfg = c.to_candle();
        assert_eq!(candle_cfg.hidden_size, c.hidden_size);
        assert_eq!(candle_cfg.num_attention_heads, c.num_attention_heads);
        assert_eq!(candle_cfg.local_attention, c.local_attention);
        assert!(candle_cfg.classifier_config.is_none());
    }
}
