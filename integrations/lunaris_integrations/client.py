"""Shared `LunarisClient` layer for the framework adapters.

This is the ONE seam every adapter (LangGraph / CrewAI / Letta) talks to, so
each adapter stays ~tens of LOC and transport-agnostic. Two production
transports + a test double, all satisfying the same `LunarisClient` Protocol:

- `HttpLunarisClient` — talks the MemoryProtocol HTTP verbs on lunaris-server
  (`POST /v1/ingest`, `POST /v1/recall`, `POST /v1/forget`) with a Bearer JWT.
  The partition scope is bound to the JWT on the server side, so it NEVER
  travels on the wire (honors the server's scope discipline).
- `SdkLunarisClient` — wraps the in-process lunaris-py handle. `lunaris` is
  imported LAZILY so this module (and the whole unit-test layer) loads without
  the compiled cdylib / a backend.
- `StubLunarisClient` — records calls + returns canned hits; the unit layer
  runs against this with no backend, model, or wheel.

Frozen contract: `.add/tasks/sdk-integrations-dx/TASK.md` §3.
"""
from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Iterable, Protocol, runtime_checkable

# RFC 0001 Scope alphabet — must match `lunaris_core::scope::Scope::new`
# (`[A-Za-z0-9_\-.]{1,128}`). `:` is rejected so an adapter can never mint a
# scope string that byte-aliases another partition in `lunaris:{scope}:...`.
_SCOPE_RE = re.compile(r"^[A-Za-z0-9_\-.]{1,128}$")


@dataclass
class Hit:
    """One recalled item — the adapter-neutral shape every transport returns."""

    id: str
    content: str
    score: float
    source: str | None = None


# ── Exceptions ────────────────────────────────────────────────────────────────
class ConfigError(Exception):
    """A transport was constructed without the config it needs to ever work
    (e.g. an HTTP client with no base URL / an SDK client with no handle).
    Raised AT CONSTRUCTION so no request is attempted at first use."""


class InvalidScope(Exception):
    """A namespace mapped to a scope outside the Lunaris alphabet."""

    def __init__(self, value: object) -> None:
        self.value = value
        super().__init__(f"invalid_scope: {value!r}")


class UnsupportedFrameworkVersion(Exception):
    """The installed framework's base class/shape does not match the pinned
    shape the adapter was written against — fail loud, never mis-map."""

    def __init__(self, framework: str, found: object, expected: str) -> None:
        self.framework = framework
        self.found = found
        self.expected = expected
        super().__init__(
            f"unsupported {framework} version: found {found!r}, expected {expected}"
        )


# ── Scope mapping ───────────────────────────────────────────────────────────
def namespace_to_scope(ns: tuple[str, ...] | str) -> str:
    """Map a framework namespace to a validated Lunaris scope string.

    A tuple namespace (`("u", "mem")`) joins with `.` → `"u.mem"`. The result
    is validated against the Scope alphabet; `:` / bad char / empty / >128
    chars raise `InvalidScope` BEFORE any client call.
    """
    if isinstance(ns, str):
        scope = ns
    else:
        parts = tuple(ns)
        if not parts:
            raise InvalidScope(ns)
        scope = ".".join(str(p) for p in parts)
    if not _SCOPE_RE.match(scope):
        raise InvalidScope(scope)
    return scope


def require_base_methods(
    base_cls: type,
    required: Iterable[str],
    *,
    framework: str,
    found: object,
    expected: str,
) -> None:
    """Version-guard: raise `UnsupportedFrameworkVersion` if `base_cls` is
    missing any required method. Called by each adapter at import/instantiation
    so framework drift fails loud instead of silently mis-mapping."""
    missing = sorted(m for m in required if not hasattr(base_cls, m))
    if missing:
        raise UnsupportedFrameworkVersion(framework, found, expected)


# ── The client Protocol ─────────────────────────────────────────────────────
@runtime_checkable
class LunarisClient(Protocol):
    """The single interface every adapter depends on. Scope-bound at
    construction; the adapter supplies content + query, never a transport."""

    scope: str

    async def ingest(
        self, source: str, content: str, metadata: dict | None = None
    ) -> str: ...

    async def recall(self, query: str, k: int = 10) -> list[Hit]: ...

    async def forget_scope(self) -> None: ...


