//! Windows portability guard for the contextd socket proxy.
//!
//! `tokio::net::UnixStream` does not exist on Windows targets, and an
//! unconditional `use` made the whole `lunaris-mcp` binary un-buildable on
//! `x86_64-pc-windows-msvc` (mcp-prebuild `binary` job red since 2026-07-15;
//! E0432 at proxy.rs:28). A macOS/Linux dev box cannot cross-`cargo check`
//! the msvc target (aws-lc-sys build scripts need a Windows C toolchain), so
//! CI is the only true compiler for that target — this structural test is
//! the local, always-on stand-in: it pins that every unix-socket item in
//! `proxy.rs` is `#[cfg(unix)]`-gated and that a `#[cfg(not(unix))]`
//! fallback exists, so the unconditional-import regression cannot come back
//! silently between Windows CI runs.

const PROXY_SRC: &str = include_str!("../src/proxy.rs");

/// Every `use tokio::net::UnixStream` (and the io-ext import that only the
/// socket path needs) must sit directly under a `#[cfg(unix)]` attribute.
#[test]
fn unix_socket_imports_are_cfg_gated() {
    let lines: Vec<&str> = PROXY_SRC.lines().collect();
    let mut found = 0usize;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t.starts_with("use tokio::net::UnixStream")
            || t.starts_with("use tokio::io::{AsyncReadExt")
        {
            found += 1;
            let prev = lines[..i]
                .iter()
                .rev()
                .map(|l| l.trim())
                .find(|l| !l.is_empty())
                .unwrap_or("");
            assert_eq!(
                prev, "#[cfg(unix)]",
                "unix-only import at proxy.rs line {} must be #[cfg(unix)]-gated \
                 (Windows E0432 regression): `{t}`",
                i + 1
            );
        }
    }
    assert!(found >= 2, "expected the two unix-only tokio imports in proxy.rs; found {found}");
}

/// The socket leg must have a non-unix fallback so `dispatch` still compiles
/// (and routes Direct) on Windows.
#[test]
fn non_unix_fallback_exists() {
    assert!(
        PROXY_SRC.contains("#[cfg(not(unix))]"),
        "proxy.rs needs a #[cfg(not(unix))] try_socket fallback returning a \
         transport error so Windows builds route Direct-only"
    );
}
