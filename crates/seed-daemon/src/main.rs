//! SEED Sync daemon entry point.
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
use seed_core::Engine;
use seed_ipc::transport::{self, read_frame, write_frame};
use seed_ipc::{Frame, IpcEvent, IpcRequest, IpcResponse, Message};
use tokio::sync::{broadcast, mpsc, Mutex};

/// Diagnostic allocator for the fleet-soak OOM hunt (known-issues #11): daemons
/// died on single doubling allocations (5 → 80 GiB), so logging a backtrace at
/// the moment a huge allocation happens names the exact growth site. Wraps the
/// system allocator; does nothing beyond delegation unless a single request is
/// ≥ [`ALLOC_TRACE_MIN`]. Backtrace capture itself allocates, so a thread-local
/// guard breaks the recursion. Zero overhead on the hot path beyond one branch.
struct TraceAlloc;

/// Threshold for logging an allocation's backtrace: far above anything the
/// daemon legitimately allocates in one call (the whole soak corpus is ~0.5 GB),
/// far below where the leak killed nodes.
const ALLOC_TRACE_MIN: usize = 512 * 1024 * 1024;

thread_local! {
    static IN_ALLOC_TRACE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

unsafe impl std::alloc::GlobalAlloc for TraceAlloc {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        if layout.size() >= ALLOC_TRACE_MIN {
            IN_ALLOC_TRACE.with(|g| {
                if !g.get() {
                    g.set(true);
                    // stderr directly — tracing itself allocates and may be the
                    // one asking for this memory.
                    eprintln!(
                        "[alloc-trace] single allocation of {} bytes\n{}",
                        layout.size(),
                        std::backtrace::Backtrace::force_capture()
                    );
                    g.set(false);
                }
            });
        }
        unsafe { std::alloc::System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: TraceAlloc = TraceAlloc;

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

#[derive(Subcommand, Clone)]
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
    let cli = Cli::parse();
    let command = cli.command.clone().unwrap_or(Command::Run);
    init_logging(&command, cli.data_dir.clone());

    match command {
        Command::Run => run(cli.data_dir, cli.socket),
        Command::Service => {
            #[cfg(windows)]
            {
                service::run_as_service().map_err(Into::into)
            }
            #[cfg(not(windows))]
            anyhow::bail!("`service` mode is Windows-only; use `run`");
        }
        Command::Install => platform_service_cmd("install"),
        Command::Uninstall => platform_service_cmd("uninstall"),
        Command::Start => platform_service_cmd("start"),
        Command::Stop => platform_service_cmd("stop"),
    }
}

fn env_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "seed_daemon=info,seed_core=info".into())
}

/// Console commands (`run`, install/start/…) log to stderr. The SCM-launched
/// `service` has no console, so it logs to `<data_dir>\daemon.log` instead —
/// otherwise its diagnostics (e.g. a failed `create_share`) vanish.
fn init_logging(command: &Command, data_dir_override: Option<PathBuf>) {
    let _ = command;
    let _ = &data_dir_override;
    #[cfg(windows)]
    if matches!(command, Command::Service) {
        let dir = data_dir_override.unwrap_or_else(default_data_dir);
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("daemon.log"))
        {
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_env_filter(env_filter())
                .with_writer(std::sync::Mutex::new(file))
                .init();
            return;
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(env_filter())
        .init();
}

#[cfg(windows)]
mod service;

fn platform_service_cmd(cmd: &str) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        service::manage(cmd).map_err(Into::into)
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
        tracing::warn!("service management is Windows-only; no-op on this platform");
        Ok(())
    }
}

pub(crate) fn default_data_dir() -> PathBuf {
    // On Windows the daemon and the user-run GUI must agree on a single location
    // so they derive the same IPC socket/pipe even across the service↔user
    // account boundary; use the machine-wide dir rather than a per-profile one.
    #[cfg(windows)]
    {
        seed_ipc::machine_data_dir()
    }
    #[cfg(not(windows))]
    {
        directories::ProjectDirs::from("io.github", "steeb_k", "SeedSync")
            .map(|d| d.data_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".seed-data"))
    }
}

pub(crate) fn default_socket(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("seed.sock")
}

fn run(data_dir: Option<PathBuf>, socket: Option<PathBuf>) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let data_dir = data_dir.unwrap_or_else(default_data_dir);
        let socket = socket.unwrap_or_else(|| default_socket(&data_dir));
        // Console mode: run until Ctrl-C.
        serve(data_dir, socket, async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
    })
}

