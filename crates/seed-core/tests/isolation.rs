//! A node that can reach no member of a share must say so — and a node that is
//! merely *alone* must not.
//!
//! This is the regression test for known-issues #17, and by extension for how #16
//! stayed hidden. Every peer comparison the engine makes (the health percent, the
//! convergence check, the consensus fingerprint) filters on *online* peers, and all
//! of them are vacuously true of an empty set — so a node partitioned from its whole
//! pool agreed with everyone it could hear (nobody), held everything it knew about
//! (nothing), and reported `Healthy 100%`. A live share sat like that for over a
//! week without a single screen showing anything wrong.
//!
//! The fix must distinguish two states that both have zero reachable peers, which is
//! exactly what makes it more subtle than "warn when the peer list is empty":
//!
//! 1. a share this device **created** that nobody has joined yet — genuinely alone,
//!    and legitimately `Healthy`; and
//! 2. a share this device **joined** but can reach nobody in — a partition.
//!
//! Getting (1) wrong would cry wolf on every newly-created share, which is precisely
//! the kind of false alarm that trains people to ignore the real one.
//!
//! `#[ignore]` like the other engine tests: opens real iroh endpoints. Run with:
//!   cargo test -p seed-core --test isolation -- --ignored

mod common;

use seed_core::identity::ShareKey;
use seed_core::Engine;

/// The status string the daemon would serve the GUI/CLI for a share.
fn status(engine: &Engine, share_id: &str) -> String {
    engine
        .list_summaries()
        .into_iter()
        .find(|s| s.share_id == share_id)
        .map(|s| format!("{:?}", s.status))
        .unwrap_or_else(|| "<missing>".into())
}

/// A share you just created, that nobody has joined, is alone — not partitioned.
/// It must keep reading `Healthy`.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn lone_creator_is_healthy_not_stranded() -> anyhow::Result<()> {
    let data = tempfile::tempdir()?;
    let folder = tempfile::tempdir()?;
    std::fs::write(folder.path().join("a.txt"), b"hello")?;

    let mut engine = Engine::new(data.path()).await?;
    let created = engine.create_share(folder.path(), vec![]).await?;

    let _seed = common::SecretGuard::new(&created.share_id);
    assert_eq!(
        status(&engine, &created.share_id),
        "Healthy",
        "a share nobody has joined yet is alone, not partitioned — flagging it would \
         cry wolf on every freshly-created share"
    );
    Ok(())
}

/// A share added from a key whose creator does not exist reaches nobody. That is the
/// exact shape of known-issues #16 — and before #17 it reported `Healthy 100%`.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn joiner_that_reaches_nobody_reports_no_peers() -> anyhow::Result<()> {
    let data = tempfile::tempdir()?;
    let folder = tempfile::tempdir()?;

    // A key minted by a device that isn't there: the endpoint id baked into it
    // belongs to no running node, so the *entire* bootstrap set is a dead address —
    // which is precisely what a real joiner faces whenever the creating device
    // happens to be offline.
    let ghost = ShareKey::generate_master().with_endpoint_id(rand::random::<[u8; 32]>());

    let mut engine = Engine::new(data.path()).await?;
    let share_id = engine
        .add_share(&ghost.encode(), folder.path(), vec![])
        .await?;

    // Reconcile first, and note that this is the whole point rather than setup
    // hygiene. A share whose health is still the provisional 0 lands on `Syncing`
    // anyway, so asserting before this line would pass without the fix and prove
    // nothing. Once a *master* with nothing left to fetch reconciles, its health is
    // 100 — and 100 plus "no online peer disagrees with me" (vacuously true, there
    // being no online peers) is exactly the pair of conditions that produced
    // `Healthy 100%` on a node talking to no one. This asserts that being partitioned
    // now outranks that.
    engine.reconcile(&share_id).await?;

    let st = status(&engine, &share_id);
    assert_ne!(
        st, "Healthy",
        "a fully-partitioned master reported Healthy — health computed over an empty \
         peer set (known-issues #17); it is 100% of nothing, not a clean bill of health"
    );
    assert_eq!(
        st, "NoPeers",
        "a joiner that can reach no member must say so; reporting anything else \
         (least of all Healthy) is what hid the cold-join bootstrap failure for a week"
    );
    Ok(())
}
