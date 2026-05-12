//! `cargo xtask check-scope` — grep-pin guard for `Scope::dev()` leaks.
//!
//! RFC 0001 / P0 #1 Wave 2 invariant: `Scope::dev()` is a migration crutch
//! (`#[doc(hidden)] pub` on `lunaris_core::Scope`). Any production call site
//! that mints `Scope::dev()` MUST carry a `// scope-dev-allowed: <reason>`
//! marker on the same line or on the call-site's leading comment. CI runs
//! this scanner to fail loud when a new `Scope::dev()` appears without the
//! marker.
//!
//! ### Scanner design (text-mode, no `syn`)
//!
//! For each `.rs` file under `crates/`:
//! 1. Skip files whose path matches the static allow-list (test/bench
//!    crates, the `Scope` impl itself, SDK conformance shims, tests/ and
//!    benches/ directories, doc-comment examples are filtered separately).
//! 2. Walk the file line-by-line tracking brace depth inside a
//!    `#[cfg(test)] mod ... { ... }` region; lines inside that region are
//!    test-module bodies and skipped wholesale.
//! 3. Drop comment-only lines (`^\s*//`).
//! 4. For every remaining line containing `Scope::dev()`:
//!    - If the line carries `scope-dev-allowed`, accept.
//!    - Else, if the immediately preceding non-blank line carries
//!      `scope-dev-allowed`, accept (multi-line call sites).
//!    - Else, flag as a violation.
//!
//! Exit 0 with violation count 0 = clean. Exit 1 with the offending
//! file:line:line-content for every violation otherwise. Owner: invariant
//! documented in `CLAUDE.md` "Scope (RFC 0001)" section.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// One un-marked `Scope::dev()` occurrence outside of allowed locations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub file: PathBuf,
    pub line: usize,
    pub content: String,
}

/// File-path allow-list. Paths matching any of these substrings are
/// scanned for nothing — the call site is by convention test/bench/SDK-shim
/// code where `Scope::dev()` is the documented default.
const PATH_ALLOWLIST: &[&str] = &[
    "/tests/",
    "/benches/",
    "crates/lunaris-bench/",
    "crates/lunaris-conformance/",
    // The Scope impl itself defines `Scope::dev()` and exercises it in unit
    // tests inside the same file.
    "crates/lunaris-core/src/scope.rs",
    // SDK conformance bridges expose `Scope::dev()` to PyO3/napi test
    // harnesses. They are test-only entry points despite living under
    // `src/`. Markers (`// scope-dev-allowed: sdk-conformance-bridge`) are
    // applied to the individual call sites as a belt-and-braces signal,
    // but the path is still allow-listed to keep the scanner robust to
    // future bridge additions.
    "crates/lunaris-py/src/conformance.rs",
    "crates/lunaris-ts/src/conformance.rs",
];

/// Allow-list marker token. Any line containing this substring is
/// considered an intentional, audited `Scope::dev()` site.
pub const ALLOW_MARKER: &str = "scope-dev-allowed";

/// Walk `crates/` and return every un-marked `Scope::dev()` site.
pub fn scan_workspace(workspace_root: &Path) -> Result<Vec<Violation>> {
    let crates_dir = workspace_root.join("crates");
    let mut out = Vec::new();
    visit_dir(&crates_dir, &mut out)?;
    out.sort_by(|a, b| (a.file.clone(), a.line).cmp(&(b.file.clone(), b.line)));
    Ok(out)
}

fn visit_dir(dir: &Path, out: &mut Vec<Violation>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            visit_dir(&path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            scan_file(&path, out)?;
        }
    }
    Ok(())
}

fn scan_file(path: &Path, out: &mut Vec<Violation>) -> Result<()> {
    let path_str = path.to_string_lossy();
    if PATH_ALLOWLIST.iter().any(|p| path_str.contains(p)) {
        return Ok(());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("read_to_string {}", path.display()))?;
    out.extend(scan_text(path, &content));
    Ok(())
}

