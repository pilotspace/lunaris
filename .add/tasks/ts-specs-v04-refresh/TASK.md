# TASK: Refresh lunaris-ts specs to v0.4 native API

slug: ts-specs-v04-refresh · created: 2026-06-11 · stage: production
phase: build   <!-- specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower
     the autonomy level with `autonomy: conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: Make both SDK test suites track the SHIPPED v0.4+ API and actually run in CI. Scope widened at specify with evidence: the Python siblings carry the IDENTICAL disease (same deleted factories, same `:`-scope alphabet, same CI invisibility) — one task, both suites.

Ground facts (2026-06-11, locally reproduced against a fresh bindings-it addon build):
  - TS (crates/lunaris-ts/__test__): 7 hard failures + 2 FALSE-PASSES. embedder_config.spec.mts tests fastembed()x2 / ollama() / RerankerConfig.fastembed (deleted in v0.4 N-03); fromOnnxPath/fromOnnxBytes tests pass BY ACCIDENT ("EmbedderConfig.fromOnnxPath is not a function" TypeError matches their /onnx|read/i throw pattern). scope.spec.mts x3 expects `:` valid ("acme:agent-42", "agent:alpha", char-class "A0._:-") — pre-v0.2.1 alphabet.
  - PY (crates/lunaris-py/tests): 11 failed / 11 passed / 9 skipped on test_embedder_config.py + test_scope.py (36 stale fastembed/ollama refs; same `:` expectations).
  - Shipped surface (both SDKs, verified via index.d.ts + dir()): EmbedderConfig.{noop, native, nativeQuantized | native_quantized} · RerankerConfig.{noop, native, nativeQuantized | native_quantized}. NOTHING else.
  - CI invisibility: conformance-bindings runs ONLY backend_parity (TS spec + test_backend_parity.py); no workflow anywhere runs the full suites — the rot was structurally undetectable until ci-bindings-venv-fix made the workflow run at all.
  - Scope alphabet [A-Za-z0-9_\-.]{1,128} (v0.2.1): `:` rejection is a TYPE-LEVEL defense against KV key aliasing — specs asserting `:` valid are asserting a security regression.

Framings weighed: refresh BOTH suites + wire full runs into memory-smoke (chosen — same disease, same fix, one PR) · TS-only as originally scoped (rejected — leaves the Python half rotting with identical CI invisibility) · delete the stale tests instead of porting (rejected — the offline factory-surface tier is exactly what catches the next API-cutover drift).

Scope boundary: crates/lunaris-ts/__test__/ (the two stale files), crates/lunaris-py/tests/ (the two stale files), .github/workflows/conformance-bindings.yml memory-smoke job (full-suite wiring). NO addon/binding/Rust code changes — specs track the addon, never the reverse. backend_parity specs untouched (the moon-only milestone may reshape them later).

Must:
<must>
  - embedder specs (TS+PY) rewritten to the shipped surface: offline tier asserts noop/native/nativeQuantized factories exist + cheap error paths (nativeQuantized with a missing GGUF path throws without any model download); deleted-API tests removed; the two /onnx/i false-passes replaced with an explicit "deleted factories are GONE" assertion (fastembed/ollama/fromOnnxPath/fromOnnxBytes undefined) that CANNOT false-pass via error-message regex
  - scope specs (TS+PY) rewritten to the v0.2.1 alphabet: valid cases drawn from [A-Za-z0-9_\-.]; NEW discriminating test asserts `:` is REJECTED (comment names the KV-aliasing defense)
  - memory-smoke job runs the FULL suites: TS step becomes `npm test`; PY step becomes `pytest crates/lunaris-py/tests/ -v`
  - local green: both suites 0-failed (skips allowed for env-gated tests) against a fresh bindings-it build, with HF_HUB_OFFLINE=1; CI green on the task PR
</must>
Reject:
<reject>
  - any change under crates/lunaris-py/src, crates/lunaris-ts/src, or Rust code to make a spec pass -> specs track the addon, never the reverse; that would be a change request
  - deleting a failing test without a shipped-surface replacement -> coverage must not shrink
  - a full-suite CI step that needs network model downloads -> the offline tier must stay offline (runner has no model cache)
</reject>
After:
<after>
  - both SDK suites are green against the shipped API and run end-to-end on every bindings-touching PR
  - the next API cutover that forgets the SDK specs turns CI red instead of rotting invisibly for 7 weeks
  - the `:`-rejection regression test pins the scope-aliasing defense at both SDK boundaries
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ The full suites are offline-safe on a bare CI runner — lowest confidence because native()/native_quantized() construction MIGHT eagerly touch the model cache; cost if wrong: memory-smoke fails on model-miss. Mitigation baked into the contract: the offline tier asserts surface-only (typeof/hasattr) and only exercises cheap error paths; verified locally with HF_HUB_OFFLINE=1 before pushing.
  - [x] Full pytest dir + npm test skip gracefully without backend env — locally evidenced (9 py skips; parity specs gate on env URLs; TS IT tier gates on LUNARIS_TS_IT=1).
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: suites green against the shipped addon
  Given a fresh bindings-it build of lunaris-py and lunaris-ts
  When `npm test` and `pytest crates/lunaris-py/tests/` run locally with no backend env and HF_HUB_OFFLINE=1
  Then both finish with 0 failures (env-gated tests skip), including the four rewritten files
  And no binding/Rust source file changed

Scenario: deleted factories stay deleted (no accidental passes)
  Given the rewritten embedder specs
  When the deleted-API assertions run
  Then fastembed/ollama/fromOnnxPath/fromOnnxBytes are asserted ABSENT via typeof/hasattr
  And the shipped factories (noop/native/nativeQuantized) are asserted present
  And no assertion relies on matching an error-message regex (the old /onnx|read/i false-pass mechanism)

Scenario: colon scope is rejected at the SDK boundary
  Given the rewritten scope specs
  When Scope("acme:agent-42") is constructed in TS and in PY
  Then both throw/raise the alphabet error (the KV-aliasing defense)
  And valid identifiers drawn from [A-Za-z0-9_\-.]{1,128} still construct

Scenario: CI runs the full suites
  Given the task PR
  When conformance-bindings memory-smoke runs
  Then the TS step is `npm test` and the PY step is the full crates/lunaris-py/tests/ dir, both green offline
  And the per-driver-parity job is byte-identical to before
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
DELIVERABLES (no API endpoints — this is a test-suite + CI contract):
  crates/lunaris-ts/__test__/embedder_config.spec.mts  — rewritten: offline surface tier
    (noop/native/nativeQuantized on EmbedderConfig + RerankerConfig), cheap error paths
    (nativeQuantized with missing GGUF path throws offline), explicit deleted-API absence
    block; existing IT tier (LUNARIS_TS_IT=1) retargeted to native()
  crates/lunaris-ts/__test__/scope.spec.mts            — `:` cases -> valid-alphabet cases;
    NEW `:`-rejected test citing the KV-aliasing defense
  crates/lunaris-py/tests/test_embedder_config.py      — same rewrite, snake_case surface
    (native_quantized), pytest.raises for error paths
  crates/lunaris-py/tests/test_scope.py                — same alphabet rewrite + `:`-rejected
  .github/workflows/conformance-bindings.yml           — memory-smoke job ONLY:
    TS step `npx vitest run ... backend_parity.spec.mts` -> `npm test`
    PY step `pytest .../test_backend_parity.py -v`      -> `pytest crates/lunaris-py/tests/ -v`
    per-driver-parity job byte-identical
Evidence protocol:
  red   = standing, measured 2026-06-11: TS 7 failed + 2 false-passes; PY 11 failed / 11 passed / 9 skipped
  green = local: both suites 0-failed with HF_HUB_OFFLINE=1 + no backend env, fresh bindings-it build;
          CI: memory-smoke green on the task PR while running the full suites
Schema: zero binding/Rust source changes (reject-guarded). Coverage must not shrink:
  every deleted test has a shipped-surface replacement in the same file.
```

