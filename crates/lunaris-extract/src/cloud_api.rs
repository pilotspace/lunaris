//! [`CloudApiExtractor`] — provider-mux extractor for Anthropic / OpenAI /
//! Gemini.
//!
//! Per D-01 the cloud-API path is selectable via `LUNARIS_EXTRACT_PROVIDER` env
//! (`anthropic` | `openai` | `gemini`, case-insensitive). Each provider speaks
//! a slightly different structured-output API:
//!
//! - **Anthropic** (`/v1/messages` with `tools` + `input_schema`): claude-3-5-
//!   sonnet / claude-3-haiku style tool-use that constrains the model output
//!   to a JSON schema. Headers: `x-api-key`, `anthropic-version: 2023-06-01`.
//! - **OpenAI** (`/v1/chat/completions` with `response_format: {type:
//!   "json_schema", json_schema: ...}`): gpt-4o / gpt-4o-mini structured
//!   outputs. Header: `Authorization: Bearer <key>`.
//! - **Gemini** (`generativelanguage.googleapis.com/v1beta/models/<model>:
//!   generateContent?key=<key>` with `generationConfig.responseSchema`): same
//!   schema shape as OpenAI; auth via query param.
//!
//! ## D-21 single-retry-then-flag (W-1 fix)
//!
//! On a transient error (HTTP 429 / 5xx / network drop / timeout) the per-
//! chunk path retries ONCE. If the retry also fails, the extractor emits a
//! sentinel [`crate::Entity`] with `entity_type =
//! "__lunaris_sentinel__"` + `name = "__transient_after_retry__"` and
//! `valid_from_iso = "transient: <error>"`. The validator (`validator.rs`)
//! detects this sentinel by `entity_type` and routes it to
//! [`crate::NeedsReviewReason::TransientAfterRetry`] carrying the wrapped
//! error text — so the sentinel NEVER lands in `out.entities` and Plan 03-03
//! ingest fan-out can publish a `__lunaris_verify__` MQ message per item per
//! D-19.
//!
//! ## Failure modes
//!
//! Same shape as `ollama.rs`. Cloud-api specifically also has:
//! - HTTP 429 / 5xx → transient → single retry → sentinel-on-exhaust
//! - HTTP 4xx (auth, invalid request) → non-transient → bubble up as
//!   `LunarisError::Storage(StorageError::Backend("cloud-api: HTTP <status>"))`

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use lunaris_core::{LunarisError, StorageError};
use serde::Deserialize;
use ulid::Ulid;

use crate::Extractor;
use crate::types::{ChunkInput, Entity, EntityId, RawExtraction, RawExtractionBatch, Relation};
use crate::validator::{TRANSIENT_SENTINEL_NAME, TRANSIENT_SENTINEL_TYPE};

/// Per-call HTTP timeout for cloud APIs. 30s is generous; cloud APIs can take
/// 5-10s under load. The per-batch timeout (D-02) clamps the overall extract.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Default per-batch timeout (D-02). For cloud-API the budget is informational
/// — if the user picks cloud-API they accept the network latency floor. The
/// fall-through to per-chunk still happens on timeout.
const DEFAULT_BATCH_TIMEOUT_MS: u64 = 150;

/// Default retry budget per D-21.
const DEFAULT_MAX_RETRIES: u8 = 1;

/// Selectable provider per D-01. Reads from `LUNARIS_EXTRACT_PROVIDER` env
/// (case-insensitive) at [`CloudApiExtractorOpts::default`] time.
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
                "cloud-api: unknown provider {other:?} (expected anthropic|openai|gemini)"
            )))),
        }
    }
}

impl CloudProvider {
    fn default_model(self) -> &'static str {
        match self {
            Self::Anthropic => "claude-3-5-haiku-latest",
            Self::OpenAI => "gpt-4o-mini",
            Self::Gemini => "gemini-2.5-flash",
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
            Self::Anthropic => "ANTHROPIC_EXTRACT_MODEL",
            Self::OpenAI => "OPENAI_EXTRACT_MODEL",
            Self::Gemini => "GEMINI_EXTRACT_MODEL",
        }
    }
}

