//! Persistence / restart-safety: a viewer that has synced a share keeps it
//! across an engine restart, and can restore its folder from the persisted local
//! doc + blob stores even with the master offline.
//!
//! `#[ignore]` (opens real iroh endpoints); run with `-- --ignored`.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use seed_core::Engine;

fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut map = BTreeMap::new();
    for d in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if d.file_type().is_file() {
            let rel = d
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            map.insert(rel, std::fs::read(d.path()).unwrap());
        }
    }
    map
}

async fn apply_until(
    engine: &mut Engine,
    id: &str,
    folder: &Path,
    want: &BTreeMap<String, Vec<u8>>,
) -> anyhow::Result<()> {
    for _ in 0..120 {
        let _ = engine.apply(id).await?;
        if &snapshot(folder) == want {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::bail!(
        "timeout: have {:?} want {:?}",
        snapshot(folder).keys().collect::<Vec<_>>(),
        want.keys().collect::<Vec<_>>()
    )
}

/// A master keeps its write capability across a restart: the seed is reloaded
/// (from the OS keystore where available, else the DB fallback) and the master
/// key + publish still work.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn master_keeps_write_capability_after_restart() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let data = tmp.path().join("m-data");
    let folder = tmp.path().join("m-folder");
    std::fs::create_dir_all(&data)?;
    std::fs::create_dir_all(&folder)?;
    std::fs::write(folder.join("a.txt"), b"one")?;

    let (share_id, master_key) = {
        let mut m = Engine::new(&data).await?;
        let c = m.create_share(&folder, vec![]).await?;
        m.shutdown().await?;
        (c.share_id, c.master_key)
    };
    // Outside the block on purpose: the whole point of this test is that the
    // seed survives the restart below, so the guard must outlive it.
    let _seed = common::SecretGuard::new(&share_id);

    // Restart on the same data dir.
    let mut m2 = Engine::new(&data).await?;
    let (revealed_master, _viewer) = m2.reveal_keys(&share_id)?;
    assert_eq!(
        revealed_master.as_deref(),
        Some(master_key.as_str()),
        "master key must survive restart"
    );
    // Write capability still works (would error if reloaded read-only).
    std::fs::write(folder.join("b.txt"), b"two")?;
    m2.publish(&share_id).await?;
    m2.shutdown().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn viewer_restores_from_local_store_after_restart() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let m_data = tmp.path().join("m-data");
    let v_data = tmp.path().join("v-data");
    let m_folder = tmp.path().join("m-folder");
    let v_folder = tmp.path().join("v-folder");
    for d in [&m_data, &v_data, &m_folder, &v_folder] {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(m_folder.join("a.txt"), b"alpha")?;
    std::fs::create_dir_all(m_folder.join("d"))?;
    std::fs::write(m_folder.join("d/b.txt"), b"bravo")?;

    let want = snapshot(&m_folder);
    let vid;
    {
        let mut master = Engine::new(&m_data).await?;
        let mut viewer = Engine::new(&v_data).await?;
        tokio::time::timeout(Duration::from_secs(20), async {
            master.wait_online().await;
            viewer.wait_online().await;
        })
        .await
        .map_err(|_| anyhow::anyhow!("not online"))?;

        let created = master.create_share(&m_folder, vec![]).await?;
        let m_addr = master.endpoint_addr();
        vid = viewer
            .add_share(&created.viewer_key, &v_folder, vec![m_addr])
            .await?;

        apply_until(&mut viewer, &vid, &v_folder, &want).await?;

        // Shut both down cleanly so the stores are released.
        master.shutdown().await?;
        viewer.shutdown().await?;
    }
    // `vid` is the share id (master and viewer share one). Guard at function
    // scope so the master's seed outlives the restarts below.
    let _seed = common::SecretGuard::new(&vid);

    // Simulate the viewer's files being lost while it was down.
    for entry in std::fs::read_dir(&v_folder)? {
        let p = entry?.path();
        if p.is_dir() {
            std::fs::remove_dir_all(&p)?;
        } else {
            std::fs::remove_file(&p)?;
        }
    }
    assert!(snapshot(&v_folder).is_empty());

    // Restart the viewer on the same data dir — with the MASTER OFFLINE.
    let mut viewer2 = Engine::new(&v_data).await?;
    // The share must have been reloaded from SQLite.
    let summaries = viewer2.list_summaries();
    assert_eq!(summaries.len(), 1, "share should persist across restart");
    assert_eq!(summaries[0].share_id, vid);

    // Reconcile should restore the folder from the persisted local stores.
    apply_until(&mut viewer2, &vid, &v_folder, &want).await?;
    assert_eq!(snapshot(&v_folder), want);

    viewer2.shutdown().await?;
    Ok(())
}