/// Core text-mode scanner. Public so the integration test can exercise it
/// without touching the filesystem.
pub fn scan_text(path: &Path, content: &str) -> Vec<Violation> {
    let lines: Vec<&str> = content.lines().collect();
    let test_ranges = compute_test_module_ranges(&lines);
    let mut out: Vec<Violation> = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        if test_ranges.iter().any(|(lo, hi)| idx >= *lo && idx <= *hi) {
            continue;
        }
        // Comment-only line — even with `Scope::dev()` in a doc comment or
        // RFC reference, it can't be a call site.
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        if line.contains("Scope::dev()") {
            let marked_here = line.contains(ALLOW_MARKER);
            // Multi-line call-site support: the marker may appear on a
            // leading comment line above the call. Walk back across the
            // current statement (`storage\n    .atomic_write(...)` is one
            // statement) until we hit a comment with the marker, OR cross
            // a statement boundary (`;`/`{`/`}` at line end ignoring
            // trailing comments), OR reach the top of the file. Sibling
            // call sites are separated by a statement boundary so their
            // markers cannot leak into this one.
            let mut marked_prev = false;
            for back in (0..idx).rev() {
                let prev_raw = lines[back];
                let prev = prev_raw.trim();
                if prev.is_empty() {
                    continue;
                }
                if prev.starts_with("//") {
                    if prev.contains(ALLOW_MARKER) {
                        marked_prev = true;
                    }
                    // Comment line — keep walking up only if no marker
                    // yet (allow stacked comment lines above the marker).
                    if marked_prev {
                        break;
                    }
                    continue;
                }
                // Statement-boundary detection: strip trailing line
                // comment, then check the last meaningful char.
                let body = match prev.find("//") {
                    Some(i) => prev[..i].trim_end(),
                    None => prev.trim_end(),
                };
                if let Some(last) = body.chars().last()
                    && (last == ';' || last == '{' || last == '}')
                {
                    break;
                }
                // Otherwise — continuation line of the same statement.
                // Keep walking up.
            }
            if !marked_here && !marked_prev {
                out.push(Violation {
                    file: path.to_path_buf(),
                    line: idx + 1,
                    content: line.to_string(),
                });
            }
        }
    }
    out
}

/// Identify every line-index range (inclusive lo, inclusive hi) covered by
/// a `#[cfg(test)] mod ... { ... }` block. The scanner is text-mode so it
/// uses brace counting on bytes outside of strings/chars — that's enough
/// for the well-formed Rust files this scanner targets. False positives
/// here would manifest as a missed violation, not a phantom one (we
/// over-skip rather than over-flag).
fn compute_test_module_ranges(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_start();
        if line.starts_with("#[cfg(test)]") {
            // Find the next `mod ... {` opener — typically on the next
            // line or two. Tolerate intervening blank lines / attributes.
            let mut j = i + 1;
            while j < lines.len() {
                let next = lines[j].trim_start();
                if next.is_empty() || next.starts_with('#') || next.starts_with("//") {
                    j += 1;
                    continue;
                }
                // Now `next` is the first meaningful line after the attr.
                // Must be `mod ` or `pub mod ` etc.
                if next.contains("mod ")
                    && lines[j].contains('{')
                    && let Some(end) = find_matching_brace(lines, j)
                {
                    ranges.push((i, end));
                    i = end + 1;
                    break;
                }
                // Not a test module — treat the attribute as something else
                // (`#[cfg(test)] use ...`, function-level attr). Stop the
                // attribute search and resume linearly.
                break;
            }
        }
        i += 1;
    }
    ranges
}

