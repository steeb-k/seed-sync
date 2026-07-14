//! The share-key rendezvous, end to end against the **real** pkarr server.
//!
//! This is the load-bearing half of the known-issues #16 fix, and the half that
//! cannot be proven by reasoning about our own code: it depends on a third-party
//! service accepting a packet signed by a key that is not any iroh node's device key,
//! and handing it back to a caller who knows only that key's public half. If that
//! round trip doesn't work, a joiner whose creator is offline still has nowhere to
//! dial and #16 is not actually fixed — so it gets a test that really does talk to
//! `dns.iroh.link`.
//!
//! `#[ignore]` twice over: it opens real iroh endpoints *and* requires internet.
//!   cargo test -p seed-core --test rendezvous -- --ignored --nocapture

use seed_core::identity::ShareKey;
use seed_core::{rendezvous, Engine};

/// A master publishes its address under the share key; a second, entirely separate
/// node — holding nothing but the share's *public* key, exactly what a cold joiner
/// has — resolves it and gets back a dialable address for that master.
#[tokio::test]
#[ignore = "opens real iroh endpoints and requires internet; run with --ignored"]
async fn master_publishes_and_a_cold_joiner_resolves_it() -> anyhow::Result<()> {
    let a_data = tempfile::tempdir()?;
    let b_data = tempfile::tempdir()?;

    let master = Engine::new(a_data.path()).await?;
    let joiner = Engine::new(b_data.path()).await?;

    // Wait for a relay home / direct addresses — publishing before the endpoint has
    // any address would (correctly) refuse, since an address-less record is not
    // dialable by anyone.
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while master.endpoint_addr().addrs.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("master endpoint never got a dialable address"))?;

    // A fresh share key. The seed is the master's to sign with; the public half is
    // all a joiner (master or viewer) ever has.
    let key = ShareKey::generate_master();
    let seed = key.seed_bytes().expect("a master key carries the seed");
    let master_pub = key.master_pub_bytes();

    rendezvous::publish(master.endpoint(), seed).await?;

    // The joiner resolves by share pubkey alone — no ticket, no bootstrap, no
    // knowledge of who created the share or whether that device still exists.
    let found = rendezvous::resolve(joiner.endpoint(), master_pub).await?;

    assert_eq!(
        found.id,
        master.endpoint_addr().id,
        "the rendezvous must resolve to the master that published, not to the share \
         key itself — the endpoint id rides in the record's user_data precisely \
         because the packet is *named* by a key that is nobody's node"
    );
    assert!(
        !found.addrs.is_empty(),
        "a resolved rendezvous address with no transport addresses is not dialable, \
         which would leave a cold joiner exactly as stranded as before"
    );
    Ok(())
}

/// A **viewer** can resolve the rendezvous. This is not incidental: viewers hold only
/// `master_pub`, so if resolution needed the seed, read-only members could never cold
/// join at all — and the whole scheme would just move #16 rather than fix it.
#[tokio::test]
#[ignore = "opens real iroh endpoints and requires internet; run with --ignored"]
async fn a_viewer_key_is_enough_to_resolve() -> anyhow::Result<()> {
    let a_data = tempfile::tempdir()?;
    let b_data = tempfile::tempdir()?;

    let master = Engine::new(a_data.path()).await?;
    let viewer = Engine::new(b_data.path()).await?;

    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while master.endpoint_addr().addrs.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("master endpoint never got a dialable address"))?;

    let key = ShareKey::generate_master();
    rendezvous::publish(master.endpoint(), key.seed_bytes().unwrap()).await?;

    // Decode the *viewer* key — the seed is not in it — and resolve with that.
    let viewer_key = ShareKey::decode(&key.encode_viewer())?;
    assert!(
        viewer_key.seed_bytes().is_none(),
        "a viewer key must not carry the master seed; if it did, viewers could \
         publish (and clobber) rendezvous records"
    );

    let found = rendezvous::resolve(viewer.endpoint(), viewer_key.master_pub_bytes()).await?;
    assert_eq!(found.id, master.endpoint_addr().id);
    Ok(())
}

/// **Known-issues #16, reproduced and fixed.**
///
/// A joiner is handed a key whose baked-in creator endpoint id is dead — the exact
/// situation of a real joiner whenever the creating device is offline, which is what
/// stranded the ARM64 box. Its entire bootstrap set is therefore a dead address: no
/// doc sync, no gossip, no member registry, no `peer_names` (it has never synced, so
/// there is nothing remembered). Every repair path in the engine is downstream of a
/// first contact that cannot happen.
///
/// A *different*, live master then publishes the rendezvous — and the joiner reaches
/// it and syncs. Before this fix there was no path from the left-hand state to the
/// right-hand one, no matter how many other masters were up.
#[tokio::test]
#[ignore = "opens real iroh endpoints and requires internet; run with --ignored"]
async fn joiner_with_a_dead_creator_still_syncs_via_rendezvous() -> anyhow::Result<()> {
    let a_data = tempfile::tempdir()?;
    let b_data = tempfile::tempdir()?;
    let a_folder = tempfile::tempdir()?;
    let b_folder = tempfile::tempdir()?;
    std::fs::write(a_folder.path().join("hello.txt"), b"from the live master")?;

    let mut live_master = Engine::new(a_data.path()).await?;
    let mut joiner = Engine::new(b_data.path()).await?;

    let created = live_master.create_share(a_folder.path(), vec![]).await?;

    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while live_master.endpoint_addr().addrs.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("master endpoint never got a dialable address"))?;

    // Same share (same master keypair, so the same namespace), but the key we hand the
    // joiner names a creator that does not exist. Nothing else about it is unusual —
    // this is precisely what a key minted by a since-retired device looks like.
    let real_key = ShareKey::decode(&created.master_key)?;
    let dead_creator_key = real_key.with_endpoint_id(rand::random::<[u8; 32]>());

    let share_id = joiner
        .add_share(&dead_creator_key.encode(), b_folder.path(), vec![])
        .await?;
    assert_eq!(share_id, created.share_id, "must be the same share");

    // The bug: nowhere to dial, and every screen would once have called this Healthy.
    joiner.reconcile(&share_id).await?;
    let stranded = joiner
        .list_summaries()
        .into_iter()
        .find(|s| s.share_id == share_id)
        .map(|s| format!("{:?}", s.status));
    assert_eq!(
        stranded.as_deref(),
        Some("NoPeers"),
        "with a dead creator and nothing remembered, the joiner must be — and must \
         report itself — cut off from the share"
    );

    // The fix: any live master advertises itself under the share key, and the joiner
    // finds it with nothing but the key it already holds.
    for job in live_master.rendezvous_publishes() {
        job.run().await;
    }
    for job in joiner.rendezvous_dials() {
        job.run().await;
    }

    // ...and now it actually syncs the content, which is the only proof that matters.
    let want = std::fs::read(a_folder.path().join("hello.txt"))?;
    let synced = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            let _ = live_master.reconcile(&share_id).await;
            let _ = joiner.reconcile(&share_id).await;
            if std::fs::read(b_folder.path().join("hello.txt"))
                .ok()
                .as_ref()
                == Some(&want)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    })
    .await;
    assert!(
        synced.is_ok(),
        "the joiner never synced: the rendezvous did not rescue a cold join whose \
         creator was offline, which is the entire point of known-issues #16"
    );
    Ok(())
}
