# lunaris-retrieve

Composable retrieval DSL (vector, keyword, graph, RAPTOR tree) for the [Lunaris](https://github.com/lunaris-dev/lunaris) agent
memory engine.

This crate is the fused retrieval DSL — Vector + Keyword + Graph + Tree (RAPTOR hierarchical) operators with RRF fusion, cross-encoder reranking, and tower middleware for caching/timeout/retry.

## Use

```toml
[dependencies]
lunaris-retrieve = "0.2"
```

See the [Lunaris repository](https://github.com/lunaris-dev/lunaris) for
the umbrella crate, the 10-minute quickstart, the architecture overview,
and benchmarks.

## License

Apache-2.0. See [LICENSE](https://github.com/lunaris-dev/lunaris/blob/main/LICENSE).
