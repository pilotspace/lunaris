//! Release-metadata guards — the workspace must stay *actually* publishable.
//!
//! Structural-guard style follows `lunaris-bench/tests/eval_workflow_guard.rs`
//! (assert on the release artefact, not on a mock of it). It lives in `xtask`
//! rather than `lunaris-bench` on purpose: `xtask` depends on four leaf crates
//! (anyhow / clap / serde / serde_json) and on no Lunaris crate, so this gate
//! compiles on a bare checkout without the `vendor/moon` submodule or a C++
//! toolchain. A release guard you cannot run before tagging is not a guard.
//!
//! Two defects are pinned here.
//!
//! 1. **Unpublishable dependency closure.** `crates/lunaris-hook` carried
//!    `publish = true` while depending on `lunaris-memory-service`
//!    (`publish = false`). crates.io rejects a manifest whose normal/build
//!    dependency graph reaches a crate that was never uploaded, so
//!    `cargo publish -p lunaris-hook` could only ever fail — and
//!    `scripts/release-preflight.sh` listed it under `HYGIENE_CRATES`, i.e.
//!    the preflight actively asserted it was ready to ship.
//!
//! 2. **The moondb publish guard skipped a source that had genuinely moved.**
//!    `.github/workflows/crates-publish.yml` decided "already published" from
//!    the *version string* alone. The vendored SDK
//!    (`vendor/moon/sdk/rust`, submodule pinned at moon `v0.8.5`) still
//!    declares `version = "0.2.1"` — the same string that has been on
//!    crates.io since the 0.2.x era — while its source moved several moon
//!    releases forward. Measured 2026-08-15: published `moondb` 0.2.1 contains
//!    **zero** occurrences of `ConnectionManager` (it is still on
//!    `MultiplexedConnection`); the pinned `v0.8.5` source contains **seven**.
//!    The `MultiplexedConnection` -> `ConnectionManager` reconnect fix (moon
//!    PR #419) is therefore absent from every crates.io consumer of the
//!    published lunaris crates, and the guard reported
//!    "moondb 0.2.1 already on crates.io — skipping" on every run.
//!
//!    crates.io versions are immutable, so the repair is NOT to republish:
//!    it is to *fail loudly* so the operator cuts a new moondb release and
//!    bumps the vendored manifest + the workspace `moon` pin together.

use std::path::{Path, PathBuf};
use std::process::Command;

// ───────────────────────── shared helpers ─────────────────────────

/// Repo root = `<root>/xtask` -> `<root>`.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask/ has a parent (repo root)")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

// ─────────────────── cargo metadata (publish flags) ───────────────────

#[derive(serde::Deserialize)]
struct Metadata {
    packages: Vec<Package>,
}

#[derive(serde::Deserialize)]
struct Package {
    name: String,
    /// `null` => publishable to any registry; `[]` => `publish = false`;
    /// a non-empty list => restricted to those registries (still publishable).
    publish: Option<Vec<String>>,
    manifest_path: String,
    dependencies: Vec<Dep>,
}

#[derive(serde::Deserialize)]
struct Dep {
    name: String,
    /// `null` for a normal dependency; otherwise `"dev"` / `"build"`.
    kind: Option<String>,
    req: String,
}

impl Package {
    fn is_publishable(&self) -> bool {
        !matches!(&self.publish, Some(list) if list.is_empty())
    }

