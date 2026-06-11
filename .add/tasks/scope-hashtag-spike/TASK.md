# TASK: Design spike: {scope} hash-tag keyspace for multi-shard TXN

slug: scope-hashtag-spike · created: 2026-06-11 · stage: production
phase: done   <!-- specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower
     the autonomy level with `autonomy: conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: Design spike (doc only) — can Lunaris keep INGEST-04 atomicity on a MULTI-SHARD Moon, and is a `{scope}` hash-tag key format the right path? Deliverable (RESHAPED at freeze per Tin Dang's "Recommend Moon work now"): docs/design/scope-hashtag-txn-rfc.md — an RFC-grade Moon TXN-pinning design ready to schedule as Moon work — plus the live probe script that grounds it. NO production code changes.

Ground facts (vendor/moon src/transaction/mod.rs + command/transaction.rs + live probe @6395 --shards 4, 2026-06-11):
  - Moon TXN is SHARD-LOCAL: per-connection undo log; a write routing to another shard inside TXN is REJECTED with ERR_TXN_CROSS_SHARD ("use hash tags {tag}...") — fails loudly, never corrupts.
  - slot_for_key uses extract_hash_tag(key) (first {...} substring) else the full key. Lunaris keys carry NO braces today, so every multi-key atomic_write sprays across shards.
  - LIVE-PROBED: on --shards 4, an unbraced 8-key TXN got 1 OK + 7 rejections (the 1 OK = key that happened to hash to the connection's shard). CRITICAL EXTRA FINDING: a fully {acme.a1}-braced TXN got 8/8 REJECTED — hash tags co-locate keys with EACH OTHER but the TXN runs on whatever shard ACCEPTED the connection (SO_REUSEPORT), and a client cannot pick its shard. Hash-tagging alone is INSUFFICIENT on Moon v0.3.0; it needs a Moon-side TXN-pinning/forwarding primitive.
  - Today every Lunaris deployment recipe runs --shards 1, so the gap is LATENT (and Moon's own perf guidance prefers --shards 1 for non-pipelined workloads).
  - Scope alphabet [A-Za-z0-9_\-.]{1,128} excludes braces — a braced key format is collision-free and reversible.

Framings weighed: design doc + probe now, implementation deferred to a Moon-side primitive (chosen — the probe proves client-only hash-tagging cannot work on v0.3.0) · implement braced keyspace now (rejected — buys nothing until Moon can pin TXN shards; pure migration pain) · connect-time shard-count guard as immediate hardening (recommended IN the doc as the one cheap v0.7 action, but implementation is a follow-up task, not this spike).
Scope boundary: docs/design/ + scripts/ only. Zero changes under crates/.
Must:
<must>
  - scripts/spike-scope-hashtag-probe.py: reproducible probe (TXN BEGIN + unbraced vs braced SETs vs --shards 4) printing a verdict table
  - docs/design/scope-hashtag-txn-rfc.md: RFC-grade Moon TXN-pinning design — problem statement w/ exact code pointers (transaction/mod.rs per-connection CrossStoreTxn; handler_sharded/mod.rs:1630 cross-shard write guard; txn.rs commit path's four ctx.shard_id-bound structures: txn_manager, WAL XactCommit, kv_intents, hnsw_queue; slots.rs extract_hash_tag), probe evidence verbatim, the connection-shard-binding finding, THREE MOON-SIDE MECHANISMS analyzed with implementation cost (M1 `TXN.BEGIN PIN <key>` + shard-side txn-state table + SPSC write forwarding · M2 connection shard-pin/migration handshake · M3 full server-side TXN forwarding with txn state moved out of ConnectionState), a recommended mechanism + proposed command surface + undo-log/WAL implications, Lunaris `{scope}`-braced keyspace adoption + migration sketch (all key families incl. FT doc + graph keys), and the open Linux connection-migration question with OrbStack verification recipe
  - doc names the immediate v0.7 hardening follow-up: connect-time shard-count probe (INFO/CONFIG) -> warn or fail-fast on shards>1
</must>
Reject:
<reject>
  - any crates/ diff in this task -> out of scope, split to a new task
  - recommending {scope} braces WITHOUT the Moon TXN-pin dependency being named -> the probe disproved client-only tagging; the doc must not oversell
</reject>
After:
<after>
  - the multi-shard atomicity gap is a written, probed, prioritized design artifact instead of tribal knowledge
  - Moon gets a concrete, evidence-backed feature request (TXN pinning) it can schedule
  - the v0.8 keyspace decision (braces or not) has its options + migration costs pre-analyzed
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ "Connection migration" in Moon's Linux feature set might ALREADY solve shard binding (probe ran on macOS/kqueue) — lowest confidence because the macOS probe cannot falsify a Linux-only path; the doc flags it as an OPEN QUESTION with the OrbStack verification recipe rather than asserting either way. Cost if wrong: the Moon feature request may partially exist.
  - [x] TXN rejects (not silently splits) cross-shard writes — live-proven, so current INGEST-04 cannot silently corrupt on multi-shard; worst case is loud ingest failure.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: probe reproduces the cross-shard rejection and the binding finding
  Given a fresh Moon --shards 4
  When spike-scope-hashtag-probe.py runs
  Then unbraced multi-key TXN shows partial rejection AND braced TXN still rejects
       unless the connection landed on the tag's shard, with a printed verdict table
  And a --shards 1 run shows all writes accepted (the latent-gap control)

Scenario: RFC is schedulable as Moon work
  Given the probe evidence
  When docs/design/scope-hashtag-txn-rfc.md is read by a Moon maintainer
  Then it contains the exact code pointers, three Moon-side pinning mechanisms with
       implementation costs, a recommended mechanism with a concrete command surface and
       undo-log/WAL implications, the Lunaris braced-keyspace migration sketch, and the
       open Linux connection-migration question
  And it does NOT recommend client-only hash-tagging (disproven by the probe)
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
DELIVERABLES (no crates/ changes):
  scripts/spike-scope-hashtag-probe.py   — argparse: --port (default 6395); prints VERDICT table;
                                           exit 0 iff observations match the documented matrix
  docs/design/scope-hashtag-txn-rfc.md   — RFC sections: Problem · Evidence (probe + code
                                           pointers: CrossStoreTxn in ConnectionState;
                                           handler_sharded/mod.rs:1630 guard; commit path's four
                                           shard_id-bound structures) · Mechanisms (M1 TXN.BEGIN
                                           PIN <key> + shard-side txn table + SPSC forwarding /
                                           M2 connection shard-pin handshake / M3 full server-side
                                           TXN forwarding) · Recommendation (one mechanism, with
                                           command surface + undo-log/WAL implications) · Lunaris
                                           adoption ({scope}-braced keyspace, all key families
                                           incl. FT-doc + graph keys, migration sketch) · Open
                                           question (Linux connection-migration, OrbStack recipe) ·
                                           Follow-up tasks (incl. connect-time shard-count guard)
Direction locked at freeze (reshaped by Tin Dang's answer "Recommend Moon work now"): produce the
  concrete Moon TXN-pinning RFC — deeper Moon-side design — rather than an options memo; client-only
  hash-tagging stays disproven; Lunaris braced-keyspace adoption is contingent on the Moon primitive.
```

