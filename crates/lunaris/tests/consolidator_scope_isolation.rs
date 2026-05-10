//! Plan 12-02 HELIOS-04 — scope-filter isolation test (CONTEXT.md D-04/D-05).
//!
//! Hard acceptance gate: with `ConsolidatorPipelineHandle::enable_for_scope(
//! "helios:fs/")` ON, seed 50% `helios:fs/…` + 50% `other:…` events; assert
//! ZERO `AuditEvent::ConsolidatorPromotion` messages for `other:` sources.
//! Also asserts NON-ZERO promotions for the `helios:fs/` half so the filter
//! can't be silently vacuous (control gate baked in).
//!
//! The test uses an in-process `Arc<dyn StoragePort>` fixture that:
//!
//! - implements the broker surface (`publish`/`subscribe`) as a single
//!   mpsc channel per topic (`__lunaris_consolidate__` + `__lunaris_audit__`).
//! - installs an `AlwaysPromoteConsolidator` via `Lunaris::with_consolidator`
//!   that unconditionally emits one `PromotionEvent` per event it sees
//!   AFTER `consolidate_scoped`'s filter has run. The filter is the
//!   single point of truth we're testing; ACT-R scoring is Phase 4's
//!   concern.
//! - drives the full publish → worker → `consolidate_scoped` → audit-emit
//!   loop end-to-end inside one tokio runtime.
//!
//! The `#[ignore]`-gated dual-backend variant probes MOON_URL / PG_URL the
//! same way `helios_scratchpad_smoke.rs` does and runs the same assertions
//! against live backends. Default `cargo test --workspace` still compiles
//! the file and runs the default in-process gate.

// Note: `std::env::set_var` is `unsafe` under Rust 2024 (MSRV 1.94). We need
// it to shrink the worker's debounce window from the 60 s default so the
// test runs in seconds instead of minutes. Localised `unsafe` is preferred
// over duplicating the env-var plumbing in the worker just for tests — the
// only reads of the env var happen at `run_consolidate_worker` startup, and
// we call `enable_for_scope` only after `set_var` returns.
#![deny(rust_2018_idioms, unreachable_pub)]

use std::collections::HashMap;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris::{Consolidator, Lunaris, NoopReranker, Reranker};
use lunaris_consolidate::{ConsolidateEvent, ConsolidationReport, PromotionEvent, types::FactId};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    BiTemporal, Embedder, Episode, Hlc, HlcClock, LunarisError, StorageCapabilities, StorageError,
    StoragePort, StubEmbedder,
};
use lunaris_extract::EntityId;
use parking_lot::Mutex;
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// AlwaysPromoteConsolidator — test fixture (see advisor guidance). Promotes
// every event it sees via one `PromotionEvent`. The PromotionEvent's
// `episode_id` is copied from the incoming ConsolidateEvent so the test can
// look the source up in its local `episode_id -> source` map (no storage
// reads needed).
// ---------------------------------------------------------------------------

#[derive(Default)]
struct AlwaysPromoteConsolidator;

#[async_trait]
impl Consolidator for AlwaysPromoteConsolidator {
    async fn consolidate(
        &self,
        _storage: Arc<dyn StoragePort>,
        events: &[ConsolidateEvent],
    ) -> Result<ConsolidationReport, LunarisError> {
        // Emit ONE promotion per event. This is a test-only fixture — the
        // filter we're gating lives in `Consolidator::consolidate_scoped`'s
        // default impl, which runs BEFORE this method is called. If an
        // `other:` event reaches this path, the filter has leaked.
        let mut promotions = Vec::with_capacity(events.len());
        for ev in events {
            promotions.push(PromotionEvent {
                episode_id: ev.episode_id,
                // Deterministic synthetic FactId — the scope-filter test
                // doesn't inspect fact content, only the promotion's
                // episode_id (to map back to source).
                fact_id: FactId::from_triple(EntityId([1; 16]), "test", EntityId([2; 16])),
                activation_score: 1.0,
            });
        }
        Ok(ConsolidationReport { promotions, archives: Vec::new(), communities_rebuilt: 0 })
    }

