//! Ephemeral child-process Moon — one disposable server per fixture.
//!
//! ## Why a child process and not the embedded server
//!
//! `lunaris-memory-service`'s `embedded-moon` feature links the whole Moon
//! **server** crate into the calling binary. CLAUDE.md pins that feature out of
//! every default feature set precisely so `cargo test --workspace` and CI
//! clippy never compile it. A test harness that reached for it would re-import
//! that cost into every test target in the workspace.
//!
//! Spawning a prebuilt `moon` binary keeps the invariant intact: no test binary
//! ever links Moon, and the fixture costs one `fork`/`exec` — measured at
//! **2.7–6 ms** to RESP-ready on an M-series Mac with `--appendonly no`. That is
//! cheap enough that every test gets its OWN Moon, which is also the only
//! arrangement that faithfully replaces `memory://` (a fresh, empty,
//! process-private store per `connect`). No shared once-per-binary instance, no
//! cross-test scope collisions, no leak-on-exit problem from a `static` guard
//! whose `Drop` never runs.
//!
//! ## Safety rails
//!
//! * The port is OS-assigned (bind `127.0.0.1:0`, read it back, release), and
//!   any draw landing on a [`RESERVED_PORTS`] entry is re-rolled. The live
//!   store (6381) and the dedicated bench Moon (6399) can never be touched.
//! * The data directory lives under [`std::env::temp_dir`] and is removed in
//!   [`EphemeralMoon::drop`], which also `SIGKILL`s the child and reaps it.
//! * Only a PID this fixture spawned is ever signalled.
//! * No `FLUSHALL`, ever — isolation comes from a fresh directory.

use std::io::Read as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// Env var naming a prebuilt `moon` binary. Takes precedence over discovery.
pub const MOON_BINARY_ENV: &str = "MOON_TEST_BINARY";

/// Ports that carry real data on developer machines and CI runners. An
/// ephemeral fixture must never bind, write to, or shut down any of them.
///
/// * 6379 — stock Redis default
/// * 6380 — Moon dev default
/// * 6381 — the LIVE Lunaris memory store on the maintainer's box
/// * 6399 — the dedicated benchmark Moon
pub const RESERVED_PORTS: &[u16] = &[6379, 6380, 6381, 6399];

/// How long to wait for the spawned Moon to answer `PING` with `+PONG`.
const READY_BUDGET: Duration = Duration::from_secs(15);

/// Attempts to draw a port + spawn before giving up. Covers the (tiny) race
/// between releasing the probe listener and Moon binding the same port.
const SPAWN_ATTEMPTS: usize = 5;

