//! ADD task `hook-recall-graph-hybrid` (contract FROZEN @ v1.1, 2026-07-14):
//! e2e discriminator + degrade pins for hybrid context recall.
//!
//! Drives the REAL `lunaris-contextd` binary — the process Claude Code's
//! UserPromptSubmit hook consults for `additionalContext`
//! (docs/integration/claude-code.md; amendment v1.1 §2) — over its unix
//! socket with `recall_for_prompt` requests.
//!
//! This file references NO new symbols, so it compiles TODAY: the live
//! discriminator is ASSERTION-RED (a graph-pipeline fact is invisible to the
//! current chunks-only recall) and goes green only when the v1.1 hybrid root
//! serves the production path. The memory:// tests are degrade guard pins.
//!
//! Live gate: LUNARIS_HOOK_TEST_MOON_URL (moon-it pattern: skipped when
//! unset). Run on this box where the staged granite GGUF exists — contextd
//! embeds the prompt in-process.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use lunaris_core::keyspace::fact_key;
use lunaris_core::storage::types::WriteOp;
use lunaris_core::{Episode, HlcClock, Scope, StubEmbedder};
use lunaris_extract::types::{EntityId, Fact};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use ulid::Ulid;

const FACT_TEXT: &str = "the zephyr-relay service listens on port 7443";
const PROMPT: &str = "which port does the zephyr-relay service listen on?";

// ─── contextd harness ────────────────────────────────────────────────────────

