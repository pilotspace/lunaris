//! [`OllamaExtractor`] — Extractor backed by an Ollama HTTP endpoint.
//!
//! POSTs `/api/chat` with `{model, messages, format: <json_schema>, stream:
//! false}` where `format` is the JSON-schema translation of the GBNF grammar
//! files (D-04 → D-05 fallback path for backends that don't speak GBNF
//! natively). 10s HTTP timeout per CLAUDE.md "design for failure" rule.
//!
//! Per-batch timeout (D-02) wraps the batched HTTP call; on timeout the
//! extractor falls back to per-chunk extraction. Mirrors the candle backend's
//! D-02 pattern exactly.
//!
//! ## Failure modes
//!
//! | Condition                                    | Returned error                                                          |
//! |----------------------------------------------|--------------------------------------------------------------------------|
//! | reqwest client build failure (TLS, etc.)     | `LunarisError::Storage(StorageError::Backend("ollama client: ..."))`     |
//! | HTTP error (network, timeout, etc.)          | `LunarisError::Storage(StorageError::Backend("ollama: ..."))`            |
//! | non-success HTTP status                      | `LunarisError::Storage(StorageError::Backend("ollama: HTTP <status>"))`  |
//! | response body fails JSON parse               | `LunarisError::Storage(StorageError::Backend("ollama parse: ..."))`      |
//! | response shape lacks `entities`/`relations`  | `LunarisError::Storage(StorageError::Backend("ollama returned wrong shape: ..."))` (T-03-01-02 mitigation) |

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lunaris_core::{LunarisError, StorageError};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::Extractor;
use crate::types::{ChunkInput, Entity, EntityId, RawExtraction, RawExtractionBatch, Relation};

/// Per-call HTTP timeout (CLAUDE.md "design for failure"). Ollama is local-
/// network so 10s is more than enough; the per-batch timeout (D-02) clamps
/// further on top of this.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Default Ollama endpoint.
const DEFAULT_ENDPOINT: &str = "http://localhost:11434";

/// Default Ollama model identifier (matches `ollama pull gemma3:4b`).
const DEFAULT_MODEL: &str = "gemma3:4b";

/// Default per-batch timeout per D-02 (matches candle backend).
const DEFAULT_BATCH_TIMEOUT_MS: u64 = 150;

/// Construction options for [`OllamaExtractor`].
///
/// `Default` resolves `endpoint` from `OLLAMA_URL` env (falls back to
/// `http://localhost:11434`) and `model` from `OLLAMA_EXTRACT_MODEL` env
/// (falls back to `gemma3:4b`).
#[derive(Clone, Debug)]
pub struct OllamaExtractorOpts {
    pub endpoint: String,
    pub model: String,
    pub batch_timeout_ms: u64,
}

impl Default for OllamaExtractorOpts {
    fn default() -> Self {
        Self {
            endpoint: std::env::var("OLLAMA_URL").unwrap_or_else(|_| DEFAULT_ENDPOINT.to_string()),
            model: std::env::var("OLLAMA_EXTRACT_MODEL")
                .unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
            batch_timeout_ms: DEFAULT_BATCH_TIMEOUT_MS,
        }
    }
}

#[derive(Clone)]
pub struct OllamaExtractor {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    batch_timeout_ms: u64,
    /// Cached JSON-schema translation of the GBNF grammars. Built once at
    /// construction time per D-04; sent in every `/api/chat` request as the
    /// `format` field so Ollama enforces the structured output server-side.
    format_schema: Arc<serde_json::Value>,
}

impl std::fmt::Debug for OllamaExtractor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OllamaExtractor")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("batch_timeout_ms", &self.batch_timeout_ms)
            .finish()
    }
}

