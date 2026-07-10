//! [`CloudApiVerifier`] — provider-mux verifier for Anthropic / OpenAI /
//! Gemini.
//!
//! ## Phase 12b — thin wrapper
//!
//! This file was migrated from a direct HTTP client implementation to a thin
//! wrapper around `CloudBackend` (lunaris-llm) + `LlmVerifier`. The public
//! API is preserved byte-for-byte: `CloudProvider`, `CloudApiVerifierOpts`,
//! `CloudApiVerifier`, constructor signatures, `Verifier` impl.
//!
//! ## D-21 audit-reason note (Phase 12b behavioral change)
//!
//! The legacy `CloudApiVerifier` emitted a `VerifyDecision` with
//! `reason: "transient_after_retry: <err>"` and the correct provider
//! `backend` tag after D-21 retry exhaustion. `LlmVerifier` wraps errors
//! from `LlmBackend::generate` as `Ok(VerifyDecision::deferred())` with
//! `backend: Noop` and reason `"deferred (NoopVerifier or backend abstain)"`.
//! The D-21 audit signal is therefore less specific in this path.
//! Tracked: Phase 12c R3 (error-audit propagation).
//!
//! ## Schema constraint note (Phase 12b behavioral change)
//!
//! The legacy implementation sent provider-native schema constraints
//! (Anthropic `tools`, OpenAI `response_format`, Gemini `responseSchema`).
//! `LlmVerifier` delegates via `SchemaConstraint::None`. `CloudBackend`
//! (lunaris-llm) handles per-provider request building without a schema
//! constraint, falling back to free-form generation + post-hoc parse.
//! Tracked: Phase 12c R4 (schema-constraint propagation).
//!
//! ## Default models (D-02, unchanged)
//!
//! - Anthropic: `claude-3-5-sonnet-latest`
//! - OpenAI: `gpt-4o-2024-11-20`
//! - Gemini: `gemini-1.5-pro-latest`

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use lunaris_core::{LunarisError, StorageError};
use lunaris_llm::cloud::{CloudBackend, CloudBackendOpts, CloudProvider as LlmCloudProvider};

use crate::Verifier;
use crate::llm_verifier::{LlmVerifier, LlmVerifierOpts};
use crate::types::{VerifierBackend, VerifyDecision};
use lunaris_extract::NeedsReviewItem;

/// Per-call timeout for cloud APIs. 60s accommodates slower-thinking
/// models on the verifier path.
const HTTP_TIMEOUT_MS: u64 = 60_000;

/// Default retry budget per D-21 (single retry on transient).
const DEFAULT_MAX_RETRIES: u8 = 1;

/// Env var for the provider selector.
pub const ENV_PROVIDER: &str = "LUNARIS_VERIFY_PROVIDER";

/// Env var for the API key.
pub const ENV_API_KEY: &str = "LUNARIS_VERIFY_API_KEY";

/// Selectable provider per D-01. Reads from `LUNARIS_VERIFY_PROVIDER` env
/// (case-insensitive) at [`CloudApiVerifierOpts::default`] time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloudProvider {
    Anthropic,
    OpenAI,
    Gemini,
    /// MiniMax `api.minimax.io` — part of the cutover's cloud mux (extract
    /// already had it; verify gains it for provider parity).
    MiniMax,
    /// Any OpenAI-compatible `/chat/completions` server at a caller-supplied
    /// base URL (Ollama `/v1`, llama-server, vLLM, LM Studio). Base URL from
    /// `LUNARIS_OPENAI_COMPAT_BASE_URL`; API key optional.
    OpenAiCompat,
}

impl FromStr for CloudProvider {
    type Err = LunarisError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "anthropic" | "claude" => Ok(Self::Anthropic),
            "openai" | "gpt" => Ok(Self::OpenAI),
            "gemini" | "google" => Ok(Self::Gemini),
            "minimax" => Ok(Self::MiniMax),
            "openai-compat" | "openai-compatible" => Ok(Self::OpenAiCompat),
            other => Err(LunarisError::Storage(StorageError::Backend(format!(
                "cloud-api-verify: unknown provider {other:?} \
                 (expected anthropic|openai|gemini|minimax|openai-compat)"
            )))),
        }
    }
}

