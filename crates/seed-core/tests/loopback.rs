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

/// Known-issues #14: a viewer must honor the master's *replicated* ignore list,
/// not silently fall back to its own (empty) local one. The list used to be stored
/// as a value blob that the viewer — content auto-download disabled — never
/// fetched, so it fell back to local and DELETED files the master had ignored. Now
/// the list rides the doc *key* (`\x00i/`), so it reaches the viewer with the
/// manifest. Control: a *non*-ignored rogue file IS still reverted, proving the
/// mirror is live and only the ignore rule spared the ignored file.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn viewer_honors_replicated_ignore_list() -> anyhow::Result<()> {
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

    // Master shares one file and ignores everything matching `*.log`.
    std::fs::write(a_folder.path().join("readme.txt"), b"hello")?;
    let created = master
        .create_share(a_folder.path(), vec!["*.log".to_string()])
        .await?;
    let master_addr = master.endpoint_addr();
    let share_id = viewer
        .add_share(&created.viewer_key, b_folder.path(), vec![master_addr])
        .await?;
    assert_eq!(share_id, created.share_id);

    // Initial sync: viewer mirrors readme.txt (and, alongside the manifest,
    // receives the master's `\x00i/` ignore entry).
    let want = snapshot(a_folder.path());
    sync_until(&mut viewer, &share_id, b_folder.path(), &want).await?;

    // Two purely-local files the master doesn't have:
    //   secret.log -> matches the master's ignore rule; must be PRESERVED.
    //   rogue.txt  -> not ignored; the strict mirror must REVERT (delete) it.
    let secret = b_folder.path().join("secret.log");
    let rogue = b_folder.path().join("rogue.txt");
    std::fs::write(&secret, b"local only")?;
    std::fs::write(&rogue, b"should vanish")?;

    // Drive reconcile passes until the un-ignored rogue file is gone — then the
    // ignored file has survived every one of those same passes.
    tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            for j in master.presence_broadcasts() {
                j.send().await;
            }
            for j in viewer.presence_broadcasts() {
                j.send().await;
            }
            let _ = master.apply(&share_id).await;
            let _ = viewer.apply(&share_id).await;
            if !rogue.exists() {
                return anyhow::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("viewer never reverted the un-ignored rogue.txt"))??;

    assert!(
        secret.is_file(),
        "viewer deleted a file the master ignored — replicated ignore list not honored (known-issues #14)"
    );
    assert_eq!(
        std::fs::read(&secret)?,
        b"local only",
        "an ignored local file must be left byte-untouched"
    );
    assert_eq!(std::fs::read(b_folder.path().join("readme.txt"))?, b"hello");

    master.shutdown().await?;
    viewer.shutdown().await?;
    println!("viewer honored replicated ignore list (secret.log preserved, rogue.txt reverted)");
    Ok(())
}

/// A genuinely empty (0-byte) file must sync — including a unicode-named one.
/// iroh-docs filters 0-byte entries out of queries as deletion markers, so empty
/// files ride a dedicated `\x00e/<path>` control key (non-empty marker value) and
/// peers create them directly. Regression: a 0-byte `café.txt` must not be
/// confused with a tombstone, and must mirror to peers.
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

/// Distributed sourcing: once one viewer holds a file, a second viewer that joins
/// later must source it from that **peer**, not re-download it from the master —
/// even though both viewers joined bootstrapped to the master (as peers do from a
/// share key, which carries the master's id). Guards the property that the master
/// is not the exclusive uploader in the staggered case; see
/// `docs/distributed-downloads.md`.
///
/// NOTE: this exercises the *staggered* path (viewer1 fully synced before viewer2
/// joins), which both the engine-driven downloader and stock iroh-docs' own
/// `ContentReady` gossip already distribute. It does **not** cover the concurrent
/// case (both viewers pulling a brand-new file at once), where the master is the
/// only holder and necessarily serves both — see the doc's "what doesn't help yet"
/// section. We assert by byte accounting: the master's sent bytes during viewer2's
/// sync stay well under one file because viewer1 serves it.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn second_viewer_sources_from_peer_not_master() -> anyhow::Result<()> {
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

    // A 4 MiB file: large enough that re-serving it is an unmistakable jump in the
    // master's sent bytes, dwarfing doc/gossip/presence overhead.
    let content = gen_bytes(4 * 1024 * 1024);
    std::fs::write(a_folder.path().join("payload.bin"), &content)?;
    let created = master.create_share(a_folder.path(), vec![]).await?;
    let master_addr = master.endpoint_addr();

    // viewer1 joins (bootstrapped to the master) and syncs the file: the master
    // uploads it once, here.
    let share_id = viewer1
        .add_share(
            &created.viewer_key,
            b_folder.path(),
            vec![master_addr.clone()],
        )
        .await?;
    let want = snapshot(a_folder.path());
    sync_until(&mut viewer1, &share_id, b_folder.path(), &want).await?;

    // Baseline the master's sent bytes *after* viewer1 is fully served, so the
    // window below captures only what the master uploads on viewer2's behalf.
    let master_sent_before = master.byte_totals().0;

    // viewer2 joins the same way — bootstrapped to the master, NOT to viewer1. It
    // learns viewer1 as a peer through the share's presence/doc mesh (rooted at the
    // master) and pulls the content from viewer1 rather than the master. The master
    // and viewer1 keep reconciling so they stay live sync peers (and propagate
    // viewer1's address to viewer2).
    let share_id2 = viewer2
        .add_share(&created.viewer_key, c_folder.path(), vec![master_addr])
        .await?;
    assert_eq!(share_id2, share_id);

    let res = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let _ = master.reconcile(&share_id).await;
            let _ = viewer1.apply(&share_id).await;
            let _ = viewer2.apply(&share_id2).await?;
            if snapshot(c_folder.path()) == want {
                return anyhow::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    res.map_err(|_| anyhow::anyhow!("viewer2 did not sync"))??;
    assert_eq!(std::fs::read(c_folder.path().join("payload.bin"))?, content);

    let master_delta = master.byte_totals().0.saturating_sub(master_sent_before);
    let file = content.len() as u64;

    // The master must NOT have re-served the whole file to viewer2: viewer1 did.
    // (Old behavior: master serves viewer2 too → master_delta >= ~one file.)
    assert!(
        master_delta < file / 2,
        "master sent {master_delta} B serving viewer2 (>= half the {file} B file): \
         it re-uploaded the file instead of letting viewer1 serve it — the master is \
         still the exclusive uploader"
    );
    println!(
        "load distributed: master sent only {master_delta} B while viewer2 fetched a \
         {file} B file (sourced from viewer1)"
    );

    master.shutdown().await?;
    viewer1.shutdown().await?;
    viewer2.shutdown().await?;
    Ok(())
}

