//! The sync engine: maps a local folder to an iroh-docs document + blob store,
//! enforces the signed-manifest trust model, and mirrors state to disk.
//!
//! Per share, the model is:
//!   * one iroh-docs replica whose namespace is derived from the share's
//!     32-byte secret (master) / public (viewer);
//!   * one doc entry per file (key = relative POSIX path, value = file content),
//!     which exists purely to transfer & dedup content over the network;
//!   * one signed control entry `\x00manifest` holding the authoritative,
//!     master-signed file list — the single source of truth.
//!
//! A viewer applies by reconciling its folder to the verified manifest: it
//! writes/overwrites listed files and deletes anything not listed (so deletions
//! propagate and a viewer's own edits are reverted — strict, hard-overwrite
//! mirror).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use futures_lite::StreamExt;
use iroh_blobs::Hash;
use iroh_docs::{
    api::Doc, engine::LiveEvent, store::Query, sync::Capability, AuthorId, NamespaceId,
    NamespaceSecret,
};

/// How long since a peer was last heard from before we consider it offline.
const PEER_ONLINE_TTL_SECS: i64 = 60;

/// Tracks the peers seen for one share, fed by the doc's live events. `total`
/// is every distinct peer seen since the daemon started; `online` is those that
/// are currently connected (a neighbor) or heard-from within the TTL.
#[derive(Default)]
struct PeerRoster {
    peers: HashMap<String, PeerEntry>,
}

struct PeerEntry {
    neighbor: bool,
    last_seen: i64,
}

impl PeerRoster {
    fn note(&mut self, id: &str, neighbor: Option<bool>) {
        let e = self.peers.entry(id.to_string()).or_insert(PeerEntry {
            neighbor: false,
            last_seen: 0,
        });
        e.last_seen = now_secs();
        if let Some(n) = neighbor {
            e.neighbor = n;
        }
    }

    fn is_online(&self, e: &PeerEntry, now: i64) -> bool {
        e.neighbor || (now - e.last_seen) < PEER_ONLINE_TTL_SECS
    }

    fn counts(&self) -> (u32, u32) {
        let now = now_secs();
        let online = self
            .peers
            .values()
            .filter(|e| self.is_online(e, now))
            .count() as u32;
        (online, self.peers.len() as u32)
    }

    fn infos(&self) -> Vec<seed_ipc::PeerInfo> {
        let now = now_secs();
        self.peers
            .iter()
            .map(|(id, e)| seed_ipc::PeerInfo {
                node_id: id.chars().take(16).collect(),
                // We can't yet tell a peer's role from doc events; reported as
                // Viewer as a placeholder until presence carries it.
                role: seed_ipc::Role::Viewer,
                online: self.is_online(e, now),
                last_seen: e.last_seen,
                have_seqno: 0,
            })
            .collect()
    }
}

use crate::identity::{Role, ShareKey};
use crate::manifest::{self, Manifest, SignedManifest};
use crate::node::IrohNode;
use crate::scan::{self, IgnoreSet};

/// Reserved doc key holding the signed manifest. The `\x00` prefix namespaces
/// control keys away from user file paths.
const MANIFEST_KEY: &[u8] = b"\x00manifest";
/// How long a published manifest stays valid; the master re-signs on each
/// publish, so this only bounds how stale an unattended share may get.
const MANIFEST_TTL_SECS: i64 = 60 * 60 * 24 * 30;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Returned by [`Engine::create_share`].
pub struct CreatedShare {
    pub share_id: String,
    pub master_key: String,
    pub viewer_key: String,
}

/// In-memory per-share state.
struct ShareState {
    key: ShareKey,
    folder: PathBuf,
    doc: Doc,
    ignore: Vec<String>,
    /// Highest manifest seqno accepted (anti-rollback watermark).
    last_seqno: u64,
    /// Cheap (path,size,mtime) signature of the last published folder state,
    /// used by the master reconcile loop to skip unchanged ticks.
    last_quick_sig: u64,
    /// When paused, the reconcile loop skips this share.
    paused: bool,
    /// Live peer membership, updated by the doc event task.
    roster: Arc<StdMutex<PeerRoster>>,
    /// Unix seconds of the last successful publish (master) or applied update
    /// (viewer); 0 if none yet this session.
    last_updated: i64,
}

