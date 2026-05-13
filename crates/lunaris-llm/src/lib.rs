//! lunaris-llm — pluggable LLM-backend abstraction shared by
//! `lunaris-extract`, `lunaris-verify`, and the turn-end reflect supervisor.
//!
//! ## Why this crate exists
//!
//! Before this crate, `lunaris-extract` and `lunaris-verify` each carried
//! their own copies of:
//!
//! - a candle Gemma-3 model loader + handle (`CandleGemma3_4B` /
//!   `CandleGemma3_27B` / `CandleGemma3_270M`),
//! - an Ollama HTTP client speaking `/api/chat`,
//! - a three-provider cloud-API mux (Anthropic / OpenAI / Gemini),
//! - and a per-crate env namespace (`LUNARIS_EXTRACT_PROVIDER`,
//!   `LUNARIS_VERIFY_PROVIDER`).
//!
//! That duplication is ~3.2k lines and means swapping in a new backend
//! (Qwen, Llama, a hosted SaaS) touches every consuming crate. This crate
//! collapses the surface to one trait — [`LlmBackend`] — with one method —
//! [`LlmBackend::generate`] — and one constraint enum — [`SchemaConstraint`].
//! Three concrete impls live behind feature flags and are wired by
//! [`config::LlmConfig`].
//!
//! ## Trait shape
//!
//! ```ignore
//! use lunaris_llm::{LlmBackend, SchemaConstraint, GenOpts};
//!
//! async fn example(backend: &dyn LlmBackend) -> Result<String, lunaris_core::LunarisError> {
//!     backend
//!         .generate("Extract entities from: ...", SchemaConstraint::None, GenOpts::default())
//!         .await
//! }
//! ```
//!
//! Both **extract** (grammar-constrained entity JSON), **verify**
//! (grammar-constrained `{winner_id, loser_id}` JSON), and **reflect**
//! (structured `{invalidate, boost, pre_warm}`) flow through this one
//! method. If a future use-case can't fit, the abstraction is wrong —
//! pivot before adding a second method.
//!
//! ## Scope of this commit
//!
//! Trait + config + tests only. No backend implementations yet — those
//! land in the follow-up commit that *moves* the CandleGemma3 / Ollama /
//! CloudApi modules out of `lunaris-extract` and `lunaris-verify`.
//! Downstream crates don't depend on `lunaris-llm` yet either.
//!
//! ## Feature flags (forwarded by consumers)
//!
//! - `candle` — links the candle stack; required for `CandleBackend`.
//! - `ollama` — links `reqwest`; required for `OllamaBackend`.
//! - `cloud-api` — links `reqwest`; required for the per-provider impls.
//!
//! Default is `[]` (noop) so a `lunaris-llm = { workspace = true }`
//! that forgets to enable a feature still compiles — it just can't
//! construct a real backend.

#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

use std::time::Duration;

use async_trait::async_trait;
use lunaris_core::LunarisError;

#[cfg(feature = "candle")]
pub mod candle;
pub mod config;
#[cfg(feature = "ollama")]
pub mod ollama;

#[cfg(feature = "candle")]
pub use candle::{CandleBackend, CandleBackendOpts};
pub use config::{LlmConfig, Pipeline, ProviderKind};
#[cfg(feature = "ollama")]
pub use ollama::{OllamaBackend, OllamaBackendOpts};

/// Object-safe async LLM backend.
///
/// One method, three constraint flavors. Consumers cast to
/// `Arc<dyn LlmBackend>` and inject via builder.
#[async_trait]
pub trait LlmBackend: Send + Sync + 'static {
    /// Generate a completion for `prompt`, optionally constrained to a
    /// grammar or JSON schema.
    ///
    /// Returns the decoded text. The caller is responsible for parsing
    /// (e.g. `lunaris-verify::parse_decision_json`). Backends MUST honour
    /// `opts.timeout` and return [`LunarisError::Storage`] (or a more
    /// specific variant the host crate maps) on timeout — never silently
    /// return a partial completion.
    async fn generate(
        &self,
        prompt: &str,
        constraint: SchemaConstraint<'_>,
        opts: GenOpts,
    ) -> Result<String, LunarisError>;

    /// Stable identifier for telemetry, quality gates, and audit logs.
    /// Example: `"candle://gemma-3-4b-it"`, `"ollama://gemma3:4b"`,
    /// `"anthropic://claude-3-5-sonnet-latest"`.
    fn model_id(&self) -> &str;

    /// Returns `true` when this backend produces real completions; `false`
    /// for the noop backend so callers can short-circuit prompt assembly.
    fn applies(&self) -> bool {
        true
    }
}

