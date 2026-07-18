---
name: dream
description: >-
  Drive a Lunaris memory-consolidation pass: list clusters of ripe, raw
  episodes with memory.dream_agenda, author distilled prose for the
  clusters worth keeping, write it durably with memory.distill, and
  memory.resolve any memory judged stale or superseded along the way. The
  harness (you) is the distiller/judge — Lunaris only surfaces candidates
  and stores what you write. Use this skill when the user says "dream",
  "/dream", "consolidate memory", "run distillation", or when a Lunaris
  SessionStart digest nudges "N memories are ripe for distillation — run
  /dream to consolidate."
user-invocable: true
when_to_use: "Invoke when the user runs /dream, asks to consolidate/distill memory, or a Lunaris SessionStart context block contains a '⏳ ripe for distillation' nudge line."
category: workflows
keywords: [dream, distill, consolidate, memory, lunaris, engram]
argument-hint: "[optional: a scope hint or a focus area, e.g. 'focus on auth-related memories'] (no argument = consolidate the whole current-project scope)"
license: MIT
metadata:
  author: lunaris
  version: "1.0.0"
---

# /dream — memory consolidation pass

`/dream` turns a pile of raw, individually-referenced episodes into a small
number of durable, high-signal knowledge records. Lunaris never runs an
internal distillation LLM for this — **you are the distiller and the
judge**. The three MCP tools below are already registered on the active
Lunaris MCP server (`memory.dream_agenda`, `memory.distill`,
`memory.resolve` — confirmed in the tool roster).

## The loop

### 1. List the agenda — `memory.dream_agenda`

Call it with no arguments to get sane defaults (`limit=20`,
`min_cluster_size=1`):

```
memory.dream_agenda()
```

It returns **read-only** candidate clusters — it writes nothing. Each
cluster (`DreamClusterDto`) carries:

- `cluster_id` — `"com:<hex>"` (a Leiden entity community) or
  `"src:<class>"` (a source-class fallback bucket)
- `size` / `member_episode_ids` (sorted ULIDs)
- `mean_activation` / `max_activation` — how heavily-referenced the
  cluster is
- `dominant_source` — the most common source class among members
- `snippets` — up to 3 member content previews (<=280 chars each) to help
  you judge the cluster without hydrating every member

Narrow with `limit`, `min_cluster_size` (skip small/noisy clusters), or
`max_activation` (only consider candidates decayed enough to be "ripe")
when the default agenda is too broad.

### 2. Read the members, judge, and author distilled prose

For each cluster worth consolidating (skip clusters that are noise, a
single one-off, or too heterogeneous to summarize honestly):

- Read the cluster's `snippets` (and hydrate individual `member_episode_ids`
  via `memory.recall` / your normal recall tools if you need more than the
  preview) to understand what actually happened.
- Choose exactly one `kind`:
  - `decision` — a durable choice that was made and why
  - `lesson` — something learned the hard way (a mistake, a gotcha found
    in production, a surprising root cause)
  - `invariant` — a rule that must hold going forward
  - `gotcha` — a sharp edge / trap future-you or a teammate will hit again
- Write `content` as **plain prose** — never a JSON envelope. Lunaris's
  digest anti-injection filter drops JSON-shaped content, so a distilled
  record must read like a sentence or short paragraph, not a data blob.
  Be concrete: name the what, the why, and (for lesson/gotcha) how to avoid
  it next time.

### 3. Write it durably — `memory.distill`

```
memory.distill(
  kind: "decision" | "lesson" | "invariant" | "gotcha",
  content: "<your authored prose>",
  source_episode_ids: [<the cluster's member_episode_ids you actually used>],
)
```

This is the **transactional apply step**:

- It writes ONE new episode, `source = "distilled:{kind}:<scope>"`, at
  `source_priority = 95` — above `decision:` (90) — so it outranks the raw
  episodes it replaces in a future SessionStart digest.
- It **archives** every id in `source_episode_ids` (activation drop via the
  ledger's `archived_at` marker). Archive is NOT a delete — the source
  episodes stay fully recall-hydratable, they just lose their recall boost
  and drop out of the next `memory.dream_agenda` candidate set. This is
  what actually shrinks the agenda for the next `/dream` pass.
- `source_episode_ids` must be non-empty ULID strings you actually
  distilled from — provenance is required, not decorative.
- Pass `dedupe_key` if you might re-run the same distillation (e.g. a retry
  after a partial failure): a replay returns the prior
  `distilled_episode_id` with `was_duplicate=true` and
  `archived_count=0` — it will never double-archive.
- Optional `title` / `tags` are accepted for forward compatibility but not
  yet persisted in v1 (informational only — don't rely on them for
  anything durable).

### 4. Resolve anything you judged stale or superseded — `memory.resolve`

While you're in a cluster's episodes, you may find one that is flat-out
wrong now (superseded by a later decision, or the underlying claim is no
longer true). That is a **different** action from distillation — use
`memory.resolve` against the relevant `memory.verify_agenda` entry (or any
episode id you are confident is a verify-agenda candidate):

```
memory.resolve(episode_id: "<ulid>", action: "keep" | "invalidate" | "supersede", reason: "<why>", superseded_by: "<ulid>" /* required for supersede */)
```

- `keep` — prune the agenda row, leave the episode live.
- `invalidate` — soft-tombstone the episode (MVCC; it stops appearing in
  `memory.recall`). Irreversible.
- `supersede` — same as invalidate, but requires `superseded_by` (the
  replacement episode's ULID), echoed back in the response.

Do not use `memory.distill`'s archive as a substitute for `memory.resolve`
— archive only lowers a memory's recall boost, it never tombstones it.
Only `memory.resolve(action: invalidate|supersede)` actually removes an
episode from recall.

## What NOT to do

- Do not invent `source_episode_ids` — only cite ids you actually read and
  used.
- Do not write `content` as JSON, YAML, or any structured envelope — plain
  prose only.
- Do not call `memory.distill` for a cluster you didn't actually read; a
  low-effort restatement of the snippets is worse than leaving the cluster
  alone for a future pass.
- Do not treat a `/dream` nudge as urgent — it is advisory. If nothing in
  the agenda is worth consolidating this session, it is fine to do nothing.

## v2 (not yet wired) — doc-stubs only

These triggers are **documented intent, not implemented automation**. Do
not assume either fires today; both are inert without the env var AND the
scheduling/piggyback code that would read it (neither exists yet):

- `LUNARIS_DREAM_CRON` — a future autonomous, unattended `/dream` pass run
  on a schedule (e.g. nightly) rather than only on explicit user invocation.
- `LUNARIS_DREAM_PIGGYBACK` — a future session-end trigger that offers (or
  auto-runs) a `/dream` pass as part of session teardown, instead of only
  the SessionStart nudge that points back at this skill.

Until these land, the only way `/dream` runs is a human or harness
explicitly invoking it (or reacting to the SessionStart nudge line).
