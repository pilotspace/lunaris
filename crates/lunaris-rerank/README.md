# lunaris-rerank

Cross-encoder reranker (bge-reranker-v2-m3) for the [Lunaris](https://github.com/pilotspace/lunaris) agent
memory engine.

This crate is the bge-reranker-v2-m3 cross-encoder reranker, used at the recall surface to boost relevance without an LLM round-trip.

**It is a quality stage, not a latency-class stage.** Measured on an Apple M4 Pro with full Metal offload: **p50 1301.3 ms** at the default pool (`top_in = 2k = 60` fused candidates, ~21 ms per candidate pair, no batching), **575.6 ms** at `top_in=30`, plus a one-time ~1.0–1.4 s lazy GGUF load on the first reranked recall of the process. A CPU-forced control measured ~7.4 s. Enabling rerank voids the 25 ms p50 recall contract and the 100 ms latency SLO — see `docs/operations/capacity.md` §4. The former "sub-30 ms per candidate batch on CPU" claim was unmeasured and was retracted on 2026-08-21.

## Use

```toml
[dependencies]
lunaris-rerank = "0.2"
```

See the [Lunaris repository](https://github.com/pilotspace/lunaris) for
the umbrella crate, the 10-minute quickstart, the architecture overview,
and benchmarks.

## License

Apache-2.0. See [LICENSE](https://github.com/pilotspace/lunaris/blob/main/LICENSE).
