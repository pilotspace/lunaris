# Slack / Email / Meeting Notes

**Reach for these three when your conversational data arrives in
channels, threads, or headings — `SlackArchive` for a workspace export,
`EmailThreading` for mail threads, `MeetingNotesMemory` for meeting
minutes.**

All three wrap [`MessageStream`](./index.md#messagestream--recency-weighted-message-recall)
(the email and meeting wrappers also hold a `WorkingMemory` for future
additive surface, and expose an opt-in
[graph pipeline](../guides/graph.md) toggle). Each has a hard-coded source
prefix — they are *named recipes*, not general-purpose composers. If you
need an alternate prefix, compose `MessageStream` directly.

| Wrapper | Source prefix | Composes | Public surface |
|---|---|---|---|
| `SlackArchive` | `slack:archive/` | `MessageStream` | `new` / `ingest_channel` / `recall` / `channel` / `user` |
| `EmailThreading` | `email:thread/` | `MessageStream` + `WorkingMemory` (+ graph toggle) | `new` / `ingest` / `thread` / `recall` / `with_graph_pipeline` |
| `MeetingNotesMemory` | `meeting:notes/` | `MessageStream` + `WorkingMemory` (+ graph toggle) | `new` / `note` / `recall` / `attendees` / `with_graph_pipeline` |

---

## `SlackArchive`

`crates/lunaris-recipes/src/conversational/slack_archive.rs` — a channel +
user-filtered message archive. Read-heavy, so it holds *only* a
`MessageStream` (no `WorkingMemory`).

| Method | Signature | Notes |
|---|---|---|
| `new` | `fn new(lunaris: Arc<Lunaris>) -> Self` | **no prefix arg** — rooted at `slack:archive/` |
| `ingest_channel` | `async fn ingest_channel(channel: impl Into<String>, participant_id: impl Into<String>, message: impl Into<String>) -> Result<Lsn, LunarisError>` | `channel` becomes the `MessageStream` `thread_id`; both `channel` + `participant_id` land in metadata |
| `recall` | `async fn recall(query: &str) -> Result<Vec<Hit>, LunarisError>` | recall across the whole archive |
| `channel` | `fn channel(id: impl Into<String>) -> SlackArchiveQuery` | narrow to one channel (no I/O — deferred to `SlackArchiveQuery::recall`) |
| `user` | `fn user(id: impl Into<String>) -> SlackArchiveQuery` | narrow to one user |

`SlackArchiveQuery` adds `with_user(id)` (chain a user narrow on top of a
channel narrow → `Filter::And`) and `recall(query)`. The narrowed `recall`
builds the same `Vector + Keyword ⊕ RRF(60)` plan and attaches a pre-built
`Filter::Eq` on the `channel` / `participant_id` field — no new retrieval
DSL is introduced.

> The `channel` / `participant_id` chunk-payload fields are not yet emitted
> by the ingest pipeline, so the metadata-`Eq` narrow is structurally wired
> (it passes both backend translators) but currently matches an empty set
> until that payload extension lands. The archive-wide `recall` and the
> `source`-prefix narrowing path are fully wired end-to-end. See the module
> rustdoc for the full caveat.

### Example

Shaped after `slack_archive_channel_filter_parity` in
`crates/lunaris-recipes/tests/conversational_parity.rs`:

```rust,no_run
use std::sync::Arc;
use lunaris::Lunaris;
use lunaris_recipes::conversational::SlackArchive;

#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    let lunaris = Arc::new(Lunaris::open("moon://localhost:6379").await?);
    let archive = SlackArchive::new(lunaris.clone());

    // Bulk-ingest a workspace export, message by message.
    archive.ingest_channel("general", "U_ALICE", "Standup in 5, room Helios.").await?;
    archive.ingest_channel("general", "U_BOB",   "I'll be 2 min late.").await?;
    archive.ingest_channel("incident-2025-05-12", "U_ALICE", "Rolled back deploy 0.2.3.").await?;

    // Recall across the whole archive.
    let wide = archive.recall("what happened with the deploy?").await?;
    println!("archive-wide hits: {}", wide.len());

    // Narrowed to one channel.
    let narrow = archive.channel("general").recall("standup").await?;
    println!("#general hits: {}", narrow.len());

    Ok(())
}
```

---

## `EmailThreading`

`crates/lunaris-recipes/src/conversational/email_threading.rs` — a
thread-scoped email archive with an opt-in graph builder.

| Method | Signature | Notes |
|---|---|---|
| `new` | `fn new(lunaris: Arc<Lunaris>) -> Self` | **no prefix arg** — rooted at `email:thread/` |
| `ingest` | `async fn ingest(root_id: impl Into<String>, from: impl Into<String>, body: impl Into<String>) -> Result<Lsn, LunarisError>` | one email into thread `root_id`, authored by `from` |
| `thread` | `fn thread(root_id: impl Into<String>) -> EmailThreading` | returns a **narrowed `Self`** scoped at `email:thread/<root_id>/` — the `Filter::StartsWith` on `source` does the narrowing (fully wired) |
| `recall` | `async fn recall(query: &str) -> Result<Vec<Hit>, LunarisError>` | recall across the current scope (whole archive, or one thread on a narrowed handle) |
| `with_graph_pipeline` | `fn with_graph_pipeline(self, enable: bool) -> Self` | flips `lunaris.graph_pipeline().enable()` / `disable()`; builder-style; idempotent. Graph defaults OFF (blueprint §5.2) — opt in deliberately |

### Example

Shaped after `email_threading_graph_off_parity` /
`email_threading_graph_on_opt_in`:

```rust,no_run
use std::sync::Arc;
use lunaris::Lunaris;
use lunaris_recipes::conversational::EmailThreading;

#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    let lunaris = Arc::new(Lunaris::open("postgres://lunaris@localhost/lunaris").await?);

    // Opt in to the sender/recipient graph BEFORE ingest if you want edges
    // (the extractor runs inside the ingest hot path, not after the fact).
    let email = EmailThreading::new(lunaris.clone()).with_graph_pipeline(true);

    email.ingest("RFC-0042", "alice@example.com", "Proposing the new retention sweep.").await?;
    email.ingest("RFC-0042", "bob@example.com",   "+1, but let's cap it at 90 days.").await?;
    email.ingest("RFC-0042", "alice@example.com", "Done, capped at 90d in v2.").await?;

    // Recall across all threads.
    let all = email.recall("retention sweep cap").await?;
    println!("all-threads hits: {}", all.len());

    // Narrow to one thread, then recall within it.
    let in_thread = email.thread("RFC-0042").recall("what was the cap?").await?;
    println!("RFC-0042 hits: {}", in_thread.len());

    Ok(())
}
```

---

## `MeetingNotesMemory`

`crates/lunaris-recipes/src/conversational/meeting_notes_memory.rs` — stores
meeting headings as `thread_id` and note bodies as message content; supports
attendee-narrowed recall and the same graph toggle.

| Method | Signature | Notes |
|---|---|---|
| `new` | `fn new(lunaris: Arc<Lunaris>) -> Self` | **no prefix arg** — rooted at `meeting:notes/` |
| `note` | `async fn note(heading: impl Into<String>, body: impl Into<String>) -> Result<Lsn, LunarisError>` | one note under `heading`; participant defaults to `"scribe"` |
| `recall` | `async fn recall(query: &str) -> Result<Vec<Hit>, LunarisError>` | recall across the meeting corpus |
| `attendees` | `fn attendees(attendees: Vec<String>) -> MeetingNotesQuery` | narrow to notes attributed to `attendees` — **takes an owned `Vec<String>`**, not `&[&str]` |
| `with_graph_pipeline` | `fn with_graph_pipeline(self, enable: bool) -> Self` | same semantics as `EmailThreading` |

`MeetingNotesQuery::recall(query)` emits a `Filter::And` of per-attendee
`Filter::Eq { field: "participant_id", .. }` (all attendees must be present)
— same metadata-payload caveat as `SlackArchive`'s channel narrow.

### Example

Shaped after `meeting_notes_memory_transcript_parity`:

```rust,no_run
use std::sync::Arc;
use lunaris::Lunaris;
use lunaris_recipes::conversational::MeetingNotesMemory;

#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    let lunaris = Arc::new(Lunaris::open("moon://localhost:6379").await?);
    let notes = MeetingNotesMemory::new(lunaris.clone());

    notes.note("2025-05-12 / Roadmap", "Decided to ship the 90-day retention cap in v2.").await?;
    notes.note("2025-05-12 / Roadmap", "Action item: Alice to write the migration doc.").await?;
    notes.note("2025-05-13 / Standup", "Migration doc drafted, in review.").await?;

    // Recall across the whole corpus.
    let hits = notes.recall("what was decided about retention?").await?;
    println!("corpus hits: {}", hits.len());

    // Narrow to notes attributed to a set of attendees (owned Vec<String>).
    let by_alice = notes.attendees(vec!["scribe".to_string()]).recall("action items").await?;
    println!("attendee-narrowed hits: {}", by_alice.len());

    Ok(())
}
```

## Notes

- **Hard-coded prefixes.** `slack:archive/`, `email:thread/`,
  `meeting:notes/` — if you need different roots, compose `MessageStream`
  yourself; these wrappers won't take a prefix argument.
- **Graph is opt-in and ingest-time.** `with_graph_pipeline(true)` must be
  called *before* ingest if you want entity/relation edges; retrofitting
  graph on an already-ingested corpus requires re-ingest. See
  [The Graph Pipeline](../guides/graph.md).
- **Tenant isolation** is orthogonal to these channel prefixes — see
  [Multi-Agent & Scope](../guides/multi-agent.md).