/// Brace-match starting at `start_line` (line containing the opening `{`
/// of a `mod tests { ... }`). Returns the line index of the line
/// containing the matching `}`. Naive char-by-char count ignoring strings
/// and chars — sufficient for the well-formed Rust files this scanner
/// targets.
fn find_matching_brace(lines: &[&str], start_line: usize) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut seen_open = false;
    for (li, line) in lines.iter().enumerate().skip(start_line) {
        let mut chars = line.chars().peekable();
        let mut in_line_comment = false;
        while let Some(c) = chars.next() {
            if in_line_comment {
                break;
            }
            match c {
                '/' if chars.peek() == Some(&'/') => {
                    in_line_comment = true;
                }
                '{' => {
                    depth += 1;
                    seen_open = true;
                }
                '}' => {
                    depth -= 1;
                    if seen_open && depth == 0 {
                        return Some(li);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

pub fn cmd_check_scope(workspace_root: &Path) -> Result<()> {
    let violations = scan_workspace(workspace_root)?;
    if violations.is_empty() {
        println!("xtask check-scope: OK — no un-marked Scope::dev() leaks under crates/");
        return Ok(());
    }
    eprintln!(
        "xtask check-scope: FAIL — {} un-marked `Scope::dev()` site(s) under crates/",
        violations.len()
    );
    for v in &violations {
        eprintln!("  {}:{}: {}", v.file.display(), v.line, v.content.trim());
    }
    eprintln!();
    eprintln!(
        "Each production call site of `Scope::dev()` MUST carry a `// scope-dev-allowed: <reason>` marker"
    );
    eprintln!(
        "on the same line or on the leading comment line. See CLAUDE.md §Conventions / RFC 0001."
    );
    anyhow::bail!("check-scope: {} violation(s)", violations.len());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn flags_unmarked_production_site() {
        let src = r#"
fn route() {
    storage.atomic_write(&Scope::dev(), &ops).await;
}
"#;
        let v = scan_text(&PathBuf::from("crates/foo/src/lib.rs"), src);
        assert_eq!(v.len(), 1, "expected one violation; got {v:#?}");
        assert_eq!(v[0].line, 3);
    }

    #[test]
    fn accepts_same_line_marker() {
        let src = r#"
fn route() {
    storage.atomic_write(&Scope::dev(), &ops).await; // scope-dev-allowed: reason
}
"#;
        let v = scan_text(&PathBuf::from("crates/foo/src/lib.rs"), src);
        assert!(v.is_empty(), "expected no violation; got {v:#?}");
    }

    #[test]
    fn accepts_preceding_marker_for_multiline_calls() {
        let src = r#"
fn route() {
    // scope-dev-allowed: documented reason here
    storage
        .atomic_write(&Scope::dev(), &ops)
        .await;
}
"#;
        let v = scan_text(&PathBuf::from("crates/foo/src/lib.rs"), src);
        assert!(v.is_empty(), "expected no violation; got {v:#?}");
    }

    #[test]
    fn skips_inline_test_module() {
        let src = r#"
fn prod() {
    storage.write(&Scope::dev(), &ops); // scope-dev-allowed: reason
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn t() {
        let s = Scope::dev();
        let _ = s;
    }
}
"#;
        let v = scan_text(&PathBuf::from("crates/foo/src/lib.rs"), src);
        assert!(v.is_empty(), "expected no violation inside test module; got {v:#?}");
    }

    #[test]
    fn skips_comment_only_lines() {
        let src = r#"
// This is a doc comment mentioning Scope::dev() — not a call site.
/// Doc comment example uses `Scope::dev()` as the placeholder.
fn route() {}
"#;
        let v = scan_text(&PathBuf::from("crates/foo/src/lib.rs"), src);
        assert!(v.is_empty(), "expected no violation for comment-only lines; got {v:#?}");
    }

    #[test]
    fn flags_multiple_sites_independently() {
        let src = r#"
fn a() { storage.write(&Scope::dev(), &ops); }
fn b() { storage.write(&Scope::dev(), &ops); } // scope-dev-allowed: ok
fn c() { storage.write(&Scope::dev(), &ops); }
"#;
        let v = scan_text(&PathBuf::from("crates/foo/src/lib.rs"), src);
        assert_eq!(v.len(), 2, "expected two violations (a + c); got {v:#?}");
    }
}
