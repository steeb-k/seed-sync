//! Deterministic reproduction for the race behind the intermittent
//! `multi_master::delete_survives_unseen_master_copy` failure.
//!
//! That test drives A and B concurrently and quietly depends on B's *initial doc
//! sync* delivering A's delete tombstone before B's first reconcile publishes
//! B's own independently-seeded copy. That ordering is a race, not a guarantee:
//! under load the reconcile wins, B publishes `x.bin` with a record timestamp
//! newer than the tombstone, `resolve_tombstones` then hands the path to the
//! content — and the delete is lost **permanently**, on every member.
//!
//! This test removes the timing entirely. `add_share_open` opens B's replica and
//! returns the dial WITHOUT starting live-sync, so B provably reconciles blind
//! (no tombstone, no remote entries), which is exactly the state the racy test
//! reaches by accident. Only then is sync started.
//!
//! `#[ignore]` because it opens real iroh endpoints; run serially:
//!   cargo test -p seed-core --test tombstone_race -- --ignored --nocapture --test-threads 1

mod common;

use std::time::Duration;

use seed_core::Engine;

/// Deterministic per-path content (same shape as the multi-master suite's).
fn content_for(path: &str, size: usize) -> Vec<u8> {
    let mut v = path.as_bytes().to_vec();
    v.extend(common::gen_bytes(size.saturating_sub(v.len())));
    v.truncate(size);
    v
}

/// A master that joins with a pre-existing copy of a deleted file, and whose
/// replica has not yet synced, must not resurrect the delete. The tombstone's
/// timestamp is strictly newer than B's file mtime, so LWW must favour the
/// delete no matter which side reconciles first.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn blind_joining_master_must_not_resurrect_a_delete() -> anyhow::Result<()> {
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

    // B's independently-seeded copies, written BEFORE the delete so their mtimes
    // are older than the tombstone.
    std::fs::write(b_folder.path().join("x.bin"), &x_bytes)?;
    std::fs::write(b_folder.path().join("y.bin"), &y_bytes)?;

    // A deletes X strictly later → tombstone ts is newer than B's file mtimes.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    std::fs::remove_file(a_folder.path().join("x.bin"))?;
    a.reconcile(&share_id).await?;

    // B joins, but live-sync is deliberately NOT started yet.
    let mut b = Engine::new(b_data.path()).await?;
    tokio::time::timeout(Duration::from_secs(20), b.wait_online())
        .await
        .map_err(|_| anyhow::anyhow!("B endpoint never came online"))?;
    let a_addr = a.endpoint_addr();
    let (share_id_b, sync) = b
        .add_share_open(&created.master_key, b_folder.path(), vec![a_addr])
        .await?;
    assert_eq!(share_id_b, share_id);

    // Blind passes: B's replica holds nothing, so the "local file absent from
    // the replica" arm finds no tombstone and treats x.bin as brand-new content.
    for _ in 0..3 {
        if let Err(e) = b.reconcile(&share_id).await {
            println!("  B blind reconcile error: {e:#}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    println!("B reconciled 3x blind (replica not yet synced)");

    // Now let them talk. The tombstone must still win.
    sync.start().await?;

    let started = std::time::Instant::now();
    let mut passes: u32 = 0;
    let converged = tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let _ = a.reconcile(&share_id).await;
            let _ = b.reconcile(&share_id).await;
            let a_x = a_folder.path().join("x.bin").exists();
            let b_x = b_folder.path().join("x.bin").exists();
            let y_ok = std::fs::read(a_folder.path().join("y.bin")).ok().as_deref()
                == Some(&y_bytes[..])
                && std::fs::read(b_folder.path().join("y.bin")).ok().as_deref()
                    == Some(&y_bytes[..]);
            if !a_x && !b_x && y_ok {
                return;
            }
            passes += 1;
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
        converged.is_ok(),
        "a blind joining master resurrected a delete after {passes} passes: \
         A has x.bin={}, B has x.bin={}",
        a_folder.path().join("x.bin").exists(),
        b_folder.path().join("x.bin").exists(),
    );
    println!("tombstone survived a blind joining master");

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}
