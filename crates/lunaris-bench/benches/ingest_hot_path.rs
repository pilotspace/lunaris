//! Plan 02-04 — Criterion bench for the Phase 2 ingest hot path.
//! Plan 03-04 — extended with the graph-ON variant per backend (INGEST-06).
//!
//! Measures `Lunaris::ingest(Episode { 12 KB markdown })` end-to-end on TWO
//! recipes:
//!
//! 1. `ingest_12kb_md/{moon,postgres}` — Phase 2 fast path (graph OFF).
//!    Blueprint §4.1 latency budget asserts p50 ≤ 50 ms / p99 ≤ 110 ms on
//!    both `MoonStorage` AND `PostgresStorage`.
//! 2. `ingest_12kb_md_graph_on/{moon,postgres}` — Plan 03-04 INGEST-06
//!    graph-ON path. Blueprint §4.1 budget: p50 ≤ 300 ms / p99 ≤ 570 ms.
//!    The bench enables `graph_pipeline` on the handle BEFORE the iter loop
//!    and installs a non-network `StubExtractor` (D-03) so the measurement
//!    isolates the storage + atomic_write fan-out cost from the real
//!    Gemma-3 4B forward pass (~150 ms/chunk per D-03 — would inflate the
//!    budget assertion to fail on dev boxes).
//!
//! Both recipes share the same backend selection + skip discipline + handle
//! construction, just with `with_extractor(StubExtractor)` +
//! `graph_pipeline().enable()` chained on for the graph-on variant.
//!
//! ## Backend selection
//!
//! Each backend opts in via an env var:
//! - `MOON_URL=moon://localhost:6380` → registers the `*/moon` benches.
//! - `PG_URL=postgres://lunaris:lunaris@localhost/lunaris` → registers the
//!   `*/postgres` benches.
//!
//! When a URL env is unset OR the host:port doesn't accept a 1-sec TCP probe,
//! the bench group emits `eprintln!("SKIP …")` and continues without
//! registering that backend. Empty bench groups (both backends unreachable)
//! finalise with zero registered benches → Criterion reports "no benches".
//!
//! ## Embedder + extractor + reranker choices
//!
//! Default: `lunaris_core::StubEmbedder::new(768)` + `NoopReranker`. The
//! graph-on variant ALSO installs a `StubExtractor` (defined below) that
//! returns 10 entities/chunk × 3 relations/entity = 30 relations/chunk
//! (D-04 default density per I-3 fix) — matches the realistic ingest
//! fan-out shape WITHOUT burning the 1.1 GB Gemma cache on every CI run.
//! Real-extractor variant gated behind `LUNARIS_EXTRACT_GEMMA_PATH` is a
//! deferred follow-up (mirror of `LUNARIS_EMBED_GEMMA_PATH` from Plan
//! 02-04).
//!
//! ## Sample size + warmup
//!
//! Default Criterion `sample_size = 100` × ~50 ms/sample = 5 s per backend
//! per bench. We override to `sample_size = 30` and `measurement_time = 30 s`
//! so each backend completes in ~30 s (faster CI feedback; still tight CI on
//! p99 with 30 samples per the central limit theorem).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use lunaris::Lunaris;
use lunaris_bench::{tcp_reachable, twelve_kb_markdown_fixture};
use lunaris_core::{Episode, LunarisError, StubEmbedder};
use lunaris_extract::{
    ChunkInput, Entity, EntityId, Extractor, RawExtraction, RawExtractionBatch, Relation,
};
use ulid::Ulid;

/// Bench-local backend descriptor. Pairs a human label with the env-var URL
/// so the SKIP messages render the label, not the URL (T-02-04-01).
struct Backend {
    label: &'static str,
    env: &'static str,
}

const BACKENDS: &[Backend] =
    &[Backend { label: "moon", env: "MOON_URL" }, Backend { label: "postgres", env: "PG_URL" }];

