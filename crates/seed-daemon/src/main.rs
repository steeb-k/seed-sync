//! Seed Sync daemon entry point.
//!
//! One binary, multiple runtime modes selected by subcommand:
//!   * `run`     — foreground console process (development, and Linux default).
//!   * `service` — entered by the Windows Service Control Manager (added in M4).
//!   * `install` / `uninstall` / `start` / `stop` — manage the Windows service.
//!
//! In `run` mode it hosts the [`seed_core::Engine`], serves the IPC protocol on
//! a local socket, and runs a background reconcile loop that applies updates to
//! viewer shares and republishes changed master shares.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use seed_core::{identity::Role, Engine};
use seed_ipc::transport::{self, read_frame, write_frame};
use seed_ipc::{Frame, IpcEvent, IpcRequest, IpcResponse, Message};
use tokio::sync::{broadcast, mpsc, Mutex};

#[derive(Parser)]
#[command(name = "seed-daemon", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Override the data directory (config + iroh stores). Defaults to the
    /// platform application-data location.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    /// Override the IPC socket path. Defaults to `<data_dir>/seed.sock`.
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
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

/// Shared daemon state handed to each connection handler.
#[derive(Clone)]
struct Daemon {
    engine: Arc<Mutex<Engine>>,
    events: broadcast::Sender<IpcEvent>,
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
        Command::Run => run(cli.data_dir, cli.socket),
        Command::Service => {
            #[cfg(windows)]
            {
                anyhow::bail!("service mode not yet implemented");
            }
            #[cfg(not(windows))]
            anyhow::bail!("`service` mode is Windows-only; use `run`");
        }
        Command::Install | Command::Uninstall | Command::Start | Command::Stop => {
            #[cfg(not(windows))]
            {
                tracing::warn!("service management is a no-op on this platform");
                Ok(())
            }
            #[cfg(windows)]
            anyhow::bail!("service management not yet implemented");
        }
    }
}

fn default_data_dir() -> PathBuf {
    directories::ProjectDirs::from("io.github", "steeb_k", "SeedSync")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".seed-data"))
}

fn run(data_dir: Option<PathBuf>, socket: Option<PathBuf>) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let data_dir = data_dir.unwrap_or_else(default_data_dir);
        std::fs::create_dir_all(&data_dir)?;
        let socket = socket.unwrap_or_else(|| data_dir.join("seed.sock"));

        let engine = Engine::new(&data_dir).await?;
        // Wait (briefly) for a complete address so NodeAddr is useful, but don't
        // block startup forever if relays are unreachable.
        let _ = tokio::time::timeout(Duration::from_secs(10), engine.wait_online()).await;

        let (events, _) = broadcast::channel::<IpcEvent>(128);
        let daemon = Daemon {
            engine: Arc::new(Mutex::new(engine)),
            events,
        };

        tokio::spawn(reconcile_loop(daemon.clone()));

        let listener = transport::bind(&socket)?;
        tracing::info!("seed-daemon listening on {}", socket.display());

        loop {
            match transport::accept(&listener).await {
                Ok(stream) => {
                    let d = daemon.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_conn(d, stream).await {
                            tracing::debug!("connection closed: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("accept error: {e}");
                }
            }
        }
    })
}

/// Periodically apply updates to viewer shares and republish changed masters.
async fn reconcile_loop(daemon: Daemon) {
    let mut tick = tokio::time::interval(Duration::from_millis(750));
    loop {
        tick.tick().await;
        let mut engine = daemon.engine.lock().await;
        let mut any_change = false;
        for (id, role) in engine.shares_roles() {
            let changed = match role {
                Role::Viewer => engine.apply(&id).await.unwrap_or(false),
                Role::Master => engine.publish_if_changed(&id).await.unwrap_or(false),
            };
            if changed {
                any_change = true;
                let _ = daemon.events.send(IpcEvent::ShareListChanged);
            }
        }
        drop(engine);
        if any_change {
            tracing::debug!("reconcile produced changes");
        }
    }
}

