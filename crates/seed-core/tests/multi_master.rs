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

/// In-place overwrite of a large (swarm-path) file: a master rewrites an existing
/// file with new content (no delete), and the peer must converge by exporting the
/// freshly-downloaded blob from its local store. Regression for the double-download
/// bug where `materialize` skipped the store export because the stale target still
/// existed and fell through to `self_heal_file`, re-fetching the whole blob over
/// the network a second time. This asserts correctness of the fixed path across
/// the >4 MiB swarm route; the peer ends byte-identical to the new content.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn inplace_overwrite_large_file_converges() -> anyhow::Result<()> {
    let mut c = cluster(2, 0).await?;

    // Seed a >4 MiB file from master 0 (exercises the swarm download path).
    let v1 = content_for("big.bin-v1", 6 * 1024 * 1024);
    std::fs::write(c.nodes[0].folder().join("big.bin"), &v1)?;
    let mut want = BTreeMap::new();
    want.insert("big.bin".to_string(), v1);
    c.drive_until(Duration::from_secs(180), "seed big file", |c| {
        c.converged(&want)
    })
    .await?;

    // Overwrite it IN PLACE with different large content (no delete), strictly
    // later so LWW takes it. The peer already has the old file on disk, so this is
    // exactly the materialize path that used to re-download over the network.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let v2 = content_for("big.bin-v2", 5 * 1024 * 1024 + 1234);
    std::fs::write(c.nodes[0].folder().join("big.bin"), &v2)?;
    want.insert("big.bin".to_string(), v2);
    c.drive_until(
        Duration::from_secs(180),
        "peer takes in-place overwrite",
        |c| c.converged(&want),
    )
    .await?;
    println!("in-place large-file overwrite converged");

    c.shutdown().await?;
    Ok(())
}

/// Two masters each generate a deterministic corpus (hundreds/thousands of
/// small-to-mid files) into their folders concurrently; the union must
/// converge byte-identically on both, verified by streaming hashes. Also logs
/// wall-clock as the baseline for the scan-cost decision (known-issues #28).
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
    let generation = job.generation();
    let outcome = job.run().await?;
    viewer.finish_reconcile(&share_id, generation, Some(outcome));
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