Status: FROZEN @ v1 — approved by Tin Dang (2026-06-11, freeze #7, reshaped to a concrete Moon TXN-pinning RFC per the freeze answer "Recommend Moon work now")
Least-sure flag surfaced at freeze:
  ⚠ [spec] The probe ran on macOS — Linux "connection migration" MIGHT change the binding story; the doc records it as an open question with an OrbStack recipe instead of a claim.
  ⚠ [contract] Locking "A now, B later" pre-commits the v0.8 keyspace direction to braces-pending-Moon-work; if Moon ships a different primitive (e.g. server-side TXN forwarding) the migration sketch transfers but the format choice reopens.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: design spike — the probe script IS the executable test (exit 0 iff the observation matrix holds); no cargo suite. Red = probe script absent.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - scripts/spike-scope-hashtag-probe.py: --shards 4 matrix (unbraced partial-reject, braced still-reject-unless-lucky-shard, single-shard control all-OK); verdict printed; non-zero exit on any deviation
  - RFC completeness (docs/design/scope-hashtag-txn-rfc.md): §6 SEMANTIC check (read in full against the §3 section list)
</test_plan>

Tests live in: `scripts/spike-scope-hashtag-probe.py` · MUST run red (missing implementation) before Build.
Red confirmed 2026-06-11: `ls scripts/spike-scope-hashtag-probe.py docs/design/scope-hashtag-txn-rfc.md` → both "No such file or directory" (red for the right reason — deliverables absent, not a broken harness).
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

- [x] all tests pass — the probe IS the test: `python3 scripts/spike-scope-hashtag-probe.py --port 6403` → exit 0, all three rows MATCH (P1 4 OK/4 reject · P2 16/16 conns all-rejected, homogeneous, 0 lucky · P3 8/8 OK + COMMIT +OK). Run twice (6401, 6403) with self-launched fresh servers in tempdirs.
- [x] coverage did not decrease — design spike, no cargo suite in scope; workspace untouched (zero crates/ diff confirmed via `git diff --stat HEAD -- crates/` = empty)
- [x] no test or contract was altered during build — §3 untouched after FROZEN stamp; one build-time fix INSIDE the probe before any green claim: wire form is `TXN BEGIN` (two-arg subcommand, per is_txn_begin in command/transaction.rs:98), not `TXN.BEGIN` — a correction of the test harness toward reality, not a weakening
- [x] concurrency / timing — probe launches its own servers on --port/--port+1 in TemporaryDirectory, kills them in finally; 15s PING readiness loop; ports are caller-chosen so SO_REUSEPORT stale-listener collisions are avoidable (lsof-checked clean before runs)
- [x] no exposed secrets / injection / new deps — stdlib-only Python, localhost sockets, tempdirs
- [x] layering — docs/design/ + scripts/ only; spike scope boundary held
- [x] reviewed — autonomy: auto; auto-resolved on complete evidence (no security/concurrency/architecture residue)

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — probe is a standalone executable; RFC §Probe line + Appendix reference it; script docstring + RESULT line reference the RFC back. Verified by running it (exit 0).
- [x] DEAD-CODE (code) — unused `rest` from partition() caught in review and fixed (`line, _, _`) before gate; no other unreferenced symbol
- [x] SEMANTIC (prose) — RFC read in full against the §3 section list: Problem ✓ · Evidence with all six code pointers (CrossStoreTxn/conn_state.rs · mod.rs:1630 guard + sibling sites · txn.rs four shard_id-bound commit structures · abort.rs · slots.rs:19-20 · transaction.rs:36) ✓ · M1/M2/M3 with costs ✓ · Recommendation M1 + command surface + undo/WAL implications ✓ · Lunaris adoption table covers KV + FT-doc + graph + MQ/scratchpad families + 5-step migration ✓ · Open Linux connection-migration question + OrbStack recipe ✓ · Follow-ups incl. connect-time shard-count guard ✓ · Reject honored: client-only hash-tagging explicitly disproven, never recommended ✓

### GATE RECORD
Outcome: PASS (auto-resolved under autonomy: auto — evidence complete, no security finding, no crates/ surface)
Reviewed by: Claude (ADD verify, auto) · date: 2026-06-11

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): ERR_TXN_CROSS_SHARD rate in any future multi-shard deployment (today: structurally zero — all recipes are --shards 1); re-run scripts/spike-scope-hashtag-probe.py on every vendor/moon bump to detect TXN-semantics changes.
Spec delta for the next loop: the spike disproved its own original premise (client-side hash-tagging) and produced a Moon feature request instead — the v0.8 keyspace decision is now gated on Moon shipping TXN BEGIN PIN, with the connect-time shard-count guard as the only Lunaris-side action available now.

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
- [SDD · folded] a "design spike" contract can be reshaped at freeze into a cross-repo RFC when the probe disproves the in-repo fix — the freeze answer itself carried the scope change (evidence: freeze #7 "Recommend Moon work now" → §3 v1 rewritten before stamping)
- [TDD · folded] for doc-deliverable spikes, making the probe script the executable test (exit 0 = evidence matrix reproduces) keeps red/green meaningful without a cargo suite (evidence: red = file absent, green = MATCH verdict, two runs)
- [DDD · folded] Moon subcommand wire form is `TXN BEGIN` (two args), not `TXN.BEGIN` — dotted-command intuition from FT.*/MQ.* does not transfer to TXN/TEMPORAL handlers (evidence: is_txn_begin at vendor/moon/src/command/transaction.rs:98; first probe run failed on it)
- [ADD · folded] probes that launch their own servers in tempdirs are immune to the SO_REUSEPORT stale-listener trap that bit the dim_configurable investigation (evidence: probe runs clean on arbitrary ports without lsof archaeology)
<!-- e.g.  - [DDD · folded] the model missed multi-tenancy (evidence: scenario_x failed) -->
