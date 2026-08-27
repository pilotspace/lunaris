# Skill + CLI memory surface — technical plan

**Status:** proposed, not started · **Decided:** 2026-08-27 (Tin, 2 interview rounds)
**Supersedes:** the assumption that MCP is the agent's memory write surface.

---

## 1. The finding this exists to fix

Measured on the live store (`moon://127.0.0.1:6381`), read-only, 2026-08-27:

| observation | value |
|---|---|
| total keys | 1,653,158 |
| episodes | 303,874 |
| curated episodes (n=3000 sample) | **5 — all test fixtures** (`edit:test-record-edit`, `test/round-trip`) |
| real curated memories, all time | **0** |
| distinct scopes | **542** vs Moon's `max_scopes_recommended: 512` |
| activation records | 17,903 (60.9% `strong`, 39.1% `weak`) |

Episode source composition (n=3000):

```
58.43%  lunaris:tool_call:post
29.40%  lunaris:tool_call:pre
 9.00%  lunaris:memory_injection
 2.10%  lunaris:pre_tool_use
 0.90%  lunaris:turn_feedback
 0.167% curated  <- all test fixtures
```

**The comparison that sets the target.** Claude Code's plain-markdown
`MEMORY.md` holds **137 curated entries** (66 project / 45 feedback / 18
reference) about this same project. The purpose-built engine holds zero. The
difference is not the store — it is that MEMORY.md has a *skill* telling the
agent when to write. That is the active ingredient this plan ships.

---

## 2. What already exists (verified, do not rebuild)

```
                    ┌───────────────────────────────┐
   MCP client ─────▶│ lunaris-mcp (thin proxy)      │──┐
                    │ src/proxy.rs, socket-first,   │  │
                    │ circuit breaker after N       │  │
                    └───────────────────────────────┘  │
                                                       ▼
                    ┌───────────────────────────────┐ unix socket
   shell / CI ─────▶│ lunaris-cli (thin router)     │ ~/.lunaris/
                    │ src/route.rs, socket-first,   │ codex-contextd.sock
                    │ no breaker (one-shot)         │  │
                    └───────────────────────────────┘  │
                                                       ▼
                    ┌────────────────────────────────────────────────┐
                    │ lunaris-contextd  (THE warm daemon)            │
                    │ ContextRequest::Memory(MemoryRequest)          │
                    │   -> ContextService::handle_memory             │
                    │ resident GGUF + per-scope handle cache         │
                    └────────────────────────────────────────────────┘
                                        │
                    lunaris_memory_service::protocol::dispatch  (21 ops)
```

Both transports already reach the **identical** `dispatch`. This plan is a
transport swap plus a judgment layer, **not** an architecture change.

### Op coverage today — the full surface sweep

Counted across **every** surface, not just the shared dispatch. 12 binaries
exist in the workspace; 5 are memory surfaces (`lunaris`, `lunaris-contextd`,
`lunaris-hook`, `lunaris-mcp`, `lunaris-server`), the rest are codegen / bench
/ eval tooling.

| surface | ops | reaches the shared `dispatch`? |
|---|---|---|
| `MemoryRequest` (`lunaris-memory-service/src/protocol.rs`) | **21** | it IS the dispatch |
| `lunaris-mcp` `#[tool]` roster | **20** | 19 shared + 1 MCP-only |
| `lunaris` CLI | **5** | yes |
| `ContextRequest` (`lunaris-hook/src/context.rs`) | **8** | 1 (`Memory` umbrella) + **7 hook-only** |
| `lunaris-server` HTTP | **9** under `/v1` + 4 infra | partly |
| SDK `ScopedLunaris` (Py + TS) | **8** methods | in-process |

**Distinct memory operations across all surfaces: 34**
= 21 shared + 1 MCP-only + 7 hook-only + 5 inspector-only.
**CLI coverage is 5 of 34. CLI write ops: 0.**

`MemoryRequest` (21): `Ingest · Recall · Forget · Remember · Profile ·
Retention · RetentionEnforce · RecordDecision · RecordEdit · Feedback ·
Status · RepairVectors · ScratchpadWrite · ScratchpadRead · ScratchpadGrep ·
ScratchpadConsolidate · ScratchpadHandover · VerifyAgenda · Resolve ·
DreamAgenda · Distill`

#### Three asymmetries that change the work

1. **`memory.list_scopes` is MCP-ONLY.** `grep -c 'ListScopes'
   crates/lunaris-memory-service/src/protocol.rs` = **0**. It is implemented in
   `crates/lunaris-mcp/src/tools/list_scopes.rs` and, separately, in
   `crates/lunaris-server/src/routes/browse.rs` — never on the shared dispatch.
   **Deleting MCP deletes the capability** unless it is ported first. This is a
   Phase 6 precondition, not a detail.
