//! A master whose seed cannot be loaded from the OS keystore must not silently
//! become a viewer.
//!
//! Observed in the field (2026-07-14), on a Linux box whose `systemd --user` daemon
//! starts at boot, before the login keyring is unlocked:
//!
//! ```text
//! WARN seed_core::engine: master seed for 4741b9bf… unavailable from keystore;
//!      running read-only: Secret Service: unlock prompt was dismissed
//! INFO seed_core::engine: reloaded 1 share(s)
//! ```
//!
//! The share is a **master** in the database (`role_master = 1`), but the key the
//! engine actually loaded is the stored *seedless* one — which is, by definition, a
//! viewer key. So the daemon ran that share read-only for the rest of the process's
//! life, announced only by that one WARN.
//!
//! Read-only is not a safe degradation here, it is a destructive one. A viewer does
//! not merely decline to publish: it treats the replica as authoritative and
//! **reverts local edits** (`Viewer: replica wins, always`). So a user editing files
//! in what they believe is their master share has those edits silently rolled back,
//! while every screen reports `Healthy`. Losing the write key must be a visible,
//! non-destructive fault: hold the share inert until the key is available, and never
//! let a would-be master act as a viewer over the user's files.
//!
//! Synthetic and offline: temp dirs, no network, and the keystore entry it creates is
//! its own (keyed by a freshly generated share id) and is deleted as part of the test.
//!
//! `#[ignore]`: opens a real iroh endpoint (Engine::new binds one) and touches the OS
//! keystore. Run with:
//!   cargo test -p seed-core --test keystore -- --ignored

use seed_core::identity::ShareKey;
use seed_core::Engine;

fn status(engine: &Engine, share_id: &str) -> String {
    engine
        .list_summaries()
        .into_iter()
        .find(|s| s.share_id == share_id)
        .map(|s| format!("{:?}", s.status))
        .unwrap_or_else(|| "<missing>".into())
}

/// Reload a master share whose seed the keystore will not hand back. It must not come
/// up as a viewer.
#[tokio::test]
#[ignore = "opens a real iroh endpoint and touches the OS keystore; run with --ignored"]
async fn a_master_whose_seed_is_locked_does_not_become_a_viewer() -> anyhow::Result<()> {
    let data = tempfile::tempdir()?;
    let folder = tempfile::tempdir()?;
    std::fs::write(folder.path().join("mine.txt"), b"my content")?;

    // Create a master share. This stores the seed in the OS keystore and persists the
    // row with role_master = 1, seed_in_keyring = 1.
    let share_id = {
        let mut engine = Engine::new(data.path()).await?;
        let created = engine.create_share(folder.path(), vec![]).await?;
        let summaries = engine.list_summaries();
        let s = summaries
            .iter()
            .find(|s| s.share_id == created.share_id)
            .expect("share present");
        assert_eq!(
            s.role,
            seed_ipc::Role::Master,
            "sanity: the share we just created must be a master"
        );
        engine.shutdown().await?;
        created.share_id
    };

    // Simulate the field failure: the keystore will not return the seed. On the
    // affected box the cause was a dismissed unlock prompt; the engine reaches the
    // identical branch (`secrets::load_seed` returns Err) however the read fails.
    seed_core::secrets::delete_seed(&share_id);

    // Reload from the same data dir — this is the daemon restarting.
    let engine = Engine::new(data.path()).await?;
    let summaries = engine.list_summaries();
    let s = summaries
        .iter()
        .find(|s| s.share_id == share_id)
        .expect("the share must still be listed, not silently dropped");

    assert_ne!(
        s.role,
        seed_ipc::Role::Viewer,
        "a master share whose seed could not be loaded came back as a VIEWER. A viewer \
         reverts local edits to match the replica, so this silently destroys the user's \
         own writes in their own master share while reporting Healthy. Losing the write \
         key must be a visible fault, not a quiet demotion to read-only."
    );
    assert_eq!(
        format!("{:?}", s.status),
        "KeyLocked",
        "the fault must be visible and name itself; anything else leaves the user with \
         a share that just quietly stops working"
    );

    engine.shutdown().await?;
    Ok(())
}

