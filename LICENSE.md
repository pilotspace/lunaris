# License

Lunaris is **dual-licensed** under either of the following, at your option:

- **Apache License, Version 2.0** — see [`LICENSE-APACHE`](./LICENSE-APACHE)
  ([apache.org/licenses/LICENSE-2.0](https://www.apache.org/licenses/LICENSE-2.0))
- **MIT License** — see [`LICENSE-MIT`](./LICENSE-MIT)
  ([opensource.org/licenses/MIT](https://opensource.org/licenses/MIT))

SPDX expression: `Apache-2.0 OR MIT`

## Scope

The dual-license applies to every Lunaris-owned crate and SDK in this
repository:

| Component                            | Manifest                                       | License             |
| ------------------------------------ | ---------------------------------------------- | ------------------- |
| Rust workspace (all member crates)   | `Cargo.toml`                                   | `Apache-2.0 OR MIT` |
| Python SDK (`pip install lunaris`)   | `crates/lunaris-py/pyproject.toml`             | `Apache-2.0 OR MIT` |
| TypeScript SDK (`npm i lunaris`)     | `crates/lunaris-ts/package.json`               | `Apache-2.0 OR MIT` |
| MLX evaluation spike                 | `crates/lunaris-embed-mlx-spike/Cargo.toml`    | `Apache-2.0 OR MIT` |

Vendored third-party code under `vendor/` retains its upstream license; see
the individual `LICENSE` file in each vendored directory.

## Contributions

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this work by you, as defined in the Apache-2.0
license, shall be dual-licensed as above, without any additional terms or
conditions.
