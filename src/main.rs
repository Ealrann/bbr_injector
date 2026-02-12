mod archive_db;
mod chia_messages;
mod config;
mod http_server;
mod in_flight;
mod injector;
mod metrics;
mod peer;
mod proofs;
mod shutdown;
#[cfg(feature = "test-mode")]
mod test_mode;
#[cfg(feature = "test-mode")]
mod toy_debug_client;
mod ws;

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::archive_db::ArchiveDb;
use crate::config::Config;
use crate::http_server::{HttpServerConfig, run_http_server};
use crate::in_flight::{InFlightState, InFlightStore, SendMode};
use crate::injector::Injector;
use crate::proofs::{ArchiveProofProvider, ProofServingMetrics};
use crate::shutdown::Shutdown;
#[cfg(feature = "test-mode")]
use crate::test_mode::{TestArgs, run_test};

const DEFAULT_PROOFS_PER_WINDOW: u32 = 3;
const DEFAULT_WINDOW_MS: u64 = 2_000;

#[derive(Debug, Parser)]
#[command(name = "bbr-injector")]
#[command(about = "Inject compact-proof announcements into specific peers at a fixed rate", long_about = None)]
struct Cli {
    /// Path to the config TOML file.
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the injector normally (default).
    Run,

    #[cfg(feature = "test-mode")]
    /// Inject 1-2 proofs into a toy node, then check debug metrics on 1-2 toy nodes.
    Test {
        /// Base URL of toy node A debug HTTP server.
        #[arg(long)]
        toy_a: String,

        /// Base URL of toy node B debug HTTP server (optional).
        #[arg(long)]
        toy_b: Option<String>,

        /// Target peer (host:port) to inject into. Defaults to the first `peers.targets` entry.
        #[arg(long)]
        target_peer: Option<String>,

        /// Number of proofs/events to inject (default: 2).
        #[arg(long, default_value_t = 2)]
        count: usize,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    // rustls 0.23 requires selecting a process-level CryptoProvider when multiple
    // providers are enabled via crate features. Choose ring explicitly to avoid runtime panics.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();
    let config = Config::load(&cli.config).await?;

    let in_flight_defaults = InFlightState {
        cursor_event_id: 0,
        end_event_id: None,
        proofs_per_window: DEFAULT_PROOFS_PER_WINDOW,
        window_ms: DEFAULT_WINDOW_MS,
        paused: true,
        send_mode: SendMode::Announcement,
    };
    let in_flight =
        InFlightStore::load_or_init(config.storage.in_flight_path.clone(), in_flight_defaults)
            .await?;
    in_flight.set_paused(true).await?;

    #[cfg(feature = "test-mode")]
    if let Some(Command::Test {
        toy_a,
        toy_b,
        target_peer,
        count,
    }) = cli.command
    {
        return run_test(
            config,
            TestArgs {
                toy_a_base_url: toy_a,
                toy_b_base_url: toy_b,
                target_peer,
                proofs_to_inject: count,
            },
        )
        .await;
    }

    let archive = Arc::new(ArchiveDb::new(&config.archive)?);
    let proof_metrics = Arc::new(ProofServingMetrics::default());
    let proof_provider = Arc::new(ArchiveProofProvider::new(
        archive.clone(),
        50_000,
        proof_metrics.clone(),
    ));

    let shutdown = Shutdown::new();
    let shutdown_ctrlc = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("Ctrl-C received; shutting down");
        shutdown_ctrlc.trigger();
        // If we don't exit quickly (e.g. due to a stuck task), hard-exit after a short grace period,
        // or immediately on a second Ctrl-C.
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {},
        }
        std::process::exit(130);
    });

    let (injector, injector_handle) = Injector::start(
        config.clone(),
        in_flight.clone(),
        archive,
        proof_provider,
        proof_metrics.clone(),
    )
    .await?;

    let listen_addr: std::net::SocketAddr = config.http.listen_addr.parse()?;
    let http_cfg = HttpServerConfig {
        listen_addr,
        static_dir: config.http.static_dir.clone(),
    };

    let injector_task = tokio::spawn(injector.run(shutdown.clone()));
    let http_task = tokio::spawn(run_http_server(
        shutdown.clone(),
        http_cfg,
        injector_handle.snapshot_rx,
        injector_handle.control_tx,
        proof_metrics,
    ));

    let (injector_res, http_res) = tokio::try_join!(injector_task, http_task)?;
    injector_res?;
    http_res?;
    Ok(())
}
