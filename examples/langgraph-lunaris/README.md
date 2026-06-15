# LangGraph + Lunaris

Use Lunaris as the persistent memory `BaseStore` for a LangGraph agent.

```bash
pip install "lunaris-integrations[langgraph]"
```

`LunarisStore` is a drop-in `langgraph.store.base.BaseStore` backed by Lunaris.
It is **transport-agnostic** — hand it a client factory that builds either an
`HttpLunarisClient` (talks the MemoryProtocol HTTP verbs on `lunaris-server`)
or an `SdkLunarisClient` (wraps the in-process `lunaris` wheel). The example
below uses HTTP.

## Run (live = needs a running lunaris-server)

```bash
export LUNARIS_SERVER_URL="http://127.0.0.1:8080"
export LUNARIS_TOKEN="<a JWT whose `scope` claim is the partition>"
python main.py
```

The scope is bound to the JWT on the server — it never travels on the wire.
The namespace you pass to `aput`/`asearch` selects the per-namespace scope.
