//! Deterministic 1M-fact synthetic corpus generator + on-disk fingerprint cache.
//!
//! ## Why not seed via `Lunaris::ingest`?
//!
//! Naive sequential `ingest(Episode)` of 1M Episodes at the budgeted ~50 ms p50
//! would burn ~14 hours per bench run. Per the plan's `<critical_constraints>`
//! we bypass the public ingest API and emit `WriteOp::KvPut` + `WriteOp::VectorUpsert`
//! batches directly via `StoragePort::atomic_write` in chunks of 10 000 ops. Each
//! atomic_write is bounded by the backend's batch limit (Moon TXN.COMMIT, Postgres
//! single COMMIT) so a power loss mid-build leaves a partially-written corpus +
//! NO fingerprint file — the next run rebuilds from scratch (T-02-04-02 mitigation).
//!
//! ## Determinism contract
//!
//! - Vocab + predicate set are static constants below (10 K entity names,
//!   32 predicates).
//! - Per-fact bytes (id ULID, subject/object indices, predicate, embedding) all
//!   derive from a `rand_chacha::ChaCha20Rng` seeded by the caller. Same seed +
//!   same `count` → byte-identical corpus across runs / machines.
//! - Embeddings use a `det_vec(text, dim)` helper that mirrors
//!   `lunaris_core::embedder::StubEmbedder`'s LCG-via-DefaultHasher algorithm
//!   so a `StubEmbedder`-driven recall query produces identical vectors to the
//!   stored fact embeddings (lets the bench measure the storage hot path
//!   independent of the real EmbeddingGemma cost).
//!
//! ## Fingerprint cache
//!
//! After a successful build, the generator writes a small JSON fingerprint to
//! `<target>/lunaris-bench/corpus-1M-<urlhash>.json`:
//!
//! ```json
//! { "backend_url_hash": "abcdef1234567890", "fact_count": 1000000, "seed": 42, "completed": true, "duration_secs": 423.7 }
//! ```
//!
//! On the next bench invocation, the generator hashes the URL, reads the
//! fingerprint, and returns `Ok(())` immediately if `completed && fact_count
//! && seed` all match. There is no schema migration handling — bumping the
//! corpus shape requires deleting the fingerprint file (which is what we want).
//!
//! ## Threat model snapshot
//!
//! - **T-02-04-01** (info disclosure via URL in logs): the URL itself is hashed
//!   to 16 hex chars before any disk write or log line — credentials in a
//!   `postgres://user:pass@host` URL never reach `target/`.
//! - **T-02-04-02** (corrupt fingerprint after interrupt): `completed: true`
//!   is only written AFTER the final batch lands. Interrupted runs leave no
//!   fingerprint → next run rebuilds.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lunaris::Lunaris;
use lunaris_core::{
    BiTemporal, Fact, HlcClock, LunarisError, Scope, StorageError, StoragePort, WriteOp,
};
use lunaris_extract::EntityId;
use lunaris_storage_moon::keyspace::fact_key;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha20Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use ulid::Ulid;

// ----- public constants -----

/// Default fact count for the recall bench corpus. Plan 02-04 spec.
pub const DEFAULT_CORPUS_COUNT: u64 = 1_000_000;

/// Default RNG seed for the corpus generator.
pub const DEFAULT_CORPUS_SEED: u64 = 42;

/// Default RNG seed for the synthetic query set. Distinct from the corpus seed
/// so query bytes don't accidentally match a fact's `fact_text` verbatim
/// (which would let the bench cheat with an exact-match short-circuit).
pub const DEFAULT_QUERY_SEED: u64 = 99;

/// Number of WriteOps per `atomic_write` batch during corpus build. Each fact
/// produces 2 WriteOps (KvPut + VectorUpsert) so this is ~5000 facts/batch.
pub const CORPUS_BATCH_OPS: usize = 10_000;

/// Embedding dimensionality — matches Moon profile (`max_vector_dim = 768`)
/// AND `lunaris_core::StubEmbedder::new(768)` so a `StubEmbedder`-driven recall
/// query produces vectors compatible with the corpus embeddings.
pub const CORPUS_EMBEDDING_DIM: usize = 768;

/// Predicate vocab for the synthetic graph-on relations (Plan 03-04).
///
/// Mirror of the relation predicates the real Gemma-3 4B extractor would emit
/// — kept stable and small (5 entries) so a deterministic round-robin pairing
/// produces the same edges across runs.
const SYNTHETIC_GRAPH_PREDICATES: &[&str] =
    &["mentions", "related_to", "depends_on", "succeeds", "co_occurs"];

// ----- graph density (Plan 03-04 D-04 / I-3 / I-4) -----

/// Per-Episode entity + relation density to seed when generating a graph-on
/// corpus (Plan 03-04 D-04).
///
/// When `CorpusGenOpts.graph_density = Some(...)`, the bulk-write path emits
/// `WriteOp::GraphNode` per entity + `WriteOp::GraphEdge` per relation
/// alongside the per-chunk `KvPut` + `VectorUpsert`.
///
/// **Default density (D-04):** 10 entities/chunk × 3 relations/entity = 30
/// relations/chunk — matches the typical 12 KB markdown extraction ratio so
/// the graph-on bench measures realistic ingest fan-out cost rather than a
/// 4-entity straw man (I-3 fix). Bench harnesses MUST use [`Default`] unless
/// they have a specific reason to deviate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GraphDensity {
    /// Number of [`WriteOp::GraphNode`] writes seeded per chunk.
    pub entities_per_chunk: u32,
    /// Number of [`WriteOp::GraphEdge`] writes seeded per entity (so total
    /// edges per chunk = `entities_per_chunk * relations_per_entity`).
    pub relations_per_entity: u32,
}

impl Default for GraphDensity {
    fn default() -> Self {
        Self { entities_per_chunk: 10, relations_per_entity: 3 }
    }
}

/// Knobs the corpus generator accepts (Plan 03-04 additive extension).
///
/// `graph_density: None` (default) reproduces the Plan 02-04 no-graph corpus
/// shape exactly — every existing recall-bench caller that takes positional
/// `(count, seed)` is unchanged. `graph_density: Some(...)` enables the
/// graph-on bulk-write path which emits additional `GraphNode` + `GraphEdge`
/// `WriteOp`s alongside the per-chunk KvPut + VectorUpsert.
///
/// Determinism contract: same `(count, seed, graph_density)` triple → byte-
/// identical corpus across runs / machines / OSes.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CorpusGenOpts {
    pub fact_count: u64,
    pub seed: u64,
    /// When `Some(_)`, switches to the graph-on bulk-write path (D-04).
    /// When `None`, behaves identically to the Plan 02-04 no-graph corpus.
    #[serde(default)]
    pub graph_density: Option<GraphDensity>,
}

impl Default for CorpusGenOpts {
    fn default() -> Self {
        Self { fact_count: DEFAULT_CORPUS_COUNT, seed: DEFAULT_CORPUS_SEED, graph_density: None }
    }
}

// ----- error -----

