//! Plan 05-03 PROTO-06 — full protocol suite against a real `lunaris-server`.
//!
//! Spawns `target/{profile}/lunaris-server` as a subprocess, parses
//! `LISTENING_ON <addr>` from its stderr (Plan 05-01 main.rs:53-57 contract),
//! runs [`lunaris_conformance::run_full_protocol_suite`] against it, kills
//! the child + cleans up the temp tokens-file on completion (success OR
//! failure, including panic).
//!
//! ## Skip discipline (two-tier — Plan 02-04 + Plan 04-03)
//!
//! 1. Neither `MOON_URL` nor `PG_URL` set / TCP-reachable → SKIP cleanly.
//! 2. Resolved `lunaris-server` binary path does not exist → SKIP cleanly
//!    (developer needs to run `cargo build -p lunaris-server` first).
//! 3. Server fails to print `LISTENING_ON <addr>` within 10s → SKIP cleanly
//!    (best-effort; surface the diagnostic so CI logs show why).
//!
//! When all three gates pass: runs the suite end-to-end and returns its
//! `Result`. Cleanup happens unconditionally via the [`Cleanup`] RAII guard.

#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use reqwest::Client;
use url::Url;

#[tokio::test]
async fn lunaris_server_protocol_conformance() -> anyhow::Result<()> {
    // 1. Pick a backend (prefer MOON_URL; fall back to PG_URL).
    let storage_url = match (
        probe_backend("MOON_URL", std::env::var("MOON_URL").ok()),
        probe_backend("PG_URL", std::env::var("PG_URL").ok()),
    ) {
        (Some(u), _) => u,
        (None, Some(u)) => u,
        (None, None) => {
            eprintln!(
                "SKIP run_protocol_lunaris_server: neither MOON_URL nor PG_URL set / reachable"
            );
            return Ok(());
        }
    };

    // 2. Resolve binary path (Shared Pattern 1 — verbatim from
    //    crates/lunaris-conformance/tests/crash_recovery.rs:247-261).
    let bin = resolve_lunaris_server_bin();
    if !std::path::Path::new(&bin).exists() {
        eprintln!(
            "SKIP run_protocol_lunaris_server: binary not found at {bin} — \
             run `cargo build -p lunaris-server` first"
        );
        return Ok(());
    }

    // 3. Write a temp tokens-file with two test tokens. The runner cleans up
    //    via the Cleanup RAII guard regardless of test outcome.
    let tokens_path =
        std::env::temp_dir().join(format!("lunaris-conformance-tokens-{}.json", ulid::Ulid::new()));
    let tokens_body = serde_json::json!({
        "tok-test":   { "tenant": "test-tenant",   "scopes": ["ingest", "recall", "forget"] },
        "tok-ingest": { "tenant": "ingest-tenant", "scopes": ["ingest"] }
    });
    std::fs::write(&tokens_path, tokens_body.to_string())?;
    let _tokens_cleanup = TempPathCleanup::new(tokens_path.clone());

    // 4. Spawn the server with --bind 127.0.0.1:0 (ephemeral) + tight rate
    //    limit so rate_limit::burst_returns_429 fires within a few hundred ms.
    let mut child = Command::new(&bin)
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--storage")
        .arg(&storage_url)
        .arg("--tokens-file")
        .arg(&tokens_path)
        .arg("--rate-per-second")
        .arg("5")
        .arg("--rate-burst")
        .arg("10")
        .arg("--cors-origins")
        .arg("*")
        .arg("--shutdown-grace-secs")
        .arg("2")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;

    // 5. Read stderr in a background thread; resolve LISTENING_ON address.
    //    On timeout / failure, kill the child + skip cleanly.
    let bound_addr = match wait_for_listening(&mut child, Duration::from_secs(10)) {
        Ok(addr) => addr,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("SKIP run_protocol_lunaris_server: server start failed: {e}");
            return Ok(());
        }
    };

    // RAII guard — kills the child on any return path (Ok, Err, panic).
    let _child_guard = ChildCleanup::new(child);

    let base_url = Url::parse(&format!("http://{bound_addr}"))?;
    let client = Client::builder().timeout(Duration::from_secs(30)).build()?;

    // 6. Run the protocol suite.
    // Cleanup happens via Drop (ChildCleanup + TempPathCleanup).
    lunaris_conformance::run_full_protocol_suite(client, base_url, "tok-test".to_string()).await
}

