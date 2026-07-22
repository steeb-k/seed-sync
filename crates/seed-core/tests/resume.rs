//! Large-file download resilience across interruptions — the coverage gap flagged
//! in commit 7c1857e ("instrument if paste-then-rename recurs on a healthy mesh")
//! and root-caused in the field to the suspend/resume cycle: a laptop that sleeps
//! mid-transfer must still converge a multi-part swarm download.
//!
//! We synthesize a suspend WITHOUT sleeping the machine. From the app's point of
//! view a suspend is just "the in-flight transfer is cancelled and the process may
//! come back with a fresh engine on the same on-disk store." Two levers reproduce
//! that in-process:
//!   * `set_paused(true)` aborts the in-flight swarm download (exactly what the
//!     pause path does on a real suspend — see engine.rs downloads_inflight abort);
//!   * dropping the engine and reopening `Engine::new` on the same data dir is the
//!     daemon restart that heals the field bug.
//!
//! `#[ignore]` (opens real iroh endpoints); run serially:
//!   cargo test -p seed-core --test resume -- --ignored --nocapture --test-threads 1

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use common::{gen_bytes, snapshot};
use seed_core::Engine;

/// A file big enough that a swarm download over loopback does NOT finish inside a
/// single reconcile poll, so we can deterministically catch it mid-transfer.
const BIG: usize = 192 * 1024 * 1024;

fn content_for(path: &str, size: usize) -> Vec<u8> {
    let mut v = path.as_bytes().to_vec();
    v.extend(gen_bytes(size.saturating_sub(v.len())));
    v.truncate(size);
    v
}

/// This share's self-reported content percent on `engine` (== `s.health`; the same
/// number the GUI shows as "Syncing NN%").
fn percent(engine: &Engine, share: &str) -> u8 {
    engine
        .list_summaries()
        .into_iter()
        .find(|s| s.share_id == share)
        .map(|s| s.percent)
        .unwrap_or(0)
}

fn has_file(folder: &Path, rel: &str, want: &[u8]) -> bool {
    std::fs::read(folder.join(rel))
        .map(|b| b == want)
        .unwrap_or(false)
}

/// Size of the largest `.bitfield` file directly under `dir` (iroh-blobs writes one
/// per *partial* blob; a complete blob has none). A non-empty bitfield means the
/// store persisted WHICH ranges are verified — the record a resume needs.
fn largest_bitfield_len(dir: &Path) -> u64 {
    let mut max = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "bitfield") {
                if let Ok(m) = p.metadata() {
                    max = max.max(m.len());
                }
            }
        }
    }
    max
}

/// Total bytes of all files under `dir` (recursively) — used to prove the partial
/// blob's data is on disk after a restart even if the store reports 0%.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(m) = p.metadata() {
                total += m.len();
            }
        }
    }
    total
}

/// Pump presence + reconcile for every engine once (the daemon loop the in-process
/// harness has to drive itself).
async fn tick(engines: &mut [(&mut Engine, String)]) {
    for (e, _) in engines.iter() {
        for j in e.presence_broadcasts() {
            j.send().await;
        }
        for r in e.presence_rejoins() {
            r.join().await;
        }
    }
    for (e, share) in engines.iter_mut() {
        let _ = e.reconcile(share).await;
    }
}

/// Build three co-masters (0 creates, 1 & 2 join, all bootstrapped to 0). Returns
/// engines + their folders + share/key so the test can own each lifecycle.
async fn three_masters() -> anyhow::Result<(
    String,
    String,
    Vec<Engine>,
    Vec<tempfile::TempDir>,
    Vec<tempfile::TempDir>,
)> {
    let mut datas = Vec::new();
    let mut folders = Vec::new();
    let mut engines = Vec::new();
    for i in 0..3 {
        let data = tempfile::tempdir()?;
        let folder = tempfile::tempdir()?;
        let e = Engine::new(data.path()).await?;
        e.set_device_name(&format!("master{i}"))?;
        e.wait_online().await;
        datas.push(data);
        folders.push(folder);
        engines.push(e);
    }
    let created = engines[0].create_share(folders[0].path(), vec![]).await?;
    let bootstrap = engines[0].endpoint_addr();
    for i in 1..3 {
        let id = engines[i]
            .add_share(&created.master_key, folders[i].path(), vec![bootstrap.clone()])
            .await?;
        assert_eq!(id, created.share_id);
    }
    Ok((
        created.share_id,
        created.master_key,
        engines,
        datas,
        folders,
    ))
}