/// Spawn the real `lunaris-contextd` binary on a temp socket with the given
/// extra env; wait until the socket accepts connections.
async fn spawn_contextd(socket: &Path, extra_env: &HashMap<&str, String>) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lunaris-contextd"));
    cmd.arg("--socket")
        .arg(socket)
        .env_remove("LUNARIS_CONTEXT_RECALL")
        .env_remove("LUNARIS_CONTEXT_RECALL_TIMEOUT_MS")
        .env_remove("LUNARIS_STORE_URL")
        .kill_on_drop(true);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let child = cmd.spawn().expect("spawn lunaris-contextd");

    for _ in 0..200 {
        if UnixStream::connect(socket).await.is_ok() {
            return child;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("lunaris-contextd never bound {}", socket.display());
}

/// One request/response round-trip over the contextd socket protocol
/// (write JSON, shutdown write half, read to EOF).
async fn request(socket: &Path, body: &Value) -> Value {
    let round_trip = async {
        let mut stream = UnixStream::connect(socket).await.expect("connect contextd socket");
        stream.write_all(body.to_string().as_bytes()).await.expect("write request");
        stream.shutdown().await.expect("shutdown write half");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read response");
        serde_json::from_slice::<Value>(&buf).expect("contextd responds with JSON")
    };
    // Generous budget: the FIRST request lazily opens the Lunaris handle and
    // loads the GGUF embedder inside the contextd process.
    tokio::time::timeout(Duration::from_secs(180), round_trip)
        .await
        .expect("contextd answered within budget")
}

fn recall_for_prompt(scope: &str) -> Value {
    json!({
        "type": "recall_for_prompt",
        "scope": scope,
        "session_id": "hybrid-e2e",
        "prompt": PROMPT,
    })
}

/// Seed the live-Moon scope: three chunk episodes through the production
/// ingest path (none mention the fact) + ONE fact through the production
/// graph-ON write ops (KvPut{fact_key} + VectorUpsert{"facts"} — the exact
/// op shapes of crates/lunaris/src/ingest.rs graph-ON step 5).
async fn seed_scope(moon_url: &str, scope: &Scope) {
    let storage = lunaris::open(moon_url).await.expect("open moon storage for seeding");
    let clock = HlcClock::new(0);
    let embedder = StubEmbedder::new(768);

    for content in [
        "the build pipeline uses cargo nextest for the workspace test run",
        "the deploy target is the staging cluster behind the blue gateway",
        "yesterday we migrated the session store to the new schema",
    ] {
        let ep = Episode::new(scope.clone(), "test:chunk-seed", content, &clock);
        lunaris_ingest::ingest_episode(storage.as_ref(), &embedder, &clock, ep)
            .await
            .expect("seed chunk episode");
    }

    let fact_id = Ulid::new();
    let fact = Fact {
        id: fact_id,
        subject_id: EntityId([7u8; 16]),
        predicate: "listens_on".to_owned(),
        object_id: EntityId([9u8; 16]),
        fact_text: FACT_TEXT.to_owned(),
        confidence: 0.95,
        valid_from_iso: "2026-07-14T00:00:00Z".to_owned(),
        valid_to_iso: None,
    };
    let stub_embedding: Vec<f32> = (0..768).map(|i| ((i % 13) as f32 - 6.0) / 13.0).collect();
    let ops = vec![
        WriteOp::KvPut {
            key: fact_key(scope, fact_id),
            value: serde_json::to_vec(&fact).expect("serialize fact"),
        },
        WriteOp::VectorUpsert {
            index: "facts".into(),
            id: fact_id.to_bytes().to_vec(),
            embedding: stub_embedding,
            metadata: json!({"predicate": "listens_on", "fact_text": FACT_TEXT}),
        },
    ];
    storage.atomic_write(scope, &ops).await.expect("seed fact via graph-ON ops");
}

// ─── Live-Moon discriminator ─────────────────────────────────────────────────

/// §2 scenario 1 — "a graph-pipeline fact surfaces in injected context".
/// The fact exists in NO chunk, so it is reachable ONLY through the facts
/// legs of the v1.1 hybrid root. Default (hybrid) MUST surface it in
/// rendered_context; LUNARIS_CONTEXT_RECALL=vector (legacy) MUST NOT —
/// the same seeded scope discriminates the two routings.
#[tokio::test]
async fn fact_surfaces_in_injected_context_moon() {
    let Ok(moon_url) = std::env::var("LUNARIS_HOOK_TEST_MOON_URL") else {
        eprintln!("skipping: LUNARIS_HOOK_TEST_MOON_URL not set (live-Moon gate)");
        return;
    };

    let scope_str = format!("ctxd-hybrid-{}", Ulid::new());
    let scope = Scope::new(&scope_str).expect("valid scope");
    seed_scope(&moon_url, &scope).await;

    let dir = tempfile::tempdir().expect("tempdir");

    // Leg 1 — default routing (hybrid per contract v1.1).
    let hybrid_socket = dir.path().join("hybrid.sock");
    let mut env = HashMap::new();
    env.insert("LUNARIS_STORE_URL", moon_url.clone());
    let mut hybrid_child = spawn_contextd(&hybrid_socket, &env).await;
    let hybrid = request(&hybrid_socket, &recall_for_prompt(&scope_str)).await;
    let _ = hybrid_child.kill().await;

    // Leg 2 — legacy opt-out on the SAME seeded scope.
    let legacy_socket = dir.path().join("legacy.sock");
    env.insert("LUNARIS_CONTEXT_RECALL", "vector".to_owned());
    let mut legacy_child = spawn_contextd(&legacy_socket, &env).await;
    let legacy = request(&legacy_socket, &recall_for_prompt(&scope_str)).await;
    let _ = legacy_child.kill().await;

    let legacy_rendered = legacy["rendered_context"].as_str().unwrap_or_default();
    assert!(
        !legacy_rendered.contains("7443"),
        "discriminator broken: the legacy chunks-only path must NOT reach the fact; \
         got: {legacy}"
    );

    assert_eq!(hybrid["ok"], json!(true), "hybrid recall must not error: {hybrid}");
    let hybrid_rendered = hybrid["rendered_context"].as_str().unwrap_or_default();
    assert!(
        hybrid_rendered.contains("7443"),
        "graph-pipeline fact (reachable ONLY via the facts legs) must surface in the \
         injected context under default hybrid recall; got: {hybrid}"
    );
    let sources: Vec<&str> = hybrid["memories"]
        .as_array()
        .map(|m| m.iter().filter_map(|v| v["source"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        sources.iter().any(|s| s.starts_with("fact:")),
        "fact hit must carry source=fact:{{predicate}} provenance; sources: {sources:?}"
    );
}

// ─── memory:// degrade pins (no live backend needed) ─────────────────────────

fn memory_env() -> HashMap<&'static str, String> {
    let mut env = HashMap::new();
    env.insert("LUNARIS_STORE_URL", "memory://".to_owned());
    // Force the fast NoopEmbedder fallback — these pins exercise routing, not
    // embedding quality, and must stay CI-cheap.
    env.insert("LUNARIS_EMBEDDER_GGUF", "/nonexistent/embedder.gguf".to_owned());
    env.insert("LUNARIS_RERANKER_GGUF", "/nonexistent/reranker.gguf".to_owned());
    env
}

/// §2 scenario 3 — "hybrid failure/timeout degrades, never blocks":
/// TIMEOUT_MS=0 forces an instant hybrid timeout; the response must be
/// exactly what the legacy path serves (control run), and the connection
/// completes cleanly — no hybrid error may surface to the caller.
#[tokio::test]
async fn timeout_zero_degrades_to_legacy() {
    let dir = tempfile::tempdir().expect("tempdir");

    let timeout_socket = dir.path().join("timeout.sock");
    let mut env = memory_env();
    env.insert("LUNARIS_CONTEXT_RECALL_TIMEOUT_MS", "0".to_owned());
    let mut timeout_child = spawn_contextd(&timeout_socket, &env).await;
    let degraded = request(&timeout_socket, &recall_for_prompt("ctxd-degrade-pin")).await;
    let _ = timeout_child.kill().await;

    let control_socket = dir.path().join("control.sock");
    let mut env = memory_env();
    env.insert("LUNARIS_CONTEXT_RECALL", "vector".to_owned());
    let mut control_child = spawn_contextd(&control_socket, &env).await;
    let control = request(&control_socket, &recall_for_prompt("ctxd-degrade-pin")).await;
    let _ = control_child.kill().await;

    assert_eq!(
        degraded, control,
        "an instantly-timed-out hybrid must serve EXACTLY the legacy response"
    );
    assert!(
        degraded["error"]
            .as_str()
            .map(str::to_lowercase)
            .unwrap_or_default()
            .find("hybrid")
            .is_none(),
        "no hybrid-flavored error may surface to the agent: {degraded}"
    );
}

/// §2 scenario 4 — "legacy opt-out is byte-identical": with
/// LUNARIS_CONTEXT_RECALL=vector, an empty-store prompt recall responds
/// exactly like today's path (the strong routing pin — facts legs never
/// consulted — lives in context_hybrid_root.rs at the unit level).
#[tokio::test]
async fn legacy_env_routes_old_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("legacy-pin.sock");
    let mut env = memory_env();
    env.insert("LUNARIS_CONTEXT_RECALL", "vector".to_owned());
    let mut child = spawn_contextd(&socket, &env).await;
    let response = request(&socket, &recall_for_prompt("ctxd-legacy-pin")).await;
    let _ = child.kill().await;

    // Today's memory:// behavior: the embedded backend has no BM25, so the
    // legacy keyword fallback surfaces its error as ok:false — the adapter
    // treats it as "no context". Pin the shape, not the exact message.
    assert!(response.get("ok").is_some(), "contextd must answer the legacy request: {response}");
    let rendered = response["rendered_context"].as_str().unwrap_or_default();
    assert!(rendered.is_empty(), "empty store must inject nothing: {response}");
}
