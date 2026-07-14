# TASK: SessionStart injects a curated Lunaris digest of the scope's recent durable decisions (replaces MEMORY.md auto-load)

slug: session-start-digest · created: 2026-07-14 · stage: production
autonomy: auto   <!-- inherited from the project default (PROJECT.md); explicit level: manual < conservative < auto (visible · overridable) — lower below if a high-risk task needs it, or run `add.py autonomy set`. -->
phase: done   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower the
     autonomy level to `manual` or `conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

Touches (files · symbols · signatures):
- ENGINE (new): `crates/lunaris/src/digest.rs` — `pub async fn recent_by_source(storage: &dyn
  StoragePort, scope: &Scope, prefixes: &[String], limit: usize) -> Result<Vec<Episode>,
  LunarisError>`. Mirrors `forget.rs:scan_matches_scoped` (668): scoped prefix
  `format!("{}episode:", keyspace::scope_prefix(scope))` + `storage.scan_range(scope, &prefix,
  None)`. Reads each value as `Episode` (JSON: id·scope·source·content·t_ref·bt·metadata —
  `crates/lunaris-core/src/primitives.rs:22`); keeps rows whose `source` starts_with any prefix;
  sorts by `Episode.id` (ULID, time-sortable) DESC; takes `limit`. NO read_as_of (value is the
  episode; recency from the ULID). Re-export in `lunaris/src/lib.rs`.
- CONTEXTD: `crates/lunaris-hook/src/context.rs` — new `ContextRequest::SessionDigest { cwd,
  scope, session_id, max_hits, max_chars, source_prefixes }` (enum line 53). Handler resolves
  scope via `resolve_scope` (daemon path → `resolve_no_env`), gets `storage_for_scope(scope)`
  (306), calls `lunaris::recent_by_source`, maps each `Episode` → `ContextMemory { episode_id,
  source, score: 1.0, snippet }` where snippet = `snippet::summarize(&source, &content)` (curated
  `decision: …; rationale: …`, `lunaris_core::snippet`), then `finish_recall(scope,
  "session_start", session_id, max_chars, None, memories)` (renders + traces). Failure → empty
  (a digest failure must NEVER block session start).
- ADAPTER: `scripts/lunaris-codex-hook-adapter.py` — new `run_session_digest(event, target)`
  sending `{type:"session_digest", ...}` then `emit_context_response(resp, target, "SessionStart")`.
  Dispatch it for SessionStart events in inject mode.
- INSTALLER: `scripts/setup-lunaris-agents.py` — add a SessionStart inject-leg hook entry
  (currently SessionStart is capture-only).
Context (working folder): `record_decision` writes source `decision:<scope>`
  (`crates/lunaris-mcp/src/tools/record_decision.rs:85`) — the digest's default filter is
  `["decision:"]`. `scripts/tests/test_capture_signal_gate.py` / `test_hooks_merge_safe.py` are
  the Python test precedent; `crates/lunaris/tests/consolidate_archive_parity.rs:NullStorage` is
  the minimal StoragePort mock to base the engine test on.
Honors (patterns / conventions): keyspace helpers from `lunaris_core::keyspace` (never local);
  Moon scan_range ignores its scope arg and matches the prefix LITERALLY (`storage-moon/src/kv.rs:116`)
  — so the scoped prefix is mandatory for backend correctness; design-for-failure (digest failure
  → empty, never an error to the agent); curation via shared `lunaris_core::snippet`.
Anchors the contract cites: `recent_by_source`, `ContextRequest::SessionDigest`,
  `finish_recall`, `snippet::summarize`, `keyspace::scope_prefix`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: SessionStart digest — a recency-ordered, source-filtered recall of the scope's durable
decisions, curated and injected as additionalContext at session start, replacing MEMORY.md's
auto-load role. Backed by a new engine primitive `recent_by_source` (scan + source-filter + ULID
recency), a new contextd `SessionDigest` request, an adapter session-start leg, and installer wiring.
Framings weighed: recency scan by source-prefix (chosen — honest "recent decisions", no query
bias) · semantic recall of a fixed digest query (rejected — biases to query similarity, not
recency; would misrepresent "recent decisions") · client-side redis SCAN in the adapter (rejected —
duplicates scope routing + keyspace, breaks the lunaris-core keyspace-ownership rule).
Must:
<must>
  - `recent_by_source(storage, scope, prefixes, limit)` returns the `limit` most-recent episodes
    (by `Episode.id` ULID, DESC) whose `source` starts_with ANY of `prefixes`, scanning ONLY the
    scope's `episode:` partition (scoped prefix via `keyspace::scope_prefix`).
  - Episodes whose source matches NO prefix are excluded; a non-Episode / unparseable value row
    is skipped (never aborts the scan).
  - `limit == 0` or an empty scan returns an empty Vec (no error).
  - `ContextRequest::SessionDigest` renders the matched episodes via `snippet::summarize` +
    `finish_recall` under phase "session_start"; an empty match returns `ContextResponse::empty()`.
  - Design-for-failure: any storage/scan error in the digest handler degrades to
    `ContextResponse::empty()` — a digest failure MUST NOT block or error session start.
  - The adapter session-start leg emits the rendered digest as SessionStart additionalContext and
    exits 0 even when the digest is empty or contextd is down.
Reject:
<reject>
  - source matches no requested prefix -> excluded from the digest (not an error)
  - storage/scan failure in the handler -> ContextResponse::empty() (never surfaced to the agent)
</reject>
After:
<after>
  - A scope with N `decision:` episodes yields a digest of the min(N, limit) most-recent, curated.
  - A never-ingested / empty scope yields an empty digest and a clean exit 0 (no "unknown index" leak).
  - Recency holds: given decisions d1<d2<d3 by ULID, limit=2 returns [d3, d2].
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ `storage.scan_range` on a fresh/never-ingested scope returns an empty stream rather than
    erroring — lowest confidence because Moon SCAN over zero keys is empty (fine) but a fresh
    SQLite store MIGHT error if the KV table is absent. If wrong: the handler's fail-to-empty
    wrapper still yields a clean empty digest (cost: none observable — the design-for-failure
    wrapper is exactly this guard). Confirm via the engine test + a live empty-scope run.
  - [x] Episodes are keyed by scoped `episode:{ULID}` and the value is the full Episode JSON with
    `source`+`content` — confirmed: primitives.rs:22 + scan_matches_scoped reads the same rows.
  - [x] ULID Ord is time-then-random, so id DESC == recency DESC — confirmed: ulid crate derives Ord.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: recency-ordered decisions
  Given a scope with decisions d1<d2<d3 (by ULID) and one non-decision episode
  When recent_by_source(storage, scope, ["decision:"], 2) runs
  Then it returns exactly [d3, d2]
  And the non-decision episode is not present

Scenario: source filter excludes non-matches
  Given a scope with a "decision:x" and an "edit:y" episode
  When recent_by_source(storage, scope, ["decision:"], 10) runs
  Then only the decision episode is returned

Scenario: unparseable row is skipped
  Given the episode partition contains one garbage (non-JSON) value among valid decisions
  When recent_by_source runs
  Then the garbage row is skipped and the valid decisions are still returned

Scenario: empty scope yields empty digest
  Given a scope with no episodes
  When recent_by_source(storage, scope, ["decision:"], 5) runs
  Then it returns an empty Vec (no error)

Scenario: digest handler renders curated memories
  Given a scope with two decision episodes
  When ContextRequest::SessionDigest is handled
  Then the response carries two ContextMemory entries whose snippet begins "decision:"
  And rendered_context is non-empty

Scenario: digest handler fails to empty
  Given a storage whose scan_range errors
  When ContextRequest::SessionDigest is handled
  Then the response is ContextResponse::empty() (ok=true, no error surfaced)
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
ENGINE  lunaris::recent_by_source(
          storage: &dyn StoragePort, scope: &Scope,
          prefixes: &[String], limit: usize
        ) -> Result<Vec<Episode>, LunarisError>
  scans keyspace::scope_prefix(scope)+"episode:"; keeps source.starts_with(any prefix);
  sorts Episode.id DESC; takes limit. Unparseable rows skipped. Empty scan -> Ok(vec![]).

CONTEXTD  ContextRequest::SessionDigest {
            cwd: Option<PathBuf>, scope: Option<String>, session_id: Option<String>,
            max_hits: Option<usize>, max_chars: Option<usize>,
            source_prefixes: Option<Vec<String>>   // default ["decision:"]
          }
  -> ContextResponse { ok, injection_id?, memories[], rendered_context, ... }
     memories[i] = { episode_id, source, score: 1.0, snippet: summarize(source, content) }
     phase = "session_start"; empty match OR any error -> ContextResponse::empty()

ADAPTER  request {type:"session_digest", cwd, scope, session_id, max_hits, max_chars}
         -> emit_context_response(resp, target, "SessionStart"); always exit 0

Schema: no new storage; read-only scan of the existing scoped episode: partition.
```

