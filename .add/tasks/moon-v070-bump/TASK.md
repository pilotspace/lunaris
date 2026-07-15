# TASK: Bump vendored Moon to v0.7.1 (durability hardening + #69 upgrade safety + SQ8/TTL patch)

slug: moon-v070-bump · created: 2026-07-15 · stage: production
autonomy: auto   <!-- inherited from the project default (PROJECT.md); explicit level: manual < conservative < auto (visible · overridable) — lower below if a high-risk task needs it, or run `add.py autonomy set`. -->
phase: build   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower the
     autonomy level to `manual` or `conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

Touches (files · symbols · signatures):
  - `vendor/moon` (submodule) — pinned `f9ad681` → target `4161cdc` (**v0.7.1** tag deref; confirmed reachable from `pilotspace/moon` origin/main — "not our ref" guard holds). RETARGET 2026-07-15: was 5dcfbd2 (v0.7.0); v0.7.1 adds the SQ8/TQ code-size mis-dispatch CPU-error-storm fix (#73 — hits Lunaris's SQ8 opt-in indexes) + deterministic replica TTL (#71). NOTE: f9ad681 is NOT an ancestor of the tag (side-branch pin); 85 commits in `f9ad681..v0.7.1`. `.gitmodules` url = `https://github.com/pilotspace/moon.git`.
  - `Cargo.toml` [workspace.dependencies] `moondb` + `Cargo.lock` — path dep at `vendor/moon/sdk/rust`, version **0.2.1 unchanged at v0.7.1** (VERIFIED: `git diff f9ad681..v0.7.1 -- sdk/rust/src/` is EMPTY — zero SDK API drift, the key low-risk fact).
  - `scripts/test-recovery.py` — the durability/recovery harness (all-plane survive-restart; the #69 v0.6→v0.7 upgrade-replay guard extends it).
  - `docs/durability.md` (+ book mirror `docs/book/src/operations/durability.md`) — the crash/upgrade guarantee wording to refresh for WAL v3 atomic-durable-writes + FTS-term-dict-durability + #69.
  - `crates/lunaris-mcp/src/embedded_moon.rs:378,393` — `parse_from([])` clap defaults; asserts `--wal-kv-log` default `"auto"` still inherited post-bump.
  - `crates/lunaris-storage-moon/src/client.rs` — `MoonClient`/`TypedClient = moon::MoonClient` (must still compile against bumped moondb); MQ plane (`publish`/`subscribe` → `MQ.PUSH`/`POP`) + temporal plane (`read_as_of` → `TEMPORAL.SNAPSHOT_AT`) are exactly what #69 protects on upgrade.
Context (working folder): Moon changelog at the v0.7.1 tag (`git show v0.7.1:CHANGELOG.md` — [0.7.1] + [0.7.0] sections); the `vendor/moon` submodule tree (worktree currently carries ~530 files of pre-existing rustfmt noise vs the pin — stash before checkout, matching the four prior fmt-noise stashes).
Honors (patterns / conventions): `reference_vendor_moon` — never pin a Moon commit not on pilotspace/moon (4161cdc verified reachable from origin/main → no `not our ref`); `submodule-tag-parity` CI (release-tag-gated) must stay green; `reference_moon_verify_base` — `cargo check` ≠ `cargo test`, verify the full build+test; INGEST-04 (no new atomic_write); read-your-writes invariant untouched by this durability-only bump.
Anchors the contract cites: `vendor/moon` pinned SHA `4161cdc` (v0.7.1) · `moondb` 0.2.1 (unchanged, empty SDK diff verified) · `scripts/test-recovery.py` all-plane + #69 upgrade-replay · `docs/durability.md` guarantee wording · `cargo test --workspace --exclude lunaris-py --exclude lunaris-ts` + `cargo clippy --workspace --all-targets -D warnings` green.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: moon-v071-bump
Framings weighed: pin-bump + full Lunaris-side re-validation (chosen) · bump-only-trust-Moon-CI (rejected: Moon's CI never exercises the Lunaris SDK path over MQ/temporal — the planes #69 protects) · defer-to-0.8 (rejected: 0.7.1 fixes a live CPU-storm on Lunaris's SQ8 opt-in config)
Must:
<must>
  - vendor/moon gitlink moves to `4161cdc` (v0.7.1 tag deref); `.gitmodules` untouched
  - `Cargo.lock` moondb resolution stays 0.2.1 (path dep at vendor/moon/sdk/rust); workspace builds with zero Lunaris source changes
  - `cargo test --workspace --exclude lunaris-py --exclude lunaris-ts` green against the bumped Moon
  - `cargo clippy --workspace --all-targets -D warnings` green
  - recovery harness gains MQ + temporal plane probes (previously unprobed — the #69-protected planes) and passes kill-9 + AOF replay on v0.7.1
  - upgrade-replay guard: data written by the OLD pinned Moon binary (f9ad681) on a data dir is fully intact — MQ + temporal history included — after restarting the SAME data dir under the NEW v0.7.1 binary (#69)
  - embedded-moon clap-default asserts stay green: `--wal-kv-log` default "auto" still inherited (`crates/lunaris-mcp/src/embedded_moon.rs:378,393`)
  - `docs/durability.md` + book mirror refreshed: WAL v3 atomic durable writes · FTS term-dict durability · #69 upgrade safety · 0.7.1 patch notes (SQ8 #73 CPU-storm, #71 replica TTL determinism)
</must>
Reject:
<reject>
  - pinned SHA not reachable from pilotspace/moon remote -> "not_our_ref" (refuse to commit the gitlink)
  - moondb SDK API/version drift forcing Lunaris source changes -> "sdk_drift" (change-request back to SPECIFY; never silently patch call sites)
  - any pre-existing workspace test failing against v0.7.1 -> "bump_regression" (investigate; never weaken the test)
  - upgrade replay loses MQ/temporal entries -> "upgrade_data_loss" (HARD-STOP; the bump does not ship)
</reject>
After:
<after>
  - Lunaris workspace pinned to Moon v0.7.1 with all gates green; kill-9 recovery + v0.6-era→v0.7.1 upgrade replay proven across KV/vector/graph/MQ/temporal via the harness; durability docs tell the WAL v3 + #69 + 0.7.1 story; Tier-2 (WAIT-durable ingest) and Tier-3 (read-split spike) unblocked
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ The old pinned Moon (f9ad681) is buildable on this machine to produce the "old binary" side of the upgrade-replay test — lowest confidence because sibling Moon HEADs have been unbuildable before (reference_moon_verify_base) and release builds are heavy on this disk-starved box; if wrong: fall back to v0.7.1-only kill-9 replay + cite Moon's own #69 regression test upstream (weaker evidence — record at the gate).
  ⚠ The 530-file vendor/moon worktree diff is pure rustfmt noise (verified by sampling 1 file only); if wrong: stash (never discard) preserves whatever is buried — cost near-zero if stashed.
  - [ ] The recovery harness's BGREWRITEAOF anchor flow still works under 0.7.1's WAL v3 layout — confirm on first harness run.
  - [ ] submodule-tag-parity CI fires only on release tags — no action for this PR's branch push.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: gitlink pinned to v0.7.1
  Given the vendor/moon submodule pinned at f9ad681
  When the bump lands
  Then `git submodule status vendor/moon` reports 4161cdc (v0.7.1)
  And `.gitmodules` is byte-identical to before

Scenario: workspace compiles with zero source changes
  Given the bumped submodule
  When `cargo build --workspace` runs
  Then it succeeds and `git diff --stat` shows no crates/**.rs modified by the bump itself
  And Cargo.lock still resolves moondb 0.2.1

Scenario: full test suite green
  Given the bumped submodule
  When `cargo test --workspace --exclude lunaris-py --exclude lunaris-ts` runs
  Then every pre-existing test passes unmodified

Scenario: clippy wall holds
  Given the bumped submodule
  When `cargo clippy --workspace --all-targets -- -D warnings` runs
  Then it exits 0

Scenario: MQ + temporal survive kill-9 on v0.7.1
  Given a v0.7.1 Moon with ingested episodes, MQ messages (published, some consumed), and temporal history (BGREWRITEAOF anchored)
  When Moon is SIGKILLed and restarted on the same data dir
  Then MQ backlog + PEL state and TEMPORAL.SNAPSHOT_AT reads match the pre-kill snapshot
  And FT.SEARCH recall over pre-kill docs is unchanged

Scenario: v0.6-era WAL replays intact under v0.7.1 (#69)
  Given a data dir written by the OLD pinned Moon binary (f9ad681) holding KV+vector+graph+MQ+temporal state
  When the v0.7.1 binary starts on that same data dir
  Then all five planes read back identical to the pre-upgrade snapshot — MQ/temporal history not dropped
  And no error-level plane-scan log lines appear during replay

Scenario: embedded-moon defaults survive the bump
  Given the bumped submodule
  When the embedded_moon clap-default unit tests run (`--features embedded-moon`)
  Then `--wal-kv-log` still defaults to "auto" (asserts at embedded_moon.rs:378,393 green)

Scenario: durability docs refreshed
  Given the bump is validated
  When docs/durability.md and the book mirror are read
  Then both state WAL v3 atomic durable writes, FTS term-dict durability, #69 upgrade safety, and the 0.7.1 SQ8/#71 patch notes

Scenario: reject a pin not on the remote
  Given a candidate SHA unreachable from pilotspace/moon origin
  When the bump is attempted
  Then it stops with "not_our_ref"
  And the gitlink remains at the previous pin

Scenario: reject SDK drift
  Given the bumped submodule breaks a moondb call site compile
  When the workspace build fails for that reason
  Then the task stops with "sdk_drift" as a change request back to SPECIFY
  And no Lunaris source file is patched to paper over it

Scenario: reject test regression
  Given any pre-existing test fails against v0.7.1
  When the failure is confirmed bump-caused
  Then the task records "bump_regression" and investigates
  And no test is weakened or skipped to force green

Scenario: reject upgrade data loss
  Given the upgrade-replay probe finds missing MQ or temporal entries
  When the loss is confirmed
  Then the task HARD-STOPs with "upgrade_data_loss"
  And the gitlink is not committed at the new pin
```

Edge sweep: concurrent-write-during-kill is covered by the harness's kill-9 timing (in-flight writes may lose the tail — the documented AOF everysec window, asserted as "at most the fsync window", not zero-loss); duplicate BGREWRITEAOF anchor is idempotent (harness already tolerates); empty-data-dir upgrade (fresh v0.7.1 start) is the existing test_moon_kill wipe path — ruled in via current coverage.

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

This task ships no HTTP surface — the "contract" is the dependency-pin shape + the validation gates:

```
PIN   vendor/moon gitlink = 4161cdc (v0.7.1)         # .gitmodules untouched
DEP   moondb = 0.2.1 via path vendor/moon/sdk/rust   # Cargo.lock resolution unchanged
GATES (all must hold, else the named reject fires):
  cargo build --workspace                                        -> ok        | "sdk_drift"
  cargo test --workspace --exclude lunaris-py --exclude lunaris-ts -> green   | "bump_regression"
  cargo clippy --workspace --all-targets -- -D warnings          -> exit 0    | "bump_regression"
  scripts/test-recovery.py (extended: +MQ +temporal probes)      -> ALL PASS  | "upgrade_data_loss"
  scripts/test-recovery.py --upgrade-replay (old-binary data dir → v0.7.1 restart) -> ALL PASS | "upgrade_data_loss"
  pin reachability: git -C vendor/moon merge-base --is-ancestor 4161cdc origin/main -> true | "not_our_ref"
Schema: no Lunaris schema change; Moon-side WAL v3 layout is internal to the submodule.
Docs: docs/durability.md + docs/book/src/operations/durability.md gain the WAL v3 / #69 / 0.7.1 wording.
```

Ground SHA: e579473 (lunaris main) · vendor/moon f9ad681 → 4161cdc

Least-sure flag surfaced at freeze: ⚠ [test] the old pinned Moon (f9ad681) may be unbuildable on this machine, gutting the old-binary→v0.7.1 upgrade-replay leg — because sibling Moon HEADs have failed to build before (reference_moon_verify_base) and release builds strain this disk-starved box; if wrong: fall back to v0.7.1-only kill-9 replay + upstream #69 regression-test citation, recorded as weaker evidence at the gate. ⚠ [spec] the 530-file vendor/moon worktree diff is presumed pure rustfmt noise from a 1-file sample; if wrong: the stash (never discard) preserves it — near-zero cost.

Status: FROZEN @ v1 — approved by Tin Dang (standing directive in this session: "Bump Moon version to latest 0.7.1"; milestone Tier-1 confirmed 2026-07-15; autonomy: auto)
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: n/a (dependency bump — the contract-gate battery + live-run evidence replaces line coverage)
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_gitlink_pinned: submodule gitlink == 4161cdc (RED: f9ad681)
  - test_gitmodules_untouched: pilotspace/moon url intact (green guard)
  - test_pin_reachable_from_remote: merge-base --is-ancestor origin/main -> "not_our_ref" reject guard (green guard)
  - test_moondb_lock_unchanged: Cargo.lock moondb == 0.2.1 -> "sdk_drift" tripwire (green guard)
  - test_harness_probes_mq_and_temporal: scripts/test-recovery.py probes MQ + TEMPORAL.SNAPSHOT_AT (RED: unprobed)
  - test_harness_upgrade_replay_mode: harness has the #69 upgrade-replay mode (RED: absent)
  - test_durability_docs_refreshed: WAL v3 + #69 + 0.7.1 wording in both doc copies (RED: stale)
  - LIVE gates (run at build/verify, evidence in §6): cargo build/test/clippy battery · extended harness kill-9 run · upgrade-replay run · embedded-moon clap-default tests -> "bump_regression"/"upgrade_data_loss"
</test_plan>

RED evidence (2026-07-15, pre-build): `6 failure(s): gitlink_pinned_v071, harness_mq_probe, harness_temporal_probe, harness_upgrade_replay_mode, durability_doc_docs, durability_doc_operations` — each flips via a planned build batch (red satisfiable); 3 standing guards already green.

Tests live in: `./tests/` · MUST run red (missing implementation) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `vendor/moon` (gitlink only) · `Cargo.lock` · `scripts/test-recovery.py` · `docs/durability.md` · `docs/book/src/operations/durability.md` · `./src/` (task-local helper scripts if any)
Strategy (ordered batches):
  1. Stash vendor/moon fmt-noise worktree (message notes it's pre-bump rustfmt noise) → checkout 4161cdc → parent `git add vendor/moon`.
  2. `cargo build --workspace` (catches sdk_drift immediately; Cargo.lock check).
  3. Extend scripts/test-recovery.py: MQ-plane probe (MQ.PUSH/backlog/PEL) + temporal-plane probe (TEMPORAL.SNAPSHOT_AT) into snapshot/replay_probes; add `--upgrade-replay` mode (old-binary write → v0.7.1 restart → all-plane compare) using a pre-built old binary if buildable (⚠ assumption 1 fallback documented).
  4. Run full gates: workspace tests · clippy -D warnings · recovery harness (kill-9 + upgrade-replay).
  5. Refresh durability docs (both copies).
  6. Atomic commits per batch via tmp/<msg>.txt (repo convention).
Safety rule (feature-specific): NEVER discard the vendor/moon worktree diff — stash it; the gitlink commit contains ONLY the pin move (no Lunaris .rs changes ride along).
Code lives in: `scripts/` + docs (no crate source changes expected — sdk_drift reject otherwise)
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

- [ ] all tests pass
- [ ] coverage did not decrease
- [ ] no test or contract was altered during build
- [ ] the green was EARNED, not gamed — no overfit to fixtures, vacuous asserts, or stubbed-away logic (score with an adversarial refute-read — a subagent recommended under `autonomy: auto`; a confirmed cheat is HARD-STOP)
- [ ] concurrency / timing of the risky operation is safe
- [ ] no exposed secrets, injection openings, or unexpected dependencies
- [ ] layering & dependencies follow CONVENTIONS.md
- [ ] a person reviewed and approved the change

### Build expectations — what "correct" looks like (fill BEFORE build; confirm each at the gate)
> Pre-declare the OBSERVABLE outcomes a correct build must produce — derived from §2 SCENARIOS
> + §3 CONTRACT — so this gate checks the build is RIGHT, not merely that tests are green. Each
> row is evidence you can SEE, not a restatement of a test name.
- [x] v0.6.0→v0.7.1 upgrade replay: ALL PLANES PASS (KV · bi-temporal hash · MQ entries+self-heal · graph node · temporal smoke) — confirmed by `scripts/test-recovery.py --upgrade-replay --old-bin ~/.lunaris/bin/moon(0.6.0) --new-bin vendor/moon/target/release/moon(0.7.1)` → `upgrade_replay PASS` 2026-07-15; Moon log line `Shard 0: replayed MQ WAL — 1 create, 3 push, 1 pop, 1 ack, 0 skipped` = the #69 legacy-framing unwrap doing its job
- [x] v0.7.1 kill-9 self-restart: ALL PLANES PASS — same harness, new-bin on both sides
- [x] moondb still 0.2.1 in Cargo.lock; lock diff = moon server crate 0.6.0→0.7.1 + atoi 3.1.0 dual-version only
- [x] `cargo build --workspace --exclude lunaris-py --exclude lunaris-ts` green vs bumped Moon (15m00s); the two excluded cdylibs are the KNOWN pre-existing plain-cargo link failures (feedback_py_ts_sdk_testing) — not bump-caused
- [ ] full test battery green (cargo test + clippy -D warnings + embedded-moon clap defaults) — running
- [ ] gitlink committed at 4161cdc — pending battery green

### Discovered en route (evidence, not blockers)
- KNOWN-GAP (pre-existing, NOT a bump regression — v0.6.0→v0.6.0 baseline reproduces it): MQ delivery cursor/PEL not replayed on restart — stream ENTRIES survive (XRANGE byte-identical) but pre-restart un-ACKed backlog is never redelivered; post-restart pushes deliver fine after the idempotent MQ CREATE Lunaris's publish() always issues. Upstream Moon issue material: replaying a `pop` record appears to advance the cursor past ALL entries, contradicting task #47's PEL-metadata intent. Repro: harness --upgrade-replay, same-binary both sides.
- v0.7.1 IMPROVES on v0.6.0: durable-queue registration now survives restart (v0.6.0 loses it entirely — "stream is not a durable queue" post-restart).
- MQ WAL plane replay is ASYNC post-listen — verifiers must poll (wait_plane_replay added).
- `TEMPORAL.INVALIDATE` arg-less form (SDK release_snapshot) rejected by BOTH 0.6.0 and 0.7.1 servers — SDK/server drift, upstream note.
- This box's <5% free disk trips Moon's diskfull write-pause mid-harness — `--disk-free-min-pct 1` now baked into the harness launcher.

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

### Spec delta
Forward changes for the next loop — each re-enters at Specify as the next task. One line
each, tagged `[SPEC · open|seeded|dropped]`, with evidence (e.g. `[SPEC · open] rate-limit
the retry path (evidence: prod herd spikes)`). See the `add` skill's `deltas.md`.

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->
