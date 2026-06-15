# Letta + Lunaris (connector shim + recipe)

> **Status: connector shim + recipe (not a drop-in archival backend).** Letta's
> archival store (`letta.services.passage_manager`) is server-side and
> DB-coupled, with no clean lightweight connector base to subclass at the
> pinned version. So Lunaris ships a thin **client-backed connector**
> (`LunarisArchivalConnector`) that maps Letta's archival `insert`/`search`
> onto Lunaris, plus this recipe for wiring a Letta deployment at Lunaris.

```bash
pip install "lunaris-integrations[letta]"
```

## Option A — use the connector shim directly

Drive Lunaris archival memory from your own code with the same insert/search
verbs Letta uses:

```bash
export LUNARIS_SERVER_URL="http://127.0.0.1:8080"
export LUNARIS_TOKEN="<a JWT whose `scope` claim is the partition>"
python main.py
```

`insert(passage)` → `ingest`, `search(query, top_k)` → `recall`. A passage may
be a raw string or a `letta.schemas.passage.Passage` / `PassageCreate`.

## Option B — point a Letta server at Lunaris

For a full Letta deployment, run `lunaris-server` and route Letta's archival
memory calls through the connector at your agent's tool/service boundary
(Letta's archival insert/search are server-side). Bind one Lunaris scope per
Letta agent via the JWT `scope` claim. As Letta stabilises a pluggable archival
connector base, this shim becomes a drop-in subclass — the mapping does not
change because both sides already speak `insert`/`search`.