impl ShareState {
    fn ignore_set(&self) -> IgnoreSet {
        // Always ignore our own control prefix on disk (there is none on disk,
        // but keep the hook for future per-share metadata files).
        let (set, _bad) = IgnoreSet::compile(&self.ignore);
        set
    }
}

/// The engine owns the iroh node and the set of shares.
pub struct Engine {
    node: IrohNode,
    author: AuthorId,
    shares: HashMap<String, ShareState>,
    db: crate::db::Db,
}

impl Engine {
    /// Bootstrap the engine against a data directory, reloading any persisted
    /// shares (so the daemon is restart-safe).
    pub async fn new(data_dir: &Path) -> anyhow::Result<Self> {
        let node = IrohNode::spawn(data_dir).await?;
        let author = node.docs_api().author_default().await?;
        let db = crate::db::Db::open(&data_dir.join("state.db"))?;
        let mut engine = Self {
            node,
            author,
            shares: HashMap::new(),
            db,
        };
        engine.reload_shares().await?;
        Ok(engine)
    }

    /// Re-open every persisted share's replica and resume sync. Folder content
    /// is restored from the persisted local doc/blob stores even before peers
    /// reconnect.
    async fn reload_shares(&mut self) -> anyhow::Result<()> {
        for rec in self.db.load_all()? {
            let key = match ShareKey::decode(&rec.key) {
                Ok(k) => k,
                Err(e) => {
                    tracing::warn!("skipping unreadable share {}: {e}", rec.share_id);
                    continue;
                }
            };
            let state = self
                .open_share(
                    &key,
                    &PathBuf::from(&rec.folder),
                    vec![],
                    rec.ignore,
                    rec.last_seqno,
                    rec.paused,
                )
                .await?;
            self.shares.insert(rec.share_id, state);
        }
        if !self.shares.is_empty() {
            tracing::info!("reloaded {} share(s)", self.shares.len());
        }
        Ok(())
    }

    /// Open (import + start serving/syncing) a share's replica and build its
    /// in-memory state. Shared by create, add, and reload.
    async fn open_share(
        &self,
        key: &ShareKey,
        folder: &Path,
        bootstrap: Vec<iroh::EndpointAddr>,
        ignore: Vec<String>,
        last_seqno: u64,
        paused: bool,
    ) -> anyhow::Result<ShareState> {
        let capability = match key.role {
            Role::Master => {
                let seed = key
                    .seed_bytes()
                    .ok_or_else(|| anyhow!("master key missing seed"))?;
                Capability::Write(NamespaceSecret::from_bytes(&seed))
            }
            Role::Viewer => {
                let ns = NamespaceId::from(
                    iroh_docs::NamespacePublicKey::from_bytes(&key.master_pub_bytes())
                        .map_err(|e| anyhow!("bad namespace key: {e}"))?,
                );
                Capability::Read(ns)
            }
        };
        let doc = self
            .node
            .docs_api()
            .import_namespace(capability)
            .await
            .context("import namespace")?;
        std::fs::create_dir_all(folder)?;

        // If no explicit bootstrap was given, a viewer can still reach the
        // master by its endpoint id (carried in the key) via n0 DNS discovery —
        // build an address with just the id and let discovery resolve it.
        let mut bootstrap = bootstrap;
        if bootstrap.is_empty() && matches!(key.role, Role::Viewer) {
            if let Some(eid) = key.endpoint_id() {
                if let Ok(pk) = iroh::EndpointId::from_bytes(&eid) {
                    tracing::info!("no bootstrap given; using endpoint-id discovery");
                    bootstrap.push(iroh::EndpointAddr::new(pk));
                }
            }
        }

        // Subscribe (keeps live sync alive + feeds the peer roster) and register
        // the namespace for serving + connect to any bootstrap peers.
        let roster = Arc::new(StdMutex::new(PeerRoster::default()));
        spawn_event_task(&doc, roster.clone()).await?;
        doc.start_sync(bootstrap).await.context("start sync")?;
        Ok(ShareState {
            key: key.clone(),
            folder: folder.to_path_buf(),
            doc,
            ignore,
            last_seqno,
            last_quick_sig: 0,
            paused,
            roster,
            last_updated: 0,
        })
    }

