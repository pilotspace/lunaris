# moon-v080-bump — gate evidence (running record)

## G7 — upgrade-replay (v0.7.1 data dir → v0.8.0 binary)
**PASS** (2026-07-15). `scripts/test-recovery.py --upgrade-replay
--old-bin ~/.lunaris/bin/moon --new-bin vendor/moon/target/release/moon`
on port 6396. All planes intact: KV string, bi-temporal hash, graph node,
MQ backlog, TEMPORAL.SNAPSHOT_AT. rc=0.

## G8 — dashtable P0 A/B on the reconstructed production checkpoint
**PASS** (2026-07-16).
- A-leg (stock v0.7.1, no fix): rc=101 — reproduces the exact
  `unreachable: double NeedsSplit after split_segment` recovery panic.
- B-leg (v0.8.0 pin with fix 00823f16): boots the same data dir in ~10s;
  dbsize 219,661; 376 indexes; post-boot write accepted.
Gate dirs: ~/.lunaris/g8-dashtable-gate (+ g8-run-b working copy).

## G6 — recovery TESTs 1–3 on the v0.8.0 binary
**PASS with one explained pre-existing failure** (2026-07-16, port 6396):
- TEST 2 (Lunaris kill-9 mid-ingest): PASS — 100/200 docs at SIGKILL,
  post-kill dbsize/num_docs sane, recall executable.
- TEST 3 (writes after restart): PASS.
- TEST 1 plane probes (KV / bi-temporal / MQ survive+self-heal / graph /
  temporal snapshot): ALL PASS. dbsize + indices + chunks num_docs
  identical across kill-9 + AOF replay.
- TEST 1 semantic probe set-identity: FAIL — **explained, NOT a bump
  regression**. A/B on v0.7.1 fails identically (logs:
  scratchpad g6-full.log / g6-v071-ab.log / g6-settle.log). Root cause is
  a pre-existing Lunaris SDK bug from the v0.6 llama.cpp cutover:
  lunaris-py's `llamacpp` feature never forwards `lunaris/llamacpp` to the
  umbrella crate, so the wheel's default `open()` silently resolves
  NoopEmbedder → zero vectors → the probes measure HNSW tie-break order,
  which legitimately permutes across an AOF-replay rebuild. Proof: pure
  vector probes via `recall_simple_execute` return score 0.0 for every
  hit in insertion order for ANY query. Same bug in lunaris-ts.
  Follow-up task queued: sdk-llamacpp-feature-forwarding (fix both SDKs,
  build-graph regression test, rebuild wheel, rerun TEST 1 to full green).
- Harness fixes landed during triage: child spawn uses `sys.executable`
  (bare `python` not on PATH); post-restart 2s settle for apples-to-apples
  probes; both committed with the bump.

## G3 — workspace test battery
**PASS** (2026-07-17, split across three runs after a Metal-wedge abort):
- G3 main run: 89 suites green before `context_reuse` hit the known
  Metal-wedge family (36 min system-time spin) → SIGKILL, cargo aborted
  the remainder.
- G3b (package-scoped continuation, LUNARIS_DEVICE=cpu): 9 suites green.
- G3c (`--no-fail-fast` remainder: lunaris-memory, memory-service,
  recipes, rerank, retrieve, server, storage-*, verify, xtask): rc=0,
  132 suite-result lines, zero failures.
- `cold_start_under_500ms` flaked once under full build load (711 ms);
  solo retry PASS (and it passed in G5 earlier). Environmental, not a
  regression.

## G4 — clippy --workspace --all-targets -D warnings
**PASS** (2026-07-16). Definitive bare run rc=0 (an earlier pipe-masked
rc was discarded).

## G5 — lunaris-mcp --features embedded-moon
**PASS** (2026-07-16). 42+5 suites green; one earlier failure reproduced
as a stray leftover moon process (environmental), clean rerun green.

## SDK zero-vector follow-up (from G6 finding)
Fixed IN this branch (`83411fa`): both SDK crates now forward
`lunaris/llamacpp` + GPU features; 4-test manifest guard
(`sdk_feature_forwarding.rs`) proven red→green; rebuilt wheel gives real
semantic ranking from default `open()` (Tesla 0.248 / Warsaw 0.128 /
Super Bowl 0.375). Guard re-verified green post-commit. Full
`test-recovery.py` TEST 1 rerun with the fixed wheel deferred (first
attempt SIGBUS'd under G3 memory pressure) — tracked as follow-up.

## G1/G2/G9 — contract battery
**PASS** (2026-07-16, post-commit): 9/9 including
gitlink_pinned_v080_fix (flipped green at commit `c95ec46`).
