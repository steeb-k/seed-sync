//! seed-soak — spawn a fleet of real `seed-daemon` processes on one machine,
//! share a deterministic corpus between them, watch health/CPU/convergence for
//! the run's duration, and write a timestamped markdown report. The watched
//! production-readiness gate (see `docs/production-readiness-plan.md`).
//!
//! Two standard runs:
//!   seed-soak fleet    --root D:\seed-soak      # 3 masters + 25 viewers, scaled corpus (~1.2 GB)
//!   seed-soak fullsize --root D:\seed-soak-full # 3 masters + 3 viewers, full corpus (~33 GB, 6 ISOs)
//!   seed-soak clean    --root D:\seed-soak      # kill strays + delete the tree
//!
//! Scenario flags: `--churn <secs>` (deterministic edits on a rotating master),
//! `--degrade-viewer <idx>` + `--degrade-at <secs>` (pause that viewer, resume
//! at half-duration, assert the degraded→recovered PeerHealth pair arrives),
//! `--conflict` (same-path writes on two masters, ≥1.1 s apart, plus one
//! deliberate sub-second race that is observed, not asserted),
//! `--health-secs A/R` (shrink the 12 h/8 h thresholds for the run).
//! Ctrl-C finalizes the report instead of losing the run.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand};
use seed_harness::corpus::{self, CorpusSpec};
use seed_harness::proc::{request, wait_for_socket, DaemonSpawn, Daemons};
use seed_ipc::transport::{self, read_frame, write_frame};
use seed_ipc::{Frame, IpcEvent, IpcRequest, IpcResponse, Message};

#[derive(Parser)]
#[command(name = "seed-soak", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fleet-scale soak: production peer count, scaled corpus.
    Fleet(RunArgs),
    /// Full-size content soak: few nodes, real 3–6 GB ISOs (~33 GB/copy).
    Fullsize(RunArgs),
    /// Mid-size throughput reading (~6–8 GB/copy): rates stabilize in minutes.
    Midsize(RunArgs),
    /// Kill any daemons recorded in `<root>/pids.txt` and delete the tree.
    Clean {
        #[arg(long)]
        root: PathBuf,
    },
}

#[derive(Args)]
struct RunArgs {
    /// Working root; each node gets `<root>/node-NN/{data,folder,daemon.log}`.
    #[arg(long)]
    root: PathBuf,
    #[arg(long, default_value_t = 3)]
    masters: usize,
    #[arg(long)]
    viewers: Option<usize>,
    /// Run length, seconds (poll/report window after setup).
    #[arg(long, default_value_t = 3600)]
    duration: u64,
    /// Path to a release seed-daemon binary.
    #[arg(long, default_value = "target/release/seed-daemon.exe")]
    daemon_bin: PathBuf,
    /// Mutate a deterministic fraction of files on a rotating master every N seconds.
    #[arg(long)]
    churn: Option<u64>,
    /// Pause this viewer index (0-based among viewers) mid-run…
    #[arg(long)]
    degrade_viewer: Option<usize>,
    /// …at this many seconds into the run (resumed at half-duration).
    #[arg(long, default_value_t = 300)]
    degrade_at: u64,
    /// Same-path conflicting writes on masters 0 and 1 (ordered + one sub-second race).
    #[arg(long, default_value_t = false)]
    conflict: bool,
    /// Shrink health thresholds: "UNHEALTHY/RENOTIFY" in seconds (e.g. 60/120).
    #[arg(long)]
    health_secs: Option<String>,
    /// Poll/sample interval, seconds.
    #[arg(long, default_value_t = 30)]
    interval: u64,
    /// Alternate working root for the LAST `--alt-nodes` nodes (split-disk A/B:
    /// e.g. seeder + most nodes on an SSD root, the tail nodes on an HDD root,
    /// so per-node rates in the SAME run compare disk classes with identical
    /// machine/background conditions).
    #[arg(long)]
    alt_root: Option<PathBuf>,
    /// How many trailing nodes live under `--alt-root`.
    #[arg(long, default_value_t = 2)]
    alt_nodes: usize,
    /// Run mutation scenarios WITHOUT waiting for all masters to reach
    /// Healthy @ 100% — deliberately exercises deletes racing still-seeding
    /// masters (known-issues #12, fixed via timestamped tombstones). The
    /// ordered-conflict assertion is publish-lag-sensitive ungated (#5), so
    /// combine with churn only, not --conflict.
    #[arg(long, default_value_t = false)]
    no_scenario_gate: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Fleet(mut a) => {
            a.viewers.get_or_insert(25);
            run(a, CorpusSpec::scaled(), "fleet").await
        }
        Command::Fullsize(mut a) => {
            a.viewers.get_or_insert(3);
            run(a, CorpusSpec::full(), "fullsize").await
        }
        Command::Midsize(mut a) => {
            a.viewers.get_or_insert(1);
            run(a, CorpusSpec::midsize(), "midsize").await
        }
        Command::Clean { root } => clean(&root),
    }
}

