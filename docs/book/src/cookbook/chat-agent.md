# Chat Agent Memory

**Reach for `ChatAgentMemory` when you want a single chat agent to
`remember` turns and `recall` them per user, with nothing else to wire.**

`ChatAgentMemory` (`crates/lunaris-recipes/src/conversational/chat_agent_memory.rs`)
is the thinnest conversational wrapper: a per-user `MessageStream` + a
per-user `WorkingMemory`, both bound to the same `"chat:<user_id>/"` scope
prefix (the shared prefix is what keeps a later consolidator pass from
leaking across users). It exposes three methods:

| Method | Signature | Forwards to |
|---|---|---|
| `new` | `fn new(lunaris: Arc<Lunaris>, scope: Scope, user_id: &str) -> Self` | `MessageStream::new` + `WorkingMemory::new` |
| `remember` | `async fn remember(turn: impl Into<String>) -> Result<Lsn, LunarisError>` | `MessageStream::ingest(turn, "default", "user")` |
| `recall` | `async fn recall(query: &str) -> Result<Vec<Hit>, LunarisError>` | `MessageStream::recall` |

Chat sessions are a **flat stream** in this wrapper — `thread_id` is always
`"default"`. If you need per-session partitioning, use
[`MultiTurnConversation`](./multi-turn.md) instead.

Recall ordering is the ACT-R recency-weighted blend inherited from
`MessageStream::recall`: the fused `Vector + Keyword ⊕ RRF(60)` score is
summed with an Anderson-1996 base-level activation term (`d = 0.5`), so a
turn from a minute ago outranks a same-relevance turn from an hour ago.

## Example

Shaped after `chat_agent_memory_moon_postgres_parity` in
`crates/lunaris-recipes/tests/conversational_parity.rs`: open a handle,
construct a per-user memory, replay a handful of turns, then recall.

```rust,no_run
use std::sync::Arc;
use lunaris::{Lunaris, Scope};
use lunaris_recipes::conversational::ChatAgentMemory;

#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    // One handle per process — share via Arc. URL scheme picks the backend.
    let lunaris = Arc::new(Lunaris::open("moon://127.0.0.1:6380").await?);
    // The partition every recipe below reads and writes in.
    let scope = Scope::new("acme-support-bot")?;

    let mem = ChatAgentMemory::new(lunaris.clone(), scope.clone(), "user-42");

    // Record conversational turns as they happen.
    mem.remember("I'm planning a trip to Kyoto in April.").await?;
    mem.remember("My budget is around 3000 USD.").await?;
    mem.remember("I'd like a ryokan with an onsen for at least one night.").await?;

    // Later — recall what's relevant to the next prompt.
    let hits = mem.recall("what kind of accommodation does the user want?").await?;
    for h in &hits {
        println!("score={:.3} source={} text={}", h.score, h.source, h.text);
    }

    Ok(())
}
```

Swap the URL for `moon://localhost:6380` and the code is byte-for-byte
identical — that is the parity contract.

## Resuming a session

**There is no explicit "load session" step — you resume by reconstructing
`ChatAgentMemory` with the *same* `user_id`.** The wrapper is a stateless
handle over durable storage: `ChatAgentMemory::new` does no I/O, it just builds
the `"chat:<user_id>/"` scope prefix; every prior `remember` already wrote an
`Episode` into the backend keyspace (`lunaris:{scope}:{kind}:{ulid}`). So a
fresh process — a new request handler, a restarted service, a different machine
— that constructs the same id immediately recalls the full history:

```rust,no_run
use std::sync::Arc;
use lunaris::{Lunaris, Scope};
use lunaris_recipes::conversational::ChatAgentMemory;

#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    // A brand-new process. Nothing was kept in memory between runs.
    let lunaris = Arc::new(Lunaris::open("moon://127.0.0.1:6380").await?);
    // The partition every recipe below reads and writes in.
    let scope = Scope::new("acme-support-bot")?;

    // Same user id as before ⇒ same `"chat:user-42/"` scope ⇒ same memory.
    // `new` is pure — the first storage round-trip is the `recall` below.
    let mem = ChatAgentMemory::new(lunaris.clone(), scope.clone(), "user-42");

    let prior = mem.recall("what has the user told me so far?").await?;
    for h in &prior {
        println!("score={:.3} text={}", h.score, h.text);
    }
    // ... then carry on: `mem.remember(...)` appends to the same history.
    Ok(())
}
```

Same id ⇒ same memory; that is the whole contract. There is nothing to
serialize, snapshot, or hand back between turns — the backend is the session
store, and constructing the wrapper is just a name binding.

## Notes

- **One `ChatAgentMemory` per user.** Construction is pure (no I/O); the
  first storage round-trip happens on the first `remember` / `recall`.
- **`new` takes `&str` for the user id**, not `impl Into<String>` — the
  scope prefix is built immediately as `"chat:<user_id>/"`.
- **No `consolidate()` here.** The wrapper holds a `WorkingMemory` for
  future additive surface, but the cross-session promotion pass is
  `MultiTurnConversation`'s differentiator. See [Multi-Turn
  Conversation](./multi-turn.md).
- **Multi-agent scoping.** The `"chat:<user_id>/"` prefix isolates one
  user's turns inside this `MessageStream` — but `MessageStream` builds
  episodes with `Scope::dev()`, so that is *source-prefix* isolation, not a
  tenant wall. For RLS-grade per-agent isolation (separate agent platforms,
  the HTTP `tenant` claim, the low-level `lunaris.scoped(scope)` handle), see
  [Multi-Agent & Scope → Multi-agent patterns](../guides/multi-agent.md#multi-agent-patterns)
  and the runnable [`examples/multi-agent-rs/`](https://github.com/pilotspace/lunaris/tree/main/examples/multi-agent-rs).
- **Embedder / backend tuning** lives in the
  [Configuration Reference](../reference/configuration.md) — the recipe adds
  no knobs of its own beyond `MessageStream::with_top_k` on the underlying
  primitive (not surfaced on `ChatAgentMemory`).