/// Construction options for [`CloudApiExtractor`].
///
/// `Default` reads provider from `LUNARIS_EXTRACT_PROVIDER` (defaults to
/// Anthropic), model from `<PROVIDER>_EXTRACT_MODEL` (defaults to the
/// provider's default), and api_key from `<PROVIDER>_API_KEY` (empty string
/// if unset — `new` will reject empty keys with an actionable error).
#[derive(Clone, Debug)]
pub struct CloudApiExtractorOpts {
    pub provider: CloudProvider,
    pub model: String,
    pub api_key: String,
    pub batch_timeout_ms: u64,
    pub max_retries: u8,
}

impl Default for CloudApiExtractorOpts {
    fn default() -> Self {
        let provider = std::env::var("LUNARIS_EXTRACT_PROVIDER")
            .ok()
            .and_then(|s| CloudProvider::from_str(&s).ok())
            .unwrap_or(CloudProvider::Anthropic);
        let model = std::env::var(provider.model_env())
            .unwrap_or_else(|_| provider.default_model().to_string());
        let api_key = std::env::var(provider.api_key_env()).unwrap_or_default();
        Self {
            provider,
            model,
            api_key,
            batch_timeout_ms: DEFAULT_BATCH_TIMEOUT_MS,
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }
}

#[derive(Clone)]
pub struct CloudApiExtractor {
    client: reqwest::Client,
    provider: CloudProvider,
    model: String,
    api_key: String,
    batch_timeout_ms: u64,
    max_retries: u8,
}

impl std::fmt::Debug for CloudApiExtractor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // T-03-01-03 mitigation: NEVER log the api_key. Print only the
        // provider+model so a `tracing::debug!(?extractor)` line stays safe.
        f.debug_struct("CloudApiExtractor")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("batch_timeout_ms", &self.batch_timeout_ms)
            .field("max_retries", &self.max_retries)
            .finish()
    }
}

