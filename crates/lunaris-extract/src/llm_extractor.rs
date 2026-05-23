//! [`LlmExtractor`] — backend-agnostic [`Extractor`] consuming any
//! `Arc<dyn LlmBackend>` from `lunaris-llm`.
//!
//! ## Why this exists
//!
//! Phase 11 unifies the three LLM-using pipelines (extract, verify,
//! reflect) onto a single trait — `lunaris_llm::LlmBackend`. This module
//! is the extract-side adapter: it owns the GBNF-instructed prompt
//! template, the per-batch / per-chunk timeout fallback (D-02), and the
//! post-hoc JSON parse against the GBNF grammar shape (D-05). Whatever
//! `LlmBackend` impl the caller picks (`CandleBackend`, `OllamaBackend`,
//! `CloudBackend`, or a test stub) flows through this one extractor.
//!
//! ## Coexistence with the legacy impls
//!
//! The pre-existing [`crate::CandleGemma3_4B`] / [`crate::OllamaExtractor`]
//! / [`crate::CloudApiExtractor`] are **unchanged** by this commit. They
//! remain the documented v0.2 backends and continue to load the same
//! weights / hit the same endpoints. This adapter is *additive*: new
//! callers can opt in via `Lunaris::with_extractor(Arc::new(
//! LlmExtractor::new(backend)))` without touching v0.2 paths. The
//! follow-up commit (per the agreed migration plan) re-implements the
//! legacy structs as thin wrappers around `LlmExtractor` and deletes the
//! duplicated load / forward / decode code.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lunaris_core::LunarisError;
use lunaris_llm::{GenOpts, LlmBackend, SchemaConstraint};
use ulid::Ulid;

use crate::Extractor;
use crate::types::{ChunkInput, Entity, EntityId, RawExtraction, RawExtractionBatch, Relation};

/// Construction options for [`LlmExtractor`]. Mirrors the D-02 budgets
/// from the existing extract backends so a drop-in swap preserves
/// behaviour.
#[derive(Clone, Debug)]
pub struct LlmExtractorOpts {
    /// Per-batch timeout — wraps the whole-batch generate call. On
    /// timeout the extractor falls back to per-chunk extraction.
    pub batch_timeout_ms: u64,
    /// Per-chunk timeout for the fallback path. On timeout an empty
    /// extraction is emitted with `tracing::warn!`.
    pub per_chunk_timeout_ms: u64,
    /// Max output tokens passed to [`LlmBackend::generate`].
    pub max_tokens: u32,
    /// Sampling temperature (0.0 = greedy).
    pub temperature: f32,
    /// Optional GBNF grammar text passed through as
    /// [`SchemaConstraint::Gbnf`]. The candle backend appends this to
    /// the prompt; ollama drops it (no GBNF support); cloud-API drops
    /// it (use `JsonSchema` mode instead). When this is `None`, the
    /// extractor sends [`SchemaConstraint::None`] — the lighter prompt
    /// path. Set this when wrapping a legacy v0.2 candle path that
    /// expected the grammar text inline (preserves D-04 / D-05
    /// behavior under the wrapper migration).
    pub gbnf: Option<&'static str>,
}

impl Default for LlmExtractorOpts {
    fn default() -> Self {
        Self {
            batch_timeout_ms: 150,
            per_chunk_timeout_ms: 450,
            max_tokens: 512,
            temperature: 0.0,
            gbnf: None,
        }
    }
}

/// Backend-agnostic extractor. Holds an `Arc<dyn LlmBackend>` so the
/// underlying provider can be swapped at runtime via `LlmConfig`.
#[derive(Clone)]
pub struct LlmExtractor {
    backend: Arc<dyn LlmBackend>,
    opts: LlmExtractorOpts,
}

impl std::fmt::Debug for LlmExtractor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmExtractor")
            .field("model_id", &self.backend.model_id())
            .field("opts", &self.opts)
            .finish()
    }
}

impl LlmExtractor {
    pub fn new(backend: Arc<dyn LlmBackend>) -> Self {
        Self { backend, opts: LlmExtractorOpts::default() }
    }

    pub fn with_opts(backend: Arc<dyn LlmBackend>, opts: LlmExtractorOpts) -> Self {
        Self { backend, opts }
    }

