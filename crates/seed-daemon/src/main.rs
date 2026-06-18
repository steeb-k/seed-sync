//! Seed Sync daemon entry point.
//!
//! One binary, multiple runtime modes selected by subcommand:
//!   * `run`     — foreground console process (development, and Linux default).
//!   * `service` — entered by the Windows Service Control Manager (added in M4).
//!   * `install` / `uninstall` / `start` / `stop` — manage the Windows service.
//!
//! On non-Windows platforms the service subcommands are accepted but no-op with
//! a clear message; `run` is the real mode.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "seed-daemon", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Override the data directory (config DB + iroh stores). Defaults to the
    /// platform application-data location.
    #[arg(long, global = true)]
    data_dir: Option<std::path::PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon in the foreground.
    Run,
    /// Entered by the Windows SCM (Windows only).
    Service,
    /// Register the Windows service.
    Install,
    /// Remove the Windows service.
    Uninstall,
    /// Start the registered Windows service.
    Start,
    /// Stop the registered Windows service.
    Stop,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "seed_daemon=info,seed_core=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run(cli.data_dir),
        Command::Service => {
            #[cfg(windows)]
            {
                // M4: hand off to windows-service dispatcher.
                anyhow::bail!("service mode not yet implemented");
            }
            #[cfg(not(windows))]
            {
                anyhow::bail!("`service` mode is Windows-only; use `run`");
            }
        }
        Command::Install | Command::Uninstall | Command::Start | Command::Stop => {
            #[cfg(not(windows))]
            {
                tracing::warn!("service management is a no-op on this platform");
                Ok(())
            }
            #[cfg(windows)]
            {
                anyhow::bail!("service management not yet implemented");
            }
        }
    }
}

fn run(_data_dir: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        tracing::info!("seed-daemon starting (skeleton)");
        // M2: build the Engine, bind the IPC listener, enter the serve loop.
        Ok(())
    })
}
