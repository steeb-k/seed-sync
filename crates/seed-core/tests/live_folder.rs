//! The share folder is **live while we sync it**.
//!
//! Every other suite changes the folder *between* reconcile passes, which is the
//! tidy case. Real use isn't tidy: a pass on a multi-GB share runs for a minute or
//! more (field logs: 80 s+), and the user drops an ISO in, saves a document, or
//! deletes something right through the middle of it. This suite covers writes that
//! land **during** a pass.
//!
//! That window is what known-issues #30 was: the pass finished by re-walking the
//! folder and recording *that* as "settled", absorbing changes it had never
//! scanned. The next pass saw a signature matching its baseline, skipped the full
//! scan, and the change never propagated — silently, with every member still
//! reporting `Healthy 100%`, until an unrelated edit moved the signature or the
//! 4-hourly deep verify forced a rescan.
//!
//! The mid-pass write is delivered through `ReconcileJob::debug_before_settle`
//! rather than by racing a background thread against the scan, so these are
//! deterministic rather than timing-dependent.
//!
//! `#[ignore]` because every test opens real iroh endpoints; run serially:
//!   cargo test -p seed-core --test live_folder -- --ignored --nocapture --test-threads 1

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use common::{cluster, gen_bytes, snapshot, Cluster};

fn content_for(tag: &str, size: usize) -> Vec<u8> {
    let mut v = tag.as_bytes().to_vec();
    v.extend(gen_bytes(size.saturating_sub(v.len())));
    v.truncate(size);
    v
}

/// Run exactly one reconcile pass on `node`, performing `mutate` inside it — after
/// the merge has read the folder, before the pass records the folder as settled.
/// That is precisely where a user's save lands during a long pass.
async fn pass_with_midpass_write<F>(c: &mut Cluster, node: usize, mutate: F) -> anyhow::Result<()>
where
    F: Fn() + Send + Sync + 'static,
{
    let id = c.share_id.clone();
    let job = c.nodes[node]
        .engine
        .make_reconcile_job(&id)?
        .expect("share should be reconcilable");
    let outcome = job.debug_before_settle(mutate).run().await?;
    c.nodes[node].engine.finish_reconcile(&id, Some(outcome));
    Ok(())
}

/// Assert the health number is not lying: any node claiming 100% must actually
/// hold, on disk, the fileset its own manifest describes. Two nodes that both
/// claim 100% *and* agree on a fingerprint therefore have byte-identical folders.
///
/// This is the check that was missing when the field report came in — a peer sat on
/// a two-day-old file and reported `Healthy 100%` the whole time, because health
/// only ever asked the blob store whether it had the content, never the filesystem
/// whether it had been written.
fn assert_healthy_nodes_agree(c: &Cluster) {
    let full: Vec<usize> = (0..c.nodes.len())
        .filter(|i| {
            c.nodes[*i]
                .engine
                .list_summaries()
                .into_iter()
                .find(|s| s.share_id == c.share_id)
                .map(|s| s.percent >= 100)
                .unwrap_or(false)
        })
        .collect();
    // Only nodes that also agree on the manifest are comparable: one at 100% on an
    // older manifest is behind, not lying.
    let fp0 = match full.first() {
        Some(i) => c.self_fp(*i),
        None => return,
    };
    let agreeing: Vec<usize> = full.into_iter().filter(|i| c.self_fp(*i) == fp0).collect();
    if agreeing.len() < 2 || fp0 == 0 {
        return;
    }
    let base = snapshot(c.nodes[agreeing[0]].folder());
    for i in &agreeing[1..] {
        let other = snapshot(c.nodes[*i].folder());
        assert_eq!(
            base, other,
            "nodes {} and {i} both report Healthy 100% on the same manifest \
             (fp={fp0:016x}) but hold different folders — health is lying",
            agreeing[0],
        );
    }
}

fn write(root: &Path, rel: &str, bytes: &[u8]) {
    let abs = root.join(rel);
    if let Some(p) = abs.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(&abs, bytes).unwrap();
}

