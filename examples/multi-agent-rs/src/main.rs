//! Lunaris multi-agent / multi-session example — Rust.
//!
//! Proves three things against a **live single-shard Moon backend**, whose URL
//! comes from `LUNARIS_STORE_URL` (no default — see `store_url` below):
//!
//! 1. **Hard isolation between agents.** Two `Scope`s under one `Lunaris`
//!    handle; recall under each scope never returns the other agent's content.
//! 2. **Multiple working sessions inside one agent.** Several episodes whose
//!    `source` encodes a session/task; a single recall spans all of them.
//! 3. **Resume across a process boundary.** `drop(lunaris)`, `Lunaris::open`-
//!    equivalent again, re-derive the scope, recall — the agent's episodes are
//!    still there (Moon is durable; no explicit load step).
//!
//! ## Embedder choice
//!
//! This example uses the **`with_parts_keyword` escape hatch** rather than
//! `Lunaris::open`. Rationale: it must run with no services beyond the one
//! Moon, no C++ toolchain and no model download — which is also why
//! `Cargo.toml` sets `default-features = false`. So:
//!
//! - `storage` = `Arc<MoonStorage>::connect(&store_url()?)`
//! - `keyword` = the **same** `Arc<MoonStorage>` (it impls `KeywordPort`, so
//!   you still get BM25)
//! - `embedder` = `StubEmbedder::new(768)` — deterministic, NOT semantic. The
//!   Moon `chunks` FT index is 768d (matching the granite-r2 embedder), so the
//!   dims line up; the *ranking* is meaningless, which is why the assertions
//!   below check "≥ 1 hit" / "no cross-scope leak" rather than "the right hit
//!   ranked first". Swap `StubEmbedder` for `Lunaris::open` on a default-
//!   feature build and the same code recalls semantically.
//!
//! ## Prerequisites
//!
//! A single-shard Moon, with its URL exported as `LUNARIS_STORE_URL`.
//! `--shards 1` is required: the sharded default breaks Lunaris's
//! `atomic_write` with `TXN does not support cross-shard writes`.
//!
//! Lunaris 0.7.0 rejects any Moon below 0.8.5, and v0.8.5 shipped no release
//! assets, so there is nothing to download yet — build one from the vendored
//! submodule:
//!
//! ```sh
//! cargo build --release --manifest-path vendor/moon/Cargo.toml --bin moon
//! vendor/moon/target/release/moon --port 6380 --shards 1 --dir /tmp/lunaris-multi-agent-data
//! redis-cli -p 6380 PING   # -> PONG
//! ```
//!
//! Then:
//!
//! ```sh
//! cd examples/multi-agent-rs
//! export LUNARIS_STORE_URL="moon://127.0.0.1:6380"
//! cargo run
//! ```
//!
//! The verbatim run output is captured in this directory's `README.md`.

use std::sync::Arc;

use anyhow::{Context, Result};
use lunaris::{
    Embedder, EpisodeBuilder, HlcClock, KeywordPort, Lunaris, MoonStorage, Query, Scope,
    StoragePort, StubEmbedder, Vector,
};

/// The store URL, read from `LUNARIS_STORE_URL`.
///
/// There is no default on purpose, and this example is the reason the rule
/// exists: it ingests demo episodes under two scopes and then asserts on what
/// comes back. A hardcoded `moon://localhost:6380` would write that demo data
/// into whatever Moon happens to own the port on the reader's machine — which
/// on a developer box is very often a real store.
fn store_url() -> Result<String> {
    std::env::var("LUNARIS_STORE_URL").context(
        "set LUNARIS_STORE_URL=moon://127.0.0.1:6380 (single-shard) \
         — see examples/multi-agent-rs/README.md",
    )
}

/// `EpisodeBuilder::new(source, content)`.
///
/// `EpisodeBuilder` auto-generates a fresh ULID per episode
/// (`into_episode` does `self.id.unwrap_or_else(Ulid::new)`), so no explicit
/// `.id(...)` is needed here — override it only when you want idempotent replay.
fn episode(source: &str, content: &str) -> EpisodeBuilder {
    EpisodeBuilder::new(source, content)
}

/// Build a `Lunaris` handle wired to the live Moon backend with `MoonStorage`
/// as BOTH the storage and keyword (BM25) port, and a deterministic 768d
/// `StubEmbedder`. See the module docs for why the stub embedder.
async fn open_moon_handle() -> Result<Lunaris> {
    let url = store_url()?;
    let moon = Arc::new(MoonStorage::connect(&url).await.with_context(|| {
        format!("MoonStorage::connect({url}) failed — is the single-shard Moon server running?")
    })?);
    let storage: Arc<dyn StoragePort> = moon.clone();
    let keyword: Arc<dyn KeywordPort> = moon.clone();
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    let clock: Arc<HlcClock> = HlcClock::new(0);
    Ok(Lunaris::with_parts_keyword(storage, keyword, embedder, clock))
}

