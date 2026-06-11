//! Phase 14.1 integration tests — `apply_reflect_invalidate` MVCC stamping.
//!
//! ## Test plan
//!
//! Two backends tested in parallel sections:
//!
//! 1. **Moon** — gated on `MOON_URL` env var (TCP-probed); SKIPs cleanly if
//!    unset / unreachable.
//! 2. **Postgres** — gated on `LUNARIS_TEST_POSTGRES_URL` env var (TCP-probed);
//!    SKIPs cleanly if unset / unreachable.
//!
//! Each backend test:
//!
//! - Ingests two raw facts via `atomic_write` (bypasses the LLM pipeline so
//!   no embedder / extractor is required).
//! - Calls `apply_reflect_invalidate` with ONE of the two fact ulids.
//! - Asserts:
//!   a. The stamped ulid's persisted payload has `bt["sys"][1]` non-null.
//!   b. The non-stamped ulid's payload has `bt["sys"][1]` null (still live).
//!   c. `apply_reflect_invalidate` returns exactly the one stamped ulid.
//!   d. `apply_reflect_invalidate` issued exactly ONE `atomic_write` (D-11).
//!   (Verified indirectly: calling it a second time for the same ulid
//!   returns empty — idempotency guard proves the first write went through.)
//! - Asserts that a second call for the already-invalidated ulid returns
//!   empty (idempotency guard).
//!
//! The test does NOT assert on audit events published to `__lunaris_audit__`
//! because polling a subscribe stream adds significant complexity without
//! adding correctness signal beyond what the MVCC byte assertion covers.
//! The audit path is unit-tested in `lunaris-verify/src/reflect_apply.rs`.

#![forbid(unsafe_code)]

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use lunaris_core::{BiTemporal, Hlc, HlcClock, Scope, StoragePort, WriteOp};
use lunaris_verify::apply_reflect_invalidate;
use ulid::Ulid;

// ---------------------------------------------------------------------------
// Shared probe helper (verbatim Shared Pattern 2 per codebase convention)
// ---------------------------------------------------------------------------

