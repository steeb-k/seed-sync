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

/// Whether `engine`'s roster shows a *remote* peer named `name` as an online master.
/// `role` is `seed_ipc::Role`, which the test crate doesn't depend on by name, so
/// match it via Debug (the enum derives it: "Master" / "Viewer").
fn sees_online_master(engine: &Engine, share_id: &str, name: &str) -> bool {
    engine
        .peers(share_id)
        .ok()
        .map(|peers| {
            peers.iter().any(|p| {
                p.node_id != "This device"
                    && p.name.as_deref() == Some(name)
                    && p.online
                    && format!("{:?}", p.role) == "Master"
            })
        })
        .unwrap_or(false)
}

/// Three masters, all sharing one master key, must each see the *other two* as named,
/// online masters. This is the all-to-all roster case that regressed: presence rides a
/// gossip swarm whose one-shot bootstrap is a star (the creator bootstraps with no
/// peers; B and C dial only the creator), so without active mesh repair the creator
/// receives nobody's presence and the leaves only see each other through the creator.
/// The fix drives `presence_rejoins()` each tick to connect every member doc-sync found.
///
/// NOTE: on loopback there's no NAT, so the raw star may already converge — this is
/// primarily a wiring/regression smoke test that exercises the 3-node path and the
/// `presence_rejoins()` plumbing. The authoritative check is the 3-machine deployment.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn presence_three_masters_all_to_all() -> anyhow::Result<()> {
    let datas: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let folders: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();

    let mut a = Engine::new(datas[0].path()).await?;
    let mut b = Engine::new(datas[1].path()).await?;
    let mut c = Engine::new(datas[2].path()).await?;
    tokio::time::timeout(Duration::from_secs(20), async {
        a.wait_online().await;
        b.wait_online().await;
        c.wait_online().await;
    })
    .await
    .map_err(|_| anyhow::anyhow!("endpoints did not come online"))?;

    a.set_device_name("alice")?;
    b.set_device_name("bob")?;
    c.set_device_name("carol")?;

    // A creates; B and C join from the *master* key (so all three are masters),
    // bootstrapping only from A — reproducing the star topology.
    let created = a.create_share(folders[0].path(), vec![]).await?;
    let a_addr = a.endpoint_addr();
    let share = created.share_id.clone();
    let share_b = b
        .add_share(&created.master_key, folders[1].path(), vec![a_addr.clone()])
        .await?;
    let share_c = c
        .add_share(&created.master_key, folders[2].path(), vec![a_addr])
        .await?;
    assert_eq!(share, share_b);
    assert_eq!(share, share_c);

    // Drive what the reconcile loop does on every node: broadcast presence and grow
    // the gossip mesh toward all-to-all, until each node sees the other two.
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            for e in [&a, &b, &c] {
                for j in e.presence_broadcasts() {
                    j.send().await;
                }
                for r in e.presence_rejoins() {
                    r.join().await;
                }
            }
            let converged = sees_online_master(&a, &share, "bob")
                && sees_online_master(&a, &share, "carol")
                && sees_online_master(&b, &share, "alice")
                && sees_online_master(&b, &share, "carol")
                && sees_online_master(&c, &share, "alice")
                && sees_online_master(&c, &share, "bob");
            if converged {
                return;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "roster didn't converge all-to-all: \
         a_sees_bob={} a_sees_carol={} b_sees_alice={} b_sees_carol={} \
         c_sees_alice={} c_sees_bob={}",
        sees_online_master(&a, &share, "bob"),
        sees_online_master(&a, &share, "carol"),
        sees_online_master(&b, &share, "alice"),
        sees_online_master(&b, &share, "carol"),
        sees_online_master(&c, &share, "alice"),
        sees_online_master(&c, &share, "bob"),
    );

    a.shutdown().await?;
    b.shutdown().await?;
    c.shutdown().await?;
    println!("3-master all-to-all roster OK");
    Ok(())
}
