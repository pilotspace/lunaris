# Appendix: RFCs & Changelog

**Deeper and internal references — design rationale, the change log, and the
live-measurement / benchmark evidence behind the performance claims in this
book.** These live in the repository (not rendered into this book) so they
can stay close to the code; the paths below are repo-relative.

## Design RFCs (`docs/rfcs/`)

The accepted design RFCs that shape the current architecture. Where this book
says "RFC 000N" it means one of these:

| RFC | Title | What it decides |
|---|---|---|
| `docs/rfcs/0001-scope-newtype.md` | `Scope` newtype and `ScopedLunaris<'a>` typestate | Multi-agent partition key — the validated `Scope` alphabet, the `lunaris:{scope}:{kind}:{ulid}` KV format, Postgres RLS, and why the typestate makes cross-scope leaks a compile error. |
| `docs/rfcs/0004-extractor-tiers.md` | `ExtractorTier` typestate enum and laptop-floor default swap | The candle / Ollama / cloud-API extractor tiers and the default-model choice. |
| `docs/rfcs/0006-verifier-default-swap.md` | Verifier default swap: Gemma 3 27B → Gemma 3 270M | Why the verifier defaults to `NoopVerifier` and the `verify-small` laptop-floor build. |
| `docs/rfcs/0007-fallback-combinators.md` | `FallbackExtractor<P, F>` / `FallbackEmbedder<P, F>` with per-provider circuit breakers | The v0.3 resilience primitives — the fan-out / fallback combinators referenced by the [Cognee migration](../migrating/cognee.md). |

See also `.planning/architect/blueprint.md` — the canonical architecture
document the RFCs amend.

## Changelog & release scope

- `CHANGELOG.md` — per-version change log (v0.1 → v0.2.x and beyond).
- `docs/RELEASE.md` — current release scope and what lands in the next minor.
- `docs/migration/0.1-to-0.2.md` — the 0.1 → 0.2 breaking-change guide
  (`Scope`, RLS role recipe, the documented v0.2.0 operational constraints).

## Live-measurement & benchmark reports

- `LIVE-MEASUREMENT-REPORT.md` — Lunaris ↔ Moon SDK/server contract-drift
  report; the live evidence behind the "sub-25 ms recall" moat (strict-replay
  p50 ≈ 10 ms / p99 ≈ 21 ms on the 2026-04-23 run).
- `docs/benchmarks/v0.2.x/README.md` — reproducible benchmark harness +
  baseline numbers (the scripts, the SQuAD/replay setup, the env vars).
- `docs/benchmarks/v0.2.x/verifier-divergence.md` — RFC 0006 §4 verifier
  divergence capture (the AS_OF-parity `ScoreEpsilon` story).
- `milestones/v0.1.1-bench/recovery-test.log` — the crash-recovery evidence
  log referenced from [Durability & Recovery](../operations/durability.md).

## Audits

- `docs/audits/v0.2.1-unwrap-audit.md` — the v0.2.1 `unwrap()` / `expect()`
  audit (panic-surface review).
- `tmp/v0.2-code-review.md` — the v0.2 release-gate code review (RC-1 … RC-4,
  P-1 … P-5; closure status tracked in `docs/migration/0.1-to-0.2.md` §10.3).

## How this fits together

- Conventions enforced in code review (Scope, HTTP DTO discipline, Postgres
  RLS, the grep-pinned invariants) live in the repository `CLAUDE.md`.
- The generated rustdoc — built from `cargo doc` on every release — is the
  authoritative API surface; see [API Reference](../reference/api.md).
- Where this book disagrees with the Rust source, the source wins.