fn probe_backend(name: &str, url: Option<String>) -> Option<String> {
    let url = url?;
    let host_port = if let Some(rest) = url.strip_prefix("moon://") {
        rest.split('/').next()?.to_string()
    } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        let after_scheme = url.split("://").nth(1)?;
        let authority = after_scheme.split('/').next()?;
        let bare = authority.rsplit('@').next()?;
        if bare.contains(':') { bare.to_string() } else { format!("{bare}:5432") }
    } else {
        eprintln!("phase_14_1: SKIP {name} (unknown URL scheme)");
        return None;
    };
    let timeout = Duration::from_secs(1);
    let addr = match host_port.to_socket_addrs().ok().and_then(|mut it| it.next()) {
        Some(a) => a,
        None => {
            eprintln!("phase_14_1: SKIP {name} (DNS resolution of {host_port} failed)");
            return None;
        }
    };
    match TcpStream::connect_timeout(&addr, timeout) {
        Ok(_) => Some(url),
        Err(_) => {
            eprintln!(
                "phase_14_1: SKIP {name} (TCP probe to {host_port} failed within {}ms)",
                timeout.as_millis()
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Shared test body — runs against any StoragePort impl
// ---------------------------------------------------------------------------

/// Seed a fact row with `bt.sys.1 = None` (live) at a given key.
async fn seed_fact(storage: &Arc<dyn StoragePort>, scope: &Scope, key: Vec<u8>, ulid: Ulid) {
    let clock = HlcClock::new(0);
    let bt = BiTemporal::now(&clock);
    let payload = serde_json::json!({
        "fact_text": format!("test fact {ulid}"),
        "ulid": ulid.to_string(),
        "bt": serde_json::to_value(bt).unwrap(),
    });
    let value = serde_json::to_vec(&payload).unwrap();
    storage
        .atomic_write(scope, &[WriteOp::KvPut { key, value }])
        .await
        .expect("seed_fact atomic_write");
}

async fn run_reflect_invalidate_test(storage: Arc<dyn StoragePort>, label: &str) {
    let scope = Scope::new(format!("phase-14-1-test-{}", Ulid::new())).expect("valid scope");
    // Use a single shared clock so that bt.sys.0 (ingest) and bt.sys.1
    // (invalidation) are stamped from the same monotonic sequence — avoids
    // sys.0 > sys.1 if both happen within the same wall-ms.
    let clock = HlcClock::new(0);

    let ulid_a = Ulid::new();
    let ulid_b = Ulid::new();
    let key_a = format!("fact:{ulid_a}").into_bytes();
    let key_b = format!("fact:{ulid_b}").into_bytes();

    // Seed both facts.
    seed_fact(&storage, &scope, key_a.clone(), ulid_a).await;
    seed_fact(&storage, &scope, key_b.clone(), ulid_b).await;

    // --- Call apply_reflect_invalidate for ulid_a only ---
    let stamped = apply_reflect_invalidate(&storage, &scope, &clock, None, &[ulid_a])
        .await
        .expect("invalidate");

    assert_eq!(stamped, vec![ulid_a], "{label}: stamped ulids must contain only ulid_a");

    // Assert ulid_a is now invalidated (bt.sys.1 non-null).
    let now = clock.tick();
    let row_a = storage
        .read_as_of(&scope, &key_a, now)
        .await
        .expect("read_as_of ulid_a")
        .expect("row_a must exist");
    let payload_a: serde_json::Value = serde_json::from_slice(&row_a.value).expect("parse row_a");
    assert!(
        !payload_a["bt"]["sys"][1].is_null(),
        "{label}: ulid_a bt.sys[1] must be non-null after invalidation; got bt={}",
        payload_a["bt"]
    );
    assert!(row_a.bt.sys.1.is_some(), "{label}: row_a.bt.sys.1 must be Some after invalidation");

    // Assert ulid_b is still live (bt.sys.1 null).
    let row_b = storage
        .read_as_of(&scope, &key_b, now)
        .await
        .expect("read_as_of ulid_b")
        .expect("row_b must exist");
    let payload_b: serde_json::Value = serde_json::from_slice(&row_b.value).expect("parse row_b");
    assert!(
        payload_b["bt"]["sys"][1].is_null(),
        "{label}: ulid_b bt.sys[1] must be null (still live); got bt={}",
        payload_b["bt"]
    );
    assert!(row_b.bt.sys.1.is_none(), "{label}: row_b.bt.sys.1 must be None (still live)");

    // Idempotency: second call for ulid_a returns empty (already stamped).
    let stamped_again = apply_reflect_invalidate(&storage, &scope, &clock, None, &[ulid_a])
        .await
        .expect("invalidate #2");
    assert!(
        stamped_again.is_empty(),
        "{label}: second invalidation of already-stamped ulid must return empty"
    );

    eprintln!("phase_14_1 [{label}]: PASS");
}

// ---------------------------------------------------------------------------
// Moon test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reflect_invalidate_moon() -> anyhow::Result<()> {
    let url = match probe_backend("MOON_URL", std::env::var("MOON_URL").ok()) {
        Some(u) => u,
        None => return Ok(()), // SKIP
    };

    let storage: Arc<dyn StoragePort> =
        Arc::new(lunaris_storage_moon::MoonStorage::connect(&url).await?);

    run_reflect_invalidate_test(storage, "moon").await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Postgres parity test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reflect_invalidate_postgres() -> anyhow::Result<()> {
    let url = match probe_backend(
        "LUNARIS_TEST_POSTGRES_URL",
        std::env::var("LUNARIS_TEST_POSTGRES_URL").ok(),
    ) {
        Some(u) => u,
        None => return Ok(()), // SKIP
    };

    let storage: Arc<dyn StoragePort> =
        Arc::new(lunaris_storage_postgres::PostgresStorage::connect(&url).await?);

    run_reflect_invalidate_test(storage, "postgres").await;
    Ok(())
}

// ---------------------------------------------------------------------------
// ScopedLunaris::end_turn integration smoke (in-memory, no live backend)
// ---------------------------------------------------------------------------
//
// Validates that `ScopedLunaris::end_turn` wires through to
// `apply_reflect_invalidate` using an in-memory RecordingStorage. The
// test installs a stub `ReflectSupervisor` that returns a known
// `ReflectOutput` (invalidate = [ulid_a]) and asserts the recording
// storage received exactly one `atomic_write` for that ulid.

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris::Lunaris;
use lunaris_core::{
    CypherDialect, CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, StorageCapabilities,
    StorageError, StubEmbedder, VectorHit,
};
use lunaris_verify::{ReflectInput, ReflectOutput, ReflectSupervisor};
use parking_lot::Mutex;
use std::collections::HashMap;

#[derive(Default)]
struct CapturingStorage {
    rows: Mutex<HashMap<Vec<u8>, Row<Bytes>>>,
    write_batches: Mutex<Vec<Vec<WriteOp>>>,
    publishes: Mutex<Vec<Vec<u8>>>,
}

#[async_trait]
impl StoragePort for CapturingStorage {
    async fn atomic_write(&self, _s: &Scope, ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        {
            let mut rows = self.rows.lock();
            for op in ops {
                if let WriteOp::KvPut { key, value } = op {
                    let bt: BiTemporal = serde_json::from_slice::<serde_json::Value>(value)
                        .ok()
                        .and_then(|v| serde_json::from_value(v["bt"].clone()).ok())
                        .unwrap_or(BiTemporal::at(Hlc::ZERO, Hlc::ZERO));
                    rows.insert(
                        key.clone(),
                        Row { key: key.clone(), value: value.clone().into(), bt },
                    );
                }
            }
        }
        self.write_batches.lock().push(ops.to_vec());
        Ok(Lsn { wall_ms: 1, counter: self.write_batches.lock().len() as u32 })
    }
    async fn read_as_of(
        &self,
        _s: &Scope,
        key: &[u8],
        _t: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Ok(self.rows.lock().get(key).cloned())
    }
    async fn publish(
        &self,
        _s: &Scope,
        _t: &str,
        _p: u16,
        payload: Bytes,
    ) -> Result<u64, StorageError> {
        self.publishes.lock().push(payload.to_vec());
        Ok(self.publishes.lock().len() as u64)
    }
    #[allow(clippy::too_many_arguments)]
    async fn vector_search(
        &self,
        _s: &Scope,
        _i: &str,
        _q: &[f32],
        _k: usize,
        _f: Option<&Filter>,
        _t: Option<Hlc>,
        _r: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        Ok(vec![])
    }
    async fn graph_traverse(
        &self,
        _s: &Scope,
        _q: &CypherQuery,
        _t: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Ok(GraphResult::default())
    }
    async fn scan_range(
        &self,
        _s: &Scope,
        _p: &[u8],
        _t: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        use futures::stream;
        Ok(Box::pin(stream::empty()))
    }
    async fn subscribe(
        &self,
        _s: &Scope,
        _g: &str,
        _t: &str,
        _p: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("CapturingStorage::subscribe"))
    }
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: true,
            graph_native: false,
            rerank_native: false,
            queue_native: false,
            max_vector_dim: 768,
            native_rrf: false,
            max_scopes_recommended: 0,
            cypher_dialect: CypherDialect::Legacy,
            graph_decay_native: false,
            graph_navigate_native: false,
        }
    }
}

