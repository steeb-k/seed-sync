//! A share that is removed (or paused) must stop the reconcile pass that is
//! already running for it — known-issues #34.
//!
//! A [`ReconcileJob`] is a *snapshot*: it clones the doc, folder, blob store and
//! downloader handles and then runs off the engine lock, so nothing the engine
//! does to `ShareState` can reach it. Before the fix, `remove_share` dropped the
//! state, left the doc and deleted every DB row while the pass kept merging —
//! materializing files into a folder the user had just detached, dialing peers,
//! and burning bandwidth, with the engine map, the DB, the CLI and the GUI all
//! correctly reporting no shares. In the field one such pass was still running
//! 14 minutes after removal, and the daemon's sequential reconcile loop meant it
//! stalled every other share for as long as it ran.
//!
//! `#[ignore]` (opens a real iroh endpoint); run with `-- --ignored`.

mod common;

use std::path::Path;

use seed_core::engine::ReconcileCancelled;
use seed_core::Engine;

fn seed_folder(folder: &Path) {
    std::fs::create_dir_all(folder.join("nested")).unwrap();
    // Enough files that the merge loop is guaranteed to iterate: the cancel
    // check sits at the top of each iteration.
    for i in 0..8 {
        std::fs::write(folder.join(format!("f{i}.bin")), format!("content {i}")).unwrap();
    }
    std::fs::write(folder.join("nested/deep.bin"), b"deep").unwrap();
}

/// Build an engine over a freshly seeded folder and create a master share in it.
async fn setup() -> anyhow::Result<(tempfile::TempDir, Engine, String, common::SecretGuard)> {
    common::init_tracing();
    let dir = tempfile::tempdir()?;
    let folder = dir.path().join("share");
    seed_folder(&folder);
    let mut engine = Engine::new(&dir.path().join("data")).await?;
    let created = engine.create_share(&folder, vec![]).await?;
    let guard = common::SecretGuard::new(&created.share_id);
    Ok((dir, engine, created.share_id, guard))
}

/// Positive control. Without this, the two cancellation tests below would pass
/// just as happily if `run()` had started failing for some unrelated reason.
#[tokio::test]
#[ignore]
async fn an_untouched_pass_still_completes() -> anyhow::Result<()> {
    let (_dir, mut engine, share_id, _seed) = setup().await?;

    let job = engine
        .make_reconcile_job(&share_id)?
        .expect("a live share yields a job");
    let outcome = job.run().await;
    assert!(
        outcome.is_ok(),
        "an uncancelled pass must run to completion: {:?}",
        outcome.err()
    );

    engine.shutdown().await
}

#[tokio::test]
#[ignore]
async fn removing_a_share_cancels_its_in_flight_pass() -> anyhow::Result<()> {
    let (_dir, mut engine, share_id, _seed) = setup().await?;

    // The daemon's reconcile loop builds the job under the engine lock, then
    // releases it and runs the pass. Removal lands in exactly that window.
    let job = engine
        .make_reconcile_job(&share_id)?
        .expect("a live share yields a job");
    engine.remove_share(&share_id, false).await?;

    // `ReconcileOutcome` isn't `Debug`, so match rather than `expect_err`.
    let err = match job.run().await {
        Ok(_) => panic!("a pass whose share was removed ran to completion"),
        Err(e) => e,
    };
    assert!(
        err.downcast_ref::<ReconcileCancelled>().is_some(),
        "expected ReconcileCancelled, got: {err:#}"
    );

    engine.shutdown().await
}

#[tokio::test]
#[ignore]
async fn pausing_a_share_cancels_its_in_flight_pass() -> anyhow::Result<()> {
    let (_dir, mut engine, share_id, _seed) = setup().await?;

    let job = engine
        .make_reconcile_job(&share_id)?
        .expect("a live share yields a job");
    engine.set_paused(&share_id, true)?;

    let err = match job.run().await {
        Ok(_) => panic!("a pass whose share was paused ran to completion"),
        Err(e) => e,
    };
    assert!(
        err.downcast_ref::<ReconcileCancelled>().is_some(),
        "expected ReconcileCancelled, got: {err:#}"
    );

    engine.shutdown().await
}

/// The cancel flag must not leak into the *next* pass: resuming a paused share
/// (or any later tick) has to produce a job that runs normally again.
#[tokio::test]
#[ignore]
async fn a_later_pass_is_not_poisoned_by_an_earlier_cancel() -> anyhow::Result<()> {
    let (_dir, mut engine, share_id, _seed) = setup().await?;

    let cancelled = engine
        .make_reconcile_job(&share_id)?
        .expect("a live share yields a job");
    engine.set_paused(&share_id, true)?;
    assert!(cancelled.run().await.is_err(), "first pass must abort");
    // What the daemon's reconcile loop does with a failed pass: commit nothing,
    // clear the busy guard. Without it `publishing` stays set and no further job
    // is ever built for this share.
    engine.finish_reconcile(&share_id, None);

    engine.set_paused(&share_id, false)?;
    let job = engine
        .make_reconcile_job(&share_id)?
        .expect("a resumed share yields a job");
    let outcome = job.run().await;
    assert!(
        outcome.is_ok(),
        "the pass after a resume must run normally: {:?}",
        outcome.err()
    );

    engine.shutdown().await
}