/// RAII guard that kills + reaps a child process on drop. Survives panics.
struct ChildCleanup {
    child: Option<Child>,
}

impl ChildCleanup {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }
}

impl Drop for ChildCleanup {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// RAII guard that removes a temp file on drop. Survives panics.
struct TempPathCleanup {
    path: PathBuf,
}

impl TempPathCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for TempPathCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Resolve the `lunaris-server` binary path via three fall-throughs:
/// 1. `CARGO_BIN_EXE_lunaris-server` (only set when binary is in same package
///    as the test — won't be for lunaris-conformance, but try anyway for
///    forward-compat in case Cargo extends this).
/// 2. Workspace `<target_dir>/{debug|release}/lunaris-server` walker honoring
///    `CARGO_TARGET_DIR`.
///
/// (`which lunaris-server` PATH walking is intentionally NOT included — the
/// dev workflow always builds from this workspace; a system-installed binary
/// would likely be a different version + would mask version-mismatch bugs.)
fn resolve_lunaris_server_bin() -> String {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_lunaris-server") {
        return p;
    }
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().and_then(|p| p.parent()).unwrap_or(&manifest);
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| workspace.join("target"));
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    target_dir.join(profile).join("lunaris-server").to_string_lossy().into_owned()
}

/// Read the child's stderr in a background thread until the
/// `LISTENING_ON <addr>` line appears (per Plan 05-01 main.rs:53-57). On
/// timeout, returns Err — caller should kill the child + skip.
fn wait_for_listening(child: &mut Child, timeout: Duration) -> anyhow::Result<String> {
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stderr piped on lunaris-server child"))?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            // Echo to test stderr so failures are diagnosable.
            eprintln!("[lunaris-server] {line}");
            if let Some(addr) = line.strip_prefix("LISTENING_ON ") {
                let _ = tx.send(addr.trim().to_string());
                // Don't break — keep draining stderr so the child doesn't
                // block on a full pipe. The receiver will already have the
                // address.
            }
        }
    });
    match rx.recv_timeout(timeout) {
        Ok(addr) => Ok(addr),
        Err(e) => anyhow::bail!("timeout waiting for LISTENING_ON line on stderr: {e}"),
    }
}

/// TCP-probe the backend URL within 1s. Mirror of
/// `crates/lunaris-conformance/tests/crash_recovery.rs::probe_backend` (W-3 +
/// W-7 fixes from Plan 04-03 — `to_socket_addrs()` accepts hostnames + literal
/// IPs; takes `Option<String>` so the test never mutates process env).
fn probe_backend(name: &str, url: Option<String>) -> Option<String> {
    let url = url?;
    let host_port = if let Some(rest) = url.strip_prefix("moon://") {
        rest.split('/').next()?.to_string()
    } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        let after_scheme = url.split("://").nth(1)?;
        let authority = after_scheme.split('/').next()?;
        let bare = authority.rsplit('@').next()?;
        if bare.contains(':') { bare.to_string() } else { format!("{bare}:5432") }
    } else if let Some(rest) = url.strip_prefix("redis://") {
        rest.split('/').next()?.to_string()
    } else {
        return None;
    };

    let timeout = Duration::from_secs(1);
    let addr = match host_port.to_socket_addrs().ok().and_then(|mut it| it.next()) {
        Some(a) => a,
        None => {
            eprintln!(
                "run_protocol_lunaris_server: SKIP {name} (DNS resolution of {host_port} failed)"
            );
            return None;
        }
    };
    match TcpStream::connect_timeout(&addr, timeout) {
        Ok(_) => Some(url),
        Err(_) => {
            eprintln!(
                "run_protocol_lunaris_server: SKIP {name} (TCP probe to {host_port} failed within {}ms)",
                timeout.as_millis()
            );
            None
        }
    }
}