/// A partial swarm download's progress must SURVIVE an engine restart on the same
/// data dir — the field bug was 219 MB of bytes on disk but a near-empty verified
/// bitfield, so every restart re-fetched from ~0 and a large file never converged
/// on a frequently-suspending laptop.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn partial_swarm_download_survives_restart() -> anyhow::Result<()> {
    let (share, master_key, mut engines, datas, folders) = three_masters().await?;

    // Node 2 holds back while 0 and 1 fully seed, so node 2 later pulls from TWO
    // full providers (the swarm path: size >= 4 MiB AND >= 2 online peers).
    engines[2].set_paused(&share, true)?;

    let big = content_for("big.iso", BIG);
    std::fs::write(folders[0].path().join("big.iso"), &big)?;

    // Drive until nodes 0 and 1 both hold big.iso (node 2 paused, excluded).
    {
        let mut set: Vec<(&mut Engine, String)> =
            engines.iter_mut().map(|e| (e, share.clone())).collect();
        let deadline = Duration::from_secs(180);
        let start = std::time::Instant::now();
        loop {
            tick(&mut set).await;
            if has_file(folders[0].path(), "big.iso", &big)
                && has_file(folders[1].path(), "big.iso", &big)
            {
                break;
            }
            if start.elapsed() > deadline {
                anyhow::bail!("seed on nodes 0+1 timed out");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    eprintln!("nodes 0+1 seeded big.iso");

    // Unpause node 2 and catch it well into the transfer: reconcile-poll until it
    // reports a *substantial* partial (>= MIN_FROZEN, < 100), then freeze it by
    // pausing (aborts the in-flight swarm). Freezing at a meaningful percent
    // matters — a couple of percent could be lost to chunk rounding and hide a
    // total collapse to 0.
    const MIN_FROZEN: u8 = 15;
    engines[2].set_paused(&share, false)?;
    let p_before;
    {
        let mut caught = None;
        for _ in 0..8000 {
            {
                let mut set: Vec<(&mut Engine, String)> =
                    engines.iter_mut().map(|e| (e, share.clone())).collect();
                tick(&mut set).await;
            }
            let p = percent(&engines[2], &share);
            if (MIN_FROZEN..100).contains(&p) {
                caught = Some(p);
                break;
            }
            if p >= 100 {
                anyhow::bail!("node 2 finished before reaching {MIN_FROZEN}% — raise BIG");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        p_before = caught.ok_or_else(|| {
            anyhow::anyhow!("node 2 never reached {MIN_FROZEN}% to freeze")
        })?;
    }
    engines[2].set_paused(&share, true)?;
    eprintln!("froze node 2 mid-transfer at {p_before}%");

    // Snapshot the partial on disk before the restart.
    let data2 = datas[2].path().to_path_buf();
    let before_bytes = dir_size(&data2.join("blobs"));

    // RESTART node 2 on the same data dir (the daemon restart / process wake).
    // Shut the old engine down FIRST so it releases the sqlite + blob store, THEN
    // reopen on the same dir. Opening a second engine while the old one still holds
    // the store would block on the file lock. Bound each step so a wedged endpoint
    // fails loudly instead of hanging. `node.shutdown()` calls `blobs.shutdown()`,
    // which flushes the partial's verified-range bitfield to disk.
    let node2 = engines.pop().expect("three engines"); // index 2 (last)
    let _ = tokio::time::timeout(Duration::from_secs(20), node2.shutdown()).await;
    let e2 = tokio::time::timeout(Duration::from_secs(30), Engine::new(&data2))
        .await
        .map_err(|_| anyhow::anyhow!("reopen engine timed out"))??;
    e2.set_device_name("master2")?;
    tokio::time::timeout(Duration::from_secs(30), e2.wait_online())
        .await
        .map_err(|_| anyhow::anyhow!("restarted engine did not come online in 30s"))?;

    // Assert the partial SURVIVED on disk with its verified-range bitfield intact.
    //
    // We check the on-disk store, not the live percent: a just-restarted node has
    // no peers yet, so nothing triggers iroh-blobs to load + report the partial
    // (`ensure_download` returns early with no providers). The bytes are on disk;
    // they're simply not counted until a download touches them. The property that
    // actually protects a suspend-prone laptop is that the ~N% already fetched, and
    // the record of WHICH ranges are valid (the bitfield), outlive the restart — so
    // the next attempt resumes instead of re-downloading from zero. Convergence with
    // peers present is covered by
    // `large_download_converges_despite_repeated_interruptions`.
    let summaries = e2.list_summaries();
    assert_eq!(summaries.len(), 1, "share must persist across restart");
    let after_bytes = dir_size(&data2.join("blobs"));
    let bitfield_bytes = largest_bitfield_len(&data2.join("blobs").join("data"));
    eprintln!(
        "after restart: blob store {} MiB -> {} MiB on disk; partial bitfield = {bitfield_bytes} bytes (frozen at {p_before}%)",
        before_bytes / (1024 * 1024),
        after_bytes / (1024 * 1024),
    );

    assert!(
        after_bytes * 10 >= before_bytes * 9,
        "partial DATA lost across restart: {} MiB -> {} MiB on disk",
        before_bytes / (1024 * 1024),
        after_bytes / (1024 * 1024),
    );
    assert!(
        bitfield_bytes > 0,
        "partial's verified-range bitfield was NOT persisted across restart — the store \
         would re-validate/re-download from scratch. `node.shutdown()` must call \
         `blobs.shutdown()` to flush it (vendor/iroh-blobs runtime note)."
    );

    let _ = tokio::time::timeout(Duration::from_secs(15), e2.shutdown()).await;
    for e in engines {
        let _ = tokio::time::timeout(Duration::from_secs(15), e.shutdown()).await;
    }
    let _ = (master_key, folders);
    Ok(())
}

/// A large swarm download interrupted REPEATEDLY (each interruption aborts the
/// in-flight transfer, as a suspend does) must still converge — progress has to
/// accumulate and the swarm must re-request only what's missing, not restart.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn large_download_converges_despite_repeated_interruptions() -> anyhow::Result<()> {
    let (share, _master_key, mut engines, _datas, folders) = three_masters().await?;

    engines[2].set_paused(&share, true)?;
    let big = content_for("big.iso", BIG);
    std::fs::write(folders[0].path().join("big.iso"), &big)?;

    // Seed nodes 0 and 1.
    {
        let mut set: Vec<(&mut Engine, String)> =
            engines.iter_mut().map(|e| (e, share.clone())).collect();
        let start = std::time::Instant::now();
        loop {
            tick(&mut set).await;
            if has_file(folders[0].path(), "big.iso", &big)
                && has_file(folders[1].path(), "big.iso", &big)
            {
                break;
            }
            if start.elapsed() > Duration::from_secs(180) {
                anyhow::bail!("seed on nodes 0+1 timed out");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    // Interrupt node 2 several times: let it advance past the last checkpoint,
    // then abort the in-flight transfer (pause) and immediately resume (unpause).
    // Progress must ACCUMULATE across interruptions — reaching a higher percent
    // each cycle proves the resume reuses what's on disk instead of restarting.
    // Read the LIVE percent while unpaused: a paused share reports 0.
    engines[2].set_paused(&share, false)?;
    let mut last = 0u8;
    for cycle in 0..8 {
        let mut reached = percent(&engines[2], &share);
        for _ in 0..400 {
            if reached >= 100 || reached > last {
                break;
            }
            {
                let mut set: Vec<(&mut Engine, String)> =
                    engines.iter_mut().map(|e| (e, share.clone())).collect();
                tick(&mut set).await;
            }
            reached = percent(&engines[2], &share);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        eprintln!("cycle {cycle}: reached {reached}% then interrupted (was {last}%)");
        assert!(
            reached + 2 >= last,
            "progress regressed across interruption {cycle}: {last}% -> {reached}%"
        );
        // Abort the in-flight swarm (as a suspend does), then resume.
        engines[2].set_paused(&share, true)?;
        engines[2].set_paused(&share, false)?;
        last = last.max(reached);
        if reached >= 100 {
            break;
        }
    }

    // Now let it run uninterrupted to full convergence.
    let want: BTreeMap<String, Vec<u8>> = BTreeMap::from([("big.iso".to_string(), big.clone())]);
    {
        let mut set: Vec<(&mut Engine, String)> =
            engines.iter_mut().map(|e| (e, share.clone())).collect();
        let start = std::time::Instant::now();
        loop {
            tick(&mut set).await;
            if snapshot(folders[2].path()) == want {
                break;
            }
            if start.elapsed() > Duration::from_secs(180) {
                anyhow::bail!(
                    "node 2 never converged after interruptions (stuck at {}%)",
                    percent(&engines[2], &share)
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    eprintln!("node 2 converged after repeated interruptions");

    for e in engines {
        let _ = tokio::time::timeout(Duration::from_secs(15), e.shutdown()).await;
    }
    Ok(())
}
