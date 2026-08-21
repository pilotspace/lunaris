# Lunaris Quickstart — Python

Mirrors the [Rust quickstart](../quickstart-rs/README.md) against the same
single-shard Moon: `lunaris.open` → `scoped.ingest` → `scoped.recall`, from one
Python script.

**0.7.0 is Moon-only.** The relational backends were deleted; if you are coming
from 0.6.x see [`docs/migration/0.6-to-0.7.md`](../../docs/migration/0.6-to-0.7.md).

## You need a Moon binary, and there isn't a download yet

Lunaris 0.7.0 refuses any Moon below `0.8.5`, and **v0.8.5 was published with
zero release assets** — the tarballs 404 and the ghcr package answers `401` to
an anonymous pull. Read
[the Rust quickstart's section on this](../quickstart-rs/README.md#read-this-before-you-start-you-need-a-moon-binary-and-there-isnt-a-download-yet)
first; it has the two paths that do work. Short version: build Moon from the
`vendor/moon` submodule, or run `lunaris try` and skip the server entirely.

## Prerequisites

- Python 3.11+ with `pip` or `uv`
- a single-shard Moon on `127.0.0.1:6380`
- the granite-r2 Q4_K_M GGUF staged at `~/.lunaris/models/` (the wheel embeds
  llama.cpp; override the path with `LUNARIS_EMBEDDER_GGUF`)

## Three steps

```bash
cd examples/quickstart-py

pip install lunaris                       # or: uv add lunaris

export LUNARIS_STORE_URL="moon://127.0.0.1:6380"
python quickstart.py
```

There is no migration step and no role bootstrap — Moon needs neither.

Expected output:

```
quickstart: opening lunaris handle at moon://127.0.0.1:6380
quickstart: ingested episode at lsn=1746000000000:1 under scope `quickstart`
quickstart: recalled 1 hit(s) for 'hello'
quickstart:   top hit score=0.83 text='# Hello from Lunaris ...'
```

## Local-dev variant (building the wheel from this repo)

```bash
cd ../../crates/lunaris-py
maturin develop --release
cd ../../examples/quickstart-py
python quickstart.py
```

`maturin develop` compiles the `lunaris-py` cdylib in place and installs it
into the active virtualenv, so the script imports your local build. Add
`--no-default-features` for a Tier-0 wheel with no C++ toolchain — that build
has no local embedder, so pass `embedder=EmbedderConfig.noop(768)` to
`lunaris.open` or configure a remote one, and expect ranking to be meaningless.

## The surface this uses

| | |
|---|---|
| `Scope("quickstart")` | validating newtype, `^[A-Za-z0-9_\-.]{1,128}$`; raises `ValueError` on a bad string |
| `handle.scoped(scope)` | scope-bound view; every operation carries the partition key |
| `EpisodeBuilder(source, content)` | scope-less payload builder — `ingest` stamps the scope, so a caller cannot inject one |
| `await scoped.ingest(builder)` | returns the string-formatted `Lsn` |
| `await scoped.recall(text)` | returns hit dicts (`text`, `score`, `source`, `id`, …), filtered to this scope |

## Where the DSL stops

The Rust quickstart also shows a typed DSL form
(`scoped.dsl().with_root(Vector::new("chunks", 30).top(5))`). **That has no
working Python equivalent**, and it is worth being precise about why:

- `scoped.dsl()` returns the codegen-frozen native `RetrievalBuilder`
  (`crates/lunaris-py/src/generated.rs`). Every combinator on it raises
  `NotImplementedError`, and it exposes no `.execute()` at all.
- The builder that *does* work is the pure-Python one reached from
  `handle.recall()` — `handle.recall().query(q).top(5).execute()`. It has no
  scope parameter, so it reads a different partition than the one this script
  ingests into.

So there is currently no scope-bound DSL path in the Python binding. Use
`scoped.recall(text)` for scoped retrieval, and read
[`examples/quickstart-rs/`](../quickstart-rs/) for the full DSL walkthrough.

## Troubleshooting

- `ImportError: No module named 'lunaris'` → `pip install lunaris`, or
  `maturin develop` for local dev.
- `error: set LUNARIS_STORE_URL=...` → the script refuses to guess a store URL.
  Guessing would let it write demo episodes into whatever Moon owns that port.
- An `unsupported Moon version` handshake rejection → your Moon predates 0.8.5.
- Recall returns nothing / a `WARN` about the embedder → the granite-r2 Q4_K_M
  GGUF isn't staged. Download it to `~/.lunaris/models/` (SHA-256s:
  `cargo run -p lunaris-bench --bin stage-models -- --help`) or pass
  `EmbedderConfig.llamacpp(gguf_path=...)` to `lunaris.open`.
- `TXN does not support cross-shard writes` → Moon is sharded. It must run with
  `--shards 1`.
