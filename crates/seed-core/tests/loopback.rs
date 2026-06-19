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

/// Total size in bytes of all files under `root` (used to assert the viewer's
/// blob store stays outboard-sized rather than holding a second copy).
fn dir_size(root: &Path) -> u64 {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
        .sum()
}

/// Deterministic, varied (non-compressible-ish) bytes of length `n`.
fn gen_bytes(n: usize) -> Vec<u8> {
    (0..n as u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
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

/// A genuinely empty (0-byte) file must sync — including a unicode-named one.
/// iroh-docs can't carry a len-0 content entry, so empty files ride the signed
/// manifest and the viewer creates them directly. Regression: a 0-byte `café.txt`
/// used to fail `set_hash` ("Attempted to insert an empty entry") and wedge the
/// whole publish on a loop.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn empty_files_sync() -> anyhow::Result<()> {
    let a_data = tempfile::tempdir()?;
    let b_data = tempfile::tempdir()?;
    let a_folder = tempfile::tempdir()?;
    let b_folder = tempfile::tempdir()?;

    let mut master = Engine::new(a_data.path()).await?;
    let mut viewer = Engine::new(b_data.path()).await?;
    tokio::time::timeout(Duration::from_secs(20), async {
        master.wait_online().await;
        viewer.wait_online().await;
    })
    .await
    .map_err(|_| anyhow::anyhow!("endpoints did not come online"))?;

    // A 0-byte unicode-named file next to a normal one. The empty file must not
    // wedge the publish, and must still mirror.
    std::fs::write(a_folder.path().join("café.txt"), b"")?;
    std::fs::write(a_folder.path().join("readme.txt"), b"hello")?;

    let created = master.create_share(a_folder.path(), vec![]).await?;
    let master_addr = master.endpoint_addr();
    let share_id = viewer
        .add_share(&created.viewer_key, b_folder.path(), vec![master_addr])
        .await?;

    let want = snapshot(a_folder.path());
    sync_until(&mut viewer, &share_id, b_folder.path(), &want).await?;
    let cafe = b_folder.path().join("café.txt");
    assert!(cafe.is_file(), "empty unicode file should exist on viewer");
    assert_eq!(std::fs::metadata(&cafe)?.len(), 0, "should be 0 bytes");
    println!("empty unicode file synced OK");

    // A file truncated to empty later must also propagate to 0 bytes.
    std::fs::write(a_folder.path().join("readme.txt"), b"")?;
    master.publish(&share_id).await?;
    let want = snapshot(a_folder.path());
    sync_until(&mut viewer, &share_id, b_folder.path(), &want).await?;
    assert_eq!(
        std::fs::metadata(b_folder.path().join("readme.txt"))?.len(),
        0,
        "truncated-to-empty should propagate"
    );

    master.shutdown().await?;
    viewer.shutdown().await?;
    println!("empty-file sync OK");
    Ok(())
}

/// A viewer must store synced content ~1x: the blob store keeps only the outboard
/// and references the mirror file, instead of holding a second copy of the bytes.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn viewer_stores_by_reference_not_copy() -> anyhow::Result<()> {
    let a_data = tempfile::tempdir()?;
    let b_data = tempfile::tempdir()?;
    let a_folder = tempfile::tempdir()?;
    let b_folder = tempfile::tempdir()?;

    let mut master = Engine::new(a_data.path()).await?;
    let mut viewer = Engine::new(b_data.path()).await?;
    tokio::time::timeout(Duration::from_secs(20), async {
        master.wait_online().await;
        viewer.wait_online().await;
    })
    .await
    .map_err(|_| anyhow::anyhow!("endpoints did not come online"))?;

    // An 8 MiB file — large enough that a duplicate copy would be unmistakable.
    let big = gen_bytes(8 * 1024 * 1024);
    std::fs::write(a_folder.path().join("big.bin"), &big)?;

    let created = master.create_share(a_folder.path(), vec![]).await?;
    let master_addr = master.endpoint_addr();
    let share_id = viewer
        .add_share(&created.viewer_key, b_folder.path(), vec![master_addr])
        .await?;

    let want = snapshot(a_folder.path());
    sync_until(&mut viewer, &share_id, b_folder.path(), &want).await?;

    // The mirror file is full size...
    assert_eq!(
        std::fs::metadata(b_folder.path().join("big.bin"))?.len(),
        big.len() as u64
    );
    // ...but the blob store holds only the outboard, not a copy of the 8 MiB.
    let blobs = dir_size(&b_data.path().join("blobs"));
    assert!(
        blobs < big.len() as u64 / 4,
        "viewer blob store is {blobs} bytes; expected outboard-only (<{} ) for an {}-byte file — content is being copied, not referenced",
        big.len() / 4,
        big.len()
    );
    println!(
        "1x confirmed: blob store {blobs} bytes for an {}-byte mirrored file",
        big.len()
    );

    master.shutdown().await?;
    viewer.shutdown().await?;
    Ok(())
}

