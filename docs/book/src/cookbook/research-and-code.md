# Research Papers & Code Repos

**Reach for `ResearchPaperCorpus` when you want a paper corpus with an
opt-in citation graph; reach for `CodeRepoMemory` when you want
"function body as-of commit N" — point-in-time recall over committed code.**

Both wrap [`DocumentCorpus`](./index.md#documentcorpus--hybrid-vector--keyword-rag)
on the ingest side; `CodeRepoMemory` adds [`TemporalQuery<Documents>`](./index.md#temporalquerys--typestate-time-travel)
on the recall side.

---

## `ResearchPaperCorpus`

`crates/lunaris-recipes/src/documentary/research_paper_corpus.rs` — a
`DocumentCorpus` plus an opt-in citation graph. The graph-on path toggles
`Lunaris::graph_pipeline().enable()` (the graph defaults OFF per blueprint
§5.2; opt in via the explicit builder call).

| Method | Signature | Notes |
|---|---|---|
| `new` | `fn new(lunaris: Arc<Lunaris>, source_prefix: impl Into<String>) -> Self` | binds the inner corpus (e.g. `"papers:"`) |
| `with_graph_pipeline` | `fn with_graph_pipeline(self, on: bool) -> Self` | `enable()` / `disable()` on the graph handle; idempotent; builder-style; **consumes `self`** |
| `ingest` | `async fn ingest(chunks: Vec<(String, serde_json::Map<String, serde_json::Value>)>) -> Result<(), LunarisError>` | forwards to `DocumentCorpus::ingest` |
| `search` | `async fn search(self, query: &str) -> Result<Vec<Hit>, LunarisError>` | forwards to `DocumentCorpus::search`; **consumes `self`** |

Put the paper's id / venue / year in the chunk metadata so a later
metadata-`Eq` filter (via the underlying `DocumentCorpus`) can narrow by
year or venue. To get citation edges in the graph, call
`with_graph_pipeline(true)` **before** ingest — extraction runs inside the
ingest hot path.

### Example

Shaped after `research_paper_corpus_parity_graph_off_recall` in
`crates/lunaris-recipes/tests/documentary_parity.rs`:

```rust,no_run
use std::sync::Arc;
use lunaris::Lunaris;
use lunaris_recipes::documentary::ResearchPaperCorpus;

#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    let lunaris = Arc::new(Lunaris::open("moon://localhost:6379").await?);

    // Opt in to the citation graph before ingest. Default is OFF.
    let papers = ResearchPaperCorpus::new(lunaris.clone(), "papers:")
        .with_graph_pipeline(true);

    let chunks = vec![
        (
            "Reciprocal Rank Fusion outperforms Condorcet fusion on TREC runs.".to_string(),
            serde_json::Map::from_iter([
                ("paper_id".to_string(), serde_json::json!("cormack2009")),
                ("year".to_string(), serde_json::json!(2009)),
            ]),
        ),
        (
            "ACT-R base-level activation models declarative memory decay.".to_string(),
            serde_json::Map::from_iter([
                ("paper_id".to_string(), serde_json::json!("anderson1996")),
                ("year".to_string(), serde_json::json!(1996)),
            ]),
        ),
    ];
    papers.ingest(chunks).await?;

    let hits = papers.search("how is rank fusion evaluated?").await?;
    for h in &hits {
        println!("score={:.3} source={} text={}", h.score, h.source, h.text);
    }

    Ok(())
}
```

---

## `CodeRepoMemory`

`crates/lunaris-recipes/src/documentary/code_repo_memory.rs` — models
"function body as-of commit N". Each commit is ingested once per chunk with
`commit_sha` and `committer_date_unix_ms` stamped into the `Episode`
metadata; recall time-travels via `TemporalQuery::<Documents>::as_of`.

| Method | Signature | Notes |
|---|---|---|
| `new` | `fn new(lunaris: Arc<Lunaris>, repo_prefix: impl Into<String>) -> Self` | binds the inner corpus (e.g. `"repo:lunaris/"`) |
| `ingest_commit` | `async fn ingest_commit(commit_sha: impl Into<String>, committer_date_unix_ms: i64, chunks: Vec<(String, serde_json::Map<String, serde_json::Value>)>) -> Result<(), LunarisError>` | stamps `commit_sha` + `committer_date_unix_ms` into each chunk's metadata, then forwards to `DocumentCorpus::ingest` (1 primitive call) |
| `recall` | `async fn recall(query: &str, as_of: Hlc) -> Result<Vec<Hit>, LunarisError>` | 2 primitive calls: `TemporalQuery::<Documents>::new` + `.as_of(ts).execute(query)` |

`Hlc`'s native shape is Unix-milliseconds (`{wall_ms: u64, counter: u32,
node_id: u16}`) — there is nowhere on the `Hlc` surface for RFC3339-nanos.
`ingest_commit` takes the committer date as an `i64` of Unix-ms;
`recall(..., as_of: Hlc)` takes the `Hlc` directly, so you control the
counter / node-id disambiguation in dense-commit scenarios. Build one with
`Hlc::from_parts(unix_ms as u64, 0, 0)`.

> `TemporalQuery` recalls across **all** Documents — `CodeRepoMemory` does
> not partition by repo at the recall layer. If you store more than one
> repo on the same handle, isolate them with distinct prefixes and a
> metadata filter, or use separate handles. The parity tests are
> fixture-isolated.

### Example

Shaped after `code_repo_memory_as_of_commit_50_round_trip_moon_postgres` in
`crates/lunaris-recipes/tests/documentary_rust_integration.rs`:

```rust,no_run
use std::sync::Arc;
use lunaris::Lunaris;
use lunaris_core::hlc::Hlc;
use lunaris_recipes::documentary::CodeRepoMemory;

#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    let lunaris = Arc::new(Lunaris::open("postgres://lunaris@localhost/lunaris").await?);
    let repo = CodeRepoMemory::new(lunaris.clone(), "repo:lunaris/");

    // Commit A — first version of the function.
    let commit_a_ms: i64 = 1_715_000_000_000; // committer date, Unix-ms
    repo.ingest_commit(
        "a1b2c3",
        commit_a_ms,
        vec![(
            "fn recall(query: &str) -> Vec<Hit> { /* v1 */ }".to_string(),
            serde_json::Map::from_iter([("path".to_string(), serde_json::json!("src/recall.rs"))]),
        )],
    ).await?;

    // Commit B — the function changed.
    let commit_b_ms: i64 = 1_715_600_000_000;
    repo.ingest_commit(
        "d4e5f6",
        commit_b_ms,
        vec![(
            "fn recall(query: &str, as_of: Hlc) -> Vec<Hit> { /* v2 */ }".to_string(),
            serde_json::Map::from_iter([("path".to_string(), serde_json::json!("src/recall.rs"))]),
        )],
    ).await?;

    // Time-travel: what did `recall` look like at commit A's timestamp?
    let as_of_a = Hlc::from_parts(commit_a_ms as u64, 0, 0);
    let hits = repo.recall("recall function signature", as_of_a).await?;
    for h in &hits {
        println!("source={} text={}", h.source, h.text);
    }

    Ok(())
}
```

## Notes

- **`recall` has no builder** — `as_of` is a positional `Hlc` argument. Use
  `Hlc::from_parts(unix_ms as u64, 0, 0)` (or capture a causal timestamp
  from `lunaris.clock().tick()`).
- **`ingest_commit` is one `atomic_write` per chunk**, batched per
  invocation — preserves the INGEST-04 contract at the per-chunk grain.
- **Graph posture** — only `ResearchPaperCorpus` exposes a graph toggle;
  `CodeRepoMemory` does not. For commit-graph-style traversal, drop to the
  `Lunaris` handle and compose `Graph::anchored(...)` yourself — see
  [The Graph Pipeline](../guides/graph.md).
- For a corpus *without* either citation graph or time-travel, use
  [`DocumentKnowledgeBase`](./document-kb.md).
