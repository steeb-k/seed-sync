//! A share whose local folder is gone must not take the daemon down, must not be
//! silently recreated, and must never be mistaken for the user deleting everything.
//!
//! Observed in the field (2026-09-17, known-issues #37): the only share on a Windows
//! box lived on `D:\SEED_Share`, and the D: drive was removed. From then on every
//! service start died about one second in:
//!
//! ```text
//! ERROR seed_daemon::service: daemon serve error: The system cannot find the path specified. (os error 3)
//! ```
//!
//! `Engine::open_share` ran `create_dir_all(folder)` before anything else, so a root
//! on a missing drive failed to open, `reload_shares` propagated that one share's
//! error with `?`, and the whole daemon exited — reporting exit code 0 to the SCM, so
//! Windows recorded a clean stop and the GUI just said "daemon not started".
//!
//! On a drive that is merely *unplugged* the old code had a second, quieter problem:
//! `create_dir_all` succeeds whenever the parent exists, so a folder the user moved or
//! deleted was recreated empty behind their back. And an empty root under a master
//! whose index is populated is exactly the shape of "the user deleted every file" —
//! the reconcile pass tombstones each indexed path a full scan no longer sees, and
//! every member deletes its copy.
//!
//! So the contract is: a missing root holds the share **inert** (listed, `FolderMissing`,
//! never reconciled, nothing created on disk), the other shares keep running, and the
//! share comes back on its own when the folder does — unless it comes back *empty*
//! while the index still says it held files, in which case it stays inert.
//!
//! `#[ignore]`: `Engine::new` binds a real iroh endpoint and creating a master share
//! touches the OS keystore. Run with:
//!   cargo test -p seed-core --test missing_folder -- --ignored

use std::path::Path;

use seed_core::Engine;

fn status(engine: &Engine, share_id: &str) -> String {
    engine
        .list_summaries()
        .into_iter()
        .find(|s| s.share_id == share_id)
        .map(|s| format!("{:?}", s.status))
        .unwrap_or_else(|| "<missing>".into())
}

/// Create a master share over `folder` (populating the index with `files`) and shut
/// the engine down, as a daemon that ran once and stopped would. Returns the share id.
async fn create_and_stop(
    data: &Path,
    folder: &Path,
    files: &[(&str, &[u8])],
) -> anyhow::Result<String> {
    std::fs::create_dir_all(folder)?;
    for (name, content) in files {
        std::fs::write(folder.join(name), content)?;
    }
    let mut engine = Engine::new(data).await?;
    let created = engine.create_share(folder, vec![]).await?;
    engine.shutdown().await?;
    Ok(created.share_id)
}

/// Delete the shares (and their keystore seeds) so the test leaves nothing behind.
async fn cleanup(engine: &mut Engine, ids: &[&str]) {
    for id in ids {
        let _ = engine.remove_share(id, false).await;
    }
}

/// The field failure: the daemon restarts and one share's root is gone. The daemon
/// must come up, list the share as `FolderMissing`, keep every other share running,
/// and leave the filesystem alone.
#[tokio::test]
#[ignore = "opens a real iroh endpoint and touches the OS keystore; run with --ignored"]
async fn a_share_whose_folder_is_gone_does_not_take_the_daemon_down() -> anyhow::Result<()> {
    let data = tempfile::tempdir()?;
    let roots = tempfile::tempdir()?;
    let gone = roots.path().join("on_the_removed_drive");
    let fine = roots.path().join("still_here");

    let gone_id = create_and_stop(data.path(), &gone, &[("mine.txt", b"my content")]).await?;
    let fine_id = create_and_stop(data.path(), &fine, &[("other.txt", b"other content")]).await?;

    // The drive goes away.
    std::fs::remove_dir_all(&gone)?;

    // This is the daemon restarting. Before the fix this was
    // `The system cannot find the path specified. (os error 3)` on a missing drive,
    // and a silently recreated empty folder on a merely-unplugged one.
    let mut engine = Engine::new(data.path()).await.map_err(|e| {
        anyhow::anyhow!("one share's missing folder took the whole engine down: {e:#}")
    })?;

    assert!(
        !gone.exists(),
        "the engine must not create a share's root folder behind the user's back — \
         an empty root under a master reads as \"the user deleted everything\""
    );
    assert_eq!(
        status(&engine, &gone_id),
        "FolderMissing",
        "the share must stay listed and name its fault; a share that vanishes from the \
         list, or reads Healthy, is a lie"
    );
    let fine_status = status(&engine, &fine_id);
    assert!(
        fine_status != "<missing>" && fine_status != "FolderMissing",
        "the other share must keep working (got {fine_status})"
    );
    assert!(
        engine.make_reconcile_job(&gone_id)?.is_none(),
        "a share with no folder must never be reconciled"
    );

    cleanup(&mut engine, &[&gone_id, &fine_id]).await;
    engine.shutdown().await?;
    Ok(())
}

