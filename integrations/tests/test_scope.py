"""RED-first: `namespace_to_scope` mirrors the Lunaris Scope alphabet.

Framework-free. The adapter boundary MUST reject a namespace that maps to a
scope containing ":" (or >128 chars / bad char) BEFORE any ingest/recall, so no
key can byte-alias another scope's partition (RFC 0001 / Scope::new).
"""
from __future__ import annotations

import pytest

from lunaris_integrations.client import InvalidScope, namespace_to_scope


def test_tuple_namespace_joins_with_dot():
    assert namespace_to_scope(("u", "mem")) == "u.mem"
    assert namespace_to_scope(("agent_a",)) == "agent_a"


def test_str_namespace_passes_through_when_valid():
    assert namespace_to_scope("agent_a") == "agent_a"
    assert namespace_to_scope("tenant-1.session_2") == "tenant-1.session_2"


def test_colon_is_rejected():
    # ":" is the KV separator in `lunaris:{scope}:{kind}:{ulid}` — forbidden.
    with pytest.raises(InvalidScope):
        namespace_to_scope(("evil:scope",))
    with pytest.raises(InvalidScope):
        namespace_to_scope("a:b")


def test_bad_char_is_rejected():
    with pytest.raises(InvalidScope):
        namespace_to_scope(("space here",))
    with pytest.raises(InvalidScope):
        namespace_to_scope(("slash/in/ns",))


def test_overlong_is_rejected():
    # Scope alphabet caps at 128 chars.
    with pytest.raises(InvalidScope):
        namespace_to_scope(("x" * 129,))


def test_empty_is_rejected():
    with pytest.raises(InvalidScope):
        namespace_to_scope(())
    with pytest.raises(InvalidScope):
        namespace_to_scope("")
