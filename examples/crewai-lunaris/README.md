# CrewAI + Lunaris

Back CrewAI agent memory with Lunaris.

```bash
pip install "lunaris-integrations[crewai]"
```

`LunarisCrewAIStorage` implements CrewAI's RAG storage interface
(`save` / `search` / `reset`) over a scope-bound `LunarisClient`. Lunaris owns
embedding + recall, so no CrewAI embedder configuration is needed.

## Run (live = needs a running lunaris-server)

```bash
export LUNARIS_SERVER_URL="http://127.0.0.1:8080"
export LUNARIS_TOKEN="<a JWT whose `scope` claim is the partition>"
python main.py
```

Wire it into a CrewAI memory by passing this storage to the memory's
`storage=` argument (see the CrewAI memory docs for the exact memory class you
use). `reset()` clears the bound scope via `forget_scope`.
