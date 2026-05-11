# lunaris-ingest

Atomic ingest pipeline for the [Lunaris](https://github.com/lunaris-dev/lunaris) agent
memory engine.

This crate is the ingest pipeline — chunker, embed fan-out, single atomic_write commit (INGEST-04). One commit per Episode, all-or-nothing.

## Use

```toml
[dependencies]
lunaris-ingest = "0.2"
```

See the [Lunaris repository](https://github.com/lunaris-dev/lunaris) for
the umbrella crate, the 10-minute quickstart, the architecture overview,
and benchmarks.

## License

Apache-2.0. See [LICENSE](https://github.com/lunaris-dev/lunaris/blob/main/LICENSE).