2. **`RepairVectors` and `ScratchpadHandover` are on the protocol but NOT on
   MCP** (`grep -c` = 0 for both in `crates/lunaris-mcp/src/main.rs`). The CLI
   already exposes `repair-vectors`, which MCP never had. Retirement loses
   nothing here.
3. **The HTTP server carries a read surface with no `MemoryRequest`
   equivalent** — `/v1/{graph, episode/{id}, snapshot/{lsn}, browse/{kind},
   detail/{kind}/{id}}`, the Memory Inspector. Five ops. **Untouched by this
   plan and unaffected by MCP retirement**, recorded here so its absence from
   the phases is deliberate rather than an oversight.

#### The 7 hook-only ops stay hook-only

`RecallForPrompt · RecallAfterTool · CaptureToolCall · CaptureToolResult ·
TurnFeedback · SessionDigest · Health` are the capture/injection path. This
plan does **not** move them — with one exception: Phase 2 needs `SessionDigest`
on the CLI, so that one op moves to `MemoryRequest` (see Phase 2).

### Both memory tiers exist; nothing bridges them

| tier | mechanism | CLI |
|---|---|---|
| short-term | `lunaris::primitives::working_memory::WorkingMemory`, session-namespaced, `scratchpad_{write,read,grep,handover}` | none |
| long-term | episodes + typed records (`remember`, `record_decision`, `record_edit`) | none |
| promotion | `memory.distill` — writes prose durably, archives sources, activation drop, provenance kept | none |

Three traps found while verifying:

1. `scratchpad_consolidate` is an **ACT-R MQ drain**, not a promotion path.
   The bridge is `distill`.
2. `distill` is driven by the **`/dream` skill, which is MCP-based**
   (`.claude/skills/dream/SKILL.md`). **Retiring MCP breaks the only curation
   skill that exists** — porting `/dream` is required, not optional.
3. Short-term has **no TTL**: `grep -ciE 'ttl|expire|evict'
   crates/lunaris/src/primitives/working_memory.rs` = 0. Short-term in name only.

---

## 3. Decisions (settled — do not re-litigate)

1. **All four drivers apply**: context cost · judgment · portability ·
   legibility. Each gets its own acceptance proof (§7).
2. **Acceptance = dogfood.** "My own store stops being noise", graded by a
   committed census over >=10 real sessions. Not a stranger install, not Helios.
3. **Retire MCP — only AFTER the census proves the replacement.** Removing the
   only working write surface first would leave no memory path at all.
4. **Three scope tiers**, routing **by kind with an override**.

---

## 4. Scope model

Today: `lunaris_hook::scope::resolve(cwd)` = `blake3(cwd + git remote.origin.url
+ branch)` -> `git_<hex16>` (`crates/lunaris-hook/src/scope.rs:118`).
Per-worktree AND per-branch, so every branch mints a permanent scope. 495 of
them exist. Nothing removes one.

Target:

| tier | key | derivation | holds |
|---|---|---|---|
| project | `proj_<hex16>` | `blake3(git remote.origin.url)` — no cwd, no branch | `decision` · `constraint` · `fix` |
| user | `user_<hex16>` | `blake3(stable user id)` | `preference` |
| session | unchanged | existing session-namespaced scratchpad | short-term working notes |
| substrate | `git_<hex16>` unchanged | existing hook telemetry scope | raw tool calls |

All satisfy the `Scope` alphabet `[A-Za-z0-9_\-.]{1,128}`.

**Recall reads the union** of project + user + current substrate scope.
**Writes go to exactly one tier**, chosen by kind, overridable by `--tier`.

> **Why this is blocking.** Writing curated memories into the per-branch hook
> scope would fragment them across ~500 scopes and *look like it worked*.
> Settle scope before the skill writes anything.

**The 542 existing scopes are left in place** (non-destructive, matches the
curation-gap "keep, stop injecting" decision). This plan stops *new*
proliferation for durable writes; it does not prune.

---

## 5. Phases

### Phase 0 — build the ruler before the thing it measures · 1.5d

No production code. Three instruments plus one unblock.

| deliverable | output |
|---|---|
| `scripts/memory-census.py <store-url>` | M1: episode count by source, curated share, scope census |
| `scripts/transcript-metrics.py <dir>` | M2 + M4 over 5,374 transcripts / 167 projects / 2.2 GB |
| `scripts/personal-eval/` | 45-case replay set built from the `feedback_*` memories |
| ship-plan fix | F22 checkbox is stale — fixed on main by `6b90629`/`fc81fd8`/`abf4345` |
| **contextd redeploy** | the running daemon (2026-08-25) predates W4.4 (2026-08-26); `strings` finds 0 hits for `LUNARIS_CONTEXT_INCLUDE_TOOLCALLS` — **the telemetry demotion you merged is not live** |