/// Run the daemon until `shutdown` resolves. Shared by console (`run`) and the
/// Windows service entry point.
pub(crate) async fn serve(
    data_dir: PathBuf,
    socket: PathBuf,
    shutdown: impl std::future::Future<Output = ()>,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(&data_dir)?;
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
    tokio::spawn(presence_loop(daemon.clone()));
    tokio::spawn(throughput_loop(daemon.clone()));

    let listener = transport::bind(&socket)?;
    tracing::info!("seed-daemon listening on {}", socket.display());

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => {
                tracing::info!("shutdown requested");
                break;
            }
            res = transport::accept(&listener) => match res {
                Ok(stream) => {
                    let d = daemon.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_conn(d, stream).await {
                            tracing::debug!("connection closed: {e}");
                        }
                    });
                }
                Err(e) => tracing::warn!("accept error: {e}"),
            }
        }
    }
    Ok(())
}

/// Periodically reconcile every share against the merged replica.
///
/// Each share's reconcile runs *off* the engine lock: we build a job under a
/// brief lock, do the heavy work (hash the folder, stream blobs in/out) while
/// unlocked so a big folder can't freeze the GUI, then re-lock briefly to commit
/// the result. This drives multi-master convergence — every node (master or
/// viewer) imports its local changes and materializes remote ones.
async fn reconcile_loop(daemon: Daemon) {
    let mut tick = tokio::time::interval(Duration::from_millis(750));
    loop {
        tick.tick().await;
        let mut changed = Vec::new();

        let ids = { daemon.engine.lock().await.share_ids() };
        for id in ids {
            // Build the job (brief lock) -> run (no lock) -> commit (brief lock).
            let job = {
                daemon
                    .engine
                    .lock()
                    .await
                    .make_reconcile_job(&id)
                    .ok()
                    .flatten()
            };
            let Some(job) = job else { continue };
            let job_started = std::time::Instant::now();
            // Slow-pass watchdog (known-issues #7): a pass that never returns
            // wedges this share silently — `publishing` stays set, so no new job
            // is ever built and the share just stops, with nothing in the log
            // (fleet soak: nodes stuck at 0% for the whole run, 2 log lines).
            // WARN once a minute with the job's phase breadcrumb so a wedge is
            // visible and localized. Diagnostic only — the pass is not aborted.
            let phase = job.phase_handle();
            let run = job.run();
            let mut run = std::pin::pin!(run);
            let outcome = loop {
                match tokio::time::timeout(Duration::from_secs(60), &mut run).await {
                    Ok(res) => break res,
                    Err(_) => {
                        let at = phase
                            .lock()
                            .map(|g| g.clone())
                            .unwrap_or_else(|_| "?".into());
                        tracing::warn!(
                            "reconcile {id} pass still running after {}s — phase: {at}",
                            job_started.elapsed().as_secs(),
                        );
                    }
                }
            };
            match outcome {
                Ok(outcome) => {
                    let did = outcome.changed();
                    daemon
                        .engine
                        .lock()
                        .await
                        .finish_reconcile(&id, Some(outcome));
                    if did {
                        changed.push(id.clone());
                    }
                }
                Err(e) => {
                    tracing::warn!("reconcile {id} failed: {e:#}");
                    daemon.engine.lock().await.finish_reconcile(&id, None);
                }
            }
            // A long pass (e.g. materializing a multi-GB blob to disk) is normal
            // during a big sync, but it delays the other shares' reconciles —
            // surface it. Presence/health no longer ride this loop, so it can't
            // starve the mesh anymore (that fleet-wide outage came from exactly
            // this: minutes-long passes silencing our broadcasts, peers aging us
            // offline at the 20s TTL).
            let took = job_started.elapsed();
            if took > Duration::from_secs(30) {
                tracing::info!("reconcile {id} pass took {}s", took.as_secs());
            }
        }

        // Retry any deferred cross-volume blob reclaims.
        daemon.engine.lock().await.retry_reclaims();

        // A changed share just moved its seqno/health: announce immediately
        // rather than waiting for the presence loop's next 3s beat.
        if !changed.is_empty() {
            let jobs = { daemon.engine.lock().await.presence_broadcasts() };
            for job in jobs {
                job.send().await;
            }
            let ts = now_unix();
            let _ = daemon.events.send(IpcEvent::ShareListChanged);
            for share_id in &changed {
                let _ = daemon.events.send(IpcEvent::LastUpdated {
                    share_id: share_id.clone(),
                    ts,
                });
            }
            tracing::debug!("reconcile changed {} share(s)", changed.len());
        }
    }
}