/// While the key is locked the share is **inert**: it must not touch the folder. This is
/// the property that actually protects data — the old read-only behavior reverted the
/// user's edits to whatever the replica held.
///
/// This needs a **live peer**, and that is not incidental. `materialize()` reverts a
/// differing file by calling `self_heal_file`, which *fetches the bytes from a peer* —
/// so with no peer online the revert quietly fails and a one-node test passes against
/// the buggy code, proving nothing. (It did, on the first attempt.) The whole point is
/// to reproduce a master that has been demoted to viewer while its pool is still there
/// to overwrite it from.
#[tokio::test]
#[ignore = "opens real iroh endpoints and touches the OS keystore; run with --ignored"]
async fn a_locked_share_does_not_let_a_peer_overwrite_the_users_files() -> anyhow::Result<()> {
    let a_data = tempfile::tempdir()?;
    let b_data = tempfile::tempdir()?;
    let a_folder = tempfile::tempdir()?;
    let b_folder = tempfile::tempdir()?;
    std::fs::write(a_folder.path().join("mine.txt"), b"original")?;

    // A: the peer that stays up the whole time, holding the share's content.
    let mut peer = Engine::new(a_data.path()).await?;
    let created = peer.create_share(a_folder.path(), vec![]).await?;
    let share_id = created.share_id.clone();
    let peer_addr = peer.endpoint_addr();

    // B: a second **master** (same master key), which is the box that will lose its key.
    let b_file = b_folder.path().join("mine.txt");
    {
        let mut b = Engine::new(b_data.path()).await?;
        b.add_share(&created.master_key, b_folder.path(), vec![peer_addr])
            .await?;
        // Sync until B has the content, so its index and replica are fully established —
        // exactly the state a long-running member is in.
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                let _ = peer.reconcile(&share_id).await;
                let _ = b.reconcile(&share_id).await;
                if std::fs::read(&b_file).ok().as_deref() == Some(b"original".as_slice()) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("B never synced the initial content"))?;
        b.shutdown().await?;
    }

    // B's daemon restarts, but its keyring is locked (the field failure). A is still up
    // and still holds `original` — so there is a peer ready to overwrite B from.
    //
    // Note both engines share one keystore entry (keyed by share id) on this machine;
    // deleting it does not disturb A, which already holds its key in memory.
    seed_core::secrets::delete_seed(&share_id);

    // The user edits a file in what is, as far as they know, their own master share.
    std::fs::write(&b_file, b"my new content")?;

    let mut b = Engine::new(b_data.path()).await?;
    assert_eq!(
        status(&b, &share_id),
        "KeyLocked",
        "precondition: B must come up with its write key locked"
    );

    // Drive both sides the way the daemon loop would, giving a demoted B every chance to
    // pull `original` back down from A and clobber the edit.
    for _ in 0..10 {
        let _ = peer.reconcile(&share_id).await;
        b.reconcile_all().await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    assert_eq!(
        std::fs::read(&b_file)?,
        b"my new content",
        "a share whose write key is locked must not rewrite the user's files. Run \
         read-only, it fetches the replica's copy from a peer and overwrites the user's \
         own edit in their own master share — silently, while reporting Healthy."
    );

    b.shutdown().await?;
    peer.shutdown().await?;
    Ok(())
}

/// Unlocking the keyring must resume the share **without a daemon restart**. Nothing
/// about "my files stopped syncing" would lead a user to restart the daemon, so if this
/// doesn't recover on its own, in practice it doesn't recover.
#[tokio::test]
#[ignore = "opens a real iroh endpoint and touches the OS keystore; run with --ignored"]
async fn unlocking_the_keystore_restores_the_master_in_place() -> anyhow::Result<()> {
    let data = tempfile::tempdir()?;
    let folder = tempfile::tempdir()?;
    std::fs::write(folder.path().join("mine.txt"), b"content")?;

    let (share_id, seed) = {
        let mut engine = Engine::new(data.path()).await?;
        let created = engine.create_share(folder.path(), vec![]).await?;
        let key = ShareKey::decode(&created.master_key)?;
        let seed = key.seed_bytes().expect("master key carries the seed");
        engine.shutdown().await?;
        (created.share_id, seed)
    };

    // Keyring locked at daemon start (the field failure).
    seed_core::secrets::delete_seed(&share_id);
    let mut engine = Engine::new(data.path()).await?;
    assert_eq!(
        status(&engine, &share_id),
        "KeyLocked",
        "precondition: the share starts out locked"
    );

    // The user logs in and the keyring unlocks — the seed becomes readable again.
    seed_core::secrets::store_seed(&share_id, &seed)?;

    let recovered = engine.retry_locked_keys().await;
    assert_eq!(
        recovered.len(),
        1,
        "the daemon must notice the key became available on its own"
    );

    let summaries = engine.list_summaries();
    let s = summaries
        .iter()
        .find(|s| s.share_id == share_id)
        .expect("share present");
    assert_eq!(
        s.role,
        seed_ipc::Role::Master,
        "once the keystore hands the seed back, the share must be a writable master \
         again — with no restart"
    );
    assert_ne!(format!("{:?}", s.status), "KeyLocked");

    seed_core::secrets::delete_seed(&share_id); // clean up after ourselves
    engine.shutdown().await?;
    Ok(())
}
