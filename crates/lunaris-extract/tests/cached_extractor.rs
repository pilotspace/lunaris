//! Content-addressed extraction cache — `CachedExtractor` decorator contract
//! (graph-ingest cost elimination, 2026-07-29).
//!
//! LLM extraction is the dominant graph-ingest cost (~23s per MiniMax-M3
//! call; ~80% of graph-ON LongMemEval wall clock). The decorator makes
//! identical (prompt-template, model-namespace, chunk) extractions hit the
//! LLM exactly once, replaying from a filesystem cache afterwards:
//!
//! - key   = blake3(namespace || build_prompt(chunk)) — the prompt string
//!   embeds chunk text + heading_path + the template itself, so editing the
//!   template auto-invalidates every stale entry;
//! - value = the chunk's `RawExtraction` JSON, written atomically
//!   (tmp file + rename) so concurrent fill processes never tear entries;
//! - on replay `source_chunk_id` is rewritten to the CURRENT chunk id
//!   (chunk ids are fresh Ulids per ingest — provenance must follow them);
//! - cache read/write failures NEVER fail extraction — a corrupt or
//!   unreadable entry degrades to a plain inner-extractor miss
//!   (design-for-failure: the cache is an accelerator, not a dependency).
//!
//! COMPILE-RED until `lunaris_extract::CachedExtractor` lands — confined to
//! this test binary.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use lunaris_core::LunarisError;
use lunaris_extract::types::{ChunkInput, Entity, EntityId, RawExtraction, RawExtractionBatch};
use lunaris_extract::{CachedExtractor, Extractor};
use std::sync::Mutex;
use ulid::Ulid;

// ─── Counting stub inner extractor ───────────────────────────────────────────

/// Deterministic inner extractor: one entity per chunk derived from the chunk
/// text, plus a call counter and a log of exactly which chunk texts reached it.
struct CountingExtractor {
    calls: AtomicUsize,
    seen_texts: Mutex<Vec<Vec<String>>>,
}

impl CountingExtractor {
    fn new() -> Arc<Self> {
        Arc::new(Self { calls: AtomicUsize::new(0), seen_texts: Mutex::new(Vec::new()) })
    }
}

fn entity_for(text: &str) -> Entity {
    Entity {
        id: EntityId::from_name_and_type(text, "Probe"),
        name: text.to_owned(),
        aliases: vec![],
        entity_type: "Probe".to_owned(),
        confidence: 0.9,
        valid_from_iso: "2026-07-29T00:00:00Z".to_owned(),
        valid_to_iso: None,
    }
}

#[async_trait]
impl Extractor for CountingExtractor {
    async fn extract(
        &self,
        _episode_id: Ulid,
        chunks: &[ChunkInput],
    ) -> Result<RawExtractionBatch, LunarisError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen_texts.lock().unwrap().push(chunks.iter().map(|c| c.text.clone()).collect());
        Ok(RawExtractionBatch {
            by_chunk: chunks
                .iter()
                .map(|c| RawExtraction {
                    source_chunk_id: c.chunk_id,
                    entities: vec![entity_for(&c.text)],
                    relations: vec![],
                    facts: vec![],
                })
                .collect(),
        })
    }
}

fn chunk(text: &str) -> ChunkInput {
    ChunkInput { chunk_id: Ulid::new(), text: text.to_owned(), heading_path: vec![] }
}

fn cached(inner: Arc<CountingExtractor>, dir: &Path, ns: &str) -> CachedExtractor {
    CachedExtractor::new(inner as Arc<dyn Extractor>, dir, ns).expect("cache dir must be creatable")
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// Fill-then-replay: the second extraction of byte-identical chunk texts (with
/// FRESH chunk ids, as real re-ingest produces) must not touch the inner
/// extractor, must return the cached entities, and must carry the NEW ids.
#[tokio::test]
async fn first_call_fills_cache_second_call_replays() {
    let dir = tempfile::tempdir().unwrap();
    let inner = CountingExtractor::new();

    let first = cached(inner.clone(), dir.path(), "MiniMax-M3");
    let fill = first
        .extract(Ulid::new(), &[chunk("alice met bob"), chunk("bob runs zephyr-relay")])
        .await
        .expect("fill extract");
    assert_eq!(inner.calls.load(Ordering::SeqCst), 1, "fill pass must call inner once");
    assert_eq!(fill.by_chunk.len(), 2);

    // Fresh decorator instance (fresh process semantics), fresh chunk ids.
    let replayed_chunks = [chunk("alice met bob"), chunk("bob runs zephyr-relay")];
    let second = cached(inner.clone(), dir.path(), "MiniMax-M3");
    let replay = second.extract(Ulid::new(), &replayed_chunks).await.expect("replay extract");

    assert_eq!(
        inner.calls.load(Ordering::SeqCst),
        1,
        "replay must be served entirely from cache — inner not called again"
    );
    assert_eq!(replay.by_chunk.len(), 2);
    for (raw, ch) in replay.by_chunk.iter().zip(replayed_chunks.iter()) {
        assert_eq!(
            raw.source_chunk_id, ch.chunk_id,
            "replayed extraction must carry the CURRENT chunk id, not the cached one"
        );
    }
    assert_eq!(
        replay.by_chunk[0].entities, fill.by_chunk[0].entities,
        "replayed entities must be byte-identical to the fill pass"
    );
    assert_eq!(replay.by_chunk[1].entities, fill.by_chunk[1].entities);
}

/// Partial hit: only the never-seen chunk reaches the inner extractor, and the
/// merged batch preserves input order (cached hit at index 0, live at 1).
#[tokio::test]
async fn partial_hit_sends_only_misses_to_inner() {
    let dir = tempfile::tempdir().unwrap();
    let inner = CountingExtractor::new();

    let c = cached(inner.clone(), dir.path(), "MiniMax-M3");
    c.extract(Ulid::new(), &[chunk("cached chunk")]).await.expect("fill");
    assert_eq!(inner.calls.load(Ordering::SeqCst), 1);

    let batch = [chunk("cached chunk"), chunk("brand new chunk")];
    let out = c.extract(Ulid::new(), &batch).await.expect("mixed extract");

    assert_eq!(inner.calls.load(Ordering::SeqCst), 2, "one more inner call for the miss");
    let last_seen = inner.seen_texts.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        last_seen,
        vec!["brand new chunk".to_owned()],
        "inner must receive ONLY the cache-miss chunk"
    );
    assert_eq!(out.by_chunk.len(), 2, "merged batch covers every input chunk");
    assert_eq!(out.by_chunk[0].source_chunk_id, batch[0].chunk_id);
    assert_eq!(out.by_chunk[0].entities[0].name, "cached chunk");
    assert_eq!(out.by_chunk[1].source_chunk_id, batch[1].chunk_id);
    assert_eq!(out.by_chunk[1].entities[0].name, "brand new chunk");
}