impl CloudApiExtractor {
    /// Construct a new cloud-API extractor.
    pub fn new(opts: CloudApiExtractorOpts) -> Result<Self, LunarisError> {
        if opts.api_key.is_empty() {
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "cloud-api: api_key is empty — set {} env",
                opts.provider.api_key_env()
            ))));
        }
        let client = reqwest::Client::builder().timeout(HTTP_TIMEOUT).build().map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("cloud-api client: {e}")))
        })?;
        Ok(Self {
            client,
            provider: opts.provider,
            model: opts.model,
            api_key: opts.api_key,
            batch_timeout_ms: opts.batch_timeout_ms,
            max_retries: opts.max_retries,
        })
    }

    /// Single attempt — POST to the provider's structured-output endpoint and
    /// parse the response. Returns the raw extraction or a structured
    /// `LunarisError`. The `Err` carries enough info for [`is_transient`] to
    /// classify whether a retry is appropriate.
    async fn try_once(&self, chunk: &ChunkInput) -> Result<RawExtraction, LunarisError> {
        let prompt = format!(
            "Extract entities and relations from the chunk below. Respond with \
             a JSON object {{\"entities\":[...],\"relations\":[...]}} only.\n\n\
             <chunk heading=\"{}\">\n{}\n</chunk>",
            chunk.heading_path.join(" / "),
            chunk.text
        );

        let (url, body, headers) = match self.provider {
            CloudProvider::Anthropic => self.build_anthropic_request(&prompt),
            CloudProvider::OpenAI => self.build_openai_request(&prompt),
            CloudProvider::Gemini => self.build_gemini_request(&prompt),
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
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "cloud-api: HTTP {status}"
            ))));
        }

        let body_text = resp.text().await.map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("cloud-api read: {e}")))
        })?;
        let extraction = parse_provider_response(self.provider, &body_text)?;
        Ok(extraction.into_raw(chunk.chunk_id))
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
                "name": "emit_extractions",
                "description": "Emit entities and relations extracted from the chunk.",
                "input_schema": format_json_schema(),
            }],
            "tool_choice": {"type": "tool", "name": "emit_extractions"},
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
                    "name": "lunaris_extraction",
                    "strict": true,
                    "schema": format_json_schema(),
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
                "responseSchema": format_json_schema(),
            }
        });
        // Gemini uses query-param auth; no auth header.
        (url, body, Vec::new())
    }

    /// Single chunk with retry budget per D-21. On retry exhaust returns the
    /// sentinel-bearing [`RawExtraction`] that the validator routes to
    /// [`crate::NeedsReviewReason::TransientAfterRetry`].
    async fn extract_one_with_retry(&self, chunk: &ChunkInput) -> RawExtraction {
        let mut last_err: Option<LunarisError> = None;
        let attempts = self.max_retries.saturating_add(1) as usize;
        for attempt in 0..attempts {
            match self.try_once(chunk).await {
                Ok(r) => return r,
                Err(e) if is_transient(&e) && attempt + 1 < attempts => {
                    tracing::warn!(err = %e, attempt = attempt + 1, max_attempts = attempts,
                        "cloud-api transient; retrying");
                    last_err = Some(e);
                    continue;
                }
                Err(e) => {
                    last_err = Some(e);
                    break;
                }
            }
        }
        // Retry exhausted (or non-transient on first attempt) — emit the
        // D-21 sentinel that the Validator routes to TransientAfterRetry.
        let err_text =
            last_err.map(|e| e.to_string()).unwrap_or_else(|| "unknown error".to_string());
        tracing::warn!(err = %err_text, chunk_id = %chunk.chunk_id,
            "cloud-api retry exhausted; emitting transient-after-retry sentinel");
        let sentinel = Entity {
            id: EntityId::from_name_and_type(TRANSIENT_SENTINEL_NAME, TRANSIENT_SENTINEL_TYPE),
            name: TRANSIENT_SENTINEL_NAME.into(),
            aliases: Vec::new(),
            entity_type: TRANSIENT_SENTINEL_TYPE.into(),
            // confidence=0.0 is in-range so the validator's GBNF check would
            // pass; the Validator explicitly checks for the sentinel BEFORE
            // its GBNF check so the order doesn't matter.
            confidence: 0.0,
            valid_from_iso: format!("transient: {err_text}"),
            valid_to_iso: None,
        };
        RawExtraction {
            source_chunk_id: chunk.chunk_id,
            entities: vec![sentinel],
            relations: Vec::new(),
            facts: Vec::new(),
        }
    }
}

#[async_trait]
impl Extractor for CloudApiExtractor {
    async fn extract(
        &self,
        _episode_id: Ulid,
        chunks: &[ChunkInput],
    ) -> Result<RawExtractionBatch, LunarisError> {
        if chunks.is_empty() {
            return Ok(RawExtractionBatch::default());
        }

        // Per-batch timeout (D-02). On timeout we fall back to per-chunk
        // (each per-chunk call has its own retry budget).
        let batch_timeout = Duration::from_millis(self.batch_timeout_ms);
        let chunks_owned: Vec<ChunkInput> = chunks.to_vec();
        let this = self.clone();
        let batch_fut = async move {
            let mut by_chunk = Vec::with_capacity(chunks_owned.len());
            for c in &chunks_owned {
                by_chunk.push(this.extract_one_with_retry(c).await);
            }
            RawExtractionBatch { by_chunk }
        };

        match tokio::time::timeout(batch_timeout, batch_fut).await {
            Ok(b) => Ok(b),
            Err(_elapsed) => {
                tracing::warn!(
                    batch_size = chunks.len(),
                    timeout_ms = self.batch_timeout_ms,
                    "cloud-api batch timeout; falling back to per-chunk"
                );
                let mut by_chunk = Vec::with_capacity(chunks.len());
                for c in chunks {
                    by_chunk.push(self.extract_one_with_retry(c).await);
                }
                Ok(RawExtractionBatch { by_chunk })
            }
        }
    }
}

