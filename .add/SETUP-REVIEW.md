# SETUP REVIEW — Lunaris

production · brownfield · drafted by Claude (Fable 5) @ 2026-06-11

| # | Decision | Lands in | Tag | Why / Evidence |
|---|----------|----------|-----|----------------|
| 1 | First contract adopts the SDK `body` wire field for MQ messages, dropping `partition`+`payload` fields from the wire (partition becomes API-only metadata) | first-contract | `guessed` | Inference: `parse_mq_pop` never reads `partition` and `QueueMsg.partition` echoes the subscriber arg (queue.rs:120,157), so the field is write-only — BUT any external Redis client (e.g. Helios-side tooling) that POPs Lunaris topics directly would break. Repo shows no such consumer; I could not prove none exists outside it. |
| 2 | `MQ PUBLISH` (publish_txn) is worth one evidence test on live Moon v0.3.0 before any in-TXN queue work | first-contract | `guessed` | Inference from the SDK shipping `publish_txn` (vendor/moon/sdk/rust/src/mq.rs:141); the 2026-06-09 spike proved dotted `MQ.POP` is server-unhandled, so SDK presence ≠ server support. The contract makes BOTH outcomes a pass — the cost of being wrong is one dead follow-up, not rework. |
| 3 | Milestone task ORDER (mq-typed-client → graph-decay → ft-navigate → sq8 → hotkeys → bench → spike) and the two depends-on edges | scope | `guessed` | My value-to-effort ranking from the 2026-06-11 integration review, confirmed by the user's "implement all as your recommended like your orders" — but the dependency edges (ft-navigate after graph-decay; bench after ft-navigate) are my judgment, not code-forced. |
| 4 | SQ8 task must eval BOTH SQ8 and TQ4 at 768d before any default flip | scope | `evidence-grounded` | vendor/moon CLAUDE.md §Vector Search: "TQ4 at 384d loses recall… TQ4 shines at 768d+; SQ8 validated at ~0.90 R@10 on MiniLM 384d" — Lunaris embeds at 768d (granite-embedding-311m, root CLAUDE.md), so Moon's own guidance makes the winner non-obvious. |
| 5 | Hash-tag multi-shard work is a DESIGN SPIKE only (no keyspace migration this milestone) | scope | `evidence-grounded` | atomic.rs:51-60 — Moon TXN rejects cross-shard writes; keyspace format `lunaris:{scope}:{kind}:{ulid}` is load-bearing across all backends + RFC 0001; migrating it is bigger than this wave. |
| 6 | Project goal line = the sub-25ms/atomicity contract | PROJECT.md | `evidence-grounded` | Root CLAUDE.md "Core Value" verbatim; benchmark memory shows strict-replay p50 10.3ms already proving it. |
| 7 | Stage = production | foundation | `evidence-grounded` | Published crates.io/npm v0.3.0 (CI release workflows), progressive-rollout constraint in root CLAUDE.md, live downstream consumer (Helios). |
| 8 | ADD code/tests live in `crates/` per cargo convention, NOT `.add/tasks/<slug>/src/` | CONVENTIONS.md | `evidence-grounded` | 25-crate cargo workspace (root Cargo.toml); duplicating source trees under .add/ would break workspace builds. TASK.md path tokens point at project root. |
| 9 | Domain nouns, invariants (INGEST-04, keyspace helpers, lock discipline, MCP schema-root), conventions, glossary | PROJECT.md / CONVENTIONS.md / GLOSSARY.md | `evidence-grounded` | Root CLAUDE.md (GSD-maintained), crates/lunaris-core, docs/ARCHITECTURE.md — copied, not invented. |
| 10 | dependencies.allowlist = current [workspace.dependencies] | allowlist | `evidence-grounded` | Root Cargo.toml, extracted 2026-06-11. |
| 11 | GSD stays the owner of .planning/ roadmap (v0.6 RAPTOR phases); ADD owns only this Moon-exploit wave | foundation | `guessed` | The two systems coexist as of today; the user invoked /add for this wave but .planning/STATE.md still says "Next: Phase 29 RAPTOR". I assumed parallel tracks, not a migration of GSD work into ADD. |

Sign: confirm in chat → the agent runs `add.py lock --by "Tin Dang"` (typing it yourself works too)