#[derive(Debug, Error)]
pub enum BenchCorpusError {
    #[error("backend error: {0}")]
    Backend(#[from] LunarisError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("invalid corpus parameters: {0}")]
    Invalid(String),
}

// ----- fingerprint -----

/// On-disk record proving a successful corpus build.
///
/// Written ONLY after the final atomic_write batch lands. Re-reads gate the
/// rebuild decision. The URL is stored as a 16-char hex SHA-256 prefix, NOT
/// the raw URL — credentials in a `postgres://user:pass@host` URL never hit
/// disk (T-02-04-01 mitigation).
///
/// **Plan 03-04 extension:** `graph_density` is `Some(_)` when this
/// fingerprint records a graph-on corpus (T-03-04-04 mitigation — switching
/// density yields a distinct fingerprint, forcing a rebuild). `None`
/// preserves the Plan 02-04 no-graph fingerprint shape (default-deserialised
/// for backward-compat with on-disk fingerprints written by Plan 02-04).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CorpusFingerprint {
    /// 16-char SHA-256 prefix of the backend URL (hex). See [`hash_url`].
    pub backend_url_hash: String,
    /// Number of facts the generator was asked to emit.
    pub fact_count: u64,
    /// Seed the RNG was initialised with.
    pub seed: u64,
    /// `true` only after the final batch landed successfully. A `false` here
    /// (or absent file) tells the next run to rebuild.
    pub completed: bool,
    /// Wall-clock duration of the successful build, in seconds.
    pub duration_secs: f64,
    /// Plan 03-04: density that this corpus was built under. `None` means
    /// "no-graph corpus" (Plan 02-04 default shape). `Some(_)` means
    /// "graph-on corpus with this density" — switching density invalidates
    /// the cache (T-03-04-04 mitigation).
    #[serde(default)]
    pub graph_density: Option<GraphDensity>,
    /// Row-shape schema version. Bumped whenever the WriteOp metadata
    /// emitted per fact / chunk / entity changes shape (e.g. Gap 9 fix
    /// 2026-04-21 added `fact_text` to facts metadata). Old fingerprints
    /// deserialise to `0` via `#[serde(default)]`, so a version bump
    /// invalidates the cache automatically without operator intervention.
    /// See [`CURRENT_CORPUS_SCHEMA_VERSION`].
    #[serde(default)]
    pub schema_version: u32,
}

/// Current row-shape version emitted by [`build_corpus_with_options`]. Bump
/// alongside any metadata-shape change so existing fingerprints invalidate.
///
/// History:
/// - `0` — pre-Gap-9 (facts metadata = `{predicate, subject}` only)
/// - `1` — Gap 9 fix 2026-04-21: facts metadata adds `fact_text`. **First
///   try emitted `format!("{} {}", subject_ulid, predicate)` — recall probe
///   returned 0 hits because `synthetic_query_set` draws from the
///   `entity_name(idx)` vocab and ULIDs never matched.**
/// - `2` — Gap 9 follow-up 2026-04-21: facts metadata `fact_text` now uses
///   `fact.fact_text` (the `entity_name(sub) predicate entity_name(obj)`
///   string built by `next_fact`) so BM25 / HYBRID hits land.
pub const CURRENT_CORPUS_SCHEMA_VERSION: u32 = 2;

// ----- url hash helpers -----

/// Hash a backend URL to a 16-char hex SHA-256 prefix. Stable across runs +
/// machines + OSes. Used to derive the fingerprint file path AND to stamp
/// the fingerprint without leaking credentials.
pub fn hash_url(url: &str) -> String {
    let digest = Sha256::digest(url.as_bytes());
    let full = hex::encode(digest);
    full[..16].to_string()
}

/// Canonical fingerprint file location for a given backend URL + count
/// (no-graph variant — Plan 02-04 shape, kept for compat).
///
/// Reads `CARGO_TARGET_DIR` if set, else uses `target/` relative to the current
/// working directory (which Cargo guarantees is the workspace root for `cargo
/// bench` / `cargo test` invocations). The directory is created lazily by
/// [`build_one_million_fact_corpus`] before the file is written.
pub fn fingerprint_path_for(url: &str, count: u64) -> PathBuf {
    fingerprint_path(url, count, None)
}

/// Plan 03-04 — density-aware fingerprint path.
///
/// When `graph_density` is `None` the path is identical to
/// [`fingerprint_path_for`] (Plan 02-04 shape preserved verbatim). When
/// `graph_density` is `Some(_)` the filename gains a `-graph-on` suffix so a
/// no-graph corpus AND a graph-on corpus can coexist on disk for the same
/// backend URL:
///
/// ```text
/// target/lunaris-bench/corpus-1000000-<urlhash>.json             // no-graph
/// target/lunaris-bench/corpus-1000000-<urlhash>-graph-on.json    // graph-on
/// ```
///
/// The fingerprint JSON content also encodes the density (see
/// [`CorpusFingerprint::graph_density`]) so a future cache invalidation when
/// `entities_per_chunk` changes is automatic — distinct density → distinct
/// fingerprint content → cache miss → rebuild (T-03-04-04 mitigation).
pub fn fingerprint_path(url: &str, count: u64, graph_density: Option<GraphDensity>) -> PathBuf {
    let target = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
    let mut p = PathBuf::from(target);
    p.push("lunaris-bench");
    let suffix = if graph_density.is_some() { "-graph-on" } else { "" };
    p.push(format!("corpus-{}-{}{}.json", count, hash_url(url), suffix));
    p
}

// ----- TCP reachability probe -----

/// Returns `true` if a 1-second TCP connect to the URL's `host:port` succeeds.
///
/// Used by [`ensure_corpus_or_skip`] AND by every Criterion bench to decide
/// whether to skip the live-backend bench group when the substrate is
/// unreachable. URL parsing is intentionally permissive: `moon://host:port`,
/// `postgres://user:pass@host:port/db`, and bare `host:port` all work.
///
/// Returns `false` on any parse error, DNS failure, or TCP timeout — never
/// panics, so callers can use it as a binary skip signal.
pub async fn tcp_reachable(url: &str) -> bool {
    let host_port = match parse_host_port(url) {
        Some(hp) => hp,
        None => return false,
    };
    matches!(
        tokio::time::timeout(Duration::from_secs(1), tokio::net::TcpStream::connect(&host_port))
            .await,
        Ok(Ok(_))
    )
}

/// Extract `host:port` from a URL or bare authority. Returns `None` on parse
/// failure or missing port. We accept the absence of a default-port fallback
/// because skipping a bench is safer than connecting to the wrong port.
fn parse_host_port(url: &str) -> Option<String> {
    // Try a real URL parse first — handles `moon://host:port`,
    // `postgres://user:pass@host:port/db`, etc. Only succeeds when BOTH host
    // and port are present (some URL grammars accept a missing port and
    // we'd rather skip than guess).
    if let Ok(parsed) = ::url::Url::parse(url)
        && let (Some(host), Some(port)) = (parsed.host_str(), parsed.port())
    {
        return Some(format!("{host}:{port}"));
    }
    // Fall back to bare `host:port` (no scheme). Accepts exactly one `:` so
    // `host:port:extra` doesn't get silently truncated.
    let parts: Vec<&str> = url.split(':').collect();
    if parts.len() == 2
        && !parts[0].is_empty()
        && !parts[1].is_empty()
        && parts[1].chars().all(|c| c.is_ascii_digit())
    {
        return Some(url.to_string());
    }
    None
}

