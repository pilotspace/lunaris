//! Windows portability guard for every source file that compiles into the
//! shipped `lunaris-mcp` binary.
//!
//! `tokio::net::UnixStream` does not exist on Windows targets, and an
//! unconditional `use` made the whole `lunaris-mcp` binary un-buildable on
//! `x86_64-pc-windows-msvc` (mcp-prebuild run 29399540377, 2026-07-15: E0432
//! at proxy.rs:28 plus four cascading E0282s — the ONLY five errors in the
//! run, i.e. the entire dependency graph compiled). A macOS/Linux dev box
//! cannot cross-`cargo check` the msvc target — the native build scripts in
//! the graph (`ring`, `llama-cpp-sys-2`, `aws-lc-sys`) need Windows C/C++
//! headers — so Windows CI is the only true compiler for that target. These
//! structural tests are the local, always-on stand-in.
//!
//! Scope: the production code (everything outside a `#[cfg(test)]` region) of
//! `lunaris-mcp/src/**` and `lunaris-memory-service/src/**` — the two
//! first-party crates whose code lands in the Windows release binary.
//! `cargo build -p lunaris-mcp` does not compile `tests/`, so test sources
//! are deliberately out of scope.

use std::path::{Path, PathBuf};

const PROXY_SRC: &str = include_str!("../src/proxy.rs");

/// Tokens that name an API which simply does not exist on `windows-msvc`.
/// Substring matching is intentional: a fully-qualified
/// `tokio::net::UnixStream::connect(..)` in an expression is exactly as fatal
/// as an ungated `use`, and the 2026-07-15 regression proved a `use`-prefix
/// check alone is too narrow a net.
const UNIX_ONLY_TOKENS: &[&str] = &[
    "UnixStream",
    "UnixListener",
    "UnixDatagram",
    "os::unix",
    "signal::unix",
    "SignalKind",
    "PermissionsExt",
    "CommandExt",
    "AsRawFd",
    "RawFd",
    "libc::",
    "nix::sys",
];

/// One ungated unix-only reference: 1-based line number + the offending line.
type Hit = (usize, String);

/// Is `line` a comment or an inner/outer doc line? Prose about unix sockets is
/// not a compile hazard.
fn is_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("//") || t.starts_with("/*") || t.starts_with('*')
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Does `line` open a `#[cfg(unix)]`-style gate (`cfg(unix)`, `cfg(all(unix,
/// ..))`)? A `cfg(not(unix))` region is NOT a gate — code there compiles only
/// off-unix, so a unix-only token inside it is the bug, not the fix.
fn opens_unix_gate(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("#[cfg(") && t.contains("unix)") && !t.contains("not(unix")
}

/// Does `line` open a `#[cfg(test)]`-style gate? Test code never reaches the
/// Windows release binary. Detected structurally rather than by truncating at
/// the first `#[cfg(test)]` in the file: proxy.rs has a `#[cfg(test)]` helper
/// at line 99, and truncation there would have skipped the entire socket
/// implementation below it — a silent hole in the guard.
fn opens_test_gate(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("#[cfg(") && t.contains("test)") && !t.contains("not(test")
}

/// Lines (0-based) covered by a gate whose opener satisfies `opens`.
///
/// The rule is deterministic for rustfmt-normalised source:
/// * the gated item starts at the first following line that is neither blank,
///   a comment, nor another attribute;
/// * if that line is a `use` statement the region is that single line;
/// * otherwise the region runs to the first line at the gate's own indentation
///   whose trimmed content is `}` — rustfmt puts an item's closing brace at
///   exactly the attribute's indentation.
fn gated_lines(lines: &[&str], opens: fn(&str) -> bool) -> Vec<bool> {
    let mut gated = vec![false; lines.len()];
    for (i, line) in lines.iter().enumerate() {
        if !opens(line) {
            continue;
        }
        let indent = indent_of(line);
        let Some(start) = (i + 1..lines.len()).find(|&j| {
            let t = lines[j].trim();
            !t.is_empty() && !is_comment(lines[j]) && !t.starts_with("#[")
        }) else {
            continue;
        };
        if lines[start].trim().starts_with("use ") {
            gated[start] = true;
            continue;
        }
        let end = (start..lines.len())
            .find(|&k| indent_of(lines[k]) == indent && lines[k].trim() == "}")
            .unwrap_or(lines.len() - 1);
        for g in gated.iter_mut().take(end + 1).skip(start) {
            *g = true;
        }
    }
    gated
}

/// Every unix-only token in the production code of `src` that is not covered
/// by a `#[cfg(unix)]` gate.
fn ungated_unix_hits(src: &str) -> Vec<Hit> {
    let lines: Vec<&str> = src.lines().collect();
    let unix_gated = gated_lines(&lines, opens_unix_gate);
    let test_gated = gated_lines(&lines, opens_test_gate);
    lines
        .iter()
        .enumerate()
        .filter(|(i, line)| {
            !unix_gated[*i]
                && !test_gated[*i]
                && !is_comment(line)
                && UNIX_ONLY_TOKENS.iter().any(|tok| line.contains(tok))
        })
        .map(|(i, line)| (i + 1, (*line).trim().to_owned()))
        .collect()
}

// ── self-tests: the scanner must be discriminating ─────────────────────────

