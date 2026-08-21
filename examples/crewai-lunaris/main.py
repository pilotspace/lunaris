"""CrewAI + Lunaris — save/search agent memory through LunarisCrewAIStorage.

LIVE example: requires a running lunaris-server (which needs a single-shard
Moon behind it). The wire credential is an OPAQUE BEARER TOKEN, not a JWT — the
server maps it to {tenant, scopes} from its --tokens-file, and the partition
scope is the `tenant` claim, resolved server-side.

    export LUNARIS_SERVER_URL=http://127.0.0.1:8080
    export LUNARIS_TOKEN=demo-token
    python main.py

See README.md for the full runbook, including why Moon v0.8.5 has to be built
from source right now.
"""
from __future__ import annotations

import os

from lunaris_integrations import HttpLunarisClient
from lunaris_integrations.crewai import LunarisCrewAIStorage


def build_storage() -> LunarisCrewAIStorage:
    client = HttpLunarisClient(
        base_url=os.environ["LUNARIS_SERVER_URL"],
        token=os.environ["LUNARIS_TOKEN"],
        scope="crew-demo",
    )
    return LunarisCrewAIStorage(client)


def main() -> None:
    storage = build_storage()
    storage.save("Alice joined Acme in 2021", {"kind": "fact"})
    storage.save("Acme is headquartered in Berlin", {"kind": "fact"})

    for row in storage.search("where is Acme based", limit=3):
        print(f"{row['score']:.3f}  {row['context']}")


if __name__ == "__main__":
    main()