// ----- vocab + predicates (static, deterministic) -----

const PREDICATES: &[&str] = &[
    "worked_at",
    "lives_in",
    "knows",
    "owns",
    "founded",
    "manages",
    "wrote",
    "translated",
    "designed",
    "built",
    "discovered",
    "invented",
    "led",
    "joined",
    "left",
    "married",
    "studied",
    "taught",
    "advised",
    "mentored",
    "competed_with",
    "collaborated_with",
    "hired",
    "fired",
    "promoted",
    "edited",
    "directed",
    "produced",
    "starred_in",
    "composed",
    "sang",
    "performed",
];

const ENTITY_VOCAB_SIZE: usize = 10_000;

/// Generate a deterministic 10 000-entry entity-name vocabulary.
///
/// We don't ship 10 K English-word fixtures (would bloat the binary). Instead
/// we synthesize names of the shape `"Subject0001"` … `"Subject9999"`. The
/// names are stable across runs because the index space is `[0, 10 000)`.
fn entity_name(idx: usize) -> String {
    format!("Subject{idx:04}")
}

// ----- deterministic embedding helper -----

/// Mirror of `lunaris_core::embedder::StubEmbedder::embed_batch` per-string
/// algorithm. Reproduced here so the corpus generator does NOT take a dep on
/// the StubEmbedder (which would force `lunaris-core::test-util` into the
/// production dep tree). The two impls MUST stay in sync — there's a unit
/// test below that hashes the output of both for a known input.
fn det_vec(s: &str, dim: usize) -> Vec<f32> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    let mut state = h.finish().max(1);
    (0..dim)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let v = ((state >> 33) as u32) as f32 / u32::MAX as f32;
            v * 2.0 - 1.0
        })
        .collect()
}

// ----- corpus build -----

/// Build a `count`-fact synthetic corpus on `handle`'s storage backend.
///
/// Skips immediately (returns `Ok(())`) if a matching fingerprint file is
/// found at [`fingerprint_path_for`]. Otherwise generates `count` facts with
/// `seed` and bulk-writes via `handle.storage().atomic_write` in batches of
/// [`CORPUS_BATCH_OPS`] / 2 facts.
///
/// ## Errors
///
/// - [`BenchCorpusError::Invalid`] — `count == 0` OR `count > 100_000_000`
///   (a soft DoS guard so a malformed bench arg can't OOM the dev box).
/// - [`BenchCorpusError::Backend`] — the backend rejected an `atomic_write`.
///   The fingerprint file is NOT written; the next run rebuilds.
/// - [`BenchCorpusError::Io`] / [`BenchCorpusError::Serde`] — fingerprint
///   directory or file IO failure. The corpus IS in storage but the cache
///   miss means subsequent runs re-write the same bytes (wasteful but
///   correct).
pub async fn build_one_million_fact_corpus(
    handle: &Lunaris,
    count: u64,
    seed: u64,
    backend_url: &str,
) -> Result<(), BenchCorpusError> {
    // Plan 03-04: thin compat wrapper — preserves the Plan 02-04 positional
    // surface verbatim (no-graph corpus). Callers wanting the graph-on
    // variant invoke [`build_corpus_with_options`] with
    // `graph_density = Some(GraphDensity::default())`.
    build_corpus_with_options(
        handle,
        CorpusGenOpts { fact_count: count, seed, graph_density: None },
        backend_url,
    )
    .await
}

