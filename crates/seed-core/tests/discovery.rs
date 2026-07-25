//! Auto-discovery: a viewer joins a share with NO explicit bootstrap address,
//! relying on the master endpoint id carried in the key + n0 DNS discovery.
//!
//! `#[ignore]` — needs real network reachability to n0 discovery and some
//! propagation time; run with `-- --ignored`.

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

#[tokio::test]
#[ignore = "needs n0 DNS discovery reachability; run with --ignored"]
async fn viewer_joins_via_discovery_without_bootstrap() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let m_data = tmp.path().join("m-data");
    let v_data = tmp.path().join("v-data");
    let m_folder = tmp.path().join("m-folder");
    let v_folder = tmp.path().join("v-folder");
    for d in [&m_data, &v_data, &m_folder, &v_folder] {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(m_folder.join("hello.txt"), b"discovered!")?;

    let mut master = Engine::new(&m_data).await?;
    let mut viewer = Engine::new(&v_data).await?;
    // Both must be online so the master publishes its address to discovery.
    tokio::time::timeout(Duration::from_secs(30), async {
        master.wait_online().await;
        viewer.wait_online().await;
    })
    .await
    .map_err(|_| anyhow::anyhow!("not online"))?;

    let created = master.create_share(&m_folder, vec![]).await?;
    let _seed = common::SecretGuard::new(&created.share_id);
    // Give discovery a moment to publish the master's record.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // NOTE: no bootstrap address — must resolve via the key's endpoint id.
    let vid = viewer
        .add_share(&created.viewer_key, &v_folder, vec![])
        .await?;

    let want = snapshot(&m_folder);
    let ok = tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let _ = viewer.apply(&vid).await;
            if snapshot(&v_folder) == want {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(ok, "viewer never synced via discovery");

    master.shutdown().await?;
    viewer.shutdown().await?;
    Ok(())
}
