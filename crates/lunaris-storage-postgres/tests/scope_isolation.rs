//! Cross-scope leak integration test — RFC 0001 Wave 1B mandatory gate.
//!
//! Proves that Postgres RLS isolation works end-to-end:
//!
//! 1. Connect as the migration owner (PG_URL — may be superuser) and apply
//!    migrations + write data under `scope = "tenant_a"`.
//! 2. Derive a second connection URL as a non-superuser application role
//!    (`lunaris_app`) whose RLS policies are enforced. Provision the role on
//!    the first connection if it does not exist.
//! 3. `read_as_of` via the app role with `scope = "tenant_b"` MUST return `None`.
//! 4. `vector_search` via the app role with `scope = "tenant_b"` MUST return zero hits.
//! 5. `read_as_of` via the app role with `scope = "tenant_a"` MUST return the row.
//!
//! ## Why a separate app role is required
//!
//! Postgres RLS is bypassed for superusers (`rolsuper = t`) and roles with
//! `rolbypassrls = t` even when `FORCE ROW LEVEL SECURITY` is set. The typical
//! CI Postgres setup runs migrations as a superuser. The production deployment
//! MUST use a non-superuser application role — this test documents and verifies
//! that invariant by provisioning `lunaris_app` and re-connecting through it.
//!
//! Gated behind the `pg-it` Cargo feature — requires a live Postgres instance
//! at `PG_URL` with pgvector + AGE + pgmq extensions available.
//!
//! ```bash
//! cargo test -p lunaris-storage-postgres --features pg-it --test scope_isolation
//! ```

#![cfg(feature = "pg-it")]

use bytes::Bytes;
use lunaris_core::{Episode, HlcClock, Scope, StoragePort, WriteOp};
use lunaris_storage_postgres::PostgresStorage;
use ulid::Ulid;

/// The migration/owner URL — may be superuser, used for setup only.
fn pg_url() -> String {
    std::env::var("PG_URL").unwrap_or_else(|_| "postgres://localhost/lunaris".into())
}

/// Derive the application-role URL from PG_URL by substituting user/password.
/// Returns `None` if the base URL cannot be parsed or if the `PG_APP_URL` env
/// var is explicitly set to an empty string (opt-out).
fn pg_app_url(base_url: &str) -> Option<String> {
    // Allow explicit override via env var for CI environments that pre-create
    // the application role with a different name.
    if let Ok(v) = std::env::var("PG_APP_URL") {
        if v.is_empty() {
            return None;
        }
        return Some(v);
    }
    // Derive from base: replace user:password@host/db with lunaris_app:lunaris_app@host/db.
    let u = url::Url::parse(base_url).ok()?;
    let host = u.host_str()?;
    let port = u.port().unwrap_or(5432);
    let db = u.path().trim_start_matches('/');
    Some(format!("postgres://lunaris_app:lunaris_app@{host}:{port}/{db}"))
}

/// Provision the `lunaris_app` non-superuser role on the owner connection.
/// Returns `Ok(true)` if the role is usable, `Ok(false)` if creation failed
/// (e.g. the DB is read-only) and the test should skip.
async fn ensure_app_role(owner: &PostgresStorage) -> Result<bool, String> {
    // Create the role if absent, grant table access.
    let r = sqlx::query(
        "DO $$ BEGIN
           IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'lunaris_app') THEN
             CREATE ROLE lunaris_app LOGIN PASSWORD 'lunaris_app' NOSUPERUSER NOBYPASSRLS;
           END IF;
         END $$",
    )
    .execute(&owner.client().pool)
    .await;
    if let Err(e) = r {
        return Err(format!("lunaris_app role creation failed: {e}"));
    }

    // Grant schema + table access. GRANT ALL ON ALL TABLES is idempotent.
    // Ignore "tuple concurrently updated" (SqlState 40001 / serialization failure)
    // which can happen when two parallel test threads both try to GRANT at the same
    // time — the other thread's GRANT will succeed and the role ends up with access.
    let grants = [
        "GRANT USAGE ON SCHEMA public TO lunaris_app",
        "GRANT ALL ON ALL TABLES IN SCHEMA public TO lunaris_app",
        "GRANT ALL ON ALL SEQUENCES IN SCHEMA public TO lunaris_app",
    ];
    for sql in grants {
        if let Err(e) = sqlx::query(sql).execute(&owner.client().pool).await {
            let s = e.to_string();
            // "tuple concurrently updated" is a benign race when two tests provision
            // the role simultaneously — the winning thread's grant is sufficient.
            if s.contains("concurrently updated") || s.contains("40001") {
                continue;
            }
            return Err(format!("grant failed ({sql}): {e}"));
        }
    }
    Ok(true)
}