/// Real multi-source swarm for a SINGLE large blob (the ISO case): a file big
/// enough to swarm, already held by two seeders, must be (a) reassembled correctly
/// by a fresh viewer from concurrent chunk-range fetches, and (b) sourced from BOTH
/// seeders rather than pulled whole from one. The master is taken offline so the
/// only sources are the two seeder peers — isolating the swarm. Without chunking,
/// one seeder serves the whole file and the other ~nothing; with it, each serves
/// roughly half. See `docs/distributed-downloads.md`.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn large_blob_swarms_across_two_seeders() -> anyhow::Result<()> {
    let a_data = tempfile::tempdir()?;
    let b_data = tempfile::tempdir()?;
    let c_data = tempfile::tempdir()?;
    let d_data = tempfile::tempdir()?;
    let a_folder = tempfile::tempdir()?;
    let b_folder = tempfile::tempdir()?;
    let c_folder = tempfile::tempdir()?;
    let d_folder = tempfile::tempdir()?;

    let mut master = Engine::new(a_data.path()).await?;
    let mut seeder1 = Engine::new(b_data.path()).await?;
    let mut seeder2 = Engine::new(c_data.path()).await?;
    let mut viewer = Engine::new(d_data.path()).await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        master.wait_online().await;
        seeder1.wait_online().await;
        seeder2.wait_online().await;
        viewer.wait_online().await;
    })
    .await
    .map_err(|_| anyhow::anyhow!("endpoints did not come online"))?;

    // 8 MiB — above the swarm threshold, so it splits across providers.
    let content = gen_bytes(8 * 1024 * 1024);
    std::fs::write(a_folder.path().join("disk.iso"), &content)?;
    let created = master.create_share(a_folder.path(), vec![]).await?;
    let master_addr = master.endpoint_addr();
    let want = snapshot(a_folder.path());

    // Both seeders fully sync the file from the master.
    let share_id = seeder1
        .add_share(
            &created.viewer_key,
            b_folder.path(),
            vec![master_addr.clone()],
        )
        .await?;
    seeder2
        .add_share(&created.viewer_key, c_folder.path(), vec![master_addr])
        .await?;
    sync_until(&mut seeder1, &share_id, b_folder.path(), &want).await?;
    sync_until(&mut seeder2, &share_id, c_folder.path(), &want).await?;

    // Master goes offline: the two seeders are now the only sources.
    master.shutdown().await?;
    let seeder1_addr = seeder1.endpoint_addr();
    let seeder2_addr = seeder2.endpoint_addr();

    // Fresh viewer joins, bootstrapped to BOTH seeders so both are roster neighbors.
    let share_id_v = viewer
        .add_share(
            &created.viewer_key,
            d_folder.path(),
            vec![seeder1_addr, seeder2_addr],
        )
        .await?;
    assert_eq!(share_id_v, share_id);

    // Let presence converge so both seeders show online in the viewer's roster
    // *before* the first download tick — otherwise the engine might pick a
    // single-source path. The doc syncs in the background during this wait; no
    // content downloads until we start reconciling (iroh-docs auto-download is off).
    tokio::time::sleep(Duration::from_secs(6)).await;

    let s1_before = seeder1.byte_totals().0;
    let s2_before = seeder2.byte_totals().0;

    let res = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let _ = seeder1.apply(&share_id).await;
            let _ = seeder2.apply(&share_id).await;
            let _ = viewer.apply(&share_id_v).await?;
            if snapshot(d_folder.path()) == want {
                return anyhow::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    })
    .await;
    res.map_err(|_| anyhow::anyhow!("viewer did not sync the swarmed blob"))??;

    // (a) Correctness: concurrent ranged fetches reassembled the exact bytes.
    assert_eq!(
        std::fs::read(d_folder.path().join("disk.iso"))?,
        content,
        "swarmed blob did not reassemble to the original content"
    );

    // (b) Distribution: each seeder served a meaningful share (≈ half). Without
    // chunking one seeder would serve ~everything and the other ~nothing.
    let s1 = seeder1.byte_totals().0.saturating_sub(s1_before);
    let s2 = seeder2.byte_totals().0.saturating_sub(s2_before);
    let file = content.len() as u64;
    assert!(
        s1 >= file / 4 && s2 >= file / 4,
        "blob was not swarmed: seeder1 sent {s1} B, seeder2 sent {s2} B for a {file} B \
         file (expected each ≥ {} B — both seeders sharing the load)",
        file / 4
    );
    println!(
        "swarm OK: seeder1 served {s1} B, seeder2 served {s2} B of a {file} B blob \
         (each ~half)"
    );

    seeder1.shutdown().await?;
    seeder2.shutdown().await?;
    viewer.shutdown().await?;
    Ok(())
}

