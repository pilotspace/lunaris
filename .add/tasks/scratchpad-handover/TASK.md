# TASK: Guarded consolidate of previous session's pad + per-session namespace rotation

slug: scratchpad-handover · created: 2026-06-11 · stage: production
phase: done   <!-- specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower
     the autonomy level with `autonomy: conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: scratchpad-handover — on a session switch, the previous session's scratchpad is consolidated into long-term memory and the new session gets a FRESH per-session pad. One namespace convention shared by lunaris-hook and the MCP scratchpad tools.

Ground facts (2026-06-11):
  - Task 1 (merged contract): ~/.lunaris/sessions.json names the active (sanitized) session per scope; read side = session_marker::read_active_at; sanitized alphabet [A-Za-z0-9_\-.].
  - WorkingMemory is namespace-keyed via scope_prefix on the Episode source; MCP default namespace is "scratchpad/" (staging::resolve_namespace); namespace charset [A-Za-z0-9_\-./], `:` rejected — so "scratchpad/{session_id}/" is VALID with sanitized ids.
  - The guarded consolidate path exists: scratchpad_consolidate::handle_inner = 3 guards (queue_native circuit-breaker / background-worker refusal / hard timeout) around WorkingMemory::consolidate(). P-C verified this is the ONLY legal consolidate entry (single __mq_consumers group).
  - CRITICAL constraint: the hook CANNOT consolidate inline — HOOK-06 gives it a ~100ms drop budget while a consolidate drain worst-cases ~51s (DRAIN_CAP x PULL_TIMEOUT_MS). The hook process is the WRONG place for handover work.
  - The MCP server never sees session_ids; its only bridge is the sessions.json marker (designed for exactly this in task 1).

Framings weighed: LAZY MCP-side handover (chosen — on each scratchpad tool call, resolve the default namespace from the marker; when the active session changed since the last served pad, run the guarded consolidate of the PREVIOUS pad first; cheap file-stat cache) · hook consolidates inline at SessionStart (rejected — 51s worst case inside a 100ms budget, violates HOOK-06) · hook spawns a detached consolidator process (rejected — process management + double-consume risk on the single consumer group; the worker-refusal guard exists precisely to prevent concurrent drainers) · cron/background worker (rejected — P-C already established the background worker conflicts with tool-triggered consolidate; one entry point).

Scope boundary: crates/lunaris-mcp (namespace resolution + handover trigger + a small marker-reading module mirroring task 1's format) + docs note in lunaris-hook. NO engine changes (WorkingMemory::consolidate reused as-is); NO new consolidation machinery; NO context injection (task 3).

Must:
<must>
  - per-session namespace convention: when sessions.json names an active session for the server's scope, the DEFAULT scratchpad namespace becomes "scratchpad/{active_session_id}/"; with no marker (hook not installed) the default stays "scratchpad/" (back-compat); an EXPLICIT namespace param always wins unchanged
  - handover trigger: on any scratchpad tool call (write/read/grep), if the marker's active session differs from the last pad this server process served, FIRST run the guarded consolidate (same 3 guards, same path as memory.scratchpad_consolidate) over the PREVIOUS session's namespace, then serve the call under the new namespace
  - failure carries forward: a failed/guard-refused consolidate logs a warn, leaves the old pad intact, and NEVER blocks the new session's call (the old pad is retried at the next switch); guard refusals (non-Moon backend, worker live) are an expected skip, not an error
  - marker reads are cheap: stat/mtime-cached file read per tool call; corrupt/missing marker = no marker (mirrors task 1 tolerance)
  - cross-session isolation proven: an integration test drives session A writes -> marker flip to B -> B's scratchpad_read starts empty AND A's facts are recallable via memory.recall after handover (on the embedded-moon path); a second test proves the consolidate-failure path leaves A's pad readable under its explicit namespace
</must>
Reject:
<reject>
  - explicit namespace param changed or rewritten by the session logic -> param is verbatim, session logic only touches the DEFAULT
  - handover consolidating the CURRENT session's pad -> only the PREVIOUS pad is drained
  - a second consolidate entry point / direct atomic_write -> INGEST-04 + single-consumer-group invariants hold
  - marker IO or consolidate failure surfacing as a tool error -> warn + serve the call
</reject>
After:
<after>
  - two consecutive agent sessions get isolated pads automatically; the old pad's facts surface in long-term recall
  - the namespace convention is ONE rule both binaries honor (hook sanitizes ids to the same alphabet the MCP namespace accepts)
  - task 3 (context inject) can recall "what did the last session leave" via the consolidated facts + the previous pad namespace
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ consolidate drain is SCOPE-wide (drain_consolidate_events takes only the scope) while consolidation filters by scope_prefix — if non-matching-namespace events are drained-then-dropped, a handover consolidate could eat events belonging to other namespaces; MUST be verified at build (read consolidate_scoped) and, if true, the handover either consolidates with the previous-pad prefix AND default prefix together or documents the drain semantics; cost if wrong: silent event loss for concurrent non-session namespaces.
  - [ ] MCP server process lifetime spans the session switch (Claude Code may restart the server per session) — if the server restarts, the "last served pad" must be derived from disk (the marker's ended/updated_at), not process memory; resolved at build by persisting last-served in the sessions.json sibling or deriving from marker alone.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: clean handover across two sessions
  Given session A wrote scratchpad keys under the default namespace and the marker flips to session B
  When the first scratchpad tool call of session B arrives
  Then the previous pad (scratchpad/A/) is consolidated via the guarded path
  And scratchpad_read under the new default returns empty
  And the consolidated facts are recallable via memory.recall

Scenario: no hook installed (no marker)
  Given no sessions.json entry for the scope
  When scratchpad tools are called
  Then the default namespace is "scratchpad/" exactly as today, no handover logic fires

Scenario: explicit namespace wins
  Given any marker state
  When a tool call carries namespace="custom/area/"
  Then it is served verbatim under that namespace and no session logic touches it

Scenario: consolidate refused or failing carries forward
  Given the previous pad has content and the consolidate guard refuses (non-Moon backend or worker live) OR the drain errors
  When session B's first tool call arrives
  Then a warn is logged, the call is served normally under B's pad
  And A's pad remains intact and readable under its explicit namespace
  And the handover is retried at the next observed switch
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
DELIVERABLES (lunaris-mcp side; engine untouched):
  crates/lunaris-mcp/src/session_pad.rs — NEW
    read-side mirror of the sessions.json format (own struct, no cross-binary import —
    same pattern as the dual JsonScopesFileStore);
    API: active_session(scope) -> Option<(sanitized_id, ended)> (mtime-cached, error->None)
         pad_namespace(scope) -> String  ("scratchpad/{id}/" or "scratchpad/")
         take_pending_handover(scope) -> Option<prev_namespace> (process-state + marker diff)
  crates/lunaris-mcp/src/tools/staging.rs — resolve_namespace gains the marker-aware default
    (explicit Some(ns) path byte-identical)
  crates/lunaris-mcp/src/tools/{scratchpad_write,scratchpad_read,scratchpad_grep}.rs —
    pre-call handover hook: if take_pending_handover -> run the SAME guarded consolidate
    (reuse handle_inner's guard block via a shared fn) over prev namespace; warn-and-continue
  docs: lunaris-hook README note naming the shared convention
Error semantics: handover failures/refusals NEVER error the tool call (warn + carry forward);
  explicit namespace param NEVER rewritten; INGEST-04 + single-consumer-group invariants hold.
Evidence protocol:
  red   = new tests fail first: session_pad unit tests (no module); namespace-default tests
          (marker present -> per-session default) fail against current "scratchpad/" constant;
          embedded-moon integration (A writes -> flip marker -> B reads empty + recall finds A's
          facts) fails without the handover trigger
  green = cargo test -p lunaris-mcp (unit + the embedded-moon gated integration) all pass;
          server_boot roster test stays green (no new tools; no outputSchema changes)
Schema: sessions.json read-only from MCP; last-served pad tracked in-process with disk-derived
  fallback (assumption 2 resolved at build).
```