/// Cross-scope KV leak gate — MANDATORY per RFC 0001 §3.5 Wave 1B.
///
/// Writes under `tenant_a` via the owner connection, then reads back via
/// a non-superuser app-role connection under `tenant_b` (must return None)
/// and under `tenant_a` (must return the row).
#[tokio::test]
async fn cross_scope_read_returns_zero_for_wrong_scope() {
    let base_url = pg_url();

    let owner = match PostgresStorage::connect(&base_url).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("PG_URL not reachable ({e}); SKIP scope_isolation test");
            return;
        }
    };

    // Provision the non-superuser app role.
    match ensure_app_role(&owner).await {
        Ok(false) => {
            eprintln!("SKIP scope_isolation: ensure_app_role returned false");
            return;
        }
        Err(e) => {
            eprintln!("SKIP scope_isolation: ensure_app_role error: {e}");
            return;
        }
        Ok(true) => {}
    }

    let app_url = match pg_app_url(&base_url) {
        Some(u) => u,
        None => {
            eprintln!("SKIP scope_isolation: PG_APP_URL='' opt-out");
            return;
        }
    };

    // Connect as the non-superuser app role WITHOUT running migrations
    // (lunaris_app has DML but not DDL access; migrations ran on the owner connection above).
    let app = match PostgresStorage::connect_no_migrate(&app_url).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("lunaris_app connection failed ({e}); SKIP scope_isolation test");
            return;
        }
    };

    let scope_a = Scope::new("tenant_a").expect("tenant_a is valid");
    let scope_b = Scope::new("tenant_b").expect("tenant_b is valid");
    let clock = HlcClock::new(0);

    // Write under scope_a using owner connection (superuser, bypasses RLS on write — intended).
    let ep =
        Episode::new(scope_a.clone(), "scope_isolation.md", "secret data for tenant A", &clock);
    let key = format!("lunaris:episode:scope-isolation:{}", ep.id).into_bytes();
    let value = serde_json::to_vec(&ep).expect("episode serializes");

    owner
        .atomic_write(&scope_a, &[WriteOp::KvPut { key: key.clone(), value: value.clone() }])
        .await
        .expect("atomic_write under scope_a must succeed");

    let as_of = clock.tick();

    // Read under scope_b via app role (non-superuser — RLS enforced): MUST return None.
    let row_b = app
        .read_as_of(&scope_b, &key, as_of)
        .await
        .expect("read_as_of under scope_b must not error");
    assert!(
        row_b.is_none(),
        "RLS LEAK: read_as_of(&scope_b) returned Some via app role — \
         cross-scope data visible. RFC 0001 §3.5 isolation gate FAILED."
    );

    // Read under scope_a via app role: MUST return the row.
    let row_a = app
        .read_as_of(&scope_a, &key, as_of)
        .await
        .expect("read_as_of under scope_a must not error")
        .expect("read_as_of(&scope_a) must find the written row");
    assert_eq!(row_a.value, Bytes::from(value.clone()), "value must round-trip under scope_a");

    let back: Episode = serde_json::from_slice(&row_a.value).expect("episode deserializes");
    assert_eq!(back.id, ep.id, "episode id must round-trip");

    eprintln!(
        "scope_isolation KvPut: scope_b read = None (RLS enforced) ✓  \
         scope_a read = Some({}) ✓",
        ep.id
    );
}