/// Presence, gossip-mesh repair, divergence self-heal kicks, the periodic deep
/// verify, long-term health alerts, and the GUI membership poke — everything
/// whose *cadence* matters — on their own task, decoupled from reconcile.
///
/// Split out of `reconcile_loop` after the full-size soak: a reconcile pass
/// that spends minutes materializing a multi-GB blob ran presence on its tail,
/// so under a big initial sync every node fell silent for minutes at a time,
/// peers aged each other offline (20s TTL), and the whole mesh read "1 of N
/// online" for as long as the sync lasted — which also blinded the health
/// detector and degraded swarm provider selection. Network dials (mesh joins,
/// doc resyncs) are spawned detached with a timeout so one hung peer can't
/// stall this loop either.
async fn presence_loop(daemon: Daemon) {
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    let mut tick_n: u64 = 0;
    // Last per-share membership/status fingerprint. A peer aging online/offline
    // produces no reconcile change, so without this the GUI would never re-query
    // and would show a stale "N of M" / dot until the next content change.
    let mut last_membership: u64 = 0;
    loop {
        tick.tick().await;
        tick_n = tick_n.wrapping_add(1);

        // Announce this device's name + health to each share's pool. Built
        // under a brief lock, sent off-lock (queued to the gossip actor).
        let jobs = { daemon.engine.lock().await.presence_broadcasts() };
        for job in jobs {
            job.send().await;
        }

        // Every ~6s: repair each share's gossip mesh (gossip's one-shot
        // bootstrap leaves a fragile star; the engine picks a few unheard
        // members to join — never the full roster, which fragments the overlay
        // at fleet scale), re-kick doc live-sync for out-of-sync shares,
        // schedule due deep verifies, and emit health alerts. Dials run
        // detached + bounded.
        if tick_n.is_multiple_of(2) {
            let rejoins = { daemon.engine.lock().await.presence_rejoins() };
            for rejoin in rejoins {
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(Duration::from_secs(30), rejoin.join()).await;
                });
            }
            let resyncs = { daemon.engine.lock().await.diverged_doc_resyncs() };
            for resync in resyncs {
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(Duration::from_secs(30), resync.run()).await;
                });
            }
            let verified = { daemon.engine.lock().await.periodic_deep_verify() };
            if !verified.is_empty() {
                tracing::info!(
                    "periodic deep verify scheduled for {} share(s)",
                    verified.len()
                );
            }
            // Recycle downloads that have been in flight implausibly long: a
            // wedged future otherwise blocks its blob's re-queue forever.
            let _ = { daemon.engine.lock().await.abort_stalled_downloads() };
            // Long-term member health: collect due alerts under the brief lock
            // (the detector is sync, transition-only sqlite writes), emit to
            // subscribers off-lock. The GUI turns these into toast + OS
            // notifications on this device and on every master.
            let alerts = { daemon.engine.lock().await.health_alerts() };
            for a in alerts {
                let _ = daemon.events.send(IpcEvent::PeerHealth {
                    share_id: a.share_id,
                    share_name: a.share_name,
                    node_id: a.node_id,
                    name: a.name,
                    percent: a.percent,
                    unhealthy_secs: a.unhealthy_secs,
                    is_self: a.is_self,
                    recovered: a.recovered,
                });
            }
        }

        // Fingerprint the visible per-share state (membership counts + status)
        // so peer online/offline transitions refresh the GUI even on an
        // otherwise idle tick.
        let membership = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            for s in daemon.engine.lock().await.list_summaries() {
                s.share_id.hash(&mut h);
                s.online.hash(&mut h);
                s.total.hash(&mut h);
                (s.status as u8).hash(&mut h);
                s.percent.hash(&mut h);
                s.paused.hash(&mut h);
            }
            h.finish()
        };
        if membership != last_membership {
            last_membership = membership;
            let _ = daemon.events.send(IpcEvent::ShareListChanged);
        }
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Sample endpoint byte counters once a second and broadcast the throughput.
async fn throughput_loop(daemon: Daemon) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut last: Option<(u64, u64)> = None;
    loop {
        tick.tick().await;
        let (sent, recv, indexing) = {
            let engine = daemon.engine.lock().await;
            let (s, r) = engine.byte_totals();
            (s, r, engine.publishing_active())
        };
        if let Some((psent, precv)) = last {
            // Counters are monotonic; saturating_sub guards a reset.
            let up = sent.saturating_sub(psent);
            let down = recv.saturating_sub(precv);
            let _ = daemon.events.send(IpcEvent::Throughput {
                down_bps: down,
                up_bps: up,
            });
        }
        last = Some((sent, recv));
        // While an off-lock import is running, nudge clients to re-query so the
        // indexing percent visibly advances.
        if indexing {
            let _ = daemon.events.send(IpcEvent::ShareListChanged);
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
        Err(e) => {
            // `{e:#}` prints the full anyhow context chain (e.g. "scan folder:
            // Access is denied."), which is what we need to diagnose failures
            // under the service where there's no console.
            tracing::warn!("request failed: {e:#}");
            IpcResponse::Err(e.to_string())
        }
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
            // Open + persist the share under a brief lock, then run the initial
            // import *without* the lock so a large folder doesn't block other
            // requests (ListShares, status, etc.). The share is visible in the
            // list while it imports; if the import errors it's just retried by the
            // reconcile loop, so creation itself still succeeds.
            let (created, job) = {
                let mut engine = daemon.engine.lock().await;
                engine.create_open(&PathBuf::from(folder), ignore).await?
            };
            let _ = daemon.events.send(IpcEvent::ShareListChanged);
            let outcome = job.run().await;
            if let Err(e) = &outcome {
                tracing::warn!("initial import for {} failed: {e:#}", created.share_id);
            }
            {
                let mut engine = daemon.engine.lock().await;
                engine.finish_reconcile(&created.share_id, outcome.ok());
            }
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
            // Open + persist under a brief lock, then start live-sync *off* the lock:
            // `start_sync` dials the bootstrap peer over the network, and holding the
            // engine mutex across that dial would freeze the reconcile loop and every
            // other IPC request if the peer is slow/unreachable (mirrors CreateShare).
            let (share_id, sync) = {
                let mut engine = daemon.engine.lock().await;
                engine
                    .add_share_open(&key, &PathBuf::from(folder), boot)
                    .await?
            };
            let _ = daemon.events.send(IpcEvent::ShareListChanged);
            if let Err(e) = sync.start().await {
                tracing::warn!("start sync for added share {share_id} failed: {e:#}");
            }
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
        IpcRequest::Pause { share_id } => {
            daemon.engine.lock().await.set_paused(&share_id, true)?;
            let _ = daemon.events.send(IpcEvent::ShareListChanged);
            IpcResponse::Ok
        }
        IpcRequest::Resume { share_id } => {
            daemon.engine.lock().await.set_paused(&share_id, false)?;
            let _ = daemon.events.send(IpcEvent::ShareListChanged);
            IpcResponse::Ok
        }
        IpcRequest::PauseAll => {
            daemon.engine.lock().await.set_paused_all(true)?;
            let _ = daemon.events.send(IpcEvent::ShareListChanged);
            IpcResponse::Ok
        }
        IpcRequest::ResumeAll => {
            daemon.engine.lock().await.resume_all()?;
            let _ = daemon.events.send(IpcEvent::ShareListChanged);
            IpcResponse::Ok
        }
        IpcRequest::RemoveShare {
            share_id,
            delete_files,
        } => {
            daemon
                .engine
                .lock()
                .await
                .remove_share(&share_id, delete_files)
                .await?;
            let _ = daemon.events.send(IpcEvent::ShareListChanged);
            IpcResponse::Ok
        }
        IpcRequest::SetSettings(_) => IpcResponse::Ok,
        IpcRequest::GetPeers { share_id } => {
            let engine = daemon.engine.lock().await;
            IpcResponse::Peers(engine.peers(&share_id)?)
        }
        IpcRequest::GetPeerHealth { share_id } => {
            let engine = daemon.engine.lock().await;
            IpcResponse::PeerHealth(engine.peer_health(&share_id)?)
        }
        IpcRequest::GetDeviceName => {
            IpcResponse::DeviceName(daemon.engine.lock().await.device_name())
        }
        IpcRequest::SetDeviceName { name } => {
            // Persist the new name and push an immediate presence broadcast so
            // peers see the rename within a tick rather than the periodic cycle.
            let jobs = {
                let engine = daemon.engine.lock().await;
                engine.set_device_name(&name)?;
                engine.presence_broadcasts()
            };
            for job in jobs {
                job.send().await;
            }
            let _ = daemon.events.send(IpcEvent::ShareListChanged);
            IpcResponse::Ok
        }
        IpcRequest::GetSettings => IpcResponse::Settings(seed_ipc::Settings::default()),
        IpcRequest::Subscribe => IpcResponse::Ok, // handled before dispatch
    })
}