/// Health must reflect *actual local completeness*, for masters too. A source
/// master that holds all content reads 100%, but a co-master that has synced the
/// manifest yet is still fetching the content must NOT read a misleading 100% — it
/// reads the real (sub-100) percentage. Regression guard for the reported "every
/// member shows Health 100% while actively downloading" bug.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn health_reflects_incomplete_master() -> anyhow::Result<()> {
    let a_data = tempfile::tempdir()?;
    let b_data = tempfile::tempdir()?;
    let a_folder = tempfile::tempdir()?;
    let b_folder = tempfile::tempdir()?;

    let mut a = Engine::new(a_data.path()).await?;
    let mut b = Engine::new(b_data.path()).await?;
    tokio::time::timeout(Duration::from_secs(20), async {
        a.wait_online().await;
        b.wait_online().await;
    })
    .await
    .map_err(|_| anyhow::anyhow!("endpoints did not come online"))?;

    // 8 MiB, so a single reconcile can't complete B's download synchronously.
    let content = gen_bytes(8 * 1024 * 1024);
    std::fs::write(a_folder.path().join("big.bin"), &content)?;
    let created = a.create_share(a_folder.path(), vec![]).await?;

    // Source master holds all content → 100% (computed from completeness, not a
    // hardcoded master=100).
    let a_pct = a
        .list_summaries()
        .into_iter()
        .find(|s| s.share_id == created.share_id)
        .map(|s| s.percent)
        .expect("share summary");
    assert_eq!(
        a_pct, 100,
        "source master with all content should read 100%"
    );

    // Co-master B joins with the MASTER key, bootstrapped to A.
    let share_id = b
        .add_share(
            &created.master_key,
            b_folder.path(),
            vec![a.endpoint_addr()],
        )
        .await?;
    assert_eq!(share_id, created.share_id);

    // Let the manifest (doc) sync in the background; no content is fetched until we
    // reconcile (iroh-docs auto-download is off).
    tokio::time::sleep(Duration::from_secs(7)).await;

    // One reconcile: B sees big.bin is missing and kicks off the (async) download,
    // but the content cannot arrive within this synchronous pass — so B is a master
    // that knows the manifest but does not yet hold the content.
    b.reconcile(&share_id).await?;

    let b_pct = b
        .list_summaries()
        .into_iter()
        .find(|s| s.share_id == share_id)
        .map(|s| s.percent)
        .expect("share summary");
    assert!(
        b_pct < 100,
        "co-master still fetching content must not read 100% (read {b_pct}%) — health \
         is not reflecting completeness"
    );
    println!("health OK: source master 100%, fetching co-master {b_pct}%");

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

