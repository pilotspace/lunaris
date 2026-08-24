# Cookbook

**Reach for the cookbook when you don't want to hand-write episodes and
retrieval plans — pick the named recipe that matches your data shape and you
get a ≤ 30-LOC, parity-tested API instead.**

> **Want the raw query surface instead of a recipe wrapper?**
> [Querying Three Ways](./querying-three-ways.md) composes the same operators
> by hand — direct `recall`, DSL fusion, and the `Tree` operator. Same Moon,
> no wrapper.

Lunaris ships a small library of *recipe types* in the `lunaris-recipes`
crate (plus `HeliosScratchpad` in the umbrella `lunaris` crate). They are
layered:

- **4 primitives** — `MessageStream`, `DocumentCorpus`, `TemporalQuery<S>`,
  `WorkingMemory`. The building blocks. Each wraps an
  `Arc<lunaris::Lunaris>` handle and exposes 3–6 public methods.
- **5 conversational wrappers** (`lunaris_recipes::conversational`) —
  `ChatAgentMemory`, `MultiTurnConversation`, `SlackArchive`,
  `EmailThreading`, `MeetingNotesMemory`. Thin compositions over
  `MessageStream` (+ `WorkingMemory`, + an optional graph-pipeline toggle).
- **5 documentary wrappers** (`lunaris_recipes::documentary`) —
  `DocumentKnowledgeBase`, `ResearchPaperCorpus`, `CodeRepoMemory`,
  `TimelineReconstruction`, `CustomerSupportHistory`. Thin compositions over
  `DocumentCorpus` and `TemporalQuery<Documents>` (+ `MessageStream` for the
  support history).

Every wrapper forwards into **at most two** primitive method invocations per
public method and holds **zero business logic** — they exist to make a
blueprint §7 recipe discoverable from the public surface, not to add
behaviour. None of them bundle a second vector or BM25 library; the fused
recall plan they assemble (`Vector + Keyword ⊕ RRF`) lowers through the
[retrieval DSL](../guides/retrieval-dsl.md) and dispatches to Moon-native
`FT.*` inside `RetrievalBuilder::execute`.

**All ten wrappers carry live-Moon tests** under
`crates/lunaris-recipes/tests/*_parity.rs` (and the documentary trio also
under `tests/documentary_rust_integration.rs`). They were Moon-vs-Postgres
byte-identity assertions until 0.7.0 removed the second backend; what remains
is the same hit-count + hit-id-ordering contract, asserted against Moon alone.
The tests are feature-gated behind `moon-it`
and probe the backend with a 1-second TCP check, so a default
`cargo test -p lunaris-recipes` stays zero-config.

## The recipe map

| Recipe | Type | Module path | Composes | Reach for it when… |
|---|---|---|---|---|
| `MessageStream` | primitive | `lunaris_recipes::MessageStream` | `Lunaris` | you have a stream of short messages and want recency-weighted recall |
| `DocumentCorpus` | primitive | `lunaris_recipes::DocumentCorpus` | `Lunaris` | you have a document corpus and want hybrid RAG (Vector + BM25 + RRF) |
| `TemporalQuery<S>` | primitive | `lunaris_recipes::TemporalQuery` | `Lunaris` | you want time-travel — "what did the agent know at time T" |
| `WorkingMemory` | primitive | `lunaris::WorkingMemory` (re-exported by `lunaris_recipes`) | `Lunaris` (+ `Consolidator`) | you want a scope-prefixed scratchpad with optional consolidator promotion |
| `ChatAgentMemory` | conversational | `lunaris_recipes::conversational::ChatAgentMemory` | `MessageStream` + `WorkingMemory` | one chat agent, per-user `remember` / `recall` |
| `MultiTurnConversation` | conversational | `lunaris_recipes::conversational::MultiTurnConversation` | `MessageStream` + `WorkingMemory` | same as above plus a cross-session `consolidate()` pass |
| `SlackArchive` | conversational | `lunaris_recipes::conversational::SlackArchive` | `MessageStream` | a Slack workspace export, channel/user-narrowed recall |
| `EmailThreading` | conversational | `lunaris_recipes::conversational::EmailThreading` | `MessageStream` + `WorkingMemory` (+ graph toggle) | email threads, optional sender/recipient graph |
| `MeetingNotesMemory` | conversational | `lunaris_recipes::conversational::MeetingNotesMemory` | `MessageStream` + `WorkingMemory` (+ graph toggle) | meeting notes by heading, attendee-narrowed recall, optional graph |
| `DocumentKnowledgeBase` | documentary | `lunaris_recipes::documentary::DocumentKnowledgeBase` | `DocumentCorpus` | a generic doc corpus with metadata filters |
| `ResearchPaperCorpus` | documentary | `lunaris_recipes::documentary::ResearchPaperCorpus` | `DocumentCorpus` (+ graph toggle) | papers, optional citation graph |
| `CodeRepoMemory` | documentary | `lunaris_recipes::documentary::CodeRepoMemory` | `DocumentCorpus` + `TemporalQuery<Documents>` | commits / PRs / code, "function body as-of commit N" |
| `TimelineReconstruction` | documentary | `lunaris_recipes::documentary::TimelineReconstruction` | `DocumentCorpus` + `TemporalQuery<Documents>` | a dated event narrative, `between(lo, hi)` / `as_of(ts)` |
| `CustomerSupportHistory` | documentary | `lunaris_recipes::documentary::CustomerSupportHistory` | `DocumentCorpus` + `MessageStream` (+ graph toggle) | tickets *and* chat transcripts, recall across both |
| `HeliosScratchpad` | (umbrella recipe) | `lunaris::HeliosScratchpad` | `WorkingMemory` | an agent filesystem: write/read/edit/grep/ls + `as_of` time-travel |