/// The namespace (model identity) isolates entries: the same chunk under a
/// different namespace is a miss, never a cross-model bleed.
#[tokio::test]
async fn namespace_isolates_cache_entries() {
    let dir = tempfile::tempdir().unwrap();
    let inner = CountingExtractor::new();

    cached(inner.clone(), dir.path(), "MiniMax-M3")
        .extract(Ulid::new(), &[chunk("shared text")])
        .await
        .expect("fill under model A");
    cached(inner.clone(), dir.path(), "gemma3:4b")
        .extract(Ulid::new(), &[chunk("shared text")])
        .await
        .expect("extract under model B");

    assert_eq!(
        inner.calls.load(Ordering::SeqCst),
        2,
        "different namespace must be a cache miss (no cross-model replay)"
    );
}

/// A corrupt cache entry is a miss, not an error: extraction succeeds via the
/// inner extractor and the entry is healed for the next call.
#[tokio::test]
async fn corrupt_cache_entry_degrades_to_miss() {
    let dir = tempfile::tempdir().unwrap();
    let inner = CountingExtractor::new();
    let c = cached(inner.clone(), dir.path(), "MiniMax-M3");

    c.extract(Ulid::new(), &[chunk("healable")]).await.expect("fill");
    assert_eq!(inner.calls.load(Ordering::SeqCst), 1);

    // Corrupt every entry file in the cache dir.
    for e in std::fs::read_dir(dir.path()).unwrap() {
        let p = e.unwrap().path();
        if p.is_file() {
            std::fs::write(&p, b"{ not valid json").unwrap();
        }
    }

    let out = c.extract(Ulid::new(), &[chunk("healable")]).await.expect("must not error");
    assert_eq!(
        inner.calls.load(Ordering::SeqCst),
        2,
        "corrupt entry must fall through to the inner extractor"
    );
    assert_eq!(out.by_chunk[0].entities[0].name, "healable");

    // Healed: the re-extract must have rewritten the entry.
    c.extract(Ulid::new(), &[chunk("healable")]).await.expect("replay after heal");
    assert_eq!(inner.calls.load(Ordering::SeqCst), 2, "healed entry replays from cache");
}

/// Observability: hit/miss counters reflect exactly what happened — the
/// harness prints these after ingest so a mis-wired cache is visible.
#[tokio::test]
async fn stats_count_hits_and_misses() {
    let dir = tempfile::tempdir().unwrap();
    let inner = CountingExtractor::new();
    let c = cached(inner.clone(), dir.path(), "MiniMax-M3");

    c.extract(Ulid::new(), &[chunk("a"), chunk("b")]).await.expect("fill");
    c.extract(Ulid::new(), &[chunk("a"), chunk("b"), chunk("c")]).await.expect("mixed");

    let stats = c.stats();
    assert_eq!(stats.misses, 3, "a+b (fill) then c are misses");
    assert_eq!(stats.hits, 2, "a+b replay on the second call");
}

/// `applies()` forwards to the inner extractor (Noop stays skippable so the
/// ingest fan-out keeps eliding GraphNode/GraphEdge WriteOps).
#[tokio::test]
async fn applies_forwards_to_inner() {
    let dir = tempfile::tempdir().unwrap();
    let noop = Arc::new(lunaris_extract::NoopExtractor) as Arc<dyn Extractor>;
    let c = CachedExtractor::new(noop, dir.path(), "noop").expect("build");
    assert!(!c.applies(), "NoopExtractor's applies()=false must pass through the cache");
}
