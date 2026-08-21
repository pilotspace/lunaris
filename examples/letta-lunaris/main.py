"""Letta + Lunaris — insert/search archival memory via LunarisArchivalConnector.

LIVE example: requires a running lunaris-server (which needs a single-shard
Moon behind it). The wire credential is an OPAQUE BEARER TOKEN, not a JWT — the
server maps it to {tenant, scopes} from its --tokens-file, and the partition
scope is the `tenant` claim, resolved server-side.

    export LUNARIS_SERVER_URL=http://127.0.0.1:8080
    export LUNARIS_TOKEN=demo-token
    python main.py

See README.md for the degrade rationale, the Letta-server wiring recipe, and
why Moon v0.8.5 has to be built from source right now.
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
