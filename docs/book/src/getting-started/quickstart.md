# 10-Minute Quickstart

**From a fresh checkout to your first ingest + recall against a local
Postgres backend in under ten minutes — Rust is canonical, with Python
and TypeScript mirrors side by side.** This targets the v0.2.x OSS
milestone: Postgres + pgvector as the backend, no Moon required. (The
default embedder is in-process llama.cpp granite-r2 (GGUF) — no Ollama
needed for embedding. The shipped `examples/quickstart-rs` crate opts into
the `ollama` build to enable the Ollama **extractor + verifier** path —
see step 2.)

> **API note.** This chapter uses the retrieval surface that exists in
> the v0.2.x source: `ScopedLunaris::recall(Query::text(...))` returns
> `Vec<Hit>` directly, and `ScopedLunaris::dsl()` (or the bare
> `Lunaris::recall()`) returns a `RetrievalBuilder` you drive with
> `.with_root(...)` + `.execute(Query::text(...))`. A fluent shorthand
> like `recall().vector("chunks", 30).top(5).execute()` is a planned
> ergonomic wrapper that is **not yet on the builder** — `RetrievalBuilder`
> has no `.vector(...)` method today. When in doubt, the Rust source wins.

## 0. Get the code

```bash
git clone https://github.com/pilotspace/lunaris && cd lunaris
```

## 1. Bring up Postgres

The `examples/quickstart-*/` directories share one Postgres image
(`postgres:16` + pgvector + pgmq + Apache AGE), built from
`scripts/pg-lunaris/`:

```bash
cd examples/quickstart-rs
docker compose up -d
docker compose ps        # wait until lunaris-quickstart-pg is "healthy"
```

