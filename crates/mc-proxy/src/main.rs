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

    // F-Stack native-mt: one stack per lcore, one application worker per
    // stack. ff_init/ff_run must stay on the main thread, so this path cannot
    // go through Proxy::bind's Tokio runtime.
    #[cfg(feature = "fstack")]
    if config.workers > 1 {
        if !config.fstack.native_mt {
            error!(
                "workers > 1 on F-Stack requires [fstack] native_mt = true                  (thread_mode=1, one stack per lcore, physical NIC with RSS); \
                 otherwise run one process per core with workers = 1"
            );
            return ExitCode::FAILURE;
        }
        return match mc_proxy::proxy::run_fstack_multi(config) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                error!("proxy stopped: {err}");
                ExitCode::FAILURE
            }
        };
    }

    // F-Stack requires ff_init/ff_run on the main thread. run_with_stack
    // takes care of both backends: on the standard backend it just calls the
    // closure; on F-Stack it initialises on this thread, runs the application
    // on another thread, and enters ff_run here.
    let stack_config =
        tokio_fstack::Config::default().with_eal_args(config.fstack.eal_args.iter().cloned());
    let app_config = config;
    let result = tokio_fstack::run_with_stack(stack_config, move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("mc-proxy-worker")
            .build()?;
        runtime.block_on(async move {
            let proxy = Proxy::bind(app_config)
                .await
                .map_err(|err| std::io::Error::other(err.to_string()))?;
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
            result
        })
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!("proxy stopped: {err}");
            ExitCode::FAILURE
        }
    }
}