/// Locate a usable `moon` binary, or `None` when this machine has none.
///
/// Resolution order:
/// 1. `$MOON_TEST_BINARY` (must exist; a set-but-missing path resolves to
///    `None`, same as unset).
/// 2. `<workspace-root>/vendor/moon/target/release/moon`
/// 3. `<workspace-root>/vendor/moon/target/debug/moon`
///
/// `None` used to be the load-bearing fallback path — every fixture degraded to
/// `memory://`. 0.7.0 deleted that backend, so `None` now means the calling
/// fixture PANICS with build instructions (`crate::open_test_store`). That is
/// the intended trade: a suite that cannot reach its substrate must say so, not
/// quietly test something else.
#[must_use]
pub fn moon_binary() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var(MOON_BINARY_ENV) {
        let p = PathBuf::from(raw.trim());
        return if p.is_file() { Some(p) } else { None };
    }
    let root = workspace_root();
    for rel in ["vendor/moon/target/release/moon", "vendor/moon/target/debug/moon"] {
        let p = root.join(rel);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// `CARGO_MANIFEST_DIR` is `<root>/crates/lunaris-test-harness`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// A running, disposable Moon server owned by this handle.
///
/// Dropping it kills the process and deletes its data directory. Hold it for
/// as long as any client of [`Self::url`] is alive — a `TestStore`/`TestEngine`
/// does exactly that.
#[derive(Debug)]
pub struct EphemeralMoon {
    child: Option<Child>,
    port: u16,
    dir: PathBuf,
    url: String,
}

impl EphemeralMoon {
    /// Spawn a single-shard, non-persistent Moon on a free loopback port.
    ///
    /// # Errors
    /// No `moon` binary on this machine, every port draw racing, the child
    /// exiting during boot, or the readiness probe exceeding `READY_BUDGET`.
    pub async fn spawn() -> anyhow::Result<Self> {
        let bin = moon_binary().ok_or_else(|| {
            anyhow!(
                "no moon binary: set ${MOON_BINARY_ENV} or build \
                 vendor/moon/target/release/moon"
            )
        })?;
        Self::spawn_from(&bin).await
    }

    /// Spawn using an explicit binary path (the DI seam `spawn` resolves for).
    ///
    /// # Errors
    /// See [`Self::spawn`].
    pub async fn spawn_from(bin: &Path) -> anyhow::Result<Self> {
        let mut last: Option<anyhow::Error> = None;
        for _ in 0..SPAWN_ATTEMPTS {
            match Self::try_spawn_once(bin).await {
                Ok(m) => return Ok(m),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("moon spawn failed with no recorded cause")))
    }

    async fn try_spawn_once(bin: &Path) -> anyhow::Result<Self> {
        let port = free_loopback_port()?;
        let dir = scratch_dir(port);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create moon scratch dir {}", dir.display()))?;

        let log_path = dir.join("moon.log");
        let log = std::fs::File::create(&log_path)
            .with_context(|| format!("create moon log {}", log_path.display()))?;
        let log_err = log.try_clone().context("clone moon log handle")?;

        // Flag rationale:
        //   --shards 1        single shard ALWAYS; a sharded Moon rejects the
        //                     Lunaris ingest TXN.
        //   --appendonly no   nothing here outlives the test; skipping the AOF
        //                     is what takes boot from ~26 ms to ~3 ms.
        //   --maxmemory 512m  bound a runaway fixture instead of the box.
        //   --pagecache-size  Moon otherwise sizes the page cache off host RAM
        //                     (~5 GB of lazy frames per instance); 64 MB keeps
        //                     many concurrent fixtures cheap.
        //   --disk-free-min-pct 1
        //                     the dev boxes this runs on sit >90 % full and the
        //                     default diskfull guard would pause writes.
        let child = Command::new(bin)
            .arg("--bind")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .arg("--dir")
            .arg(&dir)
            .arg("--shards")
            .arg("1")
            .arg("--protected-mode")
            .arg("no")
            .arg("--appendonly")
            .arg("no")
            .arg("--maxmemory")
            .arg("536870912")
            .arg("--pagecache-size")
            .arg("64mb")
            .arg("--disk-free-min-pct")
            .arg("1")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .spawn()
            .with_context(|| format!("spawn moon binary {}", bin.display()))?;

        let mut moon =
            Self { child: Some(child), port, dir, url: format!("moon://127.0.0.1:{port}") };
        moon.await_ready().await?;
        Ok(moon)
    }

    /// Poll `PING` until `+PONG`, failing fast if the child exits first.
    async fn await_ready(&mut self) -> anyhow::Result<()> {
        let started = Instant::now();
        let mut backoff = Duration::from_millis(2);
        loop {
            if let Some(child) = self.child.as_mut()
                && let Some(status) = child.try_wait().context("poll moon child")?
            {
                bail!("moon exited during boot with {status}; log tail:\n{}", self.log_tail());
            }
            if resp_ping(self.port).await {
                return Ok(());
            }
            if started.elapsed() > READY_BUDGET {
                bail!(
                    "moon on port {} not ready within {:?}; log tail:\n{}",
                    self.port,
                    READY_BUDGET,
                    self.log_tail()
                );
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_millis(50));
        }
    }

    /// `moon://127.0.0.1:<port>` — feed this straight to `Lunaris::open*`.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The loopback port this instance listens on.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Scratch data directory, removed on drop.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.dir
    }

    /// Last 2 KiB of the server log — surfaced in every failure message.
    #[must_use]
    pub fn log_tail(&self) -> String {
        let path = self.dir.join("moon.log");
        let Ok(mut f) = std::fs::File::open(&path) else {
            return format!("<no log at {}>", path.display());
        };
        let mut buf = String::new();
        if f.read_to_string(&mut buf).is_err() {
            return format!("<unreadable log at {}>", path.display());
        }
        let start = buf.len().saturating_sub(2048);
        buf.split_off(start)
    }
}

