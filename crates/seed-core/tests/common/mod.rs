//! Shared in-process multi-engine helpers for seed-core integration tests: an
//! N-node cluster builder plus the drive/assert loops every test re-implemented
//! before. Complements `seed-harness` (which owns corpus generation and
//! daemon-*process* drivers); this module owns what needs `seed_core::Engine`.
//!
//! Not every test uses every helper, and each `tests/*.rs` binary compiles its
//! own copy of this module — hence the file-wide `dead_code` allowance.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use seed_core::Engine;
use tempfile::TempDir;

/// One in-process node: a live engine plus the tempdirs backing it (held so
/// they outlive the engine).
pub struct Node {
    pub engine: Engine,
    pub data: TempDir,
    pub folder: TempDir,
}

impl Node {
    pub fn folder(&self) -> &Path {
        self.folder.path()
    }
}

/// An N-node share: `masters` co-masters (node 0 created the share) followed by
/// `viewers` read-only members, all joined and bootstrapped to node 0.
pub struct Cluster {
    pub nodes: Vec<Node>,
    pub share_id: String,
    pub master_key: String,
    pub viewer_key: String,
    pub masters: usize,
}

/// Build a cluster: node 0 creates the share from its (initially empty) folder,
/// co-masters join with the master key, viewers with the viewer key, everyone
/// bootstrapped to node 0. Seed content *after* this and drive to converge, or
/// pre-populate node 0's folder via the returned handle before first drive.
pub async fn cluster(masters: usize, viewers: usize) -> anyhow::Result<Cluster> {
    assert!(masters >= 1, "need at least the creating master");
    let total = masters + viewers;
    let mut nodes = Vec::with_capacity(total);
    for i in 0..total {
        let data = tempfile::tempdir()?;
        let folder = tempfile::tempdir()?;
        let engine = Engine::new(data.path()).await?;
        engine.set_device_name(&format!(
            "{}{i}",
            if i < masters { "master" } else { "viewer" }
        ))?;
        nodes.push(Node { engine, data, folder });
    }
    // Everyone online before exchanging bootstrap info.
    tokio::time::timeout(Duration::from_secs(20 + 5 * total as u64), async {
        for n in &nodes {
            n.engine.wait_online().await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("endpoints did not come online"))?;

    let created = {
        let folder = nodes[0].folder.path().to_path_buf();
        nodes[0].engine.create_share(&folder, vec![]).await?
    };
    let bootstrap = nodes[0].engine.endpoint_addr();
    for (i, n) in nodes.iter_mut().enumerate().skip(1) {
        let key = if i < masters {
            &created.master_key
        } else {
            &created.viewer_key
        };
        let folder = n.folder.path().to_path_buf();
        let id = n
            .engine
            .add_share(key, &folder, vec![bootstrap.clone()])
            .await?;
        assert_eq!(id, created.share_id);
    }
    Ok(Cluster {
        nodes,
        share_id: created.share_id,
        master_key: created.master_key,
        viewer_key: created.viewer_key,
        masters,
    })
}

impl Cluster {
    /// One "daemon tick" across all nodes: broadcast presence, grow the gossip
    /// mesh, reconcile. In-process tests must pump presence themselves — the
    /// daemon loop that normally does it isn't running.
    pub async fn tick(&mut self) {
        for n in &self.nodes {
            for j in n.engine.presence_broadcasts() {
                j.send().await;
            }
            for r in n.engine.presence_rejoins() {
                r.join().await;
            }
        }
        let share_id = self.share_id.clone();
        for n in &mut self.nodes {
            let _ = n.engine.reconcile(&share_id).await;
        }
    }

    /// Tick until `pred` holds, or fail after `timeout`. `label` names the wait
    /// in the failure message.
    pub async fn drive_until<F>(
        &mut self,
        timeout: Duration,
        label: &str,
        mut pred: F,
    ) -> anyhow::Result<()>
    where
        F: FnMut(&Cluster) -> bool,
    {
        let res = tokio::time::timeout(timeout, async {
            loop {
                self.tick().await;
                if pred(self) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await;
        res.map_err(|_| anyhow::anyhow!("timed out waiting for: {label}"))
    }

    /// Every node's folder equals `want` (byte-for-byte).
    pub fn folders_match(&self, want: &BTreeMap<String, Vec<u8>>) -> bool {
        self.nodes.iter().all(|n| &snapshot(n.folder()) == want)
    }

    /// This node's own manifest fingerprint, from its self entry in `peers()`.
    pub fn self_fp(&self, node: usize) -> u64 {
        self.nodes[node]
            .engine
            .peers(&self.share_id)
            .ok()
            .and_then(|ps| {
                ps.into_iter()
                    .find(|p| p.node_id == "This device")
                    .map(|p| p.manifest_fp)
            })
            .unwrap_or(0)
    }

    /// All nodes report the same non-zero fingerprint.
    pub fn fps_agree(&self) -> bool {
        let fp0 = self.self_fp(0);
        fp0 != 0 && (1..self.nodes.len()).all(|i| self.self_fp(i) == fp0)
    }

    /// The share's status string on `node` (e.g. "Healthy", "OutOfSync"), via
    /// the same summaries the daemon serves to the GUI/CLI.
    pub fn status(&self, node: usize) -> String {
        self.nodes[node]
            .engine
            .list_summaries()
            .into_iter()
            .find(|s| s.share_id == self.share_id)
            .map(|s| format!("{:?}", s.status))
            .unwrap_or_else(|| "<missing>".into())
    }

    /// Converged = every folder matches `want`, all fingerprints agree, and no
    /// node reports OutOfSync.
    pub fn converged(&self, want: &BTreeMap<String, Vec<u8>>) -> bool {
        self.folders_match(want)
            && self.fps_agree()
            && (0..self.nodes.len()).all(|i| self.status(i) != "OutOfSync")
    }

    pub async fn shutdown(self) -> anyhow::Result<()> {
        for n in self.nodes {
            n.engine.shutdown().await?;
        }
        Ok(())
    }
}

/// Read a folder into a sorted map of relative-path → contents. For corpora too
/// large to hold in memory use `seed_harness::corpus::verify` instead.
pub fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut map = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let rel = p
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                map.insert(rel, std::fs::read(&p).unwrap());
            }
        }
    }
    map
}

/// Deterministic, varied (non-compressible-ish) bytes of length `n`.
pub fn gen_bytes(n: usize) -> Vec<u8> {
    (0..n as u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}
