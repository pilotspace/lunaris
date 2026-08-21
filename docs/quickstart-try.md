# Try it in one command

`lunaris try` runs a complete memory store — ingest, index and recall — inside a
single process. No database to install, no container, no account, no config
file. It starts a private Moon on a loopback port, writes six sample memories,
asks the store a question, and shuts the server down when it exits.

```console
$ lunaris try
```

```text
lunaris try — a disposable memory store: no server, no account, no config.

  ✓ embedder       granite-embedding-311m-multilingual-r2.Q4_K_M.gguf (already staged)
  ✓ store          embedded Moon on 127.0.0.1:52350
  ✓ data           /Users/you/.lunaris/try/data (kept — a second run reuses it)
  ✓ corpus         6 sample memories

  ? why did we pick Moon instead of Postgres

 1. [0.914] sample/decisions  01M0H9HYYMTDZB9VJK4S6Y8CFS
    We chose Moon over Postgres for the memory substrate because recall has to stay under 25 ms at a hundred thousand documents per scope, and Moon answers vector search, BM25 and graph traversal in one round trip instead of three.
 2. [0.788] sample/decisions  01M0H9HZ0XKSETZTSTRGKFCZ52
    Lunaris runs against a single-shard Moon. A sharded Moon rejects the multi-key transaction the ingest path commits, so every deployment pins itself to one shard until cross-shard transactions land.
 ...

(via lunaris try — embedded Moon)
recalled 5 of 6 memories in 1 ms

Next:
  lunaris try --query "what broke at 2 a.m."   ask the sample store something else
  lunaris try --fresh                          wipe it and start over
```

> **About this transcript.** Every line above is real except the two similarity
> scores and the order they imply. The run that produced it used the
> deterministic test embedder (`LUNARIS_TRY_EMBEDDER=stub`) because the machine
> could only host one llama.cpp process at a time, so its ranking was a hash
> ordering and reprinting it here would have been a claim about retrieval
> quality that nobody measured. The banner, the port line, the six memories,
> the hit text and the `1 ms` recall are verbatim. With the granite embedder the
> same six memories come back, ranked by meaning.

Ask it something of your own:

```console
$ lunaris try --query "who should I talk to about retrieval"
```

## What it costs

| | first run | every run after |
|---|---|---|
| download | 253 MB embedder GGUF, sha256-verified, into `~/.lunaris/models/` | nothing |
| wall clock | network-bound: roughly 1–3 minutes on a normal connection | a few seconds |

There is no Moon binary to fetch — the store is compiled into the release
binary. The 253 MB is the embedding model, it is downloaded once per machine,
and it is shared with every other Lunaris surface (the MCP server, the Claude
Code hook, the SDKs), so a later `lunaris` install pays nothing.

The download shows a progress bar on stderr. If you already have weights, point
`LUNARIS_EMBEDDER_GGUF` at them and nothing is fetched.

## What it will not do

`lunaris try` **cannot reach a store it did not start.** It never reads
`LUNARIS_STORE_URL`, never looks at the daemon's advertised store, and gets its
port from binding `127.0.0.1:0`. Exporting a store URL before running it changes
nothing; the trial still talks only to its own server, and refuses outright if
the kernel ever hands it a port that carries real data (6379, 6380, 6381, 6399).

It also never issues `FLUSHALL`. `--fresh` deletes the trial's own data
directory.

## Where the data goes

`~/.lunaris/try/data`, and it stays there. That is deliberate: the second thing
people do is ask another question, and a trial that threw its store away would
answer that with nothing. Every sample carries a stable dedupe key, so re-running
returns the existing entries rather than writing a second copy — the directory
cannot grow no matter how many times you run it.

* `LUNARIS_TRY_DIR=/somewhere/else` moves it.
* `lunaris try --fresh` deletes it and starts over.

The store itself is only alive for the duration of the command. To keep a store
running for your own agents, run a Moon and point the CLI at it:

```console
$ LUNARIS_STORE_URL=moon://127.0.0.1:<port> lunaris recall --scope mine "your question"
```

## Building it yourself

The trial needs the in-process store, which is opt-in because it compiles a
whole database and must never land in an ordinary `cargo test --workspace`:

```console
$ cargo build --release -p lunaris-cli --features embedded-moon
```

A plain `cargo build -p lunaris-cli` is not, and `lunaris try` in that build
exits non-zero telling you this exact command.

**Today that clone-and-build is the only way to get it.** No workflow in
`.github/workflows/` builds `lunaris-cli`, so there are no prebuilt `lunaris`
binaries on any GitHub Release — only `lunaris-mcp` is prebuilt, and it ships
inside the `npx` / `uvx` wrapper packages rather than as a loose binary. A
release job for this CLI is tracked as **W0.9** in the ship plan; until it
lands, "one command" means one command *after* a clone, and this page should
not be read as promising a download.

## Flags

| flag | meaning |
|---|---|
| `--query <TEXT>` | ask something other than the built-in question |
| `-k, --k <N>` | how many hits to print (default 5, of a 6-memory corpus) |
| `--fresh` | delete the trial store and start from empty |

`--scope` and `--json` are accepted by every other subcommand and **refused**
here, with the command that does what you meant. The trial writes to a fixed
scope in a store it owns and prints a guided run rather than a payload, so
honouring either flag is impossible — and quietly ignoring `--scope mine` would
look exactly like putting the samples in your partition.

| environment | meaning |
|---|---|
| `LUNARIS_TRY_DIR` | where the trial store lives (default `~/.lunaris/try`) |
| `LUNARIS_EMBEDDER_GGUF` | use your own weights; nothing is downloaded |
| `LUNARIS_CLI_LOG` | raise the log level (the trial runs at `error` so first-run output stays readable) |

<!--
README splice point: insert this section as "## Try it in one command",
immediately after the badges/tagline and BEFORE the existing "Install" /
"Quickstart" heading. It is the first thing a stranger should be able to run,
and it must precede any instruction that assumes a Moon.
-->