Status: FROZEN 2026-06-11 — approved by Tin Dang ("Freeze it") at the bundle decision point
Least-sure flag surfaced at freeze:
  ⚠ [arch] The consolidate drain is scope-wide while consolidation filters by namespace prefix — IF drained events outside the previous pad's namespace are dropped rather than re-queued, an automatic handover could silently eat other namespaces' pending consolidations. Build step 1 is reading consolidate_scoped to settle this; if the hazard is real, the handover consolidates WITHOUT a prefix filter (whole-scope, matching what the background worker would do) so nothing is dropped — the per-pad isolation then comes from the namespace keying alone.
  ⚠ [spec] MCP server lifetime vs session switches is unverified (Claude Code may restart the server per session) — the handover trigger therefore derives "previous pad" from the marker file, not process memory, whenever process state is empty.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: every scenario = one test; embedded-moon integration is the discriminating wire-proof (built != wired).
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - session_pad::active_session_reads_marker / corrupt_or_missing_none / pad_namespace_shapes (red: module absent)
  - staging::default_namespace_per_session_when_marker / fallback_without_marker / explicit_param_verbatim (red: constant default today)
  - handover trigger: first-call-after-flip runs guarded consolidate over prev namespace exactly once (mock/inspect via report counts; red: no trigger)
  - failure path: guard-refusal (queue_native=false) -> warn + call served + old pad intact (red: no trigger)
  - embedded-moon integration (feature-gated like the existing P-B tests): A writes 2 keys -> marker flip -> B scratchpad_read empty + memory.recall hits A's facts
