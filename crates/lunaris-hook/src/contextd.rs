//! `lunaris-contextd` — warm local recall sidecar for Codex hooks.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use clap::Parser;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

#[derive(Parser, Debug)]
#[command(
    name = "lunaris-contextd",
    version,
    about = "Warm Lunaris context sidecar for Codex hook memory injection"
)]
struct Cli {
    /// Unix socket path. Defaults to ~/.lunaris/codex-contextd.sock.
    #[arg(long, env = "LUNARIS_CONTEXTD_SOCKET")]
    socket: Option<std::path::PathBuf>,

    /// Log filter directive. Logs go to stderr.
    #[arg(long, env = "LUNARIS_CONTEXTD_LOG", default_value = "warn")]
    log_level: String,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.log_level);

    let socket = match cli.socket {
        Some(path) => path,
        None => lunaris_hook::context::default_socket_path()?,
    };
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if socket.exists() {
        match UnixStream::connect(&socket).await {
            Ok(_) => {
                tracing::info!(
                    socket = %socket.display(),
                    "lunaris-contextd already running; exiting duplicate starter"
                );
                return Ok(());
            }
            Err(_) => {
                let _ = std::fs::remove_file(&socket);
            }
        }
    }

    // Embedded Moon (feature `embedded-moon`): bundle the storage kernel
    // in-process so contextd + one-shot hooks share ONE unified processor.
    // Launch BEFORE ContextService::new(): the service resolves storage
    // lazily per scope through scope::resolve_storage_url, which reads the
    // discovery file this writes — so contextd itself converges on its own
    // embedded Moon through the exact same path the hook binaries use.
    // Held for process lifetime; Drop cancels the server on exit.
    #[cfg(feature = "embedded-moon")]
    let _moon_guard = launch_unified_moon().await;

    let listener = UnixListener::bind(&socket)?;
    tracing::info!(socket = %socket.display(), "lunaris-contextd listening");
    let service = lunaris_hook::context::ContextService::new();

    loop {
        let (stream, _) = listener.accept().await?;
        let service = service.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, service).await {
                tracing::warn!(err = %err, "contextd connection failed");
            }
        });
    }
}

async fn handle_connection(
    mut stream: UnixStream,
    service: lunaris_hook::context::ContextService,
) -> anyhow::Result<()> {
    use lunaris_hook::context::ContextRequest;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let request: ContextRequest = serde_json::from_slice(&buf)?;
    // Memory ops answer on a distinct response channel (MemoryResponse — the
    // tool's own DTO), so they serialize through `handle_memory`; every other
    // (hook) variant answers with a ContextResponse via `handle`. Framing is
    // identical for both: one JSON response, connection-per-call.
    let bytes = match request {
        ContextRequest::Memory(mem) => serde_json::to_vec(&service.handle_memory(mem).await)?,
        other => serde_json::to_vec(&service.handle(other).await)?,
    };
    stream.write_all(&bytes).await?;
    stream.shutdown().await?;
    Ok(())
}

/// Launch the in-process Moon and advertise it for hook-binary discovery.
///
/// Skipped (returns `None`) when:
/// - `LUNARIS_STORE_URL` is set — an explicit storage override applies to the
///   hook binaries too, so launching a Moon nobody would resolve to is waste;
/// - `LUNARIS_CONTEXTD_EMBEDDED_MOON=0` — operator opt-out;
/// - the launch itself fails — WARN and fall through to today's per-scope
///   SQLite behaviour (circuit-breaker: contextd ALWAYS starts).
///
/// Data dir: `LUNARIS_CONTEXTD_MOON_DIR` or `~/.lunaris/contextd-moon-data`.
/// On success writes `~/.lunaris/contextd-moon.url` (the discovery file
/// `scope::resolve_storage_url` probes). A stale file from a crashed run is
/// harmless — readers liveness-probe before trusting it — and this overwrite
/// refreshes it with the new port.
#[cfg(feature = "embedded-moon")]
async fn launch_unified_moon() -> Option<lunaris_memory_service::embedded_moon::EmbeddedMoonGuard> {
    if std::env::var("LUNARIS_STORE_URL").is_ok() {
        tracing::info!("LUNARIS_STORE_URL set — skipping embedded Moon launch");
        return None;
    }
    if matches!(
        std::env::var("LUNARIS_CONTEXTD_EMBEDDED_MOON").as_deref(),
        Ok("0") | Ok("false") | Ok("False")
    ) {
        tracing::info!("LUNARIS_CONTEXTD_EMBEDDED_MOON disabled — skipping embedded Moon launch");
        return None;
    }
    let lunaris_dir = match dirs::home_dir() {
        Some(home) => home.join(".lunaris"),
        None => {
            tracing::warn!("no home dir — skipping embedded Moon launch");
            return None;
        }
    };
    let data_dir = std::env::var("LUNARIS_CONTEXTD_MOON_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| lunaris_dir.join("contextd-moon-data"));
    if let Err(err) = std::fs::create_dir_all(&data_dir) {
        tracing::warn!(err = %err, dir = %data_dir.display(),
            "embedded Moon data dir unusable — falling back to per-scope SQLite");
        return None;
    }

    match lunaris_memory_service::embedded_moon::launch_embedded_moon(
        &data_dir.display().to_string(),
    )
    .await
    {
        Ok(guard) => {
            let url = format!("moon://127.0.0.1:{}", guard.port);
            let url_file = lunaris_dir.join(lunaris_hook::scope::CONTEXTD_MOON_URL_FILE);
            // Write via temp+rename so a hook binary never reads a torn URL.
            let tmp = url_file.with_extension("url.tmp");
            let write = std::fs::write(&tmp, format!("{url}\n"))
                .and_then(|()| std::fs::rename(&tmp, &url_file));
            match write {
                Ok(()) => {
                    tracing::info!(%url, file = %url_file.display(),
                        "embedded Moon ready — unified store advertised");
                    Some(guard)
                }
                Err(err) => {
                    // Un-advertised Moon would split-brain (contextd on Moon,
                    // hooks on SQLite) — shut it down and use SQLite everywhere.
                    tracing::warn!(err = %err, file = %url_file.display(),
                        "cannot advertise embedded Moon — shutting it down, using SQLite");
                    guard.shutdown().await;
                    None
                }
            }
        }
        Err(err) => {
            tracing::warn!(err = %err,
                "embedded Moon launch failed — falling back to per-scope SQLite");
            None
        }
    }
}

fn init_tracing(filter: &str) {
    use tracing_subscriber::{EnvFilter, fmt};
    let _ = fmt()
        .with_env_filter(EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("warn")))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}
