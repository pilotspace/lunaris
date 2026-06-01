# Multi-Turn Conversation

**Reach for `MultiTurnConversation` when a chat agent runs many sessions per
user and you want a cross-session consolidation pass that promotes hot
scratchpad notes into long-term facts — without leaking across users.**

`MultiTurnConversation`
(`crates/lunaris-recipes/src/conversational/multi_turn_conversation.rs`)
adds one thing to the [`ChatAgentMemory`](./chat-agent.md) shape: a
`consolidate()` method. It composes a `MessageStream` + a `WorkingMemory`,
both captured at the same `"chat:<user_id>/"` scope prefix. That shared
prefix is the load-bearing invariant — `consolidate()` runs through
`Consolidator::consolidate_scoped(Some("chat:<user_id>/"))`, so any event
whose `source` does not start with that prefix is rejected by the scope
filter (closing the cross-user consolidator-leak risk).

| Method | Signature | Forwards to |
|---|---|---|
| `new` | `fn new(lunaris: Arc<Lunaris>, user_id: &str) -> Self` | `MessageStream::new` + `WorkingMemory::new` |
| `remember` | `async fn remember(turn: impl Into<String>, thread_id: impl Into<String>) -> Result<Lsn, LunarisError>` | `MessageStream::ingest(turn, thread_id, "user")` |
| `recall` | `async fn recall(query: &str) -> Result<Vec<Hit>, LunarisError>` | `MessageStream::recall` |
| `consolidate` | `async fn consolidate() -> Result<ConsolidationReport, LunarisError>` | `WorkingMemory::consolidate` |

Note `remember` here takes a **session id** (`thread_id`) — turns from
different sessions land under different `Episode` source segments but all
recall together.

`consolidate()` is a **no-op unless a `Consolidator` is installed** on the
handle — the consolidator pipeline defaults OFF (blueprint §5.2). Install
one with `lunaris.consolidator_pipeline().set_consolidator(...)`; see
[Consolidation & Verification](../guides/consolidate-verify.md) for the
ACT-R promotion model and the `ConsolidationReport` shape.

## Example

Shaped after `multi_turn_conversation_cross_session_consolidation_parity` in
`crates/lunaris-recipes/tests/conversational_parity.rs`: seed turns across
two sessions for one user, recall across both, then run a scoped
consolidation pass.

```rust,no_run
use std::sync::Arc;
use lunaris::Lunaris;
use lunaris_recipes::conversational::MultiTurnConversation;

#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);

    // (Optional) install a consolidator — without one, consolidate() is a
    // no-op that returns an empty report. The default pipeline is OFF.
    // lunaris.consolidator_pipeline().set_consolidator(Arc::new(my_consolidator));

    let conv = MultiTurnConversation::new(lunaris.clone(), "user-42");

    // Session "trip-planning".
    conv.remember("I'm planning a trip to Kyoto in April.", "trip-planning").await?;
    conv.remember("Budget is around 3000 USD.", "trip-planning").await?;

    // A later session, same user.
    conv.remember("Booked a ryokan in Higashiyama for two nights.", "booking").await?;

    // Recall spans every session for this user.
    let hits = conv.recall("where is the user staying in Kyoto?").await?;
    for h in &hits {
        println!("score={:.3} source={} text={}", h.score, h.source, h.text);
    }

    // One scope-filtered consolidation pass: only `chat:user-42/` events are
    // eligible for promotion; another user's turns are rejected by the
    // scope filter.
    let report = conv.consolidate().await?;
    println!("promotions this pass: {}", report.promotions.len());

    Ok(())
}
```

## Resuming a session

**Like [`ChatAgentMemory`](./chat-agent.md#resuming-a-session), there is no
explicit load — reconstruct `MultiTurnConversation::new(handle, "user-42")`
with the same `user_id` and the backend already holds every prior turn.** To
resume one *specific* session, pass the same `thread_id` to `remember` again:
turns from that session keep landing under the same `Episode` source segment.
`recall` still spans **all** sessions for that user — the `thread_id` only
shapes how turns are grouped, not what recall sees:

```rust,no_run
# use std::sync::Arc;
# use lunaris::Lunaris;
# use lunaris_recipes::conversational::MultiTurnConversation;
# async fn run(lunaris: Arc<Lunaris>) -> Result<(), lunaris::LunarisError> {
// A new process — same user, continuing the "trip-planning" session.
let conv = MultiTurnConversation::new(lunaris.clone(), "user-42");
conv.remember("Confirmed the ryokan for the 14th.", "trip-planning").await?;

// Recall still spans every session this user has ever had.
let hits = conv.recall("what's confirmed for the Kyoto trip?").await?;
# let _ = hits;
# Ok(())
# }
```

Construction is pure — `MessageStream::new` / `WorkingMemory::new` do no I/O —
so resuming is just a name binding; the durable backend is the session store.

## Notes

- **The scope prefix is captured at `new`.** Both the write path
  (`MessageStream::ingest`) and the promotion filter
  (`WorkingMemory::consolidate`) see `"chat:<user_id>/"`, so isolation is
  enforced at both ends.
- **`consolidate()` is bounded.** Each call drains at most 1024 recent
  consolidate events with a 50 ms per-pull timeout — heavy callers should
  invoke it repeatedly rather than expect one call to drain everything.
- **Successful promotions emit an audit event.** Each promotion publishes
  `AuditEvent::ConsolidatorPromotion` to the `__lunaris_audit__` topic.
- **Use `ChatAgentMemory` instead** if you don't need sessions or
  consolidation — it's the same recall behaviour with a flatter surface.