Each remaining chapter in this section is one (or a small group of) recipes
with a runnable-shaped example derived from the parity test for that recipe.

---

## Primitives

### `MessageStream` — recency-weighted message recall

`MessageStream` (`crates/lunaris-recipes/src/message_stream.rs`) is the
substrate every conversational wrapper composes over. It binds an
`Arc<Lunaris>` to a *thread prefix* (`"messages:"`, `"slack:archive/"`, …)
and exposes:

| Method | Signature | Notes |
|---|---|---|
| `new` | `fn new(lunaris: Arc<Lunaris>, scope: Scope, thread_prefix: impl Into<String>) -> Self` | binds the prefix |
| `with_top_k` | `fn with_top_k(self, k: usize) -> Self` | builder knob; default `8` |
| `ingest` | `async fn ingest(message, thread_id, participant_id) -> Result<Lsn, LunarisError>` | one message → one `Episode` under `{prefix}{thread_id}/`; `thread_id` + `participant_id` land in `Episode.metadata` |
| `recall` | `async fn recall(query: &str) -> Result<Vec<Hit>, LunarisError>` | fuses `Vector + Keyword` via RRF(k=60), filters to the prefix, then blends an **ACT-R base-level activation** score (Anderson 1996, `d = 0.5`) with the fused RRF score so more-recent messages rank higher |

`ingest` delegates to `Lunaris::ingest` — the umbrella pipeline performs
chunking + embedding + a single `atomic_write` (the [INGEST-04
contract](../guides/ingest.md)). One call = one message = one internal
atomic write.

**Reach for it when** your data is a flowing stream of short messages —
chat turns, Slack posts, email bodies, meeting notes — and recency matters
to recall ordering.