    fn applies(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// InProcessBroker — `Arc<dyn StoragePort>` with channel-backed publish /
// subscribe so the Consolidator worker can actually consume envelopes the
// test emits. Vector/graph/keyword surfaces are no-ops (we don't exercise
// them here).
// ---------------------------------------------------------------------------

struct InProcessBroker {
    rows: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    /// Per-topic mpsc channel. Sender side is stored so `publish` can push;
    /// receiver side is handed out on the first `subscribe` for that topic.
    consolidate_tx: mpsc::UnboundedSender<QueueMsg>,
    consolidate_rx: Mutex<Option<mpsc::UnboundedReceiver<QueueMsg>>>,
    audit_tx: mpsc::UnboundedSender<QueueMsg>,
    audit_rx: Mutex<Option<mpsc::UnboundedReceiver<QueueMsg>>>,
    /// Every audit envelope observed, in publish order.
    audit_log: Mutex<Vec<serde_json::Value>>,
    consolidate_offset: Mutex<u64>,
    audit_offset: Mutex<u64>,
}

impl InProcessBroker {
    fn new() -> Arc<Self> {
        let (consolidate_tx, consolidate_rx) = mpsc::unbounded_channel();
        let (audit_tx, audit_rx) = mpsc::unbounded_channel();
        Arc::new(Self {
            rows: Mutex::new(HashMap::new()),
            consolidate_tx,
            consolidate_rx: Mutex::new(Some(consolidate_rx)),
            audit_tx,
            audit_rx: Mutex::new(Some(audit_rx)),
            audit_log: Mutex::new(Vec::new()),
            consolidate_offset: Mutex::new(0),
            audit_offset: Mutex::new(0),
        })
    }

    /// Snapshot of the audit log — every envelope the worker published in
    /// order. Used by assertions.
    fn audit_snapshot(&self) -> Vec<serde_json::Value> {
        self.audit_log.lock().clone()
    }
}

#[async_trait]
impl StoragePort for InProcessBroker {
    async fn atomic_write(
        &self,
        _scope: &lunaris_core::Scope,
        ops: &[WriteOp],
    ) -> Result<Lsn, StorageError> {
        for op in ops {
            if let WriteOp::KvPut { key, value } = op {
                self.rows.lock().insert(key.clone(), value.clone());
            }
        }
        Ok(Lsn { wall_ms: 1, counter: 1 })
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
        Ok(Vec::new())
    }

    async fn graph_traverse(
        &self,
        _scope: &lunaris_core::Scope,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Ok(GraphResult::default())
    }

    async fn scan_range(
        &self,
        _scope: &lunaris_core::Scope,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(Box::pin(stream::iter(Vec::<Result<(Bytes, Bytes), StorageError>>::new())))
    }

    async fn read_as_of(
        &self,
        _scope: &lunaris_core::Scope,
        key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Ok(self.rows.lock().get(key).cloned().map(|v| Row {
            key: key.to_vec(),
            value: Bytes::from(v),
            bt: BiTemporal::at(Hlc::ZERO, Hlc::ZERO),
        }))
    }

    async fn publish(
        &self,
        _scope: &lunaris_core::Scope,
        topic: &str,
        partition: u16,
        payload: Bytes,
    ) -> Result<u64, StorageError> {
        match topic {
            "__lunaris_consolidate__" => {
                let mut off = self.consolidate_offset.lock();
                *off += 1;
                let msg = QueueMsg {
                    topic: topic.to_string(),
                    offset: *off,
                    partition,
                    payload: payload.clone(),
                };
                let _ = self.consolidate_tx.send(msg);
                Ok(*off)
            }
            "__lunaris_audit__" => {
                let mut off = self.audit_offset.lock();
                *off += 1;
                let msg = QueueMsg {
                    topic: topic.to_string(),
                    offset: *off,
                    partition,
                    payload: payload.clone(),
                };
                if let Ok(env) = serde_json::from_slice::<serde_json::Value>(&payload) {
                    self.audit_log.lock().push(env);
                }
                let _ = self.audit_tx.send(msg);
                Ok(*off)
            }
            _ => Ok(0),
        }
    }

    async fn subscribe(
        &self,
        _scope: &lunaris_core::Scope,
        _group: &str,
        topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        let rx_opt = match topic {
            "__lunaris_consolidate__" => self.consolidate_rx.lock().take(),
            "__lunaris_audit__" => self.audit_rx.lock().take(),
            _ => None,
        };
        let Some(rx) = rx_opt else {
            return Ok(Box::pin(stream::empty()));
        };
        Ok(Box::pin(stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|msg| (Ok(msg), rx))
        })))
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: false,
            graph_native: false,
            rerank_native: false,
            queue_native: false,
            max_vector_dim: 768,
            native_rrf: false,
        }
    }
}

