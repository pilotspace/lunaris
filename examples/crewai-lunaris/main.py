"""CrewAI + Lunaris — save/search agent memory through LunarisCrewAIStorage.

LIVE example (UAT): requires a running lunaris-server and a JWT. Run:

    export LUNARIS_SERVER_URL=http://127.0.0.1:8080
    export LUNARIS_TOKEN=<jwt>
    python main.py
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
