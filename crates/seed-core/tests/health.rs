//! Long-term peer-health detection end-to-end (in-process engines, real iroh):
//! degraded members alerting the right observers (self + masters only), the
//! renotify cadence, recovery announcements, master-majority attribution, and
//! episode persistence across an observer restart. Thresholds run in seconds
//! via `Engine::set_health_policy` — production is the same machine at hours.
//!
//! Degrade mechanism: pausing a member freezes its reconcile (health/fp stop
//! moving) while its presence keeps broadcasting the stale values — verified
//! deterministic and instantly reversible, unlike fault injection.
//!
//! `#[ignore]` because every test opens real iroh endpoints; run serially:
//!   cargo test -p seed-core --test health -- --ignored --nocapture --test-threads 1

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::{cluster, Cluster};
use seed_core::engine::PeerHealthAlert;
use seed_core::health::HealthPolicy;

fn fast_policy() -> HealthPolicy {
    HealthPolicy {
        unhealthy_after_secs: 6,
        renotify_secs: 5,
        offline_reset_secs: 3600,
    }
}

/// Tick the cluster, collecting each node's due alerts into `sink[node]`.
async fn tick_collect(c: &mut Cluster, sink: &mut [Vec<PeerHealthAlert>]) {
    c.tick().await;
    for (i, bucket) in sink.iter_mut().enumerate() {
        bucket.extend(c.nodes[i].engine.health_alerts());
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
}

/// Drive + collect until `pred(sink)` holds or time out.
async fn drive_alerts<F>(
    c: &mut Cluster,
    sink: &mut Vec<Vec<PeerHealthAlert>>,
    timeout: Duration,
    label: &str,
    mut pred: F,
) -> anyhow::Result<()>
where
    F: FnMut(&[Vec<PeerHealthAlert>]) -> bool,
{
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        tick_collect(c, sink).await;
        if pred(sink) {
            return Ok(());
        }
    }
    anyhow::bail!("timed out waiting for: {label}")
}

/// A frozen (paused) viewer whose fingerprint falls behind the two masters'
/// consensus must raise alerts on BOTH masters (and only there), repeat on the
/// renotify cadence, and announce recovery everywhere it alerted once resumed.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn degraded_viewer_alerts_masters_then_recovers() -> anyhow::Result<()> {
    let mut c = cluster(2, 2).await?;

    // Converge on the empty share first (equal fingerprints), with production
    // thresholds still in force so setup can't trip an alert.
    let want = BTreeMap::new();
    c.drive_until(Duration::from_secs(90), "empty convergence", |c| {
        c.fps_agree()
    })
    .await?;

    for n in &mut c.nodes {
        n.engine.set_health_policy(fast_policy());
    }
    // Freeze viewer 0, then move the share under it.
    let share_id = c.share_id.clone();
    c.nodes[2].engine.set_paused(&share_id, true)?;
    std::fs::write(c.nodes[0].folder().join("advance.txt"), b"v0 misses this")?;

    let mut sink: Vec<Vec<PeerHealthAlert>> = vec![Vec::new(); 4];
    drive_alerts(
        &mut c,
        &mut sink,
        Duration::from_secs(60),
        "both masters alert about the frozen viewer",
        |s| {
            s[0].iter().any(|a| !a.is_self && !a.recovered)
                && s[1].iter().any(|a| !a.is_self && !a.recovered)
        },
    )
    .await?;
    let first = sink[0].iter().find(|a| !a.is_self).unwrap();
    assert!(first.unhealthy_secs >= 6, "threshold respected");
    assert!(
        sink[3].is_empty(),
        "the healthy viewer must never alert about a peer (masters only)"
    );
    assert!(
        sink[2].is_empty(),
        "a user-paused member doesn't nag its own operator"
    );
    println!("both masters alerted about the frozen viewer OK");

    // Renotify cadence: a second alert for the same episode.
    let m0_alerts = sink[0]
        .iter()
        .filter(|a| !a.is_self && !a.recovered)
        .count();
    drive_alerts(
        &mut c,
        &mut sink,
        Duration::from_secs(30),
        "renotify fires",
        |s| s[0].iter().filter(|a| !a.is_self && !a.recovered).count() > m0_alerts,
    )
    .await?;
    println!("renotify cadence OK");

    // Resume → converge → recovery announced on both masters.
    c.nodes[2].engine.set_paused(&share_id, false)?;
    let want2 = {
        let mut w = want.clone();
        w.insert("advance.txt".to_string(), b"v0 misses this".to_vec());
        w
    };
    drive_alerts(
        &mut c,
        &mut sink,
        Duration::from_secs(90),
        "recovery alerts on both masters",
        |s| {
            s[0].iter().any(|a| !a.is_self && a.recovered)
                && s[1].iter().any(|a| !a.is_self && a.recovered)
        },
    )
    .await?;
    assert!(c.folders_match(&want2), "share converged after resume");
    println!("recovery announced on both masters OK");
    c.shutdown().await?;
    Ok(())
}