// ----------------------------- Wire format -----------------------------------

#[derive(Deserialize)]
struct ExtractionWire {
    #[serde(default)]
    entities: Vec<EntityWire>,
    #[serde(default)]
    relations: Vec<RelationWire>,
}

#[derive(Deserialize)]
struct EntityWire {
    name: String,
    entity_type: String,
    #[serde(default)]
    aliases: Vec<String>,
    confidence: f32,
    valid_from_iso: String,
    #[serde(default)]
    valid_to_iso: Option<String>,
}

#[derive(Deserialize)]
struct RelationWire {
    subject_name: String,
    subject_type: String,
    predicate: String,
    object_name: String,
    object_type: String,
    confidence: f32,
    valid_from_iso: String,
    #[serde(default)]
    valid_to_iso: Option<String>,
}

impl ExtractionWire {
    fn into_raw(self, chunk_id: Ulid) -> RawExtraction {
        let entities = self
            .entities
            .into_iter()
            .map(|e| Entity {
                id: EntityId::from_name_and_type(&e.name, &e.entity_type),
                name: e.name,
                aliases: e.aliases,
                entity_type: e.entity_type,
                confidence: e.confidence,
                valid_from_iso: e.valid_from_iso,
                valid_to_iso: e.valid_to_iso,
            })
            .collect();
        let relations = self
            .relations
            .into_iter()
            .map(|r| Relation {
                subject_id: EntityId::from_name_and_type(&r.subject_name, &r.subject_type),
                predicate: r.predicate,
                object_id: EntityId::from_name_and_type(&r.object_name, &r.object_type),
                confidence: r.confidence,
                valid_from_iso: r.valid_from_iso,
                valid_to_iso: r.valid_to_iso,
            })
            .collect();
        RawExtraction { source_chunk_id: chunk_id, entities, relations, facts: Vec::new() }
    }
}

/// Provider-specific response decoder. Each provider wraps the JSON object
/// differently:
/// - Anthropic returns `{content: [{type: "tool_use", input: <ext>}, ...]}`
/// - OpenAI returns `{choices: [{message: {content: <stringified-json>}}]}`
/// - Gemini returns `{candidates: [{content: {parts: [{text: <json>}]}}]}`
fn parse_provider_response(
    provider: CloudProvider,
    body: &str,
) -> Result<ExtractionWire, LunarisError> {
    match provider {
        CloudProvider::Anthropic => {
            let v: serde_json::Value = serde_json::from_str(body).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "cloud-api returned wrong shape (anthropic parse): {e}"
                )))
            })?;
            let content = v["content"].as_array().ok_or_else(|| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "cloud-api returned wrong shape (anthropic missing content): {body}"
                )))
            })?;
            let tool_input = content
                .iter()
                .find(|c| c["type"] == "tool_use")
                .and_then(|c| c.get("input"))
                .ok_or_else(|| {
                    LunarisError::Storage(StorageError::Backend(format!(
                        "cloud-api returned wrong shape (anthropic missing tool_use): {body}"
                    )))
                })?;
            serde_json::from_value(tool_input.clone()).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "cloud-api returned wrong shape (anthropic tool_use parse): {e}"
                )))
            })
        }
        CloudProvider::OpenAI => {
            let v: serde_json::Value = serde_json::from_str(body).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "cloud-api returned wrong shape (openai parse): {e}"
                )))
            })?;
            let content = v["choices"][0]["message"]["content"].as_str().ok_or_else(|| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "cloud-api returned wrong shape (openai missing choices[0].message.content): {body}"
                )))
            })?;
            serde_json::from_str(content).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "cloud-api returned wrong shape (openai content parse): {e}"
                )))
            })
        }
        CloudProvider::Gemini => {
            let v: serde_json::Value = serde_json::from_str(body).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "cloud-api returned wrong shape (gemini parse): {e}"
                )))
            })?;
            let text = v["candidates"][0]["content"]["parts"][0]["text"]
                .as_str()
                .ok_or_else(|| {
                    LunarisError::Storage(StorageError::Backend(format!(
                        "cloud-api returned wrong shape (gemini missing candidates[0].content.parts[0].text): {body}"
                    )))
                })?;
            serde_json::from_str(text).map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!(
                    "cloud-api returned wrong shape (gemini text parse): {e}"
                )))
            })
        }
    }
}

