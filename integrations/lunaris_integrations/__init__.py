"""lunaris_integrations — memory adapters for agent frameworks.

A thin, transport-agnostic layer on top of Lunaris. Every adapter
(LangGraph / CrewAI / Letta) is built over the shared `LunarisClient`
(HTTP MemoryProtocol or in-process lunaris-py SDK), so the adapters stay small
and the framework deps are OPTIONAL extras — never a hard dep of `lunaris` core.

    pip install lunaris-integrations[langgraph]   # or [crewai] / [letta]

The framework adapter modules (`.langgraph`, `.crewai`, `.letta`) import their
framework at module load and are version-guarded; import them only when the
corresponding extra is installed.
"""
from __future__ import annotations

from .client import (
    ConfigError,
    Hit,
    HttpLunarisClient,
    InvalidScope,
    LunarisClient,
    SdkLunarisClient,
    StubLunarisClient,
    UnsupportedFrameworkVersion,
    namespace_to_scope,
    require_base_methods,
)

__all__ = [
    "Hit",
    "LunarisClient",
    "HttpLunarisClient",
    "SdkLunarisClient",
    "StubLunarisClient",
    "ConfigError",
    "InvalidScope",
    "UnsupportedFrameworkVersion",
    "namespace_to_scope",
    "require_base_methods",
]