**Gate:** census reproduces §1's numbers; post-redeploy `strings` finds the
filter.

### Phase 1 — three-tier scope · 1d · BLOCKING

- `lunaris-core::scope_resolver`: add `project_scope(remote) -> Scope` and
  `user_scope() -> Scope`.
- `lunaris-hook::scope`: keep `resolve()` unchanged (substrate); add the two
  durable resolvers beside it.
- Recall composes the union.

**Gate G0 (discriminating):** the same memory written from two different
worktrees on two different branches lands in **one** scope and is recalled from
both. *Mutation:* revert to the branch-derived scope -> must red.

### Phase 2 — CLI write surface · 2.5d

New `Command` variants in `crates/lunaris-cli/src/request.rs`, mapped through
`to_request()` to `MemoryRequest` variants — all existing except `SessionDigest`:

| CLI | MemoryRequest | tier |
|---|---|---|
| `lunaris remember --kind <decision\|fix\|preference\|constraint> <text> [--why] [--tier project\|user]` | `Remember` | by kind, `--tier` overrides |
| `lunaris resolve <id> --reason <r>` | `Resolve` | — |
| `lunaris digest [--max-hits N]` | `SessionDigest` **(new variant — see below)** | union |
| `lunaris note <text>` / `note --read` / `note --grep <p>` | `ScratchpadWrite` / `Read` / `Grep` | session |
| `lunaris distill --cluster <id> <prose>` | `Distill` | project |
| `lunaris dream-agenda` | `DreamAgenda` | union |

**TWO protocol moves are unavoidable**, each across all **four seams**
(enum variant, `scope()` accessor arm, `op()` label, `dispatch` arm).

**Move 1 — `SessionDigest`.** `SessionDigest` is a **`ContextRequest`**
variant, NOT a `MemoryRequest` (`grep -c 'SessionDigest'
crates/lunaris-memory-service/src/protocol.rs` = 0), and
`route::Router::dispatch` accepts `MemoryRequest` only. Two options:

- **(a) chosen** — add `SessionDigest` to `MemoryRequest` (all **four seams**:
  enum variant, `scope()` accessor arm, `op()` label, `dispatch` arm) and have
  `ContextRequest::SessionDigest` delegate to it. Works on BOTH the socket and
  the direct-open path.
- (b) rejected — teach the CLI router to send `ContextRequest` too. Breaks the
  direct fallback, because `ContextService` lives only in contextd. A CLI that
  works only when the daemon is up fails G3.

**Move 2 — `list_scopes`.** MCP-only today
(`crates/lunaris-mcp/src/tools/list_scopes.rs`; `grep -c 'ListScopes'
protocol.rs` = 0). It must land on `MemoryRequest` so the capability outlives
MCP — this is what makes Phase 6 safe, and it is why **G9** exists.

Every other CLI verb maps to an existing variant with no protocol change.

Also: `--scope auto` resolving the correct tier; every command honours the
existing global `--json`, which already emits `{"via": …, "data": …}`.

**Gate G1 (discriminating):** run the **real `lunaris` binary** against a
**real contextd** over the socket, write a memory, assert `"via":"contextd"`,
then read it back **through a different surface** (`dispatch` directly).
*Mutation:* point the CLI at a second Moon -> read-back must fail.

### Phase 3 — the skills · 1.5d

- `.claude/skills/remember/SKILL.md` — NEW. Carries the judgment: the four
  kinds; **what not to write** (anything git/code/CLAUDE.md already records);
  recall-before-write so it updates instead of duplicating; when to `resolve`
  rather than append; triggers (decision with rationale, non-obvious fix, user
  correction, session end).
- `.claude/skills/dream/SKILL.md` — PORT from MCP tool calls to CLI calls.
  This is the portability driver's acceptance test: the same skill text must
  run anywhere with a shell.

**Gate:** a skill cannot be unit-tested. Graded by Phase 5's census. Stated
plainly rather than faked with a green check.

### Phase 4 — read loop + honest token measurement · 1d

- `SessionDigest` defaults to `source_prefixes = ["decision:"]`; extend to all
  four kinds across both durable tiers.
- **Measure the context-cost driver**: MCP's 20 always-on tool schemas vs a
  skill loaded on demand, in tokens, both directions.

**Gate:** seed one memory of each kind in each tier; the digest surfaces all
four. *Mutation:* drop a prefix from the default list -> must red.

### Phase 5 — dogfood · >=10 real sessions

Run it. Census after. **This is the gate that matters** (G6).

### Phase 6 — retire MCP · 0.5d · gated on Phase 5 AND G9

**Preconditions, both hard:**

