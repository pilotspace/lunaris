//! Integration proof: a seeded SQLite store migrates into a real (ephemeral)
//! Moon with the counts, content, and skips the contract promises.
//!
//! The destination comes from `lunaris-test-harness`, so this file is green
//! under both `LUNARIS_TEST_BACKEND` policies: `moon` spawns a disposable
//! child-process Moon, `memory` resolves the embedded backend. Every assertion
//! is backend-agnostic on purpose — the migration contract is about what a
//! reader sees through `StoragePort`, not about Moon's wire format.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::StreamExt;
use lunaris_core::bitemporal::BiTemporal;
use lunaris_core::hlc::{Hlc, HlcClock};
use lunaris_core::keyspace::{chunk_key, episode_key, fact_key, scope_prefix};
use lunaris_core::primitives::{Episode, Fact};
use lunaris_core::storage::WriteOp;
use lunaris_core::{Scope, StoragePort};
use lunaris_migrate::{
    MigrateError, MigrationOptions, ScopeReport, discover_scopes, migrate_scope, verify_scope,
};
use lunaris_storage_embedded::EmbeddedStorage;
use lunaris_test_harness::{TestStorage, open_test_storage};
use ulid::Ulid;

const SCOPE: &str = "migrate-it";

/// What `seed` put in the source, so the assertions read as a spec.
struct Seeded {
    _dir: tempfile::TempDir,
    source: Arc<dyn StoragePort>,
    scope: Scope,
    /// Keys that MUST arrive on the destination.
    expected: Vec<Vec<u8>>,
    /// Keys that MUST NOT arrive (closed intervals + malformed key).
    excluded: Vec<Vec<u8>>,
}

fn closed(clock: &HlcClock, close_valid: bool) -> BiTemporal {
    let t = clock.tick();
    let end = Hlc { wall_ms: t.wall_ms + 1_000, counter: 0, node_id: t.node_id };
    if close_valid {
        BiTemporal { valid: (t, Some(end)), sys: (t, None) }
    } else {
        BiTemporal { valid: (t, None), sys: (t, Some(end)) }
    }
}

fn fact(scope: &Scope, text: &str, bt: BiTemporal) -> Fact {
    Fact {
        id: Ulid::new(),
        scope: scope.clone(),
        subject: Ulid::new(),
        predicate: "employer".to_owned(),
        object: Ulid::new(),
        fact_text: text.to_owned(),
        embedding: Some(vec![0.25; 8]),
        bt,
        confidence: 0.9,
        provenance: Vec::new(),
        activation: 1.0,
    }
}

/// Seed a real file-backed SQLite store: 2 episodes, 1 chunk, 2 open facts,
/// 1 fact with a closed VALID interval, 1 fact with a closed SYS interval, and
/// one malformed key under the scope prefix.
async fn seed() -> Seeded {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("source.db");
    let url = format!("sqlite://{}", path.display());
    let source = EmbeddedStorage::connect(&url).await.expect("open sqlite source");
    let scope = Scope::new(SCOPE).expect("valid scope");
    let clock = HlcClock::new(7);

    let mut ops: Vec<WriteOp> = Vec::new();
    let mut expected: Vec<Vec<u8>> = Vec::new();
    let mut excluded: Vec<Vec<u8>> = Vec::new();

    for i in 0..2 {
        let ep = Episode::new(scope.clone(), "test", format!("episode body {i}"), &clock);
        let key = episode_key(&scope, ep.id);
        ops.push(WriteOp::KvPut {
            key: key.clone(),
            value: serde_json::to_vec(&ep).expect("serialize episode"),
        });
        expected.push(key);
    }

    // A chunk-kind row (embeddable) written as raw JSON so the fixture does not
    // depend on Chunk's full field set — classification only reads `bt`.
    {
        let id = Ulid::new();
        let key = chunk_key(&scope, id);
        let bt = BiTemporal::now(&clock);
        let value = serde_json::json!({ "id": id.to_string(), "text": "hello", "bt": bt });
        ops.push(WriteOp::KvPut {
            key: key.clone(),
            value: serde_json::to_vec(&value).expect("serialize chunk"),
        });
        expected.push(key);
    }

    for i in 0..2 {
        let f = fact(&scope, &format!("open fact {i}"), BiTemporal::now(&clock));
        let key = fact_key(&scope, f.id);
        ops.push(WriteOp::KvPut {
            key: key.clone(),
            value: serde_json::to_vec(&f).expect("serialize fact"),
        });
        expected.push(key);
    }

    let retracted = fact(&scope, "retracted fact", closed(&clock, true));
    let retracted_key = fact_key(&scope, retracted.id);
    ops.push(WriteOp::KvPut {
        key: retracted_key.clone(),
        value: serde_json::to_vec(&retracted).expect("serialize retracted"),
    });
    excluded.push(retracted_key);

    let deleted = fact(&scope, "logically deleted fact", closed(&clock, false));
    let deleted_key = fact_key(&scope, deleted.id);
    ops.push(WriteOp::KvPut {
        key: deleted_key.clone(),
        value: serde_json::to_vec(&deleted).expect("serialize deleted"),
    });
    excluded.push(deleted_key);

    // Malformed: under the scope prefix but without a `{kind}:{id}` tail.
    let stray = format!("{}strayvalue", scope_prefix(&scope)).into_bytes();
    ops.push(WriteOp::KvPut { key: stray.clone(), value: b"{}".to_vec() });
    excluded.push(stray);

    source.atomic_write(&scope, &ops).await.expect("seed write");
    Seeded { _dir: dir, source: Arc::new(source), scope, expected, excluded }
}