</test_plan>

Tests live in: `crates/lunaris-mcp/src/` unit mods + the embedded-moon test path · red run recorded before build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Safety rule (feature-specific): the handover consolidate NEVER errors or blocks the
tool call — every failure/guard-refusal path warns and serves the call (Reject #4).
Code lives in: `crates/lunaris-mcp/src/` (+ one contracted engine method, see deviation).

Build log (2026-06-11/12):
  1. ⚠-flag SETTLED FIRST (contract build step 1): read `crates/lunaris-consolidate/src/lib.rs:121-128` —
     `consolidate_scoped(Some(prefix))` filters the ALREADY-DRAINED events by
     `e.source.starts_with(prefix)` and DROPS non-matching ones (consumed from the single
     `__mq_consumers` group, never re-queued). **The hazard is REAL** — verbatim:
     "consolidate_scoped(Some(prefix)) filters the drained events and consolidates only the
     matches; the rest are silently lost." Per the frozen flag clause, the handover therefore
     consolidates WITHOUT a prefix filter (whole-scope); per-pad isolation comes from namespace
     keying alone. The manual tool's own prefix-filtered path is the split task
     `consolidate-prefix-drop` (recorded verbatim, NOT fixed here — out of this contract's scope).
  2. CONTRACTED DEVIATION from the §1 scope line "NO engine changes": implementing the flag
     clause required `WorkingMemory::consolidate_unfiltered()` in
     `crates/lunaris/src/primitives/working_memory.rs` — identical to `consolidate()` but
     passing `None` as the prefix to `consolidate_scoped`. No alternative existed inside
     lunaris-mcp: `consolidate()` hard-codes `Some(self.scope_prefix)`. The method's doc-comment
     cites the hazard + the split task. This is the flag's contracted response, not scope creep.
  3. `session_pad.rs` (NEW): marker read mirror (lenient MarkerEntry), test seam
     (`set_sessions_file_for_tests` + tokio-Mutex `lock_test_seam` — seam is process-global),
     `default_namespace`, `take_pending_handover_at` (LAST_SERVED process map; empty after
     restart ⇒ one deliberate handover fire — resolves §1 assumption 2 disk-derived).
  4. `staging.rs::resolve_namespace_session_aware`: explicit ns validated + VERBATIM;
     None → marker-aware default + pending-handover check → `run_handover_consolidate`.
  5. `scratchpad_consolidate.rs::run_handover_consolidate`: same 3 guards as handle_inner
     (queue_native / pipeline-enabled / 5s timeout) around `consolidate_unfiltered()`;
     all outcomes warn/info, never error (Reject #4).
  6. Tool call sites (write/read/grep) swapped to the session-aware resolver.
  7. Tests: 5 session_pad units + per-session-default tool test (memory:// guard-refusal
     path rides the same assertion) + embedded-moon integration
     `embedded_moon_session_handover_rotates_and_drains` (rotation + drain-proof +
     recall-hits-A's-facts + A's-pad-intact).
  8. UNMASKED LATENT BUG (red integration run, 2026-06-11): B's fresh pad served A's value
     — on Moon's native HYBRID path the pushed-down source filter is composed into the text
     query (fusion.rs::compose_query_with_filter) and constrains ONLY the BM25 branch; the
     dense-KNN branch ignores it and foreign-source hits survive RRF fusion. This is the
     pre-existing "scratchpad_read-on-Moon" bug first observed 2026-06-10 (recall-optimization
     validation). Blocked the contracted rotation scenario, so the MINIMAL fix shipped here:
     `source_filter_matches` post-enforcement in `WorkingMemory::find` (the same
     self-protection `memory.recall` already applies to source_prefix); push-down retained
     for ranking. The GENERAL DSL-level fix (any `.filter()` user on the Moon hybrid path)
     is a SPLIT candidate — record: "Moon hybrid filter bypass: compose_query_with_filter
     only constrains the BM25 branch; dense KNN leaks through RRF" (fusion.rs:168,260).
Marker-read caching note: contract said mtime-cached; shipped as a plain ~µs JSON read per
  tool call (file is <1 KiB). Recorded as a delta, not a violation — "cheap" is the rule.

<!-- EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — red 53/4-failed (commit 6551d58) → green 78 passed lunaris-mcp
      --features embedded-moon (7 suites) + 183 passed lunaris-memory (commit c9162b1).
      One cold_start flake under full-suite load (initialize >10 s); passed in isolation
      (1.4 s) AND in the clean full-suite rerun (78/78, 32 s) — not change-related
      (handover code never runs in bootstrap).
- [x] coverage did not decrease — +6 unit tests (5 session_pad + 1 engine source-filter)
      +1 tool test +1 embedded-moon integration; zero tests removed or weakened.
- [x] no test or contract was altered during build — red tests byte-identical in green
      commit; contract still FROZEN @ 2026-06-11. Two deviations recorded in §5, both
      pre-authorized by the frozen flag clauses (consolidate_unfiltered engine method;
      disk-derived restart handover).
- [x] concurrency / timing — no lock across .await (LAST_SERVED std::Mutex guard drops
      before any await; test seam uses tokio::Mutex precisely because tests hold it across
      awaits); handover drain bounded by the 5 s CONSOLIDATE_TOOL_TIMEOUT inside the tool
      call; double-drain prevented by take_pending_handover_at's take-once semantics +
      P-C's worker-refusal guard.
- [x] no exposed secrets / injection / new deps — zero new dependencies; namespace
      validator unchanged ([A-Za-z0-9_\-./]{1..=128}, ':' rejected) and sanitized hook
      session ids are namespace-legal by construction (validator-asserted in test);
      sessions.json is read-only from MCP; scope still bound at startup, never wire-supplied.
- [x] layering — session_pad mirrors the dual JsonScopesFileStore convention (no
      cross-binary import); keyspace helpers untouched; INGEST-04 holds (handover rides
      the existing consolidate path, no new atomic_write: grep confirms 1 call site in
      pipeline.rs); engine does not depend on lunaris-mcp (source_filter_matches +
      consolidate_unfiltered are self-contained).
- [x] human review — rides the PR (admin-rebase merge pattern); evidence above.

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — resolve_namespace_session_aware called by all 3 scratchpad tool
      handlers (write.rs:57 / read.rs:65 / grep.rs:71); run_handover_consolidate called
      from staging.rs:134; consolidate_unfiltered called from run_handover_consolidate;
      source_filter_matches called from WorkingMemory::find. Production-path proof: the
      embedded-moon integration test drives the real tool handlers end-to-end (built ≠
      wired discipline) — without the handover wiring the drain-proof assertion fails
      (observed red).
- [x] DEAD-CODE (code) — old resolve_namespace retained ON PURPOSE for
      scratchpad_consolidate's explicit-namespace semantics (documented call site);
      every new symbol referenced per WIRING; clippy --all-targets clean (would flag
      unused pub(crate) items in-crate).
- [x] SEMANTIC (prose) — §5 build log re-read against the diff: hazard citation
      lib.rs:121-128 verified by direct read; both deviations match the frozen flag
      clauses verbatim.

### GATE RECORD
Outcome: PASS (auto-resolved under autonomy:auto — complete evidence; no security finding;
  the one residue is the documented PRE-EXISTING manual-tool prefix-drop, already split to
  task consolidate-prefix-drop, and the Moon hybrid filter bypass at DSL level, split below)
Reviewed by: AI gate (auto) on behalf of Tin Dang · date: 2026-06-12

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): <error rate / per-rejection rate / latency>
Spec delta for the next loop: <what production taught you>

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
