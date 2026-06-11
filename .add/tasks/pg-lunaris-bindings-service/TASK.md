# TASK: pg-lunaris image for conformance-bindings postgres row

slug: pg-lunaris-bindings-service · created: 2026-06-11 · stage: production
phase: contract   <!-- specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower
     the autonomy level with `autonomy: conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: Give conformance-bindings' postgres matrix row a Postgres that Lunaris can actually migrate against. SPLIT from ci-bindings-venv-fix (frozen triage rule): the venv fix unmasked `extension "vector" is not available` in migration 20260420000001 — the job's service container is plain `postgres:16`, which can never pass.

Ground facts (2026-06-11):
  - scripts/pg-lunaris (Dockerfile + init-extensions.sql): postgres:16 + postgresql-16-pgvector + Apache AGE PG16/v1.5.0-rc0 + pgmq v1.4.4; initdb hook creates vector/age/pgmq extensions in POSTGRES_DB. User/db/password come from runtime env — NOT hardcoded.
  - The blessed CI pattern is integration.yml (Plan 14-01 D-02): buildx + docker/build-push-action (cache type=gha, scope=pg-lunaris) + manual `docker run` + /dev/tcp wait loop — GHA `services:` blocks CANNOT consume a locally-built image, so the service block must be REPLACED by steps, not re-imaged.
  - GHA cache scope=pg-lunaris is already warmed by integration.yml runs — the image build is cheap in CI.
  - conformance-bindings URLs are `postgres://postgres:lunaris@localhost:5432/lunaris` — keep them byte-identical by running the container with POSTGRES_USER=postgres / POSTGRES_PASSWORD=lunaris / POSTGRES_DB=lunaris (integration.yml uses lunaris as user; the image does not care).
  - KNOWN RISK NEXT LAYER: integration.yml runs this SAME image and run_storage_postgres still fails `unhandled cypher(cstring)` (AGE call class). run_bindings_backend_parity is a smaller fixture-driven test with ZERO run history — it may or may not touch that path. Python/TS parity steps on this row also have zero history.
  - memory-smoke job has no postgres at all — untouched by this task.

Framings weighed: copy the integration.yml build+run pattern into per-driver-parity (chosen — in-repo blessed precedent, shared GHA cache, URLs unchanged) · publish pg-lunaris to GHCR and keep services: (rejected — new registry surface + auth for a CI-only need; revisit if a third workflow needs it) · pgvector/pgvector:pg16 public image (rejected — lacks AGE + pgmq; would pass migration 20260420000001 then die at the next extension).
Scope boundary: .github/workflows/conformance-bindings.yml ONLY. Same triage-or-split rule as the parent task for the next unmasked layer (cypher-class or parity-fixture drift): trivial fix in-file or verbatim record + split. No crates/ changes.
Must:
<must>
  - per-driver-parity loses its `services: postgres` block and gains the integration.yml pattern: buildx setup -> build-push-action (context scripts/pg-lunaris, tag pg-lunaris:ci, load: true, cache-from/to type=gha scope=pg-lunaris) -> docker run -d with POSTGRES_USER=postgres POSTGRES_PASSWORD=lunaris POSTGRES_DB=lunaris -p 5432:5432 -> wait-for-port loop (30x2s /dev/tcp)
  - backend URLs and all parity steps remain byte-identical
  - green = per-driver parity (postgres) green on this task's PR run, OR the next unmasked failure recorded verbatim in §6 + split/task proposal
</must>
Reject:
<reject>
  - weakening or skipping any parity step to force green -> never (inherited reject)
  - swapping to an image that satisfies only pgvector (e.g. pgvector/pgvector) -> rejected: defers the AGE/pgmq failure one migration further
</reject>
After:
<after>
  - the postgres row exercises the same extension stack production targets (pgvector + AGE + pgmq)
  - conformance-bindings is structurally able to go fully green for the first time
  - integration.yml and conformance-bindings share one pg-lunaris build cache (no duplicate cost)
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ run_bindings_backend_parity (+ Python/TS parity) may hit the same `unhandled cypher(cstring)` class that fails run_storage_postgres on this image — lowest confidence because these tests have ZERO run history; if wrong-way, the row stays red for the cypher reason -> verbatim record + split (likely converges with the deferred live-PG parity HUMAN-UAT decision).
  - [x] The image build is fast enough for this job — integration.yml warms the shared scope=pg-lunaris GHA cache.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: postgres row migrates and runs all three parity steps
  Given per-driver-parity builds + runs pg-lunaris:ci with POSTGRES_USER=postgres
  When the workflow runs on this task's PR
  Then migration 20260420000001 succeeds (vector extension present) and the Rust, Python,
       and TypeScript parity steps execute
  And each passes OR its failure is recorded verbatim in §6 with a split proposal

