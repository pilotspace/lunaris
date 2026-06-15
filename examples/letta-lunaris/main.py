"""Letta + Lunaris — insert/search archival memory via LunarisArchivalConnector.

LIVE example (UAT): requires a running lunaris-server and a JWT. Run:

    export LUNARIS_SERVER_URL=http://127.0.0.1:8080
    export LUNARIS_TOKEN=<jwt>
    python main.py

See README.md for the degrade rationale + the Letta-server wiring recipe.
"""
from __future__ import annotations

import os

from lunaris_integrations import HttpLunarisClient
from lunaris_integrations.letta import LunarisArchivalConnector


def build_connector() -> LunarisArchivalConnector:
    client = HttpLunarisClient(
        base_url=os.environ["LUNARIS_SERVER_URL"],
        token=os.environ["LUNARIS_TOKEN"],
        scope="letta-agent-1",
    )
    return LunarisArchivalConnector(client, agent_id="letta-agent-1")


def main() -> None:
    connector = build_connector()
    connector.insert("Alice joined Acme in 2021")
    connector.insert("Acme is headquartered in Berlin")

    for row in connector.search("where is Acme based", top_k=3):
        print(f"{row['score']:.3f}  {row['content']}")


if __name__ == "__main__":
    main()
