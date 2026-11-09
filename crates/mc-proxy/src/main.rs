//! Command line entry point.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use mc_proxy::config::Config;
use mc_proxy::proxy::Proxy;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "mc-proxy",
    version,
    about = "Velocity-compatible Minecraft reverse proxy on F-Stack/DPDK or plain Linux"
)]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(short, long, env = "MC_PROXY_CONFIG", default_value = "config.toml")]
    config: PathBuf,

    /// Validate the configuration and exit.
    #[arg(long)]
    check: bool,

    /// Print an example configuration and exit.
    #[arg(long)]
    print_example: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate a forwarding secret suitable for `forwarding_secret`.
    GenSecret {
        /// Number of random bytes (hex encoded output is twice this long).
        #[arg(default_value_t = 32)]
        bytes: usize,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.print_example {
        print!("{}", include_str!("../config.example.toml"));
        return ExitCode::SUCCESS;
    }
    if let Some(Command::GenSecret { bytes }) = cli.command {
        match mc_proxy::crypto::random_bytes(bytes) {
            Ok(bytes) => {
                let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
                println!("{hex}");
                return ExitCode::SUCCESS;
            }
            Err(err) => {
                eprintln!("failed to generate secret: {err}");
                return ExitCode::FAILURE;
            }
        }
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let config = match Config::load(&cli.config) {
        Ok(config) => config,
        Err(err) => {
            error!("{err}");
            return ExitCode::FAILURE;
        }
    };
    if cli.check {
        info!(path = %cli.config.display(), "configuration is valid");
        return ExitCode::SUCCESS;
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("mc-proxy-worker")
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            error!("failed to start the Tokio runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(async move {
        let proxy = match Proxy::bind(config).await {
            Ok(proxy) => proxy,
            Err(err) => {
                error!("failed to start the proxy: {err}");
                return ExitCode::FAILURE;
            }
        };
        let shared = proxy.shared().clone();
        info!(
            backend = tokio_fstack::backend(),
            listen = %proxy.local_addr(),
            online_mode = shared.config.online_mode,
            workers = shared.config.workers,
            forwarding = shared.forwarding_enabled(),
            compression = ?shared.config.compression(),
            servers = shared
                .servers
                .iter()
                .map(|server| format!("{}={}", server.name, server.address))
                .collect::<Vec<_>>()
                .join(","),
            "mc-proxy started"
        );

        let result = tokio::select! {
            result = proxy.run() => result,
            _ = tokio::signal::ctrl_c() => {
                info!("shutdown signal received");
                Ok(())
            }
        };
        match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                error!("proxy stopped: {err}");
                ExitCode::FAILURE
            }
        }
    })
}
