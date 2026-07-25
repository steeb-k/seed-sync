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

/// Install a tracing subscriber so engine logs are visible under `--nocapture`.
///
/// Integration tests installed no subscriber at all, so when one hung there was
/// no engine output to explain it — only the test's own assertion message. Off
/// by default (tests are quiet and fast); set `RUST_LOG` to turn it on:
///
/// ```text
/// RUST_LOG=seed_core=debug cargo test -p seed-core --test multi_master -- --ignored --nocapture
/// ```
///
/// Idempotent and safe to call from every test in a binary.
pub fn init_tracing() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if std::env::var_os("RUST_LOG").is_none() {
            return;
        }
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
    });
}

/// Deletes a share's OS-keystore entry when dropped.
///
/// Creating a master share writes a seed to the OS keystore (Windows Credential
/// Manager / Keychain / Secret Service), keyed by share id. Only
/// `Engine::remove_share` deletes it, and tests tear down with `shutdown()`, so
/// every master share a test created used to leak one credential *forever*.
/// Windows caps credentials per logon session (~512) and then fails `CredWrite`
/// with `ERROR_NOT_ENOUGH_MEMORY` (8) — at which point the engine silently falls
/// back to storing the master key in its DB and the `keystore` suite can no
/// longer establish its preconditions. That is exactly how this box accumulated
/// 639 stale `seed-sync` credentials and started failing that suite.
///
/// This is a `Drop` guard rather than a teardown call on purpose: integration
/// tests panic (that is how bugs get found), and a leak that only cleans up on
/// the happy path would still bleed on every red run.
///
/// Bind it immediately after creating a share:
/// ```ignore
/// let created = engine.create_share(folder.path(), vec![]).await?;
/// let _seed = common::SecretGuard::new(&created.share_id);
/// ```
#[must_use = "bind this to a variable (`let _seed = ...`); dropping it immediately \
              deletes the keystore entry the test still needs"]
pub struct SecretGuard(String);

impl SecretGuard {
    pub fn new(share_id: &str) -> Self {
        Self(share_id.to_string())
    }
}

impl Drop for SecretGuard {
    fn drop(&mut self) {
        seed_core::secrets::delete_seed(&self.0);
    }
}

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
    /// Deletes the cluster's keystore entry when the cluster drops — including
    /// on panic. A field rather than `impl Drop for Cluster`, because `shutdown`
    /// moves `nodes` out of `self`, which a `Drop` impl would forbid.
    /// See [`SecretGuard`].
    _seed: SecretGuard,
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
        nodes.push(Node {
            engine,
            data,
            folder,
        });
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
        _seed: SecretGuard::new(&created.share_id),
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
    /// node reports OutOfSync or NoPeers.
    ///
    /// NoPeers is excluded for the same reason OutOfSync is: it is a state in which
    /// the node's agreement with the pool is not actually established. A stranded
    /// node trivially "agrees" with every peer it can hear, because it can hear
    /// none — which is precisely the vacuous-truth trap that let a partition pass
    /// for perfect health (known-issues #17).
    pub fn converged(&self, want: &BTreeMap<String, Vec<u8>>) -> bool {
        self.folders_match(want)
            && self.fps_agree()
            && (0..self.nodes.len())
                .all(|i| self.status(i) != "OutOfSync" && self.status(i) != "NoPeers")
    }

    /// Best-effort teardown, bounded per node so a wedged iroh close can't
    /// hang a test past its assertions (which have all passed by this point).
    pub async fn shutdown(self) -> anyhow::Result<()> {
        for n in self.nodes {
            let _ = tokio::time::timeout(Duration::from_secs(15), n.engine.shutdown()).await;
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