/// A `Vector("chunks", 30).top(5)` recall through a scope-bound handle.
/// Returns the hit count and the list of distinct episode sources seen.
async fn recall_sources(
    lunaris: &Lunaris,
    scope: &Scope,
    query: &str,
) -> Result<(usize, Vec<String>)> {
    let hits = lunaris
        .scoped(scope.clone())
        .dsl()
        .with_root(Vector::new("chunks", 30).top(5))
        .execute(Query::text(query))
        .await
        .with_context(|| format!("recall under scope `{}` for {query:?} failed", scope.as_str()))?;
    let mut sources: Vec<String> = hits.iter().map(|h| h.source.clone()).collect();
    sources.sort();
    sources.dedup();
    Ok((hits.len(), sources))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Unique run id so repeated `cargo run`s against a shared Moon don't read
    // each other's leftovers when we assert on cross-scope isolation.
    let run = std::process::id();
    let scope_a = Scope::new(format!("acme.agent-a-{run}")).context("Scope::new agent-a")?;
    let scope_b = Scope::new(format!("acme.agent-b-{run}")).context("Scope::new agent-b")?;
    println!("multi-agent: run id {run}");
    println!("multi-agent: scope_a = {}", scope_a.as_str());
    println!("multi-agent: scope_b = {}", scope_b.as_str());

    // ── 1. Hard isolation between agents ────────────────────────────────────
    println!("\n=== 1. hard isolation between two agents (distinct Scopes) ===");
    let lunaris = open_moon_handle().await?;
    {
        let scoped_a = lunaris.scoped(scope_a.clone());
        let scoped_b = lunaris.scoped(scope_b.clone());

        let lsn_a = scoped_a
            .ingest(episode(
                "agent-a:notes",
                "# Agent A private memo\n\nThe acme widget ships on Friday. Owner: Alice.",
            ))
            .await
            .context("agent-a ingest")?;
        let lsn_b = scoped_b
            .ingest(episode(
                "agent-b:notes",
                "# Agent B private memo\n\nThe beta gadget recall is paused. Owner: Bob.",
            ))
            .await
            .context("agent-b ingest")?;
        println!("multi-agent: ingested agent-a episode at lsn={lsn_a:?}");
        println!("multi-agent: ingested agent-b episode at lsn={lsn_b:?}");
        // FT index catch-up: Moon's FT.* index lags the KV write by a small
        // window; poll briefly before asserting on recall results.
        wait_for_index(&lunaris, &scope_a, "widget", 1).await?;
        wait_for_index(&lunaris, &scope_b, "gadget", 1).await?;

        let (count_a, sources_a) = recall_sources(&lunaris, &scope_a, "owner").await?;
        let (count_b, sources_b) = recall_sources(&lunaris, &scope_b, "owner").await?;
        println!("multi-agent: scope_a recall(\"owner\") -> {count_a} hit(s), sources={sources_a:?}");
        println!("multi-agent: scope_b recall(\"owner\") -> {count_b} hit(s), sources={sources_b:?}");

        assert!(count_a >= 1, "agent-a should see its own episode");
        assert!(count_b >= 1, "agent-b should see its own episode");
        // The wall: agent-a's hits NEVER include agent-b's source, and vice versa.
        assert!(
            sources_a.iter().all(|s| s.starts_with("agent-a:")),
            "agent-a recall leaked a non-agent-a source: {sources_a:?}"
        );
        assert!(
            sources_b.iter().all(|s| s.starts_with("agent-b:")),
            "agent-b recall leaked a non-agent-b source: {sources_b:?}"
        );
        assert!(
            !sources_a.iter().any(|s| s.starts_with("agent-b:")),
            "ISOLATION VIOLATION: agent-a saw agent-b content"
        );
        assert!(
            !sources_b.iter().any(|s| s.starts_with("agent-a:")),
            "ISOLATION VIOLATION: agent-b saw agent-a content"
        );
        println!("multi-agent: OK — neither agent can see the other's episode");
    }

    // ── 2. Multiple working sessions within one agent ──────────────────────
    println!("\n=== 2. multiple sessions / tasks within agent-a (source-prefix partition) ===");
    {
        let scoped_a = lunaris.scoped(scope_a.clone());
        // The first arg to `EpisodeBuilder::new` IS the `source` — encode the
        // session/task there: `conv:mon`, `conv:tue`, `task:deploy`.
        scoped_a
            .ingest(episode(
                "conv:mon",
                "Monday standup: we picked the acme widget rollout date — Friday.",
            ))
            .await
            .context("conv:mon ingest")?;
        scoped_a
            .ingest(episode(
                "conv:tue",
                "Tuesday sync: QA signed off on the acme widget; no blockers.",
            ))
            .await
            .context("conv:tue ingest")?;
        scoped_a
            .ingest(episode(
                "task:deploy",
                "Deploy task: cut the acme widget release branch and tag v1.0.",
            ))
            .await
            .context("task:deploy ingest")?;
        println!("multi-agent: ingested 3 session/task episodes under scope_a");
        // Wait until all 4 of agent-a's episodes (notes + 3 new) are indexed.
        wait_for_index(&lunaris, &scope_a, "acme widget", 3).await?;

        let (count_all, sources_all) = recall_sources(&lunaris, &scope_a, "acme widget").await?;
        println!(
            "multi-agent: scope_a recall(\"acme widget\") -> {count_all} hit(s), sources={sources_all:?}"
        );
        assert!(count_all >= 2, "recall should span multiple sessions");
        let session_kinds: Vec<&str> = sources_all
            .iter()
            .filter_map(|s| s.split(':').next())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        println!("multi-agent: distinct source-prefix kinds seen across the recall: {session_kinds:?}");
        assert!(
            session_kinds.len() >= 2,
            "expected hits from at least two different session/task prefixes; got {session_kinds:?}"
        );

        // Narrow to one session. `ScopedLunaris::dsl()` exposes
        // `filter_str(...)` — but the v0 filter DSL filters on Episode
        // METADATA, not on the `source` string, and `EpisodeBuilder` does not
        // copy `source` into metadata, so there is no source-prefix push-down
        // available here. We do the prefix narrowing client-side over
        // `Hit.source` (the same approach `MessageStream::recall` uses) and
        // note the limitation explicitly rather than faking a server-side filter.
        let one_session: Vec<String> = lunaris
            .scoped(scope_a.clone())
            .dsl()
            .with_root(Vector::new("chunks", 30).top(20))
            .execute(Query::text("acme widget"))
            .await
            .context("scope_a recall for session narrowing")?
            .into_iter()
            .map(|h| h.source)
            .filter(|s| s.starts_with("conv:mon"))
            .collect();
        println!(
            "multi-agent: client-side narrowed to source-prefix `conv:mon` -> {} hit(s): {one_session:?}",
            one_session.len()
        );
        println!(
            "multi-agent: NOTE — there is no server-side `source`-prefix filter today; the v0 \
             `filter_str` DSL targets Episode metadata, not the source string. Narrowing is \
             client-side over `Hit.source` (matches the recipes' MessageStream behaviour)."
        );
    }

    // ── 3. Resume across a process boundary ────────────────────────────────
    println!("\n=== 3. resume across a process boundary (drop handle, re-open, re-scope) ===");
    drop(lunaris);
    println!("multi-agent: dropped the Lunaris handle (simulating process exit)");
    {
        let lunaris2 = open_moon_handle().await?;
        let (count_a2, sources_a2) = recall_sources(&lunaris2, &scope_a, "owner").await?;
        println!(
            "multi-agent: after re-open, scope_a recall(\"owner\") -> {count_a2} hit(s), sources={sources_a2:?}"
        );
        assert!(count_a2 >= 1, "agent-a episodes should survive a handle drop (Moon is durable)");
        assert!(
            sources_a2.iter().all(|s| s.starts_with("agent-a:") || s.starts_with("conv:") || s.starts_with("task:")),
            "post-resume recall leaked an unexpected source: {sources_a2:?}"
        );
        // And isolation still holds after the re-open.
        let (count_b2, _sources_b2) = recall_sources(&lunaris2, &scope_b, "owner").await?;
        println!("multi-agent: after re-open, scope_b recall(\"owner\") -> {count_b2} hit(s)");
        assert!(count_b2 >= 1, "agent-b episodes should also survive");
        println!("multi-agent: OK — agent-a memory is durable across the process boundary");
    }

    println!("\nmulti-agent: ALL ASSERTIONS PASSED ✔");
    println!(
        "multi-agent: NOTE — the recipe wrappers (MultiTurnConversation, ChatAgentMemory, \
         MessageStream) currently build episodes with Scope::dev() and partition only by source \
         prefix; hard per-agent isolation today goes through the low-level lunaris.scoped(scope) \
         handle shown above (or, in lunaris-server, the server-side `tenant` claim). See \
         docs/book/src/guides/multi-agent.md."
    );
    Ok(())
}

/// Poll Moon's FT index until a scoped `Vector("chunks", k).top(k)` recall for
/// `probe` returns at least `min_hits` results, or ~5s elapses. Moon's FT.*
/// index lags the KV write by a small window; without this the first recall
/// after an ingest can legitimately return zero hits (a measurement race, not
/// data loss — see docs/durability.md).
async fn wait_for_index(
    lunaris: &Lunaris,
    scope: &Scope,
    probe: &str,
    min_hits: usize,
) -> Result<()> {
    for attempt in 0..50 {
        let hits = lunaris
            .scoped(scope.clone())
            .dsl()
            .with_root(Vector::new("chunks", 50).top(50))
            .execute(Query::text(probe))
            .await
            .with_context(|| format!("index-wait recall under `{}` for {probe:?}", scope.as_str()))?;
        if hits.len() >= min_hits {
            if attempt > 0 {
                println!("multi-agent: (FT index caught up after {} poll(s))", attempt + 1);
            }
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    anyhow::bail!(
        "FT index did not reach {min_hits} hit(s) for probe {probe:?} under scope `{}` within ~5s",
        scope.as_str()
    );
}
