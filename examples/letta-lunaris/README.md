# Letta + Lunaris (connector shim + recipe)

> **Status: connector shim + recipe (not a drop-in archival backend).** Letta's
> archival store (`letta.services.passage_manager`) is server-side and
> DB-coupled, with no clean lightweight connector base to subclass at the
> pinned version. So Lunaris ships a thin **client-backed connector**
> (`LunarisArchivalConnector`) that maps Letta's archival `insert`/`search`
> onto Lunaris, plus this recipe for wiring a Letta deployment at Lunaris.

## Install

```bash
pip install -e "../../integrations[letta]"
```

`lunaris-integrations` is not on PyPI yet, so install it from this repo. The
`pip install "lunaris-integrations[letta]"` line will work once it is
published.

## Option A — use the connector shim directly

Drive Lunaris archival memory from your own code with the same insert/search
verbs Letta uses. `main.py` talks to a running `lunaris-server`, which in turn
needs a single-shard Moon. **Moon v0.8.5 shipped zero release assets** (the
tarballs 404, ghcr answers `401` anonymously), so you have to build one — see
[the Rust quickstart](../quickstart-rs/README.md#read-this-before-you-start-you-need-a-moon-binary-and-there-isnt-a-download-yet).
Once you have it:

```bash
# 1. Moon (single shard — Lunaris ingest is one MULTI/EXEC TXN)
vendor/moon/target/release/moon --port 6380 --shards 1 --dir /tmp/lunaris-moon

# 2. A tokens file. The wire credential is an OPAQUE BEARER TOKEN, not a JWT —
#    the claims live server-side, in this file.
cat > /tmp/lunaris-tokens.json <<'JSON'
{ "demo-token": { "tenant": "letta-agent-1", "scopes": ["ingest", "recall", "forget"] } }
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

`insert(passage)` → `ingest`, `search(query, top_k)` → `recall`. A passage may
be a raw string or a `letta.schemas.passage.Passage` / `PassageCreate`.

## Option B — point a Letta server at Lunaris

For a full Letta deployment, run `lunaris-server` and route Letta's archival
memory calls through the connector at your agent's tool/service boundary
(Letta's archival insert/search are server-side). Bind one Lunaris scope per
Letta agent by issuing **one bearer token per agent**, each with its own
`tenant` claim in the tokens file. As Letta stabilises a pluggable archival
connector base, this shim becomes a drop-in subclass — the mapping does not
change, because both sides already speak `insert`/`search`.

## How the scope is bound

**There are no JWTs in v0.** The bearer token is opaque; the server maps it to
`{tenant, scopes}` from `--tokens-file`
(`crates/lunaris-server/src/state.rs::TokenClaims`). The partition scope is the
`tenant` claim, resolved server-side, and it never travels on the wire. The
`scope="letta-agent-1"` argument in `main.py` is validated client-side; the
server partitions by its own `tenant` claim.

## Typecheck it without a server

```bash
pip install -e "../../integrations[letta]" mypy
mypy main.py
python -c "import main"
```

That is the gate CI runs; see `.github/workflows/examples.yml`.
