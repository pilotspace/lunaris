//! Wave A.1 T2 — brute-force cosine `vector_search` regression suite.
//!
//! Locks the SQLite backend's vector recall behavior so the Wave A.1
//! cosine impl cannot silently regress.
//!
//! ## TDD discipline
//!
//! These tests were written BEFORE the implementation. The expected RED
//! signal is `StorageError::NotSupported(...)` from every `vector_search`
//! call. After the impl they should all be GREEN.
//!
//! ## Scope-isolation contract
//!
//! `vector_search` MUST only return rows whose scope-prefixed index name
//! matches `lunaris_{scope}_{index}_idx`. Cross-scope rows share the same
//! `lunaris_vec` table but differ in their `idx` column — SQL filters them
//! out before any Rust-side scoring.

#![allow(clippy::unwrap_used)]

use lunaris_core::scope::Scope;
use lunaris_core::storage::StoragePort as _;
use lunaris_core::storage::types::WriteOp;
use lunaris_storage_embedded::EmbeddedStorage;
use tempfile::TempDir;

// ── helper ────────────────────────────────────────────────────────────────────

/// Write a single vector entry under `scope` using `VectorUpsert`.
///
/// `id` must be a byte string that is unique within the test (any ASCII works;
/// we use it as the raw id bytes). `source` is stored as a metadata field so
/// `Filter::StartsWith { field: "source", ... }` can filter on it.
async fn write_vec(
    storage: &EmbeddedStorage,
    scope: &Scope,
    id: &[u8],
    source: &str,
    embedding: Vec<f32>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let meta = serde_json::json!({ "source": source });
    storage
        .atomic_write(
            scope,
            &[WriteOp::VectorUpsert {
                index: "chunks".to_string(),
                id: id.to_vec(),
                embedding,
                metadata: meta,
            }],
        )
        .await
        .map_err(|e| format!("atomic_write failed: {e}"))?;
    Ok(())
}

// ── Test 1: exact match top-1 ────────────────────────────────────────────────

/// Cosine of a vector with itself must be 1.0; the single write is returned.
#[tokio::test]
async fn cosine_recovers_exact_match_top1() {
    let storage = EmbeddedStorage::connect("memory://").await.unwrap();
    let scope = Scope::new("vt").unwrap();

    let v = vec![1.0_f32, 0.0, 0.0, 0.0];
    write_vec(&storage, &scope, b"id-001", "src:a", v.clone()).await.unwrap();

    let hits = storage.vector_search(&scope, "chunks", &v, 5, None, None, false).await.unwrap();
    assert_eq!(hits.len(), 1, "expected 1 hit, got {}", hits.len());
    assert!(
        (hits[0].score - 1.0).abs() < 1e-5,
        "cosine(v,v) must be 1.0 (within 1e-5); got {}",
        hits[0].score
    );
}

// ── Test 2: score-descending order ───────────────────────────────────────────

/// Probe: [1,0,0,0].
///
/// Embeddings and their expected cosines:
///   e1 = [1, 0, 0, 0]           → 1.000
///   e2 = [0.7, 0.7, 0, 0]       → 0.707  (normalised: each component / sqrt(0.98))
///   e3 = [0, 1, 0, 0]           → 0.000
///   e4 = [-1, 0, 0, 0]          → -1.000
///   e5 = [0.5, 0.5, 0.5, 0.5]   → 0.500
///
/// Expected top-5 order (desc): e1, e2, e5, e3, e4.
#[tokio::test]
async fn cosine_orders_hits_by_similarity_desc() {
    let storage = EmbeddedStorage::connect("memory://").await.unwrap();
    let scope = Scope::new("order-test").unwrap();

    let entries: &[(&[u8], Vec<f32>)] = &[
        (b"e1", vec![1.0, 0.0, 0.0, 0.0]),
        (b"e2", vec![0.7, 0.7, 0.0, 0.0]),
        (b"e3", vec![0.0, 1.0, 0.0, 0.0]),
        (b"e4", vec![-1.0, 0.0, 0.0, 0.0]),
        (b"e5", vec![0.5, 0.5, 0.5, 0.5]),
    ];

    for (id, emb) in entries {
        write_vec(&storage, &scope, id, "src:order", emb.clone()).await.unwrap();
    }

    let probe = vec![1.0_f32, 0.0, 0.0, 0.0];
    let hits = storage.vector_search(&scope, "chunks", &probe, 5, None, None, false).await.unwrap();

    assert_eq!(hits.len(), 5, "expected 5 hits, got {}", hits.len());

    // Scores must be strictly non-increasing.
    for w in hits.windows(2) {
        assert!(
            w[0].score >= w[1].score,
            "hits must be score-desc: score[{}] = {} < score[{}] = {}",
            0,
            w[0].score,
            1,
            w[1].score
        );
    }

    // The top hit must be e1 (cosine 1.0).
    assert_eq!(hits[0].id, b"e1".to_vec(), "top hit must be e1 (cosine 1.0)");
    assert!((hits[0].score - 1.0).abs() < 1e-4, "top score must be ~1.0");

    // The bottom hit must be e4 (cosine -1.0).
    assert_eq!(hits[4].id, b"e4".to_vec(), "bottom hit must be e4 (cosine -1.0)");
    assert!((hits[4].score - (-1.0)).abs() < 1e-4, "bottom score must be ~-1.0");
}

