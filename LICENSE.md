# License

Lunaris is licensed under the **Apache License, Version 2.0**:

- **Apache License, Version 2.0** — see [`LICENSE`](./LICENSE)
  ([apache.org/licenses/LICENSE-2.0](https://www.apache.org/licenses/LICENSE-2.0))

SPDX expression: `Apache-2.0`

## Scope

The Apache-2.0 license applies to every Lunaris-owned crate and SDK in this
repository:

| Component                            | Manifest                                       | License      |
| ------------------------------------ | ---------------------------------------------- | ------------ |
| Rust workspace (all member crates)   | `Cargo.toml`                                   | `Apache-2.0` |
| Python SDK (`pip install lunaris`)   | `crates/lunaris-py/pyproject.toml`             | `Apache-2.0` |
| TypeScript SDK (`npm i lunaris`)     | `crates/lunaris-ts/package.json`               | `Apache-2.0` |
| MLX evaluation spike                 | `crates/lunaris-embed-mlx-spike/Cargo.toml`    | `Apache-2.0` |

Vendored third-party code under `vendor/` retains its upstream license; see
the individual `LICENSE` file in each vendored directory.

## Contributions

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this work by you, as defined in the Apache-2.0
license, shall be licensed as above, without any additional terms or
conditions.
