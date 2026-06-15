"""Pytest conftest for the `lunaris_integrations` adapter suite.

The adapter package lives at `integrations/lunaris_integrations/` and is a
PURE-Python package (no native cdylib) so this unit layer runs without the
maturin wheel, a Moon backend, or any model — per the frozen contract
(`SdkLunarisClient` imports `lunaris` lazily; the framework deps are optional
extras gated with `pytest.importorskip`).

Two job:
1. Put `integrations/` on `sys.path` so `import lunaris_integrations` resolves
   from a plain checkout (no editable install needed for the unit layer).
2. Expose a `repo_root` fixture for the doc-lint test.
"""
from __future__ import annotations

import sys
from pathlib import Path

import pytest

# integrations/tests/conftest.py → parent.parent == integrations/
_INTEGRATIONS_DIR = Path(__file__).resolve().parent.parent
if str(_INTEGRATIONS_DIR) not in sys.path:
    sys.path.insert(0, str(_INTEGRATIONS_DIR))

# repo root == integrations/.. (the lunaris checkout root)
_REPO_ROOT = _INTEGRATIONS_DIR.parent


@pytest.fixture
def repo_root() -> Path:
    """Absolute path to the lunaris repository root (for doc-lint reads)."""
    return _REPO_ROOT
