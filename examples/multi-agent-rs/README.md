# `lunaris-multi-agent-rs` — multi-agent / multi-session example

A standalone crate (NOT a workspace member) that proves Lunaris's multi-agent
memory model **by running against a live Moon backend**:

1. **Hard isolation between agents** — two `Scope`s under one `Lunaris`
   handle; recall under each scope never returns the other agent's content.
2. **Multiple working sessions inside one agent** — several episodes whose
   `source` encodes a session/task (`conv:mon`, `conv:tue`, `task:deploy`); a
   single recall spans all of them, and you narrow to one session client-side
   over `Hit.source`.
3. **Resume across a process boundary** — `drop(lunaris)`, re-open, re-derive
   the scope, recall — the agent's episodes are still there. Moon is durable;
   there is no explicit load step.

## Get a Moon first

This example needs a **single-shard** Moon. `--shards 1` is mandatory, not a
tuning choice: Lunaris commits each ingest as one MULTI/EXEC transaction, and a
sharded Moon rejects it with `TXN does not support cross-shard writes`.

Lunaris 0.7.0 also refuses any Moon below `0.8.5`, and **v0.8.5 was published
with zero release assets** — the platform tarballs 404 and the ghcr package
answers `401` to an anonymous pull. So there is no binary to download today
(ship-plan task W0.1). Build one from the vendored submodule instead:

```sh
# from the repo root
git submodule update --init vendor/moon
cargo build --release --manifest-path vendor/moon/Cargo.toml --bin moon

mkdir -p /tmp/lunaris-multi-agent-data
vendor/moon/target/release/moon \
    --port 6380 --shards 1 --dir /tmp/lunaris-multi-agent-data
```

Confirm it is up with any Redis client: `redis-cli -p 6380 PING` → `PONG`.

## Run it

```sh
cd examples/multi-agent-rs
export LUNARIS_STORE_URL="moon://127.0.0.1:6380"
cargo run            # or: RUST_LOG=error cargo run   (quieter — hides the consolidate-queue WARN)
```

`LUNARIS_STORE_URL` has **no default on purpose**. This example ingests demo
episodes under two scopes and then asserts on what comes back; a hardcoded
`moon://localhost:6380` would write that data into whatever Moon owns the port
on your machine, which on a developer box is often a real store. If 6380 is
taken, pick another port and change both places together.

## Why the `with_parts_keyword` escape hatch

The example builds the handle by hand so it runs with **zero external services
beyond that one Moon**, and with no model download:

```rust
let moon = Arc::new(MoonStorage::connect(&store_url()?).await?);
let storage: Arc<dyn StoragePort> = moon.clone();
let keyword: Arc<dyn KeywordPort> = moon.clone();          // MoonStorage IS the BM25 port too
let embedder: Arc<dyn Embedder>  = Arc::new(StubEmbedder::new(768));   // 768d matches Moon's chunks FT index
let clock: Arc<HlcClock>         = HlcClock::new(0);
let lunaris = Lunaris::with_parts_keyword(storage, keyword, embedder, clock);
```

That is also why `Cargo.toml` sets `default-features = false`: no `llamacpp`
feature means no C++ toolchain, no cmake, and no 253 MB GGUF to stage.

`StubEmbedder` emits **deterministic, non-semantic** vectors — cosine scores
come out `0.0` and ranking is meaningless. That is fine here: this example
proves the *round-trip + scope isolation + durability*, so the assertions check
"≥ 1 hit returned" / "no cross-scope source leak", not "the right hit ranked
first".

For semantic recall, drop the escape hatch and use `Lunaris::open(&url)` with
default features on (the in-process llama.cpp granite-r2 embedder, staged at
`~/.lunaris/models/`). The rest of the code is unchanged — see
[`examples/quickstart-rs/`](../quickstart-rs/), which does exactly that.

## Episode IDs

`EpisodeBuilder` auto-generates a fresh ULID per episode (`into_episode` does
`self.id.unwrap_or_else(Ulid::new)`), so the example just calls
`EpisodeBuilder::new(source, content)` directly. Override the id with
`.id(...)` only when you want idempotent replay (re-ingesting the same logical
episode without creating a duplicate KV row).