# ── HTTP transport ──────────────────────────────────────────────────────────
class HttpLunarisClient:
    """MemoryProtocol-over-HTTP client. Scope is JWT-bound server-side and is
    NEVER placed on the wire."""

    def __init__(
        self, base_url: str, token: str, scope: str, *, timeout: float = 10.0
    ) -> None:
        if not base_url:
            raise ConfigError("HttpLunarisClient requires a base_url")
        if not token:
            raise ConfigError("HttpLunarisClient requires a Bearer token")
        # Validate the scope at the boundary (no `:` byte-aliasing).
        self.scope = namespace_to_scope(scope)
        self._base_url = base_url.rstrip("/")
        # httpx is a CORE dep of lunaris_integrations (not an optional extra).
        import httpx

        self._client = httpx.AsyncClient(
            base_url=self._base_url,
            headers={"Authorization": f"Bearer {token}"},
            timeout=timeout,
        )

    async def ingest(
        self, source: str, content: str, metadata: dict | None = None
    ) -> str:
        # `IngestBody.metadata` is a JSON object with `#[serde(default)]` — send
        # `{}` (NEVER null) so `deny_unknown_fields` + the Map typing accept it.
        resp = await self._client.post(
            "/v1/ingest",
            json={"source": source, "content": content, "metadata": metadata or {}},
        )
        resp.raise_for_status()
        return _lsn_to_str(resp.json()["lsn"])

    async def recall(self, query: str, k: int = 10) -> list[Hit]:
        resp = await self._client.post("/v1/recall", json={"query": query, "k": k})
        resp.raise_for_status()
        # `/v1/recall` returns a BARE JSON array of hits (`Json(Vec<Hit>)`), not
        # an object with a "hits" key.
        return [_hit_from_dict(h) for h in resp.json()]

    async def forget_scope(self) -> None:
        # Soft-purge the whole JWT-bound scope: `ForgetTarget::Scope` with an
        # empty-prefix `BySource` match (matches every source in the partition).
        # `hard=false` so the D-21 dry_run→confirm 2-step is NOT required; the
        # partition scope itself stays JWT-bound server-side.
        resp = await self._client.post(
            "/v1/forget",
            json={
                "target": {"Scope": {"BySource": ""}},
                "hard": False,
                "dry_run": False,
            },
        )
        resp.raise_for_status()

    async def aclose(self) -> None:
        await self._client.aclose()


# ── In-process SDK transport ────────────────────────────────────────────────
class SdkLunarisClient:
    """Wraps the in-process lunaris-py handle (`open(url)`); `lunaris` is
    imported lazily so this module loads without the compiled wheel. The live
    path is exercised by the example + HUMAN-UAT (needs the wheel + a backend
    per the py/ts SDK test caveat)."""

    def __init__(self, handle: object, scope: str) -> None:
        if handle is None:
            raise ConfigError("SdkLunarisClient requires an open lunaris handle")
        self.scope = namespace_to_scope(scope)
        self._handle = handle

    def _scoped(self):
        import lunaris  # lazy — only the SDK transport needs the cdylib

        return self._handle.scoped(lunaris.Scope(self.scope))

    async def ingest(
        self, source: str, content: str, metadata: dict | None = None
    ) -> str:
        import lunaris

        builder = lunaris.EpisodeBuilder(source, content)
        if metadata:
            builder = builder.metadata(metadata)
        lsn = await self._handle.scoped(lunaris.Scope(self.scope)).ingest(builder)
        return str(lsn)

    async def recall(self, query: str, k: int = 10) -> list[Hit]:
        # `scoped.recall` returns pythonized hit dicts (scope-threaded). k is
        # applied as a client-side cap; the DSL `.top(k)` path is available via
        # the example for operators who need server-side top-k.
        hits = await self._scoped().recall(query)
        return [_hit_from_dict(h) for h in list(hits)[:k]]

    async def forget_scope(self) -> None:
        # The in-process lunaris-py binding does not expose scope-forget on
        # ScopedLunaris (only ingest/recall/dsl). Use the HTTP transport for
        # scope-clear / CrewAI reset(), or call the server's /v1/forget.
        raise NotImplementedError(
            "scope-forget is not exposed by the in-process lunaris-py binding; "
            "use HttpLunarisClient for forget_scope / CrewAI reset()"
        )


# ── Test double ───────────────────────────────────────────────────────────-─
class StubLunarisClient:
    """Records every call + returns canned hits. The unit layer runs against
    this — no backend, model, or wheel."""

    def __init__(self, scope: str, hits: list[Hit] | None = None) -> None:
        self.scope = scope
        self._hits = hits or []
        self.ingest_calls: list[tuple[str, str, dict | None]] = []
        self.recall_calls: list[tuple[str, int]] = []
        self.forget_calls = 0

    async def ingest(
        self, source: str, content: str, metadata: dict | None = None
    ) -> str:
        self.ingest_calls.append((source, content, metadata))
        return f"stub-lsn-{len(self.ingest_calls)}"

    async def recall(self, query: str, k: int = 10) -> list[Hit]:
        self.recall_calls.append((query, k))
        return list(self._hits[:k])

    async def forget_scope(self) -> None:
        self.forget_calls += 1


def _hit_from_dict(d: dict) -> Hit:
    """Map a `lunaris_retrieve::Hit` JSON object to the adapter-neutral `Hit`.

    The server/SDK hit carries its body in `text` and its id as a byte array
    (16-byte ULID `Vec<u8>`); `content` is also accepted for forward-compat.
    """
    return Hit(
        id=_decode_id(d.get("id", "")),
        content=str(d.get("content", d.get("text", ""))),
        score=float(d.get("score", 0.0)),
        source=(d.get("source") or None),
    )


def _decode_id(raw: object) -> str:
    """`Hit.id` serializes as a byte array (`Vec<u8>`, 16-byte ULID). Hex-encode
    it to a stable string id; pass a plain string through unchanged."""
    if isinstance(raw, (list, tuple)):
        try:
            return bytes(int(b) & 0xFF for b in raw).hex()
        except (TypeError, ValueError):
            return ""
    return str(raw)


def _lsn_to_str(raw: object) -> str:
    """`IngestResponse.lsn` serializes as `{"wall_ms":u64,"counter":u32}`.
    Format it as the canonical `"{wall_ms}:{counter}"` (matches `Lsn` Display);
    accept a plain string/number too."""
    if isinstance(raw, dict) and "wall_ms" in raw:
        return f"{raw['wall_ms']}:{raw.get('counter', 0)}"
    return str(raw)
