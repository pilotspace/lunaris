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
//! The pre-existing `crate::CandleGemma3_4B` / [`crate::OllamaExtractor`]
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
use serde::Deserialize;
use ulid::Ulid;

use crate::Extractor;
use crate::types::{
    ChunkInput, Entity, EntityId, Fact, RawExtraction, RawExtractionBatch, Relation,
};

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

/// `pub(crate)` so `cloud_api.rs` shares this exact prompt instead of
/// maintaining its own copy -- it previously had an independent, equally
/// vague local prompt (same missing-field-names bug, found+fixed
/// separately during the same review that produced this shared version).
pub(crate) fn build_prompt(chunk: &ChunkInput) -> String {
    // T-03-01-01 mitigation: wrap chunk text in `<chunk>` delimiters so
    // the downstream validator can flag any extracted entity whose name
    // equals the literal delimiter (prompt-injection guard).
    //
    // The explicit field-name shape below matters for backends that reach
    // this function WITHOUT a GBNF grammar to fall back on. Candle gets the
    // schema enforced via SchemaConstraint::Gbnf (lunaris-llm's candle.rs
    // appends the grammar text to the prompt); Ollama drops GBNF entirely
    // (OllamaExtractor::new() sets gbnf: None) so this prompt text is the
    // ONLY schema guidance an Ollama/cloud-routed model ever sees. A vague
    // "respond with {entities:[...],relations:[...]}" placeholder (the
    // prior text) let a live model guess plausible-but-wrong field names,
    // failing parse_extraction_json's required-field check on every chunk
    // (confirmed against MiniMax-M3 via the LongMemEval graph-pipeline
    // prototype, 2026-07 -- 100% empty-extraction fallback).
    // Temporal grounding (Mechanism B, 2026-07-29 LME diagnosis + SOTA
    // comparison tmp/sota_extractor_comparison.md §3, Graphiti REFERENCE_TIME
    // mechanics): the prior wording "(from the chunk's context, else today)"
    // MANDATED hallucinated dates — 78% of a 4,882-item cache audit carried
    // the model's own "today" (2025/2026) against 2022-2023 source text — and
    // the few-shot example hardcoded "2025-01-01" twice, anchoring even
    // models that would otherwise abstain. Now: inject the episode's real
    // date when known, resolve relative expressions against it, and require
    // null over guessing. The example demonstrates one resolved date + one
    // null (never a fixed modern date).
    let reference_block = match chunk.reference_time_iso.as_deref() {
        Some(d) => format!(
            "REFERENCE_TIME: {d} (the date the conversation/document in \
             <chunk> is from)\n\
             - Resolve relative time expressions against REFERENCE_TIME: \
             \"yesterday\" = REFERENCE_TIME minus 1 day; \"last week\" = \
             about 7 days before; \"two years ago\" = 2 years before; \
             \"today\" / \"this morning\" / \"just now\" = REFERENCE_TIME.\n\
             - A fact stated in the present tense with no other date (\"I \
             work at Acme\") is known true as of this conversation: set \
             valid_from_iso to REFERENCE_TIME.\n\
             - Never output a date later than REFERENCE_TIME unless the \
             text explicitly states a future plan.\n"
        ),
        None => String::new(),
    };
    format!(
        "Extract entities and relations from the chunk below as JSON.\n\n\
         Respond with a JSON object of EXACTLY this shape (all fields \
         required except aliases and valid_to_iso):\n\
         {{\"entities\":[{{\"name\":\"Alice\",\"entity_type\":\"Person\",\
         \"aliases\":[],\"confidence\":0.9,\"valid_from_iso\":\"2023-05-14\",\
         \"valid_to_iso\":null}}],\n\
         \"relations\":[{{\"subject_name\":\"Alice\",\"subject_type\":\"Person\",\
         \"predicate\":\"met\",\"object_name\":\"Bob\",\"object_type\":\"Person\",\
         \"confidence\":0.9,\"valid_from_iso\":null,\"valid_to_iso\":null}}]}}\n\
         Use no other field names.\n\
         {reference_block}\
         Date rules for valid_from_iso / valid_to_iso (ISO 8601 dates, e.g. \
         2023-05-14):\n\
         - If the text states an explicit date (or one resolvable from the \
         rules above) for when the fact became true, use it. Month and year \
         only: use the 1st of that month. Year only: use January 1st.\n\
         - If a fact's start date is genuinely unknown, set valid_from_iso \
         to null. NEVER invent a date and NEVER infer temporal bounds from \
         unrelated events.\n\
         - Set valid_to_iso ONLY when the text says the fact ended, changed, \
         or was replaced (\"no longer\", \"used to\", \"switched from X to \
         Y\", \"sold my\"); otherwise null.\n\
         If nothing is extractable, return \
         {{\"entities\":[],\"relations\":[]}}.\n\n\
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

/// Deserialize a JSON array element-by-element, silently dropping any element
/// that fails to deserialize instead of failing the whole array. Cloud
/// extractors (MiniMax-M3, LongMemEval graph run 2026-07) intermittently emit
/// one malformed entity/relation per chunk; without this, a single bad element
/// dropped EVERY good sibling in the same chunk (q1 lost 10 chunks' worth of
/// extractions this way, silently starving the graph). Design-for-failure: a
/// bad element degrades to its own omission, never to a whole-chunk loss.
fn lenient_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
    let total = raw.len();
    let kept: Vec<T> = raw.into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect();
    if kept.len() < total {
        tracing::debug!(
            dropped = total - kept.len(),
            kept = kept.len(),
            "lenient_vec: skipped malformed extraction elements"
        );
    }
    Ok(kept)
}

/// Accept a JSON string, `null`, or an absent field, mapping the empty cases
/// to `""`. Cloud models routinely null out a `*_type` they cannot classify or
/// a `valid_from_iso` they cannot date; dropping the whole element over an
/// un-inferred type loses a real, otherwise-usable fact.
fn string_or_null<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

/// Neutral confidence for elements whose `confidence` the model omitted or
/// nulled — keeps the element rather than dropping it over a missing score.
fn mid_confidence() -> f32 {
    0.5
}

#[derive(Debug, serde::Deserialize)]
struct ExtractionJson {
    #[serde(default, deserialize_with = "lenient_vec")]
    entities: Vec<EntityJson>,
    #[serde(default, deserialize_with = "lenient_vec")]
    relations: Vec<RelationJson>,
}

#[derive(Debug, serde::Deserialize)]
struct EntityJson {
    name: String,
    #[serde(default, deserialize_with = "string_or_null")]
    entity_type: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default = "mid_confidence")]
    confidence: f32,
    #[serde(default, deserialize_with = "string_or_null")]
    valid_from_iso: String,
    #[serde(default)]
    valid_to_iso: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct RelationJson {
    subject_name: String,
    #[serde(default, deserialize_with = "string_or_null")]
    subject_type: String,
    predicate: String,
    object_name: String,
    #[serde(default, deserialize_with = "string_or_null")]
    object_type: String,
    #[serde(default = "mid_confidence")]
    confidence: f32,
    #[serde(default, deserialize_with = "string_or_null")]
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
        // Synthesize one Fact per relation: the SAME S-P-O triple the graph
        // edge carries, plus a readable `fact_text` claim sentence. The graph
        // extractor never populated the `Fact` primitive, so the `fact:` KV
        // keyspace and every fact-text consumer was starved. `fact_text` is a
        // "deduped-but-unsummed" claim (subject + humanized predicate +
        // object) — exactly the shape the LME reader-context presentation
        // hypothesis needs. Built from `&self.relations` BEFORE the
        // `into_iter` below consumes it. Relations with a blank endpoint are
        // skipped (the validator would reject the fact on empty fact_text
        // anyway); every kept fact inherits the relation's confidence +
        // bitemporal window so it passes the same validation gate.
        let facts = self
            .relations
            .iter()
            .filter(|r| !r.subject_name.trim().is_empty() && !r.object_name.trim().is_empty())
            .map(|r| Fact {
                id: Ulid::new(),
                subject_id: EntityId::from_name_and_type(&r.subject_name, &r.subject_type),
                predicate: r.predicate.clone(),
                object_id: EntityId::from_name_and_type(&r.object_name, &r.object_type),
                fact_text: synth_fact_text(&r.subject_name, &r.predicate, &r.object_name),
                confidence: r.confidence,
                valid_from_iso: r.valid_from_iso.clone(),
                valid_to_iso: r.valid_to_iso.clone(),
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
        RawExtraction { source_chunk_id: chunk_id, entities, relations, facts }
    }
}

/// Render an S-P-O triple as a readable claim sentence for `Fact::fact_text`.
/// The predicate is humanized (SCREAMING_SNAKE / snake_case → lower-cased
/// words), so `("Alice", "SHOPS_AT", "Store")` becomes `"Alice shops at Store"`.
fn synth_fact_text(subject: &str, predicate: &str, object: &str) -> String {
    let pred = predicate.trim().replace('_', " ").to_lowercase();
    format!("{} {} {}", subject.trim(), pred, object.trim())
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
            reference_time_iso: None,
        }
    }

    #[test]
    fn build_prompt_includes_every_required_json_field_name() {
        // Candle gets the field-name schema via the in-prompt GBNF grammar
        // (lunaris-llm/src/candle.rs's SchemaConstraint::Gbnf branch).
        // Ollama drops GBNF entirely (lunaris-llm/src/ollama.rs: both
        // SchemaConstraint::None and ::Gbnf map to no `format` field) and
        // OllamaExtractor::new() sets gbnf: None anyway -- so build_prompt's
        // own text is the ONLY schema guidance an Ollama/cloud-routed model
        // ever sees. Confirmed live against MiniMax-M3 (LongMemEval
        // graph-pipeline prototype, 2026-07): without explicit field names
        // every single chunk produced plausible-but-wrong JSON that failed
        // parse_extraction_json's required-field check (EntityJson /
        // RelationJson have no #[serde(default)] on entity_type/confidence/
        // valid_from_iso/subject_name/subject_type/predicate/object_name/
        // object_type), silently degrading to 100% empty extractions.
        let p = build_prompt(&chunk("Alice met Bob in Paris."));
        for field in [
            "entity_type",
            "confidence",
            "valid_from_iso",
            "subject_name",
            "subject_type",
            "predicate",
            "object_name",
            "object_type",
        ] {
            assert!(p.contains(field), "prompt missing required field name: {field}");
        }
    }

    #[tokio::test]
    async fn empty_chunks_returns_empty_batch() {
        let backend: Arc<dyn LlmBackend> = Arc::new(FauxBackend::new());
        let extractor = LlmExtractor::new(backend);
        let out = extractor.extract(Ulid::new(), &[]).await.unwrap();
        assert!(out.by_chunk.is_empty());
    }

    #[test]
    fn synthesizes_a_readable_fact_per_relation() {
        // The graph extractor (minimax cloud + ollama) only ever emitted
        // entities + relations; the `facts` array was hard-coded empty, so the
        // `fact:` KV keyspace stayed empty and every fact-text consumer
        // (retrieval snippets, the LongMemEval reader context) had nothing to
        // read — verified live 2026-07-21: a graph-ON haystack produced 161
        // graph nodes / 75 edges but 0 facts. Each validated relation must now
        // yield ONE Fact carrying the S-P-O triple AND a readable fact_text.
        let json = r#"{
            "entities":[
                {"name":"Alice","entity_type":"Person","confidence":0.9,"valid_from_iso":"2025-01-01"},
                {"name":"Racket","entity_type":"Product","confidence":0.9,"valid_from_iso":"2025-01-01"}
            ],
            "relations":[
                {"subject_name":"Alice","subject_type":"Person","predicate":"SHOPS_AT",
                 "object_name":"Store","object_type":"Org","confidence":0.8,
                 "valid_from_iso":"2025-01-01"}
            ]
        }"#;
        let raw = parse_extraction_json(json, Ulid::new());
        assert_eq!(raw.relations.len(), 1);
        assert_eq!(raw.facts.len(), 1, "exactly one fact synthesized per relation");
        let f = &raw.facts[0];
        assert_eq!(f.fact_text, "Alice shops at Store", "readable S-P-O claim sentence");
        assert_eq!(f.predicate, "SHOPS_AT", "structured predicate preserved verbatim");
        assert_eq!(f.subject_id, EntityId::from_name_and_type("Alice", "Person"));
        assert_eq!(f.object_id, EntityId::from_name_and_type("Store", "Org"));
        assert_eq!(f.confidence, 0.8, "confidence inherited from the relation");
        assert_eq!(f.valid_from_iso, "2025-01-01");
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
    async fn one_malformed_element_does_not_drop_the_whole_chunk() {
        // Real MiniMax-M3 failure modes observed in the LongMemEval graph
        // run (2026-07): a relation MISSING `subject_type`, a `null` where a
        // type/date string is expected, and an irrecoverable stub object.
        // Pre-fix, `serde_json::from_str::<ExtractionJson>` failed on the
        // WHOLE document and dropped every good sibling too — q1 alone lost
        // 10 chunks' worth of extractions this way, silently starving the
        // graph. Each recoverable element must now survive; only the
        // genuinely-unusable one is skipped.
        let backend: Arc<dyn LlmBackend> = Arc::new(FauxBackend::new().with_response(
            r#"{
                "entities":[
                    {"name":"Alice","entity_type":"Person","confidence":0.9,"valid_from_iso":"2025-01-01"},
                    {"name":"Bob","entity_type":null,"confidence":0.8,"valid_from_iso":null}
                ],
                "relations":[
                    {"subject_name":"Alice","predicate":"met","object_name":"Bob","object_type":"Person","confidence":0.9,"valid_from_iso":"2025-01-01"},
                    {"subject_name":"Alice","subject_type":"Person","predicate":"visited","object_name":"Paris","object_type":"City","confidence":0.7,"valid_from_iso":"2025-01-02"},
                    {"predicate":"orphan-no-subject-or-object"}
                ]
            }"#,
        ));
        let extractor = LlmExtractor::new(backend);
        let c = chunk("Alice met Bob in Paris.");
        let out = extractor.extract(Ulid::new(), &[c]).await.unwrap();
        assert_eq!(out.by_chunk.len(), 1);
        // Both entities survive — Bob's null entity_type/date are tolerated.
        assert_eq!(out.by_chunk[0].entities.len(), 2, "both entities must survive");
        // Two relations survive — the first (missing subject_type) is
        // recovered via default; the orphan (no subject/object name) is the
        // only element dropped.
        assert_eq!(
            out.by_chunk[0].relations.len(),
            2,
            "recoverable relations must survive; only the orphan drops"
        );
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

    // ── Session-date grounding (Mechanism B, N=125 A/B diagnosis 2026-07-29) ──
    //
    // Cache-entry audit: 78% of extracted valid_from dates were hallucinated
    // (3,359/4,882 stamped 2025 + 443 stamped 2026 against 2022-2023
    // haystacks). Two prompt defects MANDATE that outcome: the instruction
    // "else today" tells the model to stamp its own today, and the few-shot
    // example hardcodes "2025-01-01" twice, anchoring even models that would
    // otherwise abstain. Graphiti-style fix (REFERENCE_TIME injection +
    // null-over-guess): see tmp/sota_extractor_comparison.md §3.

    fn dated_chunk(text: &str, reference: &str) -> ChunkInput {
        ChunkInput {
            chunk_id: Ulid::new(),
            heading_path: vec!["section".into()],
            text: text.into(),
            reference_time_iso: Some(reference.into()),
        }
    }

    #[test]
    fn build_prompt_renders_reference_time_and_temporal_rules() {
        let p = build_prompt(&dated_chunk("I met Bob yesterday.", "2023-05-30"));
        assert!(
            p.contains("REFERENCE_TIME: 2023-05-30"),
            "prompt must inject the session date as REFERENCE_TIME"
        );
        assert!(
            p.contains("relative time expressions"),
            "prompt must instruct resolving relative dates against REFERENCE_TIME"
        );
        assert!(p.contains("NEVER invent a date"), "prompt must carry the null-over-guess rule");
        assert!(!p.contains("else today"), "the hallucination mandate must be gone");
        assert!(
            !p.contains("2025-01-01"),
            "few-shot example dates must not anchor the model to 2025"
        );
    }

    #[test]
    fn build_prompt_without_reference_time_uses_null_policy() {
        let p = build_prompt(&chunk("Alice met Bob in Paris."));
        assert!(
            !p.contains("REFERENCE_TIME:"),
            "no reference time available -> no REFERENCE_TIME line"
        );
        assert!(
            p.contains("NEVER invent a date"),
            "null-over-guess must hold even without a reference time"
        );
        assert!(!p.contains("else today"));
        assert!(!p.contains("2025-01-01"));
    }
}
