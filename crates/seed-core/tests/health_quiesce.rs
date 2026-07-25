//! Health honesty on a quiet fleet (known-issues #33).
//!
//! Every long fleet soak ended with all 28 nodes reporting `Syncing 98%` over
//! folders that verified byte-for-byte identical, and never recovered — the run
//! could sit there for another 10 minutes with `retrying=0` and nothing changed.
//! It is two separate faults wearing one symptom, which is what made it so hard to
//! read from the samples alone:
//!
//! - **99%** is not a byte shortfall at all. A node holding every byte is capped
//!   there by `list_summaries` while any online peer advertises a different
//!   manifest fingerprint. Normally transient.
//! - **98%** is real missing content: the hourly GC sweep deletes blobs the
//!   published live set is too old to know about. The set is refreshed every
//!   ~120 s and the sweep never waits for it.
//!
//! These tests pin the property that holds regardless of which fault fires: **a
//! quiet fleet whose folders are correct must say 100%** — and must mean it.
//! Health used a stricter predicate than the repair path (`base` agrees AND the
//! blob is in the store, versus "the file on disk hashes correctly"), so anything
//! in the gap was invisible to repair: `materialize` returns early on a correct
//! file and queues no fetch, and a master's scan sees local == remote and moves on.
//!
//! The reason that gap is not merely cosmetic — and why the repair re-imports
//! rather than just re-scoring — is that a blob we do not hold is a blob we cannot
//! **serve**. The soak lost 18 files' blobs on 28 of 28 nodes at once, leaving
//! content no member could hand to a joiner. Reporting 100% over that would be
//! known-issues #17's rule inverted.
//!
//! Note `Cluster::converged` does *not* require 100% (it only rejects
//! `OutOfSync`/`NoPeers`), which is why the whole tier-1 gate stayed green while
//! every soak run failed on exactly this. Waits are deliberately separate so a
//! failure says which half broke: data convergence, or the honesty of the status
//! line reporting it.
//!
//! `#[ignore]` (opens real iroh endpoints); run with `-- --ignored`.

mod common;

use std::time::Duration;

/// Churn rounds before the fleet goes quiet. The soak needed ~58 before it stuck,
/// but every one of those rounds was followed by another; the interesting state is
/// the *tail*, so a handful of rounds followed by real quiescence is the cheap
/// version of the same shape.
const ROUNDS: usize = 12;
/// Ticks driven between churn rounds — enough to make progress, deliberately not
/// enough to require full convergence. The soak churns a live fleet, not a settled
/// one, and a mid-flight pass is where residue gets left behind.
const TICKS_BETWEEN_ROUNDS: usize = 6;

/// xorshift64*, so the churn pattern is varied but identical on every run.
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn health_returns_to_100_after_churn_stops() -> anyhow::Result<()> {
    common::init_tracing();
    // 3 masters + 2 viewers: the soak's shape (3 writers, the rest read-only),
    // scaled to something that runs in a minute.
    let mut c = common::cluster(3, 2).await?;

    // Seed a corpus on the creating master. Sizes vary so a rewrite can change
    // content by changing length (`gen_bytes` is a pure function of its length).
    let mut live: Vec<String> = Vec::new();
    for i in 0..24usize {
        let name = format!("corpus/f{i:02}.bin");
        let p = c.nodes[0]
            .folder()
            .join("corpus")
            .join(format!("f{i:02}.bin"));
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, common::gen_bytes(8192 + i * 997))?;
        live.push(name);
    }

    let want = common::snapshot(c.nodes[0].folder());
    c.drive_until(Duration::from_secs(180), "initial corpus converges", |c| {
        c.converged(&want)
    })
    .await?;
    assert!(
        c.all_full(),
        "fleet should report 100% on a freshly converged corpus, got {:?}",
        (0..c.nodes.len()).map(|i| c.percent(i)).collect::<Vec<_>>()
    );

    // --- churn: rewrite / delete / add on rotating masters, never settling ---
    let mut rng: u64 = 0x2545_F491_4F6C_DD1D;
    let mut added = 0usize;
    for round in 0..ROUNDS {
        let author = round % c.masters;
        let root = c.nodes[author].folder().to_path_buf();

        // Rewrite 3 existing paths (new length => new content => new hash).
        for _ in 0..3 {
            if live.is_empty() {
                break;
            }
            let idx = (next(&mut rng) % live.len() as u64) as usize;
            let target = root.join(common::rel_to_path(&live[idx]));
            if target.exists() {
                let n = 4096 + (next(&mut rng) % 20_000) as usize;
                std::fs::write(&target, common::gen_bytes(n))?;
            }
        }
        // Delete 2.
        for _ in 0..2 {
            if live.len() <= 4 {
                break;
            }
            let idx = (next(&mut rng) % live.len() as u64) as usize;
            let name = live.remove(idx);
            let _ = std::fs::remove_file(root.join(common::rel_to_path(&name)));
        }
        // Add 2 new paths.
        for _ in 0..2 {
            added += 1;
            let name = format!("corpus/r{round:02}-{added:03}.bin");
            let p = root.join(common::rel_to_path(&name));
            std::fs::create_dir_all(p.parent().unwrap())?;
            std::fs::write(
                &p,
                common::gen_bytes(6000 + (next(&mut rng) % 30_000) as usize),
            )?;
            live.push(name);
        }

        for _ in 0..TICKS_BETWEEN_ROUNDS {
            c.tick().await;
        }
    }

    // --- the fleet goes quiet here. Nothing touches any folder again. ---

    // Stage 1: the data converges. This is what the soak already proves (every
    // node byte-identical), so it should pass even when the percent is wrong.
    c.drive_until(
        Duration::from_secs(300),
        "quiesced fleet converges on identical folders",
        |c| {
            let want = common::snapshot(c.nodes[0].folder());
            c.folders_match(&want) && c.fps_agree()
        },
    )
    .await?;

    // Stage 2: and it admits it. Separate wait, separate failure message — a fleet
    // holding identical bytes while claiming 98% is the bug under test.
    let res = c
        .drive_until(
            Duration::from_secs(120),
            "quiesced, converged fleet reports 100%",
            |c| c.all_full(),
        )
        .await;
    if res.is_err() {
        let detail: Vec<String> = (0..c.nodes.len())
            .map(|i| format!("node{i}={} {}%", c.status(i), c.percent(i)))
            .collect();
        let want = common::snapshot(c.nodes[0].folder());
        anyhow::bail!(
            "folders are byte-identical ({} files) and fingerprints agree, but the \
             fleet will not report 100%: {}. Re-run with \
             RUST_LOG=seed_core::health=debug to see which paths health is holding \
             against each node — and note a node reporting exactly 99% may instead \
             be capped by the fingerprint gate in `list_summaries`, which prints \
             nothing here because it is not a byte shortfall at all.",
            want.len(),
            detail.join(", ")
        );
    }

    c.shutdown().await?;
    println!("health returned to 100% after churn stopped OK");
    Ok(())
}

