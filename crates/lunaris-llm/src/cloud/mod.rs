//! Cloud-API [`LlmBackend`] — provider-mux over Anthropic / OpenAI / Gemini.
//!
//! Per-provider request shape + response decode live in submodules
//! (one file per provider per the 1500-line cap discipline + the
//! advisor's split-from-the-start guidance). This module owns the
//! generic [`LlmBackend`] impl, retry budget (D-21), and timeout
//! wrapping.
//!
//! ## Sentinel-on-retry-exhaust is NOT here
//!
//! The D-21 "transient-after-retry" sentinel is domain-specific to
//! `lunaris-extract`'s `Entity` shape. Cloud-API errors bubble up from
//! `generate()` as `LunarisError::Storage(Backend("cloud-api: ..."))`;
//! the extract validator detects retry exhaustion via the error kind
//! once it migrates to this backend. Keeping the sentinel out of
//! `lunaris-llm` is what lets verify + reflect reuse the same backend
//! without inheriting an extract-flavored failure mode.
//!
//! ## API-key handling
//!
//! Keys live under each provider's existing env name
//! (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`).
//! `from_env` honours these; an empty key returns an actionable error
//! pointing at the correct env name.

use std::time::Duration;

use async_trait::async_trait;
use lunaris_core::{LunarisError, StorageError};

use crate::{GenOpts, LlmBackend, ProviderKind, SchemaConstraint};

mod anthropic;
mod gemini;
mod openai;

/// Per-call HTTP transport timeout. Cloud APIs can take 5-10s under
/// load; `opts.timeout` is the narrower per-call clamp.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Default retry budget — single retry on transient errors (HTTP
/// 429 / 5xx / network drop / decode failure on a transient body),
/// matches D-21.
const DEFAULT_MAX_RETRIES: u8 = 1;

/// Which cloud provider (subset of [`ProviderKind`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudProvider {
    Anthropic,
    OpenAI,
    Gemini,
}

impl CloudProvider {
    pub fn api_key_env(self) -> &'static str {
        match self {
            CloudProvider::Anthropic => "ANTHROPIC_API_KEY",
            CloudProvider::OpenAI => "OPENAI_API_KEY",
            CloudProvider::Gemini => "GEMINI_API_KEY",
        }
    }

    pub fn telemetry_prefix(self) -> &'static str {
        match self {
            CloudProvider::Anthropic => "anthropic",
            CloudProvider::OpenAI => "openai",
            CloudProvider::Gemini => "gemini",
        }
    }
}

impl TryFrom<ProviderKind> for CloudProvider {
    type Error = LunarisError;
    fn try_from(p: ProviderKind) -> Result<Self, LunarisError> {
        match p {
            ProviderKind::Anthropic => Ok(CloudProvider::Anthropic),
            ProviderKind::OpenAI => Ok(CloudProvider::OpenAI),
            ProviderKind::Gemini => Ok(CloudProvider::Gemini),
            ProviderKind::Candle | ProviderKind::Ollama => Err(LunarisError::Storage(
                StorageError::Backend(format!("cloud-api: provider {p:?} is not a cloud provider")),
            )),
        }
    }
}

/// Construction options for [`CloudBackend`].
#[derive(Clone, Debug)]
pub struct CloudBackendOpts {
    pub provider: CloudProvider,
    pub model: String,
    pub api_key: String,
    pub max_retries: u8,
}

impl CloudBackendOpts {
    /// Construct from env. `api_key` reads the provider's standard env
    /// name; an empty value is OK at this stage — [`CloudBackend::new`]
    /// rejects empty keys with an actionable error.
    pub fn from_env(provider: CloudProvider, model: impl Into<String>) -> Self {
        let api_key = std::env::var(provider.api_key_env()).unwrap_or_default();
        Self { provider, model: model.into(), api_key, max_retries: DEFAULT_MAX_RETRIES }
    }
}

/// Cloud-API LLM backend.
#[derive(Clone)]
pub struct CloudBackend {
    client: reqwest::Client,
    provider: CloudProvider,
    model: String,
    api_key: String,
    max_retries: u8,
    model_id: String,
}

impl std::fmt::Debug for CloudBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never log the api_key — `tracing::debug!(?backend)` must stay safe.
        f.debug_struct("CloudBackend")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("max_retries", &self.max_retries)
            .finish()
    }
}