/// **The reported bug.** A master is mid-pass when the user overwrites a file in
/// place (drops a new ISO over the old one). The overwrite must still reach every
/// member — on the next tick, not four hours later at the periodic deep verify.
///
/// Before the fix this hung: the pass absorbed the new file's size+mtime into its
/// settled signature without ever hashing it, so every later pass compared equal,
/// skipped the scan, and the master went permanently blind to the new content.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn overwrite_during_a_pass_still_propagates() -> anyhow::Result<()> {
    let mut c = cluster(1, 1).await?;

    let v1 = content_for("iso-v1", 512 * 1024);
    write(c.nodes[0].folder(), "big.iso", &v1);
    let mut want = BTreeMap::new();
    want.insert("big.iso".to_string(), v1);
    c.drive_until(Duration::from_secs(120), "seed v1", |c| c.converged(&want))
        .await?;
    assert_healthy_nodes_agree(&c);

    // The overwrite lands *inside* the master's pass.
    let v2 = content_for("iso-v2", 640 * 1024);
    let target = c.nodes[0].folder().join("big.iso");
    let v2_for_hook = v2.clone();
    pass_with_midpass_write(&mut c, 0, move || {
        std::fs::write(&target, &v2_for_hook).unwrap();
    })
    .await?;
    want.insert("big.iso".to_string(), v2);

    // From here it is only ordinary ticks — no forced deep verify, no other edit to
    // nudge the signature. The overwrite has to propagate on its own.
    c.drive_until(
        Duration::from_secs(120),
        "mid-pass overwrite reaches the peer",
        |c| c.converged(&want),
    )
    .await?;
    assert_healthy_nodes_agree(&c);

    c.shutdown().await?;
    Ok(())
}

/// Same window, same-size content. Size is the more visible half of the change
/// signature, so an overwrite that only moves the mtime is the tighter case — and
/// the one a document save (edit, re-save, same length) actually produces.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn same_size_overwrite_during_a_pass_still_propagates() -> anyhow::Result<()> {
    let mut c = cluster(1, 1).await?;

    let v1 = content_for("doc-v1", 200 * 1024);
    write(c.nodes[0].folder(), "notes.doc", &v1);
    let mut want = BTreeMap::new();
    want.insert("notes.doc".to_string(), v1.clone());
    c.drive_until(Duration::from_secs(120), "seed v1", |c| c.converged(&want))
        .await?;

    let v2 = content_for("doc-v2", v1.len());
    assert_eq!(v1.len(), v2.len(), "this test is about a same-size rewrite");
    let target = c.nodes[0].folder().join("notes.doc");
    let v2_for_hook = v2.clone();
    pass_with_midpass_write(&mut c, 0, move || {
        std::fs::write(&target, &v2_for_hook).unwrap();
    })
    .await?;
    want.insert("notes.doc".to_string(), v2);

    c.drive_until(
        Duration::from_secs(120),
        "mid-pass same-size overwrite reaches the peer",
        |c| c.converged(&want),
    )
    .await?;
    assert_healthy_nodes_agree(&c);

    c.shutdown().await?;
    Ok(())
}

/// A file dropped into the folder *during* a pass was never scanned by it. It must
/// not be absorbed into the settled signature as though it had been.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn create_during_a_pass_still_propagates() -> anyhow::Result<()> {
    let mut c = cluster(1, 1).await?;

    let seed = content_for("seed", 64 * 1024);
    write(c.nodes[0].folder(), "existing.bin", &seed);
    let mut want = BTreeMap::new();
    want.insert("existing.bin".to_string(), seed);
    c.drive_until(Duration::from_secs(120), "seed", |c| c.converged(&want))
        .await?;

    let fresh = content_for("dropped-in", 128 * 1024);
    let target = c.nodes[0].folder().join("dropped-in.bin");
    let fresh_for_hook = fresh.clone();
    pass_with_midpass_write(&mut c, 0, move || {
        std::fs::write(&target, &fresh_for_hook).unwrap();
    })
    .await?;
    want.insert("dropped-in.bin".to_string(), fresh);

    c.drive_until(
        Duration::from_secs(120),
        "mid-pass create reaches the peer",
        |c| c.converged(&want),
    )
    .await?;
    assert_healthy_nodes_agree(&c);

    c.shutdown().await?;
    Ok(())
}

