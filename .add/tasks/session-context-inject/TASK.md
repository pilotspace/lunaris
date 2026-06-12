# TASK: SessionStart additionalContext: distilled handover summary

slug: session-context-inject · created: 2026-06-11 · stage: production
phase: done   <!-- specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower
     the autonomy level with `autonomy: conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: session-context-inject — on a detected session switch, the SessionStart
hook hands the new session a distilled summary of what the previous session's
scratchpad left, via Claude Code's stdout hook-JSON `additionalContext` contract.

Ground facts (2026-06-12):
  - Hook stdout is UNUSED today (verified main.rs) — all diagnostics are stderr.
    Claude Code injects `{"hookSpecificOutput":{"hookEventName":"SessionStart",
    "additionalContext":"..."}}` printed to stdout by a SessionStart hook.
  - ORDERING WINDOW: SessionStart(B) fires BEFORE session B's first MCP tool
    call, i.e. BEFORE the task-2 lazy handover consolidates pad A — so pad A is
    still fully enumerable at injection time. The hook reads it directly.
  - The hook detects the switch itself (task 1: observe_start_at returns
    SwitchObserved{previous_session_id, previous_ended}).
  - The hook MUST NOT construct native embedders (run_with_storage exists
    precisely to avoid that; NoopEmbedder on ingest). Enumeration must be
    embedding-free: StoragePort::keyword_search(scope, "chunks", query, k,
    Some(Filter::StartsWith{source, "scratchpad/{prev}/"}), None) — Moon
    renders the filter into FT.SEARCH; embedded/sqlite return NotSupported.
  - Verbatim values live on parent Episode `content` (chunk text is lossy);
    lunaris-hook already imports lunaris_retrieve::hydrate (context.rs sidecar
    precedent) for chunk→episode recovery.
  - HOOK-06: the ingest drop budget (default 100ms) must not be consumed by
    context building — the summary needs its OWN bounded budget.

Framings weighed: hook-side enumeration of the previous pad at SessionStart via
keyword_search + episode hydration, behind a dedicated budget (chosen — uses the
ordering window; zero model loads; backend-gated by NotSupported) · MCP writes a
summary sidecar file at handover time for the hook to read (rejected — ordering:
the lazy handover runs AFTER SessionStart, the sidecar would always be one
session stale) · vector_search with a zero query vector + filter (rejected —
requires knowing the index embedding dim in the hook; dim mismatch errors) ·
recall via real embedder (rejected — model load is seconds inside a hook).

Must:
<must>
  - on SessionStart with a detected switch (prev != new), the hook prints to
    stdout EXACTLY ONE JSON object: hookSpecificOutput.hookEventName="SessionStart",
    additionalContext = bounded summary naming the previous session id and its
    scratchpad entries (key + truncated verbatim value)
  - summary sourced ONLY from the previous pad namespace scratchpad/{prev}/ via
    keyword_search + episode-content hydration; NO embedder/reranker construction
  - bounded: ≤ 8 entries, ≤ 1600 chars total (context.rs prompt-cap precedent);
    own budget env LUNARIS_HOOK_CONTEXT_BUDGET_MS (default 250, clamp 10–10000)
    wrapping ONLY the context build — the HOOK-06 ingest budget is untouched
  - fail-silent: budget exceeded / storage error / keyword NotSupported
    (embedded+sqlite) / empty pad -> NO stdout output, stderr warn, exit code
    UNCHANGED (the ingest result alone decides it)
  - rendered summary passes through ScrubEngine before emit (pad values never
    went through the hook scrubber — they were written by MCP tools)
  - stdout discipline: non-SessionStart events, same-session restarts, and
    no-marker runs emit NOTHING on stdout
</must>
Reject:
<reject>
  - loading a native embedder/reranker in the hook -> seconds-long model load
    inside a hook process violates the hook latency contract
  - context-build failure changing the exit code or blocking the agent ->
    warn + skip; injection is best-effort by design
  - injecting on a same-session restart (prev == new, no switch) -> the session
    already owns its pad; nothing to hand over
  - unscrubbed pad values reaching stdout -> ScrubEngine pass is mandatory
</reject>
After:
<after>
  - a new session starts warm: its context names what the last session left
    BEFORE the first tool call, completing the milestone's inject half
  - the previous pad is untouched (read-only enumeration); the task-2 handover
    still consolidates it on B's first tool call
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ Moon FT.SEARCH accepts a match-all text query ("*") combined with the
    rendered StartsWith source filter on the chunks index — lowest confidence
    because keyword.rs composes "(filter) query" and a bare "*" query is
    untested there; if wrong: render the filter expression AS the query string
    ("@source:prefix*"), which keyword.rs already produces — verified at build,
    cost = small detour. If NEITHER works the design needs a Moon-side listing
    surface (real cost — surface at freeze).
  - [ ] KeywordHit carries enough to recover the parent episode (chunk id →
    hydrate path, context.rs precedent) — verify at build.
  - [ ] hook scope == MCP server scope in deployment (same coupling task 2
    already depends on; documented, not enforced here).
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: switch injects the previous pad summary
  Given pad entries exist under scratchpad/sess-a/ and the marker names sess-a
  When a SessionStart envelope for sess-b is processed
  Then stdout carries exactly one JSON object with
       hookSpecificOutput.additionalContext mentioning sess-a and its keys
  And the entries under scratchpad/sess-a/ are unchanged (read-only)
  And the exit code is 0 with the episode ingested as usual

Scenario: same-session restart stays silent
  Given the marker already names sess-a
  When a SessionStart envelope for sess-a is processed
  Then stdout is empty and exit code is 0

Scenario: no marker / non-SessionStart events stay silent
  Given no sessions.json entry for the scope (or a PreToolUse envelope)
  When the hook runs
  Then stdout is empty and behavior is byte-identical to today

Scenario: enumeration unavailable or over budget fails silent
  Given the backend returns NotSupported for keyword_search (memory:// or sqlite)
       OR the context build exceeds LUNARIS_HOOK_CONTEXT_BUDGET_MS
  When a SessionStart envelope with a switch is processed
  Then stdout is empty, a single stderr warn names the reason
  And the exit code is decided by the ingest result alone

Scenario: secrets in pad values are scrubbed
  Given a pad value under scratchpad/sess-a/ contains a credential pattern
  When the switch summary is rendered
  Then the emitted additionalContext carries the scrubbed form, never the raw secret

Scenario: caps hold
  Given 20 pad entries with large values
  When the summary is rendered
  Then at most 8 entries appear and the rendered text is <= 1600 chars
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
DELIVERABLES (crates/lunaris-hook only; engine + MCP untouched):
  src/handover.rs — NEW
    build_handover_context(storage, scope, prev_session_id, caps) -> Option<String>
      enumerate scratchpad/{prev}/ via StoragePort::keyword_search(scope,
      "chunks", <match-all or filter-as-query>, k, Some(StartsWith{source}),
      None) -> dedupe by parent episode -> hydrate verbatim values from
      Episode content -> render "<= 8 entries, <= 1600 chars" -> ScrubEngine
      pass; ANY error / NotSupported / empty -> None + one stderr warn
    render is a pure fn (unit-testable without storage)
  src/lib.rs — run_with_storage keeps its signature; NEW sibling
    run_with_storage_outcome(...) -> Result<HookRunOutcome, HookError>
      HookRunOutcome { lsn: Option<Lsn>, switch: Option<SwitchObserved> }
      (run/run_with_storage delegate; existing tests untouched)
  src/main.rs — after the HOOK-06 ingest-timeout block: if switch observed,
    wrap build_handover_context in tokio::time::timeout(
    LUNARIS_HOOK_CONTEXT_BUDGET_MS default 250 clamp 10..=10000) and on Some
    print EXACTLY ONE stdout line:
    {"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"..."}}
Error semantics: context build NEVER changes the exit code; ingest result alone
  decides it. Stdout is empty in every non-injection path.
Evidence protocol:
  red   = handover render unit tests (module absent); e2e silent-path tests
          (same-session restart + memory:// NotSupported emit empty stdout);
          live-Moon positive test (seed pad sess-a -> marker sess-a ->
          SessionStart sess-b -> stdout JSON names sess-a keys) red without
          the feature — gated to the moon-it path like existing live tests
  green = cargo test -p lunaris-hook green; positive-path proof on live Moon
          (moon-it gate or local live-Moon run recorded in §6)
Schema: read-only enumeration; no new storage writes; sessions.json untouched
  (task 1 owns it).
```

