# lunaris-memory

Lunaris agent memory engine — umbrella crate for the [Lunaris](https://github.com/pilotspace/lunaris) agent
memory engine.

> **Published as `lunaris-memory`, imported as `lunaris`.** The bare `lunaris`
> name on crates.io belongs to an unrelated project, so the package name is
> `lunaris-memory` while the library name stays `lunaris` — your `use` statements
> are unaffected.

This crate is the umbrella crate — re-exports the lunaris-core types, drives the ingest hot path via `Lunaris::open` and the scoped `ScopedLunaris<'a>` typestate, and bundles the default storage / embed / retrieve / extract / verify / consolidate wiring.

## Use

```sh
cargo add lunaris-memory --rename lunaris
```

which writes:

```toml
[dependencies]
lunaris = { package = "lunaris-memory", version = "0.6" }
```

```rust
use lunaris::Lunaris;
```

See the [Lunaris repository](https://github.com/pilotspace/lunaris) for
the umbrella crate, the 10-minute quickstart, the architecture overview,
and benchmarks.

## License

Apache-2.0. See [LICENSE](https://github.com/pilotspace/lunaris/blob/main/LICENSE).