## Expected output

`cargo run` exits `0` with all assertions passing. Verbatim stdout
(`RUST_LOG=error`, run against `moon --port 6380 --shards 1`):

```text
multi-agent: run id 36619
multi-agent: scope_a = acme.agent-a-36619
multi-agent: scope_b = acme.agent-b-36619

=== 1. hard isolation between two agents (distinct Scopes) ===
multi-agent: ingested agent-a episode at lsn=Lsn { wall_ms: 1778570918020, counter: 0 }
multi-agent: ingested agent-b episode at lsn=Lsn { wall_ms: 1778570918061, counter: 0 }
multi-agent: scope_a recall("owner") -> 1 hit(s), sources=["agent-a:notes"]
multi-agent: scope_b recall("owner") -> 1 hit(s), sources=["agent-b:notes"]
multi-agent: OK — neither agent can see the other's episode

=== 2. multiple sessions / tasks within agent-a (source-prefix partition) ===
multi-agent: ingested 3 session/task episodes under scope_a
multi-agent: scope_a recall("acme widget") -> 4 hit(s), sources=["agent-a:notes", "conv:mon", "conv:tue", "task:deploy"]
multi-agent: distinct source-prefix kinds seen across the recall: ["agent-a", "conv", "task"]
multi-agent: client-side narrowed to source-prefix `conv:mon` -> 1 hit(s): ["conv:mon"]
multi-agent: NOTE — there is no server-side `source`-prefix filter today; the v0 `filter_str` DSL targets Episode metadata, not the source string. Narrowing is client-side over `Hit.source` (matches the recipes' MessageStream behaviour).

=== 3. resume across a process boundary (drop handle, re-open, re-scope) ===
multi-agent: dropped the Lunaris handle (simulating process exit)
multi-agent: after re-open, scope_a recall("owner") -> 4 hit(s), sources=["agent-a:notes", "conv:mon", "conv:tue", "task:deploy"]
multi-agent: after re-open, scope_b recall("owner") -> 1 hit(s)
multi-agent: OK — agent-a memory is durable across the process boundary

multi-agent: ALL ASSERTIONS PASSED ✔
multi-agent: NOTE — the recipe wrappers (MultiTurnConversation, ChatAgentMemory, MessageStream) currently build episodes with Scope::dev() and partition only by source prefix; hard per-agent isolation today goes through the low-level lunaris.scoped(scope) handle shown above (or, in lunaris-server, the server-side `tenant` claim). See docs/book/src/guides/multi-agent.md.
```

(The `wall_ms` / run-id numbers and the line ordering inside a section change
run to run — `StubEmbedder` ties scores to `0.0` so the within-section hit
order is stable, but the leading `=== N ===` blocks and the assertion result
are the invariant.)

## The recipe-level path (not wired into this example — by design)

`lunaris_recipes::conversational::MultiTurnConversation::new(handle, "alice")`
+ `remember(turn, thread_id)` (two `thread_id`s) + `recall` spanning both is
the ergonomic conversational surface — but `lunaris-recipes` is a workspace
member (`lunaris = { workspace = true }`), so pulling it into a standalone
example crate is awkward, and it adds the consolidator stack to the build for
little gain. More importantly, `MessageStream` (which `MultiTurnConversation`
composes) builds every episode with `Scope::dev()` and partitions only by
source prefix — so it's *source-prefix + thread* isolation, **not** RLS-grade
per-agent isolation. See `docs/book/src/guides/multi-agent.md` →
"Multi-agent patterns" for the full three-level model and the `Scope::dev()`
caveat.

## See also

- [`docs/book/src/guides/multi-agent.md`](../../docs/book/src/guides/multi-agent.md)
  — the multi-agent model, the three-level table, and this example's run output.
- [`examples/quickstart-rs/`](../quickstart-rs/) — the single-episode,
  single-scope quickstart against Moon, with a real semantic embedder.