    /// Directory name under `crates/` (or the workspace root) — the identifier
    /// `scripts/release-preflight.sh` uses, which is NOT always the package
    /// name (`crates/lunaris` publishes as `lunaris-memory`).
    fn dir_name(&self) -> String {
        Path::new(&self.manifest_path)
            .parent()
            .and_then(|d| d.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

fn workspace_metadata() -> Metadata {
    let out = Command::new(env!("CARGO"))
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(repo_root())
        .output()
        .expect("cargo is on PATH");
    assert!(
        out.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("cargo metadata emits valid JSON")
}

/// Dependencies that survive into the manifest cargo uploads to crates.io.
///
/// Normal and build dependencies always do. Dev-dependencies are stripped
/// only when they are path-only (`req == "*"`); a dev-dep that carries a real
/// version requirement — every workspace-inherited one does, they all render
/// as `^X.Y.Z` — is kept and must resolve on the registry too.
fn blocks_publish(dep: &Dep) -> bool {
    match dep.kind.as_deref() {
        None | Some("build") => true,
        Some("dev") => dep.req != "*",
        _ => false,
    }
}

// ───────────────────── 1. dependency-closure guard ─────────────────────

#[test]
fn publishable_crates_depend_only_on_publishable_crates() {
    let meta = workspace_metadata();
    let unpublishable: Vec<&str> =
        meta.packages.iter().filter(|p| !p.is_publishable()).map(|p| p.name.as_str()).collect();

    let mut offenders = Vec::new();
    for pkg in meta.packages.iter().filter(|p| p.is_publishable()) {
        for dep in pkg.dependencies.iter().filter(|d| blocks_publish(d)) {
            if unpublishable.contains(&dep.name.as_str()) {
                let kind = dep.kind.as_deref().unwrap_or("normal");
                offenders.push(format!("  {} ({kind} dep) -> {}", pkg.name, dep.name));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "a crate marked publishable depends on a `publish = false` crate.\n\
         cargo strips the path and pins the registry version, so crates.io \
         rejects the upload — `cargo publish` for these can only ever fail.\n\
         Fix by marking the dependent `publish = false` too, or by making the \
         dependency publishable.\n{}",
        offenders.join("\n")
    );
}

// ──────────────── 2. release tooling agrees with the manifests ────────────────

/// Every crate `.github/workflows/crates-publish.yml` walks must be publishable.
#[test]
fn crates_publish_workflow_lists_only_publishable_crates() {
    let yml = read(".github/workflows/crates-publish.yml");
    let listed = shell_list_value(&yml, "CRATES=");
    assert!(!listed.is_empty(), "could not parse the CRATES= list out of crates-publish.yml");

    let meta = workspace_metadata();
    let offenders: Vec<&String> = listed
        .iter()
        .filter(|name| meta.packages.iter().any(|p| &p.name == *name && !p.is_publishable()))
        .collect();

    assert!(
        offenders.is_empty(),
        "crates-publish.yml would `cargo publish -p` a `publish = false` crate: {offenders:?}"
    );
}

/// `scripts/release-preflight.sh` gates the tag on `HYGIENE_CRATES`. Listing an
/// unpublishable crate there makes the preflight assert readiness for something
/// that can never ship — the precise state `lunaris-hook` was left in.
#[test]
fn release_preflight_hygiene_list_holds_only_publishable_crates() {
    let sh = read("scripts/release-preflight.sh");
    let listed = bash_array_value(&sh, "HYGIENE_CRATES=(");
    assert!(!listed.is_empty(), "could not parse HYGIENE_CRATES out of release-preflight.sh");

    let meta = workspace_metadata();
    let offenders: Vec<&String> = listed
        .iter()
        .filter(|dir| meta.packages.iter().any(|p| &p.dir_name() == *dir && !p.is_publishable()))
        .collect();

    assert!(
        offenders.is_empty(),
        "release-preflight.sh HYGIENE_CRATES holds `publish = false` crate dir(s): {offenders:?}\n\
         The preflight must not certify a crate that cannot be uploaded."
    );
}

/// Parse a `NAME=` shell assignment whose value spans backslash-continued
/// lines, into whitespace-separated tokens.
fn shell_list_value(src: &str, key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut lines = src.lines().skip_while(|l| !l.trim_start().starts_with(key));
    let Some(first) = lines.next() else { return out };
    let mut buf = first.trim_start().trim_start_matches(key).to_string();
    let mut continued = buf.trim_end().ends_with('\\');
    buf = buf.trim_end().trim_end_matches('\\').to_string();
    for line in lines {
        if !continued {
            break;
        }
        continued = line.trim_end().ends_with('\\');
        buf.push(' ');
        buf.push_str(line.trim().trim_end_matches('\\'));
    }
    out.extend(buf.split_whitespace().map(|s| s.trim_matches('"').to_string()));
    out
}

/// Parse a multi-line bash array literal `NAME=( a b\n c )`.
fn bash_array_value(src: &str, opener: &str) -> Vec<String> {
    let Some(start) = src.find(opener) else { return Vec::new() };
    let rest = &src[start + opener.len()..];
    let Some(end) = rest.find(')') else { return Vec::new() };
    rest[..end]
        .lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .flat_map(|l| l.split_whitespace())
        .map(|s| s.trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// ───────────────────── 3. moondb source-parity guard ─────────────────────

const PARITY_SCRIPT: &str = "scripts/check-vendored-moondb-parity.sh";

/// Comparison mode: `<script> <VENDORED_SRC_DIR> <PUBLISHED_SRC_DIR>`.
/// Stubs the crates.io fetch so the check is hermetic and offline.
fn run_parity(vendored: &Path, published: &Path) -> std::process::Output {
    Command::new("bash")
        .arg(PARITY_SCRIPT)
        .arg(vendored)
        .arg(published)
        .current_dir(repo_root())
        .output()
        .expect("bash is available on PATH")
}

fn scratch(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("lunaris-moondb-parity-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn write_src(dir: &Path, name: &str, body: &str) {
    std::fs::create_dir_all(dir).expect("create src dir");
    std::fs::write(dir.join(name), body).expect("write stub source");
}

#[test]
fn moondb_parity_script_accepts_identical_sources() {
    let root = scratch("same");
    let (a, b) = (root.join("vendored"), root.join("published"));
    write_src(&a, "client.rs", "pub struct MoonClient;\n");
    write_src(&b, "client.rs", "pub struct MoonClient;\n");

    let out = run_parity(&a, &b);
    assert_eq!(
        out.status.code(),
        Some(0),
        "identical sources must pass (exit 0).\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The regression that matters: same declared version, different source.
/// The old `is_published`-only guard returned "skip"; the repaired guard must
/// fail so the operator cuts a real moondb release.
#[test]
fn moondb_parity_script_rejects_diverged_sources() {
    let root = scratch("diverged");
    let (a, b) = (root.join("vendored"), root.join("published"));
    // Mirrors the real defect: vendored has the ConnectionManager reconnect
    // fix, the crates.io copy at the same version does not.
    write_src(&a, "client.rs", "use redis::aio::ConnectionManager;\n");
    write_src(&b, "client.rs", "use redis::aio::MultiplexedConnection;\n");

    let out = run_parity(&a, &b);
    assert_eq!(
        out.status.code(),
        Some(1),
        "diverged sources must FAIL (exit 1) — a version-string-only check is \
         what let moondb 0.2.1 ship without the ConnectionManager reconnect fix.\n\
         stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let combined =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(
        combined.contains("client.rs"),
        "the failure must name the diverging file(s); got:\n{combined}"
    );
}

/// A missing / renamed second argument is an operator error, not a silent pass —
/// "guard could not run" must never be indistinguishable from "guard passed".
#[test]
fn moondb_parity_script_rejects_partial_arguments() {
    let root = scratch("usage");
    let a = root.join("vendored");
    write_src(&a, "client.rs", "pub struct MoonClient;\n");

    let out = Command::new("bash")
        .arg(PARITY_SCRIPT)
        .arg(&a)
        .current_dir(repo_root())
        .output()
        .expect("bash is available on PATH");
    assert_eq!(out.status.code(), Some(2), "one-arg invocation must exit 2 (usage error)");
}

/// The workflow must actually call the guard — a script nobody runs is a no-op.
#[test]
fn crates_publish_workflow_verifies_vendored_moondb_parity() {
    let yml = read(".github/workflows/crates-publish.yml");
    assert!(
        yml.contains("check-vendored-moondb-parity.sh"),
        "crates-publish.yml must invoke {PARITY_SCRIPT} before it skips moondb.\n\
         Deciding `already published` from the version string alone is exactly \
         how the pinned SDK diverged from crates.io moondb 0.2.1 undetected."
    );
}
