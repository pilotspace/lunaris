# TASK: Bump vendored Moon v0.7.1 → v0.8.0(+dashtable fix): One Storage Kernel GA — kill-9-lossless planes + GraphUnion merge backoff

slug: moon-v080-bump · created: 2026-07-16 · stage: production
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
- `vendor/moon` (submodule gitlink) — current pin `4161cdc` (tag v0.7.1, PR #60, main `252dae8`). Bump target: **the merge commit of Moon PR #351 on origin/main** (branch `fix/dashtable-double-split` head `cbf31a55` = v0.8.0 `44f49778` + dashtable fix + CHANGELOG, CI in flight). v0.8.0 alone does NOT contain the dashtable double-NeedsSplit fix (grep confirms the `unreachable!` at `src/storage/dashtable/mod.rs`) — a tag-pin would re-ship the recovery-panic landmine the live daemon is patched against.
- `Cargo.lock` — moon server crate version bumps 0.7.1 → 0.8.0 (embedded-moon feature only); moondb SDK stays **0.2.1, byte-identical** (`git diff v0.7.1..v0.8.0 -- sdk/rust/src` is EMPTY) → API-stable bump, zero Lunaris source changes expected.
- `docs/durability.md` §2.7/§2.2b + `docs/book/src/operations/durability.md` §3.4 — extend the WAL v3 story with v0.8 One-Storage-Kernel GA (kill-9-lossless every plane, upstream crash-matrix CI #352, GraphUnion merge backoff #353).
- `.add/tasks/moon-v080-bump/tests/test_bump_contract.py` — retargeted copy of moon-v070-bump's 9-gate battery (TARGET_SHA → the #351 merge SHA).

Context (working folder): v0.7.1→v0.8.0 = 11 commits. Lunaris-relevant: `4dcfd533` #353 GraphUnion merge backoff (fixes the abort-merge CPU livelock LIVE on 6381 since 2026-07-14 — OPTIMIZATION-OPPORTUNITIES item 9 upstream ask DELIVERED); `0c576331` #349 truthful used_memory under disk-offload + `8f5e0c90` #350 batched spill files (host runs >90%-used disk); `777a4a53` #352 crash-matrix CI; `c98d230e` TLS pemfile→pki-types; rest docs/ci/lint. Live daemon: 6381 = PATCHED v0.7.1 binary via launchd, plist has `--max-unflushed-immutable-segments 4096`, 222k keys / 352 FT idx; binary backups `moon-0.7.1-stock.bak` + `moon-0.6.0.bak`; poisoned-checkpoint repro at `~/.lunaris/quarantine-dashtable-panic-20260716/`.

Honors (patterns / conventions):
- "not our ref" guard — pin only SHAs reachable from pilotspace/moon origin/main (merge #351 FIRST, then pin).
- Build/test gates scoped `--exclude lunaris-py --exclude lunaris-ts` (cdylibs never link under plain cargo).
- INGEST-04 single atomic_write · read-your-writes synchronous FT indexing · embedded-moon stays out of default features.
- Recovery harness: `scripts/test-recovery.py` TESTs 1-3 + `--upgrade-replay --old-bin ~/.lunaris/bin/moon-0.7.1-stock.bak --new-bin <v0.8+fix build>`; harness pins `--disk-free-min-pct 1`; `LUNARIS_TEST_MOON_BIN` overrides TESTs 1-3.

Anchors the contract cites: `vendor/moon` gitlink SHA · `Cargo.lock` moon 0.8.0 resolution · `scripts/test-recovery.py::test_upgrade_replay` · `test_bump_contract.py::TARGET_SHA` · `docs/durability.md` §2.7 · `crates/lunaris-mcp` embedded-moon `--wal-kv-log auto` default

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: moon-v080-bump — pin vendor/moon at v0.8.0+dashtable-fix (the Moon PR #351 merge commit), validated across all planes
Framings weighed: pin-the-#351-merge-commit (chosen — carries both v0.8.0 AND the recovery-panic fix; reachable from origin/main) · pin-tag-v0.8.0 (rejected — re-ships the double-NeedsSplit landmine; live daemon would regress on next checkpoint boot) · wait-for-a-v0.8.1-release (rejected — no release scheduled; blocks the #353 livelock cure the live daemon needs now)
Must:
<must>
  - vendor/moon gitlink pins the #351 merge commit on pilotspace/moon origin/main (v0.8.0 + dashtable fix), and the SHA is reachable from origin/main (CI-cloneable)
  - Cargo.lock resolves the moon server crate at 0.8.0 with moondb SDK unchanged at 0.2.1 (path dep)
  - full workspace build + test battery green: cargo test --workspace --exclude lunaris-py --exclude lunaris-ts, clippy --workspace --all-targets -D warnings, cargo test -p lunaris-mcp --features embedded-moon (clap --wal-kv-log "auto" default holds)
  - recovery harness TESTs 1-3 PASS against the v0.8 binary (SDK ingest + kill-9 + KV/bi-temporal/MQ/graph/temporal plane probes)
  - upgrade-replay leg PASS: v0.7.1-stock binary writes → v0.8+fix binary replays, all five planes intact
  - dashtable regression proof: the v0.8+fix binary loads the quarantined poisoned checkpoint (~/.lunaris/quarantine-dashtable-panic-20260716/) that crash-looped stock v0.7.1
  - docs/durability.md §2.7 + book mirror §3.4 extended with the v0.8 One-Storage-Kernel story (kill-9-lossless GA, crash-matrix CI upstream, GraphUnion merge backoff)
</must>
Reject:
<reject>
  - pin SHA not reachable from pilotspace/moon origin/main -> "not_our_ref"
  - moondb SDK API drift breaking lunaris-storage-moon compile -> "sdk_drift"
  - any workspace test/clippy regression attributable to the bump -> "bump_regression"
  - any plane losing durable records across the v0.7.1→v0.8 upgrade replay -> "upgrade_data_loss"
  - the v0.8+fix binary panicking on the quarantined poisoned checkpoint -> "dashtable_regression"
</reject>
After:
<after>
  - `git submodule status vendor/moon` shows the #351 merge SHA; CI clones it cleanly
  - the live 6381 flip to the v0.8+fix binary is UNBLOCKED (separate human-gated step; plist backpressure override can likely be retired after #353 drains the merge backlog — verify before removing)
  - OPTIMIZATION-OPPORTUNITIES item 9 (abort-merge livelock) marked upstream-DELIVERED by #353
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ Moon PR #351 CI goes green and merges cleanly after the v0.8.0 rebase — lowest confidence because the previous CI run died on the CHANGELOG gate and the re-trigger needed a force-push; if wrong: the pin target doesn't exist and the task blocks at contract (cost: wait/fix upstream CI, no Lunaris damage)
  - [x] moondb SDK byte-identical v0.7.1→v0.8.0 — CONFIRMED (empty sdk/rust/src diff, version 0.2.1)
  - [x] v0.8.0 lacks the dashtable fix — CONFIRMED (unreachable! still present at the tag)
  - [ ] #350 batched-spill + #349 memory-ledger changes do not alter WAL/AOF on-disk formats consumed by recovery — validated implicitly by the upgrade-replay leg going green
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: pin lands on the fix-carrying merge commit
  Given Moon PR #351 is merged into pilotspace/moon main on top of v0.8.0
  When vendor/moon is checked out at the merge SHA and the gitlink committed
  Then git submodule status shows that SHA and git merge-base --is-ancestor <SHA> origin/main succeeds
  And moondb stays 0.2.1 in Cargo.lock (no Lunaris source change)

Scenario: workspace battery green on v0.8
  Given the new pin
  When cargo test --workspace --exclude lunaris-py --exclude lunaris-ts, clippy --all-targets -D warnings, and cargo test -p lunaris-mcp --features embedded-moon run
  Then all pass with 0 failures
  And the embedded-moon --wal-kv-log "auto" default assertion still holds

Scenario: all-plane recovery on the v0.8 binary
  Given a v0.8+fix release build
  When scripts/test-recovery.py TESTs 1-3 run with LUNARIS_TEST_MOON_BIN pointing at it
  Then KV, bi-temporal, MQ, graph, and temporal probes verify after kill-9
  And the known MQ delivery-cursor gap stays a WARN (not a new regression)

Scenario: v0.7.1 → v0.8 upgrade replay is lossless
  Given data written by the stock v0.7.1 binary (moon-0.7.1-stock.bak)
  When the v0.8+fix binary replays that state (--upgrade-replay leg)
  Then all five plane probes verify byte-intact
  And a failure here is "upgrade_data_loss" and blocks the bump

Scenario: poisoned checkpoint loads (dashtable fix carried)
  Given the quarantined production checkpoint that crash-looped stock v0.7.1 119 times
  When the v0.8+fix binary boots a copy of it
  Then recovery completes with 219,661 keys + 352 FT indexes and writes accepted
  And a panic here is "dashtable_regression" and blocks the bump

Scenario: unreachable pin is rejected
  Given a candidate SHA not pushed to pilotspace/moon origin/main
  When the pin-reachability gate checks it
  Then the bump is rejected as "not_our_ref"
  And the previous pin (4161cdc) remains in place
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
BUMP vendor/moon   pin: { sha: "e41aa6716fdaed81087f5ecd9623c13c0ec4ee83" }   # Moon main = v0.8.0 (44f49778) + PR #351 dashtable fix (00823f16) + changelog (e41aa671)
  PASS -> { gitlink: e41aa671, cargo_lock: { moon: "0.8.0", moondb: "0.2.1" }, lunaris_source_changes: 0 }
  FAIL -> { error: "not_our_ref" | "sdk_drift" | "bump_regression" | "upgrade_data_loss" | "dashtable_regression" }

Gates (all must PASS before the gitlink commit):
  G1 pin-reachability   git merge-base --is-ancestor e41aa671 origin/main  (else not_our_ref)
  G2 sdk-parity         diff v0.7.1..e41aa671 -- sdk/rust/src empty AND moondb 0.2.1  (else sdk_drift)
  G3 build+test         cargo test --workspace --exclude lunaris-py --exclude lunaris-ts → 0 failed  (else bump_regression)
  G4 clippy             cargo clippy --workspace --all-targets -- -D warnings clean  (else bump_regression)
  G5 embedded-moon      cargo test -p lunaris-mcp --features embedded-moon green incl. --wal-kv-log "auto"  (else bump_regression)
  G6 recovery           scripts/test-recovery.py TESTs 1-3 vs the e41aa671 release build  (else bump_regression)
  G7 upgrade-replay     --upgrade-replay --old-bin moon-0.7.1-stock.bak --new-bin <e41aa671 build> all planes  (else upgrade_data_loss)
  G8 poisoned-ckpt      e41aa671 build boots a copy of ~/.lunaris/quarantine-dashtable-panic-20260716/ → 219,661 keys + 352 idx + write OK  (else dashtable_regression)
  G9 docs               durability.md §2.7 + book §3.4 name v0.8 kill-9-lossless GA + #353 backoff  (else bump_regression)

Schema: vendor/moon gitlink · Cargo.lock [[package]] moon 0.8.0 / moondb 0.2.1 · docs/durability.md · docs/book/src/operations/durability.md · ./tests/test_bump_contract.py
```

Status: FROZEN @ v1 — approved by Tin (autonomy: auto)
Least-sure flag surfaced at freeze: [contract] G8's poisoned-checkpoint boot needs a ~1.5GB copy on the 68%-used home volume — if the copy fails on space the gate cannot run; cost: free space + rerun, no ambiguity in the gate itself. [spec] The former ⚠ (#351 CI/merge risk) RESOLVED before freeze — merged as 00823f16/e41aa671, CI green in 2m41s.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: all 9 contract gates asserted (G1–G9), each red before the bump lands
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - check pin_sha: gitlink == e41aa671… (red while 4161cdc) [G1 + scenario pin-lands]
  - check pin_reachable: merge-base --is-ancestor vs origin/main [G1 + scenario unreachable-pin]
  - check sdk_parity: empty sdk/rust/src diff + moondb 0.2.1 in Cargo.lock [G2]
  - check cargo_lock_moon: Cargo.lock resolves moon 0.8.0 (red while 0.7.1) [G3 precondition]
  - check harness_upgrade_leg: test-recovery.py exposes --upgrade-replay/--old-bin/--new-bin + space-form "MQ", "PUSH" probes [G6/G7 harness intact]
  - check docs_v08: durability.md §2.7 mentions v0.8/kill-9-lossless/#353 backoff (red until docs edit) [G9]
  - check book_mirror: book §3.4 mirrors it (red until docs edit) [G9]
  - check dashtable_fix_in_tree: vendor/moon/src/storage/dashtable/mod.rs has the split-retry loop, NOT the bare unreachable single-retry (red while pinned at v0.7.1 which predates the fix… v0.7.1 HAS the unreachable → red) [G8 static half]
  - check embedded_moon_not_default: lunaris-mcp Cargo.toml default = [] excludes embedded-moon [G5 guard]
</test_plan>

Tests live in: `./tests/` · MUST run red (missing implementation) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `vendor/moon` `Cargo.lock` `docs/durability.md` `docs/book/src/operations/durability.md` `tmp/`
Strategy (ordered batches): 1. checkout e41aa671 in vendor/moon (G1/G2 static gates go green) 2. cargo build release moon binary from the pin 3. run G3–G5 battery 4. run G6 recovery + G7 upgrade-replay + G8 poisoned-checkpoint against the new binary 5. docs G9 6. gitlink + Cargo.lock commit via git commit -F tmp/<msg>.txt
Safety rule (feature-specific): NEVER format or otherwise dirty the vendor/moon checkout (rustfmt-noise trap); the gitlink commit contains ONLY the SHA move + Cargo.lock; live-daemon flip is OUT of build scope (human-gated After-state)
Code lives in: `vendor/moon` (gitlink) — zero Lunaris .rs changes expected (API-stable bump)
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
- [ ] <observable outcome a correct build must produce> — confirmed by <how / where>
- [ ] <another observable outcome> — confirmed by <evidence seen>

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
