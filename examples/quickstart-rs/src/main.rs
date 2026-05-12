//! Lunaris 10-minute quickstart — Rust.
//!
//! Demonstrates the v0.2.x public API surface against a local Postgres
//! backend: open a handle, ingest one episode under a scope, recall it.
//!
//! ## Prerequisites
//!
//! 1. `docker compose up -d` (from this directory) — starts Postgres
//!    16 + pgvector + pgmq + Apache AGE on `localhost:5432`.
//! 2. `ollama serve &` and `ollama pull nomic-embed-text` — Ollama at
//!    `http://localhost:11434` for the embedder. (Or rebuild with
//!    `--no-default-features --features candle` for in-process candle.)
//! 3. `export LUNARIS_PG_URL="postgres://lunaris:lunaris@localhost:5432/lunaris"`.
//! 4. `cargo run`.
//!
//! Expected output:
//!
//! ```text
//! quickstart: opening lunaris handle at postgres://...
//! quickstart: ingested episode at lsn=Lsn(1) under scope `quickstart`
//! quickstart: recalled 1 hit(s) for "hello"
//! quickstart:   top hit score=0.83 text="# Hello from Lunaris …"
//! ```
//!
//! See `examples/quickstart-rs/README.md` for the full runbook + a
//! Postgres-only no-Ollama variant that uses a hand-wired stub
//! embedder via the (internal) `with_parts` API.

use std::env;

use anyhow::{Context, Result};
use lunaris::{EpisodeBuilder, Lunaris, Scope, Vector};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let pg_url = env::var("LUNARIS_PG_URL")
        .context("set LUNARIS_PG_URL — see examples/quickstart-rs/README.md")?;

    println!("quickstart: opening lunaris handle at {pg_url}");
    let lunaris = Lunaris::open(&pg_url).await.context("Lunaris::open failed")?;

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