impl CloudBackend {
    pub fn new(opts: CloudBackendOpts) -> Result<Self, LunarisError> {
        if opts.api_key.is_empty() {
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "cloud-api: api_key is empty — set {} env",
                opts.provider.api_key_env()
            ))));
        }
        let client = reqwest::Client::builder().timeout(HTTP_TIMEOUT).build().map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("cloud-api client: {e}")))
        })?;
        let model_id = format!("{}://{}", opts.provider.telemetry_prefix(), opts.model);
        Ok(Self {
            client,
            provider: opts.provider,
            model: opts.model,
            api_key: opts.api_key,
            max_retries: opts.max_retries,
            model_id,
        })
    }

    async fn try_once(
        &self,
        prompt: &str,
        constraint: SchemaConstraint<'_>,
        opts: &GenOpts,
    ) -> Result<String, LunarisError> {
        let (url, body, headers) = match self.provider {
            CloudProvider::Anthropic => {
                anthropic::build_request(&self.model, &self.api_key, prompt, constraint, opts)
            }
            CloudProvider::OpenAI => {
                openai::build_request(&self.model, &self.api_key, prompt, constraint, opts)
            }
            CloudProvider::Gemini => {
                gemini::build_request(&self.model, &self.api_key, prompt, constraint, opts)
            }
        };

        let mut req = self.client.post(&url).json(&body);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| LunarisError::Storage(StorageError::Backend(format!("cloud-api: {e}"))))?;
        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "cloud-api: HTTP {status}: {body_text}"
            ))));
        }
        let body_text = resp.text().await.map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("cloud-api read: {e}")))
        })?;
        match self.provider {
            CloudProvider::Anthropic => anthropic::decode_response(&body_text),
            CloudProvider::OpenAI => openai::decode_response(&body_text),
            CloudProvider::Gemini => gemini::decode_response(&body_text),
        }
    }
}

#[async_trait]
impl LlmBackend for CloudBackend {
    async fn generate(
        &self,
        prompt: &str,
        constraint: SchemaConstraint<'_>,
        opts: GenOpts,
    ) -> Result<String, LunarisError> {
        let attempts = self.max_retries.saturating_add(1) as usize;
        let mut last_err: Option<LunarisError> = None;

        let outer = async {
            for attempt in 0..attempts {
                match self.try_once(prompt, constraint, &opts).await {
                    Ok(text) => return Ok(text),
                    Err(e) if is_transient(&e) && attempt + 1 < attempts => {
                        tracing::warn!(
                            err = %e,
                            attempt = attempt + 1,
                            max_attempts = attempts,
                            provider = self.provider.telemetry_prefix(),
                            "cloud-api transient; retrying"
                        );
                        last_err = Some(e);
                        continue;
                    }
                    Err(e) => {
                        last_err = Some(e);
                        break;
                    }
                }
            }
            Err(last_err.unwrap_or_else(|| {
                LunarisError::Storage(StorageError::Backend("cloud-api: unknown error".to_string()))
            }))
        };

        match tokio::time::timeout(opts.timeout, outer).await {
            Ok(r) => r,
            Err(_) => Err(LunarisError::Storage(StorageError::Backend(format!(
                "cloud-api timeout after {:?}",
                opts.timeout
            )))),
        }
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }
}

/// Returns `true` for HTTP statuses + error kinds that warrant a retry.
/// Mirrors the existing classification in
/// `lunaris-extract::cloud_api::is_transient`. Re-exported at the
/// `lunaris_llm` crate root so verify/extract wrappers can apply the same
/// classifier without duplicating the string-match logic.
pub fn is_transient(err: &LunarisError) -> bool {
    let s = err.to_string();
    s.contains("HTTP 429")
        || s.contains("HTTP 500")
        || s.contains("HTTP 502")
        || s.contains("HTTP 503")
        || s.contains("HTTP 504")
        || s.contains("timeout")
        || s.contains("connection")
        || s.contains("dns")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_api_key_is_actionable() {
        let err = CloudBackend::new(CloudBackendOpts {
            provider: CloudProvider::Anthropic,
            model: "claude-3-5-sonnet-latest".into(),
            api_key: String::new(),
            max_retries: 1,
        })
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("api_key is empty"), "got: {msg}");
        assert!(msg.contains("ANTHROPIC_API_KEY"), "got: {msg}");
    }

    #[test]
    fn model_id_namespaced_by_provider() {
        let backend = CloudBackend::new(CloudBackendOpts {
            provider: CloudProvider::OpenAI,
            model: "gpt-4o-mini".into(),
            api_key: "dummy-key".into(),
            max_retries: 1,
        })
        .unwrap();
        assert_eq!(backend.model_id(), "openai://gpt-4o-mini");
    }

    #[test]
    fn provider_kind_conversion() {
        assert_eq!(
            CloudProvider::try_from(ProviderKind::Anthropic).unwrap(),
            CloudProvider::Anthropic
        );
        assert_eq!(CloudProvider::try_from(ProviderKind::OpenAI).unwrap(), CloudProvider::OpenAI);
        assert_eq!(CloudProvider::try_from(ProviderKind::Gemini).unwrap(), CloudProvider::Gemini);
        assert!(CloudProvider::try_from(ProviderKind::Candle).is_err());
        assert!(CloudProvider::try_from(ProviderKind::Ollama).is_err());
    }

    #[test]
    fn is_transient_classifies_5xx_and_429() {
        let mk = |s: &str| LunarisError::Storage(StorageError::Backend(s.into()));
        assert!(is_transient(&mk("cloud-api: HTTP 429")));
        assert!(is_transient(&mk("cloud-api: HTTP 503")));
        assert!(is_transient(&mk("cloud-api: HTTP 504")));
        assert!(is_transient(&mk("cloud-api: connection reset")));
        assert!(!is_transient(&mk("cloud-api: HTTP 401")));
        assert!(!is_transient(&mk("cloud-api: HTTP 400")));
    }
}