/// 2-of-3 master-majority attribution: with masters A+B on the new fingerprint
/// and master C frozen on the old one, A and B must blame exactly C. The
/// paused C stays silent (user-paused = no self-nag).
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn minority_master_attributed_by_majority() -> anyhow::Result<()> {
    let mut c = cluster(3, 0).await?;
    std::fs::write(c.nodes[0].folder().join("base.txt"), b"agreed")?;
    let mut want = BTreeMap::new();
    want.insert("base.txt".to_string(), b"agreed".to_vec());
    c.drive_until(Duration::from_secs(90), "baseline convergence", |c| {
        c.converged(&want)
    })
    .await?;

    for n in &mut c.nodes {
        n.engine.set_health_policy(fast_policy());
    }
    let share_id = c.share_id.clone();
    c.nodes[2].engine.set_paused(&share_id, true)?;
    std::fs::write(c.nodes[0].folder().join("moved-on.txt"), b"c is behind")?;

    let mut sink: Vec<Vec<PeerHealthAlert>> = vec![Vec::new(); 3];
    drive_alerts(
        &mut c,
        &mut sink,
        Duration::from_secs(60),
        "majority masters attribute the minority master",
        |s| {
            s[0].iter().any(|a| !a.is_self && !a.recovered)
                && s[1].iter().any(|a| !a.is_self && !a.recovered)
        },
    )
    .await?;
    // Exactly one distinct peer blamed by each, and never each other: the two
    // majority masters share the consensus fp, so only C can be degraded.
    for observer in 0..2 {
        let blamed: std::collections::HashSet<&str> = sink[observer]
            .iter()
            .filter(|a| !a.is_self)
            .map(|a| a.node_id.as_str())
            .collect();
        assert_eq!(
            blamed.len(),
            1,
            "master {observer} must blame exactly one member (the frozen C)"
        );
    }
    assert!(sink[2].is_empty(), "paused C stays silent");
    println!("2-of-3 attribution OK");

    c.nodes[2].engine.set_paused(&share_id, false)?;
    want.insert("moved-on.txt".to_string(), b"c is behind".to_vec());
    drive_alerts(
        &mut c,
        &mut sink,
        Duration::from_secs(90),
        "recovery after resume",
        |s| {
            s[0].iter().any(|a| !a.is_self && a.recovered)
                && s[1].iter().any(|a| !a.is_self && a.recovered)
        },
    )
    .await?;
    println!("recovery attribution OK");
    c.shutdown().await?;
    Ok(())
}