/// Plan 03-04 — Build a synthetic corpus on `handle`'s storage backend, with
/// optional graph density.
///
/// Behaves identically to [`build_one_million_fact_corpus`] when
/// `opts.graph_density.is_none()`. When `Some(density)`, ALSO emits per-chunk
/// `WriteOp::GraphNode` (one per entity) + `WriteOp::GraphEdge` (one per
/// relation) batches alongside the existing per-fact `KvPut` +
/// `VectorUpsert`. Total ops per "synthetic chunk" = 2 (fact KvPut +
/// VectorUpsert) + `entities_per_chunk` (GraphNode) +
/// `entities_per_chunk * relations_per_entity` (GraphEdge).
///
/// The fingerprint cache discriminator uses the `-graph-on` filename suffix
/// (T-03-04-04 mitigation) so a no-graph corpus and a graph-on corpus can
/// coexist on disk for the same backend URL. Switching `entities_per_chunk`
/// or `relations_per_entity` invalidates the cache (the fingerprint JSON
/// includes the density) — distinct density → distinct fingerprint content
/// → cache miss → rebuild.
pub async fn build_corpus_with_options(
    handle: &Lunaris,
    opts: CorpusGenOpts,
    backend_url: &str,
) -> Result<(), BenchCorpusError> {
    let count = opts.fact_count;
    let seed = opts.seed;
    let graph_density = opts.graph_density;

    if count == 0 || count > 100_000_000 {
        return Err(BenchCorpusError::Invalid(format!(
            "count must be in (0, 100_000_000]; got {count}"
        )));
    }

    let url_hash = hash_url(backend_url);
    let fp_path = fingerprint_path(backend_url, count, graph_density);

    // Cache hit → skip rebuild. Validates count + seed + completed flag +
    // graph_density so a partial run from a previous interrupted invocation
    // OR a density change re-runs (T-03-04-04 mitigation).
    if let Ok(bytes) = std::fs::read(&fp_path)
        && let Ok(fp) = serde_json::from_slice::<CorpusFingerprint>(&bytes)
        && fp.completed
        && fp.fact_count == count
        && fp.seed == seed
        && fp.backend_url_hash == url_hash
        && fp.graph_density == graph_density
        && fp.schema_version == CURRENT_CORPUS_SCHEMA_VERSION
    {
        tracing::info!(
            target: "lunaris-bench",
            count,
            seed,
            graph_on = graph_density.is_some(),
            duration_secs = fp.duration_secs,
            "corpus fingerprint hit; skipping rebuild"
        );
        return Ok(());
    }

    // Build from scratch.
    tracing::info!(
        target: "lunaris-bench",
        count,
        seed,
        graph_on = graph_density.is_some(),
        "building synthetic fact corpus"
    );
    let started = Instant::now();
    let storage: Arc<dyn StoragePort> = handle.storage();
    let clock = handle.clock();
    let mut rng = ChaCha20Rng::seed_from_u64(seed);

    // Per-fact ULIDs are derived from RNG bytes so they're deterministic for
    // a given seed. Tests below verify two builds with the same seed produce
    // identical first-fact ULIDs.
    let facts_per_batch = (CORPUS_BATCH_OPS / 2).max(1) as u64;
    let mut emitted: u64 = 0;
    let mut chunk_idx: u64 = 0;
    while emitted < count {
        let batch_size = std::cmp::min(facts_per_batch, count - emitted);
        // Capacity hint covers no-graph (2 ops/fact) AND graph-on (extra
        // entities + relations per fact; we treat each fact as one
        // "synthetic chunk" for density bookkeeping so the WriteOp count
        // mirrors the per-chunk extraction shape from a real ingest).
        let extra_per_fact = match graph_density {
            None => 0u64,
            Some(d) => {
                let e = d.entities_per_chunk as u64;
                e + e * d.relations_per_entity as u64
            }
        };
        let cap = (batch_size * (2 + extra_per_fact)) as usize;
        let mut ops: Vec<WriteOp> = Vec::with_capacity(cap);
        for _ in 0..batch_size {
            let fact = next_fact(&mut rng, &clock)?;
            let key = fact_key(&Scope::dev(), fact.id);
            let value = serde_json::to_vec(&fact)?;
            let embedding = fact.embedding.clone().unwrap_or_default();
            ops.push(WriteOp::KvPut { key, value });
            ops.push(WriteOp::VectorUpsert {
                index: "facts".to_string(),
                id: fact.id.to_bytes().to_vec(),
                embedding,
                // Gap 9 fix (2026-04-21): include `fact_text` (mirrors the
                // production `lunaris/src/ingest.rs::ingest` shape and the
                // Postgres tsvector source `payload->>'fact_text'`) so the
                // bench corpus is searchable by both backends' BM25/HYBRID
                // path. Without this the recall_q probe returns 0 hits and
                // SKIPs the bench.
                //
                // The text MUST be `fact.fact_text` (the
                // `entity_name(sub) predicate entity_name(obj)` form built
                // by `next_fact`), NOT a freshly-rebuilt `subject predicate`
                // string. `synthetic_query_set` draws from the same
                // `entity_name(idx)` vocab — using the wrong fact text
                // emits ULIDs that no query can match (Gap 9 follow-up
                // recall regression caught 2026-04-21).
                metadata: serde_json::json!({
                    "predicate": fact.predicate,
                    "subject": fact.subject.to_string(),
                    "fact_text": fact.fact_text.clone(),
                }),
            });
            // Plan 03-04 graph-on extension — emit deterministic synthetic
            // GraphNode + GraphEdge ops per "chunk" (per fact in this
            // bench-only context). EntityIds derive from
            // `(chunk_idx, entity_idx)` via blake3 (mirrors the Plan 03-01
            // EntityId::from_name_and_type path) so the corpus is
            // byte-identical across runs for the same seed.
            if let Some(density) = graph_density {
                let entities_per_chunk = density.entities_per_chunk as usize;
                let relations_per_entity = density.relations_per_entity as usize;

                // Pre-derive EntityIds so the relation pairing can index
                // back into the same set without recomputing.
                let entity_ids: Vec<EntityId> = (0..entities_per_chunk)
                    .map(|entity_idx| {
                        EntityId::from_name_and_type(
                            &format!("synthetic-entity-{chunk_idx}-{entity_idx}"),
                            "BenchEntity",
                        )
                    })
                    .collect();

                // GraphNode writes — props.id_hex matches Plan 03-02's
                // Cypher MATCH (n {id_hex: sid}) per W-7.
                for (entity_idx, eid) in entity_ids.iter().enumerate() {
                    let id_hex = format!("{eid}");
                    ops.push(WriteOp::GraphNode {
                        graph: "lunaris_graph".to_string(),
                        id: eid.0.to_vec(),
                        label: "BenchEntity".to_string(),
                        props: serde_json::json!({
                            "id_hex": id_hex,
                            "name": format!("entity-{chunk_idx}-{entity_idx}"),
                            "type": "BenchEntity",
                        }),
                    });
                }

                // GraphEdge writes — round-robin pairing wraps around the
                // entity list so every relation has both endpoints in the
                // same chunk. Predicate cycles through the fixed vocab to
                // keep the predicate distribution realistic.
                if entities_per_chunk > 0 {
                    for (subj_idx, subj_id) in entity_ids.iter().enumerate() {
                        for r in 0..relations_per_entity {
                            let obj_idx = (subj_idx + r + 1) % entities_per_chunk;
                            let obj_id = &entity_ids[obj_idx];
                            let predicate = SYNTHETIC_GRAPH_PREDICATES
                                [(subj_idx + r) % SYNTHETIC_GRAPH_PREDICATES.len()];
                            ops.push(WriteOp::GraphEdge {
                                graph: "lunaris_graph".to_string(),
                                src: subj_id.0.to_vec(),
                                dst: obj_id.0.to_vec(),
                                rel: predicate.to_string(),
                                props: serde_json::json!({
                                    "confidence": 0.9,
                                }),
                            });
                        }
                    }
                }
            }
            chunk_idx += 1;
        }
        storage
            .atomic_write(&lunaris_core::Scope::dev(), &ops)
            .await
            .map_err(|e| BenchCorpusError::Backend(LunarisError::Storage(e)))?;
        emitted += batch_size;
        if emitted.is_multiple_of(100_000) || emitted == count {
            tracing::debug!(
                target: "lunaris-bench",
                emitted,
                total = count,
                graph_on = graph_density.is_some(),
                "corpus build progress"
            );
        }
    }

    let duration = started.elapsed();
    let fp = CorpusFingerprint {
        backend_url_hash: url_hash,
        fact_count: count,
        seed,
        completed: true,
        duration_secs: duration.as_secs_f64(),
        graph_density,
        schema_version: CURRENT_CORPUS_SCHEMA_VERSION,
    };
    write_fingerprint(&fp_path, &fp)?;
    tracing::info!(
        target: "lunaris-bench",
        count,
        seed,
        graph_on = graph_density.is_some(),
        duration_secs = fp.duration_secs,
        "corpus build complete; fingerprint written"
    );
    Ok(())
}

/// Generate one synthetic [`Fact`] from the RNG. Pure — no IO.
fn next_fact(rng: &mut ChaCha20Rng, clock: &HlcClock) -> Result<Fact, BenchCorpusError> {
    // ULID from 16 random bytes — this matches `Ulid::from_bytes`.
    let mut bytes = [0u8; 16];
    rng.fill(&mut bytes);
    let id = Ulid::from_bytes(bytes);

    let mut sub_bytes = [0u8; 16];
    rng.fill(&mut sub_bytes);
    let mut obj_bytes = [0u8; 16];
    rng.fill(&mut obj_bytes);
    let subject = Ulid::from_bytes(sub_bytes);
    let object = Ulid::from_bytes(obj_bytes);

    let pred_idx = rng.random_range(0..PREDICATES.len());
    let predicate = PREDICATES[pred_idx];
    let sub_name_idx = (subject.0 as usize) % ENTITY_VOCAB_SIZE;
    let obj_name_idx = (object.0 as usize) % ENTITY_VOCAB_SIZE;
    let fact_text =
        format!("{} {} {}", entity_name(sub_name_idx), predicate, entity_name(obj_name_idx));
    let embedding = det_vec(&fact_text, CORPUS_EMBEDDING_DIM);
    Ok(Fact {
        id,
        scope: lunaris_core::Scope::dev(), // RFC 0001 Wave 0 migration crutch
        subject,
        predicate: predicate.to_string(),
        object,
        fact_text,
        embedding: Some(embedding),
        bt: BiTemporal::now(clock),
        confidence: 0.95,
        provenance: Vec::new(),
        activation: 0.5,
    })
}