1. **G6 green** — the census proves the replacement over >=10 real sessions.
2. **G9 green** — every MCP tool name has a reachable non-MCP equivalent.
   Today exactly one fails that check: **`memory.list_scopes`**, which exists
   only on MCP and on the HTTP `/v1/scopes` route. Phase 2's Move 2 ports it.

**Deletion set:** `crates/lunaris-mcp`, `crates/lunaris-mcp-npm`,
`crates/lunaris-mcp-py`. Deleted crates get tombstones, never yanks.

**Doc/dist surfaces to update:** `README.md`, `docs/book/src/mcp/`,
`docs/integration/claude-code.md`, `docs/integration/codex.md`, the npx/uvx
distribution, `.mcp.json`, and the 20-tool roster guard in
`crates/lunaris-mcp/tests/server_boot.rs` (which is deleted with the crate —
G9 becomes its successor).

**Explicitly NOT affected:** the `lunaris-server` HTTP surface, including the
Memory Inspector routes (`/v1/{graph, episode/{id}, snapshot/{lsn},
browse/{kind}, detail/{kind}/{id}}`). They are a peer surface, not an MCP
consumer, and they survive retirement untouched.


---

## 6. Metrics

| id | metric | baseline (measured 2026-08-27) | target |
|---|---|---|---|
| **M1** | curated share of new episodes | **0 real / 303,874** | >=1 per session; **>=40 in month one** (MEMORY.md's demonstrated rate) |
| **M2** | repeat-correction rate | to be extracted from 5,374 transcripts | set after baseline; direction **down** |
| **M3** | citation rate | 60.9% strong / 39.1% weak (n=4000 of 17,903) | **no target yet — see caveat** |
| **M4** | context cost | 639 chars (~160 tok) mean per injection; budgets healthy, 0 violations | net tokens/session down, measured both sides |
| **M5** | re-discovery rate | unmeasured | stretch, not a gate |

> **M3 caveat — do not report this number yet.** Injected memories are ~100%
> raw telemetry, so an n-gram match between an injected `cargo test …` and the
> agent running `cargo test …` is **echo, not use**. M3 becomes meaningful only
> once M1 > 0 and curated memories dominate injection. Publishing 60.9% as
> "our memories get used" would be exactly the unlabelled-number defect the
> operating-points decision exists to kill.

### The falsifiable eval

45 `feedback_*` memories, each with a known "learned on date X" moment. Seed
them, replay the transcript context from just **before** each was learned, and
ask whether recall surfaces the right lesson **before** the mistake. 45
labelled cases, offline, repeatable, built from data that already exists.
LongMemEval where the questions are your own failures.

---

## 7. Gates

| id | gate | proves | blocking |
|---|---|---|---|
| G0 | durable memories are branch-independent | scope model | yes |
| G1 | a CLI write is readable by every other surface | shared dispatch | yes |
| G2 | zero raw telemetry reaches an agent's context | W4.4 ratchet (exists, absolute, with toggle arm) | yes |
| G3 | contextd down -> CLI falls back **and says so** (`"via":"direct"`) | legibility driver | yes |
| G4 | a killed write leaves no partial episode | `atomic_write` | yes |
| G5 | growth bounded or documented | retention has a caller | no |
| G6 | **the agent actually writes** (M1 over >=10 sessions) | the whole thesis | **yes** |
| G7 | net token reduction | context-cost driver | yes |
| G8 | `/dream` and `/remember` run with no MCP present | portability driver | yes |
| G9 | **every MCP tool name has a reachable non-MCP equivalent** | nothing is lost to retirement | yes, gates Phase 6 |

---

## 8. Risks

- **Depends on the agent remembering to call it.** Logged when the approach
  was chosen (2026-08-20). A skill narrows it; it does not close it. G6 is the
  only honest detector.
- **A wrong tier hides a memory silently** — it reads exactly like the memory
  was never written. Mitigated by kind-routing defaults over free choice.
- **542 > 512 scopes.** This plan stops new proliferation for durable writes;
  it does not reduce the existing count. Recall p99 degradation above Moon's
  soft limit is documented, **not measured here**.
- **A partial surface inventory reads exactly like a complete one.** The first
  draft of this plan counted `MemoryRequest`'s 21 ops and treated them as the
  universe; the real figure is **34** across six surfaces, and the gap hid
  `list_scopes` — a capability MCP retirement would have silently deleted.
  **G9 is the automated successor to that catch**: it keys on the decision
  ("nothing is lost"), not on a human re-reading the roster.

- **MCP retirement blast radius** — README, book, two integration guides, the
  npx/uvx distribution, and the roster guard. Gated behind G6 for exactly
  this reason.

---

## 9. Budget

~7 working days of build + ~1 week elapsed dogfood before G6 opens Phase 6.