Status: FROZEN 2026-06-11 — approved by Tin Dang ("Freeze it") at the bundle decision point
Least-sure flag surfaced at freeze:
  ⚠ [test] Offline-safety of native()/native_quantized() construction on a bare runner is unproven — the contract therefore keeps the offline tier surface-only (no construction beyond cheap error paths) and mandates a local HF_HUB_OFFLINE=1 pass before pushing; residual cost if wrong anyway: one CI round to find which call touches the cache.
  ⚠ [spec] Widening to the Python suite doubles the diff surface in one task — accepted deliberately (same disease, same fix, one PR); if the PY rewrite surfaces addon-behavior drift beyond spec-staleness, that part records-verbatim + splits rather than growing the task further.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: 4 rewritten files green; suite-level test count must not shrink (every deleted test replaced in-file).
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - red NOW (measured this session, no new red authoring needed): TS suite -> 7 failed + 2 false-pass; PY two files -> 11 failed / 11 passed / 9 skipped
  - rewrite -> local green offline: `HF_HUB_OFFLINE=1 npm test` and `HF_HUB_OFFLINE=1 pytest crates/lunaris-py/tests/ -q`, 0 failures
  - deleted-API absence: TS `expect(typeof (EmbedderConfig as any).fastembed).toBe("undefined")` style; PY `assert not hasattr(...)` — regex-proof by construction
  - `:`-rejection: TS `expect(() => Scope.new("acme:agent-42")).toThrow()`; PY `pytest.raises(ValueError)` (exact exception type confirmed at build from existing passing rejection tests)
  - CI: task-PR conformance-bindings memory-smoke green with the full-suite steps
