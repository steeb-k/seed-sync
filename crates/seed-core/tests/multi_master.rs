//! Multi-master correctness suite: near-simultaneous writes across co-masters,
//! LWW conflicts, the empty↔non-empty cross-author flip (known-issues #4), and
//! the deep-verify force / rescan-policy regressions (known-issues #2, #3).
//!
//! `#[ignore]` because every test opens real iroh endpoints; run serially:
//!   cargo test -p seed-core --test multi_master -- --ignored --nocapture --test-threads 1

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::{cluster, gen_bytes, snapshot};

/// Deterministic per-path content: the path itself as a prefix (so no two files
/// collide) followed by varied bytes.
fn content_for(path: &str, size: usize) -> Vec<u8> {
    let mut v = path.as_bytes().to_vec();
    v.extend(gen_bytes(size.saturating_sub(v.len())));
    v.truncate(size);
    v
}

/// Three co-masters each write a disjoint batch of files in the same instant
/// (all writes land before any node reconciles). The union must reach every
/// node, fingerprints must agree, and nobody may report OutOfSync. Includes a
/// >4 MiB file per master so the swarm path participates.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn concurrent_distinct_files() -> anyhow::Result<()> {
    let mut c = cluster(3, 0).await?;

    // Simultaneous burst: every master writes its slice before anyone ticks.
    let mut want = BTreeMap::new();
    for (m, node) in c.nodes.iter().enumerate() {
        for i in 0..30 {
            let rel = format!("m{m}/file{i:03}.bin");
            let bytes = content_for(&rel, 1024 + i * 997);
            let abs = node.folder().join(&rel);
            std::fs::create_dir_all(abs.parent().unwrap())?;
            std::fs::write(&abs, &bytes)?;
            want.insert(rel, bytes);
        }
        let rel = format!("m{m}/swarm.bin");
        let bytes = content_for(&rel, 6 * 1024 * 1024);
        std::fs::write(node.folder().join(&rel), &bytes)?;
        want.insert(rel, bytes);
    }

    c.drive_until(
        Duration::from_secs(180),
        "3-master union convergence",
        |c| c.converged(&want),
    )
    .await?;
    println!(
        "3 masters converged on {} files ({} each) with agreeing fingerprints",
        want.len(),
        want.len() / 3
    );
    c.shutdown().await?;
    Ok(())
}

/// Same-path conflict between two masters resolves last-writer-wins in both
/// directions, with mtimes separated by >1 s. Sub-second same-path writes are a
/// documented race (known-issues #5, LWW compares local mtime vs record ts) and
/// are deliberately NOT asserted here.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn concurrent_same_file_ordered() -> anyhow::Result<()> {
    let mut c = cluster(2, 0).await?;

    // Seed the contested file from master 0 and converge.
    std::fs::write(c.nodes[0].folder().join("c.txt"), b"v1 from m0")?;
    let mut want = BTreeMap::new();
    want.insert("c.txt".to_string(), b"v1 from m0".to_vec());
    c.drive_until(Duration::from_secs(90), "seed converged", |c| {
        c.converged(&want)
    })
    .await?;

    // m1 edits strictly later → m1 wins everywhere.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    std::fs::write(c.nodes[1].folder().join("c.txt"), b"v2 from m1 (newer)")?;
    want.insert("c.txt".to_string(), b"v2 from m1 (newer)".to_vec());
    c.drive_until(Duration::from_secs(90), "m1 LWW win", |c| {
        c.converged(&want)
    })
    .await?;
    println!("LWW m1-newer OK");

    // And the reverse direction: m0 edits strictly later → m0 wins.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    std::fs::write(c.nodes[0].folder().join("c.txt"), b"v3 from m0 (newest)")?;
    want.insert("c.txt".to_string(), b"v3 from m0 (newest)".to_vec());
    c.drive_until(Duration::from_secs(90), "m0 LWW win", |c| {
        c.converged(&want)
    })
    .await?;
    println!("LWW m0-newer OK");

    c.shutdown().await?;
    Ok(())
}