/// The same defect, deterministically: health must not dock a file whose bytes are
/// correct on disk just because the local index hasn't caught up.
///
/// Organically this is a race — a pass skips a path it couldn't read while the user
/// was mid-write, so the index keeps the older hash — which is why it surfaces in a
/// 70-minute fleet soak and not in the gate. `debug_forget_index_entry` reproduces
/// the identical state in one call.
///
/// The failure this pins is not "the percent is a bit off". `materialize` returns
/// early on a file that hashes correctly and queues no download, so nothing in the
/// engine will ever revisit the path: the share sits below 100% with `retrying=0`
/// over a byte-perfect folder until something unrelated forces a rescan. And
/// because the percent is integer-truncated, one path docked by the
/// `size - 1` floor is enough to turn a complete 100% into `Syncing 99%`.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn health_ignores_a_stale_index_when_the_file_on_disk_is_right() -> anyhow::Result<()> {
    common::init_tracing();
    let mut c = common::cluster(1, 1).await?;

    // Two files, so the docked one is a minority of the bytes: this asserts the
    // percent is *exactly* 100, not merely "not much less than 100".
    std::fs::write(
        c.nodes[0].folder().join("a.bin"),
        common::gen_bytes(64 * 1024),
    )?;
    std::fs::write(
        c.nodes[0].folder().join("b.bin"),
        common::gen_bytes(48 * 1024),
    )?;

    let want = common::snapshot(c.nodes[0].folder());
    c.drive_until(Duration::from_secs(120), "both files converge", |c| {
        c.converged(&want)
    })
    .await?;
    // Reaching 100% is a *wait*, not an immediate assertion: `Healthy` also requires
    // agreeing with the fingerprints peers have most recently broadcast in presence,
    // and those lag our own by a beat or two. A share that holds every byte but
    // hasn't heard a matching fingerprint yet is deliberately capped at `Syncing 99%`
    // (see `list_summaries`), which is honest and transient — quite distinct from the
    // permanent shortfall this test is about.
    c.drive_until(
        Duration::from_secs(60),
        "precondition: a converged 2-file share settles at 100%",
        |c| c.all_full(),
    )
    .await?;

    // Make the viewer's index forget a file it is holding correctly. Folder and
    // replica are untouched, so the *only* thing wrong is the bookkeeping.
    let share = c.share_id.clone();
    c.nodes[1].engine.debug_forget_index_entry(&share, "a.bin");

    // Drive passes. Nothing needs to be fetched or written — the question is purely
    // what the node now claims about itself. Note no rescan will rescue it: the
    // folder signature is unchanged, so `do_scan` stays false and the merge loop
    // never revisits a path it believes is already settled.
    let res = c
        .drive_until(
            Duration::from_secs(60),
            "viewer with a stale index entry still reports 100%",
            |c| c.percent(1) == 100,
        )
        .await;

    let want_now = common::snapshot(c.nodes[0].folder());
    assert!(
        c.folders_match(&want_now),
        "the folders must be untouched by this — only the index was edited"
    );
    if res.is_err() {
        anyhow::bail!(
            "the viewer holds both files byte-for-byte but reports {}%: a stale index \
             entry is not a missing file. Nothing will ever re-fetch a file that \
             already hashes correctly (`materialize` returns early on it), so this \
             does not heal — and because the percent is integer-truncated, the single \
             byte docked by the `size - 1` floor is enough to turn a complete share \
             into `Syncing 99%` (known-issues #33).",
            c.percent(1)
        );
    }

    c.shutdown().await?;
    println!("health ignored a stale index over a correct file OK");
    Ok(())
}
