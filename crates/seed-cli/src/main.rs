//! Headless IPC client for driving a running `seed-daemon` — used for scripted
//! testing and the loopback integration harness (Checkpoint #1).

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use seed_ipc::transport::{self, read_frame, write_frame};
use seed_ipc::{Frame, IpcRequest, IpcResponse, Message};

#[derive(Parser)]
#[command(name = "seed-cli", version, about)]
struct Cli {
    /// Path to the daemon's IPC socket.
    #[arg(long)]
    socket: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the daemon's dialable endpoint-ticket address.
    NodeAddr,
    /// Create a new master share for a folder.
    Create {
        #[arg(long)]
        folder: PathBuf,
        /// Ignore glob (repeatable).
        #[arg(long = "ignore")]
        ignore: Vec<String>,
    },
    /// Add an existing share from a key.
    Add {
        #[arg(long)]
        key: String,
        #[arg(long)]
        folder: PathBuf,
        /// Bootstrap endpoint-ticket (from `node-addr` on the other daemon).
        #[arg(long)]
        bootstrap: Option<String>,
    },
    /// List shares.
    List,
    /// Trigger a master republish.
    Publish {
        #[arg(long)]
        share: String,
    },
    /// Reveal a share's keys (master key only if this node is master).
    Reveal {
        #[arg(long)]
        share: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let req = match cli.command {
        Command::NodeAddr => IpcRequest::NodeAddr,
        Command::Create { folder, ignore } => IpcRequest::CreateShare {
            folder: folder.to_string_lossy().into_owned(),
            generate_ignore: false,
            ignore,
        },
        Command::Add {
            key,
            folder,
            bootstrap,
        } => IpcRequest::AddShare {
            key,
            folder: folder.to_string_lossy().into_owned(),
            bootstrap,
        },
        Command::List => IpcRequest::ListShares,
        Command::Publish { share } => IpcRequest::Publish { share_id: share },
        Command::Reveal { share } => IpcRequest::RevealKeys { share_id: share },
    };

    let resp = request(&cli.socket, req).await?;
    print_response(&resp);
    Ok(())
}

/// Send one request and wait for its correlated response.
async fn request(socket: &std::path::Path, req: IpcRequest) -> anyhow::Result<IpcResponse> {
    let stream = transport::connect(socket).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    let id = 1;
    write_frame(
        &mut writer,
        &Frame {
            id,
            body: Message::Request(req),
        },
    )
    .await?;

    loop {
        let Some(frame) = read_frame(&mut reader).await? else {
            anyhow::bail!("daemon closed the connection without responding");
        };
        if frame.id == id {
            if let Message::Response(resp) = frame.body {
                return Ok(resp);
            }
        }
        // ignore events / other frames
    }
}

fn print_response(resp: &IpcResponse) {
    match resp {
        IpcResponse::NodeAddr(t) => println!("{t}"),
        IpcResponse::ShareCreated {
            share_id,
            master_key,
            viewer_key,
        } => {
            println!("share_id: {share_id}");
            println!("master_key: {master_key}");
            println!("viewer_key: {viewer_key}");
        }
        IpcResponse::ShareAdded { share_id } => println!("share_id: {share_id}"),
        IpcResponse::Keys {
            master_key,
            viewer_key,
        } => {
            if let Some(m) = master_key {
                println!("master_key: {m}");
            }
            println!("viewer_key: {viewer_key}");
        }
        IpcResponse::Shares(shares) => {
            if shares.is_empty() {
                println!("(no shares)");
            }
            for s in shares {
                println!(
                    "{}  {:?}  {:?} {}%  {}  {}",
                    s.share_id, s.role, s.status, s.percent, s.name, s.folder
                );
            }
        }
        IpcResponse::Ok => println!("ok"),
        IpcResponse::Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        other => println!("{other:?}"),
    }
}
