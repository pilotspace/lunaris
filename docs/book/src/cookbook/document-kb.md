# Document Knowledge Base

**Reach for `DocumentKnowledgeBase` when you have a corpus of documents and
want hybrid RAG over it — semantic + BM25 fused with RRF — with metadata
filters and a result cap.**

`DocumentKnowledgeBase`
(`crates/lunaris-recipes/src/documentary/document_knowledge_base.rs`) is a
thin wrapper over [`DocumentCorpus`](./index.md#documentcorpus--hybrid-vector--keyword-rag).
It exists to make "knowledge base over a document source" discoverable from
the public surface; every method forwards into at most one primitive call
and there is no business logic of its own.

| Method | Signature | Notes |
|---|---|---|
| `new` | `fn new(lunaris: Arc<Lunaris>, source_prefix: impl Into<String>) -> Self` | binds the inner `DocumentCorpus` to `source_prefix` (e.g. `"kb:docs/"`) |
| `ingest` | `async fn ingest(chunks: Vec<(String, serde_json::Map<String, serde_json::Value>)>) -> Result<(), LunarisError>` | each `(content, metadata)` pair → one `Episode` under `{prefix}{ulid}` |
| `filter` | `fn filter(self, field: impl Into<String>, value: impl Into<serde_json::Value>) -> Self` | adds a `Filter::Eq` on a metadata field; multiple calls AND together; **consumes `self`** |
| `top` | `fn top(self, k: usize) -> Self` | caps output; default `10`; **consumes `self`** |
| `search` | `async fn search(self, query: &str) -> Result<Vec<Hit>, LunarisError>` | **consumes `self`**; fans out a `Vector + Keyword(BM25) ⊕ RRF(60)` plan with a generous over-fetch, executes, then prunes to the source prefix and caps at `k` |

`search` returns `Vec<Hit>` ranked by the fused RRF score. The
Moon-native-RRF vs Postgres-client-side-merge branch lives inside
`RetrievalBuilder::execute` — the wrapper is pure plan composition. Source
prefix scoping runs post-hydrate (the `chunks` FT schema does not carry
`source`), so a modest over-fetch is applied before pruning to the corpus's
prefix.

If your documents are pre-chunked already, pass them directly; otherwise
chunk them upstream (the umbrella ingest pipeline's markdown chunker targets
~500 tokens with 100-token overlap — see [Ingesting
Observations](../guides/ingest.md)).

## Example

Shaped after `document_knowledge_base_parity_quickstart_rag` in
`crates/lunaris-recipes/tests/documentary_parity.rs`:

```rust,no_run
use std::sync::Arc;
use lunaris::Lunaris;
use lunaris_recipes::documentary::DocumentKnowledgeBase;

#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    let lunaris = Arc::new(Lunaris::open("moon://127.0.0.1:6380").await?);

    let kb = DocumentKnowledgeBase::new(lunaris.clone(), "kb:docs/");

    // Ingest pre-chunked content with metadata.
    let chunks = vec![
        (
            "Lunaris commits all storage fan-out in a single atomic_write.".to_string(),
            serde_json::Map::from_iter([
                ("lang".to_string(), serde_json::json!("en")),
                ("section".to_string(), serde_json::json!("architecture")),
            ]),
        ),
        (
            "The retrieval DSL fuses Vector, Keyword, and Graph operators via RRF.".to_string(),
            serde_json::Map::from_iter([
                ("lang".to_string(), serde_json::json!("en")),
                ("section".to_string(), serde_json::json!("retrieval")),
            ]),
        ),
    ];
    kb.ingest(chunks).await?;

    // Hybrid RAG: filter on metadata, cap results, search.
    let hits = kb
        .filter("lang", "en")
        .top(5)
        .search("how does Lunaris guarantee atomicity?")
        .await?;

    for h in &hits {
        println!("score={:.3} source={} text={}", h.score, h.source, h.text);
    }

    Ok(())
}
```

Swap the URL for `moon://localhost:6380` — same code, same result set
(parity contract).

## Notes

- **Builder methods consume `self`.** `kb.filter(..).top(..).search(..)` is
  the idiom; you can't reuse a `DocumentKnowledgeBase` after calling
  `search`.
- **`filter` is metadata-`Eq` only** (`Filter::Eq` from `lunaris-core` is
  canonical — never build SQL `WHERE` strings). Multiple `.filter` calls AND
  together. For valid-time filtering use [`TimelineReconstruction`](./timeline.md)
  or `TemporalQuery` directly.
- **No batch-ingest helper** — `ingest` issues one internal `atomic_write`
  per chunk. A true batched bulk-ingest path is a post-v0.1 addition; for
  large static corpora today, prefer the bench-crate bulk helpers (see the
  RAG scenario in [Helios Scratchpad](./helios-scratchpad.md)) or accept the
  per-chunk write cost.
- For citation-graph-aware paper corpora, see
  [`ResearchPaperCorpus`](./research-and-code.md).
