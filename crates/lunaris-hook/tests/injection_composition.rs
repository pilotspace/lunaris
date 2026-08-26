//! W4.4 — the injection-composition ratchet.
//!
//! The curation-gap census measured the live store over 1,204 real injection
//! blocks and found **99.9% of everything injected was a raw tool call**, with
//! two curated entries in the entire history. The engine was fine; it was
//! being fed shell-command noise. The fix demotes raw telemetry to substrate:
//! still captured, still stored, still returned by `memory.recall` — never
//! injected into an agent's context automatically.
//!
//! This drives the REAL `lunaris-contextd` binary, the process Claude Code's
//! hook consults for `additionalContext`, over a store seeded with a
//! deliberately hostile mix: telemetry outnumbers curated memories 4:1 and
//! every episode matches the prompt. A filter that leaked would leak here.
//!
//! ## Why this cannot pass vacuously
//!
//! "Zero telemetry injected" is also what an empty result looks like, and an
//! empty result is exactly what a broken seed, a store that never indexed, or
//! a recall that errored would produce. Two things close that route:
//!
//! 1. The default arm asserts at least one CURATED memory came back. A silent
//!    no-hit run fails instead of reporting a clean composition.
//! 2. The toggle arm re-runs the same store and prompt with
//!    `LUNARIS_CONTEXT_INCLUDE_TOOLCALLS=1` and requires telemetry to
//!    REAPPEAR. That is what makes this a measurement of the filter rather
//!    than an assertion about an empty read: the only difference between the
//!    arms is the flag, so the exclusion has to be its cause.
//!
//! ## Backend
//!
//! A disposable child-process Moon from `lunaris-test-harness`, and contextd
//! runs on the `NoopEmbedder` fallback — hybrid's keyword leg is text-based,
//! so recall works without a 253 MB GGUF download in CI.

mod contextd_harness;

use lunaris_core::{Episode, HlcClock, Scope, StubEmbedder};
use lunaris_test_harness::open_test_store;
use serde_json::{Value, json};

use contextd_harness::{memory_env, request, spawn_contextd};

const SCOPE: &str = "w44-composition-ratchet";
const PROMPT: &str = "what did we decide about the zephyr relay deployment gateway";

/// Sources the hook treats as raw tool-call telemetry. Kept as literals rather
/// than imported: this test asserts on the WIRE contract an agent sees, and a
/// rename of the internal constant must not silently retarget the assertion.
const TELEMETRY_SOURCES: [&str; 4] = [
    "lunaris:tool_call:pre",
    "lunaris:tool_call:post",
    "lunaris:pre_tool_use",
    "lunaris:post_tool_use",
];

/// Seed a hostile mix: 12 telemetry episodes to 3 curated ones, all of them
/// matching the prompt's vocabulary so ranking cannot separate them by luck.
async fn seed(moon_url: &str, scope: &Scope) {
    let storage = lunaris::open(moon_url).await.expect("open moon storage for seeding");
    let clock = HlcClock::new(0);
    let embedder = StubEmbedder::new(768);

    let write = async |source: &str, content: String| {
        let ep = Episode::new(scope.clone(), source, &content, &clock);
        lunaris_ingest::ingest_episode(storage.as_ref(), &embedder, &clock, ep)
            .await
            .expect("seed episode");
    };

    for i in 0..3 {
        for source in TELEMETRY_SOURCES {
            write(
                source,
                format!(
                    "{{\"tool\":\"Bash\",\"command\":\"deploy zephyr relay gateway --retry {i}\",\
                      \"cwd\":\"/srv/zephyr\",\"exit\":0}}"
                ),
            )
            .await;
        }
    }

    write(
        &format!("decision:{SCOPE}"),
        "we decided the zephyr relay deployment goes through the blue gateway, \
         because the green one cannot hold the connection during a rollover"
            .to_owned(),
    )
    .await;
    write(
        &format!("edit:{SCOPE}"),
        "changed the zephyr relay gateway timeout to 30s in deployment config".to_owned(),
    )
    .await;
    write(
        &format!("distilled:{SCOPE}"),
        "zephyr relay deployment: always drain the gateway before a rollover".to_owned(),
    )
    .await;
}

fn recall_for_prompt() -> Value {
    json!({
        "type": "recall_for_prompt",
        "scope": SCOPE,
        "session_id": "w44-ratchet",
        "prompt": PROMPT,
    })
}

/// Sources of the memories contextd chose to inject.
fn injected_sources(response: &Value) -> Vec<String> {
    response["memories"]
        .as_array()
        .map(|hits| hits.iter().filter_map(|h| h["source"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

fn is_telemetry(source: &str) -> bool {
    TELEMETRY_SOURCES.contains(&source)
}

#[tokio::test]
async fn raw_telemetry_is_never_injected_but_stays_recallable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = open_test_store().await;
    let scope = Scope::new(SCOPE).expect("valid scope");
    seed(store.url(), &scope).await;

    // ── Arm 1: default. Telemetry is demoted. ────────────────────────────────
    let socket = dir.path().join("default.sock");
    let mut child = spawn_contextd(&socket, &memory_env(store.url())).await;
    let default = request(&socket, &recall_for_prompt()).await;
    let _ = child.kill().await;

    let sources = injected_sources(&default);
    let telemetry: Vec<&String> = sources.iter().filter(|s| is_telemetry(s)).collect();
    let curated = sources.len() - telemetry.len();

    // Instrument self-check FIRST: 0% telemetry and 0 hits are the same
    // observable state, and only one of them means the filter worked.
    assert!(
        curated > 0,
        "no curated memory was injected, so \"zero telemetry\" proves nothing — the seed, \
         the index or the recall is broken. Injected sources: {sources:?}\nResponse: {default}"
    );
    assert!(
        telemetry.is_empty(),
        "raw tool-call telemetry reached an agent's context. {} of {} injected memories were \
         telemetry: {telemetry:?}",
        telemetry.len(),
        sources.len()
    );

    // ── Arm 2: the toggle. Telemetry comes back. ─────────────────────────────
    // Same store, same prompt, one flag different — so arm 1's silence is
    // attributable to the filter and not to an empty read.
    let socket = dir.path().join("toggled.sock");
    let mut env = memory_env(store.url());
    env.insert("LUNARIS_CONTEXT_INCLUDE_TOOLCALLS", "1".to_owned());
    let mut child = spawn_contextd(&socket, &env).await;
    let toggled = request(&socket, &recall_for_prompt()).await;
    let _ = child.kill().await;

    let toggled_sources = injected_sources(&toggled);
    assert!(
        toggled_sources.iter().any(|s| is_telemetry(s)),
        "LUNARIS_CONTEXT_INCLUDE_TOOLCALLS=1 must restore telemetry injection. Without this \
         arm, arm 1 would pass for a store that simply returned nothing. Injected: \
         {toggled_sources:?}\nResponse: {toggled}"
    );
}
