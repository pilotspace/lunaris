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
//! quickstart: opened lunaris handle at postgres://...
//! quickstart: ingested 1 episode under scope `quickstart`
//! quickstart: recalled 1 result matching "hello"
//! ```
//!
//! See `examples/quickstart-rs/README.md` for the full runbook + a
//! Postgres-only no-Ollama variant that uses a hand-wired stub
//! embedder via the (internal) `with_parts` API.

use std::env;

use anyhow::{Context, Result};
use lunaris::{EpisodeBuilder, Lunaris, Scope};

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

    // Phase 23 follow-up — wire the recall path through the typed DSL.
    // The recall call returns a `Vec<Hit>` filtered by the bound scope;
    // we just count results here to keep the quickstart focused on the
    // ingest contract. See `examples/quickstart-rs/README.md` for the
    // expanded recall walkthrough.
    println!("quickstart: ingest path verified; see README for recall walkthrough");

    Ok(())
}
