//! No test may probe a live fixture by trying only the FIRST resolved address.
//!
//! ## The defect this pins
//!
//! Five test files grew a byte-similar TCP probe that resolved a host and then
//! took `to_socket_addrs().next()`. A hostname routinely resolves to several
//! addresses, and nothing requires the first to be the one the server bound.
//!
//! `.github/workflows/integration.yml` sets `MOON_URL: moon://localhost:6390`
//! and launches Moon with no `--bind`, so Moon takes its `127.0.0.1` default
//! (`vendor/moon/src/config.rs`) — IPv4 only. On a GitHub runner `localhost`
//! also resolves to `::1`. The job's wait-step probes with bash `/dev/tcp`,
//! which walks the whole list and connects; these probes tried one address and
//! reported "no fixture". Four live suites therefore skipped inside the very
//! job that had built, port-checked and guaranteed their Moon — and before F27
//! routed their skips, they skipped in silence.
//!
//! A guard against re-growing the copy is the fix for the CAUSE. The single
//! implementation is `lunaris_test_harness::live_probe`, whose
//! `a_dead_first_address_does_not_hide_a_live_second_one` proves it walks the
//! list; this sweep proves nobody reintroduces a private one that does not.

use std::path::{Path, PathBuf};

/// Walk every `.rs` under `crates/*/tests/` and `crates/*/src/`, skipping the
/// harness's own `live_probe` (the one legitimate implementation) and this
/// file (which quotes the pattern it forbids).
fn rust_sources(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.join("crates")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if name != "target" && name != "node_modules" {
                    stack.push(p);
                }
            } else if p.extension().and_then(|s| s.to_str()) == Some("rs") {
                out.push(p);
            }
        }
    }
    out
}

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().and_then(|p| p.parent()).expect("workspace root").to_path_buf()
}

/// Strip line comments, so prose that QUOTES the forbidden pattern — including
/// the doc comments on the replacements — is not read as code.
///
/// A detector that cannot tell code from commentary produces false positives
/// forever, and a guard that cries wolf gets an allow-list bolted on until it
/// detects nothing. Quote state is tracked so a `//` inside a string literal
/// (`"moon://host"`) does not truncate the rest of a line that might carry a
/// real offender.
fn strip_line_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.lines() {
        let bytes = line.as_bytes();
        let mut in_str = false;
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' if in_str => i += 1, // skip the escaped byte
                b'"' => in_str = !in_str,
                b'/' if !in_str && bytes.get(i + 1) == Some(&b'/') => break,
                _ => {}
            }
            i += 1;
        }
        out.push_str(&line[..i.min(line.len())]);
        out.push('\n');
    }
    out
}

/// Does `src` resolve addresses and then consume only the first?
///
/// Whitespace-normalised, because the fifth site was split across four lines
/// and a single-line grep walked straight past it — the same "keyed on one
/// phrasing" trap the F27 sweep was written to avoid.
fn takes_only_the_first_address(src: &str) -> bool {
    let code = strip_line_comments(src);
    let flat: String = code.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut from = 0;
    while let Some(rel) = flat[from..].find("to_socket_addrs") {
        let at = from + rel;
        // Look only as far as the end of the expression. A `.next()` beyond a
        // `;` belongs to some other statement.
        let tail = &flat[at..];
        let end = tail.find(';').unwrap_or(tail.len());
        let expr = &tail[..end];
        if expr.contains(". next ()") || expr.contains(".next()") {
            return true;
        }
        from = at + "to_socket_addrs".len();
    }
    false
}

#[test]
fn no_probe_gives_up_after_one_address() {
    let root = workspace_root();
    let allow = ["live_probe.rs", "no_first_address_only_probe.rs"];
    let mut offenders = Vec::new();
    let mut scanned = 0usize;
    for path in rust_sources(&root) {
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if allow.contains(&name) {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&path) else { continue };
        scanned += 1;
        if takes_only_the_first_address(&src) {
            offenders.push(path.strip_prefix(&root).unwrap_or(&path).display().to_string());
        }
    }
    // The sweep's own vacuity floor. A walk that found nothing reports the
    // same clean board as a workspace with no offenders — and a broken walk
    // (wrong root, an over-eager skip) is exactly how such a guard rots.
    assert!(
        scanned > 400,
        "the sweep scanned only {scanned} files; the walk is broken, not the workspace clean"
    );
    assert!(
        offenders.is_empty(),
        "these files resolve a host and try only the first address, so a fixture bound to the \
         second one reads as absent:\n  {}\nUse lunaris_test_harness::live_probe instead — it \
         tries every address, and announces the ones it tried when none answer.",
        offenders.join("\n  ")
    );
}

/// The vacuity floor. A detector that never matches would pass the sweep above
/// while the whole workspace re-grew the defect.
#[test]
fn the_detector_recognises_the_shape_it_forbids() {
    // The exact expression that was in four files, on one line and split.
    assert!(takes_only_the_first_address(
        "let Some(addr) = host_port.to_socket_addrs().ok().and_then(|mut it| it.next()) else {"
    ));
    assert!(takes_only_the_first_address(
        "x\n  .to_socket_addrs()\n  .ok()\n  .and_then(|mut it| it.next())\n  .map(|a| go(a));"
    ));
    // And the correct forms must NOT trip it, or the guard is unusable.
    assert!(!takes_only_the_first_address(
        "for addr in host_port.to_socket_addrs()? { probe(addr) }"
    ));
    assert!(!takes_only_the_first_address(
        "let addrs: Vec<_> = host_port.to_socket_addrs()?.collect();\nlet x = it.next();"
    ));
    // Prose describing the defect is not the defect. Every replacement's doc
    // comment quotes the pattern it removed; reading those as code is how this
    // guard first went red against five already-fixed files.
    assert!(!takes_only_the_first_address(
        "/// The local copy took `to_socket_addrs().next()`, so it gave up early.\nfn f() {}"
    ));
    // But a real offender sharing a line with a URL string still trips it —
    // the `//` in `moon://` must not truncate the scan.
    assert!(takes_only_the_first_address(
        "let a = \"moon://h:1\"; let b = h.to_socket_addrs().ok().and_then(|mut i| i.next());"
    ));
}