// ----- Plan 03-04 — bench-only StubExtractor (D-03 / I-3 / I-4) -----

/// Bench-only extractor density.
///
/// Parameterised per I-3 fix so the bench measures the realistic graph-on
/// payload — `default()` is 10 entities/chunk × 3 relations/entity = 30
/// relations/chunk (D-04 default). NOT a 4-entity straw man; if the
/// `Default` impl drifts, the bench would silently report a fast-but-
/// meaningless p50 because the per-Episode WriteOp count would shrink.
#[derive(Clone, Debug)]
struct EntityDensity {
    entities_per_chunk: usize,
    relations_per_entity: usize,
}

impl Default for EntityDensity {
    /// Match the D-04 default density so the bench measures realistic
    /// graph-on payload. 10 entities/chunk × 3 relations/entity = 30
    /// relations/chunk (NOT 30 relations/entity — I-4 typo guard).
    fn default() -> Self {
        Self { entities_per_chunk: 10, relations_per_entity: 3 }
    }
}

/// Bench-only deterministic extractor with parameterised density (I-3 fix).
///
/// Returns `density.entities_per_chunk` entities + `density.relations_per_entity`
/// relations PER chunk so the bench measures the realistic graph-on payload
/// (D-04 default: 10 entities/chunk × 3 relations/entity = 30 relations/chunk
/// = full ingest fan-out cost). Zero network, zero blocking, constant memory.
///
/// Used by the graph-on ingest bench to isolate the storage + atomic_write
/// fan-out cost from the real Gemma-3 4B forward pass (~150 ms/chunk per
/// D-03). Real-extractor variant gated behind `LUNARIS_EXTRACT_GEMMA_PATH` is
/// a deferred follow-up (mirror of `LUNARIS_EMBED_GEMMA_PATH` from Plan
/// 02-04).
#[derive(Clone, Debug)]
struct StubExtractor {
    density: EntityDensity,
}

impl StubExtractor {
    fn new() -> Self {
        Self { density: EntityDensity::default() }
    }
}

#[async_trait]
impl Extractor for StubExtractor {
    async fn extract(
        &self,
        _episode_id: Ulid,
        chunks: &[ChunkInput],
    ) -> Result<RawExtractionBatch, LunarisError> {
        let by_chunk = chunks
            .iter()
            .map(|c| {
                // Generate `entities_per_chunk` entities per chunk.
                let entities: Vec<Entity> = (0..self.density.entities_per_chunk)
                    .map(|i| Entity {
                        id: EntityId::from_name_and_type(
                            &format!("bench-entity-{}-{}", c.chunk_id, i),
                            "BenchEntity",
                        ),
                        name: format!("entity-{i}-for-chunk-{}", c.chunk_id),
                        aliases: vec![],
                        entity_type: "BenchEntity".into(),
                        confidence: 0.95,
                        valid_from_iso: "2024-01-01T00:00:00Z".into(),
                        valid_to_iso: None,
                    })
                    .collect();

                // Generate `relations_per_entity` relations per entity (round-
                // robin paired to the next entity in the list, wrapping;
                // mirrors typical extraction shape).
                let relation_cap =
                    self.density.entities_per_chunk * self.density.relations_per_entity;
                let mut relations: Vec<Relation> = Vec::with_capacity(relation_cap);
                if !entities.is_empty() {
                    for (idx, e) in entities.iter().enumerate() {
                        for r in 0..self.density.relations_per_entity {
                            let target_idx = (idx + r + 1) % entities.len();
                            relations.push(Relation {
                                subject_id: e.id,
                                predicate: format!("bench_rel_{r}"),
                                object_id: entities[target_idx].id,
                                confidence: 0.9,
                                valid_from_iso: "2024-01-01T00:00:00Z".into(),
                                valid_to_iso: None,
                            });
                        }
                    }
                }

                RawExtraction { source_chunk_id: c.chunk_id, entities, relations, facts: vec![] }
            })
            .collect();
        Ok(RawExtractionBatch { by_chunk })
    }