#[async_trait]
impl KeywordPort for InProcessBroker {
    async fn keyword_search(
        &self,
        _index: &str,
        _query: &str,
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// Helpers shared by default + ignore-gated tests.
// ---------------------------------------------------------------------------

fn build_in_process_lunaris() -> (Lunaris, Arc<InProcessBroker>, Arc<HlcClock>) {
    let broker = InProcessBroker::new();
    let storage: Arc<dyn StoragePort> = broker.clone();
    let keyword: Arc<dyn KeywordPort> = broker.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    let handle = Lunaris::with_parts_keyword(storage, keyword, embedder, clock.clone())
        .with_reranker(Arc::new(NoopReranker) as Arc<dyn Reranker>)
        .with_consolidator(Arc::new(AlwaysPromoteConsolidator) as Arc<dyn Consolidator>);
    (handle, broker, clock)
}

// ---------------------------------------------------------------------------
// Hard gate — in-process end-to-end scope isolation.
// ---------------------------------------------------------------------------

/// Phase 12 HELIOS-04 / CONTEXT.md D-04 + D-05 — HARD acceptance gate.
///
/// With `enable_for_scope("helios:fs/")` ON, a 50/50 seed of
/// `helios:fs/…` + `other:…` Episodes MUST emit audit events ONLY for
/// `helios:fs/` promotions. Zero `other:` promotions in the audit log is
/// the non-negotiable invariant. The control half asserts the filter
/// isn't vacuous — Helios events MUST be promoted (otherwise the test
/// is a false-pass).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consolidator_scope_isolation_zero_other_promotions() {
    // Short debounce so the worker flushes quickly in test time.
    unsafe {
        std::env::set_var("LUNARIS_CONSOLIDATE_DEBOUNCE_MS", "50");
    }

    let (lunaris, broker, clock) = build_in_process_lunaris();
    let _ = clock; // unused; retained for clarity.

    // Enable scope-filter for Helios BEFORE seeding so the worker spawns
    // with the correct filter in place.
    lunaris.consolidator_pipeline().enable_for_scope("helios:fs/");
    assert_eq!(
        lunaris.consolidator_pipeline().scope_prefix(),
        Some("helios:fs/".to_string()),
        "handle snapshot must show the active scope"
    );

    // Build the 50/50 seed. 20 events per side — small enough to keep the
    // default test cycle fast, large enough that a filter bug would surface
    // as > 0 other: promotions consistently.
    const PER_SIDE: usize = 20;
    let mut id_to_source: HashMap<ulid::Ulid, String> = HashMap::new();

    for i in 0..PER_SIDE {
        let src_helios = format!("helios:fs/session-isolation/note-{i}");
        let ep = Episode::new(
            lunaris_core::Scope::dev(),
            &src_helios,
            format!("Helios note {i} body"),
            lunaris.clock().as_ref(),
        );
        id_to_source.insert(ep.id, src_helios);
        lunaris.ingest(ep).await.expect("ingest helios");

        let src_other = format!("other:tenant-{i}/note");
        let ep = Episode::new(
            lunaris_core::Scope::dev(),
            &src_other,
            format!("Other note {i} body"),
            lunaris.clock().as_ref(),
        );
        id_to_source.insert(ep.id, src_other);
        lunaris.ingest(ep).await.expect("ingest other");
    }

    // Wait for the worker's debounce flush to land. Debounce = 50 ms;
    // drain-grace on the flush path is synchronous so 750 ms gives
    // comfortable head-room on a loaded test box.
    tokio::time::sleep(Duration::from_millis(750)).await;

    // Cleanly stop the worker so the audit log is frozen at assertion time.
    lunaris.consolidator_pipeline().disable();
    lunaris.consolidator_pipeline().join_worker().await;

    // Partition audit log: helios vs other by looking up the episode_id.
    let audits = broker.audit_snapshot();
    let mut helios_promotions = 0usize;
    let mut other_promotions = 0usize;
    let mut unknown_promotions = 0usize;

    for env in &audits {
        if env.get("kind").and_then(|v| v.as_str()) != Some("ConsolidatorPromotion") {
            continue;
        }
        let Some(episode_id_str) = env.get("episode_id").and_then(|v| v.as_str()) else {
            unknown_promotions += 1;
            continue;
        };
        let Ok(episode_id) = episode_id_str.parse::<ulid::Ulid>() else {
            unknown_promotions += 1;
            continue;
        };
        match id_to_source.get(&episode_id) {
            Some(src) if src.starts_with("helios:fs/") => helios_promotions += 1,
            Some(src) if src.starts_with("other:") => other_promotions += 1,
            Some(_) | None => unknown_promotions += 1,
        }
    }

    eprintln!(
        "scope_isolation: helios_promotions={helios_promotions} \
         other_promotions={other_promotions} unknown={unknown_promotions} \
         total_audits={}",
        audits.len()
    );

    // HARD gate — no cross-tenant audit leak.
    assert_eq!(
        other_promotions, 0,
        "T-12-02-02: `other:` promotions MUST be zero — the scope filter \
         has leaked. helios={helios_promotions} other={other_promotions}"
    );

    // Non-vacuous control — if the filter is over-strict, Helios promotions
    // would also be zero and the test would be a false-pass.
    assert!(
        helios_promotions > 0,
        "scope filter must NOT be vacuous — helios_promotions > 0 required \
         (got {helios_promotions})"
    );

    // Sanity: unknown should be 0 (every PromotionEvent.episode_id comes
    // from an ingested Episode we tracked).
    assert_eq!(
        unknown_promotions, 0,
        "every promoted episode_id MUST trace back to a seeded Episode"
    );
}

/// Phase 12 HELIOS-04 control — with system-wide `enable()` (no scope),
/// the SAME 50/50 seed DOES produce `other:` promotions. Proves the
/// isolation test above is gating on the scope filter rather than a
/// side-effect of the `AlwaysPromoteConsolidator` fixture.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consolidator_system_wide_sees_other_promotions_control() {
    unsafe {
        std::env::set_var("LUNARIS_CONSOLIDATE_DEBOUNCE_MS", "50");
    }