/// Two masters each generate a deterministic corpus (hundreds/thousands of
/// small-to-mid files) into their folders concurrently; the union must
/// converge byte-identically on both, verified by streaming hashes. Also logs
/// wall-clock as the baseline for the scan-cost decision (usability-findings #5).
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn thousands_small_files() -> anyhow::Result<()> {
    use seed_harness::corpus::{self, CorpusSpec, SizeBucket};

    let mut c = cluster(2, 0).await?;

    // Distinct labels + seeds → disjoint filenames, overlapping dirs (realistic).
    let spec_a = CorpusSpec {
        seed: 0xA0,
        buckets: vec![
            SizeBucket {
                count: 700,
                min: 1 << 10,
                max: 32 << 10,
                label: "a-small",
            },
            SizeBucket {
                count: 60,
                min: 64 << 10,
                max: 512 << 10,
                label: "a-mid",
            },
            SizeBucket {
                count: 4,
                min: 5 << 20,
                max: 8 << 20,
                label: "a-swarm",
            },
        ],
        max_dir_depth: 3,
    };
    let spec_b = CorpusSpec {
        seed: 0xB1,
        buckets: vec![
            SizeBucket {
                count: 300,
                min: 1 << 10,
                max: 32 << 10,
                label: "b-small",
            },
            SizeBucket {
                count: 30,
                min: 64 << 10,
                max: 512 << 10,
                label: "b-mid",
            },
            SizeBucket {
                count: 2,
                min: 5 << 20,
                max: 8 << 20,
                label: "b-swarm",
            },
        ],
        max_dir_depth: 3,
    };
    let started = std::time::Instant::now();
    let man_a = corpus::generate(c.nodes[0].folder(), &spec_a)?;
    let man_b = corpus::generate(c.nodes[1].folder(), &spec_b)?;
    let mut union = man_a.clone();
    union.extend(man_b.clone());
    let total: u64 = union.values().map(|(s, _)| *s).sum();
    println!(
        "generated {} files / {:.1} MB in {:.1}s",
        union.len(),
        total as f64 / 1e6,
        started.elapsed().as_secs_f64()
    );

    let sync_started = std::time::Instant::now();
    c.drive_until(Duration::from_secs(600), "corpus union convergence", |c| {
        c.fps_agree()
            && (0..c.nodes.len()).all(|i| c.status(i) == "Healthy")
            && corpus::verify(c.nodes[0].folder(), &union)
                .map(|p| p.is_empty())
                .unwrap_or(false)
            && corpus::verify(c.nodes[1].folder(), &union)
                .map(|p| p.is_empty())
                .unwrap_or(false)
    })
    .await?;
    println!(
        "two masters converged {} files / {:.1} MB in {:.1}s",
        union.len(),
        total as f64 / 1e6,
        sync_started.elapsed().as_secs_f64()
    );
    c.shutdown().await?;
    Ok(())
}

/// Cross-author empty↔non-empty flip (known-issues #4 regression): when master
/// B's content entry and master A's empty marker are both live for one path,
/// every member must resolve to the timestamp-newer side and compute the SAME
/// fingerprint — under the old stream-order merge the winner was arbitrary and
/// members could false-alarm OutOfSync.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn empty_nonempty_cross_master() -> anyhow::Result<()> {
    let mut c = cluster(2, 0).await?;

    // B (master 1) authors the content entry; converge.
    std::fs::write(c.nodes[1].folder().join("p.txt"), b"content from B")?;
    let mut want = BTreeMap::new();
    want.insert("p.txt".to_string(), b"content from B".to_vec());
    c.drive_until(Duration::from_secs(90), "content seeded", |c| {
        c.converged(&want)
    })
    .await?;

    // A (master 0) truncates it to empty, strictly newer → the empty marker
    // (authored by A) must beat B's still-live content entry everywhere.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    std::fs::write(c.nodes[0].folder().join("p.txt"), b"")?;
    want.insert("p.txt".to_string(), Vec::new());
    c.drive_until(Duration::from_secs(90), "truncate-to-empty wins", |c| {
        c.converged(&want)
    })
    .await?;
    println!("empty-marker (newer) beat content entry on both members OK");

    // Reverse flip: B refills it, strictly newer → content must win again.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    std::fs::write(c.nodes[1].folder().join("p.txt"), b"refilled by B")?;
    want.insert("p.txt".to_string(), b"refilled by B".to_vec());
    c.drive_until(Duration::from_secs(90), "refill wins", |c| {
        c.converged(&want)
    })
    .await?;
    println!("content entry (newer) beat empty marker on both members OK");

    c.shutdown().await?;
    Ok(())
}

