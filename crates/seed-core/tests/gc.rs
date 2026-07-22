//! Blob-GC live set (known-issues #22). The blob store never garbage-collected,
//! so orphaned blobs — most visibly incomplete partials stranded by a removed
//! share — grew on disk unbounded. GC is now enabled with an hourly sweep whose
//! protected ("live") set is recomputed from the live replicas by
//! `Engine::gc_refresh_job`. This test exercises that live-set computation — the
//! safety-critical part, since a hash left out of it would be swept even though a
//! replica still references it. The sweep itself is vendored iroh-blobs code
//! driven by this set, and only ever touches the blob store, never a folder.
//!
//! `#[ignore]` (opens a real iroh endpoint); run with `-- --ignored`.

use std::time::Duration;

use seed_core::Engine;

#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn gc_live_set_tracks_live_blobs_and_releases_a_removed_share() -> anyhow::Result<()> {
    let data = tempfile::tempdir()?;
    let folder = tempfile::tempdir()?;

    let mut engine = Engine::new(data.path()).await?;
    tokio::time::timeout(Duration::from_secs(20), engine.wait_online())
        .await
        .map_err(|_| anyhow::anyhow!("endpoint did not come online"))?;

    // Three distinct, non-empty files → three distinct content blobs (empty files
    // would hash to Hash::EMPTY and need no stored blob).
    for (name, body) in [
        ("a.bin", &b"alpha contents"[..]),
        ("b.bin", b"bravo contents"),
        ("c.bin", b"charlie contents"),
    ] {
        std::fs::write(folder.path().join(name), body)?;
    }
    let created = engine.create_share(folder.path(), vec![]).await?;
    let share = created.share_id.clone();

    // A refreshed live set must protect every referenced content blob. Drive a few
    // reconcile passes (import is idempotent) and refresh until the set covers at
    // least the three files.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let _ = engine.apply(&share).await;
            engine.gc_refresh_job().run().await;
            if engine.debug_gc_live_set_len().unwrap_or(0) >= 3 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "GC live set never covered the 3 content blobs; got {:?}",
            engine.debug_gc_live_set_len()
        )
    })?;

    // Remove the share, then refresh: nothing references those blobs any more, so
    // they drop out of the live set and the store's next sweep may reclaim them.
    // (This is exactly the "stranded partials from a deleted share" case #22 is
    // about — proven here at the live-set boundary.)
    engine.remove_share(&share, false).await?;
    engine.gc_refresh_job().run().await;
    assert_eq!(
        engine.debug_gc_live_set_len(),
        Some(0),
        "a removed share must release all its blobs from the GC live set"
    );

    engine.shutdown().await?;
    println!("GC live set tracked live blobs and released a removed share OK");
    Ok(())
}