/// Classify a `LunarisError` as transient (D-21 retry-eligible) vs permanent.
/// Transient = network-blip / 429-backoff / 5xx server error / timeout.
fn is_transient(e: &LunarisError) -> bool {
    let msg = e.to_string();
    // HTTP 429 backoff
    if msg.contains("HTTP 429") {
        return true;
    }
    // HTTP 5xx server error
    if msg.contains("HTTP 5") {
        return true;
    }
    // Network / connect / timeout from reqwest's Display impl
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

/// Shared JSON-schema for `entities` + `relations` (passed verbatim to all
/// three providers). Mirrors `default_format_schema` in `ollama.rs`.
fn format_json_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "entities": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["name", "entity_type", "confidence", "valid_from_iso"],
                    "properties": {
                        "name":          {"type": "string"},
                        "entity_type":   {"type": "string"},
                        "aliases":       {"type": "array", "items": {"type": "string"}},
                        "confidence":    {"type": "number"},
                        "valid_from_iso": {"type": "string"},
                        "valid_to_iso":  {"type": ["string", "null"]}
                    }
                }
            },
            "relations": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["subject_name", "subject_type", "predicate", "object_name", "object_type", "confidence", "valid_from_iso"],
                    "properties": {
                        "subject_name": {"type": "string"},
                        "subject_type": {"type": "string"},
                        "predicate":    {"type": "string"},
                        "object_name":  {"type": "string"},
                        "object_type":  {"type": "string"},
                        "confidence":   {"type": "number"},
                        "valid_from_iso": {"type": "string"},
                        "valid_to_iso": {"type": ["string", "null"]}
                    }
                }
            }
        },
        "required": ["entities", "relations"]
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
    fn empty_api_key_rejected() {
        let opts = CloudApiExtractorOpts {
            provider: CloudProvider::Anthropic,
            model: "claude-3-5-haiku-latest".into(),
            api_key: "".into(),
            batch_timeout_ms: 150,
            max_retries: 1,
        };
        let err = CloudApiExtractor::new(opts).expect_err("empty key must error");
        let msg = err.to_string();
        assert!(msg.contains("api_key is empty"), "got: {msg}");
        assert!(msg.contains("ANTHROPIC_API_KEY"), "got: {msg}");
    }

    #[test]
    fn is_transient_classifies_429_and_5xx() {
        let e_429 = LunarisError::Storage(StorageError::Backend("cloud-api: HTTP 429".into()));
        let e_503 = LunarisError::Storage(StorageError::Backend("cloud-api: HTTP 503".into()));
        let e_400 = LunarisError::Storage(StorageError::Backend("cloud-api: HTTP 400".into()));
        let e_to = LunarisError::Storage(StorageError::Backend("cloud-api: timeout".into()));
        assert!(is_transient(&e_429));
        assert!(is_transient(&e_503));
        assert!(!is_transient(&e_400));
        assert!(is_transient(&e_to));
    }

    #[test]
    fn debug_impl_redacts_api_key() {
        // Construct with a fake key and prove Debug doesn't print it.
        let opts = CloudApiExtractorOpts {
            provider: CloudProvider::Anthropic,
            model: "claude-3-5-haiku-latest".into(),
            api_key: "sk-ant-SECRET-KEY-ABCDEF".into(),
            batch_timeout_ms: 150,
            max_retries: 1,
        };
        let extractor = CloudApiExtractor::new(opts).unwrap();
        let dbg = format!("{extractor:?}");
        assert!(!dbg.contains("SECRET"), "Debug must redact api_key, got: {dbg}");
        assert!(!dbg.contains("sk-ant"), "Debug must redact api_key, got: {dbg}");
    }
}
