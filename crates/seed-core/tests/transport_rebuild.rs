//! The in-process transport rebuild (known-issues #36) must be a real
//! restart-equivalent: after `Engine::rebuild_transport` the node keeps its
//! endpoint id, reopens every share from the DB, reconnects to its members, and
//! keeps syncing in both directions — and a reconcile pass that was built before
//! the rebuild cannot commit into the recreated share.
//!
//! This is the mechanics test. The field fault it exists for (a days-old
//! endpoint that cannot dial a member a fresh endpoint reaches instantly) cannot
//! be reproduced on demand; what *can* be proven is that the remedy is safe to
//! fire, which is what lets the ladder fire it automatically.
//!
//! `#[ignore]` like the other engine tests: opens real iroh endpoints. Run with:
//!   cargo test -p seed-core --test transport_rebuild -- --ignored

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::{cluster, gen_bytes};

#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn rebuild_keeps_identity_and_resumes_sync_both_ways() -> anyhow::Result<()> {
    common::init_tracing();
    let mut c = cluster(2, 0).await?;

    // Baseline: the two masters converge on a first file.
    let mut want = BTreeMap::new();
    let first = gen_bytes(4096);
    std::fs::write(c.nodes[0].folder().join("before.bin"), &first)?;
    want.insert("before.bin".to_string(), first);
    c.drive_until(Duration::from_secs(90), "baseline convergence", |c| {
        c.converged(&want)
    })
    .await?;

    let id_before = c.nodes[1].engine.endpoint().id();
    let shares_before = c.nodes[1].engine.share_ids().len();

    // Rebuild node 1's whole iroh stack in place.
    let took = c.nodes[1].engine.rebuild_transport().await?;
    println!("rebuild took {:.2}s", took.as_secs_f32());
    assert_eq!(
        c.nodes[1].engine.endpoint().id(),
        id_before,
        "a rebuild must keep the endpoint id (same node.key), or every member's \
         roster, rendezvous record and share key would point at a ghost"
    );
    assert_eq!(
        c.nodes[1].engine.share_ids().len(),
        shares_before,
        "every share must be reopened from the DB"
    );
    assert_eq!(
        c.nodes[1].engine.transport_rebuilds(),
        0,
        "a manual rebuild is not counted as a ladder rebuild"
    );

    // Member visibility comes back: node 0 sees node 1 online again, and vice
    // versa, without either being restarted.
    c.drive_until(
        Duration::from_secs(90),
        "members online after rebuild",
        |c| {
            (0..2).all(|n| {
                c.nodes[n]
                    .engine
                    .peers(&c.share_id)
                    .map(|ps| ps.iter().skip(1).any(|p| p.online))
                    .unwrap_or(false)
            })
        },
    )
    .await?;

    // Sync resumes in BOTH directions across the rebuilt transport.
    let from_rebuilt = gen_bytes(8192);
    std::fs::write(c.nodes[1].folder().join("after-from-1.bin"), &from_rebuilt)?;
    want.insert("after-from-1.bin".to_string(), from_rebuilt);
    let from_other = gen_bytes(8192);
    std::fs::write(c.nodes[0].folder().join("after-from-0.bin"), &from_other)?;
    want.insert("after-from-0.bin".to_string(), from_other);
    c.drive_until(Duration::from_secs(120), "convergence after rebuild", |c| {
        c.converged(&want)
    })
    .await?;

    // A second rebuild right away must also be safe (the ladder may fire more
    // than once over a long outage).
    c.nodes[1].engine.rebuild_transport().await?;
    let again = gen_bytes(2048);
    std::fs::write(c.nodes[0].folder().join("after-second.bin"), &again)?;
    want.insert("after-second.bin".to_string(), again);
    c.drive_until(
        Duration::from_secs(120),
        "convergence after second rebuild",
        |c| c.converged(&want),
    )
    .await?;

    c.shutdown().await
}

/// A reconcile pass built before a rebuild belongs to the old generation: its
/// result must be dropped, not committed into the reopened share.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn stale_generation_pass_is_fenced_off() -> anyhow::Result<()> {
    common::init_tracing();
    let mut c = cluster(1, 0).await?;
    std::fs::write(c.nodes[0].folder().join("a.bin"), gen_bytes(1024))?;

    let job = c.nodes[0]
        .engine
        .make_reconcile_job(&c.share_id)?
        .expect("a job for the only share");
    let generation = job.generation();
    c.nodes[0].engine.rebuild_transport().await?;
    // The old job's handles point at the torn-down node; whatever it returns
    // (or however it fails) must not touch the fresh state.
    let outcome = job.run().await.ok();
    c.nodes[0]
        .engine
        .finish_reconcile(&c.share_id, generation, outcome);

    // The fresh state is not left "busy" by the stale finish: a new job can be
    // built immediately and the share still syncs its file.
    let fresh = c.nodes[0].engine.make_reconcile_job(&c.share_id)?;
    assert!(
        fresh.is_some(),
        "the stale pass must not have cleared or set the fresh share's busy guard"
    );
    drop(fresh);
    c.nodes[0]
        .engine
        .finish_reconcile(&c.share_id, generation + 1, None);
    c.shutdown().await
}