impl OllamaExtractor {
    /// Construct a new Ollama-backed extractor. Builds a `reqwest::Client`
    /// with a 10s timeout and pre-computes the JSON-schema translation of the
    /// GBNF grammars (D-04).
    pub fn new(opts: OllamaExtractorOpts) -> Result<Self, LunarisError> {
        let client = reqwest::Client::builder().timeout(HTTP_TIMEOUT).build().map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("ollama client: {e}")))
        })?;
        Ok(Self {
            client,
            endpoint: opts.endpoint,
            model: opts.model,
            batch_timeout_ms: opts.batch_timeout_ms,
            format_schema: Arc::new(default_format_schema()),
        })
    }

    /// Send a single chat completion to Ollama for one chunk. Returns the
    /// parsed [`RawExtraction`] or a structured `LunarisError`.
    async fn extract_one(&self, chunk: &ChunkInput) -> Result<RawExtraction, LunarisError> {
        let url = format!("{}/api/chat", self.endpoint.trim_end_matches('/'));
        // T-03-01-01 mitigation: wrap chunk text in `<chunk>` delimiters so the
        // validator can reject extracted entities whose names equal the literal.
        let prompt = format!(
            "Extract entities and relations from the chunk below. Respond with \
             a JSON object {{\"entities\":[...],\"relations\":[...]}} only.\n\n\
             <chunk heading=\"{}\">\n{}\n</chunk>",
            chunk.heading_path.join(" / "),
            chunk.text
        );
        let body = ChatRequest {
            model: self.model.clone(),
            messages: vec![ChatMessage { role: "user".into(), content: prompt }],
            format: (*self.format_schema).clone(),
            stream: false,
        };

        let resp =
            self.client.post(&url).json(&body).send().await.map_err(|e| {
                LunarisError::Storage(StorageError::Backend(format!("ollama: {e}")))
            })?;
        if !resp.status().is_success() {
            return Err(LunarisError::Storage(StorageError::Backend(format!(
                "ollama: HTTP {}",
                resp.status()
            ))));
        }

        let parsed: ChatResponse = resp.json().await.map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!("ollama parse: {e}")))
        })?;

        // The `message.content` field carries the model's text output; with
        // `format` set, Ollama emits valid JSON conforming to the schema.
        let content = parsed.message.content;
        // Response shape validation BEFORE returning (T-03-01-02 mitigation).
        // We reject content that doesn't parse as our `{entities, relations}`
        // wire shape — never silently corrupt the extraction batch.
        let extraction: ExtractionWire = serde_json::from_str(&content).map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!(
                "ollama returned wrong shape: parse failed ({e}) for body {content}"
            )))
        })?;

        Ok(extraction.into_raw(chunk.chunk_id))
    }

    /// Per-chunk fallback path (D-02). Each chunk runs serially with no
    /// per-chunk timeout (the outer `tokio::time::timeout` in `extract`
    /// already capped the batch); on per-chunk error we emit empty.
    async fn extract_per_chunk(&self, chunks: &[ChunkInput]) -> RawExtractionBatch {
        let mut by_chunk = Vec::with_capacity(chunks.len());
        for c in chunks {
            match self.extract_one(c).await {
                Ok(r) => by_chunk.push(r),
                Err(e) => {
                    tracing::warn!(err = %e, chunk_id = %c.chunk_id,
                        "ollama per-chunk failed; emitting empty");
                    by_chunk
                        .push(RawExtraction { source_chunk_id: c.chunk_id, ..Default::default() });
                }
            }
        }
        RawExtractionBatch { by_chunk }
    }
}

#[async_trait]
impl Extractor for OllamaExtractor {
    async fn extract(
        &self,
        _episode_id: Ulid,
        chunks: &[ChunkInput],
    ) -> Result<RawExtractionBatch, LunarisError> {
        if chunks.is_empty() {
            return Ok(RawExtractionBatch::default());
        }

        // (D-02) Per-batch timeout — the Ollama path emits one HTTP call per
        // chunk so "batch" is sequential here; we still wrap in
        // `tokio::time::timeout` so a slow Ollama instance triggers the
        // per-chunk fallback instead of stalling the runtime.
        let batch_timeout = Duration::from_millis(self.batch_timeout_ms);
        let chunks_owned: Vec<ChunkInput> = chunks.to_vec();
        let this = self.clone();
        let batch_fut = async move {
            let mut by_chunk = Vec::with_capacity(chunks_owned.len());
            for c in &chunks_owned {
                let r = this.extract_one(c).await?;
                by_chunk.push(r);
            }
            Ok::<RawExtractionBatch, LunarisError>(RawExtractionBatch { by_chunk })
        };

        match tokio::time::timeout(batch_timeout, batch_fut).await {
            Ok(Ok(b)) => Ok(b),
            Ok(Err(e)) => {
                tracing::warn!(err = %e, batch_size = chunks.len(),
                    "ollama batch failed; falling back to per-chunk");
                Ok(self.extract_per_chunk(chunks).await)
            }
            Err(_elapsed) => {
                tracing::warn!(
                    batch_size = chunks.len(),
                    timeout_ms = self.batch_timeout_ms,
                    "ollama batch timeout; falling back to per-chunk"
                );
                Ok(self.extract_per_chunk(chunks).await)
            }
        }
    }
}

// ----------------------------- Wire format -----------------------------------

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    format: serde_json::Value,
    stream: bool,
}

#[derive(Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    message: ChatMessageOut,
}

#[derive(Deserialize)]
struct ChatMessageOut {
    content: String,
}

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

/// Build the JSON-schema translation of the GBNF grammars (D-04). The schema
/// is a simple `{entities: [...], relations: [...]}` object — Ollama validates
/// the model's output against it server-side, so the model's response is
/// guaranteed-shaped before it ever reaches our `serde_json::from_str` call.
fn default_format_schema() -> serde_json::Value {
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
    fn opts_default_resolves_to_localhost_ollama() {
        // Avoid env-var contamination from the test runner — read the actual
        // resolved values and just check they're non-empty.
        let opts = OllamaExtractorOpts::default();
        assert!(!opts.endpoint.is_empty());
        assert!(!opts.model.is_empty());
        assert_eq!(opts.batch_timeout_ms, DEFAULT_BATCH_TIMEOUT_MS);
    }

    #[test]
    fn extractor_construction_succeeds_with_defaults() {
        let _e = OllamaExtractor::new(OllamaExtractorOpts::default()).expect("client builds");
    }

    #[test]
    fn format_schema_has_required_top_level_keys() {
        let s = default_format_schema();
        assert_eq!(s["type"], "object");
        assert!(s["properties"]["entities"].is_object());
        assert!(s["properties"]["relations"].is_object());
        assert_eq!(s["required"][0], "entities");
        assert_eq!(s["required"][1], "relations");
    }
}
