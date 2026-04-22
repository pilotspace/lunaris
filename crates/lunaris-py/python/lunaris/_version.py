"""Version info — mirrors `Cargo.toml` + `pyproject.toml`.

The canonical runtime value is `lunaris.__version__`, injected by the Rust
cdylib via `m.setattr("__version__", env!("CARGO_PKG_VERSION"))` in
`src/lib.rs`. This file exists so Python tools (setuptools legacy, static
version readers) have a string-literal fallback to pick up.
"""
__version__ = "0.1.1"