async fn dest_keys(dest: &dyn StoragePort, scope: &Scope) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let prefix = scope_prefix(scope).into_bytes();
    let mut out = BTreeMap::new();
    let mut stream = dest.scan_range(scope, &prefix, None).await.expect("dest scan");
    while let Some(row) = stream.next().await {
        let (k, v) = row.expect("dest row");
        out.insert(k.to_vec(), v.to_vec());
    }
    out
}

async fn run_commit(seeded: &Seeded, dest: &TestStorage) -> ScopeReport {
    migrate_scope(
        seeded.source.as_ref(),
        dest.port().as_ref(),
        &seeded.scope,
        &MigrationOptions::committing(),
    )
    .await
    .expect("migrate")
}

#[tokio::test]
async fn migrates_current_rows_and_leaves_closed_intervals_behind() {
    let seeded = seed().await;
    let dest = open_test_storage().await;

    let report = run_commit(&seeded, &dest).await;

    assert_eq!(report.scanned, 8, "every seeded key is scanned: {report:?}");
    assert_eq!(report.eligible, 5, "2 episodes + 1 chunk + 2 open facts: {report:?}");
    assert_eq!(report.written, 5, "a committing run writes every eligible row");
    assert_eq!(report.skipped_closed_valid, 1, "the retracted fact is skipped");
    assert_eq!(report.skipped_closed_sys, 1, "the logically deleted fact is skipped");
    assert_eq!(report.skipped_foreign_key, 1, "the malformed key is skipped");
    assert_eq!(report.skipped(), 3);
    assert_eq!(report.by_kind.get("episode").copied(), Some(2));
    assert_eq!(report.by_kind.get("chunk").copied(), Some(1));
    assert_eq!(report.by_kind.get("fact").copied(), Some(2));
    // chunk + 2 facts carry embeddings; episodes do not.
    assert_eq!(report.needs_reembed, 3, "the re-embed backlog is reported, not hidden");

    let landed = dest_keys(dest.port().as_ref(), &seeded.scope).await;
    assert_eq!(landed.len(), 5, "destination holds exactly the eligible rows");
    for key in &seeded.expected {
        assert!(landed.contains_key(key), "missing {}", String::from_utf8_lossy(key));
    }
    for key in &seeded.excluded {
        assert!(!landed.contains_key(key), "leaked {}", String::from_utf8_lossy(key));
    }
}

#[tokio::test]
async fn migrated_content_is_byte_identical() {
    let seeded = seed().await;
    let dest = open_test_storage().await;
    run_commit(&seeded, &dest).await;

    let prefix = scope_prefix(&seeded.scope).into_bytes();
    let mut source_rows = BTreeMap::new();
    let mut stream =
        seeded.source.scan_range(&seeded.scope, &prefix, None).await.expect("source scan");
    while let Some(row) = stream.next().await {
        let (k, v) = row.expect("source row");
        source_rows.insert(k.to_vec(), v.to_vec());
    }
    drop(stream);

    let landed = dest_keys(dest.port().as_ref(), &seeded.scope).await;
    for key in &seeded.expected {
        assert_eq!(
            landed.get(key),
            source_rows.get(key),
            "content drift at {}",
            String::from_utf8_lossy(key)
        );
    }

    let v = verify_scope(seeded.source.as_ref(), dest.port().as_ref(), &seeded.scope, 32)
        .await
        .expect("verify");
    assert!(v.ok(), "verification must pass after a clean migration: {v:?}");
    assert_eq!(v.source_eligible, 5);
    assert_eq!(v.dest_rows, 5);
    assert_eq!(v.sampled, 5, "sample of 32 covers all five rows");
    assert_eq!(v.dest_only, 0);
}

