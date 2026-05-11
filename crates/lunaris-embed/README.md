# lunaris-embed

Embedding providers (candle EmbeddingGemma, Ollama) for the [Lunaris](https://github.com/lunaris-dev/lunaris) agent
memory engine.

This crate is embedding backends behind a typed Embedder trait — candle EmbeddingGemma 300M for in-process inference and Ollama HTTP for sidecar deployments.

## Use

```toml
[dependencies]
lunaris-embed = "0.2"
```

See the [Lunaris repository](https://github.com/lunaris-dev/lunaris) for
the umbrella crate, the 10-minute quickstart, the architecture overview,
and benchmarks.

## License

Apache-2.0. See [LICENSE](https://github.com/lunaris-dev/lunaris/blob/main/LICENSE).