fn write_fingerprint(path: &Path, fp: &CorpusFingerprint) -> Result<(), BenchCorpusError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(fp)?;
    std::fs::write(path, bytes)?;
    Ok(())
}

/// Plan 04-03 — chaos corpus.
///
/// Smaller than the 1M-fact bench corpus (10K facts by default) so the SIGKILL
/// window during `atomic_write` is observable within the property test's
/// `[1 ms, atomic_write_p99]` delay domain (D-23 / D-25). Reuses
/// [`build_corpus_with_options`] (no-graph variant — keeps the chaos signal
/// focused on the KV+vector atomic_write path).
///
/// Determinism contract: same `(count, seed, backend_url)` triple → byte-
/// identical corpus across runs (seeded `ChaCha20Rng` per
/// [`build_corpus_with_options`]).
///
/// Returns the number of facts emitted on success — used by the chaos
/// subprocess (`crates/lunaris-bench/src/bin/chaos.rs`) to log "wrote N
/// primitives" for observability.
pub async fn build_chaos_corpus(
    handle: &Lunaris,
    count: u64,
    seed: u64,
    backend_url: &str,
) -> Result<u64, BenchCorpusError> {
    build_corpus_with_options(
        handle,
        CorpusGenOpts { fact_count: count, seed, graph_density: None },
        backend_url,
    )
    .await?;
    Ok(count)
}

/// Wrap [`build_one_million_fact_corpus`] with a TCP-reachability pre-check.
///
/// Returns:
/// - `Ok(true)` — backend reachable AND corpus built (or cache-hit).
/// - `Ok(false)` — backend unreachable; corpus NOT built. Caller treats as
///   "skip the bench group". A short `eprintln!("SKIP …")` line is emitted
///   for the bench output so an operator running `cargo bench` sees why.
/// - `Err(_)` — backend reachable BUT the build itself failed. Caller
///   surfaces (corpus genuinely broken).
pub async fn ensure_corpus_or_skip(
    handle: &Lunaris,
    count: u64,
    seed: u64,
    backend_url: &str,
    label: &str,
) -> Result<bool, BenchCorpusError> {
    if !tcp_reachable(backend_url).await {
        eprintln!("SKIP {label} corpus build: backend unreachable (1s TCP probe failed)");
        return Ok(false);
    }
    build_one_million_fact_corpus(handle, count, seed, backend_url).await?;
    Ok(true)
}

// ----- markdown document corpus (Plan 05-04 HELIOS-02 / EVAL-06) -----

/// Plan 05-04 HELIOS-02 / EVAL-06 — bulk-write `count` synthetic ~12 KB
/// markdown documents directly to `storage`.
///
/// Same deterministic ChaCha20-seeded generator family as
/// [`build_chaos_corpus`] + [`build_one_million_fact_corpus`], but emits one
/// [`Episode`](lunaris_core::Episode) per document with a markdown body shaped like the
/// `lunaris-ingest` `twelve_kb_markdown_fixture` (heading + paragraphs
/// targeting ~12 KB).
///
/// Bypasses [`Lunaris::ingest`] for the same reason
/// [`build_one_million_fact_corpus`] does — sequential ingest of 50K episodes
/// at the budgeted ~50 ms p50 would burn ~40 minutes per smoke run, whereas
/// the bulk-write path lands the corpus in single-digit minutes (or seconds
/// when the count is small + the backend is fast). Each `atomic_write` batches
/// 64 episodes (~12 KB / episode → ~768 KB / batch — well under both Moon
/// `TXN.COMMIT` and Postgres single-COMMIT limits).
///
/// ## Determinism contract
///
/// Same `(count, seed)` tuple → byte-identical corpus across runs / machines /
/// OSes. The per-document body is deterministic via `ChaCha20Rng::seed_from_u64(seed)`.
///
/// ## Source convention
///
/// Synthetic episodes use `source = "bench:md-doc/<idx>"` so they don't collide
/// with `helios:fs/...` Helios-namespaced data when both run on the same backend.
///
/// ## Errors
///
/// - [`BenchCorpusError::Invalid`] — `count == 0`.
/// - [`BenchCorpusError::Backend`] — the backend rejected an `atomic_write`.
///
/// Returns the number of episodes written on success.
pub async fn build_md_doc_corpus(
    storage: &dyn StoragePort,
    count: u64,
    seed: u64,
) -> Result<u64, BenchCorpusError> {
    if count == 0 {
        return Err(BenchCorpusError::Invalid(format!("count must be > 0; got {count}")));
    }

    /// Per-batch episode count. 64 × ~12 KB ≈ 768 KB per atomic_write —
    /// fits comfortably within both Moon TXN.COMMIT and Postgres single-COMMIT
    /// throughput envelopes (analog of [`CORPUS_BATCH_OPS`] but expressed in
    /// episodes-per-batch rather than ops-per-batch since each markdown doc
    /// emits exactly one KvPut).
    const BATCH: u64 = 64;

    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    let mut written: u64 = 0;
    let mut batch_ops: Vec<WriteOp> = Vec::with_capacity(BATCH as usize);

    for idx in 0..count {
        let episode = synth_md_episode(idx, &mut rng);
        // Storage key mirrors lunaris_ingest::episode_key shape verbatim:
        // `episode:` ASCII prefix + the episode id's 16 bytes. Deferring to a
        // local format!() rather than pulling lunaris-ingest as a dep here —
        // this crate already carries enough deps and the prefix is stable.
        let mut key = Vec::with_capacity(8 + 16);
        key.extend_from_slice(b"episode:");
        key.extend_from_slice(&episode.id.to_bytes());
        let value = serde_json::to_vec(&episode).map_err(BenchCorpusError::Serde)?;
        batch_ops.push(WriteOp::KvPut { key, value });

        if batch_ops.len() as u64 >= BATCH {
            storage
                .atomic_write(&lunaris_core::Scope::dev(), &batch_ops)
                .await
                .map_err(|e| BenchCorpusError::Backend(LunarisError::Storage(e)))?;
            written += batch_ops.len() as u64;
            batch_ops.clear();
        }
    }
    if !batch_ops.is_empty() {
        storage
            .atomic_write(&lunaris_core::Scope::dev(), &batch_ops)
            .await
            .map_err(|e| BenchCorpusError::Backend(LunarisError::Storage(e)))?;
        written += batch_ops.len() as u64;
    }
    Ok(written)
}

