//! Headless IPC client used to drive a running `seed-daemon` from scripts and
//! the loopback integration harness (Checkpoint #1). Filled in alongside the
//! IPC server in M2.

use clap::Parser;

#[derive(Parser)]
#[command(name = "seed-cli", version, about)]
struct Cli {
    /// Path to the daemon's IPC socket / named pipe.
    #[arg(long)]
    socket: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let _cli = Cli::parse();
    println!("seed-cli skeleton — IPC client lands in M2");
    Ok(())
}