impl CloudProvider {
    fn default_model(self) -> &'static str {
        match self {
            // D-02: verifier uses the larger Anthropic model (sonnet) for
            // slow-path arbitration — haiku is lunaris-extract's choice for
            // the fast extractor path.
            Self::Anthropic => "claude-3-5-sonnet-latest",
            Self::OpenAI => "gpt-4o-2024-11-20",
            Self::Gemini => "gemini-1.5-pro-latest",
            Self::MiniMax => "MiniMax-M3",
            // No universal default for arbitrary OpenAI-compatible servers —
            // operator names it via OPENAI_COMPAT_VERIFY_MODEL.
            Self::OpenAiCompat => "",
        }
    }
    fn api_key_env(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::OpenAI => "OPENAI_API_KEY",
            Self::Gemini => "GEMINI_API_KEY",
            Self::MiniMax => "MINIMAX_API_KEY",
            Self::OpenAiCompat => "LUNARIS_OPENAI_COMPAT_API_KEY",
        }
    }
    fn model_env(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_VERIFY_MODEL",
            Self::OpenAI => "OPENAI_VERIFY_MODEL",
            Self::Gemini => "GEMINI_VERIFY_MODEL",
            Self::MiniMax => "MINIMAX_VERIFY_MODEL",
            Self::OpenAiCompat => "OPENAI_COMPAT_VERIFY_MODEL",
        }
    }

    /// Translate to the `lunaris_llm::cloud::CloudProvider` counterpart.
    fn to_llm_provider(self) -> LlmCloudProvider {
        match self {
            Self::Anthropic => LlmCloudProvider::Anthropic,
            Self::OpenAI => LlmCloudProvider::OpenAI,
            Self::Gemini => LlmCloudProvider::Gemini,
            Self::MiniMax => LlmCloudProvider::MiniMax,
            Self::OpenAiCompat => LlmCloudProvider::OpenAiCompat,
        }
    }
}

/// Build the canonical arbitration JSON schema for `{winner_id, loser_id, reason}`.
/// Passed as `SchemaConstraint::JsonSchema` so each cloud provider uses its
/// native structured-output mechanism (Anthropic tools, OpenAI response_format,
/// Gemini responseSchema). Restores R4 behavior dropped in Phase 12b.
fn decision_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "winner_id": { "type": "string" },
            "loser_id":  { "type": "string" },
            "reason":    { "type": "string" }
        },
        "required": ["winner_id", "loser_id", "reason"]
    })
}

/// Map a [`CloudProvider`] to its matching [`VerifierBackend`] tag for
/// audit records (D-22).
fn provider_to_backend(p: CloudProvider) -> VerifierBackend {
    match p {
        CloudProvider::Anthropic => VerifierBackend::CloudAnthropic,
        CloudProvider::OpenAI => VerifierBackend::CloudOpenAI,
        CloudProvider::Gemini => VerifierBackend::CloudGemini,
        CloudProvider::MiniMax => VerifierBackend::CloudMiniMax,
        CloudProvider::OpenAiCompat => VerifierBackend::CloudOpenAiCompat,
    }
}

/// Construction options for [`CloudApiVerifier`].
///
/// `Default` reads provider from `LUNARIS_VERIFY_PROVIDER` (falls back to
/// Anthropic), model from `<PROVIDER>_VERIFY_MODEL` (falls back to the
/// provider's default), and api_key from `LUNARIS_VERIFY_API_KEY` — OR, if
/// that is unset, from `<PROVIDER>_API_KEY` (empty string if both unset;
/// `new` rejects empty keys with an actionable error).
#[derive(Clone, Debug)]
pub struct CloudApiVerifierOpts {
    pub provider: CloudProvider,
    pub model: String,
    pub api_key: String,
    pub max_retries: u8,
    /// Base URL for [`CloudProvider::OpenAiCompat`]; ignored by the
    /// fixed-endpoint providers. `Default` reads
    /// `LUNARIS_OPENAI_COMPAT_BASE_URL`.
    pub base_url: Option<String>,
}

impl Default for CloudApiVerifierOpts {
    fn default() -> Self {
        let provider = std::env::var(ENV_PROVIDER)
            .ok()
            .and_then(|s| CloudProvider::from_str(&s).ok())
            .unwrap_or(CloudProvider::Anthropic);
        let model = std::env::var(provider.model_env())
            .unwrap_or_else(|_| provider.default_model().to_string());
        // Prefer LUNARIS_VERIFY_API_KEY so ops can rotate one env var
        // regardless of provider; fall back to the provider-specific env.
        let api_key = std::env::var(ENV_API_KEY)
            .or_else(|_| std::env::var(provider.api_key_env()))
            .unwrap_or_default();
        let base_url = match provider {
            CloudProvider::OpenAiCompat => {
                std::env::var("LUNARIS_OPENAI_COMPAT_BASE_URL").ok().filter(|s| !s.is_empty())
            }
            _ => None,
        };
        Self { provider, model, api_key, max_retries: DEFAULT_MAX_RETRIES, base_url }
    }
}