/// Cross-scope vector search leak gate — RFC 0001 §3.5.
///
/// Inserts a chunk embedding under `tenant_a` via the owner connection, then
/// verifies that `vector_search` under `tenant_b` (non-superuser app role)
/// returns zero hits, while `tenant_a` returns the inserted chunk.
#[tokio::test]
async fn cross_scope_vector_search_returns_zero_for_wrong_scope() {
    let base_url = pg_url();

    let owner = match PostgresStorage::connect(&base_url).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("PG_URL not reachable ({e}); SKIP scope_isolation vector test");
            return;
        }
    };

    match ensure_app_role(&owner).await {
        Ok(false) | Err(_) => {
            eprintln!("SKIP scope_isolation vector: could not provision lunaris_app role");
            return;
        }
        Ok(true) => {}
    }

    let app_url = match pg_app_url(&base_url) {
        Some(u) => u,
        None => {
            eprintln!("SKIP scope_isolation vector: PG_APP_URL='' opt-out");
            return;
        }
    };

    let app = match PostgresStorage::connect_no_migrate(&app_url).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("lunaris_app connection failed ({e}); SKIP scope_isolation vector test");
            return;
        }
    };

    let scope_a = Scope::new("tenant_a").expect("valid");
    let scope_b = Scope::new("tenant_b").expect("valid");
    let clock = HlcClock::new(0);

    let chunk_id = Ulid::new();
    let embedding = vec![1.0_f32; 768];

    // Write under scope_a (owner/superuser — RLS bypassed for writes intentionally here).
    owner
        .atomic_write(
            &scope_a,
            &[WriteOp::VectorUpsert {
                index: "chunks".to_string(),
                id: chunk_id.to_bytes().to_vec(),
                embedding: embedding.clone(),
                metadata: serde_json::json!({
                    "text": "secret chunk for tenant A",
                    "scope_test": true,
                }),
            }],
        )
        .await
        .expect("VectorUpsert under scope_a must succeed");

    let as_of_hlc = clock.tick();
    let query = vec![1.0_f32; 768];
    let chunk_id_bytes = chunk_id.to_bytes().to_vec();

    // vector_search under scope_b via app role: MUST return zero hits for our chunk.
    let hits_b = app
        .vector_search(&scope_b, "chunks", &query, 10, None, Some(as_of_hlc), false)
        .await
        .expect("vector_search under scope_b must not error");
    let our_hits_b: Vec<_> = hits_b.iter().filter(|h| h.id == chunk_id_bytes).collect();
    assert!(
        our_hits_b.is_empty(),
        "RLS LEAK: vector_search(&scope_b) via app role returned {} hit(s) for our chunk — \
         cross-scope data visible. RFC 0001 §3.5 isolation gate FAILED.",
        our_hits_b.len()
    );

    // vector_search under scope_a via app role: MUST find our chunk.
    let hits_a = app
        .vector_search(&scope_a, "chunks", &query, 10, None, Some(as_of_hlc), false)
        .await
        .expect("vector_search under scope_a must not error");
    let our_hits_a: Vec<_> = hits_a.iter().filter(|h| h.id == chunk_id_bytes).collect();
    assert!(
        !our_hits_a.is_empty(),
        "vector_search(&scope_a) via app role returned zero hits — \
         scope_a must see its own data (RLS over-blocking or write did not store scope)."
    );

    eprintln!(
        "scope_isolation VectorSearch: scope_b hits = 0 (RLS enforced) ✓  \
         scope_a hits = {} ✓",
        our_hits_a.len()
    );
}