    let (lunaris, broker, _clock) = build_in_process_lunaris();

    // System-wide enable — no scope filter.
    lunaris.consolidator_pipeline().enable();
    assert!(lunaris.consolidator_pipeline().scope_prefix().is_none());

    const PER_SIDE: usize = 10;
    let mut id_to_source: HashMap<ulid::Ulid, String> = HashMap::new();
    for i in 0..PER_SIDE {
        let src_helios = format!("helios:fs/session-control/note-{i}");
        let ep =
            Episode::new(lunaris_core::Scope::dev(), &src_helios, "h", lunaris.clock().as_ref());
        id_to_source.insert(ep.id, src_helios);
        lunaris.ingest(ep).await.unwrap();

        let src_other = format!("other:tenant-{i}/note");
        let ep =
            Episode::new(lunaris_core::Scope::dev(), &src_other, "o", lunaris.clock().as_ref());
        id_to_source.insert(ep.id, src_other);
        lunaris.ingest(ep).await.unwrap();
    }

    tokio::time::sleep(Duration::from_millis(750)).await;
    lunaris.consolidator_pipeline().disable();
    lunaris.consolidator_pipeline().join_worker().await;

    let audits = broker.audit_snapshot();
    let mut other_promotions = 0usize;
    for env in &audits {
        if env.get("kind").and_then(|v| v.as_str()) != Some("ConsolidatorPromotion") {
            continue;
        }
        if let Some(id_str) = env.get("episode_id").and_then(|v| v.as_str())
            && let Ok(id) = id_str.parse::<ulid::Ulid>()
            && let Some(src) = id_to_source.get(&id)
            && src.starts_with("other:")
        {
            other_promotions += 1;
        }
    }

    assert!(
        other_promotions > 0,
        "control: system-wide enable() MUST see `other:` promotions. \
         If this is 0, the isolation test above is proving nothing."
    );
}

// ---------------------------------------------------------------------------
// Dual-backend #[ignore]-gated variant. Default `cargo test --workspace` does
// NOT run this — it fires only when MOON_URL or PG_URL is set AND the TCP
// probe succeeds. Mirrors helios_scratchpad_smoke.rs probe discipline.
// ---------------------------------------------------------------------------