/// Stub reflect supervisor that returns a fixed `ReflectOutput`.
#[derive(Clone)]
struct StubReflectSupervisor {
    output: ReflectOutput,
}

#[async_trait]
impl ReflectSupervisor for StubReflectSupervisor {
    async fn reflect(
        &self,
        _input: ReflectInput,
    ) -> Result<ReflectOutput, lunaris_core::LunarisError> {
        Ok(self.output.clone())
    }
}

#[tokio::test]
async fn scoped_end_turn_applies_invalidate_in_memory() {
    use lunaris_core::HlcClock;

    let cs = Arc::new(CapturingStorage::default());
    let storage: Arc<dyn StoragePort> = cs.clone();
    let clock = HlcClock::new(0);
    let embedder: Arc<dyn lunaris_core::Embedder> = Arc::new(StubEmbedder::new(4));

    let ulid_a = Ulid::new();
    let ulid_b = Ulid::new();
    let key_a = format!("fact:{ulid_a}").into_bytes();
    let key_b = format!("fact:{ulid_b}").into_bytes();

    // Seed both rows directly.
    let now = clock.tick();
    let bt_live = BiTemporal::at(now, now);
    for (key, ulid) in [(&key_a, ulid_a), (&key_b, ulid_b)] {
        let payload = serde_json::json!({ "ulid": ulid.to_string(), "bt": serde_json::to_value(bt_live).unwrap() });
        cs.rows.lock().insert(
            key.clone(),
            Row {
                key: key.clone(),
                value: serde_json::to_vec(&payload).unwrap().into(),
                bt: bt_live,
            },
        );
    }

    // Build a Lunaris handle with the capturing storage.
    let handle = Lunaris::with_parts(storage, embedder, clock.clone()).with_reflect_supervisor(
        Arc::new(StubReflectSupervisor {
            output: ReflectOutput { invalidate: vec![ulid_a], boost: vec![], pre_warm_query: None },
        }),
    );

    let scope = Scope::new("test-scope").unwrap();
    let scoped = handle.scoped(scope.clone());

    // Before end_turn: exactly 0 atomic_writes (the rows were inserted directly).
    assert_eq!(cs.write_batches.lock().len(), 0);

    let turn_id = Ulid::new();
    let output = scoped
        .end_turn(ReflectInput {
            turn_id: Some(turn_id),
            turn_summary: "test turn".to_string(),
            recent_fact_ids: vec![ulid_a, ulid_b],
            recent_chunk_ids: vec![],
        })
        .await
        .expect("end_turn");

    // Output carries both invalidate and boost/pre_warm_query fields.
    assert_eq!(output.invalidate, vec![ulid_a]);
    assert!(output.boost.is_empty());
    assert!(output.pre_warm_query.is_none());

    // D-11: exactly one atomic_write for the invalidation batch.
    assert_eq!(
        cs.write_batches.lock().len(),
        1,
        "ScopedLunaris::end_turn must produce exactly one atomic_write"
    );

    // Assert ulid_a is stamped (bt.sys.1 non-null in stored bytes).
    let now2 = clock.tick();
    let row_a = cs.rows.lock().get(&key_a).cloned().expect("row_a exists");
    let v_a: serde_json::Value = serde_json::from_slice(&row_a.value).unwrap();
    assert!(
        !v_a["bt"]["sys"][1].is_null(),
        "ulid_a bt.sys[1] must be non-null after end_turn invalidation; got {}",
        v_a["bt"]
    );

    // Assert ulid_b is still live.
    let row_b = cs.rows.lock().get(&key_b).cloned().expect("row_b exists");
    let v_b: serde_json::Value = serde_json::from_slice(&row_b.value).unwrap();
    assert!(
        v_b["bt"]["sys"][1].is_null(),
        "ulid_b bt.sys[1] must be null (not invalidated); got {}",
        v_b["bt"]
    );

    let _ = now2; // suppress unused warning
}

#[tokio::test]
async fn scoped_end_turn_noop_supervisor_no_write() {
    let cs = Arc::new(CapturingStorage::default());
    let storage: Arc<dyn StoragePort> = cs.clone();
    let clock = HlcClock::new(0);
    let embedder: Arc<dyn lunaris_core::Embedder> = Arc::new(StubEmbedder::new(4));

    let handle = Lunaris::with_parts(storage, embedder, clock);
    // Default supervisor is NoopReflectSupervisor → empty output → no write.
    let scope = Scope::new("test-noop").unwrap();
    let scoped = handle.scoped(scope);
    let output = scoped
        .end_turn(ReflectInput {
            turn_id: None,
            turn_summary: "noop turn".to_string(),
            recent_fact_ids: vec![Ulid::new()],
            recent_chunk_ids: vec![],
        })
        .await
        .expect("end_turn noop");

    assert_eq!(output, ReflectOutput::default());
    assert_eq!(cs.write_batches.lock().len(), 0, "noop supervisor must produce no writes");
}
