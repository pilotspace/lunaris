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
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let request: lunaris_hook::context::ContextRequest = serde_json::from_slice(&buf)?;
    let response = service.handle(request).await;
    let bytes = serde_json::to_vec(&response)?;
    stream.write_all(&bytes).await?;
    stream.shutdown().await?;
    Ok(())
}

fn init_tracing(filter: &str) {
    use tracing_subscriber::{EnvFilter, fmt};
    let _ = fmt()
        .with_env_filter(EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("warn")))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}
