# LangGraph + Lunaris

Use Lunaris as the persistent memory `BaseStore` for a LangGraph agent.

`LunarisStore` is a drop-in `langgraph.store.base.BaseStore` backed by Lunaris.
It is **transport-agnostic** — hand it a client factory that builds either an
`HttpLunarisClient` (talks the MemoryProtocol HTTP verbs on `lunaris-server`)
or an `SdkLunarisClient` (wraps the in-process `lunaris` wheel). `main.py` uses
HTTP.

## Install

```bash
pip install -e "../../integrations[langgraph]"
```

`lunaris-integrations` is not on PyPI yet, so install it from this repo. The
`pip install "lunaris-integrations[langgraph]"` line will work once it is
published.

## Run it

`main.py` talks to a running `lunaris-server`, which in turn needs a
single-shard Moon. **Moon v0.8.5 shipped zero release assets** (the tarballs
404, ghcr answers `401` anonymously), so you have to build one — see
[the Rust quickstart](../quickstart-rs/README.md#read-this-before-you-start-you-need-a-moon-binary-and-there-isnt-a-download-yet).
Once you have it:

```bash
# 1. Moon (single shard — Lunaris ingest is one MULTI/EXEC TXN)
vendor/moon/target/release/moon --port 6380 --shards 1 --dir /tmp/lunaris-moon

# 2. A tokens file. The wire credential is an OPAQUE BEARER TOKEN, not a JWT —
#    the claims live server-side, in this file.
cat > /tmp/lunaris-tokens.json <<'JSON'
{ "demo-token": { "tenant": "user-42", "scopes": ["ingest", "recall", "forget"] } }
JSON

# 3. The server
LUNARIS_STORAGE="moon://127.0.0.1:6380" \
LUNARIS_TOKENS_FILE=/tmp/lunaris-tokens.json \
cargo run --release -p lunaris-server

# 4. The example
export LUNARIS_SERVER_URL="http://127.0.0.1:8080"
export LUNARIS_TOKEN="demo-token"
python main.py
```

## How the scope is bound

**There are no JWTs in v0.** The bearer token is opaque; the server maps it to
`{tenant, scopes}` from `--tokens-file`
(`crates/lunaris-server/src/state.rs::TokenClaims`). The partition scope is the
`tenant` claim, it is resolved server-side, and it never travels on the wire —
the request DTOs carry `#[serde(deny_unknown_fields)]` precisely so a client
cannot smuggle a `scope` override past it.

`LunarisStore` maps the LangGraph namespace to a Lunaris scope through
`namespace_to_scope` and passes it to your client factory. With
`HttpLunarisClient` that scope is validated client-side but the server still
uses its own `tenant` claim, so **one token per namespace** if you want real
per-namespace partitions.

## Typecheck it without a server

```bash
pip install -e "../../integrations[langgraph]" mypy
mypy main.py
python -c "import main"
```

That is the gate CI runs; see `.github/workflows/examples.yml`.