/// 1-vs-1 master split: no strict majority → NEITHER master may blame the
/// other (misattribution is worse than none); instead the online master
/// self-alerts once its share turns OutOfSync past the settle window.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored (slow: waits out the 45s settle window)"]
async fn split_masters_self_alert_without_blame() -> anyhow::Result<()> {
    let mut c = cluster(2, 0).await?;
    std::fs::write(c.nodes[0].folder().join("base.txt"), b"agreed")?;
    let mut want = BTreeMap::new();
    want.insert("base.txt".to_string(), b"agreed".to_vec());
    c.drive_until(Duration::from_secs(90), "baseline convergence", |c| {
        c.converged(&want)
    })
    .await?;

    for n in &mut c.nodes {
        n.engine.set_health_policy(fast_policy());
    }
    let share_id = c.share_id.clone();
    c.nodes[1].engine.set_paused(&share_id, true)?;
    std::fs::write(c.nodes[0].folder().join("split.txt"), b"fork")?;

    // The OutOfSync settle window (45s) plus the 6s policy threshold.
    let mut sink: Vec<Vec<PeerHealthAlert>> = vec![Vec::new(); 2];
    drive_alerts(
        &mut c,
        &mut sink,
        Duration::from_secs(120),
        "online master self-alerts",
        |s| s[0].iter().any(|a| a.is_self && !a.recovered),
    )
    .await?;
    assert!(
        !sink[0].iter().any(|a| !a.is_self),
        "1-vs-1 fingerprint split must not blame the other master"
    );
    assert!(
        !sink[1].iter().any(|a| !a.is_self),
        "the paused master must not blame anyone either"
    );
    println!("split masters: self-alert without attribution OK");
    c.shutdown().await?;
    Ok(())
}

/// An open episode must survive a restart of the OBSERVING master: the accrued
/// time reloads from sqlite, `peers()` reports it immediately, and the renotify
/// cadence continues instead of a fresh 12h clock starting.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn episode_survives_observer_restart() -> anyhow::Result<()> {
    let mut c = cluster(1, 1).await?;
    c.drive_until(Duration::from_secs(90), "empty convergence", |c| {
        c.fps_agree()
    })
    .await?;
    for n in &mut c.nodes {
        n.engine.set_health_policy(fast_policy());
    }
    let share_id = c.share_id.clone();
    c.nodes[1].engine.set_paused(&share_id, true)?;
    std::fs::write(c.nodes[0].folder().join("go.txt"), b"advance")?;

    let mut sink: Vec<Vec<PeerHealthAlert>> = vec![Vec::new(); 2];
    drive_alerts(
        &mut c,
        &mut sink,
        Duration::from_secs(60),
        "first alert before the restart",
        |s| s[0].iter().any(|a| !a.is_self && !a.recovered),
    )
    .await?;

    // Restart the observer on the same data dir (drop reconstructs everything
    // from sqlite + gossip). The episode must come back with time on the clock.
    // Take node 0 out, shut its engine down FIRST (reopening the same data dir
    // while the old engine still holds the stores contends on sqlite/blob
    // locks), then reopen on the same tempdirs. Both steps bounded so a wedged
    // iroh close or a flaky relay can't hang the test forever.
    let old = c.nodes.remove(0);
    let _ = tokio::time::timeout(Duration::from_secs(20), old.engine.shutdown()).await;
    let engine = seed_core::Engine::new(old.data.path()).await?;
    c.nodes.insert(
        0,
        common::Node {
            engine,
            data: old.data,
            folder: old.folder,
        },
    );
    c.nodes[0].engine.set_health_policy(fast_policy());
    tokio::time::timeout(Duration::from_secs(30), c.nodes[0].engine.wait_online())
        .await
        .map_err(|_| anyhow::anyhow!("restarted engine never came online"))?;

    // After presence re-establishes, the viewer's episode is already aged.
    c.drive_until(Duration::from_secs(60), "roster re-established", |c| {
        c.nodes[0]
            .engine
            .peers(&c.share_id)
            .map(|ps| ps.iter().any(|p| p.node_id != "This device" && p.online))
            .unwrap_or(false)
    })
    .await?;
    let peers = c.nodes[0].engine.peers(&share_id)?;
    let viewer = peers
        .iter()
        .find(|p| p.node_id != "This device")
        .expect("viewer visible again");
    assert!(
        viewer.unhealthy_secs >= 4,
        "episode reloaded from sqlite with accrued time (got {}s)",
        viewer.unhealthy_secs
    );

    // And the cadence continues: another alert without a fresh threshold wait.
    let mut sink: Vec<Vec<PeerHealthAlert>> = vec![Vec::new(); 2];
    drive_alerts(
        &mut c,
        &mut sink,
        Duration::from_secs(30),
        "renotify continues after restart",
        |s| s[0].iter().any(|a| !a.is_self && !a.recovered),
    )
    .await?;
    println!("episode survived the observer restart OK");
    c.shutdown().await?;
    Ok(())
}
