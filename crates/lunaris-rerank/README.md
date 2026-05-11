# lunaris-rerank

Cross-encoder reranker (bge-reranker-v2-m3) for the [Lunaris](https://github.com/lunaris-dev/lunaris) agent
memory engine.

This crate is the bge-reranker-v2-m3 cross-encoder reranker — sub-30 ms per candidate batch on CPU, used at the recall surface to boost relevance without an LLM round-trip.

## Use

```toml
[dependencies]
lunaris-rerank = "0.2"
```

See the [Lunaris repository](https://github.com/lunaris-dev/lunaris) for
the umbrella crate, the 10-minute quickstart, the architecture overview,
and benchmarks.

## License

Apache-2.0. See [LICENSE](https://github.com/lunaris-dev/lunaris/blob/main/LICENSE).
