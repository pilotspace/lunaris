# Milestone: memory-contract-integrity

**Goal:** Every promise the Lunaris memory surface makes is either kept or refused loudly on the production (Moon) backend — forget deletes, dedupe dedupes, cold-start injects, secrets never reach injected context. No silent no-ops.

**Why now:** The 2026-07-14 live deep test (MCP + hooks, Moon 6381) proved four contract violations that all fail *silently* — the worst failure mode for a memory engine whose core value is provable correctness. Findings ledger: memory files `project_lunaris_mcp_deep_test_findings` / `project_lunaris_hooks_deep_test_findings` (session 2634d8ba).

## Scope

- `crates/lunaris/src/forget.rs`, `crates/lunaris/src/handle.rs` (scope-aware forget pipeline)
- `crates/lunaris-storage-moon` (dedupe sidecar, keyword-leg error semantics)
- `scripts/lunaris-codex-hook-adapter.py` + `scripts/setup-lunaris-agents.py` (contextd lifecycle, verify cleanup)
- `crates/lunaris-hook/src/scrub.rs` + `context.rs` (scrubber set, curation nested-key tolerance)
- `crates/lunaris-mcp` (tool-contract honesty: as_of / dedupe caveats surfaced, scratchpad exact-key path)

Out of scope: real fact embeddings / SourceOp weighting (ranking quality), embedded FTS5, scope-derivation UX, reranker-in-contextd — recorded as deltas, not tasks.

## Tasks (breadth-first)

1. **forget-scope-routing** — Wave-1D scope-aware forget: `ScopedLunaris::forget` routes scan+write through the bound scope using canonical `lunaris_core::keyspace` keys; MCP `memory.forget` actually removes; live Moon discriminator red→green.
2. **contextd-cold-start-lifecycle** — inject deadline starts after daemon-ready; health answers during GGUF load; no socket-unlink/duplicate-spawn storm; `stop_verify_contextd` really kills; verify green with DEFAULT timeouts.
3. **moon-parity-honesty** — dedupe_key works on Moon (KV sidecar); `as_of` on Moon either enforced or loudly rejected; scratchpad exact-key read bypasses BM25 key analysis (KV/filter lookup; FT errors on keyword leg degrade, never surface); MCP tool descriptions carry backend caveats.
4. **scrub-and-curation-hardening** — built-in scrubber set extended (sk-ant-*, sk-*, xox*, glpat-, AIza*, case-insensitive password/passwd/pwd/token/secret=); curation tolerates smart-quote-scrubbed nested keys; discriminating live test: captured secret NEVER appears in injected context.

## Exit criteria

- [ ] `memory.forget` (prefix AND episode_id) removes episodes under a real scope on live Moon; removed>0 and post-forget recall misses — verifier: red→green live test in task 1
- [ ] Turnkey `--verify` passes with DEFAULT env on a cold box (no LUNARIS_CONTEXT_TIMEOUT_MS override) and leaves ZERO stray contextd processes — verifier: task 2 live proof + process-count assertion
- [ ] Same dedupe_key twice on Moon → was_duplicate=true, one episode — verifier: task 3 discriminating test on live Moon
- [ ] A captured `sk-ant-*` key and `password=…` value never appear in additionalContext — verifier: task 4 live capture→inject discriminator
