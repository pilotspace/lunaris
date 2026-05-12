# Customer Support History

**Reach for `CustomerSupportHistory` when your support data is split across
two shapes — ticket bodies *and* chat transcripts — and you want one
`recall` call that returns hits from both.**

`CustomerSupportHistory`
(`crates/lunaris-recipes/src/documentary/customer_support_history.rs`)
composes a [`DocumentCorpus`](./index.md#documentcorpus--hybrid-vector--keyword-rag)
for ticket bodies (`source = "ticket:<ulid>"`, `ticket_id` lands in metadata)
and a [`MessageStream`](./index.md#messagestream--recency-weighted-message-recall)
for chat transcripts (`source = "chat:<ticket_id>/<turn_idx>/"`), plus an opt-in
[graph pipeline](../guides/graph.md) toggle for a product/customer
relationship graph.

| Method | Signature | Notes |
|---|---|---|
| `new` | `fn new(lunaris: Arc<Lunaris>) -> Self` | **no prefix arg** — prefixes `ticket:` and `chat:` are hard-coded |
| `with_graph_pipeline` | `fn with_graph_pipeline(self, on: bool) -> Self` | `enable()` / `disable()` on the graph handle; builder-style; **consumes `self`**; default OFF |
| `ingest_ticket` | `async fn ingest_ticket(ticket_id: impl Into<String>, body: impl Into<String>) -> Result<(), LunarisError>` | `ticket_id` is stamped into metadata; 1 primitive call (`DocumentCorpus::ingest`) |
| `ingest_chat` | `async fn ingest_chat(ticket_id: impl Into<String>, turn_idx: usize, participant: impl Into<String>, msg: impl Into<String>) -> Result<Lsn, LunarisError>` | `turn_idx` is **`usize`**; the `ticket_id/turn_idx` slug becomes the `MessageStream` thread id so chats cluster per ticket; 1 primitive call (`MessageStream::ingest`) |
| `recall` | `async fn recall(query: &str) -> Result<Vec<Hit>, LunarisError>` | 2 primitive calls (`DocumentCorpus::search` + `MessageStream::recall`); returns the **concatenation** of (ticket hits, chat hits) |

### How `recall` fuses the two buckets

It doesn't fuse across types. RRF runs **within** each primitive's own
bucket — ticket hits are RRF-fused among themselves, chat hits among
themselves — and `recall` returns `tickets ++ chats`. Ordering across the
two buckets is not normalised (tie-bucket behaviour is deferred). Each
returned hit is checked to carry its expected source prefix (`ticket:` vs
`chat:`) — a record double-indexed under both prefixes would be caught here
rather than silently collapsing duplicates.

> The prefixes are hard-coded — this wrapper is a named recipe, not a
> general composer. If you need different prefixes, compose `DocumentCorpus`
> + `MessageStream` directly.

## Example

Shaped after the `"refund"` recall scenario in
`crates/lunaris-recipes/tests/documentary_rust_integration.rs`:

```rust,no_run
use std::sync::Arc;
use lunaris::Lunaris;
use lunaris_recipes::documentary::CustomerSupportHistory;

#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    let lunaris = Arc::new(Lunaris::open("postgres://lunaris@localhost/lunaris").await?);
    let hist = CustomerSupportHistory::new(lunaris.clone());

    // Ticket bodies → DocumentCorpus under `ticket:<id>`.
    hist.ingest_ticket("T-1042", "Customer requests a refund for order #5567 — duplicate charge.").await?;
    hist.ingest_ticket("T-1043", "Shipment delayed; customer asks for partial refund.").await?;

    // Chat transcripts → MessageStream under `chat:<ticket_id>/<turn>`.
    hist.ingest_chat("T-1042", 0, "customer", "Hi, I was charged twice for order #5567.").await?;
    hist.ingest_chat("T-1042", 1, "agent",    "Apologies — I've issued a full refund, 3-5 business days.").await?;
    hist.ingest_chat("T-1042", 2, "customer", "Thank you!").await?;

    // One recall across BOTH buckets — returns ticket hits ++ chat hits.
    let hits = hist.recall("refund for a duplicate charge").await?;
    for h in &hits {
        let bucket = if h.source.starts_with("ticket:") { "ticket" } else { "chat" };
        println!("[{bucket}] score={:.3} source={} text={}", h.score, h.source, h.text);
    }

    Ok(())
}
```

Swap the URL for `moon://localhost:6379` — the parity test asserts both
backends return the same set and that source prefixes are preserved.

## Notes

- **`turn_idx` is `usize`**, not `u32` — pass a plain index.
- **`recall` returns a concatenation, not a globally ranked list.** If you
  need a single ranked stream across tickets and chats, re-rank the
  combined `Vec<Hit>` yourself (e.g. by `Hit::score`) or use the
  cross-encoder reranker on a hand-composed plan — see
  [The Retrieval DSL](../guides/retrieval-dsl.md).
- **Graph is opt-in and ingest-time.** Call `with_graph_pipeline(true)`
  before ingesting if you want product/customer edges.
- **GDPR / retention** — to purge one customer's data, route through
  `Lunaris::forget(ForgetTarget::Scope(...))` on the relevant `ticket:` /
  `chat:` prefixes; see [Forgetting](../guides/forget.md).