/// Cloud-API verifier (Anthropic / OpenAI / Gemini). Phase 12b wraps
/// `CloudBackend` (lunaris-llm) + `LlmVerifier`.
///
/// The `Debug` impl intentionally NEVER prints the api_key — it is safe
/// to use in `tracing::debug!(?verifier)` spans.
#[derive(Clone)]
pub struct CloudApiVerifier {
    inner: LlmVerifier,
    /// Retained for `Debug` + for surfacing the provider tag. The api_key
    /// is NOT stored here — it lives only inside `CloudBackend` (lunaris-llm).
    provider: CloudProvider,
    model: String,
    max_retries: u8,
}

impl std::fmt::Debug for CloudApiVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // T-04-01-03 mitigation: NEVER log the api_key. Print only the
        // provider+model so a `tracing::debug!(?verifier)` line stays safe.
        f.debug_struct("CloudApiVerifier")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("max_retries", &self.max_retries)
            .finish()
    }
}

impl CloudApiVerifier {
    pub fn new(opts: CloudApiVerifierOpts) -> Result<Self, LunarisError> {
        // openai-compat: key optional (local servers are typically
        // unauthenticated; base_url required — enforced by CloudBackend::new),
        // but the model has no universal default and must be explicit.
        if opts.provider == CloudProvider::OpenAiCompat {
            if opts.model.trim().is_empty() {
                return Err(LunarisError::Storage(StorageError::Backend(
                    "cloud-api-verify: openai-compat model is empty — set \
                     OPENAI_COMPAT_VERIFY_MODEL (or CloudApiVerifierOpts.model) to the model \
                     your server hosts"
                        .to_string(),
                )));
            }
        } else if opts.api_key.is_empty() {
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "cloud-api-verify: api_key is empty — set {} or {} env",
                ENV_API_KEY,
                opts.provider.api_key_env()
            ))));
        }

        let llm_provider = opts.provider.to_llm_provider();
        let backend_opts = CloudBackendOpts {
            provider: llm_provider,
            model: opts.model.clone(),
            api_key: opts.api_key,
            max_retries: opts.max_retries,
            base_url: opts.base_url,
        };
        let backend: Arc<dyn lunaris_llm::LlmBackend> = Arc::new(CloudBackend::new(backend_opts)?);

        let verifier = LlmVerifier::with_opts(
            backend,
            LlmVerifierOpts {
                timeout_ms: HTTP_TIMEOUT_MS,
                max_tokens: 2048,
                temperature: 0.0,
                backend_tag: provider_to_backend(opts.provider),
                // R4: restore provider-native schema constraints (Anthropic
                // tools, OpenAI response_format, Gemini responseSchema) so
                // structured output is enforced server-side.
                schema: Some(decision_schema()),
                // R3: restore D-21 audit-reason signal — on retry-exhausted
                // transient failure the decision carries `cause =
                // "transient_after_retry: <err>"` and the correct provider
                // backend tag rather than generic Noop + empty cause.
                defer_with_cause_on_error: true,
                ..LlmVerifierOpts::default()
            },
        );

        Ok(Self {
            inner: verifier,
            provider: opts.provider,
            model: opts.model,
            max_retries: opts.max_retries,
        })
    }
}

#[async_trait]
impl Verifier for CloudApiVerifier {
    async fn verify(&self, item: NeedsReviewItem) -> Result<VerifyDecision, LunarisError> {
        self.inner.verify(item).await
    }

