"""RED-first: the shared `LunarisClient` layer (frozen contract §3).

These tests are FRAMEWORK-FREE — they must run anywhere (no langgraph/crewai/
letta, no maturin wheel, no backend). At RED they fail with ModuleNotFoundError
because `lunaris_integrations` does not exist yet; that is the right reason.
"""
from __future__ import annotations

import asyncio
import json

import httpx
import pytest
import respx

from lunaris_integrations.client import (
    ConfigError,
    Hit,
    HttpLunarisClient,
    LunarisClient,
    SdkLunarisClient,
    StubLunarisClient,
    UnsupportedFrameworkVersion,
    require_base_methods,
)


def _run(coro):
    return asyncio.run(coro)


# ── Scenario: HttpLunarisClient round-trips the MemoryProtocol verbs ──────────
def test_http_client_roundtrip():
    async def body():
        with respx.mock(base_url="http://srv", assert_all_called=True) as router:
            # Real server shapes (verified against lunaris-server):
            #  - IngestResponse.lsn → {"wall_ms":u64,"counter":u32} (+ queue_lag_warn)
            #  - /v1/recall → a BARE ARRAY of Hit; body field is `text`, id is a
            #    16-byte ULID byte array.
            ingest = router.post("/v1/ingest").mock(
                return_value=httpx.Response(
                    200,
                    json={
                        "lsn": {"wall_ms": 1746000000000, "counter": 7},
                        "queue_lag_warn": False,
                    },
                )
            )
            recall = router.post("/v1/recall").mock(
                return_value=httpx.Response(
                    200,
                    json=[
                        {
                            "id": list(range(16)),
                            "score": 0.9,
                            "text": "Alice joined Acme",
                            "source": "chat",
                            "heading_path": [],
                            "valid_to": None,
                            "degraded": False,
                            "rerank_applied": False,
                        }
                    ],
                )
            )
            client = HttpLunarisClient(
                base_url="http://srv", token="jwt-abc", scope="agent_a"
            )
            lsn = await client.ingest("chat", "Alice joined Acme", {"k": "v"})
            hits = await client.recall("where did Alice join", k=5)

        assert lsn == "1746000000000:7"
        assert len(hits) == 1 and isinstance(hits[0], Hit)
        assert hits[0].content == "Alice joined Acme"
        assert hits[0].id == bytes(range(16)).hex()
        assert hits[0].score == 0.9

        sent_ingest = json.loads(ingest.calls.last.request.content)
        # scope is JWT-bound — it MUST NOT travel on the wire.
        assert "scope" not in sent_ingest and "tenant" not in sent_ingest
        assert sent_ingest["source"] == "chat"
        assert sent_ingest["content"] == "Alice joined Acme"
        assert sent_ingest["metadata"] == {"k": "v"}
        # Bearer token rides in the Authorization header, not the body.
        assert ingest.calls.last.request.headers["authorization"] == "Bearer jwt-abc"

        sent_recall = json.loads(recall.calls.last.request.content)
        assert sent_recall == {"query": "where did Alice join", "k": 5}

    _run(body())


# ── Scenario: missing transport config fails at construction ──────────────────
def test_missing_config_errors():
    with pytest.raises(ConfigError):
        HttpLunarisClient(base_url="", token="jwt", scope="agent_a")
    with pytest.raises(ConfigError):
        HttpLunarisClient(base_url="http://srv", token="", scope="agent_a")
    with pytest.raises(ConfigError):
        SdkLunarisClient(handle=None, scope="agent_a")
    # No request is attempted at first use because construction already failed.


# ── Scenario: the SAME adapter is transport-agnostic (surface half) ───────────
# The full "same LunarisStore over two transports records identical shapes"
# proof lives in test_langgraph.py (needs langgraph). This framework-free half
# pins the precondition: ALL three client impls satisfy the ONE LunarisClient
# Protocol, so an adapter can hold any of them without branching on transport.
def test_clients_share_protocol():
    stub = StubLunarisClient(scope="agent_a")
    http = HttpLunarisClient(base_url="http://srv", token="jwt", scope="agent_a")
    sdk = SdkLunarisClient(handle=object(), scope="agent_a")  # dummy non-None handle
    for impl in (stub, http, sdk):
        assert isinstance(impl, LunarisClient)
        assert impl.scope == "agent_a"


# ── Scenario: framework API drift is rejected, not mis-mapped ─────────────────
def test_framework_drift_rejected():
    class Bogus:  # missing the methods a real BaseStore declares
        pass

    with pytest.raises(UnsupportedFrameworkVersion):
        require_base_methods(
            Bogus,
            {"aput", "aget", "asearch"},
            framework="langgraph",
            found="0.0.0",
            expected=">=0.2",
        )


def test_stub_records_calls():
    stub = StubLunarisClient(scope="agent_a", hits=[Hit("1", "x", 1.0, "s")])

    async def body():
        lsn = await stub.ingest("chat", "hello", {"m": 1})
        hits = await stub.recall("q", k=7)
        await stub.forget_scope()
        return lsn, hits

    lsn, hits = _run(body())
    assert stub.ingest_calls == [("chat", "hello", {"m": 1})]
    assert stub.recall_calls == [("q", 7)]
    assert stub.forget_calls == 1
    assert lsn and hits and hits[0].content == "x"