#[tokio::test]
async fn dry_run_writes_nothing() {
    let seeded = seed().await;
    let dest = open_test_storage().await;

    let report = migrate_scope(
        seeded.source.as_ref(),
        dest.port().as_ref(),
        &seeded.scope,
        &MigrationOptions::default(),
    )
    .await
    .expect("dry run");

    assert_eq!(report.eligible, 5, "a dry run still counts what would move");
    assert_eq!(report.written, 0, "a dry run writes nothing");
    assert_eq!(report.batches, 0, "a dry run issues no atomic_write");
    assert!(
        dest_keys(dest.port().as_ref(), &seeded.scope).await.is_empty(),
        "destination must be untouched by a dry run"
    );
}

#[tokio::test]
async fn commit_without_acknowledgement_is_refused() {
    let seeded = seed().await;
    let dest = open_test_storage().await;

    let opts = MigrationOptions { commit: true, ..MigrationOptions::default() };
    let err = migrate_scope(seeded.source.as_ref(), dest.port().as_ref(), &seeded.scope, &opts)
        .await
        .expect_err("commit without acknowledgement must refuse");
    assert!(matches!(err, MigrateError::LossyNotAcknowledged), "got {err:?}");
    assert!(
        dest_keys(dest.port().as_ref(), &seeded.scope).await.is_empty(),
        "a refused run must not have written"
    );
}

#[tokio::test]
async fn re_running_is_idempotent() {
    let seeded = seed().await;
    let dest = open_test_storage().await;

    let first = run_commit(&seeded, &dest).await;
    let after_first = dest_keys(dest.port().as_ref(), &seeded.scope).await;
    let second = run_commit(&seeded, &dest).await;
    let after_second = dest_keys(dest.port().as_ref(), &seeded.scope).await;

    assert_eq!(first.written, 5, "the first run must actually have written");
    assert_eq!(first.eligible, second.eligible, "the same rows are eligible on a re-run");
    assert_eq!(first.written, second.written, "a re-run overwrites the same count");
    assert_eq!(after_first.len(), after_second.len(), "deterministic keys cannot duplicate");
    assert_eq!(after_first, after_second, "a re-run is byte-stable");

    let v = verify_scope(seeded.source.as_ref(), dest.port().as_ref(), &seeded.scope, 32)
        .await
        .expect("verify");
    assert!(v.ok(), "re-run must still verify: {v:?}");
    assert_eq!(v.dest_only, 0, "a re-run introduces no orphans");
}

#[tokio::test]
async fn reembed_manifest_lists_exactly_the_keys_that_lost_their_vectors() {
    let seeded = seed().await;
    let dest = open_test_storage().await;
    let manifest = seeded._dir.path().join("reembed.jsonl");

    let opts =
        MigrationOptions { reembed_manifest: Some(manifest.clone()), ..MigrationOptions::default() };
    let report = migrate_scope(seeded.source.as_ref(), dest.port().as_ref(), &seeded.scope, &opts)
        .await
        .expect("dry run with manifest");

    let body = std::fs::read_to_string(&manifest).expect("manifest written");
    let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), report.needs_reembed as usize);
    assert_eq!(lines.len(), 3, "1 chunk + 2 facts carry embeddings");
    for line in lines {
        let v: serde_json::Value = serde_json::from_str(line).expect("manifest line is JSON");
        let kind = v["kind"].as_str().expect("kind");
        assert!(matches!(kind, "chunk" | "fact"), "unexpected kind {kind}");
        assert!(v["key"].as_str().expect("key").starts_with("lunaris:migrate-it:"));
    }
}

#[tokio::test]
async fn scopes_are_discoverable_from_an_enumerable_source() {
    let seeded = seed().await;
    let scopes = discover_scopes(seeded.source.as_ref()).await.expect("list_scopes");
    assert!(
        scopes.iter().any(|s| s.as_str() == SCOPE),
        "--all-scopes must find the seeded scope, got {scopes:?}"
    );
}