    fn applies(&self) -> bool {
        self.inner.applies()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_provider_from_str_parses_all_three() {
        assert_eq!(CloudProvider::from_str("anthropic").unwrap(), CloudProvider::Anthropic);
        assert_eq!(CloudProvider::from_str("Claude").unwrap(), CloudProvider::Anthropic);
        assert_eq!(CloudProvider::from_str("openai").unwrap(), CloudProvider::OpenAI);
        assert_eq!(CloudProvider::from_str("GPT").unwrap(), CloudProvider::OpenAI);
        assert_eq!(CloudProvider::from_str("gemini").unwrap(), CloudProvider::Gemini);
        assert_eq!(CloudProvider::from_str("Google").unwrap(), CloudProvider::Gemini);
    }

    #[test]
    fn cloud_provider_from_str_rejects_unknown() {
        let err = CloudProvider::from_str("deepseek").unwrap_err();
        assert!(err.to_string().contains("unknown provider"));
    }

    #[test]
    fn minimax_and_openai_compat_parse_and_bridge() {
        // Cutover provider parity: verify gains MiniMax (extract had it) and
        // the generic OpenAI-compatible URL backend.
        assert_eq!(CloudProvider::from_str("minimax").unwrap(), CloudProvider::MiniMax);
        assert_eq!(CloudProvider::from_str("openai-compat").unwrap(), CloudProvider::OpenAiCompat);
        assert_eq!(provider_to_backend(CloudProvider::MiniMax), VerifierBackend::CloudMiniMax);
        assert_eq!(
            provider_to_backend(CloudProvider::OpenAiCompat),
            VerifierBackend::CloudOpenAiCompat
        );
    }

    #[test]
    fn openai_compat_constructs_keyless_with_explicit_model_and_base_url() {
        let v = CloudApiVerifier::new(CloudApiVerifierOpts {
            provider: CloudProvider::OpenAiCompat,
            model: "qwen3:4b".into(),
            api_key: String::new(),
            max_retries: 1,
            base_url: Some("http://localhost:11434/v1".into()),
        })
        .expect("keyless openai-compat verifier must construct");
        let dbg = format!("{v:?}");
        assert!(dbg.contains("OpenAiCompat"), "got: {dbg}");
    }

    #[test]
    fn openai_compat_requires_model_and_base_url() {
        let err = CloudApiVerifier::new(CloudApiVerifierOpts {
            provider: CloudProvider::OpenAiCompat,
            model: String::new(),
            api_key: String::new(),
            max_retries: 1,
            base_url: Some("http://localhost:11434/v1".into()),
        })
        .expect_err("empty model must fail fast");
        assert!(err.to_string().contains("OPENAI_COMPAT_VERIFY_MODEL"), "got: {err}");

        let err = CloudApiVerifier::new(CloudApiVerifierOpts {
            provider: CloudProvider::OpenAiCompat,
            model: "qwen3:4b".into(),
            api_key: String::new(),
            max_retries: 1,
            base_url: None,
        })
        .expect_err("missing base_url must fail fast");
        assert!(err.to_string().contains("LUNARIS_OPENAI_COMPAT_BASE_URL"), "got: {err}");
    }

    #[test]
    fn default_model_is_sonnet_for_anthropic() {
        // D-02: verifier uses the larger sonnet model, not haiku.
        assert_eq!(CloudProvider::Anthropic.default_model(), "claude-3-5-sonnet-latest");
    }

    #[test]
    fn env_provider_name_is_verify_not_extract() {
        assert_eq!(ENV_PROVIDER, "LUNARIS_VERIFY_PROVIDER");
        assert_eq!(ENV_API_KEY, "LUNARIS_VERIFY_API_KEY");
    }

    #[test]
    fn empty_api_key_rejected() {
        let opts = CloudApiVerifierOpts {
            provider: CloudProvider::Anthropic,
            model: "claude-3-5-sonnet-latest".into(),
            api_key: "".into(),
            max_retries: 1,
            base_url: None,
        };
        let err = CloudApiVerifier::new(opts).expect_err("empty key must error");
        let msg = err.to_string();
        assert!(msg.contains("api_key is empty"), "got: {msg}");
        assert!(msg.contains("LUNARIS_VERIFY_API_KEY"), "got: {msg}");
    }

    #[test]
    fn provider_to_backend_maps_correctly() {
        assert_eq!(provider_to_backend(CloudProvider::Anthropic), VerifierBackend::CloudAnthropic);
        assert_eq!(provider_to_backend(CloudProvider::OpenAI), VerifierBackend::CloudOpenAI);
        assert_eq!(provider_to_backend(CloudProvider::Gemini), VerifierBackend::CloudGemini);
    }

    #[test]
    fn debug_impl_redacts_api_key() {
        let opts = CloudApiVerifierOpts {
            provider: CloudProvider::Anthropic,
            model: "claude-3-5-sonnet-latest".into(),
            api_key: "sk-ant-SECRET-KEY-ABCDEF".into(),
            max_retries: 1,
            base_url: None,
        };
        let verifier = CloudApiVerifier::new(opts).unwrap();
        let dbg = format!("{verifier:?}");
        assert!(!dbg.contains("SECRET"), "Debug must redact api_key, got: {dbg}");
        assert!(!dbg.contains("sk-ant"), "Debug must redact api_key, got: {dbg}");
    }

    // ── R3 regression: transient failure preserves audit reason + backend tag ─
    /// After D-21 retry exhaustion the cloud wrapper must emit
    /// `Ok(VerifyDecision::deferred_with_cause(...))` so the D-22 audit
    /// pipeline can distinguish transient backend failures from NoopVerifier
    /// passthroughs. `cause` must contain the `transient_after_retry:` prefix
    /// and the error detail; `backend` must be the provider tag, not `Noop`.
    #[tokio::test]
    async fn cloud_api_transient_failure_preserves_audit_reason() {
        use std::sync::Arc;

        use async_trait::async_trait;
        use lunaris_core::{LunarisError, StorageError};
        use lunaris_extract::{EntityId, Fact, NeedsReviewItem, NeedsReviewReason};
        use lunaris_llm::{GenOpts, LlmBackend, SchemaConstraint};
        use ulid::Ulid;

        use crate::Verifier;
        use crate::llm_verifier::{LlmVerifier, LlmVerifierOpts};
        use crate::types::VerifierBackend;

        struct TransientCloudBackend;
        #[async_trait]
        impl LlmBackend for TransientCloudBackend {
            async fn generate(
                &self,
                _: &str,
                _: SchemaConstraint<'_>,
                _: GenOpts,
            ) -> Result<String, LunarisError> {
                Err(LunarisError::Storage(StorageError::Backend(
                    "cloud-api: HTTP 503: upstream overloaded".into(),
                )))
            }
            fn model_id(&self) -> &str {
                "stub://cloud-transient"
            }
        }

        let verifier = LlmVerifier::with_opts(
            Arc::new(TransientCloudBackend),
            LlmVerifierOpts {
                backend_tag: VerifierBackend::CloudAnthropic,
                defer_with_cause_on_error: true,
                schema: Some(super::decision_schema()),
                ..LlmVerifierOpts::default()
            },
        );

        let sid = EntityId::from_name_and_type("Alice", "Person");
        let oid = EntityId::from_name_and_type("Bob", "Person");
        let item = NeedsReviewItem::Fact {
            reason: NeedsReviewReason::GbnfFailure {
                schema_path: "facts/0".into(),
                error: "fixture".into(),
            },
            raw: Fact {
                id: Ulid::new(),
                subject_id: sid,
                predicate: "knows".into(),
                object_id: oid,
                fact_text: "Alice knows Bob".into(),
                confidence: 0.9,
                valid_from_iso: "2025-01-01".into(),
                valid_to_iso: None,
            },
        };

        let decision = verifier.verify(item).await.expect("must be Ok(deferred)");
        assert!(!decision.applies(), "transient failure must defer, not arbitrate");
        assert_eq!(
            decision.backend,
            VerifierBackend::CloudAnthropic,
            "backend tag must be CloudAnthropic, not Noop"
        );
        let cause = decision.cause.expect("cause must be Some after transient failure");
        assert!(
            cause.contains("transient_after_retry"),
            "cause must have transient_after_retry prefix: {cause}"
        );
        assert!(cause.contains("HTTP 503"), "cause must include upstream error detail: {cause}");
    }

    // ── R4 regression: decision_schema contains required arbitration fields ────
    /// `decision_schema()` must produce a JSON schema that names all three
    /// required fields (`winner_id`, `loser_id`, `reason`) so cloud providers
    /// constrain their output to the arbitration shape. This is the structural
    /// contract that R4 restores: provider-native schema mode (Anthropic tools,
    /// OpenAI response_format, Gemini responseSchema) uses this schema.
    #[test]
    fn cloud_api_decision_schema_has_required_arbitration_fields() {
        let schema = decision_schema();
        let props = schema["properties"].as_object().expect("schema must have properties");
        assert!(props.contains_key("winner_id"), "schema must require winner_id");
        assert!(props.contains_key("loser_id"), "schema must require loser_id");
        assert!(props.contains_key("reason"), "schema must require reason");
        let required = schema["required"].as_array().expect("schema must have required array");
        let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(required_names.contains(&"winner_id"), "winner_id must be required");
        assert!(required_names.contains(&"loser_id"), "loser_id must be required");
        assert!(required_names.contains(&"reason"), "reason must be required");
    }
}