/// A drive that is plugged back in (or a folder moved back) resumes the share in
/// place — no restart, exactly like the keystore retry.
#[tokio::test]
#[ignore = "opens a real iroh endpoint and touches the OS keystore; run with --ignored"]
async fn a_folder_that_comes_back_with_its_files_resumes_in_place() -> anyhow::Result<()> {
    let data = tempfile::tempdir()?;
    let roots = tempfile::tempdir()?;
    let folder = roots.path().join("share");
    let parked = roots.path().join("share.unplugged");

    let id = create_and_stop(data.path(), &folder, &[("mine.txt", b"my content")]).await?;

    std::fs::rename(&folder, &parked)?;
    let mut engine = Engine::new(data.path()).await?;
    assert_eq!(status(&engine, &id), "FolderMissing", "precondition");
    assert!(
        engine.retry_inert_shares().await.is_empty(),
        "nothing to recover while the folder is still gone"
    );

    // The drive comes back, files and all.
    std::fs::rename(&parked, &folder)?;
    let recovered = engine.retry_inert_shares().await;
    assert_eq!(
        recovered.len(),
        1,
        "the daemon must notice the folder is back on its own"
    );
    let s = status(&engine, &id);
    assert_ne!(s, "FolderMissing", "the share must be open again (got {s})");
    assert!(
        engine.make_reconcile_job(&id)?.is_some(),
        "a recovered share must reconcile again"
    );

    cleanup(&mut engine, &[&id]).await;
    engine.shutdown().await?;
    Ok(())
}

/// A folder that reappears **empty** while the index says it held files is not
/// "the user deleted everything": the share stays inert until content is back.
#[tokio::test]
#[ignore = "opens a real iroh endpoint and touches the OS keystore; run with --ignored"]
async fn a_folder_that_comes_back_empty_is_not_treated_as_a_mass_delete() -> anyhow::Result<()> {
    let data = tempfile::tempdir()?;
    let roots = tempfile::tempdir()?;
    let folder = roots.path().join("share");

    let id = create_and_stop(data.path(), &folder, &[("mine.txt", b"my content")]).await?;

    std::fs::remove_dir_all(&folder)?;
    let mut engine = Engine::new(data.path()).await?;
    assert_eq!(status(&engine, &id), "FolderMissing", "precondition");

    // Someone recreates the path — a fresh mount point, an empty replacement disk,
    // `mkdir` by hand — but the files are not there.
    std::fs::create_dir_all(&folder)?;
    assert!(
        engine.retry_inert_shares().await.is_empty(),
        "an empty root under a populated index must NOT reopen the share: the next \
         full scan would tombstone every indexed file and every member would delete \
         its copy"
    );
    assert_eq!(status(&engine, &id), "FolderMissing");

    // The content is restored; now it is safe.
    std::fs::write(folder.join("mine.txt"), b"my content")?;
    assert_eq!(engine.retry_inert_shares().await.len(), 1);
    assert_ne!(status(&engine, &id), "FolderMissing");

    cleanup(&mut engine, &[&id]).await;
    engine.shutdown().await?;
    Ok(())
}

/// The drive is pulled while the daemon is running: the share must report
/// `FolderMissing` and stop reconciling, refuse an empty stand-in, and pick up again
/// when the folder is back with its files.
#[tokio::test]
#[ignore = "opens a real iroh endpoint and touches the OS keystore; run with --ignored"]
async fn a_folder_that_vanishes_while_running_is_reported_and_left_alone() -> anyhow::Result<()> {
    let data = tempfile::tempdir()?;
    let roots = tempfile::tempdir()?;
    let folder = roots.path().join("share");
    let parked = roots.path().join("share.unplugged");
    std::fs::create_dir_all(&folder)?;
    std::fs::write(folder.join("mine.txt"), b"my content")?;

    let mut engine = Engine::new(data.path()).await?;
    let id = engine.create_share(&folder, vec![]).await?.share_id;
    assert_ne!(status(&engine, &id), "FolderMissing", "precondition");

    std::fs::rename(&folder, &parked)?;
    assert_eq!(
        status(&engine, &id),
        "FolderMissing",
        "a root that vanishes mid-run must be reported, not read as Healthy"
    );
    assert!(
        engine.make_reconcile_job(&id)?.is_none(),
        "no reconcile pass may run against a missing root"
    );
    assert!(!folder.exists(), "nothing may recreate the folder");

    // A replacement disk shows up at the same path — empty. Same rule as the
    // inert path: not a mass delete, keep holding the share.
    std::fs::create_dir_all(&folder)?;
    assert!(
        engine.make_reconcile_job(&id)?.is_none(),
        "an empty root under a populated index must NOT be reconciled: the pass would \
         tombstone every indexed file and every member would delete its copy"
    );
    assert_eq!(
        status(&engine, &id),
        "FolderMissing",
        "while the gate holds the share it must still say so, not read Healthy"
    );

    // The real folder is back with its files.
    std::fs::remove_dir(&folder)?;
    std::fs::rename(&parked, &folder)?;
    assert!(engine.make_reconcile_job(&id)?.is_some());
    assert_ne!(status(&engine, &id), "FolderMissing");

    cleanup(&mut engine, &[&id]).await;
    engine.shutdown().await?;
    Ok(())
}
