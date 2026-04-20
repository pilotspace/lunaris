//! [`CloudApiVerifier`] — provider-mux verifier for Anthropic / OpenAI /
//! Gemini.
//!
//! Per D-01 the cloud-API path is selectable via `LUNARIS_VERIFY_PROVIDER` env
//! (`anthropic` | `openai` | `gemini`, case-insensitive). Each provider
//! speaks a different structured-output API (matches
//! `lunaris-extract::CloudApiExtractor`):
//!
//! - **Anthropic** (`/v1/messages` with `tools` + `input_schema`): sonnet-
//!   class tool-use that constrains the model output to a JSON schema.
//!   Headers: `x-api-key`, `anthropic-version: 2023-06-01`.
//! - **OpenAI** (`/v1/chat/completions` with `response_format: {type:
//!   "json_schema", json_schema: ...}`): gpt-4o structured outputs. Header:
//!   `Authorization: Bearer <key>`.
//! - **Gemini** (`generativelanguage.googleapis.com/v1beta/models/<model>:
//!   generateContent?key=<key>` with `generationConfig.responseSchema`):
//!   same schema shape as OpenAI; auth via query param.
//!
//! ## D-21 single-retry-then-flag (preserved verbatim from lunaris-extract)
//!
//! On a transient error (HTTP 429 / 5xx / network drop / timeout) the
//! per-item path retries ONCE. If the retry also fails, the verifier returns
//! `Ok(VerifyDecision { winner_id: None, loser_id: None, reason:
//! "transient_after_retry: <error>", backend: <provider>, decided_at_iso:
//! now() })` — the worker treats this as a deferred decision (skip
//! atomic_write) AND the operator sees the failure surfaced via the
//! audit record.
//!
//! ## Default models (D-02)
//!
//! - Anthropic: `claude-3-5-sonnet-latest` (NOT haiku — verifier is the
//!   slow-path "get it right" model)
//! - OpenAI: `gpt-4o-2024-11-20` (latest non-mini for arbitration quality)
//! - Gemini: `gemini-1.5-pro-latest`
//!
//! ## Failure modes
//!
//! Same shape as `lunaris-extract/src/cloud_api.rs`. HTTP 4xx (auth, invalid
//! request) → non-transient → bubble up as
//! `LunarisError::Storage(StorageError::Backend("cloud-api-verify: HTTP <status>"))`.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use lunaris_core::{LunarisError, StorageError};
use serde::Deserialize;

use crate::Verifier;
use crate::types::{VerifierBackend, VerifyDecision};
use lunaris_extract::NeedsReviewItem;

/// Per-call HTTP timeout for cloud APIs. 60s is generous to accommodate
/// slower-thinking models on the verifier path (27B-class responses).
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);

/// Default retry budget per D-21 (single retry on transient).
const DEFAULT_MAX_RETRIES: u8 = 1;

/// Env var for the provider selector.
pub const ENV_PROVIDER: &str = "LUNARIS_VERIFY_PROVIDER";

/// Env var for the API key.
pub const ENV_API_KEY: &str = "LUNARIS_VERIFY_API_KEY";

/// Backoff applied between the initial attempt and the single retry.
const RETRY_BACKOFF: Duration = Duration::from_millis(200);

/// Selectable provider per D-01. Reads from `LUNARIS_VERIFY_PROVIDER` env
/// (case-insensitive) at [`CloudApiVerifierOpts::default`] time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloudProvider {
    Anthropic,
    OpenAI,
    Gemini,
}

impl FromStr for CloudProvider {
    type Err = LunarisError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "anthropic" | "claude" => Ok(Self::Anthropic),
            "openai" | "gpt" => Ok(Self::OpenAI),
            "gemini" | "google" => Ok(Self::Gemini),
            other => Err(LunarisError::Storage(StorageError::Backend(format!(
                "cloud-api-verify: unknown provider {other:?} (expected anthropic|openai|gemini)"
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
        }
    }
    fn api_key_env(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::OpenAI => "OPENAI_API_KEY",
            Self::Gemini => "GEMINI_API_KEY",
        }
    }
    fn model_env(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_VERIFY_MODEL",
            Self::OpenAI => "OPENAI_VERIFY_MODEL",
            Self::Gemini => "GEMINI_VERIFY_MODEL",
        }
    }
}

