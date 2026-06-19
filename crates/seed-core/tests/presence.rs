//! Presence: pool members exchange their display name + sync health over gossip,
//! and it shows up in `Engine::peers()`.

use std::time::Duration;

use seed_core::Engine;

/// Find a *remote* peer (not "This device") by display name, returning its
/// reported health percent.
fn remote_peer_percent(engine: &Engine, share_id: &str, name: &str) -> Option<u8> {
    engine
        .peers(share_id)
        .ok()?
        .into_iter()
        .find(|p| p.node_id != "This device" && p.name.as_deref() == Some(name))
        .map(|p| p.percent)
}

#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn presence_propagates_name_and_health() -> anyhow::Result<()> {
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

    std::fs::write(a_folder.path().join("a.txt"), b"hello")?;
    std::fs::write(a_folder.path().join("b.txt"), b"world!")?;

    // Each side names itself.
    master.set_device_name("alice")?;
    viewer.set_device_name("bob")?;

    let created = master.create_share(a_folder.path(), vec![]).await?;
    let master_addr = master.endpoint_addr();
    let share_id = viewer
        .add_share(&created.viewer_key, b_folder.path(), vec![master_addr])
        .await?;

    // Drive what the reconcile loop does — broadcast presence from both sides and
    // let the viewer apply — until each side sees the other's name at full health.
    let result = tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            for j in master.presence_broadcasts() {
                j.send().await;
            }
            for j in viewer.presence_broadcasts() {
                j.send().await;
            }
            let _ = viewer.apply_all_viewers().await;

            let bob_on_master = remote_peer_percent(&master, &share_id, "bob");
            let alice_on_viewer = remote_peer_percent(&viewer, &share_id, "alice");
            if bob_on_master == Some(100) && alice_on_viewer == Some(100) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "presence didn't converge: bob_on_master={:?}, alice_on_viewer={:?}",
        remote_peer_percent(&master, &share_id, "bob"),
        remote_peer_percent(&viewer, &share_id, "alice"),
    );

    // This device shows its own name in the peer list, too.
    let mine = master.peers(&share_id)?;
    assert!(mine.iter().any(|p| p.name.as_deref() == Some("alice")));

    master.shutdown().await?;
    viewer.shutdown().await?;
    println!("presence name + health propagation OK");
    Ok(())
}
