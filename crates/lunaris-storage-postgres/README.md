# lunaris-storage-postgres

Postgres + pgvector storage backend for the [Lunaris](https://github.com/pilotspace/lunaris) agent
memory engine.

This crate is the OSS-default storage backend — pgvector for ANN, AGE for graph, pgmq for the audit queue, tsvector for BM25.

## Use

```toml
[dependencies]
lunaris-storage-postgres = "0.2"
```

See the [Lunaris repository](https://github.com/pilotspace/lunaris) for
the umbrella crate, the 10-minute quickstart, the architecture overview,
and benchmarks.

## License

Apache-2.0. See [LICENSE](https://github.com/pilotspace/lunaris/blob/main/LICENSE).
