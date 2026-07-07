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
    /// List the peers (and their friendly names) for a share.
    Peers {
        #[arg(long)]
        share: String,
    },
    /// Open long-term-health episodes for a share's members (degraded 12h+
    /// tracking; empty when everyone is healthy).
    PeerHealth {
        #[arg(long)]
        share: String,
    },
    /// Print this device's display name.
    DeviceName,
    /// Set this device's display name (propagated to peers via presence).
    SetName {
        #[arg(long)]
        name: String,
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
        Command::Peers { share } => IpcRequest::GetPeers { share_id: share },
        Command::PeerHealth { share } => IpcRequest::GetPeerHealth { share_id: share },
        Command::DeviceName => IpcRequest::GetDeviceName,
        Command::SetName { name } => IpcRequest::SetDeviceName { name },
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
                let retrying = if s.retrying > 0 {
                    format!("  [{} retrying]", s.retrying)
                } else {
                    String::new()
                };
                println!(
                    "{}  {:?}  {:?} {}%{}  {}  {}",
                    s.share_id, s.role, s.status, s.percent, retrying, s.name, s.folder
                );
            }
        }
        IpcResponse::Peers(peers) => {
            if peers.is_empty() {
                println!("(no peers)");
            }
            for p in peers {
                println!(
                    "{}  name={}  {:?}  online={}  seqno={}  {}%",
                    p.node_id,
                    p.name.as_deref().unwrap_or("-"),
                    p.role,
                    p.online,
                    p.have_seqno,
                    p.percent
                );
            }
        }
        IpcResponse::PeerHealth(rows) => {
            if rows.is_empty() {
                println!("(no open health episodes)");
            }
            for r in rows {
                println!(
                    "{}  name={}  online={}  {}%  unhealthy={}s  alerted={}",
                    if r.node_id.is_empty() {
                        "this-device"
                    } else {
                        r.node_id.as_str()
                    },
                    r.name.as_deref().unwrap_or("-"),
                    r.online,
                    r.percent,
                    r.unhealthy_secs,
                    r.alerted
                );
            }
        }
        IpcResponse::DeviceName(n) => println!("{n}"),
        IpcResponse::Ok => println!("ok"),
        IpcResponse::Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        other => println!("{other:?}"),
    }
}