    fn applies(&self) -> bool {
        true
    }
}

fn ingest_benches(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group("ingest_hot_path");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(30));
    group.warm_up_time(Duration::from_secs(5));

    let fixture = twelve_kb_markdown_fixture();

    let mut registered_any = false;
    for backend in BACKENDS {
        let Ok(url) = std::env::var(backend.env) else {
            eprintln!("SKIP ingest/{} bench: {} unset", backend.label, backend.env);
            continue;
        };
        // 1-sec TCP probe so an unreachable backend doesn't burn 30s per
        // sample on the open() retry path.
        if !runtime.block_on(tcp_reachable(&url)) {
            eprintln!(
                "SKIP ingest/{} bench: backend unreachable (1s TCP probe failed)",
                backend.label
            );
            continue;
        }
        // Build the handle ONCE per backend; the inner iter loop reuses it.
        // StubEmbedder isolates the storage hot path from the EmbeddingGemma
        // forward-pass cost.
        let handle = match runtime.block_on(async {
            let h = Lunaris::open(&url).await?;
            Ok::<_, lunaris::LunarisError>(h.with_embedder(Arc::new(StubEmbedder::new(768))))
        }) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("SKIP ingest/{} bench: open failed: {}", backend.label, e);
                continue;
            }
        };

        registered_any = true;
        group.bench_with_input(
            BenchmarkId::new("ingest_12kb_md", backend.label),
            &fixture,
            |b, content| {
                b.to_async(&runtime).iter(|| async {
                    // Each iteration constructs a fresh Episode (new ULID) so
                    // we measure ingest, not idempotent re-write.
                    let ep = Episode::new(
                        lunaris::Scope::dev(),
                        "bench/12kb_doc.md",
                        *content,
                        &handle.clock(),
                    );
                    let _lsn = handle.ingest(ep).await.expect("ingest");
                });
            },
        );

        // ----- Plan 03-04 — graph-ON variant (INGEST-06) -----
        //
        // Build a SEPARATE handle for graph-on so the toggle state doesn't
        // leak back into the no-graph bench above. `with_extractor` swaps
        // the extractor on the GraphPipelineHandle (D-03 — install
        // StubExtractor BEFORE enabling so the first ingest sees the right
        // extractor); `graph_pipeline().enable()` flips the toggle ON before
        // the iter loop. The is_enabled() assertion catches the toggle bug
        // at registration time (T-03-04-03 mitigation).
        let handle_graph_on =
            handle.clone().with_extractor(Arc::new(StubExtractor::new()) as Arc<dyn Extractor>);
        handle_graph_on.graph_pipeline().enable();
        assert!(
            handle_graph_on.graph_pipeline().is_enabled(),
            "graph-on bench requires graph pipeline enabled — got is_enabled() == false"
        );

        group.bench_with_input(
            BenchmarkId::new("ingest_12kb_md_graph_on", backend.label),
            &fixture,
            |b, content| {
                b.to_async(&runtime).iter(|| async {
                    // Same shape as the no-graph variant — fresh Episode per
                    // iter so we measure ingest fan-out (KvPut + chunk
                    // VectorUpsert + per-extracted-entity GraphNode +
                    // per-relation GraphEdge + per-fact KvPut), NOT
                    // idempotent re-write.
                    let ep = Episode::new(
                        lunaris::Scope::dev(),
                        "bench/12kb_doc.md",
                        *content,
                        &handle_graph_on.clock(),
                    );
                    let _lsn = handle_graph_on.ingest(ep).await.expect("ingest graph_on");
                });
            },
        );
    }

    if !registered_any {
        eprintln!(
            "ingest_hot_path: no live backends reachable; skipping group. \
             Set MOON_URL and/or PG_URL to populate Criterion data."
        );
    }
    group.finish();
}

criterion_group!(benches, ingest_benches);
criterion_main!(benches);