    pub fn endpoint_addr(&self) -> iroh::EndpointAddr {
        self.node.addr()
    }

    /// This node's dialable address as an endpoint-ticket string, for handing to
    /// a peer as a bootstrap hint.
    pub fn endpoint_ticket(&self) -> String {
        iroh_tickets::endpoint::EndpointTicket::from(self.node.addr()).to_string()
    }

    /// Parse an endpoint-ticket string into a dialable address.
    pub fn parse_bootstrap(s: &str) -> anyhow::Result<iroh::EndpointAddr> {
        use std::str::FromStr;
        let ticket = iroh_tickets::endpoint::EndpointTicket::from_str(s)
            .map_err(|e| anyhow!("bad bootstrap ticket: {e}"))?;
        Ok(ticket.endpoint_addr().clone())
    }

    /// Build IPC summaries for all shares.
    pub fn list_summaries(&self) -> Vec<seed_ipc::ShareSummary> {
        self.shares
            .iter()
            .map(|(id, s)| {
                let role = match s.key.role {
                    Role::Master => seed_ipc::Role::Master,
                    Role::Viewer => seed_ipc::Role::Viewer,
                };
                let name = s
                    .folder
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| id.clone());
                let status = if s.paused {
                    seed_ipc::ShareStatus::Paused
                } else if s.last_seqno > 0 {
                    seed_ipc::ShareStatus::Healthy
                } else {
                    seed_ipc::ShareStatus::Syncing
                };
                let (online, total) = s.roster.lock().map(|r| r.counts()).unwrap_or((0, 0));
                seed_ipc::ShareSummary {
                    share_id: id.clone(),
                    name,
                    folder: s.folder.to_string_lossy().into_owned(),
                    role,
                    status,
                    percent: if s.last_seqno > 0 { 100 } else { 0 },
                    online,
                    total,
                    paused: s.paused,
                    last_updated: s.last_updated,
                }
            })
            .collect()
    }

    /// Peer membership for a share (for the GUI's "view peers" dialog).
    pub fn peers(&self, share_id: &str) -> anyhow::Result<Vec<seed_ipc::PeerInfo>> {
        let state = self
            .shares
            .get(share_id)
            .ok_or_else(|| anyhow!("unknown share {share_id}"))?;
        Ok(state.roster.lock().map(|r| r.infos()).unwrap_or_default())
    }

    /// Reveal the keys for a share. Returns the master key only when this node
    /// holds master role for the share.
    pub fn reveal_keys(&self, share_id: &str) -> anyhow::Result<(Option<String>, String)> {
        let state = self
            .shares
            .get(share_id)
            .ok_or_else(|| anyhow!("unknown share {share_id}"))?;
        let viewer = state.key.encode_viewer();
        let master = match state.key.role {
            Role::Master => Some(state.key.encode()),
            Role::Viewer => None,
        };
        Ok((master, viewer))
    }

    /// Wait until this engine's endpoint is online (has a complete address).
    pub async fn wait_online(&self) {
        self.node.wait_online().await;
    }

    /// Cumulative (bytes_sent, bytes_received) for throughput sampling.
    pub fn byte_totals(&self) -> (u64, u64) {
        self.node.byte_totals()
    }

    /// Diagnostic: list the doc keys currently visible for a share.
    pub async fn debug_doc_keys(&self, share_id: &str) -> anyhow::Result<Vec<String>> {
        let state = self
            .shares
            .get(share_id)
            .ok_or_else(|| anyhow!("unknown share {share_id}"))?;
        let mut s = std::pin::pin!(state.doc.get_many(Query::all()).await?);
        let mut out = Vec::new();
        while let Some(e) = s.next().await {
            out.push(String::from_utf8_lossy(e?.key()).into_owned());
        }
        Ok(out)
    }

    /// Create a brand-new master share for `folder`, returning the (master,
    /// viewer) key strings. Performs the initial publish.
    pub async fn create_share(
        &mut self,
        folder: &Path,
        ignore: Vec<String>,
    ) -> anyhow::Result<CreatedShare> {
        let key = ShareKey::generate_master().with_endpoint_id(self.node.endpoint_id_bytes());
        let share_id = key.share_id_hex();
        let state = self
            .open_share(&key, folder, vec![], ignore.clone(), 0, false)
            .await?;
        self.shares.insert(share_id.clone(), state);
        self.publish(&share_id).await?; // sets last_seqno

        // Persist (with the post-publish seqno).
        let last_seqno = self.shares[&share_id].last_seqno;
        self.db.upsert_share(&crate::db::ShareRecord {
            share_id: share_id.clone(),
            key: key.encode(),
            folder: folder.to_string_lossy().into_owned(),
            role_master: true,
            ignore,
            last_seqno,
            paused: false,
        })?;

        Ok(CreatedShare {
            share_id,
            master_key: key.encode(),
            viewer_key: key.encode_viewer(),
        })
    }

    /// Add an existing share from a key string, syncing into `folder`. Optional
    /// `bootstrap` addresses kick off the connection without DNS discovery
    /// (used by the loopback harness; production resolves via the endpoint id).
    pub async fn add_share(
        &mut self,
        key_str: &str,
        folder: &Path,
        bootstrap: Vec<iroh::EndpointAddr>,
    ) -> anyhow::Result<String> {
        let key = ShareKey::decode(key_str).context("decode share key")?;
        let share_id = key.share_id_hex();
        let state = self
            .open_share(&key, folder, bootstrap, vec![], 0, false)
            .await?;
        let role_master = matches!(key.role, Role::Master);
        self.shares.insert(share_id.clone(), state);

        self.db.upsert_share(&crate::db::ShareRecord {
            share_id: share_id.clone(),
            key: key.encode(),
            folder: folder.to_string_lossy().into_owned(),
            role_master,
            ignore: vec![],
            last_seqno: 0,
            paused: false,
        })?;
        Ok(share_id)
    }

    /// (Master) Scan the folder and publish a new signed manifest + content.
    pub async fn publish(&mut self, share_id: &str) -> anyhow::Result<()> {
        let author = self.author;
        let state = self
            .shares
            .get_mut(share_id)
            .ok_or_else(|| anyhow!("unknown share {share_id}"))?;
        let signing = state
            .key
            .signing_key()
            .ok_or_else(|| anyhow!("cannot publish: not a master share"))?;

        let ignore = state.ignore_set();
        let scanned = scan::scan(&state.folder, &ignore).context("scan folder")?;

        // Write/refresh a doc entry per file (content -> blob), and collect the
        // authoritative file list.
        let mut files = Vec::with_capacity(scanned.len());
        let mut live_keys: HashSet<Vec<u8>> = HashSet::new();
        for sf in &scanned {
            let content = std::fs::read(&sf.abs_path)
                .with_context(|| format!("read {}", sf.abs_path.display()))?;
            state
                .doc
                .set_bytes(author, sf.entry.path.as_bytes().to_vec(), content)
                .await
                .with_context(|| format!("set doc entry {}", sf.entry.path))?;
            live_keys.insert(sf.entry.path.as_bytes().to_vec());
            files.push(sf.entry.clone());
        }

        // GC doc entries for files that no longer exist (cosmetic; the manifest
        // is authoritative). Skip the manifest control key.
        let mut existing = std::pin::pin!(state.doc.get_many(Query::all()).await?);
        let mut stale: Vec<Vec<u8>> = Vec::new();
        while let Some(entry) = existing.next().await {
            let entry = entry?;
            let k = entry.key().to_vec();
            if k == MANIFEST_KEY {
                continue;
            }
            if !live_keys.contains(&k) {
                stale.push(k);
            }
        }
        for k in stale {
            let _ = state.doc.del(author, k).await;
        }

        // Build, sign, and publish the manifest.
        let seqno = state.last_seqno + 1;
        let manifest = Manifest::new(
            state.key.share_id().to_vec(),
            seqno,
            now_secs() + MANIFEST_TTL_SECS,
            files,
            state.ignore.clone(),
        );
        let signed = manifest::sign(&manifest, &signing).map_err(|e| anyhow!("sign: {e}"))?;
        let mut cbor = Vec::new();
        ciborium::into_writer(&signed, &mut cbor).context("encode signed manifest")?;
        state
            .doc
            .set_bytes(author, MANIFEST_KEY.to_vec(), cbor)
            .await
            .context("publish manifest")?;
        state.last_seqno = seqno;
        state.last_quick_sig = scan::quick_signature(&state.folder, &ignore);
        state.last_updated = now_secs();
        self.db.set_seqno(share_id, seqno)?;
        Ok(())
    }

    /// (Master) Publish only if the folder changed since the last publish, using
    /// a cheap (path,size,mtime) signature. Returns whether a publish happened.
    pub async fn publish_if_changed(&mut self, share_id: &str) -> anyhow::Result<bool> {
        let changed = {
            let state = self
                .shares
                .get(share_id)
                .ok_or_else(|| anyhow!("unknown share {share_id}"))?;
            let sig = scan::quick_signature(&state.folder, &state.ignore_set());
            sig != state.last_quick_sig
        };
        if changed {
            self.publish(share_id).await?;
        }
        Ok(changed)
    }

    /// (Viewer/mirror) Reconcile the local folder to the latest verified
    /// manifest. Returns `Ok(true)` if a new manifest was applied, `Ok(false)`
    /// if nothing new was ready yet (content still downloading, or already
    /// up to date).
    pub async fn apply(&mut self, share_id: &str) -> anyhow::Result<bool> {
        let state = self
            .shares
            .get_mut(share_id)
            .ok_or_else(|| anyhow!("unknown share {share_id}"))?;

        // 1. Fetch the manifest control entry.
        let Some(manifest_entry) = state
            .doc
            .get_one(Query::single_latest_per_key().key_exact(MANIFEST_KEY))
            .await?
        else {
            return Ok(false); // not synced yet
        };
        let mhash = manifest_entry.content_hash();
        if !self.node.blobs.blobs().has(mhash).await? {
            return Ok(false); // manifest content still downloading
        }
        let bytes = self.node.blobs.blobs().get_bytes(mhash).await?;
        let signed: SignedManifest =
            ciborium::from_reader(bytes.as_ref()).context("decode signed manifest")?;

        // 2. Verify signature/validity (not seqno yet) against the pinned master.
        let pinned = state.key.master_pub;
        let share_id_bytes = state.key.share_id();
        let manifest = manifest::verify_signed(&signed, &pinned, &share_id_bytes, now_secs())
            .map_err(|e| anyhow!("manifest verification failed: {e}"))?;
        // Anti-rollback: a strictly older manifest is an attack; reject it. An
        // equal seqno is the manifest we already have — still reconcile disk
        // (to revert local drift), but don't advance the watermark.
        if manifest.seqno < state.last_seqno {
            return Err(anyhow!(
                "rollback: manifest seqno {} < watermark {}",
                manifest.seqno,
                state.last_seqno
            ));
        }
        let is_new = manifest.seqno > state.last_seqno;
        // Self-consistency: signed root must match the signed file list.
        manifest::verify_root(&manifest, &manifest.files).map_err(|e| anyhow!("root: {e}"))?;

        // 3. Ensure every listed file's content is present before mutating disk.
        let mut desired: HashSet<String> = HashSet::new();
        for fe in &manifest.files {
            desired.insert(fe.path.clone());
            let arr: [u8; 32] = fe.hash.as_slice().try_into().context("bad hash len")?;
            if !self.node.blobs.blobs().has(Hash::from(arr)).await? {
                return Ok(false); // a file's content is still downloading; retry later
            }
        }

        // 4. Write/overwrite listed files whose local content differs.
        for fe in &manifest.files {
            let target = state.folder.join(rel_to_native(&fe.path));
            if file_matches(&target, &fe.hash) {
                continue;
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let arr: [u8; 32] = fe.hash.as_slice().try_into().unwrap();
            self.node
                .blobs
                .blobs()
                .export(Hash::from(arr), &target)
                .await
                .with_context(|| format!("export {}", fe.path))?;
        }

        // 5. Delete anything on disk not in the manifest (deletion propagation
        //    + revert of viewer-created/edited files).
        let ignore = state.ignore_set();
        for sf in scan::scan(&state.folder, &ignore)? {
            if !desired.contains(&sf.entry.path) {
                let _ = std::fs::remove_file(&sf.abs_path);
            }
        }
        prune_empty_dirs(&state.folder);

        if is_new {
            state.last_seqno = manifest.seqno;
            state.last_updated = now_secs();
            let seqno = manifest.seqno;
            self.db.set_seqno(share_id, seqno)?;
        }
        Ok(is_new)
    }

    /// Reconcile every (non-paused) share: viewers apply, masters publish on
    /// change. Returns the ids of shares that changed (for status events).
    pub async fn reconcile_all(&mut self) -> Vec<String> {
        let plan: Vec<(String, Role, bool)> = self
            .shares
            .iter()
            .map(|(id, s)| (id.clone(), s.key.role, s.paused))
            .collect();
        let mut changed = Vec::new();
        for (id, role, paused) in plan {
            if paused {
                continue;
            }
            let did = match role {
                Role::Viewer => self.apply(&id).await.unwrap_or(false),
                Role::Master => self.publish_if_changed(&id).await.unwrap_or(false),
            };
            if did {
                changed.push(id);
            }
        }
        changed
    }

    /// Pause or resume a share (persisted; the reconcile loop skips paused shares).
    pub fn set_paused(&mut self, share_id: &str, paused: bool) -> anyhow::Result<()> {
        let state = self
            .shares
            .get_mut(share_id)
            .ok_or_else(|| anyhow!("unknown share {share_id}"))?;
        state.paused = paused;
        self.db.set_paused(share_id, paused)?;
        Ok(())
    }

    /// Remove a share from the engine and persistence. Optionally delete its
    /// local folder contents.
    pub async fn remove_share(&mut self, share_id: &str, delete_files: bool) -> anyhow::Result<()> {
        if let Some(state) = self.shares.remove(share_id) {
            let _ = state.doc.leave().await;
            if delete_files {
                let _ = std::fs::remove_dir_all(&state.folder);
            }
        }
        self.db.remove_share(share_id)?;
        Ok(())
    }

    /// Endpoint address for a peer to dial (used by tests to bootstrap).
    pub async fn shutdown(self) -> anyhow::Result<()> {
        self.node.shutdown().await
    }
}