/// Cold-start relief: three viewers pull a brand-new large file at the same time,
/// with only the master holding it initially. The master should NOT have to upload
/// a full copy to each (~3×); randomized per-part master grace desynchronizes the
/// viewers so they pull different parts off the master first, become partial
/// seeders, and trade the rest among themselves. We assert the master's total
/// upload stays well under 3× the file. See `docs/distributed-downloads.md`.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn cold_start_relief_spreads_master_load() -> anyhow::Result<()> {
    let a_data = tempfile::tempdir()?;
    let b_data = tempfile::tempdir()?;
    let c_data = tempfile::tempdir()?;
    let d_data = tempfile::tempdir()?;
    let a_folder = tempfile::tempdir()?;
    let b_folder = tempfile::tempdir()?;
    let c_folder = tempfile::tempdir()?;
    let d_folder = tempfile::tempdir()?;

    let mut master = Engine::new(a_data.path()).await?;
    let mut v1 = Engine::new(b_data.path()).await?;
    let mut v2 = Engine::new(c_data.path()).await?;
    let mut v3 = Engine::new(d_data.path()).await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        master.wait_online().await;
        v1.wait_online().await;
        v2.wait_online().await;
        v3.wait_online().await;
    })
    .await
    .map_err(|_| anyhow::anyhow!("endpoints did not come online"))?;

    let content = gen_bytes(8 * 1024 * 1024);
    std::fs::write(a_folder.path().join("disk.iso"), &content)?;
    let created = master.create_share(a_folder.path(), vec![]).await?;
    let m = master.endpoint_addr();
    let want = snapshot(a_folder.path());

    // Each viewer is bootstrapped to the master AND the other viewers, so they can
    // discover and pull from one another (not just the master).
    let v1a = v1.endpoint_addr();
    let v2a = v2.endpoint_addr();
    let v3a = v3.endpoint_addr();
    let share_id = v1
        .add_share(
            &created.viewer_key,
            b_folder.path(),
            vec![m.clone(), v2a.clone(), v3a.clone()],
        )
        .await?;
    v2.add_share(
        &created.viewer_key,
        c_folder.path(),
        vec![m.clone(), v1a.clone(), v3a],
    )
    .await?;
    v3.add_share(&created.viewer_key, d_folder.path(), vec![m, v1a, v2a])
        .await?;

    // Let presence converge so each viewer sees the others online before downloads
    // start (the doc syncs in the background; no content fetches until we reconcile).
    tokio::time::sleep(Duration::from_secs(6)).await;

    let master_before = master.byte_totals().0;

    let res = tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let _ = master.reconcile(&share_id).await;
            let _ = v1.apply(&share_id).await;
            let _ = v2.apply(&share_id).await;
            let _ = v3.apply(&share_id).await;
            if snapshot(b_folder.path()) == want
                && snapshot(c_folder.path()) == want
                && snapshot(d_folder.path()) == want
            {
                return anyhow::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    })
    .await;
    res.map_err(|_| anyhow::anyhow!("viewers did not all sync"))??;

    // All three reassembled correctly.
    for f in [b_folder.path(), c_folder.path(), d_folder.path()] {
        assert_eq!(
            std::fs::read(f.join("disk.iso"))?,
            content,
            "bad reassembly"
        );
    }

    let master_delta = master.byte_totals().0.saturating_sub(master_before);
    let file = content.len() as u64;
    let ratio = master_delta as f64 / file as f64;
    println!(
        "cold-start: master uploaded {master_delta} B = {ratio:.2}x the {file} B file \
         for 3 concurrent cold viewers (no relief would be ~3x)"
    );
    assert!(
        ratio < 2.5,
        "master uploaded {ratio:.2}x the file to 3 viewers — cold-start relief is not \
         spreading load (expected < 2.5x; ~3x means each pulled a full copy from the master)"
    );

    master.shutdown().await?;
    v1.shutdown().await?;
    v2.shutdown().await?;
    v3.shutdown().await?;
    Ok(())
}

/// SMOKETEST for the "one locked file must not break the whole share" guarantee.
///
/// A master share contains readable files AND files locked by another process. The
/// locked files must NOT abort the master's reconcile: the readable files publish
/// and sync to a viewer regardless. Then, once the locks are released, the
/// previously-locked files must sync on their own — even though releasing a lock
/// doesn't change the folder's (size,mtime) signature — proving the tracked,
/// cheap per-file retry works. This is the regression guard for the reported bug
/// where a single locked file (e.g. an AOMEI `.lock`) silently blocked all syncing.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn locked_files_do_not_block_share_and_sync_after_unlock() -> anyhow::Result<()> {
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

    // Readable content (incl. a substantial file) plus two files we'll lock.
    let payload = gen_bytes(1024 * 1024);
    std::fs::write(a_folder.path().join("normal.txt"), b"hello")?;
    std::fs::write(a_folder.path().join("payload.bin"), &payload)?;
    let locked1 = a_folder.path().join("locked1.bin");
    let locked2 = a_folder.path().join("locked2.bin");
    std::fs::write(&locked1, b"first locked payload")?;
    std::fs::write(&locked2, b"second locked payload")?;

    // Lock both files for the duration of phase 1 (another process holds them).
    #[cfg(windows)]
    let guards = {
        use std::os::windows::fs::OpenOptionsExt;
        let g1 = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&locked1)?;
        let g2 = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&locked2)?;
        (g1, g2)
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked1, std::fs::Permissions::from_mode(0o000))?;
        std::fs::set_permissions(&locked2, std::fs::Permissions::from_mode(0o000))?;
    }

    let created = master.create_share(a_folder.path(), vec![]).await?;
    let share_id = viewer
        .add_share(
            &created.viewer_key,
            b_folder.path(),
            vec![master.endpoint_addr()],
        )
        .await?;

    // Phase 1: the readable files must sync to the viewer despite the locked ones.
    let mut want_partial = BTreeMap::new();
    want_partial.insert("normal.txt".to_string(), b"hello".to_vec());
    want_partial.insert("payload.bin".to_string(), payload.clone());
    sync_until(&mut viewer, &share_id, b_folder.path(), &want_partial).await?;
    assert!(
        !b_folder.path().join("locked1.bin").exists()
            && !b_folder.path().join("locked2.bin").exists(),
        "locked files must not have published while locked"
    );
    println!("phase 1 OK: readable files synced; locked files correctly skipped");

    // Honest status: while files are locked the master must NOT read "Healthy" — it
    // reports them as retrying, so a stuck/locked file is never hidden.
    let m = master
        .list_summaries()
        .into_iter()
        .find(|s| s.share_id == share_id)
        .expect("master share summary");
    assert!(
        m.retrying >= 2,
        "master should report >=2 files retrying while locked, got {}",
        m.retrying
    );
    assert!(
        !format!("{:?}", m.status).contains("Healthy"),
        "share must not read Healthy while files are locked/retrying (status {:?}, retrying {})",
        m.status,
        m.retrying
    );
    println!(
        "honest status OK: master reports {} file(s) retrying, status {:?}",
        m.retrying, m.status
    );

    // Phase 2: release the locks. The previously-locked files must now sync — note
    // releasing a lock does NOT change the folder's (size,mtime) signature, so this
    // exercises the tracked per-file retry, not a fresh full scan.
    #[cfg(windows)]
    drop(guards);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked1, std::fs::Permissions::from_mode(0o644))?;
        std::fs::set_permissions(&locked2, std::fs::Permissions::from_mode(0o644))?;
    }

    let want_full = snapshot(a_folder.path());
    let res = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            // Master retries the skipped files (and publishes them); viewer pulls.
            let _ = master.reconcile(&share_id).await;
            let _ = viewer.apply(&share_id).await?;
            if snapshot(b_folder.path()) == want_full {
                return anyhow::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    res.map_err(|_| anyhow::anyhow!("previously-locked files did not sync after unlock"))??;
    assert_eq!(
        std::fs::read(b_folder.path().join("locked1.bin"))?,
        b"first locked payload"
    );
    assert_eq!(
        std::fs::read(b_folder.path().join("locked2.bin"))?,
        b"second locked payload"
    );
    println!("phase 2 OK: previously-locked files synced after release (tracked retry)");

    // Once everything is published, the master is settled again: nothing retrying.
    let m = master
        .list_summaries()
        .into_iter()
        .find(|s| s.share_id == share_id)
        .expect("master share summary");
    assert_eq!(
        m.retrying, 0,
        "no files should be retrying after unlock + converge (status {:?})",
        m.status
    );
    println!("honest status OK: master settled, 0 retrying");

    master.shutdown().await?;
    viewer.shutdown().await?;
    Ok(())
}