/// Known-issues #2 regression: a deep verify requested while a reconcile job is
/// in flight must survive that job's commit (the old `last_quick_sig = 0` force
/// was clobbered by the in-flight job's write-back and silently lost), then
/// actually run, heal same-size+mtime corruption, and clear.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn deep_verify_survives_inflight_reconcile() -> anyhow::Result<()> {
    let mut c = cluster(1, 1).await?;
    let share_id = c.share_id.clone();

    let good = content_for("x.bin", 8192);
    std::fs::write(c.nodes[0].folder().join("x.bin"), &good)?;
    let want = snapshot(c.nodes[0].folder());
    c.drive_until(Duration::from_secs(90), "initial sync", |c| {
        c.folders_match(&want)
    })
    .await?;

    // Reproduce the race exactly as the daemon interleaves it: build a job
    // (in-flight), then request the verify, then commit the job.
    let viewer = &mut c.nodes[1].engine;
    let job = viewer
        .make_reconcile_job(&share_id)?
        .expect("share is idle, job must build");
    viewer.request_deep_verify(&share_id);
    assert!(
        viewer.debug_deep_verify_pending(&share_id),
        "request must mark the verify pending"
    );
    let outcome = job.run().await?;
    viewer.finish_reconcile(&share_id, Some(outcome));
    assert!(
        viewer.debug_deep_verify_pending(&share_id),
        "an in-flight (unforced) job's commit must NOT swallow the pending verify \
         — this is the known-issues #2 race"
    );

    // Give the pending verify something only a full rehash can catch: in-place
    // corruption with identical (size, mtime).
    let path = c.nodes[1].folder().join("x.bin");
    let orig_mtime = std::fs::metadata(&path)?.modified()?;
    let mut corrupt = good.clone();
    corrupt[10] ^= 0xff;
    corrupt[4000] ^= 0xff;
    std::fs::write(&path, &corrupt)?;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)?
        .set_modified(orig_mtime)?;

    c.drive_until(
        Duration::from_secs(90),
        "forced verify heals corruption",
        |c| {
            std::fs::read(c.nodes[1].folder().join("x.bin"))
                .map(|b| b == good)
                .unwrap_or(false)
        },
    )
    .await?;
    assert!(
        !c.nodes[1].engine.debug_deep_verify_pending(&share_id),
        "a completed forced scan must clear the pending flag"
    );
    println!("deep verify survived the in-flight commit, ran, and healed OK");
    c.shutdown().await?;
    Ok(())
}

/// Known-issues #3 regression: while a share sits OutOfSync (here: one master
/// paused with a frozen fingerprint while the other publishes a change), the
/// live master must NOT re-hash the folder on a cadence — the old code forced a
/// full hashing scan every 60 s for the whole episode. The new policy escalates
/// only after 10 min, so within this window: zero forced scans.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored (slow: ~3 min of wall-clock waiting)"]
async fn no_rescan_thrash_while_out_of_sync() -> anyhow::Result<()> {
    let mut c = cluster(2, 0).await?;
    let share_id = c.share_id.clone();

    std::fs::write(c.nodes[0].folder().join("base.txt"), b"agreed")?;
    let mut want = BTreeMap::new();
    want.insert("base.txt".to_string(), b"agreed".to_vec());
    c.drive_until(Duration::from_secs(90), "baseline convergence", |c| {
        c.converged(&want)
    })
    .await?;

    // Freeze master 1 (paused shares keep broadcasting their stale presence
    // fingerprint) and change master 0 → persistent disagreement.
    c.nodes[1].engine.set_paused(&share_id, true)?;
    std::fs::write(c.nodes[0].folder().join("newer.txt"), b"m1 never sees this")?;
    c.drive_until(
        Duration::from_secs(120),
        "divergence trips OutOfSync",
        |c| c.status(0) == "OutOfSync",
    )
    .await?;
    println!("OutOfSync reached; observing rescan behavior for 90s…");

    // Observe well past the old 60 s rescan cadence: the full-scan counter must
    // stay flat (nothing on disk is changing) and no verify may be pending
    // (escalation waits 10 min).
    let baseline = c.nodes[0].engine.debug_full_scans(&share_id);
    let observe_until = std::time::Instant::now() + Duration::from_secs(90);
    while std::time::Instant::now() < observe_until {
        c.tick().await;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let scans = c.nodes[0].engine.debug_full_scans(&share_id);
    assert_eq!(
        scans, baseline,
        "an OutOfSync share with an unchanged folder must not re-hash on a cadence \
         (was: one full rehash every 60s)"
    );
    assert!(
        !c.nodes[0].engine.debug_deep_verify_pending(&share_id),
        "self-heal escalation must not fire before DIVERGENCE_DEEP_VERIFY_SECS"
    );
    println!("no rescan thrash over 90s OK ({scans} full scans, unchanged)");

    // Recovery: unpause master 1 → episode clears, both converge on the union.
    c.nodes[1].engine.set_paused(&share_id, false)?;
    want.insert("newer.txt".to_string(), b"m1 never sees this".to_vec());
    c.drive_until(
        Duration::from_secs(120),
        "episode clears after resume",
        |c| c.converged(&want),
    )
    .await?;
    println!("resumed and re-converged OK");
    c.shutdown().await?;
    Ok(())
}