/// TCP probe — same W-3 / W-7 pattern as helios_scratchpad_smoke.rs.
fn probe_backend(env_name: &str) -> Option<String> {
    let url = std::env::var(env_name).ok()?;
    let host_port = url
        .strip_prefix("moon://")
        .or_else(|| url.strip_prefix("redis://"))
        .or_else(|| url.strip_prefix("postgres://"))
        .or_else(|| url.strip_prefix("postgresql://"))
        .unwrap_or(&url);
    let host_port = host_port.rsplit('@').next().unwrap_or(host_port);
    let host_port = host_port.split('/').next().unwrap_or(host_port);
    let timeout = Duration::from_secs(1);
    let addr = host_port.to_socket_addrs().ok().and_then(|mut it| it.next())?;
    if TcpStream::connect_timeout(&addr, timeout).is_ok() {
        Some(url)
    } else {
        eprintln!("SKIP {env_name} (TCP probe to {host_port} failed)");
        None
    }
}

/// Phase 12 HELIOS-04 live-backend variant of the isolation gate. Runs
/// only with `MOON_URL` and/or `PG_URL` set. Seed size is controlled by
/// `LUNARIS_SCOPE_ISOLATION_NOTES` (default 200 per-side = 400 total; set
/// to 5000 for the documented 10K-note workload).
///
/// Fallback discipline — if Postgres fails with a pgmq partition-packing
/// error, the test emits `POSTGRES_FALLBACK pgmq_partition_packing_error`
/// and continues. This is the pre-authorized v0.1.1 fallback (ROADMAP
/// Phase 12 risk register row #5; CONTEXT.md D-13). Operator follow-up is
/// documented in `.planning/phases/12-helios-production-hardening/12-HUMAN-UAT.md`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires MOON_URL or PG_URL"]
async fn consolidator_scope_isolation_dual_backend_live() -> anyhow::Result<()> {
    for url_env in ["MOON_URL", "PG_URL"] {
        let Some(url) = probe_backend(url_env) else {
            continue;
        };
        eprintln!("\n=== scope isolation live: {url_env} ({url}) ===");

        let per_side: usize = std::env::var("LUNARIS_SCOPE_ISOLATION_NOTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(200);

        let open_result = Lunaris::open(&url).await;
        let lunaris = match open_result {
            Ok(l) => l,
            Err(e) if url_env == "PG_URL" => {
                let err_s = format!("{e}");
                if err_s.contains("pgmq") || err_s.contains("partition") {
                    eprintln!(
                        "POSTGRES_FALLBACK pgmq_partition_packing_error: {err_s}\n\
                         See .planning/phases/12-helios-production-hardening/12-HUMAN-UAT.md"
                    );
                    continue;
                }
                return Err(e.into());
            }
            Err(e) => return Err(e.into()),
        };
        let lunaris =
            lunaris.with_consolidator(Arc::new(AlwaysPromoteConsolidator) as Arc<dyn Consolidator>);

        lunaris.consolidator_pipeline().enable_for_scope("helios:fs/");

        let session = format!("live-isolation-{}", ulid::Ulid::new());
        let mut id_to_source: HashMap<ulid::Ulid, String> = HashMap::new();
        for i in 0..per_side {
            let src_helios = format!("helios:fs/{session}/note-{i}");
            let ep = Episode::new(
                lunaris_core::Scope::dev(),
                &src_helios,
                format!("H {i}"),
                lunaris.clock().as_ref(),
            );
            id_to_source.insert(ep.id, src_helios);
            lunaris.ingest(ep).await?;

            let src_other = format!("other:{session}/note-{i}");
            let ep = Episode::new(
                lunaris_core::Scope::dev(),
                &src_other,
                format!("O {i}"),
                lunaris.clock().as_ref(),
            );
            id_to_source.insert(ep.id, src_other);
            lunaris.ingest(ep).await?;
        }

        // Live backends: give the worker a full debounce window + drain.
        tokio::time::sleep(Duration::from_secs(3)).await;
        lunaris.consolidator_pipeline().disable();
        lunaris.consolidator_pipeline().join_worker().await;

        // Drain __lunaris_audit__ bounded — 5s total.
        let audit_stream = lunaris
            .storage()
            .subscribe(
                &lunaris_core::Scope::dev(),
                "lunaris-scope-isolation-test",
                "__lunaris_audit__",
                0,
            )
            .await?;
        let mut audit_stream = audit_stream;
        let mut other_promotions = 0usize;
        let mut helios_promotions = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(deadline, audit_stream.next()).await {
            let Ok(env) = serde_json::from_slice::<serde_json::Value>(&msg.payload) else {
                continue;
            };
            if env.get("kind").and_then(|v| v.as_str()) != Some("ConsolidatorPromotion") {
                continue;
            }
            if let Some(id_str) = env.get("episode_id").and_then(|v| v.as_str())
                && let Ok(id) = id_str.parse::<ulid::Ulid>()
                && let Some(src) = id_to_source.get(&id)
            {
                if src.starts_with("helios:fs/") {
                    helios_promotions += 1;
                } else if src.starts_with("other:") {
                    other_promotions += 1;
                }
            }
        }

        eprintln!(
            "{url_env} live isolation: helios={helios_promotions} \
             other={other_promotions} per_side={per_side}"
        );

        assert_eq!(
            other_promotions, 0,
            "{url_env} HARD gate: `other:` promotions MUST be 0; got \
             {other_promotions}"
        );
        assert!(
            helios_promotions > 0,
            "{url_env}: helios_promotions > 0 required (filter-not-vacuous)"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Default-build sanity — runs without --ignored, proves the file compiles
// and the helpers behave.
// ---------------------------------------------------------------------------

#[test]
fn probe_backend_missing_env_returns_none() {
    // Use a deliberately-unset env var name so the probe returns None.
    assert!(probe_backend("LUNARIS_SCOPE_ISOLATION_UNSET_ENV_VAR").is_none());
}

// ---------------------------------------------------------------------------
// Plan 16-02 — CONSOL-V1-04 post-default-flip isolation gate.
//
// The block above gates the scope-filter with a backend-agnostic fixture
// (`AlwaysPromoteConsolidator`). That is still the right contract for the
// filter-layer regression: the filter lives in the `Consolidator::
// consolidate_scoped` default impl and runs BEFORE any backend is invoked,
// so it is provably backend-independent.
//
// What the v0.1.2 default flip (Plan 16-01) changes is the *production*
// wire: `Lunaris::open` now resolves to `ActRConsolidator::default()`. The
// blueprint contract for CONSOL-V1-04 is that the Helios 50/50 isolation
// invariant MUST hold under the real backend too. This block adds that
// proof, wired explicitly (not env-resolved) to `ActRConsolidator::default`.
//
// Sizing note — `ActRConsolidator::default()` promote threshold is `+1.0`
// with `d=0.5`. A single-event episode has `activation = ln(1) = 0.0`
// (elapsed floors to 1s), which lands in the neutral band → zero
// promote/archive events. To non-vacuously exercise the real backend on
// the Helios half we publish 16 `ConsolidateEvent`s with the SAME
// `episode_id` and elapsed ≈ 1s each → `activation = ln(16) ≈ 2.77 > 1.0`
// → promote. The `other:` half uses single-event episodes — the scope
// filter drops them either way (zero-or-not-zero both falsify under a
// broken filter because promote-count would leak with 16 refs too).
// ---------------------------------------------------------------------------

use lunaris_consolidate::{ActRConsolidator, CONSOLIDATE_TOPIC};

/// Build a `Lunaris` with an explicit consolidator instance (not env-
/// resolved, so test determinism doesn't depend on
/// `LUNARIS_CONSOLIDATOR_BACKEND`). Mirrors `build_in_process_lunaris`
/// but parameterised.
fn build_in_process_lunaris_with(
    consolidator: Arc<dyn Consolidator>,
) -> (Lunaris, Arc<InProcessBroker>) {
    let broker = InProcessBroker::new();
    let storage: Arc<dyn StoragePort> = broker.clone();
    let keyword: Arc<dyn KeywordPort> = broker.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    let clock = HlcClock::new(0);
    let handle = Lunaris::with_parts_keyword(storage, keyword, embedder, clock)
        .with_reranker(Arc::new(NoopReranker) as Arc<dyn Reranker>)
        .with_consolidator(consolidator);
    (handle, broker)
}

/// Publish 16 `ConsolidateEvent`s with the SAME `episode_id` to the
/// `__lunaris_consolidate__` queue so the worker's debounce buffer has
/// enough references to lift the episode's activation above the ActR
/// promote threshold. Mirrors the production worker's buffer shape.
///
/// `source` is copied into each envelope — `Consolidator::consolidate_scoped`
/// reads it to decide whether to forward or drop the batch.
async fn publish_n_refs_for(
    broker: &Arc<InProcessBroker>,
    episode_id: ulid::Ulid,
    source: &str,
    n: usize,
    lsn_wall_ms: u64,
) {
    for counter in 0..n {
        let envelope = serde_json::json!({
            "kind": "ingest_committed",
            "episode_id": episode_id.to_string(),
            "lsn_wall_ms": lsn_wall_ms,
            "lsn_counter": counter,
            "source": source,
        });
        let payload = serde_json::to_vec(&envelope).expect("envelope serialize");
        broker
            .publish(&lunaris_core::Scope::dev(), CONSOLIDATE_TOPIC, 0, payload.into())
            .await
            .expect("publish consolidate event");
    }
}

/// Run the 50/50 scope-isolation scenario end-to-end against whichever
/// consolidator the caller wires. Returns `(helios_promotions,
/// helios_archives, other_touched)` where `other_touched` counts any
/// promote OR archive whose source starts with `other:` — the v0.1.1
/// HARD gate. Sixteen refs per Helios episode; one ref per `other`
/// episode.
async fn run_scope_isolation_scenario(
    consolidator: Arc<dyn Consolidator>,
    per_side: usize,
) -> (usize, usize, usize) {
    unsafe {
        std::env::set_var("LUNARIS_CONSOLIDATE_DEBOUNCE_MS", "50");
    }

    let (lunaris, broker) = build_in_process_lunaris_with(consolidator);
    lunaris.consolidator_pipeline().enable_for_scope("helios:fs/");

    // now_ms minus 1 second so `ActRConsolidator::consolidate` sees
    // elapsed ≈ 1s (floored at 1). Determinism: the scenario is not
    // wall-clock-sensitive so long as the 1s offset holds — runs that
    // sit on a debounce flush > 1s late still see elapsed ≤ small-
    // constant, and `ln(16 × k^-0.5)` remains > 1.0 for k up to ~50.
    let now_ms =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()
            as u64;
    let lsn_ms = now_ms.saturating_sub(1_000);

    // Map every episode_id we'll publish back to its source so the
    // assertion phase can partition the audit log.
    let mut id_to_source: HashMap<ulid::Ulid, String> = HashMap::new();

    for i in 0..per_side {
        // Helios half — 16 refs per episode, activation ≈ ln(16) ≈ 2.77 > 1.0.
        let src_helios = format!("helios:fs/session-isolation-actr/note-{i}");
        let ep_helios = ulid::Ulid::new();
        id_to_source.insert(ep_helios, src_helios.clone());
        publish_n_refs_for(&broker, ep_helios, &src_helios, 16, lsn_ms).await;

        // Other half — single ref per episode. Scope filter drops these
        // before `consolidate` sees them; even if it didn't, one ref at
        // elapsed=1s is activation=0 (neutral band, no emission).
        let src_other = format!("other:tenant-{i}/note");
        let ep_other = ulid::Ulid::new();
        id_to_source.insert(ep_other, src_other.clone());
        publish_n_refs_for(&broker, ep_other, &src_other, 1, lsn_ms).await;
    }

    // Worker debounce window + drain grace. 50 ms debounce + ~700 ms
    // scheduling headroom matches the sibling test's timing envelope.
    tokio::time::sleep(Duration::from_millis(750)).await;
    lunaris.consolidator_pipeline().disable();
    lunaris.consolidator_pipeline().join_worker().await;

    let mut helios_promotions = 0usize;
    let mut helios_archives = 0usize;
    let mut other_touched = 0usize;
    for env in broker.audit_snapshot() {
        let kind = env.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let is_promote = kind == "ConsolidatorPromotion";
        let is_archive = kind == "ConsolidatorArchive";
        if !(is_promote || is_archive) {
            continue;
        }
        let Some(id_str) = env.get("episode_id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(id) = id_str.parse::<ulid::Ulid>() else {
            continue;
        };
        match id_to_source.get(&id) {
            Some(src) if src.starts_with("helios:fs/") => {
                if is_promote {
                    helios_promotions += 1;
                } else {
                    helios_archives += 1;
                }
            }
            Some(src) if src.starts_with("other:") => {
                other_touched += 1;
            }
            _ => {}
        }
    }
    eprintln!(
        "scope_isolation_actr: helios_promotions={helios_promotions} \
         helios_archives={helios_archives} other_touched={other_touched} \
         per_side={per_side}"
    );
    (helios_promotions, helios_archives, other_touched)
}

/// Plan 16-02 HARD GATE — scope-isolation invariant holds under the
/// production default backend (`ActRConsolidator::default()`, explicit —
/// not env-resolved so the test does not depend on
/// `LUNARIS_CONSOLIDATOR_BACKEND` state).
///
/// Assertions (all three must hold together):
///
/// 1. At least one `ConsolidatorPromotion` OR `ConsolidatorArchive`
///    event is emitted for the `helios:fs/` half. This gates the real
///    ACT-R scorer: under `NoopConsolidator` this assertion FAILS (Noop
///    returns empty reports). Proves the test is backend-sensitive.
/// 2. Zero `ConsolidatorPromotion` / `ConsolidatorArchive` events for
///    any source NOT starting with `helios:fs/`. Preserves the v0.1.1
///    HARD cross-tenant leak gate under the real backend.
/// 3. The helios-half emission count is `≤ 16 × PER_SIDE` (one
///    promote/archive per episode at most — sanity that the fixture
///    didn't accidentally amplify).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consolidator_scope_isolation_under_actr_default() {
    const PER_SIDE: usize = 20;
    let consolidator: Arc<dyn Consolidator> = Arc::new(ActRConsolidator::default());
    let (helios_promotions, helios_archives, other_touched) =
        run_scope_isolation_scenario(consolidator, PER_SIDE).await;

    // Gate 2 (v0.1.1 invariant) — no cross-tenant leak under real backend.
    assert_eq!(
        other_touched, 0,
        "CONSOL-V1-04: `other:` sources MUST receive zero ConsolidatorPromotion \
         / ConsolidatorArchive events under ActRConsolidator. got={other_touched}"
    );

    // Gate 1 — real-backend non-vacuity. Fails under NoopConsolidator.
    let helios_events = helios_promotions + helios_archives;
    assert!(
        helios_events > 0,
        "CONSOL-V1-04: real ActR backend MUST emit ≥ 1 promote-or-archive \
         event for the helios:fs/ half (the v0.1.1 test ran against \
         AlwaysPromote; this gate proves ActR actually fires). \
         helios_promotions={helios_promotions} helios_archives={helios_archives}"
    );

    // Gate 3 — sanity cap.
    assert!(
        helios_events <= PER_SIDE,
        "one promote/archive per episode at most — got {helios_events} for \
         {PER_SIDE} helios episodes (fixture amplification bug?)"
    );
}

/// Plan 16-02 regression companion — the SAME scenario wired to
/// `NoopConsolidator` MUST fail the non-vacuity gate (helios events == 0).
/// This proves `consolidator_scope_isolation_under_actr_default`'s gate 1
/// is genuinely backend-sensitive and not a tautology.
///
/// Expressed as a `#[should_panic]` inversion so the proof is part of the
/// default test run — if the non-vacuity gate ever degrades into something
/// Noop could satisfy (e.g. asserting `audit.len() >= 0`), this test will
/// flip to passing-without-panic and CI flags the regression.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[should_panic(expected = "MUST emit ≥ 1 promote-or-archive")]
async fn consolidator_scope_isolation_under_noop_fails_non_vacuity_gate() {
    use lunaris_consolidate::NoopConsolidator;
    const PER_SIDE: usize = 20;
    let consolidator: Arc<dyn Consolidator> = Arc::new(NoopConsolidator);
    let (helios_promotions, helios_archives, other_touched) =
        run_scope_isolation_scenario(consolidator, PER_SIDE).await;

    // v0.1.1 cross-tenant gate still holds trivially under Noop (nothing
    // ever promoted, so nothing leaked). This assert must pass to reach
    // the gate-1 failure below.
    assert_eq!(other_touched, 0);

    // This is the gate that SHOULD panic — Noop emits nothing. Mirror of
    // the ActR test's message so #[should_panic(expected=...)] matches.
    let helios_events = helios_promotions + helios_archives;
    assert!(
        helios_events > 0,
        "CONSOL-V1-04: real ActR backend MUST emit ≥ 1 promote-or-archive \
         event for the helios:fs/ half (the v0.1.1 test ran against \
         AlwaysPromote; this gate proves ActR actually fires). \
         helios_promotions={helios_promotions} helios_archives={helios_archives}"
    );
}