    /// Single-chunk extract. Builds the prompt, calls the backend, and
    /// post-hoc parses the JSON. On any error or non-JSON output, emits
    /// an empty extraction (matches the existing candle behaviour — a
    /// poisoned extraction is worse than an empty one).
    async fn extract_one(&self, chunk: &ChunkInput) -> RawExtraction {
        let prompt = build_prompt(chunk);
        let gen_opts = GenOpts {
            max_tokens: self.opts.max_tokens,
            temperature: self.opts.temperature,
            timeout: Duration::from_millis(self.opts.per_chunk_timeout_ms),
        };
        // Honour `opts.gbnf` when set — this preserves the legacy
        // CandleGemma3_4B prompt-quality behaviour (which embedded the
        // ENTITIES_GBNF + RELATIONS_GBNF grammars). When `None`, the
        // lighter prompt-only path is used; backends that support a
        // transport-level schema (Ollama JSON-schema / cloud-API
        // tool-use) are wired separately on a per-call basis.
        let constraint = match &self.opts.gbnf {
            Some(g) => SchemaConstraint::Gbnf(g),
            None => SchemaConstraint::None,
        };
        match self.backend.generate(&prompt, constraint, gen_opts).await {
            Ok(decoded) => parse_extraction_json(&decoded, chunk.chunk_id),
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    chunk_id = %chunk.chunk_id,
                    model_id = self.backend.model_id(),
                    "LlmExtractor generate failed; emitting empty extraction"
                );
                RawExtraction { source_chunk_id: chunk.chunk_id, ..Default::default() }
            }
        }
    }
}

#[async_trait]
impl Extractor for LlmExtractor {
    async fn extract(
        &self,
        _episode_id: Ulid,
        chunks: &[ChunkInput],
    ) -> Result<RawExtractionBatch, LunarisError> {
        if chunks.is_empty() {
            return Ok(RawExtractionBatch::default());
        }
        // Per-batch timeout (D-02) wraps the whole loop. On timeout we
        // emit best-effort partials rather than failing the batch.
        let batch_timeout = Duration::from_millis(self.opts.batch_timeout_ms);
        let chunks_owned = chunks.to_vec();
        let this = self.clone();
        let batch_fut = async move {
            let mut by_chunk = Vec::with_capacity(chunks_owned.len());
            for c in &chunks_owned {
                by_chunk.push(this.extract_one(c).await);
            }
            RawExtractionBatch { by_chunk }
        };
        match tokio::time::timeout(batch_timeout, batch_fut).await {
            Ok(b) => Ok(b),
            Err(_elapsed) => {
                tracing::warn!(
                    batch_size = chunks.len(),
                    timeout_ms = self.opts.batch_timeout_ms,
                    model_id = self.backend.model_id(),
                    "LlmExtractor batch timeout; emitting empty per-chunk extractions"
                );
                let by_chunk = chunks
                    .iter()
                    .map(|c| RawExtraction { source_chunk_id: c.chunk_id, ..Default::default() })
                    .collect();
                Ok(RawExtractionBatch { by_chunk })
            }
        }
    }

    fn applies(&self) -> bool {
        self.backend.applies()
    }
}

fn build_prompt(chunk: &ChunkInput) -> String {
    // T-03-01-01 mitigation: wrap chunk text in `<chunk>` delimiters so
    // the downstream validator can flag any extracted entity whose name
    // equals the literal delimiter (prompt-injection guard).
    format!(
        "Extract entities and relations from the chunk below as JSON.\n\n\
         Respond with a JSON object {{\"entities\":[...],\"relations\":[...]}} only.\n\n\
         <chunk heading=\"{}\">\n{}\n</chunk>",
        chunk.heading_path.join(" / "),
        chunk.text
    )
}

/// Best-effort JSON parse of the model's decoded output. Tolerant of
/// trailing junk; extracts the first balanced `{` ... `}` substring.
///
/// `pub(crate)` so `cloud_api` can reuse the same parse path when building
/// its own extraction result (it delegates `generate()` but not the full
/// `LlmExtractor::extract` path, to preserve the D-21 sentinel contract).
///
/// Only callable when the `cloud-api` feature is enabled; the attribute
/// suppresses a dead_code lint when it is not.
#[cfg(feature = "cloud-api")]
pub(crate) fn parse_extraction_json_pub(decoded: &str, chunk_id: Ulid) -> RawExtraction {
    parse_extraction_json(decoded, chunk_id)
}