/// A `request` that can't stall the sample loop: a daemon that is slow to answer
/// (observed during heavy materialization) costs one skipped sample, not minutes
/// of blind time between samples.
///
/// The request runs on its OWN spawned task and the timeout waits on the
/// JoinHandle: a plain `timeout(request(...))` proved insufficient — the
/// fullsize HDD soak froze forever pre-report with zero CPU/IO, consistent
/// with the connect/read blocking its worker thread inside poll, where a
/// same-task timeout can never fire. Timing out the JoinHandle always fires;
/// a truly stuck request leaks one abandoned task instead of the whole run
/// (minidump of the hang: seed-soak-hang-2720.dmp).
async fn request_bounded(sock: &std::path::Path, req: IpcRequest) -> anyhow::Result<IpcResponse> {
    let sock = sock.to_path_buf();
    let handle = tokio::spawn(async move { request(&sock, req).await });
    match tokio::time::timeout(Duration::from_secs(15), handle).await {
        Ok(Ok(res)) => res,
        Ok(Err(join)) => Err(anyhow::anyhow!("IPC request task failed: {join}")),
        Err(_) => Err(anyhow::anyhow!("IPC request timed out (15s)")),
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn clean(root: &Path) -> anyhow::Result<()> {
    let pids_file = root.join("pids.txt");
    if let Ok(text) = std::fs::read_to_string(&pids_file) {
        let mut sys = sysinfo::System::new();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        for line in text.lines() {
            if let Ok(pid) = line.trim().parse::<u32>() {
                if let Some(p) = sys.process(sysinfo::Pid::from_u32(pid)) {
                    // Only kill what is actually still a seed-daemon (pid reuse).
                    if p.name().to_string_lossy().contains("seed-daemon") {
                        println!("killing stray seed-daemon pid {pid}");
                        p.kill();
                    }
                }
            }
        }
    }
    if root.exists() {
        println!("removing {}", root.display());
        std::fs::remove_dir_all(root)?;
    }
    Ok(())
}

struct NodeHandle {
    idx: usize,
    is_master: bool,
    /// This node's working dir (`<root or alt_root>/node-NN`).
    dir: PathBuf,
    sock: PathBuf,
    folder: PathBuf,
    pid: u32,
}

async fn run(a: RunArgs, spec: CorpusSpec, kind: &str) -> anyhow::Result<()> {
    let viewers = a.viewers.unwrap_or(0);
    let total = a.masters + viewers;
    let root = a.root.clone();
    std::fs::create_dir_all(&root)?;
    println!(
        "seed-soak {kind}: {} masters + {viewers} viewers, corpus ≤ {:.1} GB/copy, {}s under {}",
        a.masters,
        spec.max_bytes() as f64 / 1e9,
        a.duration,
        root.display()
    );
    anyhow::ensure!(
        a.daemon_bin.exists(),
        "daemon binary {} not found — build with `cargo build --release -p seed-daemon`",
        a.daemon_bin.display()
    );

    // --- spawn the fleet ---
    let mut nodes = Vec::with_capacity(total);
    let mut children = Vec::with_capacity(total);
    let mut pids = String::new();
    let alt_from = a
        .alt_root
        .as_ref()
        .map(|_| total.saturating_sub(a.alt_nodes))
        .unwrap_or(total);
    for i in 0..total {
        let base = if i >= alt_from {
            a.alt_root.as_ref().unwrap()
        } else {
            &root
        };
        let dir = base.join(format!("node-{i:02}"));
        if i == alt_from {
            println!(
                "nodes {alt_from}..{} live under alt root {}",
                total - 1,
                base.display()
            );
        }
        let folder = dir.join("folder");
        std::fs::create_dir_all(&folder)?;
        let sock = dir.join("sock");
        let mut spawn = DaemonSpawn::new(&a.daemon_bin, dir.join("data"), &sock)
            .rust_log("seed_daemon=info,seed_core=info")
            .log_to(dir.join("daemon.log"));
        if let Some(hs) = &a.health_secs {
            let (u, r) = hs
                .split_once('/')
                .ok_or_else(|| anyhow::anyhow!("--health-secs wants UNHEALTHY/RENOTIFY"))?;
            spawn = spawn
                .env("SEED_HEALTH_UNHEALTHY_SECS", u.trim())
                .env("SEED_HEALTH_RENOTIFY_SECS", r.trim());
        }
        let child = spawn.spawn()?;
        let _ = writeln!(pids, "{}", child.id());
        nodes.push(NodeHandle {
            idx: i,
            is_master: i < a.masters,
            dir,
            sock,
            folder,
            pid: child.id(),
        });
        children.push(child);
    }
    let _guard = Daemons(children);
    std::fs::write(root.join("pids.txt"), &pids)?;
    for n in &nodes {
        wait_for_socket(&n.sock, Duration::from_secs(120)).await?;
    }
    println!("all {total} daemons up");

    // --- corpus + share wiring ---
    println!("generating corpus into node-00 (this is the slow part on fullsize)…");
    let gen_started = Instant::now();
    let mut manifest = corpus::generate(&nodes[0].folder, &spec)?;
    let total_bytes: u64 = manifest.values().map(|(s, _)| *s).sum();
    println!(
        "corpus: {} files / {:.2} GB in {:.0}s",
        manifest.len(),
        total_bytes as f64 / 1e9,
        gen_started.elapsed().as_secs_f64()
    );

    let IpcResponse::NodeAddr(bootstrap) = request(&nodes[0].sock, IpcRequest::NodeAddr).await?
    else {
        anyhow::bail!("expected NodeAddr");
    };
    let IpcResponse::ShareCreated {
        share_id,
        master_key,
        viewer_key,
    } = request(
        &nodes[0].sock,
        IpcRequest::CreateShare {
            folder: nodes[0].folder.to_string_lossy().into_owned(),
            generate_ignore: false,
            ignore: vec![],
        },
    )
    .await?
    else {
        anyhow::bail!("expected ShareCreated");
    };
    println!("share {share_id} created; joining members (staggered)…");
    for n in nodes.iter().skip(1) {
        let key = if n.is_master {
            master_key.clone()
        } else {
            viewer_key.clone()
        };
        let resp = request(
            &n.sock,
            IpcRequest::AddShare {
                key,
                folder: n.folder.to_string_lossy().into_owned(),
                bootstrap: Some(bootstrap.clone()),
            },
        )
        .await?;
        anyhow::ensure!(
            matches!(resp, IpcResponse::ShareAdded { .. }),
            "AddShare on node {} failed: {resp:?}",
            n.idx
        );
        // Stagger joins to mimic a rollout, not a thundering herd.
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    println!("all members joined");

    // Distinct display names (all nodes otherwise default to the hostname,
    // which makes the health-event log ambiguous).
    for n in &nodes {
        let role = if n.is_master { "master" } else { "viewer" };
        let _ = request(
            &n.sock,
            IpcRequest::SetDeviceName {
                name: format!("{role}-{:02}", n.idx),
            },
        )
        .await;
    }

    let start_unix = now_unix();
    // Subscribe on node-00 (a master): count PeerHealth events for the report.
    let health_events = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    {
        let sink = health_events.clone();
        let sock = nodes[0].sock.clone();
        tokio::spawn(async move {
            let Ok(stream) = transport::connect(&sock).await else {
                return;
            };
            let (mut reader, mut writer) = tokio::io::split(stream);
            let _ = write_frame(
                &mut writer,
                &Frame {
                    id: 9,
                    body: Message::Request(IpcRequest::Subscribe),
                },
            )
            .await;
            while let Ok(Some(frame)) = read_frame(&mut reader).await {
                if let Message::Event(IpcEvent::PeerHealth {
                    node_id,
                    name,
                    percent,
                    unhealthy_secs,
                    is_self,
                    recovered,
                    ..
                }) = frame.body
                {
                    sink.lock().unwrap().push(format!(
                        "t+{}s  {}  {}  {percent}%  {unhealthy_secs}s  self={is_self}",
                        now_unix() - start_unix,
                        if recovered { "RECOVERED" } else { "UNHEALTHY" },
                        name.unwrap_or(node_id),
                    ));
                }
            }
        });
    }

    // --- watch loop ---
    let started = Instant::now();
    let mut csv = std::fs::File::create(root.join("samples.csv"))?;
    writeln!(
        csv,
        "unix,elapsed_s,node,role,status,percent,online,total,retrying,cpu_pct,rss_mb"
    )?;
    let mut sys = sysinfo::System::new();
    let mut anomalies: Vec<String> = Vec::new();
    let mut out_of_sync_since: BTreeMap<usize, u64> = BTreeMap::new();
    let mut hot_since: BTreeMap<usize, u64> = BTreeMap::new();
    let mut churn_round: u64 = 0;
    let mut last_churn = Instant::now();
    let mut degraded_done = false;
    let mut resumed_done = false;
    let mut conflict_done = false;
    let mut interrupted = false;
    // Churn/conflict wait for every master to finish its initial import+publish
    // (Healthy @ 100%). Mutating mid-seeding asserts semantics the engine
    // deliberately does not provide: a churn DELETE races the other masters'
    // still-pending initial publish of the same path — deletion is absence in
    // the doc, indistinguishable from never-seen, so the slower master
    // re-publishes the file and resurrects it fleet-wide (known-issues #12);
    // and gross publish lag inverts wall-clock LWW expectations (#5). Fleet
    // soak #7 converged 28/28 byte-identical but FAILed verify on exactly
    // those two races. Steady-state mutation is what the product promises.
    let mut fleet_seeded = a.no_scenario_gate;

    while started.elapsed() < Duration::from_secs(a.duration) {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(a.interval)) => {}
            _ = tokio::signal::ctrl_c() => {
                println!("\nCtrl-C — finalizing report");
                anomalies.push(format!("t+{}s run interrupted by Ctrl-C", started.elapsed().as_secs()));
                interrupted = true;
            }
        }
        if interrupted {
            break;
        }
        let elapsed = started.elapsed().as_secs();

        // Scenario: degrade / resume one viewer.
        if let Some(v) = a.degrade_viewer {
            let idx = a.masters + v;
            if !degraded_done && elapsed >= a.degrade_at && idx < nodes.len() {
                degraded_done = true;
                println!("t+{elapsed}s pausing viewer node-{idx:02} (degrade scenario)");
                let _ = request_bounded(
                    &nodes[idx].sock,
                    IpcRequest::Pause {
                        share_id: share_id.clone(),
                    },
                )
                .await;
            }
            if degraded_done && !resumed_done && elapsed >= a.duration / 2 {
                resumed_done = true;
                println!("t+{elapsed}s resuming viewer node-{idx:02}");
                let _ = request_bounded(
                    &nodes[idx].sock,
                    IpcRequest::Resume {
                        share_id: share_id.clone(),
                    },
                )
                .await;
            }
        }
        // Readiness gate for the mutation scenarios (see `fleet_seeded` above).
        if (a.churn.is_some() || a.conflict) && !fleet_seeded {
            let mut ready = 0;
            for n in nodes.iter().filter(|n| n.is_master) {
                if let Ok(IpcResponse::Shares(shares)) =
                    request_bounded(&n.sock, IpcRequest::ListShares).await
                {
                    if shares.iter().any(|s| {
                        s.share_id == share_id
                            && s.percent == 100
                            && format!("{:?}", s.status) == "Healthy"
                    }) {
                        ready += 1;
                    }
                }
            }
            if ready == a.masters {
                fleet_seeded = true;
                // Start the churn clock from readiness, not process start.
                last_churn = Instant::now();
                println!(
                    "t+{elapsed}s all {ready} masters Healthy @ 100% — mutation scenarios armed"
                );
            }
        }
        // Scenario: multi-master churn. Quiet tail: no round in the final
        // stretch of the window — a mutation fired at (or after) window close
        // races the convergence gate through the 45 s divergence settle window
        // (nodes still applying it read Healthy), and the verify then catches
        // stragglers mid-application as stale. Observed: round 11 at t+3606 of
        // a 3600 s window FAILed an otherwise-perfect run on two masters.
        const CHURN_QUIET_TAIL_SECS: u64 = 120;
        if let Some(churn_secs) = a.churn {
            if fleet_seeded
                && last_churn.elapsed() >= Duration::from_secs(churn_secs)
                && elapsed + CHURN_QUIET_TAIL_SECS < a.duration
            {
                last_churn = Instant::now();
                let m = (churn_round as usize) % a.masters;
                match corpus::mutate(&nodes[m].folder, &mut manifest, spec.seed, churn_round, 0.02)
                {
                    Ok(s) => println!(
                        "t+{elapsed}s churn round {churn_round} on node-{m:02}: {} rewritten, {} deleted, {} added",
                        s.rewritten.len(),
                        s.deleted.len(),
                        s.added.len()
                    ),
                    Err(e) => anomalies.push(format!("t+{elapsed}s churn failed: {e:#}")),
                }
                churn_round += 1;
            }
        }
        // Scenario: same-path conflict at ~1/3 duration.
        if a.conflict
            && !conflict_done
            && fleet_seeded
            && elapsed >= a.duration / 3
            && a.masters >= 2
        {
            conflict_done = true;
            println!("t+{elapsed}s conflict scenario: ordered then sub-second same-path writes");
            let path = "conflict/ordered.txt";
            for m in &nodes[..2] {
                std::fs::create_dir_all(m.folder.join("conflict"))?;
            }
            std::fs::write(nodes[0].folder.join(path), b"from m0")?;
            // Causal ordering: wait until m0's write is VISIBLE on m1 before
            // writing the newer version there. A blind sleep asserts wall-clock
            // ordering under unbounded publish lag, which no LWW system can
            // honor — fleet soak #7 (heavy load, reconcile passes lagging
            // minutes) converged unanimously on the OLDER write because m0's
            // doc record carried a later timestamp than m1's file mtime
            // (known-issues #5). "Edit made after seeing the other side" is
            // the ordering LWW must and does get right.
            let visible = tokio::time::timeout(Duration::from_secs(180), async {
                loop {
                    if std::fs::read(nodes[1].folder.join(path)).is_ok_and(|b| b == b"from m0") {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            })
            .await;
            if visible.is_err() {
                anomalies.push(format!(
                    "t+{elapsed}s ordered-conflict: m0's write not visible on m1 within 180s — \
                     ordered assertion may be unreliable this run"
                ));
            }
            tokio::time::sleep(Duration::from_millis(1200)).await;
            std::fs::write(nodes[1].folder.join(path), b"from m1 (newer, must win)")?;
            manifest.insert(path.to_string(), {
                let bytes = b"from m1 (newer, must win)";
                (bytes.len() as u64, {
                    let mut h = blake3::Hasher::new();
                    h.update(bytes);
                    h.finalize().to_hex().to_string()
                })
            });
            // Deliberate sub-second race: WHICH side wins is arbitrary by design
            // (documented LWW limitation — local mtime vs record timestamp), but
            // the fleet must still agree on ONE winner; final verification pins
            // the manifest to whatever node-00 converged to.
            std::fs::write(nodes[0].folder.join("conflict/race.txt"), b"m0 racer")?;
            std::fs::write(nodes[1].folder.join("conflict/race.txt"), b"m1 racer")?;
            anomalies.push(format!(
                "t+{elapsed}s sub-second same-path race injected (conflict/race.txt) — winner arbitrary by design, fleet-consistency still asserted"
            ));
        }

        // Sample every node.
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        for n in &nodes {
            let (cpu, rss_mb) = sys
                .process(sysinfo::Pid::from_u32(n.pid))
                .map(|p| (p.cpu_usage(), p.memory() / (1 << 20)))
                .unwrap_or((0.0, 0));
            let role = if n.is_master { "master" } else { "viewer" };
            match request_bounded(&n.sock, IpcRequest::ListShares).await {
                Ok(IpcResponse::Shares(shares)) => {
                    for s in shares.iter().filter(|s| s.share_id == share_id) {
                        let status = format!("{:?}", s.status);
                        writeln!(
                            csv,
                            "{},{},{},{},{},{},{},{},{},{:.1},{}",
                            now_unix(),
                            elapsed,
                            n.idx,
                            role,
                            status,
                            s.percent,
                            s.online,
                            s.total,
                            s.retrying,
                            cpu,
                            rss_mb
                        )?;
                        // Anomaly: OutOfSync or NoPeers sustained > 5 min. NoPeers is
                        // tracked on the same clock because a partitioned node is
                        // exactly as broken as a diverged one and, before it had a
                        // status of its own, was the *quieter* of the two — it read
                        // as Healthy, so a soak could sit on a fully-stranded node
                        // for hours and report nothing (known-issues #16/#17).
                        if status == "OutOfSync" || status == "NoPeers" {
                            let since = *out_of_sync_since.entry(n.idx).or_insert(elapsed);
                            if elapsed - since > 300 && (elapsed - since) < 300 + a.interval {
                                anomalies.push(format!(
                                    "t+{elapsed}s node-{:02} {status} sustained > 5 min",
                                    n.idx
                                ));
                            }
                        } else {
                            out_of_sync_since.remove(&n.idx);
                        }
                        // Anomaly: hot daemon (> 25% of a core) sustained > 5 min.
                        if cpu > 25.0 {
                            let since = *hot_since.entry(n.idx).or_insert(elapsed);
                            if elapsed - since > 300 && (elapsed - since) < 300 + a.interval {
                                anomalies.push(format!(
                                    "t+{elapsed}s node-{:02} CPU > 25% sustained > 5 min ({cpu:.0}%)",
                                    n.idx
                                ));
                            }
                        } else {
                            hot_since.remove(&n.idx);
                        }
                    }
                }
                Ok(other) => {
                    anomalies.push(format!(
                        "t+{elapsed}s node-{:02} ListShares unexpected: {other:?}",
                        n.idx
                    ));
                }
                Err(e) => {
                    anomalies.push(format!(
                        "t+{elapsed}s node-{:02} IPC failed (daemon dead?): {e:#}",
                        n.idx
                    ));
                }
            }
        }
        csv.flush()?;
        println!(
            "t+{elapsed}s sampled {total} nodes ({} anomalies, {} health events)",
            anomalies.len(),
            health_events.lock().unwrap().len()
        );
    }

    // --- final convergence check ---
    println!("run window over — waiting up to 10 min for full convergence, then verifying bytes…");
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut all_healthy = false;
    while Instant::now() < deadline && !interrupted {
        let mut healthy = 0;
        for n in &nodes {
            if let Ok(IpcResponse::Shares(shares)) =
                request_bounded(&n.sock, IpcRequest::ListShares).await
            {
                if shares
                    .iter()
                    .any(|s| s.share_id == share_id && format!("{:?}", s.status) == "Healthy")
                {
                    healthy += 1;
                }
            }
        }
        if healthy == total {
            all_healthy = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    // The sub-second race has an arbitrary but fleet-consistent winner: pin the
    // expected content to whatever node-00 settled on before verifying everyone.
    if conflict_done {
        let race = nodes[0].folder.join("conflict/race.txt");
        if race.is_file() {
            let size = std::fs::metadata(&race).map(|m| m.len()).unwrap_or(0);
            if let Ok(hex) = corpus::hash_file(&race) {
                manifest.insert("conflict/race.txt".to_string(), (size, hex));
            }
        }
    }
    let mut verify_lines = Vec::new();
    if !interrupted {
        for n in &nodes {
            let problems = corpus::verify(&n.folder, &manifest)?;
            if problems.is_empty() {
                verify_lines.push(format!("node-{:02}: byte-identical ✓", n.idx));
            } else {
                for p in problems.iter().take(10) {
                    verify_lines.push(format!("node-{:02}: {p}", n.idx));
                }
                if problems.len() > 10 {
                    verify_lines.push(format!(
                        "node-{:02}: … {} more mismatches",
                        n.idx,
                        problems.len() - 10
                    ));
                }
            }
        }
    }

    // Swarm-deadline retries: grep the daemon logs (usability-findings #7).
    let mut deadline_retries = 0usize;
    for n in &nodes {
        if let Ok(log) = std::fs::read_to_string(n.dir.join("daemon.log")) {
            deadline_retries += log.matches("deadline").count();
        }
    }

    // --- report ---
    let verdict = if interrupted {
        "INTERRUPTED (no verdict)"
    } else if all_healthy && verify_lines.iter().all(|l| l.ends_with('✓')) && anomalies.is_empty()
    {
        "PASS"
    } else if all_healthy && verify_lines.iter().all(|l| l.ends_with('✓')) {
        "PASS with anomalies (see timeline)"
    } else {
        "FAIL"
    };
    let mut report = String::new();
    let _ = writeln!(report, "# seed-soak {kind} report ({start_unix})\n");
    let _ = writeln!(
        report,
        "- config: {} masters + {viewers} viewers, corpus {} files / {:.2} GB, duration {}s, interval {}s",
        a.masters,
        manifest.len(),
        total_bytes as f64 / 1e9,
        a.duration,
        a.interval
    );
    let _ = writeln!(
        report,
        "- scenarios: churn={:?} degrade_viewer={:?} conflict={} health_secs={:?}",
        a.churn, a.degrade_viewer, a.conflict, a.health_secs
    );
    if let Some(alt) = &a.alt_root {
        let _ = writeln!(
            report,
            "- split roots: nodes {alt_from}..{} under {} (rest under {})",
            total - 1,
            alt.display(),
            root.display()
        );
    }
    let _ = writeln!(report, "- verdict: **{verdict}**");
    let _ = writeln!(
        report,
        "- all nodes Healthy at end: {all_healthy}; swarm-deadline log hits: {deadline_retries}\n"
    );
    let _ = writeln!(report, "## Convergence verification\n");
    for l in &verify_lines {
        let _ = writeln!(report, "- {l}");
    }
    let _ = writeln!(report, "\n## PeerHealth events (observed on node-00)\n");
    for e in health_events.lock().unwrap().iter() {
        let _ = writeln!(report, "- {e}");
    }
    let _ = writeln!(report, "\n## Anomaly timeline\n");
    if anomalies.is_empty() {
        let _ = writeln!(report, "- none");
    }
    for a in &anomalies {
        let _ = writeln!(report, "- {a}");
    }
    let _ = writeln!(
        report,
        "\nSamples: `samples.csv` next to this report. Per-daemon logs under `node-NN/daemon.log`."
    );
    let report_path = root.join(format!("report-{start_unix}.md"));
    std::fs::write(&report_path, report)?;
    println!(
        "report written: {} — verdict: {verdict}",
        report_path.display()
    );

    // Daemons are killed by the drop guard; data stays for inspection (`clean` removes).
    Ok(())
}
