//! F22 — the `repair-vectors` subcommand, driven as the shipped binary.
//!
//! The repair logic has its own unit and live-Moon coverage in
//! `lunaris-storage-moon`. What that coverage cannot show is that anything an
//! operator can actually TYPE reaches it. A function that is correct and
//! unreachable fixes nothing, and the 622 damaged rows in the live store are
//! damaged precisely because the component that should have caught them was
//! never on the path they took.
//!
//! So this test runs `CARGO_BIN_EXE_lunaris` as a subprocess against a real
//! Moon, with a real damaged row, and asserts on the bytes of stdout and on
//! the state of the store afterwards. Nothing here is stubbed: the binary
//! parses its own argv, resolves the store from `LUNARIS_STORE_URL`, and goes
//! through `lunaris_memory_service::protocol::dispatch` like every other
//! surface.
//!
//! Both arms matter. The preview arm is the one an operator runs first, and a
//! preview that quietly mutated would be the worst bug this feature could
//! have; the commit arm is the one that has to actually work.

use std::process::Command;

use lunaris_core::{Scope, StoragePort, WriteOp};
use lunaris_storage_moon::MoonStorage;
use lunaris_storage_moon::keyspace::ft_index_name;
use lunaris_test_harness::EphemeralMoon;

const DIM: usize = 768;

fn unit_vector() -> Vec<f32> {
    let mut v = vec![0.0_f32; DIM];
    v[0] = 1.0;
    v
}

/// Reproduce a row written before the write-side guard: a real upsert through
/// the production path, then `vec` overwritten with zeroes by hand. The guard
/// means a zero embedding can no longer produce a `vec` field at all, so this
/// is the only way to build the legacy shape.
async fn seed_legacy_zero_row(storage: &MoonStorage, scope: &Scope, id: &[u8]) -> String {
    storage
        .atomic_write(
            scope,
            &[WriteOp::VectorUpsert {
                index: "chunks".into(),
                id: id.to_vec(),
                embedding: unit_vector(),
                metadata: serde_json::json!({
                    "text": "a document Lunaris failed to embed",
                    "source": "f22-cli.md",
                }),
            }],
        )
        .await
        .expect("seeding the pre-corruption row must succeed");

    let key = format!("{}:{}", ft_index_name(scope, "chunks"), hex::encode(id));
    let mut typed = storage.client().typed();
    let replaced: i64 = typed
        .hset(key.as_bytes(), "vec", vec![0u8; DIM * 4])
        .await
        .expect("overwriting `vec` must succeed");
    assert_eq!(
        replaced, 0,
        "HSET must have REPLACED an existing `vec` (0), not created one (1) — \
         otherwise the fixture is not a legacy row"
    );
    key
}

async fn vec_len(storage: &MoonStorage, key: &str) -> Option<usize> {
    let mut typed = storage.client().typed();
    let raw: Option<Vec<u8>> = typed.hget(key.as_bytes(), "vec").await.expect("HGET must succeed");
    raw.map(|v| v.len())
}

