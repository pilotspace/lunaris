"""
test_wheel_structure.py — RED/GREEN test for lunaris-mcp PyPI wheel structure.

RED state  : pytest fails with ModuleNotFoundError (package not installed).
GREEN state: pytest passes after `pip install -e crates/lunaris-mcp-py` or
             after the wheel is built and installed locally.

CI: the build-wheel job in mcp-prebuild.yml installs the wheel in a
    temporary venv and runs this test before uploading to PyPI.
"""
import importlib.util
import pathlib
import sys


def test_lunaris_mcp_importable():
    """Package must be importable after install."""
    spec = importlib.util.find_spec("lunaris_mcp")
    assert spec is not None, (
        "lunaris_mcp not found. Install with: pip install -e crates/lunaris-mcp-py"
    )


def test_main_module_exists():
    """__main__.py must be present so `python -m lunaris_mcp` and uvx work."""
    spec = importlib.util.find_spec("lunaris_mcp.__main__")
    assert spec is not None, (
        "lunaris_mcp.__main__ not found — __main__.py missing from the package."
    )


def test_version_is_set():
    """__version__ must be a non-empty string matching semver shape."""
    import re
    import lunaris_mcp  # noqa: PLC0415
    assert hasattr(lunaris_mcp, "__version__"), "__version__ missing from lunaris_mcp"
    assert isinstance(lunaris_mcp.__version__, str), "__version__ must be a string"
    assert re.match(r"^\d+\.\d+\.\d+", lunaris_mcp.__version__), (
        f"__version__ does not look like semver: {lunaris_mcp.__version__!r}"
    )


def test_bin_directory_exists_in_package():
    """The bin/ directory must be present (populated by build-wheel CI or maturin develop)."""
    import lunaris_mcp  # noqa: PLC0415
    pkg_dir = pathlib.Path(lunaris_mcp.__file__).parent
    bin_dir = pkg_dir / "bin"
    assert bin_dir.is_dir(), (
        f"bin/ directory not found at {bin_dir}. "
        "In CI, the build-wheel job extracts the binary before building the wheel. "
        "For local dev, copy the binary manually: cp target/.../lunaris-mcp "
        "crates/lunaris-mcp-py/python/lunaris_mcp/bin/"
    )