/// Known-issues #12 regression: a delete must survive a master that never saw
/// the path but holds an identical copy on disk (the delete-vs-still-seeding
/// race). Master A publishes X and Y, deletes X (timestamped tombstone); master
/// B then joins with a pre-populated folder holding BOTH files (mtimes older
/// than the delete). Deletion-as-absence used to make B re-publish X and
/// resurrect it fleet-wide; the tombstone must now win — and an edit NEWER than
/// the tombstone must still resurrect the path (LWW in the other direction).
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn delete_survives_unseen_master_copy() -> anyhow::Result<()> {
    use seed_core::Engine;

    common::init_tracing();

    let a_data = tempfile::tempdir()?;
    let b_data = tempfile::tempdir()?;
    let a_folder = tempfile::tempdir()?;
    let b_folder = tempfile::tempdir()?;

    let x_bytes = content_for("x.bin", 4096);
    let y_bytes = content_for("y.bin", 4096);

    // A seeds and publishes X + Y.
    std::fs::write(a_folder.path().join("x.bin"), &x_bytes)?;
    std::fs::write(a_folder.path().join("y.bin"), &y_bytes)?;
    let mut a = Engine::new(a_data.path()).await?;
    tokio::time::timeout(Duration::from_secs(20), a.wait_online())
        .await
        .map_err(|_| anyhow::anyhow!("A endpoint never came online"))?;
    let created = a.create_share(a_folder.path(), vec![]).await?;
    let _seed = common::SecretGuard::new(&created.share_id);
    let share_id = created.share_id.clone();
    a.reconcile(&share_id).await?;

    // B's folder holds identical copies — written NOW, so their mtimes are
    // older than the upcoming delete (the "independently seeded" copy).
    std::fs::write(b_folder.path().join("x.bin"), &x_bytes)?;
    std::fs::write(b_folder.path().join("y.bin"), &y_bytes)?;

    // A deletes X strictly later than B's file mtimes → tombstone ts is newer.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    std::fs::remove_file(a_folder.path().join("x.bin"))?;
    a.reconcile(&share_id).await?; // detects the deletion, writes the tombstone

    // B joins as a co-master and reconciles against the replica.
    let mut b = Engine::new(b_data.path()).await?;
    tokio::time::timeout(Duration::from_secs(20), b.wait_online())
        .await
        .map_err(|_| anyhow::anyhow!("B endpoint never came online"))?;
    let a_addr = a.endpoint_addr();
    let share_id_b = b
        .add_share(&created.master_key, b_folder.path(), vec![a_addr])
        .await?;
    assert_eq!(share_id_b, share_id);

    // Drive both until B honors the delete: X gone on BOTH, Y intact on both.
    //
    // Reconcile errors are reported, not discarded: this loop used to `let _ =`
    // them, so when it hung the *reason* was invisible and the only output was
    // the assertion below. A wedged doc read in particular surfaces here as an
    // error after `DOC_READ_TIMEOUT_SECS` (120s) — the same budget as this
    // timeout, so it would otherwise expire silently at the same moment.
    let started = std::time::Instant::now();
    let mut last_a_err = String::new();
    let mut last_b_err = String::new();
    let mut passes: u32 = 0;
    let deadline = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            if let Err(e) = a.reconcile(&share_id).await {
                let s = format!("{e:#}");
                if s != last_a_err {
                    println!(
                        "  [{:>5.1}s] A reconcile error: {s}",
                        started.elapsed().as_secs_f32()
                    );
                    last_a_err = s;
                }
            }
            if let Err(e) = b.reconcile(&share_id).await {
                let s = format!("{e:#}");
                if s != last_b_err {
                    println!(
                        "  [{:>5.1}s] B reconcile error: {s}",
                        started.elapsed().as_secs_f32()
                    );
                    last_b_err = s;
                }
            }
            let a_x = a_folder.path().join("x.bin").exists();
            let b_x = b_folder.path().join("x.bin").exists();
            let x_gone = !a_x && !b_x;
            let y_ok = std::fs::read(a_folder.path().join("y.bin")).ok().as_deref()
                == Some(&y_bytes[..])
                && std::fs::read(b_folder.path().join("y.bin")).ok().as_deref()
                    == Some(&y_bytes[..]);
            if x_gone && y_ok {
                return;
            }
            passes += 1;
            // Heartbeat every ~5s so a hang shows whether passes are still
            // running (and what each side sees) versus wedged inside reconcile.
            if passes % 20 == 0 {
                println!(
                    "  [{:>5.1}s] pass {passes}: A.x={a_x} B.x={b_x} y_ok={y_ok}",
                    started.elapsed().as_secs_f32()
                );
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await;
    assert!(
        deadline.is_ok(),
        "delete did not survive B's unseen copy after {passes} passes in {:.1}s: \
         A has x.bin={}, B has x.bin={} (last A err: {}; last B err: {})",
        started.elapsed().as_secs_f32(),
        a_folder.path().join("x.bin").exists(),
        b_folder.path().join("x.bin").exists(),
        if last_a_err.is_empty() {
            "none"
        } else {
            &last_a_err
        },
        if last_b_err.is_empty() {
            "none"
        } else {
            &last_b_err
        },
    );
    println!("tombstone beat the unseen copy (X deleted on both, Y intact)");

    // Edit-after-delete: B re-creates X with a fresh mtime (newer than the
    // tombstone) → legitimate resurrection, must propagate back to A.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let x2 = content_for("x.bin-v2", 2048);
    std::fs::write(b_folder.path().join("x.bin"), &x2)?;
    let revived = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let _ = a.reconcile(&share_id).await;
            let _ = b.reconcile(&share_id).await;
            if std::fs::read(a_folder.path().join("x.bin")).ok().as_deref() == Some(&x2[..]) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await;
    assert!(
        revived.is_ok(),
        "edit-after-delete did not resurrect x.bin on A"
    );
    println!("edit-after-delete resurrected X (newer mtime beat the tombstone)");

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

/// The "deleted a big file, pasted a replacement with the same name, and it kept
/// vanishing every time" bug. Once a delete tombstone is recorded, re-adding
/// *different* content at that path must publish and survive — even when the new
/// file's mtime is OLDER than the delete (copy, extract-from-archive and
/// download all preserve the source's mtime). A single node reproduces it: the
/// member who deleted couldn't re-add. The tombstone now stores the deleted
/// content's hash, so a re-add of different content is no longer mistaken for
/// the deleted file lingering.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn replaced_file_survives_stale_mtime() -> anyhow::Result<()> {
    use seed_core::Engine;

    let a_data = tempfile::tempdir()?;
    let a_folder = tempfile::tempdir()?;
    let path = a_folder.path().join("iso.bin");

    let v1 = content_for("iso.bin-v1", 4096);
    let v2 = content_for("iso.bin-v2", 8192); // genuinely different content

    std::fs::write(&path, &v1)?;
    let mut a = Engine::new(a_data.path()).await?;
    tokio::time::timeout(Duration::from_secs(20), a.wait_online())
        .await
        .map_err(|_| anyhow::anyhow!("A endpoint never came online"))?;
    let created = a.create_share(a_folder.path(), vec![]).await?;
    let _seed = common::SecretGuard::new(&created.share_id);
    let share_id = created.share_id.clone();
    a.reconcile(&share_id).await?; // publish v1

    // Delete it and reconcile → tombstone (ts = now, value = hash(v1)).
    std::fs::remove_file(&path)?;
    a.reconcile(&share_id).await?;
    assert!(!path.exists(), "precondition: the delete was recorded");

    // Paste a REPLACEMENT: different content, and a STALE mtime 60 s before the
    // delete — exactly what a copy/extract/download leaves behind.
    std::fs::write(&path, &v2)?;
    let stale = std::time::SystemTime::now() - Duration::from_secs(60);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)?
        .set_modified(stale)?;

    // Several reconcile passes must NOT delete the replacement.
    for _ in 0..8 {
        a.reconcile(&share_id).await?;
    }
    assert_eq!(
        std::fs::read(&path).ok().as_deref(),
        Some(&v2[..]),
        "replacement with a stale mtime was deleted by the tombstone (the reported bug)"
    );

    a.shutdown().await?;
    Ok(())
}