fn run_cli(url: &str, scope: &str, extra: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lunaris"));
    cmd.arg("repair-vectors")
        .arg("--scope")
        .arg(scope)
        .args(extra)
        // The CLI resolves its store the same way every surface does. Setting
        // this rather than writing a discovery file keeps the test from
        // depending on — or disturbing — anything under ~/.lunaris.
        .env("LUNARIS_STORE_URL", url)
        // ...except that LUNARIS_STORE_URL alone does NOT keep it off a real
        // store. `route.rs` is socket-FIRST: with a reachable contextd the
        // call is served by the daemon, which resolved its own store long ago,
        // and the URL above is never consulted. Measured 2026-09-01 on a
        // machine running contextd: the preview reported `scanned=0` and
        // printed `(via contextd)` — it had walked the developer's live store,
        // not the ephemeral Moon this test seeded. Pointing the socket at a
        // path that cannot exist forces the direct leg, which is what the
        // module doc above claims this test exercises.
        .env("LUNARIS_CONTEXTD_SOCKET", "/nonexistent/repair-vectors-e2e.sock")
        .env_remove("LUNARIS_SCOPE");
    cmd.output().expect("the lunaris binary must be runnable")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_repair_vectors_subcommand_previews_then_repairs_a_real_store() {
    let moon = match EphemeralMoon::spawn().await {
        Ok(m) => m,
        Err(e) => {
            // Not a silent skip: in the integration job, where a Moon is
            // guaranteed by construction, this panics.
            lunaris_test_harness::strict_skip::note_unavailable(format!(
                "repair_vectors_end_to_end: no ephemeral Moon ({e})"
            ));
            return;
        }
    };
    let storage =
        MoonStorage::connect_with_dim(moon.url(), DIM).await.expect("connect to a private Moon");
    let scope_name = "f22.cli";
    let scope = Scope::new(scope_name).expect("valid scope");

    let id = ulid::Ulid::new().to_bytes().to_vec();
    let key = seed_legacy_zero_row(&storage, &scope, &id).await;
    assert_eq!(vec_len(&storage, &key).await, Some(DIM * 4), "fixture must start damaged");

    // ── preview ────────────────────────────────────────────────────────────
    let out = run_cli(moon.url(), scope_name, &[]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(out.status.success(), "preview must exit 0.\nstdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("preview:") && stdout.contains("unindexable=1"),
        "the preview must report the damage it found.\nstdout: {stdout}"
    );
    assert!(
        stdout.contains("repaired=0") && stdout.contains("re-run with --commit"),
        "the preview must say plainly that it changed nothing.\nstdout: {stdout}"
    );
    assert_eq!(
        vec_len(&storage, &key).await,
        Some(DIM * 4),
        "PREVIEW MUTATED THE STORE — the `vec` field is gone after a run with no --commit"
    );

    // ── commit ─────────────────────────────────────────────────────────────
    let out = run_cli(moon.url(), scope_name, &["--commit"]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(out.status.success(), "commit must exit 0.\nstdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("repaired:") && stdout.contains("repaired=1"),
        "the commit must report the row it fixed.\nstdout: {stdout}"
    );
    assert_eq!(
        vec_len(&storage, &key).await,
        None,
        "the CLI must have actually removed the unindexable `vec` — if this is \
         still Some, the subcommand parsed and reported without reaching storage"
    );

    // The document must survive the repair; only the vector goes.
    let mut typed = storage.client().typed();
    let content: Option<String> =
        typed.hget(key.as_bytes(), "content").await.expect("HGET content must succeed");
    assert!(
        content.as_deref().is_some_and(|c| c.contains("failed to embed")),
        "the CLI repair must not have destroyed the document text"
    );

    // Idempotent: a second commit finds nothing left to do, which is what makes
    // the command safe for an operator to re-run after an interrupted sweep.
    let out = run_cli(moon.url(), scope_name, &["--commit"]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "a second commit must exit 0.\nstdout: {stdout}");
    assert!(
        stdout.contains("unindexable=0") && stdout.contains("repaired=0"),
        "a repeated sweep must be a no-op.\nstdout: {stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_index_is_refused_rather_than_reported_clean() {
    let moon = match EphemeralMoon::spawn().await {
        Ok(m) => m,
        Err(e) => {
            lunaris_test_harness::strict_skip::note_unavailable(format!(
                "repair_vectors_unknown_index: no ephemeral Moon ({e})"
            ));
            return;
        }
    };
    let _storage =
        MoonStorage::connect_with_dim(moon.url(), DIM).await.expect("connect to a private Moon");

    // A typo'd index name would sweep a key prefix nothing writes to and report
    // scanned=0 — which reads exactly like a healthy store. It has to be an
    // error instead.
    let out = run_cli(moon.url(), "f22.cli-typo", &["--index", "chunk"]);
    assert!(!out.status.success(), "an unknown index must be a non-zero exit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown vector index"),
        "the error must name the problem.\nstderr: {stderr}"
    );
}
