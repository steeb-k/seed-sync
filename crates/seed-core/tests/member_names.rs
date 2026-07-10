//! Member-name persistence: the member list keeps naming members that are
//! offline — or that this node has NEVER heard directly — via the doc member
//! registry (`\x00m/` records written by masters) and the local `peer_names`
//! cache that survives engine restarts.
//!
//! `#[ignore]` (opens real iroh endpoints); run with `-- --ignored`.

use std::time::Duration;

use seed_core::Engine;

/// The remote (not "This device") peer named `name`, as
/// `(online, role_debug, last_seen)`. Role is `seed_ipc::Role`, which the test
/// crate doesn't depend on by name, so match it via Debug ("Master"/"Viewer").
fn remote_peer(engine: &Engine, share_id: &str, name: &str) -> Option<(bool, String, i64)> {
    engine
        .peers(share_id)
        .ok()?
        .into_iter()
        .find(|p| p.node_id != "This device" && p.name.as_deref() == Some(name))
        .map(|p| (p.online, format!("{:?}", p.role), p.last_seen))
}

/// End to end across three nodes:
///  1. alice (master) and bob (viewer) sync; alice hears bob via presence and
///     publishes both identities into the doc member registry.
///  2. alice goes OFFLINE. carol joins fresh, bootstrapping only from bob — she
///     has never heard alice — yet her member list names "alice" (offline,
///     Master) purely from the replicated registry.
///  3. carol restarts with EVERYONE offline and still lists both members by
///     name, from the `peer_names` cache preloaded into the roster.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn member_names_survive_offline_masters_and_restarts() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let a_data = tmp.path().join("a-data");
    let b_data = tmp.path().join("b-data");
    let c_data = tmp.path().join("c-data");
    let a_folder = tmp.path().join("a-folder");
    let b_folder = tmp.path().join("b-folder");
    let c_folder = tmp.path().join("c-folder");
    for d in [&a_data, &b_data, &c_data, &a_folder, &b_folder, &c_folder] {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(a_folder.join("a.txt"), b"hello")?;

    let mut alice = Engine::new(&a_data).await?;
    let mut bob = Engine::new(&b_data).await?;
    tokio::time::timeout(Duration::from_secs(20), async {
        alice.wait_online().await;
        bob.wait_online().await;
    })
    .await
    .map_err(|_| anyhow::anyhow!("endpoints did not come online"))?;
    alice.set_device_name("alice")?;
    bob.set_device_name("bob")?;

    let created = alice.create_share(&a_folder, vec![]).await?;
    let share = created.share_id.clone();
    let alice_addr = alice.endpoint_addr();
    let share_b = bob
        .add_share(&created.viewer_key, &b_folder, vec![alice_addr])
        .await?;
    assert_eq!(share, share_b);

    // Converge: presence both ways, reconcile passes on both (alice's passes
    // publish the member registry — her own record first, bob's once heard).
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            for j in alice.presence_broadcasts() {
                j.send().await;
            }
            for j in bob.presence_broadcasts() {
                j.send().await;
            }
            let _ = alice.apply(&share).await;
            let _ = bob.apply(&share).await;
            if remote_peer(&alice, &share, "bob").is_some()
                && remote_peer(&bob, &share, "alice").is_some()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("alice/bob presence did not converge"))?;

    // One more master pass now that bob is definitely in alice's roster, so his
    // registry record is certainly published and live-synced toward bob.
    alice.apply(&share).await?;
    let _ = bob.apply(&share).await;

    // The master vanishes.
    alice.shutdown().await?;

    // carol joins fresh, reachable only via bob — she can never hear alice.
    let mut carol = Engine::new(&c_data).await?;
    tokio::time::timeout(Duration::from_secs(20), carol.wait_online())
        .await
        .map_err(|_| anyhow::anyhow!("carol endpoint did not come online"))?;
    carol.set_device_name("carol")?;
    let bob_addr = bob.endpoint_addr();
    let share_c = carol
        .add_share(&created.viewer_key, &c_folder, vec![bob_addr])
        .await?;
    assert_eq!(share, share_c);

    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            for j in bob.presence_broadcasts() {
                j.send().await;
            }
            for j in carol.presence_broadcasts() {
                j.send().await;
            }
            let _ = bob.apply(&share).await;
            let _ = carol.apply(&share).await;
            if remote_peer(&carol, &share, "alice").is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "carol never learned alice's name from the doc registry; carol sees: {:?}",
            carol.peers(&share).map(|ps| ps
                .iter()
                .map(|p| (p.node_id.clone(), p.name.clone()))
                .collect::<Vec<_>>())
        )
    })?;

    let (online, role, last_seen) = remote_peer(&carol, &share, "alice").unwrap();
    assert!(
        !online,
        "alice is offline; the registry must not fake liveness"
    );
    assert_eq!(role, "Master");
    assert!(
        last_seen > 0,
        "registry records carry a last-seen timestamp"
    );

    // Give carol a presence tick so the flush persists her remembered names.
    for j in carol.presence_broadcasts() {
        j.send().await;
    }

    // Everyone down; carol restarts. The member list must name alice AND bob
    // immediately from the local peer_names cache — no network involved.
    bob.shutdown().await?;
    carol.shutdown().await?;
    let carol2 = Engine::new(&c_data).await?;
    let alice_row = remote_peer(&carol2, &share, "alice");
    let bob_row = remote_peer(&carol2, &share, "bob");
    assert!(
        alice_row.is_some() && bob_row.is_some(),
        "restarted carol must still name both members, got: {:?}",
        carol2.peers(&share).map(|ps| ps
            .iter()
            .map(|p| (p.node_id.clone(), p.name.clone()))
            .collect::<Vec<_>>())
    );
    assert_eq!(alice_row.unwrap().1, "Master");
    assert_eq!(bob_row.unwrap().1, "Viewer");

    carol2.shutdown().await?;
    println!("member-name registry + cache OK");
    Ok(())
}
