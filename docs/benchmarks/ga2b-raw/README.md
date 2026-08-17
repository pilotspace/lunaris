# GA-2b raw result envelopes

Audit trail for [`docs/operations/capacity.md`](../../operations/capacity.md)
— the JSON envelopes exactly as emitted by
`recall-latency measure` (harness: `crates/lunaris-bench/src/bin/recall_latency.rs`,
runner: `scripts/bench/perf/recall_latency.sh`). Corpus: 100k docs, Moon
v0.8.5 @ 6401, Apple M4 Pro / macOS 15.7.9, retrieval-only methodology.

The rerank config ran as two 250-query processes (disjoint
`--query-offset`, per-process lazy model load excluded from timed
samples). Merged percentiles over the 500 combined raw samples:

```
n=500 mean=1309.5 p50=1301.3 p95=1367.0 p99=1510.7 max=2585.0  (ms)
```

Raw per-query sample files (`*.samples`) land in `target/ga2b/` on each
run and are not committed.