/// Env var that turns [`EphemeralMoon`]'s drop into a post-mortem (F35).
///
/// Set it to any non-empty value to make drop report whether the child was
/// still ALIVE, and to keep the scratch directory (with `moon.log`) instead
/// of deleting it.
pub const KEEP_ENV: &str = "LUNARIS_TEST_MOON_POSTMORTEM";

impl Drop for EphemeralMoon {
    /// Synchronous, best-effort reap. Runs on unwind too, so a panicking test
    /// still leaves no orphan process and no scratch directory.
    ///
    /// ## The post-mortem escape hatch (F35)
    ///
    /// `lunaris-mcp`'s `record_decision_smoke` / `record_edit_smoke` fail
    /// under a full workspace run with `moon: redis error: broken pipe`. The
    /// mechanism is NOT established: it reproduced 2-of-12 once and 0-of-52
    /// since, including under saturating CPU load, so the original
    /// "resource contention" reading is unsupported.
    ///
    /// Two hypotheses remain and exactly one observation separates them —
    /// was the Moon alive when the client's socket broke?
    ///
    /// - **Child DIED** → the Moon was reaped or crashed. Resource contention.
    /// - **Child ALIVE** → the socket dropped under a live server, and the
    ///   read-only wiring of `lunaris_storage_moon::retry::with_conn_retry`
    ///   becomes the lead (`record_decision` is a write, so a transient
    ///   `broken pipe` on it has no retry above the SDK's ConnectionManager).
    ///
    /// That observation was impossible by default: this `Drop` reaped the
    /// child and deleted the scratch dir, taking `moon.log` with it. Setting
    /// [`KEEP_ENV`] preserves both. It is off by default because the normal
    /// case must leave no orphan process and no scratch directory — the
    /// hatch trades exactly that away, and only when asked.
    fn drop(&mut self) {
        let postmortem = postmortem_requested(std::env::var_os(KEEP_ENV).as_deref());

        if let Some(mut child) = self.child.take() {
            if postmortem {
                // try_wait BEFORE kill: after kill the answer is always
                // "died", which is the answer we already knew.
                let state = match child.try_wait() {
                    Ok(Some(status)) => format!("DIED (exit {status})"),
                    Ok(None) => "ALIVE".to_string(),
                    Err(e) => format!("UNKNOWN ({e})"),
                };
                eprintln!(
                    "[{KEEP_ENV}] moon on port {} at drop: {state}; data dir kept at {}",
                    self.port,
                    self.dir.display()
                );
            }
            let _ = child.kill();
            let _ = child.wait();
        }

        if postmortem {
            eprintln!("[{KEEP_ENV}] keeping {} — remove it yourself when done", self.dir.display());
            return;
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Whether [`KEEP_ENV`]'s value asks for the post-mortem.
///
/// Split out of `drop` so the decision is testable without mutating
/// process-global environment from a parallel test. An EMPTY value is a
/// deliberate NO: `FOO= cargo test` and an exported-but-cleared variable are
/// both how a shell says "off", and treating mere presence as on would leave
/// orphan scratch directories behind for anyone who cleared it that way.
fn postmortem_requested(v: Option<&std::ffi::OsStr>) -> bool {
    v.is_some_and(|v| !v.is_empty())
}

/// One RESP `PING`. `true` only on a `+PONG` reply.
async fn resp_ping(port: u16) -> bool {
    let connect = tokio::net::TcpStream::connect(("127.0.0.1", port));
    let Ok(Ok(mut s)) = tokio::time::timeout(Duration::from_millis(250), connect).await else {
        return false;
    };
    if s.write_all(b"PING\r\n").await.is_err() {
        return false;
    }
    let mut buf = [0_u8; 32];
    match tokio::time::timeout(Duration::from_millis(250), s.read(&mut buf)).await {
        Ok(Ok(n)) if n > 0 => buf[..n].starts_with(b"+PONG"),
        _ => false,
    }
}

/// Draw an OS-assigned loopback port, re-rolling any [`RESERVED_PORTS`] hit.
///
/// The listener is dropped before returning, so there is a small window in
/// which another process could claim the port; [`SPAWN_ATTEMPTS`] covers it.
fn free_loopback_port() -> anyhow::Result<u16> {
    for _ in 0..32 {
        let l = TcpListener::bind(("127.0.0.1", 0)).context("bind probe listener")?;
        let port = l.local_addr().context("read probe listener addr")?.port();
        drop(l);
        if !RESERVED_PORTS.contains(&port) {
            return Ok(port);
        }
    }
    bail!("could not draw a non-reserved loopback port in 32 attempts")
}

/// Unique scratch directory: pid + port + a monotonic counter keep concurrent
/// fixtures (and concurrent test binaries) from colliding.
fn scratch_dir(port: u16) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("lunaris-test-moon-{}-{port}-{seq}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_ports_cover_the_live_and_bench_stores() {
        assert!(RESERVED_PORTS.contains(&6381), "live memory store must be reserved");
        assert!(RESERVED_PORTS.contains(&6399), "dedicated bench Moon must be reserved");
    }

    #[test]
    fn drawn_ports_are_never_reserved() {
        for _ in 0..64 {
            let p = free_loopback_port().expect("draw port");
            assert!(!RESERVED_PORTS.contains(&p), "drew reserved port {p}");
        }
    }

    #[test]
    fn scratch_dirs_are_unique_and_under_temp() {
        let a = scratch_dir(1234);
        let b = scratch_dir(1234);
        assert_ne!(a, b, "two draws must not collide");
        assert!(a.starts_with(std::env::temp_dir()), "scratch dir must live under temp_dir");
    }

    /// F35 — the post-mortem hatch is OFF unless asked for, and an empty
    /// value is not asking.
    ///
    /// Keyed on the accepted SET, not on presence: `LUNARIS_TEST_MOON_POSTMORTEM=`
    /// and an exported-then-cleared variable are both how a shell says "off".
    /// A presence check would treat them as on and leave a scratch directory
    /// per Moon behind on every run — the failure mode the default exists to
    /// prevent, arriving through the flag meant to be opt-in.
    #[test]
    fn the_postmortem_hatch_is_off_unless_asked_for() {
        use std::ffi::OsStr;
        assert!(!postmortem_requested(None), "unset must not keep the scratch dir");
        assert!(!postmortem_requested(Some(OsStr::new(""))), "an EMPTY value must read as off");
        assert!(postmortem_requested(Some(OsStr::new("1"))));
        assert!(postmortem_requested(Some(OsStr::new("0"))), "any non-empty value opts in");
    }

    /// The default reap really removes the directory.
    ///
    /// `postmortem_requested` above is a decision, not an effect — a `drop`
    /// that consulted it and then leaked the dir anyway would pass that test.
    /// This one drops a real `EphemeralMoon` and looks at the filesystem.
    #[tokio::test]
    async fn dropping_a_moon_removes_its_scratch_dir() {
        let Ok(moon) = EphemeralMoon::spawn().await else {
            // `spawn` already explains a missing MOON_TEST_BINARY; the
            // no-silent-skip sweep covers the harness's own skip accounting.
            crate::strict_skip::note_unavailable(
                "EphemeralMoon::spawn failed — MOON_TEST_BINARY unset or unusable",
            );
            return;
        };
        let dir = moon.data_dir().to_path_buf();
        assert!(dir.exists(), "a started Moon must have its scratch dir");
        drop(moon);
        assert!(
            !dir.exists(),
            "drop left {} behind — the workspace suite spawns one Moon per test binary, \
             so a leaked dir per drop fills the boot volume",
            dir.display()
        );
    }
}