// ── Test 3: scope isolation ───────────────────────────────────────────────────

/// Rows written under `scope_b` MUST NOT appear in a search against `scope_a`,
/// even when the embeddings are identical.
#[tokio::test]
async fn vector_search_respects_scope_isolation() {
    let storage = EmbeddedStorage::connect("memory://").await.unwrap();
    let scope_a = Scope::new("scope-a").unwrap();
    let scope_b = Scope::new("scope-b").unwrap();

    let emb = vec![1.0_f32, 0.0, 0.0];

    write_vec(&storage, &scope_a, b"row-a", "src:a", emb.clone()).await.unwrap();
    write_vec(&storage, &scope_b, b"row-b", "src:b", emb.clone()).await.unwrap();

    let hits_a =
        storage.vector_search(&scope_a, "chunks", &emb, 10, None, None, false).await.unwrap();
    assert_eq!(
        hits_a.len(),
        1,
        "scope_a search must return exactly 1 hit; got {} hits (scope isolation broken)",
        hits_a.len()
    );
    assert_eq!(
        hits_a[0].id,
        b"row-a".to_vec(),
        "scope_a search must return the scope_a row, not scope_b's row"
    );

    let hits_b =
        storage.vector_search(&scope_b, "chunks", &emb, 10, None, None, false).await.unwrap();
    assert_eq!(hits_b.len(), 1, "scope_b search must return exactly 1 hit");
    assert_eq!(hits_b[0].id, b"row-b".to_vec(), "scope_b search must return the scope_b row");
}

// ── Test 4: filter composition (source prefix) ───────────────────────────────

/// `Filter::StartsWith { field: "source", prefix: "src:notes/" }` MUST be
/// applied at the SQL boundary (not post-filter in Rust). We verify by writing
/// three rows with different `source` metadata values, two matching and one
/// not, and asserting exactly 2 hits.
#[tokio::test]
async fn vector_search_composes_with_source_prefix_filter() {
    use lunaris_core::storage::types::Filter;

    let storage = EmbeddedStorage::connect("memory://").await.unwrap();
    let scope = Scope::new("filter-test").unwrap();

    // All share the same embedding — score differences don't affect hit count.
    let emb = vec![1.0_f32, 0.0, 0.0];
    write_vec(&storage, &scope, b"match-1", "src:notes/a.md", emb.clone()).await.unwrap();
    write_vec(&storage, &scope, b"match-2", "src:notes/b.md", emb.clone()).await.unwrap();
    write_vec(&storage, &scope, b"no-match", "src:other/c.md", emb.clone()).await.unwrap();

    let filter =
        Filter::StartsWith { field: "source".to_string(), prefix: "src:notes/".to_string() };
    let hits = storage
        .vector_search(&scope, "chunks", &emb, 10, Some(&filter), None, false)
        .await
        .unwrap();

    assert_eq!(
        hits.len(),
        2,
        "filter must return exactly 2 hits matching 'src:notes/' prefix; got {}",
        hits.len()
    );
    for h in &hits {
        let source = h.metadata.get("source").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            source.starts_with("src:notes/"),
            "every hit must have source starting with 'src:notes/'; got {source:?}"
        );
    }
}

// ── Test 5: empty result on no episodes ──────────────────────────────────────

/// A fresh backend with no `VectorUpsert` writes must return `Ok(vec![])`.
/// In particular, no panic and no error.
#[tokio::test]
async fn empty_result_when_no_episodes() {
    let storage = EmbeddedStorage::connect("memory://").await.unwrap();
    let scope = Scope::new("empty-scope").unwrap();

    let hits =
        storage.vector_search(&scope, "chunks", &[0.1, 0.2, 0.3], 5, None, None, false).await;
    assert!(hits.is_ok(), "empty backend must return Ok, not Err: {:?}", hits.err());
    assert!(hits.unwrap().is_empty(), "empty backend must return 0 hits");
}

// ── Test 6: zero-vector probe returns empty, not panic ───────────────────────

/// Cosine is undefined for a zero-norm probe vector. The impl must return
/// `Ok(vec![])` — neither panic nor divide-by-zero.
#[tokio::test]
async fn zero_vector_probe_returns_empty_not_panic() {
    let dir = TempDir::new().unwrap();
    let url = format!("sqlite://{}/zero_probe.db", dir.path().display());
    let storage = EmbeddedStorage::connect(&url).await.unwrap();
    let scope = Scope::new("zero-scope").unwrap();

    // Write a real embedding so there are rows to scan.
    write_vec(&storage, &scope, b"id-z1", "src:a", vec![1.0, 0.0, 0.0]).await.unwrap();
    write_vec(&storage, &scope, b"id-z2", "src:b", vec![0.0, 1.0, 0.0]).await.unwrap();

    let zero = vec![0.0_f32, 0.0, 0.0];
    let result = storage.vector_search(&scope, "chunks", &zero, 5, None, None, false).await;
    assert!(result.is_ok(), "zero-vector probe must return Ok, not Err: {:?}", result.err());
    assert!(result.unwrap().is_empty(), "zero-vector probe (undefined cosine) must return 0 hits");
}