Scenario: moon row and memory-smoke unaffected
  Given the same run
  When per-driver parity (moon) and feature-build smoke execute
  Then both stay green exactly as after ci-bindings-venv-fix
  And the moon row still neutral-skips on missing MOON_IMAGE

Scenario: no weakened step
  Given the diff of conformance-bindings.yml
  When reviewed
  Then only the services block is replaced by the build+run+wait steps; URLs and parity
       commands are byte-identical; no continue-on-error introduced
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
DELIVERABLE (one file): .github/workflows/conformance-bindings.yml, per-driver-parity job only
  REMOVE: the whole `services:` block (plain postgres:16)
  ADD (after "Init vendor/moon submodule", before setup-python), copied from
  integration.yml Plan 14-01 D-02 pattern:
    - docker/setup-buildx-action@v3
    - docker/build-push-action@v5  (context: scripts/pg-lunaris, tags: pg-lunaris:ci,
      load: true, cache-from/to: type=gha,scope=pg-lunaris)
    - docker run -d --name pg-lunaris -p 5432:5432 \
        -e POSTGRES_PASSWORD=lunaris -e POSTGRES_DB=lunaris -e POSTGRES_USER=postgres \
        pg-lunaris:ci
    - wait-for-port loop (30 attempts x 2s, bash /dev/tcp)
  UNCHANGED: memory-smoke job, all URLs, all parity/test steps, triggers, matrix,
  moon-row skip gate, the venv steps from ci-bindings-venv-fix.
Evidence protocol:
  red  = standing: per-driver parity (postgres) failed run 27346464601 with
         `extension "vector" is not available` (could never pass on postgres:16)
  green= this task's PR run: postgres row green, OR next unmasked layer recorded
         verbatim in §6 + split proposal (cypher-class risk pre-flagged)
Schema: CI-config only; no crates/ changes.
```

Status: SUPERSEDED — never frozen. At the freeze decision (2026-06-11) Tin Dang redirected: "remove pg support, now lunaris just only support Moon to maximize performance and features rich". Clarified scope: Moon-only literally (Postgres AND SQLite deprecate-first, delete next minor), executed as the upcoming moon-only milestone. The conformance-bindings postgres row will be REMOVED there, not repaired here. Bundle retained for the record.
Least-sure flag surfaced at freeze:
  ⚠ [spec] The parity tests may hit the `unhandled cypher(cstring)` AGE-call class that fails run_storage_postgres on this same image — zero run history either way; contracted response = verbatim record + split (likely folding into the deferred live-PG HUMAN-UAT track), never weaken.
  ⚠ [contract] Running the container with POSTGRES_USER=postgres (to keep URLs byte-identical) diverges from integration.yml's lunaris user — if any init script implicitly assumes the lunaris role, the row fails at initdb; cheap to flip, but it would change the URLs too.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: CI-config task — the workflow run IS the test (same protocol as ci-bindings-venv-fix). Red is standing: run 27346464601 postgres row failed `extension "vector" is not available`; plain postgres:16 can NEVER pass migration 20260420000001.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - scenario 1: this task's PR CI run — postgres row passes migration + executes Rust/Python/TS parity steps (each green or verbatim-recorded + split)
  - scenario 2: same run — moon row + memory-smoke remain green (regression guard on the venv fix)
  - scenario 3: git diff review — services block swapped for build+run+wait, nothing else changed
  - local pre-flight (cheap): docker build scripts/pg-lunaris + docker run with POSTGRES_USER=postgres + psql 'CREATE EXTENSION IF NOT EXISTS vector' smoke — validates the user-env flag against the init script before burning a CI round
</test_plan>

Tests live in: `.github/workflows/conformance-bindings.yml` (the run is the test) · red = standing postgres-row failure, recorded in parent task §6.
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