/// Health/percent must reflect *real* byte progress on a large in-flight file:
/// while a big blob downloads, the reported percent climbs through intermediate
/// values instead of staying flat at the pre-download number and jumping straight
/// to done. Regression guard for "Syncing 18%" sitting still until a big file
/// finishes (old behavior counted an incomplete file as 0 bytes present).
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn health_reports_partial_progress_during_download() -> anyhow::Result<()> {
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

    // Large enough that the transfer spans many reconcile/poll ticks, so partial
    // percentages are observable (not an instant 0 -> 100 jump).
    let content = gen_bytes(32 * 1024 * 1024);
    std::fs::write(a_folder.path().join("big.bin"), &content)?;
    let created = master.create_share(a_folder.path(), vec![]).await?;
    let share_id = viewer
        .add_share(
            &created.viewer_key,
            b_folder.path(),
            vec![master.endpoint_addr()],
        )
        .await?;
    let want = snapshot(a_folder.path());

    let percent_of = |v: &Engine| -> u8 {
        v.list_summaries()
            .into_iter()
            .find(|s| s.share_id == share_id)
            .map(|s| s.percent)
            .unwrap_or(0)
    };

    let mut saw_partial = false;
    let mut max_partial = 0u8;
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        let _ = viewer.apply(&share_id).await?;
        let pct = percent_of(&viewer);
        if pct > 0 && pct < 100 {
            saw_partial = true;
            max_partial = max_partial.max(pct);
        }
        if snapshot(b_folder.path()) == want {
            break;
        }
        if std::time::Instant::now() > deadline {
            anyhow::bail!("timed out while downloading (last percent {pct})");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert_eq!(
        std::fs::read(b_folder.path().join("big.bin"))?,
        content,
        "file must have synced correctly"
    );
    assert!(
        saw_partial,
        "percent must climb through intermediate values during a large download \
         (never observed a value strictly between 0 and 100)"
    );
    // Final state reads 100.
    assert_eq!(percent_of(&viewer), 100, "completed share should read 100%");
    println!("partial-progress OK: observed intermediate percent up to {max_partial}%, then 100%");

    master.shutdown().await?;
    viewer.shutdown().await?;
    Ok(())
}

/// Pausing a share must actually STOP an in-flight download, not just stop
/// starting new ones. Regression guard for the reported bug where pausing in the
/// GUI did nothing and the transfer ran to completion until the service was
/// stopped. We pause mid-download and assert the viewer stops receiving bytes
/// (without the fix it would keep pulling the rest of the file).
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn pausing_a_share_stops_in_flight_download() -> anyhow::Result<()> {
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

    // Loopback transfers run at multiple GB/s, so we can't reliably pause at a low
    // *percentage*. Instead use a large file and pause at the FIRST observed partial
    // (caught with a tight loop), leaving plenty that would still arrive if the
    // transfer weren't cancelled — which a small absolute byte threshold detects.
    let content = gen_bytes(256 * 1024 * 1024);
    std::fs::write(a_folder.path().join("big.bin"), &content)?;
    let created = master.create_share(a_folder.path(), vec![]).await?;
    let share_id = viewer
        .add_share(
            &created.viewer_key,
            b_folder.path(),
            vec![master.endpoint_addr()],
        )
        .await?;

    let percent_of = |v: &Engine| -> u8 {
        v.list_summaries()
            .into_iter()
            .find(|s| s.share_id == share_id)
            .map(|s| s.percent)
            .unwrap_or(0)
    };

    // Pause at the first sign of *partial* progress. Note the percent reads 100
    // before the manifest syncs (empty view), drops to a partial value once the
    // file is known and downloading, then returns to 100 when complete — so we
    // pause on the first value strictly between 0 and 100 (never on 100), and let
    // the deadline catch a genuine "too fast to observe" case.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let paused_at;
    loop {
        let _ = viewer.apply(&share_id).await?;
        let pct = percent_of(&viewer);
        if (1..100).contains(&pct) {
            viewer.set_paused(&share_id, true)?;
            paused_at = pct;
            break;
        }
        if std::time::Instant::now() > deadline {
            anyhow::bail!("never observed a partial download state (manifest not syncing?)");
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    // If pause didn't cancel the transfer, the viewer keeps receiving the rest of the
    // file. Measure endpoint received bytes over a window; with the fix it stays tiny.
    let recv0 = viewer.byte_totals().1;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let delta = viewer.byte_totals().1.saturating_sub(recv0);
    let threshold = 16 * 1024 * 1024; // 16 MiB — far below what the remaining file would be
    assert!(
        delta < threshold,
        "after pausing at {paused_at}%, the viewer received {delta} more bytes in 5s \
         — pause did not stop the in-flight download (expected < {threshold} B)"
    );
    // Paused mid-download, it must not read complete.
    assert!(
        percent_of(&viewer) < 100,
        "share reads 100% despite being paused mid-download"
    );
    println!(
        "pause OK: paused at {paused_at}%, only {delta} B received in 5s after pause \
         (transfer stopped)"
    );

    master.shutdown().await?;
    viewer.shutdown().await?;
    Ok(())
}

/// Drive two engines' reconcile loops until *both* folders equal `want`, or time
/// out. Models the daemon ticking each node.
async fn converge_two(
    a: &mut Engine,
    a_id: &str,
    a_folder: &Path,
    b: &mut Engine,
    b_id: &str,
    b_folder: &Path,
    want: &BTreeMap<String, Vec<u8>>,
) -> anyhow::Result<()> {
    let res = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let _ = a.reconcile(a_id).await;
            let _ = b.reconcile(b_id).await;
            if &snapshot(a_folder) == want && &snapshot(b_folder) == want {
                return anyhow::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    match res {
        Ok(inner) => inner,
        Err(_) => Err(anyhow::anyhow!(
            "timed out; a={:?} b={:?} want={:?}",
            snapshot(a_folder).keys().collect::<Vec<_>>(),
            snapshot(b_folder).keys().collect::<Vec<_>>(),
            want.keys().collect::<Vec<_>>()
        )),
    }
}

/// Two **masters** of the same share (the second added with the *master* key) must
/// converge bidirectionally: a file added on either side reaches the other, a
/// delete propagates, a concurrent same-path edit resolves last-writer-wins, and a
/// restarted node reconnects and re-converges. This is the multi-master feature the
/// original design called for: only master key holders can modify the share
/// (plural).
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn two_masters_converge_bidirectionally() -> anyhow::Result<()> {
    let a_data = tempfile::tempdir()?;
    let b_data = tempfile::tempdir()?;
    let a_folder = tempfile::tempdir()?;
    let b_folder = tempfile::tempdir()?;

    let mut a = Engine::new(a_data.path()).await?;
    let mut b = Engine::new(b_data.path()).await?;
    tokio::time::timeout(Duration::from_secs(20), async {
        a.wait_online().await;
        b.wait_online().await;
    })
    .await
    .map_err(|_| anyhow::anyhow!("endpoints did not come online"))?;

    // A creates the share with one file.
    std::fs::write(a_folder.path().join("a.txt"), b"from A")?;
    let created = a.create_share(a_folder.path(), vec![]).await?;
    let a_addr = a.endpoint_addr();

    // B joins with the **master** key (becomes a co-master), bootstrapped to A, and
    // independently drops in its own file *before* the first sync.
    let share_id = b
        .add_share(&created.master_key, b_folder.path(), vec![a_addr])
        .await?;
    assert_eq!(share_id, created.share_id);
    std::fs::write(b_folder.path().join("b.txt"), b"from B")?;

    // 1. Both files must reach both sides (true bidirectional add).
    let mut want = BTreeMap::new();
    want.insert("a.txt".to_string(), b"from A".to_vec());
    want.insert("b.txt".to_string(), b"from B".to_vec());
    converge_two(
        &mut a,
        &share_id,
        a_folder.path(),
        &mut b,
        &share_id,
        b_folder.path(),
        &want,
    )
    .await?;
    println!("bidirectional add OK");

    // 2. A delete on A propagates to B.
    std::fs::remove_file(a_folder.path().join("a.txt"))?;
    let mut want = BTreeMap::new();
    want.insert("b.txt".to_string(), b"from B".to_vec());
    converge_two(
        &mut a,
        &share_id,
        a_folder.path(),
        &mut b,
        &share_id,
        b_folder.path(),
        &want,
    )
    .await?;
    assert!(!b_folder.path().join("a.txt").exists());
    println!("bidirectional delete OK");

    // 3. Concurrent same-path edit → last-writer-wins. First get c.txt onto both,
    //    then edit it on B *after* a delay so B's mtime is strictly newer.
    std::fs::write(a_folder.path().join("c.txt"), b"c from A")?;
    let mut want = BTreeMap::new();
    want.insert("b.txt".to_string(), b"from B".to_vec());
    want.insert("c.txt".to_string(), b"c from A".to_vec());
    converge_two(
        &mut a,
        &share_id,
        a_folder.path(),
        &mut b,
        &share_id,
        b_folder.path(),
        &want,
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    std::fs::write(b_folder.path().join("c.txt"), b"c from B (newer)")?;
    let mut want = BTreeMap::new();
    want.insert("b.txt".to_string(), b"from B".to_vec());
    want.insert("c.txt".to_string(), b"c from B (newer)".to_vec());
    converge_two(
        &mut a,
        &share_id,
        a_folder.path(),
        &mut b,
        &share_id,
        b_folder.path(),
        &want,
    )
    .await?;
    println!("last-writer-wins OK");

    // 4. Restart B (drop + reopen its data dir). It must reconnect to A — using the
    //    creator endpoint id preserved in its stored key — and re-converge after a
    //    fresh change on A.
    b.shutdown().await?;
    let mut b = Engine::new(b_data.path()).await?;
    b.wait_online().await;
    std::fs::write(a_folder.path().join("d.txt"), b"after restart")?;
    let mut want = BTreeMap::new();
    want.insert("b.txt".to_string(), b"from B".to_vec());
    want.insert("c.txt".to_string(), b"c from B (newer)".to_vec());
    want.insert("d.txt".to_string(), b"after restart".to_vec());
    converge_two(
        &mut a,
        &share_id,
        a_folder.path(),
        &mut b,
        &share_id,
        b_folder.path(),
        &want,
    )
    .await?;
    println!("restart reconnect OK");

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

/// Cross-member divergence detection must NOT false-alarm on a healthy share: two
/// masters that agree on the fileset compute the SAME manifest fingerprint, exchange
/// it via presence, and never enter the "out of sync" state. (The fingerprint's
/// correctness — different filesets → different fingerprints — is unit-tested in
/// `engine.rs`. A sustained real partition tripping `OutOfSync` after the settle
/// window can't be held open in loopback — connected members re-converge in
/// seconds — so that positive end-to-end path isn't auto-tested here; the
/// fingerprint unit test plus the comparison logic cover its two halves.)
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn agreeing_masters_share_fingerprint_and_never_out_of_sync() -> anyhow::Result<()> {
    let a_data = tempfile::tempdir()?;
    let b_data = tempfile::tempdir()?;
    let a_folder = tempfile::tempdir()?;
    let b_folder = tempfile::tempdir()?;

    let mut a = Engine::new(a_data.path()).await?;
    let mut b = Engine::new(b_data.path()).await?;
    tokio::time::timeout(Duration::from_secs(20), async {
        a.wait_online().await;
        b.wait_online().await;
    })
    .await
    .map_err(|_| anyhow::anyhow!("endpoints did not come online"))?;

    std::fs::write(a_folder.path().join("a.txt"), b"from A")?;
    let created = a.create_share(a_folder.path(), vec![]).await?;
    let a_addr = a.endpoint_addr();
    let share_id = b
        .add_share(&created.master_key, b_folder.path(), vec![a_addr])
        .await?;
    std::fs::write(b_folder.path().join("b.txt"), b"from B")?;

    let mut want = BTreeMap::new();
    want.insert("a.txt".to_string(), b"from A".to_vec());
    want.insert("b.txt".to_string(), b"from B".to_vec());
    converge_two(
        &mut a,
        &share_id,
        a_folder.path(),
        &mut b,
        &share_id,
        b_folder.path(),
        &want,
    )
    .await?;

    // Helper: the fingerprint this engine has received from its (single) peer, and
    // this engine's own fingerprint, as exposed in the peers list.
    let peer_fp = |e: &Engine, sid: &str| -> Option<u64> {
        e.peers(sid)
            .ok()?
            .into_iter()
            .find(|p| p.node_id != "This device")
            .map(|p| p.manifest_fp)
    };
    let self_fp = |e: &Engine, sid: &str| -> Option<u64> {
        e.peers(sid)
            .ok()?
            .into_iter()
            .find(|p| p.node_id == "This device")
            .map(|p| p.manifest_fp)
    };

    // Drive presence both ways (and keep reconciling so the divergence comparison
    // runs) until each side has received the other's non-zero fingerprint.
    let res = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            for pb in a.presence_broadcasts() {
                pb.send().await;
            }
            for pb in b.presence_broadcasts() {
                pb.send().await;
            }
            let _ = a.reconcile(&share_id).await;
            let _ = b.reconcile(&share_id).await;
            if peer_fp(&a, &share_id).unwrap_or(0) != 0 && peer_fp(&b, &share_id).unwrap_or(0) != 0
            {
                return anyhow::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    })
    .await;
    res.map_err(|_| anyhow::anyhow!("peers never exchanged manifest fingerprints"))??;

    let a_self = self_fp(&a, &share_id).unwrap();
    let b_self = self_fp(&b, &share_id).unwrap();
    assert_ne!(a_self, 0, "fingerprint must be computed");
    assert_eq!(
        a_self, b_self,
        "agreeing masters must compute the same manifest fingerprint"
    );
    assert_eq!(
        peer_fp(&a, &share_id).unwrap(),
        a_self,
        "A must see B's fingerprint equal to its own"
    );
    assert_eq!(
        peer_fp(&b, &share_id).unwrap(),
        b_self,
        "B must see A's fingerprint equal to its own"
    );

    for (label, e) in [("A", &a), ("B", &b)] {
        let s = e
            .list_summaries()
            .into_iter()
            .find(|s| s.share_id == share_id)
            .expect("share summary");
        assert!(
            !format!("{:?}", s.status).contains("OutOfSync"),
            "{label} must not read OutOfSync when members agree (status {:?})",
            s.status
        );
    }
    println!("divergence false-alarm guard OK: matching fingerprints exchanged, no OutOfSync");

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

/// Deep verify catches drift the cheap change-signature can't: a viewer file
/// corrupted IN PLACE with the same size and a restored mtime leaves the
/// `(size, mtime)` signature unchanged, so an ordinary reconcile never re-hashes it
/// and the corruption survives. A forced deep verify re-hashes the file, detects the
/// mismatch against the manifest, and re-materializes the correct content. The first
/// assertion deliberately proves the gap exists (normal reconcile misses it) so the
/// test can't silently pass if deep verify became a no-op.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn deep_verify_heals_corruption_a_normal_reconcile_misses() -> anyhow::Result<()> {
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

    let good = gen_bytes(4096);
    std::fs::write(a_folder.path().join("x.bin"), &good)?;
    let created = master.create_share(a_folder.path(), vec![]).await?;
    let share_id = viewer
        .add_share(
            &created.viewer_key,
            b_folder.path(),
            vec![master.endpoint_addr()],
        )
        .await?;

    let want = snapshot(a_folder.path());
    sync_until(&mut viewer, &share_id, b_folder.path(), &want).await?;

    // Corrupt the viewer's copy in place: same length, mtime restored, so the
    // (size, mtime) change-signature is identical and a normal scan is skipped.
    let path = b_folder.path().join("x.bin");
    let orig_mtime = std::fs::metadata(&path)?.modified()?;
    let mut corrupt = good.clone();
    corrupt[0] ^= 0xff;
    corrupt[1000] ^= 0xff;
    assert_eq!(corrupt.len(), good.len());
    std::fs::write(&path, &corrupt)?;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)?
        .set_modified(orig_mtime)?;

    // A normal reconcile does NOT notice (signature unchanged): corruption survives.
    for _ in 0..6 {
        let _ = viewer.apply(&share_id).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    assert_eq!(
        std::fs::read(&path)?,
        corrupt,
        "ordinary reconcile must miss same-size+mtime corruption (proves the gap exists)"
    );
    println!("normal reconcile missed in-place corruption (as expected)");

    // Deep verify re-hashes and heals it.
    viewer.request_deep_verify(&share_id);
    sync_until(&mut viewer, &share_id, b_folder.path(), &want).await?;
    assert_eq!(
        std::fs::read(&path)?,
        good,
        "deep verify must restore the correct content from the manifest"
    );
    println!("deep verify healed the corruption OK");

    master.shutdown().await?;
    viewer.shutdown().await?;
    Ok(())
}

/// Re-kicking a share's doc live-sync (the self-heal action used on divergence) must
/// be safe on an already-synced share: after an explicit resync on both members, a
/// fresh change must still propagate. Guards against `resync_doc` breaking the
/// replication it's meant to repair.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn doc_resync_does_not_break_replication() -> anyhow::Result<()> {
    let a_data = tempfile::tempdir()?;
    let b_data = tempfile::tempdir()?;
    let a_folder = tempfile::tempdir()?;
    let b_folder = tempfile::tempdir()?;

    let mut a = Engine::new(a_data.path()).await?;
    let mut b = Engine::new(b_data.path()).await?;
    tokio::time::timeout(Duration::from_secs(20), async {
        a.wait_online().await;
        b.wait_online().await;
    })
    .await
    .map_err(|_| anyhow::anyhow!("endpoints did not come online"))?;

    std::fs::write(a_folder.path().join("a.txt"), b"from A")?;
    let created = a.create_share(a_folder.path(), vec![]).await?;
    let a_addr = a.endpoint_addr();
    let share_id = b
        .add_share(&created.master_key, b_folder.path(), vec![a_addr])
        .await?;
    std::fs::write(b_folder.path().join("b.txt"), b"from B")?;

    let mut want = BTreeMap::new();
    want.insert("a.txt".to_string(), b"from A".to_vec());
    want.insert("b.txt".to_string(), b"from B".to_vec());
    converge_two(
        &mut a,
        &share_id,
        a_folder.path(),
        &mut b,
        &share_id,
        b_folder.path(),
        &want,
    )
    .await?;

    // Explicitly re-kick doc live-sync on both members (idempotent self-heal action).
    a.resync_doc(&share_id).await?;
    b.resync_doc(&share_id).await?;

    // A fresh change must still propagate both ways after the resync.
    std::fs::write(a_folder.path().join("c.txt"), b"after resync")?;
    let mut want = BTreeMap::new();
    want.insert("a.txt".to_string(), b"from A".to_vec());
    want.insert("b.txt".to_string(), b"from B".to_vec());
    want.insert("c.txt".to_string(), b"after resync".to_vec());
    converge_two(
        &mut a,
        &share_id,
        a_folder.path(),
        &mut b,
        &share_id,
        b_folder.path(),
        &want,
    )
    .await?;
    println!("doc resync safe: replication still converges");

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}