/// Subscribe to a doc's live events in a background task. This keeps the
/// live-sync session active and feeds the peer roster (neighbor up/down, remote
/// inserts, sync completions).
async fn spawn_event_task(doc: &Doc, roster: Arc<StdMutex<PeerRoster>>) -> anyhow::Result<()> {
    let mut events = doc.subscribe().await?;
    tokio::spawn(async move {
        while let Some(ev) = events.next().await {
            if let Ok(e) = &ev {
                if let Ok(mut r) = roster.lock() {
                    match e {
                        LiveEvent::NeighborUp(pk) => r.note(&pk.to_string(), Some(true)),
                        LiveEvent::NeighborDown(pk) => r.note(&pk.to_string(), Some(false)),
                        LiveEvent::InsertRemote { from, .. } => r.note(&from.to_string(), None),
                        LiveEvent::SyncFinished(se) => r.note(&se.peer.to_string(), None),
                        _ => {}
                    }
                }
            }
            if std::env::var("SEED_DEBUG_EVENTS").is_ok() {
                eprintln!("[doc event] {ev:?}");
            }
        }
    });
    Ok(())
}

/// Convert a relative POSIX path to a native path.
fn rel_to_native(rel: &str) -> PathBuf {
    rel.split('/').collect()
}

/// Whether the file at `path` exists and its BLAKE3 hash equals `want`.
fn file_matches(path: &Path, want: &[u8]) -> bool {
    match scan::hash_file(path) {
        Ok((hash, _)) => hash == want,
        Err(_) => false,
    }
}

/// Remove now-empty directories under `root` (best effort), leaving `root`.
fn prune_empty_dirs(root: &Path) {
    fn visit(dir: &Path, root: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                visit(&p, root);
            }
        }
        if dir != root {
            let _ = std::fs::remove_dir(dir); // fails (ignored) if non-empty
        }
    }
    visit(root, root);
}