/// A delete during a pass leaves nothing behind to exclude from the signature, so
/// it is the case that needs the drift tag rather than path exclusion. Without it
/// the next walk agrees with the recorded value and the deletion never reaches
/// anyone.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn delete_during_a_pass_still_propagates() -> anyhow::Result<()> {
    let mut c = cluster(1, 1).await?;

    let keep = content_for("keep", 32 * 1024);
    let doomed = content_for("doomed", 48 * 1024);
    write(c.nodes[0].folder(), "keep.bin", &keep);
    write(c.nodes[0].folder(), "doomed.bin", &doomed);
    let mut want = BTreeMap::new();
    want.insert("keep.bin".to_string(), keep);
    want.insert("doomed.bin".to_string(), doomed);
    c.drive_until(Duration::from_secs(120), "seed both", |c| {
        c.converged(&want)
    })
    .await?;

    let target = c.nodes[0].folder().join("doomed.bin");
    pass_with_midpass_write(&mut c, 0, move || {
        std::fs::remove_file(&target).unwrap();
    })
    .await?;
    want.remove("doomed.bin");

    c.drive_until(
        Duration::from_secs(120),
        "mid-pass delete reaches the peer",
        |c| c.converged(&want),
    )
    .await?;
    assert_healthy_nodes_agree(&c);

    c.shutdown().await?;
    Ok(())
}

/// Copying a large file over an existing one isn't one write, it's a long stream of
/// them, and passes keep firing throughout. Every intermediate state is a legal
/// thing to have scanned; the requirement is only that the fleet ends on the
/// *final* bytes and nobody is left claiming health over a half-written file.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn a_file_rewritten_across_several_passes_lands_on_the_last_version() -> anyhow::Result<()> {
    let mut c = cluster(1, 1).await?;

    let v0 = content_for("v0", 96 * 1024);
    write(c.nodes[0].folder(), "grow.bin", &v0);
    let mut want = BTreeMap::new();
    want.insert("grow.bin".to_string(), v0);
    c.drive_until(Duration::from_secs(120), "seed v0", |c| c.converged(&want))
        .await?;

    // Four consecutive passes, each overwritten mid-flight — a copy in progress.
    let mut last = Vec::new();
    for step in 1..=4 {
        let bytes = content_for(&format!("v{step}"), 96 * 1024 + step * 40 * 1024);
        let target = c.nodes[0].folder().join("grow.bin");
        let for_hook = bytes.clone();
        pass_with_midpass_write(&mut c, 0, move || {
            std::fs::write(&target, &for_hook).unwrap();
        })
        .await?;
        last = bytes;
    }
    want.insert("grow.bin".to_string(), last);

    c.drive_until(
        Duration::from_secs(180),
        "fleet lands on the final version",
        |c| c.converged(&want),
    )
    .await?;
    assert_healthy_nodes_agree(&c);

    c.shutdown().await?;
    Ok(())
}

/// Co-masters, mid-pass write on one of them. The multi-writer path has its own
/// merge branches (three-way against the base index, LWW when both sides moved),
/// so the drift guard has to hold there too and not just on a single publisher.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn midpass_overwrite_propagates_between_co_masters() -> anyhow::Result<()> {
    let mut c = cluster(2, 0).await?;

    let v1 = content_for("shared-v1", 256 * 1024);
    write(c.nodes[0].folder(), "shared.bin", &v1);
    let mut want = BTreeMap::new();
    want.insert("shared.bin".to_string(), v1);
    c.drive_until(Duration::from_secs(120), "seed v1", |c| c.converged(&want))
        .await?;

    // Master 1 (not the creator) is the one interrupted mid-pass.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let v2 = content_for("shared-v2", 300 * 1024);
    let target = c.nodes[1].folder().join("shared.bin");
    let v2_for_hook = v2.clone();
    pass_with_midpass_write(&mut c, 1, move || {
        std::fs::write(&target, &v2_for_hook).unwrap();
    })
    .await?;
    want.insert("shared.bin".to_string(), v2);

    c.drive_until(
        Duration::from_secs(120),
        "co-master mid-pass overwrite converges",
        |c| c.converged(&want),
    )
    .await?;
    assert_healthy_nodes_agree(&c);

    c.shutdown().await?;
    Ok(())
}