fn parse_extraction_json(decoded: &str, chunk_id: Ulid) -> RawExtraction {
    let Some(start) = decoded.find('{') else {
        return RawExtraction { source_chunk_id: chunk_id, ..Default::default() };
    };
    let bytes = decoded.as_bytes();
    let mut depth = 0_i32;
    let mut end_excl = start;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end_excl = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    if end_excl == start {
        return RawExtraction { source_chunk_id: chunk_id, ..Default::default() };
    }
    let json_slice = &decoded[start..end_excl];
    match serde_json::from_str::<ExtractionJson>(json_slice) {
        Ok(parsed) => parsed.into_raw(chunk_id),
        Err(e) => {
            tracing::warn!(
                err = %e,
                chunk_id = %chunk_id,
                "LlmExtractor JSON parse failed; emitting empty extraction"
            );
            RawExtraction { source_chunk_id: chunk_id, ..Default::default() }
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct ExtractionJson {
    #[serde(default)]
    entities: Vec<EntityJson>,
    #[serde(default)]
    relations: Vec<RelationJson>,
}

#[derive(Debug, serde::Deserialize)]
struct EntityJson {
    name: String,
    entity_type: String,
    #[serde(default)]
    aliases: Vec<String>,
    confidence: f32,
    valid_from_iso: String,
    #[serde(default)]
    valid_to_iso: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct RelationJson {
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

impl ExtractionJson {
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

#[cfg(test)]
mod tests {
    use super::*;
    use lunaris_llm::FauxBackend;

    fn chunk(text: &str) -> ChunkInput {
        ChunkInput {
            chunk_id: Ulid::new(),
            heading_path: vec!["section".into()],
            text: text.into(),
        }
    }

    #[tokio::test]
    async fn empty_chunks_returns_empty_batch() {
        let backend: Arc<dyn LlmBackend> = Arc::new(FauxBackend::new());
        let extractor = LlmExtractor::new(backend);
        let out = extractor.extract(Ulid::new(), &[]).await.unwrap();
        assert!(out.by_chunk.is_empty());
    }

    #[tokio::test]
    async fn parses_valid_json_into_entities() {
        let backend: Arc<dyn LlmBackend> = Arc::new(FauxBackend::new().with_response(
            r#"{
                "entities":[
                    {"name":"Alice","entity_type":"Person","confidence":0.9,
                     "valid_from_iso":"2025-01-01"}
                ],
                "relations":[]
            }"#,
        ));
        let extractor = LlmExtractor::new(backend);
        let c = chunk("Alice met Bob in Paris.");
        let out = extractor.extract(Ulid::new(), &[c]).await.unwrap();
        assert_eq!(out.by_chunk.len(), 1);
        assert_eq!(out.by_chunk[0].entities.len(), 1);
        assert_eq!(out.by_chunk[0].entities[0].name, "Alice");
    }

    #[tokio::test]
    async fn malformed_json_emits_empty_extraction() {
        let backend: Arc<dyn LlmBackend> =
            Arc::new(FauxBackend::new().with_response("not json at all"));
        let extractor = LlmExtractor::new(backend);
        let c = chunk("hello");
        let out = extractor.extract(Ulid::new(), &[c]).await.unwrap();
        assert_eq!(out.by_chunk.len(), 1);
        assert!(out.by_chunk[0].entities.is_empty());
        assert!(out.by_chunk[0].relations.is_empty());
    }

    #[tokio::test]
    async fn batch_timeout_emits_empty_per_chunk_partials() {
        // 500 ms delay exceeds the 50 ms batch timeout → timeout path fires.
        let backend: Arc<dyn LlmBackend> = Arc::new(FauxBackend::new().with_delay_ms(500));
        let extractor = LlmExtractor::with_opts(
            backend,
            LlmExtractorOpts {
                batch_timeout_ms: 50,
                per_chunk_timeout_ms: 500,
                max_tokens: 64,
                temperature: 0.0,
                gbnf: None,
            },
        );
        let chunks = vec![chunk("a"), chunk("b"), chunk("c")];
        let out = extractor.extract(Ulid::new(), &chunks).await.unwrap();
        // Timeout path emits one empty extraction per input chunk.
        assert_eq!(out.by_chunk.len(), 3);
        for r in &out.by_chunk {
            assert!(r.entities.is_empty());
        }
    }

    /// Pin that `LlmExtractorOpts::gbnf` is faithfully threaded through
    /// to the backend as `SchemaConstraint::Gbnf`.
    #[tokio::test]
    async fn gbnf_opt_threads_through_to_backend() {
        let cap = Arc::new(FauxBackend::new().with_model_id("faux://capturing"));
        let extractor = LlmExtractor::with_opts(
            cap.clone() as Arc<dyn LlmBackend>,
            LlmExtractorOpts { gbnf: Some("root ::= \"{}\""), ..LlmExtractorOpts::default() },
        );
        let _ = extractor.extract(Ulid::new(), &[chunk("hi")]).await.unwrap();
        assert_eq!(cap.last_constraint_tag(), Some("gbnf"));
    }

    #[tokio::test]
    async fn no_gbnf_opt_sends_constraint_none() {
        let cap = Arc::new(FauxBackend::new());
        let extractor = LlmExtractor::new(cap.clone() as Arc<dyn LlmBackend>);
        let _ = extractor.extract(Ulid::new(), &[chunk("hi")]).await.unwrap();
        assert_eq!(cap.last_constraint_tag(), Some("none"));
    }

    #[test]
    fn parses_balanced_json_with_trailing_garbage() {
        let chunk_id = Ulid::new();
        let raw = parse_extraction_json(
            r#"junk before {"entities":[],"relations":[]} junk after"#,
            chunk_id,
        );
        assert_eq!(raw.source_chunk_id, chunk_id);
        assert!(raw.entities.is_empty());
    }
}
