# lunaris-storage-moon

Moon (Redis-compatible) storage backend for the [Lunaris](https://github.com/lunaris-dev/lunaris) agent
memory engine.

This crate is the high-performance Moon storage backend — FT.SEARCH for vectors, native HSCAN for KV, MQ for the audit queue. Requires the moondb crate (sibling repo).

## Use

```toml
[dependencies]
lunaris-storage-moon = "0.2"
```

See the [Lunaris repository](https://github.com/lunaris-dev/lunaris) for
the umbrella crate, the 10-minute quickstart, the architecture overview,
and benchmarks.

## License

Apache-2.0. See [LICENSE](https://github.com/lunaris-dev/lunaris/blob/main/LICENSE).