#[test]
fn scanner_accepts_a_gated_import() {
    let src = "#[cfg(unix)]\nuse tokio::net::UnixStream;\n\nfn other() {}\n";
    assert!(ungated_unix_hits(src).is_empty(), "a #[cfg(unix)] import must pass");
}

#[test]
fn scanner_rejects_an_ungated_import() {
    let src = "use tokio::net::UnixStream;\n";
    assert_eq!(ungated_unix_hits(src).len(), 1, "the 2026-07-15 regression must be caught");
}

#[test]
fn scanner_accepts_a_token_inside_a_gated_fn_body() {
    let src = "\
#[cfg(unix)]
async fn try_socket(&self) {
    let s = tokio::net::UnixStream::connect(p).await;
}

fn elsewhere() {}
";
    assert!(ungated_unix_hits(src).is_empty(), "a gated fn body must pass");
}

#[test]
fn scanner_rejects_a_fully_qualified_token_in_an_ungated_fn() {
    let src = "\
async fn try_socket(&self) {
    let s = tokio::net::UnixStream::connect(p).await;
}
";
    assert_eq!(
        ungated_unix_hits(src).len(),
        1,
        "a fully-qualified path is as fatal as a `use`; the old use-prefix guard missed it"
    );
}

#[test]
fn scanner_rejects_a_unix_token_inside_a_not_unix_region() {
    let src = "\
#[cfg(not(unix))]
async fn try_socket(&self) {
    let s = tokio::net::UnixStream::connect(p).await;
}
";
    assert_eq!(
        ungated_unix_hits(src).len(),
        1,
        "cfg(not(unix)) code compiles ONLY off-unix — a unix API there is always broken"
    );
}

#[test]
fn scanner_ignores_prose() {
    let src =
        "//! contextd speaks a UnixStream; see os::unix.\n// UnixListener, libc::\nfn f() {}\n";
    assert!(ungated_unix_hits(src).is_empty(), "comments are not a compile hazard");
}

#[test]
fn scanner_ignores_the_test_module() {
    let src = "fn f() {}\n#[cfg(test)]\nmod tests {\n    use tokio::net::UnixStream;\n}\n";
    assert!(ungated_unix_hits(src).is_empty(), "tests/ and cfg(test) never ship to Windows");
}

/// Regression on the guard itself: excluding test code by truncating at the
/// first `#[cfg(test)]` would blind the scanner to everything below an
/// early test-only helper — proxy.rs has one at line 99, above the entire
/// socket implementation.
#[test]
fn scanner_keeps_scanning_after_an_early_cfg_test_helper() {
    let src = "\
#[cfg(test)]
fn helper_for_tests() -> u8 {
    0
}

use tokio::net::UnixStream;
";
    assert_eq!(
        ungated_unix_hits(src).len(),
        1,
        "production code below a #[cfg(test)] item must still be scanned"
    );
}

// ── the real scan ──────────────────────────────────────────────────────────

fn rust_sources(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let entries =
        std::fs::read_dir(root).unwrap_or_else(|e| panic!("read_dir {}: {e}", root.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            out.extend(rust_sources(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// No source that compiles into the Windows binary may touch a unix-only API
/// outside a `#[cfg(unix)]` gate.
#[test]
fn no_ungated_unix_api_in_the_windows_binary_sources() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let roots = [
        manifest.join("src"),
        // Path dep compiled into the same binary; a unix-only API landing here
        // is byte-identical breakage for the msvc leg.
        manifest.join("../lunaris-memory-service/src"),
    ];

    let mut scanned = 0usize;
    let mut failures = Vec::new();
    for root in roots {
        for file in rust_sources(&root) {
            let src = std::fs::read_to_string(&file).expect("read source");
            scanned += 1;
            for (line, text) in ungated_unix_hits(&src) {
                failures.push(format!("{}:{line}: {text}", file.display()));
            }
        }
    }

    assert!(scanned >= 25, "scanner walked only {scanned} files — the roots moved");
    assert!(
        failures.is_empty(),
        "unix-only APIs outside a #[cfg(unix)] gate break the windows-msvc release leg \
         (mcp-prebuild). Gate them and add a #[cfg(not(unix))] twin:\n  {}",
        failures.join("\n  ")
    );
}

/// Every unix-only import in `proxy.rs` sits directly under `#[cfg(unix)]`.
/// Kept as an explicit, readable pin on the file that actually regressed.
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
            let prev =
                lines[..i].iter().rev().map(|l| l.trim()).find(|l| !l.is_empty()).unwrap_or("");
            assert_eq!(
                prev,
                "#[cfg(unix)]",
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

/// The platform decision must be a single named seam, and a socket the
/// operator explicitly configured on a socket-less target must be surfaced —
/// not silently dropped into Direct-only.
#[test]
fn unsupported_platform_is_reported_loudly() {
    // Everything above `#[cfg(test)] mod tests` — the unit tests name the
    // variant too, and a test mentioning a seam is not the seam.
    let production = PROXY_SRC.split("#[cfg(test)]\nmod tests").next().unwrap_or(PROXY_SRC);
    assert!(
        production.contains("SocketSupport::UnsupportedPlatform"),
        "proxy.rs must classify an explicitly-configured socket on a non-unix \
         target as SocketSupport::UnsupportedPlatform"
    );
    assert!(
        production.contains("tracing::warn!"),
        "the UnsupportedPlatform outcome must warn once — a silent downgrade \
         hides the misconfiguration from the operator"
    );
}