/// Paste-then-rename-during-write (a routine flow: paste an ISO, then immediately
/// rename it before the copy finishes). While the file is present but *unreadable*
/// — locked mid-copy — the master's scan SKIPS it (the os-error-32 "cannot read;
/// will retry" path), so it hasn't published. It is then renamed before it ever
/// published. The renamed file must still publish and converge on the peer, and the
/// pre-rename name must never surface anywhere. Regression for the reported
/// "pasted-then-renamed file won't sync until re-added".
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn pasted_then_renamed_while_locked_still_syncs() -> anyhow::Result<()> {
    let mut c = cluster(1, 1).await?; // one master (publisher) + one viewer (receiver)

    // Baseline so the share is live and converged before the tricky file.
    let base = content_for("baseline.txt", 4096);
    std::fs::write(c.nodes[0].folder().join("baseline.txt"), &base)?;
    let mut want = BTreeMap::new();
    want.insert("baseline.txt".to_string(), base);
    c.drive_until(Duration::from_secs(60), "baseline converged", |c| {
        c.converged(&want)
    })
    .await?;

    // The paste: a large file appears in the master's folder holding the content it
    // will have once the copy completes, but it is unreadable (locked mid-copy).
    let iso = content_for("renamed-final.iso", 5 * 1024 * 1024 + 777);
    let staged = c.nodes[0].folder().join("staged.iso");
    std::fs::write(&staged, &iso)?;

    // Hold it unreadable across several reconciles so the master SKIPS it and never
    // publishes it (the exact "1 file unreadable/unpublished, will retry" state).
    #[cfg(windows)]
    let lock = {
        use std::os::windows::fs::OpenOptionsExt;
        // share_mode(0) denies all sharing, so any other open (incl. the scanner's
        // in this same process) hits a sharing violation — os error 32.
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&staged)?
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o000))?;
    }
    for _ in 0..4 {
        c.tick().await;
    }
    assert!(
        !c.nodes[1].folder().join("staged.iso").exists(),
        "an unreadable (mid-paste) file must not publish to the peer"
    );

    // The rename. On Windows a share_mode(0) handle blocks rename, so release it
    // first (mimics the copy finishing, then the rename); on unix rename succeeds
    // even at 0o000, so we rename *while still locked* — the harsher ordering — and
    // only then make it readable.
    let final_path = c.nodes[0].folder().join("renamed-final.iso");
    #[cfg(windows)]
    {
        drop(lock);
        std::fs::rename(&staged, &final_path)?;
    }
    #[cfg(unix)]
    {
        std::fs::rename(&staged, &final_path)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&final_path, std::fs::Permissions::from_mode(0o644))?;
    }
    want.insert("renamed-final.iso".to_string(), iso);

    c.drive_until(
        Duration::from_secs(120),
        "pasted-then-renamed file converges under its final name",
        |c| c.converged(&want),
    )
    .await?;
    assert!(
        !c.nodes[1].folder().join("staged.iso").exists(),
        "the pre-rename name must not survive on the peer"
    );
    println!("pasted-then-renamed-while-locked file synced to the peer under its final name");

    c.shutdown().await?;
    Ok(())
}