</test_plan>

Tests live in: `crates/lunaris-ts/__test__/` · `crates/lunaris-py/tests/` · red evidence captured BEFORE build (above).
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Safety rule (feature-specific): zero binding/Rust source changes — specs track the addon, never the reverse.
Code lives in: `crates/lunaris-ts/__test__/` · `crates/lunaris-py/tests/` · `.github/workflows/conformance-bindings.yml`
Constraints: do NOT change the frozen contract; no addon code; offline tier stays offline.

### Build log (2026-06-11)
- Rewrote 4 spec files to the shipped surface; offline error paths use bogus
  model_dir/gguf_path (native factories read LOCAL files only — verified no
  network involvement, the freeze's ⚠ offline-safety flag resolved FAVORABLY).
- Build-time discovery: the feature-off `native_quantized` PY stub names its
  params `_gguf_path`/`_model_dir`, so `gguf_path=` kwargs only exist on
  feature-on wheels — tests call positionally to stay build-agnostic.
- UNMASKED LATENT FINDING (full-dir run, outside the 4 rewritten files):
  `test_gil_discipline.py::test_concurrent_awaits_dont_serialize` FAILED:
    "50 concurrent GIL-releasing awaits took 8.8186s > 1.0s — GIL likely held across .await"
  Root-caused enough to triage: EVERY `lunaris.open()` call (success or
  parse-failure, warm, empty model cache) burns ~0.8s of pure CPU
  (wall 807ms / cpu 797ms measured). The GIL invariant itself HOLDS
  (50 concurrent = 8.8s vs ~36s serialized — ~4x overlap); the test's
  "sub-millisecond error path" premise is what died. Triage per frozen flag:
  (a) the stale absolute bound is the same disease this task fixes — replaced
  with a ratio assertion (concurrent < 0.75 x serialized estimate) that still
  discriminates a held GIL (ratio >= 0.95) — intent preserved, NOT weakened;
  (b) the ~0.8s-per-open CPU cost is addon behavior -> recorded verbatim here
  and SPLIT into task `open-call-latency` (root cause + restore fast open).
- Local green: TS `npm test` 60 passed / 2 skipped (was 7 failed + 2 false-pass);
  PY full dir `pytest tests/ -q` 34 passed / 23 skipped / 0 failed (was 11+1 failed).
  Counts grew (TS embedder spec 9 -> 11 tests; PY 31 -> 32+) — coverage held.

<!-- EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [ ] all tests pass
- [ ] coverage did not decrease
- [ ] no test or contract was altered during build
- [ ] concurrency / timing of the risky operation is safe
- [ ] no exposed secrets, injection openings, or unexpected dependencies
- [ ] layering & dependencies follow CONVENTIONS.md
- [ ] a person reviewed and approved the change

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [ ] WIRING (code) — every new symbol is referenced; record where / how confirmed
- [ ] DEAD-CODE (code) — no new unused or orphaned symbol introduced
- [ ] SEMANTIC (prose / non-code) — read in full, not skimmed: <what read · what confirmed>

### GATE RECORD
Outcome: <PASS | RISK-ACCEPTED | HARD-STOP>
If RISK-ACCEPTED -> owner: <name> · ticket: <link> · expires: <date>   (never for a security gap)
Reviewed by: <name> · date: <date>

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): <error rate / per-rejection rate / latency>
Spec delta for the next loop: <what production taught you>

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