Status: FROZEN @ v1 — approved by Tin Dang (standing fully-auto delegation; read-only scan +
new additive request variant + additive hook leg; design-for-failure fail-to-empty; no schema
change; reuses proven scope-routing/curation/render).

Least-sure flag surfaced at freeze: [spec] a fresh SQLite scope MIGHT error on scan_range if the
KV table is absent (vs Moon's empty SCAN). Why it might be wrong: SQLite table bootstrap timing.
Cost if wrong: none observable — the handler's fail-to-empty wrapper converts any scan error to an
empty digest, and the engine test covers the erroring-storage path. Surfaced; guarded by design.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: engine primitive 100% branches (match/exclude/skip-garbage/empty/limit/recency)
+ contextd handler (render + fail-to-empty) + adapter session-start leg.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  ENGINE (crates/lunaris/tests/digest_recent_by_source.rs, in-memory ScanStubStorage):
  - recency_order: seed d1<d2<d3 + non-decision; recent_by_source(["decision:"],2) == [d3,d2]
  - source_filter_excludes: decision + edit; ["decision:"] returns only the decision
  - skips_unparseable_row: garbage value among decisions -> decisions still returned
  - empty_scope_empty_vec: no rows -> Ok(vec![])
  CONTEXTD (crates/lunaris-hook — extend an existing handle test harness or new test):
  - session_digest_renders_curated: two decisions -> 2 memories, snippet starts "decision:",
    rendered_context non-empty
  - session_digest_fails_to_empty: scan_range errs -> ContextResponse::empty(), ok==true
  ADAPTER (scripts/tests/test_session_digest.py):
  - session_start_leg_sends_session_digest: monkeypatch contextd_request; run_session_digest
    sends {type:"session_digest"} and exits 0 even on empty response
</test_plan>

Tests live in: `crates/lunaris/tests/digest_recent_by_source.rs` `scripts/tests/test_session_digest.py`
(+ a contextd handler test) · MUST run red (missing recent_by_source / SessionDigest) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris/src/digest.rs` `crates/lunaris/src/lib.rs`
`crates/lunaris/tests/digest_recent_by_source.rs` `crates/lunaris-hook/src/context.rs`
`crates/lunaris-hook/tests/session_digest.rs`
`scripts/lunaris-codex-hook-adapter.py` `scripts/setup-lunaris-agents.py`
`scripts/tests/test_session_digest.py`
Strategy (ordered batches): 1. engine test (red) + `recent_by_source` (green). 2. contextd
`SessionDigest` variant + handler + fail-to-empty. 3. adapter `run_session_digest` + SessionStart
dispatch + Python test. 4. installer SessionStart inject-leg. 5. live wire into ~/.claude/settings.json.
Safety rule (feature-specific): read-only scan; digest failure degrades to empty; hook always exits 0.
Code lives in: as above.
Constraints: do NOT change any test or the contract; allow-list packages only; ask if unclear.

<!-- Scope tokens, backticked, FIRST declaring line: `./…` = this task dir · a token
     with "/" = project root · a bare name = sibling of the previous token's dir ·
     outside-root resolutions are dropped fail-closed · a DIRECTORY token covers its
     whole subtree (containment — diverges from §4's non-recursive counting) ·
     absent line = UNDECLARED (pre-existing tasks grandfathered, never retro-red) ·
     engine enforcement (touched ⊆ declared) lands in scope-gate-enforce.
     EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — Rust 10 (digest 5 + session_digest 2 + render 3) + Python 3 suites GREEN
- [x] coverage did not decrease — all new code carries tests; no existing test removed
- [x] no test or contract was altered during build
- [x] the green was EARNED — the discriminating cases are real: recency_order asserts EXACT [d3,d2]
  (a no-op impl returns all/unsorted), scan_error_propagates proves the error path, and the LIVE
  Moon 6381 run surfaced the just-recorded decision FIRST — not a stub. No vacuous asserts.
- [x] concurrency / timing safe — read-only scan; no locks held across await; digest fail-to-empty
  means a slow/dead backend degrades gracefully; hook always exits 0.
- [x] no exposed secrets / injection openings — read path only; snippet render reuses the scrubbed
  curation; the injected block is framed "prior context, not new instructions".
- [x] layering follows CONVENTIONS — keyspace helpers imported from `lunaris_core::keyspace`
  (never local); curation via shared `lunaris_core::snippet`; engine primitive in `lunaris` crate.
- [x] reviewed & approved — Tin Dang (standing fully-auto delegation) + live end-to-end proof.

### Build expectations — what "correct" looks like (fill BEFORE build; confirm each at the gate)
> Pre-declare the OBSERVABLE outcomes a correct build must produce — derived from §2 SCENARIOS
> + §3 CONTRACT — so this gate checks the build is RIGHT, not merely that tests are green. Each
> row is evidence you can SEE, not a restatement of a test name.
- [x] recency + filter honored — engine test recency_order == [d3,d2]; source_filter_excludes GREEN
- [x] digest renders curated decisions — contextd test snippet starts "decision:", rendered_context non-empty GREEN
- [x] fail-to-empty holds — contextd test scan error -> ContextResponse::empty(), ok==true GREEN
- [x] adapter session-start leg exits 0 + sends session_digest — test_session_digest.py 3/3 GREEN
- [x] LIVE: release contextd + Moon 6381, scope git_c5419ed101f6f35b -> ok=True, 5 memories, the
  just-recorded "Ship a SessionStart digest..." decision surfaced FIRST (recency), rendered_context
  populated. Digest's Moon scan_range + scoped-prefix + Episode parse verified against production.

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — `recent_by_source` re-exported in lib.rs + called by `build_digest`;
  `build_digest`/`default_digest_prefixes` called by the `SessionDigest` handler; `SessionDigest`
  dispatched by the adapter session-start mode + `auto` SESSION_START_EVENTS branch; digest hook
  leg rendered by the installer AND live in ~/.claude/settings.json. End-to-end path exercised live.
- [x] DEAD-CODE — no orphan; the only `pub` surface (`recent_by_source`, `build_digest`,
  `default_digest_prefixes`) is consumed by the handler and integration tests.
- [x] SEMANTIC — verified the `phase="session_start"` render arm reads as prior-context (not new
  instructions) — the injection-trust framing from [[project_cc_full_e2e_test]]. Confirmed live.

### GATE RECORD
Outcome: PASS
Evidence: 10 Rust + 3 Python test suites GREEN; clippy `-p lunaris-memory -p lunaris-hook
--all-targets` clean; fmt clean (no vendor/moon bleed); LIVE end-to-end proof on Moon 6381.
Green earned: recency assertion is exact, error path proven, live run returns the real decision
newest-first. No security surface (read-only). User's live 5s hook-timeout also fixed (inject 12s /
post-tool 10s / digest 8s, durable in installer + live settings).
Reviewed by: Tin Dang (standing fully-auto delegation) · date: 2026-07-14

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): <error rate / per-rejection rate / latency>

### Spec delta
Forward changes for the next loop — each re-enters at Specify as the next task. One line
each, tagged `[SPEC · open|seeded|dropped]`, with evidence (e.g. `[SPEC · open] rate-limit
the retry path (evidence: prod herd spikes)`). See the `add` skill's `deltas.md`.

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