async fn handle_conn(daemon: Daemon, stream: transport::Stream) -> anyhow::Result<()> {
    let (mut reader, mut writer) = tokio::io::split(stream);

    // Outgoing frames (responses + pushed events) funnel through one channel so
    // the writer half is owned by a single task.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Frame>();
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if write_frame(&mut writer, &frame).await.is_err() {
                break;
            }
        }
    });

    loop {
        let Some(frame) = read_frame(&mut reader).await? else {
            break; // peer closed
        };
        let Message::Request(req) = frame.body else {
            continue; // ignore non-requests from clients
        };

        if matches!(req, IpcRequest::Subscribe) {
            // Forward broadcast events to this connection until it drops.
            let mut rx = daemon.events.subscribe();
            let tx = out_tx.clone();
            tokio::spawn(async move {
                while let Ok(ev) = rx.recv().await {
                    if tx
                        .send(Frame {
                            id: 0,
                            body: Message::Event(ev),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
            let _ = out_tx.send(Frame {
                id: frame.id,
                body: Message::Response(IpcResponse::Ok),
            });
            continue;
        }

        let resp = dispatch(&daemon, req).await;
        if out_tx
            .send(Frame {
                id: frame.id,
                body: Message::Response(resp),
            })
            .is_err()
        {
            break;
        }
    }

    drop(out_tx);
    let _ = writer_task.await;
    Ok(())
}

async fn dispatch(daemon: &Daemon, req: IpcRequest) -> IpcResponse {
    match handle_request(daemon, req).await {
        Ok(resp) => resp,
        Err(e) => IpcResponse::Err(e.to_string()),
    }
}

async fn handle_request(daemon: &Daemon, req: IpcRequest) -> anyhow::Result<IpcResponse> {
    Ok(match req {
        IpcRequest::ListShares => {
            let engine = daemon.engine.lock().await;
            IpcResponse::Shares(engine.list_summaries())
        }
        IpcRequest::CreateShare {
            folder,
            generate_ignore: _,
            ignore,
        } => {
            let created = {
                let mut engine = daemon.engine.lock().await;
                engine.create_share(&PathBuf::from(folder), ignore).await?
            };
            let _ = daemon.events.send(IpcEvent::ShareListChanged);
            IpcResponse::ShareCreated {
                share_id: created.share_id,
                master_key: created.master_key,
                viewer_key: created.viewer_key,
            }
        }
        IpcRequest::AddShare {
            key,
            folder,
            bootstrap,
        } => {
            let boot = match bootstrap {
                Some(s) => vec![Engine::parse_bootstrap(&s)?],
                None => vec![],
            };
            let share_id = {
                let mut engine = daemon.engine.lock().await;
                engine.add_share(&key, &PathBuf::from(folder), boot).await?
            };
            let _ = daemon.events.send(IpcEvent::ShareListChanged);
            IpcResponse::ShareAdded { share_id }
        }
        IpcRequest::Publish { share_id } => {
            let mut engine = daemon.engine.lock().await;
            engine.publish(&share_id).await?;
            IpcResponse::Ok
        }
        IpcRequest::NodeAddr => {
            let engine = daemon.engine.lock().await;
            IpcResponse::NodeAddr(engine.endpoint_ticket())
        }
        IpcRequest::RevealKeys { share_id } => {
            let engine = daemon.engine.lock().await;
            let (master_key, viewer_key) = engine.reveal_keys(&share_id)?;
            IpcResponse::Keys {
                master_key,
                viewer_key,
            }
        }
        // Stubbed in M2; fleshed out with the GUI in M3.
        IpcRequest::Pause { .. }
        | IpcRequest::Resume { .. }
        | IpcRequest::RemoveShare { .. }
        | IpcRequest::SetSettings(_) => IpcResponse::Ok,
        IpcRequest::GetPeers { .. } => IpcResponse::Peers(vec![]),
        IpcRequest::GetSettings => IpcResponse::Settings(seed_ipc::Settings::default()),
        IpcRequest::Subscribe => IpcResponse::Ok, // handled before dispatch
    })
}
