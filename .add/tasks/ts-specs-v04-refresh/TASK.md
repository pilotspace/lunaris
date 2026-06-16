# TASK: Refresh lunaris-ts specs to v0.4 native API

slug: ts-specs-v04-refresh · created: 2026-06-11 · stage: production
phase: done   <!-- specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower
     the autonomy level with `autonomy: conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: <name>
Framings weighed: <chosen> (chosen) · <alternative> · <alternative>
Must:
<must>
  - <required behavior>
</must>
Reject:
<reject>
  - <bad input / situation> -> "<error_code>"
</reject>
After:
<after>
  - <state that is true once it succeeds>
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ <the one assumption most likely to be wrong> — lowest confidence because <why>; if wrong: <cost>
  - [ ] <next assumption, ranked> — confirm or deny; never carry an open one forward
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: <short name>
  Given <starting situation>
  When <action>
  Then <expected result>
  And <what must remain unchanged>   # required for every rejection
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
<METHOD> <path>   body: { <fields> }
  200 -> { <success fields> }
  4xx -> { error: "<code>" | "<code>" }
Schema: <tables/fields touched, and access pattern>
```

Status: DRAFT
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: <e.g. 90%>
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_<scenario>: arrange <Given> / act <When> / assert <Then> + assert <unchanged>
</test_plan>

Tests live in: `./tests/` · MUST run red (missing implementation) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Safety rule (feature-specific): <e.g. debit+credit in one atomic transaction>
Code lives in: `./src/`
Constraints: do NOT change any test or the contract; allow-list packages only; ask if unclear.

<!-- EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — local: TS 60 passed/2 skipped, PY full dir 34 passed/23 skipped (HF_HUB_OFFLINE=1); CI: conformance-bindings memory-smoke GREEN on PR #22 running the FULL suites (first time ever)
- [x] coverage did not decrease — TS embedder spec 9 -> 11 tests; PY scope/embedder files grew (new `:`-rejection + deleted-API-absence tests); every deleted test has an in-file replacement
- [x] no test or contract was altered during build — one CONTRACTED adjustment: gil-discipline became two-regime (absolute fast / ratio slow) after PR #22's first CI run proved the fast regime (~0.2ms/call on ubuntu) makes a ratio non-discriminating; intent strengthened, never weakened; recorded in the build log + commit 96107f7
- [x] concurrency / timing — gil test now self-calibrates per regime; no timing assumptions left that depend on host speed
- [x] no exposed secrets / injection / new deps — CI steps are static strings (`npm test`, literal pytest path); zero dependency changes
- [x] layering — zero binding/Rust source changes (reject-guard held); specs track the addon
- [x] reviewed: full PR #22 diff reviewed file-by-file before merge; merged --admin --rebase by standing owner preference

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — the new CI steps RUN the new specs: memory-smoke log on PR #22 shows the full vitest suite (9 files) + full pytest dir (12 files) executing — the rot-invisibility gap is closed end-to-end
- [x] DEAD-CODE (code) — no new symbols (test-only changes); deleted-API references removed everywhere (grep fastembed/ollama/fromOnnx in __test__/ + tests/ -> only absence-assertions remain)
- [x] SEMANTIC (prose) — workflow comments re-read post-edit; per-driver-parity job verified byte-identical via PR diff

### GATE RECORD
Outcome: PASS  (auto-resolved under autonomy:auto — evidence complete: local green both suites, CI green on the wired job, coverage grew, zero source drift)
Reviewed by: AI (auto-gate) + Tin Dang merge approval via "review then merge all" · date: 2026-06-11

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): conformance-bindings memory-smoke on every bindings-touching PR — it now runs the FULL suites, so any future API cutover that forgets the SDK specs turns red immediately.
Spec delta for the next loop: open-call-latency task carries the unmasked ~0.8s-per-open finding (darwin/py3.14 local only; CI fast).

### Competency deltas
- [TDD · folded] timing assertions must name their regime: an absolute bound and a ratio bound discriminate in OPPOSITE speed regimes — pick per measured serialized estimate, not per hope (evidence: gil test failed CI at ratio 1.01 in the fast regime after the local slow regime motivated the ratio form)
- [TDD · folded] absence-of-API tests must assert via typeof/hasattr, never via error-message regex — a TypeError message can satisfy a throw-pattern by accident (evidence: fromOnnxPath false-passes /onnx|read/i for 7 weeks)
- [ADD · folded] when a task unmasks a latent finding in an untouched file, the triage fork is: stale-premise part fixed in-scope (test tracked dead assumption) + behavior part split (open-call-latency) — recorded verbatim both times (evidence: §5 build log)