/// Map a [`CloudProvider`] to its matching [`VerifierBackend`] tag for
/// audit records (D-22).
fn provider_to_backend(p: CloudProvider) -> VerifierBackend {
    match p {
        CloudProvider::Anthropic => VerifierBackend::CloudAnthropic,
        CloudProvider::OpenAI => VerifierBackend::CloudOpenAI,
        CloudProvider::Gemini => VerifierBackend::CloudGemini,
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
        Self {
            provider,
            model,
            api_key,
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }
}

#[derive(Clone)]
pub struct CloudApiVerifier {
    client: reqwest::Client,
    provider: CloudProvider,
    model: String,
    api_key: String,
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
        if opts.api_key.is_empty() {
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "cloud-api-verify: api_key is empty — set {} or {} env",
                ENV_API_KEY,
                opts.provider.api_key_env()
            ))));
        }
        let client = reqwest::Client::builder().timeout(HTTP_TIMEOUT).build().map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("cloud-api-verify client: {e}")))
        })?;
        Ok(Self {
            client,
            provider: opts.provider,
            model: opts.model,
            api_key: opts.api_key,
            max_retries: opts.max_retries,
        })
    }

    /// Single attempt — POST to the provider's structured-output endpoint
    /// and parse the response. Returns the parsed [`VerifyDecision`] or a
    /// structured `LunarisError`. The `Err` carries enough info for
    /// [`is_transient`] to classify whether a retry is appropriate.
    async fn try_once(&self, item: &NeedsReviewItem) -> Result<VerifyDecision, LunarisError> {
        let prompt = crate::arbitration_prompt(item);

        let (url, body, headers) = match self.provider {
            CloudProvider::Anthropic => self.build_anthropic_request(&prompt),
            CloudProvider::OpenAI => self.build_openai_request(&prompt),
            CloudProvider::Gemini => self.build_gemini_request(&prompt),
        };

        let mut req = self.client.post(&url).json(&body);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("cloud-api-verify: {e}")))
        })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "cloud-api-verify: HTTP {status}"
            ))));
        }

        let body_text = resp.text().await.map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("cloud-api-verify read: {e}")))
        })?;
        let decision_text = extract_provider_text(self.provider, &body_text)?;
        Ok(crate::parse_decision_json(&decision_text, provider_to_backend(self.provider)))
    }

    fn build_anthropic_request(
        &self,
        prompt: &str,
    ) -> (String, serde_json::Value, Vec<(&'static str, String)>) {
        let url = "https://api.anthropic.com/v1/messages".to_string();
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": 2048,
            "tools": [{
                "name": "emit_decision",
                "description": "Emit a verifier arbitration decision.",
                "input_schema": decision_json_schema(),
            }],
            "tool_choice": {"type": "tool", "name": "emit_decision"},
            "messages": [{"role": "user", "content": prompt}]
        });
        let headers = vec![
            ("x-api-key", self.api_key.clone()),
            ("anthropic-version", "2023-06-01".to_string()),
        ];
        (url, body, headers)
    }

    fn build_openai_request(
        &self,
        prompt: &str,
    ) -> (String, serde_json::Value, Vec<(&'static str, String)>) {
        let url = "https://api.openai.com/v1/chat/completions".to_string();
        let body = serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": prompt}],
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "lunaris_verify_decision",
                    "strict": true,
                    "schema": decision_json_schema(),
                }
            }
        });
        let headers = vec![("authorization", format!("Bearer {}", self.api_key))];
        (url, body, headers)
    }

    fn build_gemini_request(
        &self,
        prompt: &str,
    ) -> (String, serde_json::Value, Vec<(&'static str, String)>) {
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.model, self.api_key
        );
        let body = serde_json::json!({
            "contents": [{"role": "user", "parts": [{"text": prompt}]}],
            "generationConfig": {
                "responseMimeType": "application/json",
                "responseSchema": decision_json_schema(),
            }
        });
        (url, body, Vec::new())
    }
}

#[async_trait]
impl Verifier for CloudApiVerifier {
    async fn verify(
        &self,
        item: NeedsReviewItem,
    ) -> Result<VerifyDecision, LunarisError> {
        let mut last_err: Option<LunarisError> = None;
        let attempts = self.max_retries.saturating_add(1) as usize;
        for attempt in 0..attempts {
            match self.try_once(&item).await {
                Ok(d) => return Ok(d),
                Err(e) if is_transient(&e) && attempt + 1 < attempts => {
                    tracing::warn!(
                        err = %e, attempt = attempt + 1, max_attempts = attempts,
                        "cloud-api-verify transient; retrying"
                    );
                    last_err = Some(e);
                    tokio::time::sleep(RETRY_BACKOFF).await;
                    continue;
                }
                Err(e) => {
                    last_err = Some(e);
                    break;
                }
            }
        }
        // Retry exhausted (or non-transient on first attempt) — D-21
        // single-retry-then-flag: emit a deferred decision with a
        // `transient_after_retry: <error>` reason. The worker treats this
        // as abstain AND the operator can see the failure surfaced via
        // the audit log.
        let err_text =
            last_err.map(|e| e.to_string()).unwrap_or_else(|| "unknown error".to_string());
        tracing::warn!(
            err = %err_text,
            "cloud-api-verify retry exhausted; emitting transient-after-retry deferred decision"
        );
        Ok(VerifyDecision {
            winner_id: None,
            loser_id: None,
            reason: format!("transient_after_retry: {err_text}"),
            backend: provider_to_backend(self.provider),
            decided_at_iso: chrono::Utc::now().to_rfc3339(),
        })
    }
}

// ----------------------------- Wire format -----------------------------------

#[derive(Deserialize)]
#[allow(dead_code)]
struct DecisionWire {
    winner_id: Option<String>,
    loser_id: Option<String>,
    reason: Option<String>,
}

