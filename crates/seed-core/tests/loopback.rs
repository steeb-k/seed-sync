//! Engine-level loopback test (the library form of Checkpoint #1): two engines
//! on one machine, separate data dirs and folders, syncing a real share over
//! iroh. Exercises initial sync, update propagation, deletion propagation, and
//! viewer-edit revert (strict hard-overwrite mirror).
//!
//! `#[ignore]` because it opens real iroh endpoints; run with:
//!   cargo test -p seed-core --test loopback -- --ignored --nocapture

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use seed_core::Engine;

/// Read a folder into a sorted map of relative-path -> contents, for comparison.
fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut map = BTreeMap::new();
    for dent in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if dent.file_type().is_file() {
            let rel = dent
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            map.insert(rel, std::fs::read(dent.path()).unwrap());
        }
    }
    map
}

/// Drive `apply` until the viewer folder matches `want`, or time out.
async fn sync_until(
    engine: &mut Engine,
    share_id: &str,
    folder: &Path,
    want: &BTreeMap<String, Vec<u8>>,
) -> anyhow::Result<()> {
    let res = tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            let _ = engine.apply(share_id).await?;
            if &snapshot(folder) == want {
                return anyhow::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    match res {
        Ok(inner) => inner,
        Err(_) => Err(anyhow::anyhow!(
            "timed out; have {:?}, want {:?}",
            snapshot(folder).keys().collect::<Vec<_>>(),
            want.keys().collect::<Vec<_>>()
        )),
    }
}

#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn master_viewer_mirror_lifecycle() -> anyhow::Result<()> {
    let a_data = tempfile::tempdir()?;
    let b_data = tempfile::tempdir()?;
    let a_folder = tempfile::tempdir()?;
    let b_folder = tempfile::tempdir()?;

    let mut master = Engine::new(a_data.path()).await?;
    let mut viewer = Engine::new(b_data.path()).await?;

    // Ensure both endpoints have complete, dialable addresses before we exchange
    // bootstrap info.
    tokio::time::timeout(Duration::from_secs(20), async {
        master.wait_online().await;
        viewer.wait_online().await;
    })
    .await
    .map_err(|_| anyhow::anyhow!("endpoints did not come online"))?;

    // Seed the master's folder.
    std::fs::write(a_folder.path().join("readme.txt"), b"hello")?;
    std::fs::create_dir_all(a_folder.path().join("bin"))?;
    std::fs::write(a_folder.path().join("bin/tool.sh"), b"#!/bin/sh\necho hi\n")?;

    let created = master.create_share(a_folder.path(), vec![]).await?;
    let master_addr = master.endpoint_addr();

    // Viewer joins with the read-only key, bootstrapped to the master's addr.
    let share_id = viewer
        .add_share(&created.viewer_key, b_folder.path(), vec![master_addr])
        .await?;
    assert_eq!(share_id, created.share_id);

    // 1. Initial sync: viewer folder should mirror the master's.
    let want = snapshot(a_folder.path());
    sync_until(&mut viewer, &share_id, b_folder.path(), &want).await?;
    println!("initial sync OK ({} files)", want.len());

    // 2. Update propagation: change a file, add a file, republish.
    std::fs::write(a_folder.path().join("readme.txt"), b"hello v2")?;
    std::fs::write(a_folder.path().join("notes.md"), b"# notes")?;
    master.publish(&share_id).await?;
    let want = snapshot(a_folder.path());
    sync_until(&mut viewer, &share_id, b_folder.path(), &want).await?;
    println!("update propagation OK");

    // 3. Deletion propagation: remove a file, republish.
    std::fs::remove_file(a_folder.path().join("bin/tool.sh"))?;
    master.publish(&share_id).await?;
    let want = snapshot(a_folder.path());
    sync_until(&mut viewer, &share_id, b_folder.path(), &want).await?;
    assert!(!b_folder.path().join("bin/tool.sh").exists());
    println!("deletion propagation OK");

    // 4. Viewer-edit revert: a rogue local change must be discarded on reconcile,
    //    with no new manifest from the master.
    std::fs::write(b_folder.path().join("readme.txt"), b"rogue edit")?;
    std::fs::write(b_folder.path().join("rogue.txt"), b"should vanish")?;
    let want = snapshot(a_folder.path());
    sync_until(&mut viewer, &share_id, b_folder.path(), &want).await?;
    assert!(!b_folder.path().join("rogue.txt").exists());
    assert_eq!(
        std::fs::read(b_folder.path().join("readme.txt"))?,
        b"hello v2"
    );
    println!("viewer-edit revert OK");

    master.shutdown().await?;
    viewer.shutdown().await?;
    Ok(())
}