/// Produce one synthetic ~12 KB markdown [`Episode`]. Pure — no IO. RNG is
/// passed by mut ref so the caller controls determinism (same seed → byte-
/// identical corpus).
fn synth_md_episode(idx: u64, rng: &mut ChaCha20Rng) -> lunaris_core::Episode {
    let mut body = String::with_capacity(12_500);
    body.push_str(&format!("# Document {idx}\n\n"));
    let paragraphs = 8usize + (rng.random::<u8>() as usize % 4);
    for p in 0..paragraphs {
        body.push_str(&format!("## Section {p}\n\n"));
        let sentences = 6usize + (rng.random::<u8>() as usize % 5);
        for _ in 0..sentences {
            body.push_str(
                "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod \
                 tempor incididunt ut labore et dolore magna aliqua. ",
            );
        }
        body.push_str("\n\n");
    }
    let mut id_bytes = [0u8; 16];
    rng.fill(&mut id_bytes);
    lunaris_core::Episode {
        id: Ulid::from_bytes(id_bytes),
        scope: lunaris_core::Scope::dev(), // RFC 0001 Wave 0 migration crutch
        source: format!("bench:md-doc/{idx}"),
        content: body,
        t_ref: None,
        // Server stamps bt at ingest; bench bulk-write path uses Hlc::ZERO so
        // payload bytes are deterministic (see Plan 02-04 corpus convention).
        bt: BiTemporal::at(lunaris_core::Hlc::ZERO, lunaris_core::Hlc::ZERO),
        metadata: serde_json::Map::new(),
    }
}

// ----- synthetic query set -----

/// Generate `count` deterministic query strings drawn from the same vocab as
/// the corpus fact_text, so each query has hits in the corpus.
///
/// Each query is the predicate of a randomly-chosen fact-shape (`"<entity>
/// <predicate>"`) — short enough that the keyword path matches via BM25 AND
/// the vector path matches via embedding similarity (the entity name is in
/// both the corpus fact_text and the query). Distinct from the corpus seed
/// so query bytes don't match a fact_text verbatim.
pub fn synthetic_query_set(seed: u64, count: usize) -> Vec<String> {
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let entity_idx = rng.random_range(0..ENTITY_VOCAB_SIZE);
        let pred_idx = rng.random_range(0..PREDICATES.len());
        out.push(format!("{} {}", entity_name(entity_idx), PREDICATES[pred_idx]));
    }
    out
}

// ----- tests -----

#[cfg(test)]
mod tests {
    use super::*;
    use lunaris_core::{Embedder as _, StubEmbedder};

    #[test]
    fn url_hash_is_stable_and_truncated_to_16() {
        let h = hash_url("moon://localhost:6380");
        assert_eq!(h.len(), 16);
        // Re-hashing the same URL gives the same prefix.
        assert_eq!(h, hash_url("moon://localhost:6380"));
        // Different URL → different hash (with overwhelming probability).
        assert_ne!(h, hash_url("postgres://lunaris@localhost/lunaris"));
    }

    #[test]
    fn fingerprint_path_includes_count_and_hash() {
        let p = fingerprint_path_for("moon://localhost:6380", 1_000_000);
        let s = p.to_string_lossy();
        assert!(s.contains("lunaris-bench"));
        assert!(s.contains("1000000"));
        assert!(s.contains(&hash_url("moon://localhost:6380")));
    }

    #[test]
    fn graph_on_fingerprint_distinct_from_no_graph() {
        // Plan 03-04: graph-on fingerprint MUST differ from no-graph
        // fingerprint for the same (url, count) — otherwise switching density
        // would silently re-use a no-graph corpus on disk (T-03-04-04).
        let url = "moon://localhost:6380";
        let no_graph = fingerprint_path(url, 100, None);
        let graph_on = fingerprint_path(url, 100, Some(GraphDensity::default()));
        assert_ne!(no_graph, graph_on, "graph-on fingerprint must differ from no-graph");
        assert!(
            graph_on.to_string_lossy().contains("graph-on"),
            "graph-on fingerprint must contain '-graph-on' suffix: {}",
            graph_on.display()
        );
        // Sanity: no-graph fingerprint MUST NOT contain the graph-on suffix.
        assert!(
            !no_graph.to_string_lossy().contains("graph-on"),
            "no-graph fingerprint must NOT contain '-graph-on' suffix: {}",
            no_graph.display()
        );
    }

    #[test]
    fn graph_density_default_matches_d04_specification() {
        // D-04 + I-3 + I-4: default density is 10 entities/chunk × 3 relations/entity
        // = 30 relations/chunk. Anything else means the bench measures a straw man.
        let d = GraphDensity::default();
        assert_eq!(d.entities_per_chunk, 10, "D-04 entities/chunk");
        assert_eq!(d.relations_per_entity, 3, "D-04 relations/entity");
        // I-4 typo guard: the math must come out to 30 relations/chunk
        // (NOT 30 relations/entity).
        let relations_per_chunk = d.entities_per_chunk * d.relations_per_entity;
        assert_eq!(relations_per_chunk, 30, "10 entities × 3 rel/entity = 30 rel/chunk");
    }

    #[test]
    fn corpus_gen_opts_default_is_no_graph() {
        // Backward-compat invariant: callers who construct
        // `CorpusGenOpts::default()` get a no-graph corpus shape (Plan 02-04
        // behavior) — only callers who explicitly set
        // `graph_density = Some(_)` opt into the graph-on path.
        let o = CorpusGenOpts::default();
        assert_eq!(o.fact_count, DEFAULT_CORPUS_COUNT);
        assert_eq!(o.seed, DEFAULT_CORPUS_SEED);
        assert_eq!(o.graph_density, None);
    }

