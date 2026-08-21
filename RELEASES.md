# Releases

Per-release gate evidence. [`CHANGELOG.md`](CHANGELOG.md) is the authority on
*what* shipped; this file records *what was proven* before it shipped.

## 0.7.0 — 2026-08-18
milestones: v070-moon-only-ga (GA-1 unified recall root, GA-2 capacity study, GA-3 upgrade/rollback rehearsal, GA-4 release hygiene)
waivers: none
evidence: Moon is the only backend (Postgres + SQLite + `lunaris-migrate` deleted); one `production_root` plan asserted per surface by named conformance pins (`plan_repr`) so the benched path and the shipped path cannot drift; recall-ratchet CI gate live against a committed config-signature-locked baseline, replacing the Eval Gauntlet (20 startup failures / 0 completed runs); measured 100k-doc latency envelope with raw per-query samples committed under `docs/benchmarks/ga2b-raw/` (p50 19.2–22.4 ms, p99 ≤ 24.4 ms, 25 ms contract holds with ≤ 25 % headroom); backup/restore drill executed (RPO = 0, RTO < 1 s); §7 upgrade/rollback procedure rehearsed, not merely written
caveats: the envelope is 100k docs on a single Moon shard — beyond that the p50 contract is unvalidated (`docs/operations/capacity.md` §5). Rerank is opt-in and measures ~1.3 s/recall; enabling it voids the latency SLO. Helios canary still outstanding.

## 0.6.2 — 2026-08-15
milestones: mem0-parity-hardening, operability pack
waivers: none
evidence: last release shipping the Postgres and SQLite backends; historical `read_as_of` / `scan_range` on Moon now fail loudly (`NotSupported` → HTTP 501) instead of quietly returning present-time data, pinned by `crates/lunaris-conformance/tests/run_as_of_moon_gap.rs`

## 0.6.0-rc.2 — 2026-07-17
milestones: moon-v080-bump (+ rc.1 revalidation fixes; engram-soul-loop task 1)
waivers: none
evidence: SDK feature-forwarding P0 fixed + manifest-guarded + runtime-proven (recovery TESTs 1–3 PASS with real semantic scores); Python exit SIGABRT fixed red-134 → green-0; contextd embedded-Moon E2E-proven; workspace clippy clean; pre-existing CI reds (per-driver parity postgres pgvector, mcp-prebuild Windows UnixStream, PG AGE conformance) tracked, not regressions

## 0.6.0-rc.1 — 2026-07-15
milestones: moon-v030-exploit, claude-code-flagship, memory-contract-integrity, hook-session-scratchpad, memory-inspector
waivers: none
evidence: 5 milestones + llama.cpp-only cutover; 10/10 Moon E2E on the proxiable path; workspace clippy -D warnings clean; readiness floor clear (0 HARD-STOP, 0 waivers)