/// Provider-specific response decoder — returns the text payload that
/// `crate::parse_decision_json` will post-hoc-parse. Each provider wraps the
/// JSON object differently:
/// - Anthropic returns `{content: [{type: "tool_use", input: <json>}, ...]}`
/// - OpenAI returns `{choices: [{message: {content: <stringified-json>}}]}`
/// - Gemini returns `{candidates: [{content: {parts: [{text: <json>}]}}]}`
fn extract_provider_text(
    provider: CloudProvider,
    body: &str,
) -> Result<String, LunarisError> {
    match provider {
        CloudProvider::Anthropic => {
            let v: serde_json::Value = serde_json::from_str(body).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "cloud-api-verify returned wrong shape (anthropic parse): {e}"
                )))
            })?;
            let content = v["content"].as_array().ok_or_else(|| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "cloud-api-verify returned wrong shape (anthropic missing content): {body}"
                )))
            })?;
            let tool_input = content
                .iter()
                .find(|c| c["type"] == "tool_use")
                .and_then(|c| c.get("input"))
                .ok_or_else(|| {
                    LunarisError::Storage(StorageError::Backend(format!(
                        "cloud-api-verify returned wrong shape (anthropic missing tool_use): {body}"
                    )))
                })?;
            Ok(tool_input.to_string())
        }
        CloudProvider::OpenAI => {
            let v: serde_json::Value = serde_json::from_str(body).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "cloud-api-verify returned wrong shape (openai parse): {e}"
                )))
            })?;
            let content = v["choices"][0]["message"]["content"].as_str().ok_or_else(|| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "cloud-api-verify returned wrong shape (openai missing choices[0].message.content): {body}"
                )))
            })?;
            Ok(content.to_string())
        }
        CloudProvider::Gemini => {
            let v: serde_json::Value = serde_json::from_str(body).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "cloud-api-verify returned wrong shape (gemini parse): {e}"
                )))
            })?;
            let text = v["candidates"][0]["content"]["parts"][0]["text"]
                .as_str()
                .ok_or_else(|| {
                    LunarisError::Storage(StorageError::Backend(format!(
                        "cloud-api-verify returned wrong shape (gemini missing candidates[0].content.parts[0].text): {body}"
                    )))
                })?;
            Ok(text.to_string())
        }
    }
}

/// Classify a `LunarisError` as transient (D-21 retry-eligible) vs permanent.
/// Transient = network-blip / 429-backoff / 5xx server error / timeout.
fn is_transient(e: &LunarisError) -> bool {
    let msg = e.to_string();
    if msg.contains("HTTP 429") {
        return true;
    }
    if msg.contains("HTTP 5") {
        return true;
    }
    if msg.contains("timeout")
        || msg.contains("connection")
        || msg.contains("connect")
        || msg.contains("dns")
        || msg.contains("transient")
    {
        return true;
    }
    false
}

/// Shared JSON-schema for `{winner_id, loser_id, reason}` (passed verbatim
/// to all three providers).
fn decision_json_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "winner_id": {"type": "string"},
            "loser_id":  {"type": "string"},
            "reason":    {"type": "string"}
        },
        "required": ["winner_id", "loser_id", "reason"]
    })
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
        };
        let err = CloudApiVerifier::new(opts).expect_err("empty key must error");
        let msg = err.to_string();
        assert!(msg.contains("api_key is empty"), "got: {msg}");
        assert!(msg.contains("LUNARIS_VERIFY_API_KEY"), "got: {msg}");
    }

    #[test]
    fn is_transient_classifies_429_and_5xx() {
        let e_429 = LunarisError::Storage(StorageError::Backend("cloud-api-verify: HTTP 429".into()));
        let e_503 = LunarisError::Storage(StorageError::Backend("cloud-api-verify: HTTP 503".into()));
        let e_400 = LunarisError::Storage(StorageError::Backend("cloud-api-verify: HTTP 400".into()));
        let e_to = LunarisError::Storage(StorageError::Backend("cloud-api-verify: timeout".into()));
        assert!(is_transient(&e_429));
        assert!(is_transient(&e_503));
        assert!(!is_transient(&e_400));
        assert!(is_transient(&e_to));
    }

    #[test]
    fn debug_impl_redacts_api_key() {
        let opts = CloudApiVerifierOpts {
            provider: CloudProvider::Anthropic,
            model: "claude-3-5-sonnet-latest".into(),
            api_key: "sk-ant-SECRET-KEY-ABCDEF".into(),
            max_retries: 1,
        };
        let verifier = CloudApiVerifier::new(opts).unwrap();
        let dbg = format!("{verifier:?}");
        assert!(!dbg.contains("SECRET"), "Debug must redact api_key, got: {dbg}");
        assert!(!dbg.contains("sk-ant"), "Debug must redact api_key, got: {dbg}");
    }

    #[test]
    fn provider_to_backend_maps_correctly() {
        assert_eq!(provider_to_backend(CloudProvider::Anthropic), VerifierBackend::CloudAnthropic);
        assert_eq!(provider_to_backend(CloudProvider::OpenAI), VerifierBackend::CloudOpenAI);
        assert_eq!(provider_to_backend(CloudProvider::Gemini), VerifierBackend::CloudGemini);
    }
}