/// Output-shape constraint passed to the backend.
///
/// Three flavors map to the three transports we ship:
///
/// - `None` — free-form text (callers parse themselves).
/// - `Gbnf(...)` — llama.cpp / candle GBNF grammar; candle backends consume
///   the grammar directly; Ollama translates to JSON-schema; cloud-API
///   either uses tool-use or relaxes to `None` with a stricter prompt.
/// - `JsonSchema(...)` — a serde_json `Value` containing a JSON-schema
///   object; Ollama passes it as the `format` field; cloud-API uses
///   provider-native schema mode; candle backends fall back to GBNF if
///   the grammar can be derived, else error.
#[derive(Debug, Clone, Copy)]
pub enum SchemaConstraint<'a> {
    /// Free-form output. The caller post-processes.
    None,
    /// llama.cpp / candle GBNF grammar source.
    Gbnf(&'a str),
    /// JSON-schema object.
    JsonSchema(&'a serde_json::Value),
}

/// Per-call generation knobs. Defaults mirror the conservative settings
/// the extract + verify crates already use today; backends are free to
/// clamp values they don't support (e.g. cloud-API has no GBNF mode, so
/// it ignores grammar-only knobs).
#[derive(Debug, Clone, Copy)]
pub struct GenOpts {
    /// Max output tokens. Backends MUST honour this (truncating, not
    /// erroring, if the grammar would otherwise extend further).
    pub max_tokens: u32,
    /// Sampling temperature, `[0.0, 1.0]`. `0.0` requests greedy decoding.
    pub temperature: f32,
    /// Hard timeout per call. Mirrors the per-batch / per-chunk timeout
    /// the extract pipeline (D-02) already enforces today.
    pub timeout: Duration,
}

impl Default for GenOpts {
    fn default() -> Self {
        Self {
            // 512 covers the entity-batch + verify-decision shapes; reflect
            // is bounded by its own JSON-schema field count. Backends that
            // need more (long-form summaries — not a current use case) MUST
            // override this explicitly.
            max_tokens: 512,
            // Greedy by default — extract + verify both want determinism.
            // Reflect can pass `0.2` if we ever want diverse calibration.
            temperature: 0.0,
            // Matches the lunaris-extract per-batch budget (D-02). Verify
            // is slower today (27B) and overrides upward; once the 4B flip
            // lands this default holds end-to-end.
            timeout: Duration::from_millis(150),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Compile-time proof the trait is dyn-compatible. If a future addition
    /// (generic method, `Self: Sized` bound) breaks this, the
    /// `Arc<dyn LlmBackend>` form on every consuming builder stops
    /// compiling.
    #[test]
    fn llm_backend_is_dyn_compat() {
        fn _check<T: LlmBackend + ?Sized>() {}
        _check::<dyn LlmBackend>();

        struct Stub;
        #[async_trait]
        impl LlmBackend for Stub {
            async fn generate(
                &self,
                _prompt: &str,
                _constraint: SchemaConstraint<'_>,
                _opts: GenOpts,
            ) -> Result<String, LunarisError> {
                Ok(String::new())
            }
            fn model_id(&self) -> &str {
                "stub://noop"
            }
            fn applies(&self) -> bool {
                false
            }
        }
        let _: Arc<dyn LlmBackend> = Arc::new(Stub);
    }

    #[test]
    fn gen_opts_default_matches_extract_batch_budget() {
        let opts = GenOpts::default();
        // Pin the values so a silent default drift doesn't blow the D-02
        // per-batch budget downstream.
        assert_eq!(opts.max_tokens, 512);
        assert_eq!(opts.temperature, 0.0);
        assert_eq!(opts.timeout, Duration::from_millis(150));
    }

    #[test]
    fn schema_constraint_is_copy() {
        fn _is_copy<T: Copy>() {}
        _is_copy::<SchemaConstraint<'_>>();
    }
}
