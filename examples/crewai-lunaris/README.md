# CrewAI + Lunaris

Back CrewAI agent memory with Lunaris.

`LunarisCrewAIStorage` implements CrewAI's RAG storage interface
(`save` / `search` / `reset`) over a scope-bound `LunarisClient`. Lunaris owns
embedding + recall, so no CrewAI embedder configuration is needed.

## Install

```bash
pip install -e "../../integrations[crewai]"
```

`lunaris-integrations` is not on PyPI yet, so install it from this repo. The
`pip install "lunaris-integrations[crewai]"` line will work once it is
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
{ "demo-token": { "tenant": "crew-demo", "scopes": ["ingest", "recall", "forget"] } }
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

## Wiring it into a crew

Pass this storage to your memory's `storage=` argument (see the CrewAI memory
docs for the exact memory class you use). `reset()` clears the bound scope via
`forget_scope`, which issues a **soft** purge — `hard=false`, so the two-step
`dry_run` → confirm dance is not required.

## How the scope is bound

**There are no JWTs in v0.** The bearer token is opaque; the server maps it to
`{tenant, scopes}` from `--tokens-file`
(`crates/lunaris-server/src/state.rs::TokenClaims`). The partition scope is the
`tenant` claim, resolved server-side, and it never travels on the wire. The
`scope="crew-demo"` argument in `main.py` is validated client-side, but the
server still partitions by its own `tenant` claim — so use **one token per
crew** if you want real per-crew isolation.

## Typecheck it without a server

```bash
pip install -e "../../integrations[crewai]" mypy
mypy main.py
python -c "import main"
```

That is the gate CI runs; see `.github/workflows/examples.yml`.