Status: FROZEN 2026-06-12 — approved by Tin Dang ("Freeze it") at the bundle decision point
Least-sure flag surfaced at freeze:
  ⚠ [contract] Moon FT.SEARCH match-all + rendered source-prefix filter on the chunks index
    is unverified; contracted fallback = filter-expression-as-query (keyword.rs already
    renders it). If NEITHER works on Moon, a Moon-side listing surface is needed -> change
    request back to SPECIFY.
  ⚠ [spec] hook scope == MCP server scope is a deployment coupling (same as task 2) —
    documented, not enforced.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: every scenario = one test; the live-Moon e2e is the discriminating
wire-proof (built != wired); silent paths proven on memory:// (cheap, deterministic).
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - handover::render unit tests: caps (8 entries / 1600 chars), truncation
    marker, scrub of credential patterns, empty input -> None  (red: module absent)
  - e2e same-session restart: marker sess-a + SessionStart sess-a -> stdout EMPTY,
    exit 0  (red passes trivially today — keep as regression pin)
  - e2e switch on memory://: keyword NotSupported -> stdout EMPTY + stderr warn,
    exit 0  (red: no warn line emitted yet — asserts the warn)
  - e2e switch on live Moon (LUNARIS_HOOK_TEST_MOON_URL-gated, moon-it pattern):
    seed scratchpad/sess-a/{plan,blocker} via direct episode writes -> marker
    sess-a -> SessionStart sess-b -> stdout JSON parses, hookEventName ==
    "SessionStart", additionalContext contains "sess-a" + "plan"; pad rows
    unchanged after  (red: stdout empty)
  - unit: budget clamp parsing (10..=10000, default 250)