    #[test]
    fn fingerprint_serde_roundtrip_with_graph_density() {
        let fp = CorpusFingerprint {
            backend_url_hash: "abcdef0123456789".to_string(),
            fact_count: 100,
            seed: 7,
            completed: true,
            duration_secs: 1.23,
            graph_density: Some(GraphDensity::default()),
            schema_version: CURRENT_CORPUS_SCHEMA_VERSION,
        };
        let json = serde_json::to_string(&fp).unwrap();
        let parsed: CorpusFingerprint = serde_json::from_str(&json).unwrap();
        assert_eq!(fp, parsed);

        // Backward-compat: a Plan 02-04 fingerprint JSON without the
        // graph_density / schema_version fields MUST deserialize cleanly
        // (defaults: graph_density = None, schema_version = 0). The cache
        // hit guard then rejects v0 fingerprints because they don't match
        // CURRENT_CORPUS_SCHEMA_VERSION = 1, forcing a rebuild on the new
        // metadata shape (Gap 9 fix 2026-04-21).
        let legacy = r#"{
            "backend_url_hash": "abcdef0123456789",
            "fact_count": 100,
            "seed": 7,
            "completed": true,
            "duration_secs": 1.23
        }"#;
        let parsed_legacy: CorpusFingerprint = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed_legacy.graph_density, None);
        assert_eq!(parsed_legacy.schema_version, 0);
        assert_ne!(
            parsed_legacy.schema_version, CURRENT_CORPUS_SCHEMA_VERSION,
            "legacy fingerprint must invalidate so Gap 9's row-shape change forces a rebuild"
        );
    }

    #[tokio::test]
    async fn build_corpus_with_options_emits_graph_writeops_when_density_some() {
        // Smoke: a tiny graph-on corpus (count=2, default density) MUST
        // produce GraphNode + GraphEdge ops alongside KvPut + VectorUpsert,
        // proving the new bulk-write branch is wired end-to-end.
        let storage = Arc::new(crate::corpus::tests_recording::RecordingStorage::default());
        let storage_dyn: Arc<dyn StoragePort> = storage.clone();
        let embedder: Arc<dyn lunaris_core::Embedder> =
            Arc::new(StubEmbedder::new(CORPUS_EMBEDDING_DIM));
        let handle = Lunaris::with_parts(storage_dyn, embedder, HlcClock::new(0));

        // Use a temp target dir so the fingerprint file doesn't collide with
        // other tests' state.
        let tmp = std::env::temp_dir()
            .join(format!("lunaris-bench-graph-on-smoke-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // Safety: setting/removing CARGO_TARGET_DIR for the duration of one
        // test would race other parallel tests. Instead we point only the
        // fingerprint path through a custom build by overriding the env in
        // a sub-thread isn't reliable either. We accept that the
        // fingerprint may land in `target/lunaris-bench/` here — the
        // RecordingStorage assertions below don't depend on that.

        let url = format!("moon://test-{}-graph-on:6380", std::process::id());
        build_corpus_with_options(
            &handle,
            CorpusGenOpts { fact_count: 2, seed: 7, graph_density: Some(GraphDensity::default()) },
            &url,
        )
        .await
        .expect("build graph-on corpus");

        // We expect (per fact): 1 KvPut + 1 VectorUpsert + 10 GraphNode + 30
        // GraphEdge = 42 ops/fact. With count=2 → 84 ops total. Batched into
        // a single atomic_write call (batch capacity easily holds 84 ops).
        let mut kv = 0usize;
        let mut vec_ = 0usize;
        let mut nodes = 0usize;
        let mut edges = 0usize;
        for batch in storage.batches.lock().iter() {
            for op in batch {
                match op {
                    WriteOp::KvPut { .. } => kv += 1,
                    WriteOp::VectorUpsert { .. } => vec_ += 1,
                    WriteOp::GraphNode { .. } => nodes += 1,
                    WriteOp::GraphEdge { .. } => edges += 1,
                    WriteOp::KvDelete { .. } => {}
                    _ => {}
                }
            }
        }
        assert_eq!(kv, 2, "expected 2 KvPut (one per fact)");
        assert_eq!(vec_, 2, "expected 2 VectorUpsert (one per fact)");
        assert_eq!(nodes, 20, "expected 20 GraphNode (10 entities × 2 chunks)");
        assert_eq!(edges, 60, "expected 60 GraphEdge (10 entities × 3 rel/entity × 2 chunks)");
    }

    #[test]
    fn synthetic_query_set_is_deterministic() {
        let a = synthetic_query_set(99, 16);
        let b = synthetic_query_set(99, 16);
        assert_eq!(a, b, "same seed must produce same query set");
        let c = synthetic_query_set(100, 16);
        assert_ne!(a, c, "different seed must produce different query set");
        assert_eq!(a.len(), 16);
        // Each query is "<EntityName> <predicate>" — at least 6 + 1 + 4 chars.
        for q in &a {
            assert!(q.len() >= 11, "query too short: {q:?}");
            assert!(q.starts_with("Subject"));
        }
    }

    #[test]
    fn det_vec_matches_stub_embedder_per_string() {
        // The stored fact embedding (det_vec) MUST match what
        // StubEmbedder::embed_batch returns for the same input — this is the
        // determinism contract that lets the bench measure the storage hot
        // path independent of the real EmbeddingGemma cost.
        let stub = StubEmbedder::new(CORPUS_EMBEDDING_DIM);
        let fut = stub.embed_batch(&["hello world"]);
        let got = futures::executor::block_on(fut).expect("stub embed");
        let want = det_vec("hello world", CORPUS_EMBEDDING_DIM);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], want, "det_vec must mirror StubEmbedder per-string output");
    }

    #[test]
    fn next_fact_is_deterministic_for_same_seed() {
        let clock = HlcClock::new(0);
        let mut a = ChaCha20Rng::seed_from_u64(7);
        let mut b = ChaCha20Rng::seed_from_u64(7);
        let fa = next_fact(&mut a, &clock).unwrap();
        let fb = next_fact(&mut b, &clock).unwrap();
        assert_eq!(fa.id.to_bytes(), fb.id.to_bytes());
        assert_eq!(fa.subject.to_bytes(), fb.subject.to_bytes());
        assert_eq!(fa.predicate, fb.predicate);
        assert_eq!(fa.fact_text, fb.fact_text);
        assert_eq!(fa.embedding, fb.embedding);
    }

    #[test]
    fn invalid_count_is_rejected() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let storage: Arc<dyn StoragePort> =
            Arc::new(crate::corpus::tests_recording::RecordingStorage::default());
        let embedder: Arc<dyn lunaris_core::Embedder> =
            Arc::new(StubEmbedder::new(CORPUS_EMBEDDING_DIM));
        let handle = Lunaris::with_parts(storage, embedder, HlcClock::new(0));

        let err = rt.block_on(build_one_million_fact_corpus(&handle, 0, 0, "moon://x:1"));
        assert!(matches!(err, Err(BenchCorpusError::Invalid(_))));
    }

    #[test]
    fn parse_host_port_handles_url_and_bare_authority() {
        assert_eq!(parse_host_port("moon://localhost:6380").as_deref(), Some("localhost:6380"));
        assert_eq!(
            parse_host_port("postgres://lunaris:lunaris@localhost:5432/lunaris").as_deref(),
            Some("localhost:5432")
        );
        assert_eq!(parse_host_port("localhost:6380").as_deref(), Some("localhost:6380"));
        assert_eq!(parse_host_port("not a url").as_deref(), None);
        assert_eq!(parse_host_port("moon://localhost").as_deref(), None, "missing port → None");
    }

    #[tokio::test]
    async fn tcp_unreachable_returns_false_quickly() {
        // Port 1 on localhost is reserved + nobody listens — TCP RST is fast.
        let started = Instant::now();
        let ok = tcp_reachable("moon://localhost:1").await;
        assert!(!ok);
        // Should reject in well under 1s (the probe's timeout). RST is ~ms.
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "probe took too long: {:?}",
            started.elapsed()
        );
    }

    // ----- Plan 05-04 build_md_doc_corpus tests -----

    #[tokio::test]
    async fn build_md_doc_corpus_emits_one_kvput_per_doc() {
        // 10 docs → all fit in the first batch (BATCH=64). Expect exactly
        // one atomic_write batch with 10 KvPut ops.
        let storage = Arc::new(crate::corpus::tests_recording::RecordingStorage::default());
        let storage_dyn: &dyn StoragePort = storage.as_ref();
        let written =
            build_md_doc_corpus(storage_dyn, 10, 0xDEAD_BEEF).await.expect("build small md corpus");
        assert_eq!(written, 10, "build_md_doc_corpus reports written count");
        assert_eq!(storage.batch_count(), 1, "10 docs ≤ BATCH=64 → one atomic_write");
        let (kv, vec_) = storage.op_kind_counts();
        assert_eq!(kv, 10, "exactly 10 KvPut ops (one per doc)");
        assert_eq!(vec_, 0, "no VectorUpsert ops — markdown corpus is KV-only");
    }

    #[tokio::test]
    async fn build_md_doc_corpus_batches_at_64_docs() {
        // 130 docs → 64 + 64 + 2 = three atomic_write batches.
        let storage = Arc::new(crate::corpus::tests_recording::RecordingStorage::default());
        let storage_dyn: &dyn StoragePort = storage.as_ref();
        let written = build_md_doc_corpus(storage_dyn, 130, 7).await.expect("build");
        assert_eq!(written, 130);
        assert_eq!(storage.batch_count(), 3, "130 docs / 64 per batch = 3 batches");
        let (kv, _) = storage.op_kind_counts();
        assert_eq!(kv, 130, "130 KvPut ops total");
    }

    #[tokio::test]
    async fn build_md_doc_corpus_is_deterministic_for_same_seed() {
        // Same seed → byte-identical KvPut payloads. We compare the first
        // batch's KvPut value bytes across two runs.
        let s1 = Arc::new(crate::corpus::tests_recording::RecordingStorage::default());
        let s2 = Arc::new(crate::corpus::tests_recording::RecordingStorage::default());
        build_md_doc_corpus(s1.as_ref() as &dyn StoragePort, 4, 42).await.unwrap();
        build_md_doc_corpus(s2.as_ref() as &dyn StoragePort, 4, 42).await.unwrap();
        let b1 = s1.batches.lock().clone();
        let b2 = s2.batches.lock().clone();
        assert_eq!(b1.len(), 1);
        assert_eq!(b2.len(), 1);
        // Compare the serialized payloads pairwise — same seed must produce
        // byte-identical Episode bodies (the determinism contract).
        for (op1, op2) in b1[0].iter().zip(b2[0].iter()) {
            match (op1, op2) {
                (WriteOp::KvPut { value: v1, .. }, WriteOp::KvPut { value: v2, .. }) => {
                    assert_eq!(v1, v2, "same seed → byte-identical KvPut payloads")
                }
                _ => panic!("expected KvPut ops"),
            }
        }
    }

    #[tokio::test]
    async fn build_md_doc_corpus_rejects_zero_count() {
        let storage = Arc::new(crate::corpus::tests_recording::RecordingStorage::default());
        let result = build_md_doc_corpus(storage.as_ref() as &dyn StoragePort, 0, 1).await;
        assert!(matches!(result, Err(BenchCorpusError::Invalid(_))));
    }
}

