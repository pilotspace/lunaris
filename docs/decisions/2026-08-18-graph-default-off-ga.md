# ADR: Graph fact-legs stay opt-in (default OFF) for v0.7.0 GA

- **Date**: 2026-08-18
- **Status**: Accepted (delegated owner decision, GA-4; closes the GA-2
  "N=125 A/B → set graph default" gate)
- **Owners**: Lunaris core
- **Related**: `crates/lunaris-retrieve/src/composition.rs`
  (`production_root(k, graph)`), `crates/lunaris/src/handle.rs`
  (`LUNARIS_GRAPH_ENABLED`, D-10), `docs/operations/capacity.md`
  (GA-2b measured envelope), `docs/operations/slo.md`

## Decision

`LUNARIS_GRAPH_ENABLED` keeps its current default: **OFF**. The unified
`production_root(k, graph)` ships with `graph = false` unless the operator
opts in. The one deliberate exception stands: the Claude Code hook's
context path (`hook_recall_root`) keeps fact legs ON, because it runs
against a small personal store where the latency cost is negligible and
the fact legs are the feature.

The planned N=125 LongMemEval A/B **on the unified post-GA-1 path** is
demoted from a GA gate to a post-GA quality-tracking item. It was blocked
on judge access (MiniMax key); we are not holding GA for it because no
plausible outcome of that A/B could flip this decision — see below.

## Why the A/B can no longer change the default

Two independent lines of evidence, either sufficient alone:

1. **Latency (new, decisive).** The GA-2b capacity study
   (`docs/operations/capacity.md`, 100k-doc corpus, Moon 0.8.5, M4 Pro)
   measured the production root at **p50 19.2–22.4 ms** graph-OFF versus
   **p50 ≈ 39 ms** graph-ON — the fact legs roughly double recall cost and
   push the default path **through the 25 ms p50 core contract**. A
   default that breaks the headline contract ("sub-25 ms recall … graph
   that's opt-in" — CLAUDE.md Core Value) is not eligible, regardless of
   quality results. Note the Core Value already promises the graph as
   *opt-in*; default-ON was only ever on the table if it had been free.
2. **Quality (prior, corroborating).** The last completed N=125 A/B
   (2026-07-28, pre-GA-1 path) scored graph-OFF 88.0% vs graph-ON 83.2%.
   The 4.8-pt delta sits at the ±5-pt judge/gen noise floor, so we read it
   as "no measurable quality win", not as "graph hurts" — but a feature
   that must *beat* the noise floor to justify a 2× latency cost showed no
   win at all.

For the A/B to justify default-ON it would need to (a) show a quality gain
clearly above the noise floor **and** (b) the fact legs would need to get
~2× cheaper. (a) alone flips nothing; (b) is engineering work
(intent-gated fact injection, cheaper fact hydration) that lands after GA
by definition. Hence: decide now, ship GA, keep measuring.

## What opt-in graph-ON buys today

Unchanged by this ADR: `LUNARIS_GRAPH_ENABLED=1` enables graph extraction
at ingest and adds the fact legs to `production_root` on every recall
surface (HTTP/SDK, MCP, hook). Operators whose workloads are
entity/relation-heavy and latency-tolerant can flip it per deployment; the
recall-ratchet CI baseline pins the graph-OFF configuration
(`config_signature.graph = "0"`), so a default flip would be caught, not
silent.

## Post-GA follow-ups (quality track, not gates)

1. Re-run the N=125 A/B on the unified path when judge access returns —
   expectation is confirmation, but the unified root (BM25+RRF now on by
   default) changed both arms, so re-measure rather than assume.
2. Intent-gated fact injection (only add fact legs when the query looks
   entity/relational) — the promising direction from the 2026-07-28
   review: it targets the latency cost and the FACT_HITS temporal-reader
   poisoning at once.
3. If a future A/B + cost work make graph-ON default-eligible, that flip
   is a minor-version decision with its own ADR, a ratchet re-baseline,
   and a capacity re-run.
