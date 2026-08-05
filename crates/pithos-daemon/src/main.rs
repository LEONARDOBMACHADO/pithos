//! Binary entrypoint for the local `pithosd` Agent API daemon.

use clap::Parser;
use pithos_daemon::{DaemonConfig, DaemonService, IpcEndpoint, IpcServer};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "pithosd", version, about = "Pithos local Agent API daemon")]
struct Args {
    /// Private state directory used for the job store and local IPC endpoint.
    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// Maximum roots that clients may request for reads.
    #[arg(long = "allow-read-root")]
    allow_read_roots: Vec<PathBuf>,

    /// Maximum roots that clients may request for writes.
    #[arg(long = "allow-write-root")]
    allow_write_roots: Vec<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let state_dir = args.state_dir.unwrap_or_else(default_state_dir);
    let mut config = DaemonConfig::new(state_dir.clone());
    if !args.allow_read_roots.is_empty() || !args.allow_write_roots.is_empty() {
        config.allowed_scope = pithos_agent_api::PathScope {
            read_roots: args.allow_read_roots,
            write_roots: args.allow_write_roots,
        };
    }
    let service =
        DaemonService::open(config).map_err(|error| std::io::Error::other(error.message))?;
    let endpoint = IpcEndpoint::for_state_dir(state_dir);
    let server = IpcServer::spawn(service, endpoint.clone()).await?;
    println!("pithosd ready on {}", endpoint.display_name());
    tokio::signal::ctrl_c().await?;
    server.shutdown().await?;
    Ok(())
}

fn default_state_dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("Pithos")
            .join("pithosd")
    }
    #[cfg(unix)]
    {
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
            return PathBuf::from(runtime).join("pithosd");
        }
        let user = std::env::var_os("USER").unwrap_or_else(|| "local".into());
        let suffix = blake3::hash(user.to_string_lossy().as_bytes());
        std::env::temp_dir().join(format!(
            "pithosd-{}",
            suffix.as_bytes()[..8]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ))
    }
    #[cfg(not(any(windows, unix)))]
    {
        std::env::temp_dir().join("pithosd")
    }
}
