# lunaris

Lunaris agent memory engine — umbrella crate for the [Lunaris](https://github.com/lunaris-dev/lunaris) agent
memory engine.

This crate is the umbrella crate — re-exports the lunaris-core types, drives the ingest hot path via `Lunaris::open` and the scoped `ScopedLunaris<'a>` typestate, and bundles the default storage / embed / retrieve / extract / verify / consolidate wiring.

## Use

```toml
[dependencies]
lunaris = "0.2"
```

See the [Lunaris repository](https://github.com/lunaris-dev/lunaris) for
the umbrella crate, the 10-minute quickstart, the architecture overview,
and benchmarks.

## License

Apache-2.0. See [LICENSE](https://github.com/lunaris-dev/lunaris/blob/main/LICENSE).