Apply the schema (the quickstart binary doesn't run DDL itself):

```bash
sqlx migrate run --source ../../crates/lunaris-storage-postgres/migrations \
                 --database-url postgres://lunaris:lunaris@localhost:5432/lunaris
```

## 2. Point Lunaris at it

```bash
export LUNARIS_PG_URL="postgres://lunaris:lunaris@localhost:5432/lunaris"
```

The default embedder is **granite-embedding-311m-multilingual-r2** (768-d),
loaded from a Q4_K_M GGUF **in-process via llama.cpp** (staged at
`~/.lunaris/models/`) — **no Ollama needed for embedding**.

The shipped `examples/quickstart-rs` crate pins `features = ["ollama"]`
in its `Cargo.toml` to enable the Ollama **extractor + verifier** path
(the smallest external-dep build that exercises a real extraction + verification
flow). The embedder and reranker remain in-process llama.cpp regardless. So
for this walkthrough, start Ollama and pull the extractor model:

```bash
ollama serve &
ollama pull gemma3:4b
```

Prefer a cloud extractor instead of Ollama? Set
`LUNARIS_EXTRACT_PROVIDER=minimax` (or `anthropic`/`openai`/`gemini`) and the
matching API key — extraction and verification are remote-only in v0.6, so
there is no all-in-process alternative to Ollama for those two stages.

## 3–6. Open a handle, ingest, recall, forget

What follows is the canonical Rust flow; the Python and TypeScript
mirrors come right after.

### Rust

```rust
use std::env;

use anyhow::{Context, Result};
use lunaris::{EpisodeBuilder, ForgetTarget, Lunaris, Query, Scope, ScopeSpec, Vector};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // 3. Open a handle. The URL scheme picks the backend
    //    (postgres:// here; moon://host:port for Moon).
    let pg_url = env::var("LUNARIS_PG_URL")
        .context("set LUNARIS_PG_URL — see examples/quickstart-rs/README.md")?;
    let lunaris = Lunaris::open(&pg_url).await.context("Lunaris::open")?;

    // The Scope newtype is the multi-agent partition key (RFC 0001).
    // Scope::new validates the string against [A-Za-z0-9_\-.]{1,128}.
    let scope  = Scope::new("quickstart").context("Scope::new")?;
    let scoped = lunaris.scoped(scope);

    // 4. Ingest one episode. ScopedLunaris::ingest takes an
    //    EpisodeBuilder (scope-less payload); the wrapper stamps the
    //    bound scope on. Exactly one atomic_write per call (INGEST-04).
    let lsn = scoped
        .ingest(EpisodeBuilder::new(
            "quickstart:demo",
            "# Hello from Lunaris\n\nAlice loves chocolate.",
        ))
        .await
        .context("ingest")?;
    println!("ingested at lsn={lsn:?} under scope `quickstart`");

    // 5a. Recall — the one-shot form. ScopedLunaris::recall(query) runs the
    //     default plan (Vector over `chunks`, no fusion/rerank) and returns
    //     Vec<Hit> directly. Least ceremony for a plain semantic lookup.
    let hits = scoped
        .recall(Query::text("who loves chocolate"))
        .await
        .context("recall")?;
    println!("recalled {} hit(s)", hits.len());
    for h in &hits {
        println!("  hit id={:?} score={:.3}", h.id, h.score);
    }

    // 5b. Recall — the composable form. ScopedLunaris::dsl() returns a
    //     RetrievalBuilder pre-seeded with this scope; .with_root sets the
    //     operator tree, .execute runs the plan once and returns Vec<Hit>.
    //     Reach for this when you want hybrid fusion, graph/tree, as_of,
    //     or rerank. (Here: cap the same default Vector plan at top-5.)
    let hits = scoped
        .dsl()
        .with_root(Vector::new("chunks", 30).top(5))
        .execute(Query::text("who loves chocolate"))
        .await
        .context("recall (dsl)")?;
    println!("recalled {} hit(s) via the DSL", hits.len());

    // 6. Forget — soft delete (MVCC: stamps bt.sys_to; prior as_of
    //    reads still see it). A dry-run preview never writes.
    //
    //    NOTE (v0.2.x): Lunaris::forget is hard-coded to Scope::dev()
    //    today, so a forget issued under a real scope like `quickstart`
    //    silently matches zero rows. The dry-run preview below is safe
    //    to run regardless; per-scope ScopedLunaris::forget lands in
    //    v0.3. See CHANGELOG.md "v0.2.0 — Known issues".
    let preview = lunaris
        .forget(ForgetTarget::Scope(ScopeSpec::BySource("quickstart:".into())).dry_run())
        .await
        .context("forget dry-run")?;
    println!("forget preview: preview={} rows_would_write={}", preview.preview, preview.rows_written);

    Ok(())
}
```

Run it:

```bash
cargo run --release
```

Expected output (LSN values vary):

```text
ingested at lsn=Lsn { wall_ms: 1713789012345, counter: 0 } under scope `quickstart`
recalled 1 hit(s)
  hit id=... score=...
forget preview: preview=true rows_would_write=...
```

> **Hybrid recall, one line more.** Add BM25 keyword search and fuse it
> with reciprocal-rank fusion — against a Moon backend `fuse_rrf`
> collapses this to a single round trip; against Postgres it fuses
> client-side. Same API either way:
>
> ```rust
> use lunaris::{Keyword, Vector};
> let hits = scoped
>     .dsl()
>     .with_root(
>         Vector::new("chunks", 30)
>             .and(Keyword::bm25("chunks", 30))
>             .fuse_rrf(60)
>             .top(5),
>     )
>     .execute(Query::text("who loves chocolate"))
>     .await?;
> ```
>
> Add `.rerank(lunaris.reranker())` before `.top(5)` for the
> cross-encoder pass. The full operator catalogue is in
> [The Retrieval DSL](../guides/retrieval-dsl.md).

### Python (`pip install lunaris`)

The typed `Scope` + `EpisodeBuilder` Python surface lands in v0.3; today
the wire shape is a dict that mirrors `lunaris_core::primitives::Episode`
(the `scope` field is required).

```python
import asyncio, os
import lunaris
import ulid  # pip install python-ulid


def build_episode(scope: str, content: str) -> dict:
    return {
        "id": str(ulid.ULID()),
        "scope": scope,
        "source": "quickstart:demo",
        "content": content,
        "t_ref": None,
        "bt": {
            "valid": [{"wall_ms": 0, "counter": 0, "node_id": 0}, None],
            "sys":   [{"wall_ms": 0, "counter": 0, "node_id": 0}, None],
        },
        "metadata": {},
    }


async def main() -> None:
    pg_url = os.environ["LUNARIS_PG_URL"]
    handle = await lunaris.open(pg_url)                       # 3. open
    lsn = await handle.ingest(build_episode("quickstart",     # 4. ingest
                                            "Alice loves chocolate."))
    print(f"ingested at lsn={lsn} under scope `quickstart`")
    # 5. recall — the DSL is reachable from handle.recall(); the typed
    #    Scope binding and the recall walkthrough land alongside the v0.3
    #    SDK story. See examples/quickstart-py/README.md.


asyncio.run(main())
```

Run: `python quickstart.py` (or `maturin develop --release` from
`crates/lunaris-py/` first if you're on a repo checkout). See the
[Python SDK chapter](../sdk/python.md).

### TypeScript (`npm i @pilotspace/lunaris`)

Same story — dict-shaped episode today, typed surface in v0.3.

```typescript
import * as lunaris from "@pilotspace/lunaris";

function buildEpisode(scope: string, content: string): object {
  const ts = Date.now();
  const id = `01${ts.toString(32).toUpperCase().padStart(10, "0")}`
    .padEnd(26, "0").slice(0, 26);
  return {
    id,
    scope,
    source: "quickstart:demo",
    content,
    t_ref: null,
    bt: {
      valid: [{ wall_ms: 0, counter: 0, node_id: 0 }, null],
      sys:   [{ wall_ms: 0, counter: 0, node_id: 0 }, null],
    },
    metadata: {},
  };
}

const handle = await lunaris.open(process.env.LUNARIS_PG_URL!);     // 3. open
const lsn = await handle.ingest(buildEpisode("quickstart",          // 4. ingest
                                             "Alice loves chocolate."));
console.log(`ingested at lsn=${lsn} under scope \`quickstart\``);
// 5. recall — handle.recall() exposes the DSL; the typed-Scope binding
//    and the recall walkthrough land with the v0.3 SDK story. See
//    examples/quickstart-ts/README.md.
```

Run: `npx tsx quickstart.mts` (or `npm run build` from
`crates/lunaris-ts/` first on a repo checkout). See the
[TypeScript SDK chapter](../sdk/typescript.md).

## Tear-down

```bash
docker compose down -v   # -v wipes the pg data volume
```

## What you just did

| Step | Rust | What it is |
|---|---|---|
| Open | `Lunaris::open(url)` | URL scheme picks Postgres vs Moon |
| Scope | `Scope::new("quickstart")?` | the validated multi-agent partition key (RFC 0001) |
| Bind | `lunaris.scoped(scope)` | all ops on the returned `ScopedLunaris` are partitioned |
| Ingest | `scoped.ingest(EpisodeBuilder::new(src, body))` | one `atomic_write`: chunk + embed + commit |
| Recall | `scoped.dsl().with_root(Vector::new("chunks", 30).top(5)).execute(Query::text(q))` | one read pass, returns `Vec<Hit>` |
| Forget | `lunaris.forget(target.dry_run())` | MVCC soft delete + audit event (scoped variant in v0.3) |

## Next

- [Core Concepts](./concepts.md) — the Episode → ingest → storage →
  recall mental model, bi-temporal MVCC, the `Scope` keyspace, the
  single `atomic_write` invariant.
- [The Retrieval DSL](../guides/retrieval-dsl.md) — every operator and
  fusion / rerank / fallback combinator.
- [Ingesting Observations](../guides/ingest.md) — chunking, embedding,
  the graph pipeline.