/// A viewer whose mirror file is edited/corrupted must repair itself
/// automatically (re-fetch the verified bytes from a peer) with no user action.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn viewer_auto_heals_corrupted_file() -> anyhow::Result<()> {
    let a_data = tempfile::tempdir()?;
    let b_data = tempfile::tempdir()?;
    let a_folder = tempfile::tempdir()?;
    let b_folder = tempfile::tempdir()?;

    let mut master = Engine::new(a_data.path()).await?;
    let mut viewer = Engine::new(b_data.path()).await?;
    tokio::time::timeout(Duration::from_secs(20), async {
        master.wait_online().await;
        viewer.wait_online().await;
    })
    .await
    .map_err(|_| anyhow::anyhow!("endpoints did not come online"))?;

    let content = gen_bytes(300_000);
    std::fs::write(a_folder.path().join("data.bin"), &content)?;
    let created = master.create_share(a_folder.path(), vec![]).await?;
    let master_addr = master.endpoint_addr();
    let share_id = viewer
        .add_share(&created.viewer_key, b_folder.path(), vec![master_addr])
        .await?;

    let want = snapshot(a_folder.path());
    sync_until(&mut viewer, &share_id, b_folder.path(), &want).await?;
    assert_eq!(std::fs::read(b_folder.path().join("data.bin"))?, content);

    // Corrupt the referenced mirror file (different size + content). With the
    // file referenced by the blob store, there is no clean local copy to revert
    // from — the engine must re-fetch from the master automatically.
    std::fs::write(b_folder.path().join("data.bin"), b"** locally corrupted **")?;

    // Reconcile (what the daemon's loop does every tick) must restore it.
    sync_until(&mut viewer, &share_id, b_folder.path(), &want).await?;
    assert_eq!(
        std::fs::read(b_folder.path().join("data.bin"))?,
        content,
        "corrupted mirror file was not auto-healed"
    );
    println!("auto self-heal OK ({} bytes restored)", content.len());

    master.shutdown().await?;
    viewer.shutdown().await?;
    Ok(())
}

/// A viewer that stores content by reference must still re-serve it to other
/// peers. Sync viewer1 from the master, take the master offline, then have
/// viewer2 sync using only viewer1 as a bootstrap.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn referenced_viewer_serves_peers() -> anyhow::Result<()> {
    let a_data = tempfile::tempdir()?;
    let b_data = tempfile::tempdir()?;
    let c_data = tempfile::tempdir()?;
    let a_folder = tempfile::tempdir()?;
    let b_folder = tempfile::tempdir()?;
    let c_folder = tempfile::tempdir()?;

    let mut master = Engine::new(a_data.path()).await?;
    let mut viewer1 = Engine::new(b_data.path()).await?;
    let mut viewer2 = Engine::new(c_data.path()).await?;
    tokio::time::timeout(Duration::from_secs(25), async {
        master.wait_online().await;
        viewer1.wait_online().await;
        viewer2.wait_online().await;
    })
    .await
    .map_err(|_| anyhow::anyhow!("endpoints did not come online"))?;

    let content = gen_bytes(1_000_000);
    std::fs::write(a_folder.path().join("payload.bin"), &content)?;
    let created = master.create_share(a_folder.path(), vec![]).await?;

    // viewer1 syncs from the master and references the file.
    let share_id = viewer1
        .add_share(
            &created.viewer_key,
            b_folder.path(),
            vec![master.endpoint_addr()],
        )
        .await?;
    let want = snapshot(a_folder.path());
    sync_until(&mut viewer1, &share_id, b_folder.path(), &want).await?;

    // Master goes offline — viewer1 is now the only source.
    let viewer1_addr = viewer1.endpoint_addr();
    master.shutdown().await?;

    // viewer2 joins bootstrapped ONLY to viewer1; it must sync the doc + content
    // (manifest + the referenced file) from viewer1.
    let share_id2 = viewer2
        .add_share(&created.viewer_key, c_folder.path(), vec![viewer1_addr])
        .await?;
    assert_eq!(share_id2, share_id);

    // viewer1 keeps reconciling (so it stays a live sync peer) while viewer2 pulls.
    let res = tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            let _ = viewer1.apply(&share_id).await;
            let _ = viewer2.apply(&share_id2).await?;
            if snapshot(c_folder.path()) == want {
                return anyhow::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    res.map_err(|_| anyhow::anyhow!("viewer2 did not sync from referenced viewer1"))??;
    assert_eq!(std::fs::read(c_folder.path().join("payload.bin"))?, content);
    println!("referenced viewer re-served content to a peer OK");

    viewer1.shutdown().await?;
    viewer2.shutdown().await?;
    Ok(())
}