> **Sessions persist automatically.** `MessageStream` (and the conversational
> wrappers over it) are stateless handles over durable storage — there is no
> "save"/"load session" step. To resume, reconstruct the wrapper with the same
> id (and, for `MultiTurnConversation`, the same `thread_id` on `remember`);
> the backend already holds every prior turn. See
> [Chat Agent Memory → Resuming a session](./chat-agent.md#resuming-a-session).

### `DocumentCorpus` — hybrid Vector + Keyword RAG

`DocumentCorpus` (`crates/lunaris-recipes/src/document_corpus.rs`) is the
RAG primitive. It binds an `Arc<Lunaris>` to a *source prefix*
(`"kb:papers/"`, `"repo:src/"`, …) and is a small fluent builder:

| Method | Signature | Notes |
|---|---|---|
| `new` | `fn new(lunaris: Arc<Lunaris>, scope: Scope, source_prefix: impl Into<String>) -> Self` | binds the prefix |
| `ingest` | `async fn ingest(chunks: Vec<(String, serde_json::Map<String, serde_json::Value>)>) -> Result<(), LunarisError>` | each `(content, metadata)` pair → one `Episode` under `{prefix}{ulid}` |
| `filter` | `fn filter(self, field: impl Into<String>, value: impl Into<serde_json::Value>) -> Self` | adds a `Filter::Eq` on a metadata field; multiple calls AND together |
| `top` | `fn top(self, k: usize) -> Self` | caps output; default `10` |
| `search` | `async fn search(self, query: &str) -> Result<Vec<Hit>, LunarisError>` | **consumes `self`**; fans out a `Vector + Keyword(BM25) ⊕ RRF(60)` plan with a generous over-fetch, executes, then prunes to the source prefix and caps at `k` |

The native-RRF vs client-side-fold branch lives inside
`RetrievalBuilder::execute`; the primitive is pure plan composition.

**Reach for it when** your data is a mostly-static document corpus and the
hot path is retrieval, not ingest.

### `TemporalQuery<S>` — typestate time-travel

`TemporalQuery<S>` (`crates/lunaris-recipes/src/temporal_query.rs`) is a
time-travel combinator where `S` is a compile-only phantom marker —
`Messages`, `Documents`, or `Facts`. Method availability is bound by sealed
traits (`SupportsAsOf`, `SupportsBetween`), so an invalid combination fails
at `cargo check`, not at runtime.

| Method | Signature | Notes |
|---|---|---|
| `new` | `fn new(lunaris: Arc<Lunaris>, scope: Scope, scope: Scope) -> Self` | source `S` is a phantom — no value needed |
| `as_of` | `fn as_of(self, ts: Hlc) -> Self` | snapshot at `ts` |
| `before` / `after` | `fn before(self, ts: Hlc) -> Self` / `fn after(self, ts: Hlc) -> Self` | valid-time bounds |
| `between` | `fn between(self, after: Hlc, before: Hlc) -> Self` | requires `S: SupportsBetween`; panics if `after > before`; range is **`[after, before)`** (lower inclusive, upper exclusive) |
| `execute` | `async fn execute(self, query: &str) -> Result<Vec<Hit>, LunarisError>` | dispatch handled by `RetrievalBuilder::execute` (Moon `TEMPORAL.SNAPSHOT_AT`) |

**Reach for it when** you need "what did the agent know at time T" as a
query rather than a rebuild — audit replay, post-incident debugging, pinned
regression fixtures. See [Durability & Recovery](../operations/durability.md)
for the bi-temporal MVCC model underneath.

### `WorkingMemory` — scope-prefixed scratchpad

`WorkingMemory` lives in `lunaris::primitives::working_memory` (it is
re-exported as `lunaris_recipes::WorkingMemory` so `use
lunaris_recipes::WorkingMemory;` keeps compiling). It is a key-prefixed
scratchpad: `(k, v)` pairs stored under `{scope_prefix}{k}` as `Episode`s.

| Method | Signature | Notes |
|---|---|---|
| `new` | `fn new(lunaris: Arc<Lunaris>, scope: Scope, scope_prefix: impl Into<String>) -> Self` | binds the prefix |
| `write` | `async fn write(k: &str, v: serde_json::Value) -> Result<Lsn, LunarisError>` | value is JSON, not a bare string |
| `read` | `async fn read(k: &str) -> Result<Option<serde_json::Value>, LunarisError>` | `None` = no hit |
| `grep` | `async fn grep(pattern: &str) -> Result<Vec<(String, serde_json::Value)>, LunarisError>` | all pairs whose `source` starts with `{scope_prefix}{pattern}` |
| `consolidate` | `async fn consolidate() -> Result<ConsolidationReport, LunarisError>` | drains up to 1024 recent consolidate events and runs one scope-filtered ACT-R promotion pass (no-op if no `Consolidator` is installed — default OFF) |

`consolidate()` is the engine behind `MultiTurnConversation::consolidate()`
and `HeliosScratchpad`'s per-scope promotion toggle. See
[Consolidation & Verification](../guides/consolidate-verify.md).

**Reach for it when** an agent needs a small mutable working set that lives
in the same bi-temporal store as everything else, and you want the option of
promoting hot scratchpad notes into long-term facts.