// ----- shared in-test storage (re-used by the corpus_smoke integration test) -----
//
// Exposed via `pub(crate)` so the corpus_smoke test can inject the same shape
// without duplicating the impl. NOT part of the public API.
#[doc(hidden)]
pub mod tests_recording {
    use super::*;
    use async_trait::async_trait;
    use bytes::Bytes;
    use futures::stream::BoxStream;
    use lunaris_core::{
        CypherQuery, Filter, GraphResult, Hlc, Lsn, QueueMsg, Row, StorageCapabilities, VectorHit,
    };
    use parking_lot::Mutex;

    /// Records every `atomic_write` batch in memory + tallies KvPut /
    /// VectorUpsert ops for assertions. All non-write methods return
    /// `NotSupported` — this is a write-only recorder, sufficient for the
    /// Plan 02-04 corpus smoke test.
    #[derive(Default)]
    pub struct RecordingStorage {
        pub batches: Mutex<Vec<Vec<WriteOp>>>,
    }

    impl RecordingStorage {
        pub fn batch_count(&self) -> usize {
            self.batches.lock().len()
        }
        pub fn total_ops(&self) -> usize {
            self.batches.lock().iter().map(|b: &Vec<WriteOp>| b.len()).sum()
        }
        pub fn op_kind_counts(&self) -> (usize, usize) {
            // (kv_puts, vector_upserts)
            let mut kv = 0;
            let mut vec_ = 0;
            for batch in self.batches.lock().iter() {
                for op in batch {
                    match op {
                        WriteOp::KvPut { .. } => kv += 1,
                        WriteOp::VectorUpsert { .. } => vec_ += 1,
                        _ => {}
                    }
                }
            }
            (kv, vec_)
        }
    }

    #[async_trait]
    impl StoragePort for RecordingStorage {
        async fn atomic_write(
            &self,
            _scope: &lunaris_core::Scope,
            ops: &[WriteOp],
        ) -> Result<Lsn, StorageError> {
            self.batches.lock().push(ops.to_vec());
            Ok(Lsn { wall_ms: 1, counter: self.batches.lock().len() as u32 })
        }
        async fn vector_search(
            &self,
            _scope: &lunaris_core::Scope,
            _index: &str,
            _query: &[f32],
            _k: usize,
            _filter: Option<&Filter>,
            _as_of: Option<Hlc>,
            _rerank: bool,
        ) -> Result<Vec<VectorHit>, StorageError> {
            Err(StorageError::NotSupported("RecordingStorage::vector_search"))
        }
        async fn graph_traverse(
            &self,
            _scope: &lunaris_core::Scope,
            _query: &CypherQuery,
            _as_of: Option<Hlc>,
        ) -> Result<GraphResult, StorageError> {
            Err(StorageError::NotSupported("RecordingStorage::graph_traverse"))
        }
        async fn scan_range(
            &self,
            _scope: &lunaris_core::Scope,
            _prefix: &[u8],
            _as_of: Option<Hlc>,
        ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
            Err(StorageError::NotSupported("RecordingStorage::scan_range"))
        }
        async fn read_as_of(
            &self,
            _scope: &lunaris_core::Scope,
            _key: &[u8],
            _as_of: Hlc,
        ) -> Result<Option<Row<Bytes>>, StorageError> {
            Err(StorageError::NotSupported("RecordingStorage::read_as_of"))
        }
        async fn publish(
            &self,
            _scope: &lunaris_core::Scope,
            _topic: &str,
            _partition: u16,
            _payload: Bytes,
        ) -> Result<u64, StorageError> {
            Err(StorageError::NotSupported("RecordingStorage::publish"))
        }
        async fn subscribe(
            &self,
            _scope: &lunaris_core::Scope,
            _group: &str,
            _topic: &str,
            _partition: u16,
        ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
            Err(StorageError::NotSupported("RecordingStorage::subscribe"))
        }
        fn capabilities(&self) -> StorageCapabilities {
            StorageCapabilities {
                bi_temporal_native: false,
                graph_native: false,
                rerank_native: false,
                queue_native: false,
                max_vector_dim: CORPUS_EMBEDDING_DIM as u32,
                native_rrf: false,
                max_scopes_recommended: 0,
                cypher_dialect: lunaris_core::CypherDialect::Legacy,
                graph_decay_native: false,
            }
        }
    }
}