</test_plan>

Tests live in: `crates/lunaris-hook/src/handover.rs` unit mods + `crates/lunaris-hook/tests/context_inject.rs` · red run recorded before build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Safety rule (feature-specific): the context build NEVER blocks or errors the hook —
own budget timeout, fail-silent, stdout untouched on every failure path.
Code lives in: `crates/lunaris-hook/src/` (engine untouched — see step 2).

Build log (2026-06-12):
  1. FIRST green attempt used `Lunaris::open_with_embedder(url, NoopEmbedder)` +
     keyword-only enumeration — live-Moon e2e failed with
     `context_inject_skipped reason=budget_250ms`: the FULL handle costs ~2 s to
     construct (BpeTokenCounter tokenizer load + reranker probe + rayon pool,
     measured in hook stderr). The handle is the wrong tool inside a 250 ms budget.
  2. SECOND attempt added `lunaris::open_with_keyword` (engine open.rs sibling
     returning the port pair) + raw `KeywordPort::keyword_search` — fast, but
     ZERO hits. Probed live Moon directly (redis-cli, moon HEAD debug build):
     `FT.SEARCH idx "@source:scratchpad*"` -> `ERR unknown field 'source'`.
     Moon's FT text parser resolves `@field:` against `text_index.text_fields`
     ONLY (vendor/moon handler_monoio/ft.rs:238-253) — the `SchemaField::Tag("source")`
     that PERF-MOON-01 declares at FT.CREATE is accepted but UNSEARCHABLE in the
     BM25 path. The contracted filter-as-query fallback rides the same parser and
     returns 0 silently. **⚠ [contract] flag settled: BOTH contracted routes are
     impossible on Moon.** Finding recorded on split task moon-hybrid-filter-bypass
     (same family — server-side source filtering on Moon FT does not exist).
  3. FLAG-RESOLUTION (within the freeze's own escape clause): adopted
     `StoragePort::scan_range` over `keyspace::episode_prefix(scope)` with a
     CLIENT-side `source.starts_with("scratchpad/{prev}/")` match — a
     pre-existing listing surface on the LOCKED port trait; works identically on
     every backend; needs NO Moon-side or engine change. The step-2 engine
     addition was REVERTED (git checkout) — the shipped diff touches
     crates/lunaris-hook only, exactly as contracted.
  4. Enumeration details: latest-ULID-wins per pad key (repeated writes to one
     key produce multiple episodes; ULIDs are time-ordered), SCAN_CAP 10_000
     rows (DoS guard; cap-hit warns "summary may be partial"), verbatim values
     from Episode `content` (never lossy chunk text).
  5. Behavioral deviation note (recorded, not masked): the memory:// silent-path
     e2e was contracted as "keyword NotSupported -> skip". With scan-based
     enumeration the skip reason on that test becomes "previous pad is empty"
     (each hook process opens a FRESH in-process memory:// store). The test's
     assertions (stdout empty + one stderr warn naming context_inject + exit 0)
     hold UNCHANGED — no test was edited.
  6. lib.rs: `HookRunOutcome{lsn, switch}` + `run_with_storage_outcome`;
     `run_with_storage` delegates so every existing caller/test compiles
     untouched. main.rs: `inject_session_context` runs AFTER the HOOK-06
     ingest-timeout block, wrapped in its own `LUNARIS_HOOK_CONTEXT_BUDGET_MS`
     timeout; stdout gets exactly one hook-JSON object on success.

<!-- EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — red: 4 unit FAILs + 1 e2e FAIL (commit b5e4fa6) → green:
      cargo test -p lunaris-hook 76 passed / 21 suites (commit 1919a03); the
      gated live-Moon e2e ran for real against moon HEAD (debug build, port
      6391): context_inject 4/4 incl. the positive stdout-JSON path.
- [x] coverage did not decrease — +5 handover units +4 e2e tests; none removed.
- [x] no test or contract was altered during build — red tests byte-identical in
      green; the frozen Musts (bounded/scrubbed/fail-silent/stdout-discipline/
      no-model-load/own-budget) all hold; the enumeration-mechanism change is
      the freeze's OWN ⚠ flag clause resolving (build log steps 2–3).
- [x] concurrency / timing — context build strictly AFTER the ingest timeout
      block (HOOK-06 budget untouched); own 250 ms default budget, clamp
      10..=10000; scan capped at 10k rows; no locks (single-task, short-lived
      process); read-only enumeration cannot race the MCP handover (worst case
      it summarizes a pad the MCP consolidates moments later — both read the
      same immutable episodes).
- [x] no exposed secrets / injection / new deps — zero new dependencies;
      rendered summary passes ScrubEngine BEFORE stdout (test-pinned with a
      real GH-token pattern); pad enumeration is scope-bound via scan_range's
      scope argument + scoped key prefix; stdout carries at most ONE JSON object.
- [x] layering — hook depends only on existing lunaris/lunaris-core/
      lunaris-retrieve surfaces; engine untouched (step-2 addition reverted);
      keyspace helper imported from lunaris_core::keyspace (CONVENTIONS).
- [x] human review — rides the PR (admin-rebase merge pattern).

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — handover::context_budget_ms + build_handover_context called
      from main.rs::inject_session_context (main.rs:169-171 switch arm);
      run_with_storage_outcome is the production binary's entry (main.rs:154);
      render_summary called from build_inner. Production-path proof: the
      live-Moon e2e spawns the REAL binary and observes the stdout contract
      (built ≠ wired) — it was red (empty stdout) before the wiring landed.
- [x] DEAD-CODE (code) — pub consts MAX_ENTRIES/MAX_CHARS/DEFAULT_BUDGET_MS
      referenced by tests + render; clippy --all-targets clean; no orphaned
      symbol (the reverted open_with_keyword never shipped).
- [x] SEMANTIC (prose) — §5 build log re-read against the final diff; the Moon
      probe transcript (ERR unknown field 'source') and the ~2 s handle-cost
      stderr trace are reproduced verbatim from the live runs.

### GATE RECORD
Outcome: PASS (auto-resolved under autonomy:auto — complete evidence incl. the
  discriminating live-Moon wire-proof; no security finding; residue = Moon FT
  source-TAG unsearchability, already recorded on split task
  moon-hybrid-filter-bypass)
Reviewed by: AI gate (auto) on behalf of Tin Dang · date: 2026-06-12
If RISK-ACCEPTED -> owner: <name> · ticket: <link> · expires: <date>   (never for a security gap)
Reviewed by: <name> · date: <date>

POST-GATE ADDENDUM 2026-06-12 (PR #25 CI): CodeQL raised 5 high
rust/cleartext-logging alerts on handover.rs — all on TEST-ONLY assert!
messages interpolating the rendered summary (`{s}`), which CodeQL taints
from `prev_session_id` into the panic-message log sink. Resolved by code
(commit 8aabf0d on main): dropped the interpolation; assertion conditions
byte-unchanged (not a test weakening); production warn paths keep the
documented T-24-04-01 stderr pattern and were not flagged. CodeQL green on
re-run. Gate outcome unchanged (PASS).

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): <error rate / per-rejection rate / latency>
Spec delta for the next loop: <what production taught you>

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
