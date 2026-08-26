# lunaris-integrations

Memory adapters that drop Lunaris into your existing agent framework. One thin,
transport-agnostic layer — pick a transport once, reuse it across every adapter.

```bash
pip install -e "path/to/lunaris/integrations[langgraph]"   # or [crewai] / [letta]
```

`lunaris-integrations` is not on PyPI **yet**. `integrations-publish.yml`
builds and uploads it on a `v*` tag, so from the next release on the line is:

```bash
pip install "lunaris-integrations[langgraph]"
```

Until that tag lands, install from a checkout with the `-e` form above — the
one the [examples](../examples/) and CI both use, so it is the path that is
actually exercised.

## Why a separate package

`lunaris_integrations` is a **pure-Python** package, separate from the native
`lunaris` core wheel. Importing the client + scope layer never loads the
compiled cdylib, and the framework deps (langgraph / crewai / letta) are
**optional extras** — never hard dependencies of Lunaris core.

## The shared client

Every adapter is built over one `LunarisClient`, scope-bound at construction:

| Transport            | Use when                                                        |
|----------------------|-----------------------------------------------------------------|
| `HttpLunarisClient`  | You run `lunaris-server`; talks the MemoryProtocol HTTP verbs.  |
| `SdkLunarisClient`   | You embed the in-process `lunaris` wheel (`pip install lunaris`).|
| `StubLunarisClient`  | Tests — records calls, returns canned hits (no backend).        |

The HTTP transport binds scope to the JWT server-side; the partition scope
**never travels on the wire**. Namespaces map to scopes through the Lunaris
alphabet (`[A-Za-z0-9_\-.]{1,128}`, `:` rejected) so no key can byte-alias
another scope's partition.

## Adapters

| Framework | Class                       | Maps                                  |
|-----------|-----------------------------|---------------------------------------|
| LangGraph | `langgraph.LunarisStore`    | `aput`/`aget`/`asearch` → ingest/recall |
| CrewAI    | `crewai.LunarisCrewAIStorage` | `save`/`search`/`reset` → ingest/recall/forget |
| Letta     | `letta.LunarisArchivalConnector` | `insert`/`search` → ingest/recall (connector shim + recipe) |

> **Letta** ships as a client-backed connector shim + a [recipe](../examples/letta-lunaris/README.md):
> its archival store is server-side, so there is no clean drop-in base to
> subclass yet. The insert/search mapping is identical to the other adapters.

## Examples

Runnable per-framework examples live in [`../examples/`](../examples/):
`langgraph-lunaris/`, `crewai-lunaris/`, `letta-lunaris/`.

## Tests

```bash
pip install -e ".[langgraph,crewai,letta,test]"   # from integrations/
pytest tests/
```

The unit layer runs against `StubLunarisClient` — no backend, model, or wheel.
Live HTTP/SDK + a real framework end-to-end is exercised by the examples.
