//! Lunaris 10-minute quickstart — Rust.
//!
//! Demonstrates the public API surface against a local Moon: open a
//! handle, ingest one episode under a scope, recall it.
//!
//! Through 0.6.x this quickstart targeted Postgres + pgvector. 0.7.0 is
//! Moon-only — see `docs/migration/0.6-to-0.7.md`.
//!
//! ## Prerequisites
//!
//! 1. `docker compose up -d` (from this directory) — starts Moon on
//!    `localhost:6380` with `--shards 1` (REQUIRED: Lunaris ingest is a
//!    single MULTI/EXEC TXN and Moon refuses cross-shard writes).
//! 2. Stage the granite-r2 Q4_K_M GGUF at `~/.lunaris/models/` for the
//!    default in-process llama.cpp embedder. (Or `ollama serve &` with
//!    `--features embed-remote` + `LUNARIS_EMBEDDER_OLLAMA_URL` as the
//!    air-gap escape hatch.)
//! 3. `export LUNARIS_STORE_URL="moon://127.0.0.1:6380"`.
//! 4. `cargo run`.
//!
//! Expected output:
//!
//! ```text
//! quickstart: opening lunaris handle at moon://127.0.0.1:6380
//! quickstart: ingested episode at lsn=Lsn(1) under scope `quickstart`
//! quickstart: recalled 1 hit(s) for "hello"
//! quickstart:   top hit score=0.83 text="# Hello from Lunaris …"
//! ```
//!
//! See `examples/quickstart-rs/README.md` for the full runbook.

use std::env;

use anyhow::{Context, Result};
use lunaris::{EpisodeBuilder, Lunaris, Scope, Vector};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // No default on purpose: guessing `moon://127.0.0.1:6380` would let the
    // quickstart write demo episodes into whatever Moon happens to own that
    // port — which on a developer box is often a real store.
    let store_url = env::var("LUNARIS_STORE_URL")
        .context("set LUNARIS_STORE_URL=moon://127.0.0.1:6380 — see examples/quickstart-rs/README.md")?;

    println!("quickstart: opening lunaris handle at {store_url}");
    let lunaris = Lunaris::open(&store_url).await.context("Lunaris::open failed")?;

    let scope = Scope::new("quickstart").context("Scope::new")?;
    let scoped = lunaris.scoped(scope);

    let episode = EpisodeBuilder::new(
        "quickstart:demo",
        "# Hello from Lunaris\n\nThis is your first episode.",
    );
    let lsn = scoped.ingest(episode).await.context("scoped.ingest failed")?;
    println!("quickstart: ingested episode at lsn={lsn:?} under scope `quickstart`");

    // Recall through the scope-bound handle. `ScopedLunaris::recall` takes a
    // `Query` and returns `Vec<Hit>` already filtered to this scope's
    // partition; the default retrieval root is `Vector::new("chunks", 30)`.
    let query = "hello";
    let hits = scoped
        .recall(lunaris::Query::text(query))
        .await
        .context("scoped.recall failed")?;
    println!("quickstart: recalled {} hit(s) for {query:?}", hits.len());
    if let Some(top) = hits.first() {
        // Trim the chunk body to one line so the demo output stays tidy.
        let snippet: String = top.text.chars().take(60).collect();
        println!("quickstart:   top hit score={:.2} text={snippet:?}", top.score);
    }

    // Equivalent DSL form — explicit operator tree, capped at 5 hits:
    let dsl_hits = scoped
        .dsl()
        .with_root(Vector::new("chunks", 30).top(5))
        .execute(lunaris::Query::text(query))
        .await
        .context("scoped.dsl().execute failed")?;
    println!("quickstart: DSL form returned {} hit(s)", dsl_hits.len());

    Ok(())
}
