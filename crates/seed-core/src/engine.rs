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
use bao_tree::io::BaoContentItem;
use ed25519_dalek::SigningKey;
use futures_lite::StreamExt;
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, EndpointId};
use iroh_blobs::api::blobs::{AddPathOptions, ExportMode, ExportOptions, ImportMode};
use iroh_blobs::get::request::{get_blob, GetBlobItem};
use iroh_blobs::{store::fs::FsStore, BlobFormat, Hash};
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

    /// The full peer-id strings currently known, for re-fetching content from
    /// peers during self-heal.
    fn peer_ids(&self) -> Vec<String> {
        self.peers.keys().cloned().collect()
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

/// Write a master seed to the OS keystore without letting it block share
/// creation. The keystore call is synchronous and, under the Windows
/// LocalSystem service (session 0), the Credential Manager API can hang
/// indefinitely. Run it on a blocking thread and abandon it after a short
/// timeout so [`Engine::persist_share`] falls back to storing the key in the DB
/// rather than wedging the whole request.
async fn store_seed_bounded(share_id: &str, seed: [u8; 32]) -> anyhow::Result<()> {
    let share_id = share_id.to_owned();
    let handle = tokio::task::spawn_blocking(move || crate::secrets::store_seed(&share_id, &seed));
    match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
        Ok(Ok(inner)) => inner,
        Ok(Err(join_err)) => Err(anyhow!("keystore task failed: {join_err}")),
        Err(_) => Err(anyhow!("keystore write timed out after 5s")),
    }
}

/// Try to delete a blob's orphaned owned `data/<hash>.data` file after a reference
/// export. Returns `true` when it's gone (reclaimed now, or already moved/absent)
/// and `false` when it's still locked and should be retried on a later pass.
///
/// `ExportMode::TryReference` moves the owned blob into the mirror with a `rename`;
/// across volumes that fails with `EXDEV` and iroh falls back to a copy, leaving the
/// downloaded copy behind (no GC runs to reclaim it — see iroh-blobs `fs.rs`
/// `export_path_impl`). The store entry is then `External` (it serves from the mirror
/// file and keeps the outboard), so the owned `.data` is pure waste — deleting it
/// keeps a viewer at 1× regardless of whether its mirror is on the data dir's volume.
///
/// On Windows the file can't be deleted while iroh still holds its handle (Linux
/// allows unlinking an open file, so this usually succeeds at once there). iroh's
/// EntityManager releases the idle handle a few seconds after the export, so the
/// caller queues the hash and retries each reconcile until this returns `true`. The
/// outboard `.obao4` is never touched. Only ever called for hashes we just exported
/// (entry guaranteed `External`), so a legitimately-owned blob is never destroyed.
fn try_reclaim_owned_data(blobs_dir: &Path, hash: Hash) -> bool {
    let data_file = blobs_dir
        .join("data")
        .join(format!("{}.data", hash.to_hex()));
    match std::fs::remove_file(&data_file) {
        Ok(()) => {
            tracing::debug!("reclaimed orphaned owned blob copy {}", data_file.display());
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false, // iroh still holds the handle; retry next reconcile
    }
}

/// Provider endpoint ids to re-fetch content from during self-heal: the master
/// (its id is carried in the share key) first, then any peers seen in the roster.
fn peer_providers(key: &ShareKey, roster: &Arc<StdMutex<PeerRoster>>) -> Vec<EndpointId> {
    let mut ids = Vec::new();
    if let Some(eid) = key.endpoint_id() {
        if let Ok(id) = EndpointId::from_bytes(&eid) {
            ids.push(id);
        }
    }
    if let Ok(r) = roster.lock() {
        for s in r.peer_ids() {
            if let Ok(id) = s.parse::<EndpointId>() {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

/// Re-fetch a blob's verified bytes from a peer and atomically rewrite the mirror
/// file at `target`. Used when a referenced file diverged from its manifest hash
/// (e.g. a viewer edited it): the local reference is stale and can't self-repair,
/// so we pull a clean copy from whoever still serves it. Tries each provider in
/// turn; on total failure returns an error so the next reconcile tick retries.
async fn self_heal_file(
    endpoint: &Endpoint,
    providers: &[EndpointId],
    hash: Hash,
    target: &Path,
) -> anyhow::Result<()> {
    if providers.is_empty() {
        anyhow::bail!("no known providers to repair {}", target.display());
    }
    let mut last_err = None;
    for &pid in providers {
        let conn = match endpoint
            .connect(EndpointAddr::new(pid), iroh_blobs::ALPN)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                last_err = Some(anyhow!("connect {pid}: {e}"));
                continue;
            }
        };
        match fetch_blob_to_file(&conn, hash, target).await {
            Ok(()) => {
                tracing::info!("self-healed {} from {pid}", target.display());
                return Ok(());
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("self-heal failed for {}", target.display())))
}

/// Stream a blob from a connection, writing its verified leaves into a temp file
/// next to `target`, then atomically replace `target`. Streams chunk-by-chunk
/// (no whole-file in memory), and the content is bao-verified against `hash` as
/// it arrives, so a bad peer cannot write wrong bytes.
async fn fetch_blob_to_file(conn: &Connection, hash: Hash, target: &Path) -> anyhow::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let tmp = target.with_extension("seedheal-tmp");
    let mut file =
        std::fs::File::create(&tmp).with_context(|| format!("create temp {}", tmp.display()))?;
    let mut stream = get_blob(conn.clone(), hash);
    let mut complete = false;
    while let Some(item) = stream.next().await {
        match item {
            GetBlobItem::Item(BaoContentItem::Leaf(leaf)) => {
                file.seek(SeekFrom::Start(leaf.offset))?;
                file.write_all(&leaf.data)?;
            }
            GetBlobItem::Item(BaoContentItem::Parent(_)) => {} // hash-tree node, no data
            GetBlobItem::Done(_) => {
                complete = true;
                break;
            }
            GetBlobItem::Error(e) => {
                drop(file);
                let _ = std::fs::remove_file(&tmp);
                return Err(anyhow!("fetch {hash}: {e}"));
            }
        }
    }
    file.flush()?;
    drop(file);
    if !complete {
        let _ = std::fs::remove_file(&tmp);
        anyhow::bail!("fetch {hash}: stream ended before completion");
    }
    // Replace target (Windows rename fails if the destination exists).
    if target.exists() {
        std::fs::remove_file(target)?;
    }
    std::fs::rename(&tmp, target).with_context(|| format!("replace {}", target.display()))?;
    Ok(())
}

/// Returned by [`Engine::create_share`].
pub struct CreatedShare {
    pub share_id: String,
    pub master_key: String,
    pub viewer_key: String,
}

/// A self-contained unit of publish work holding *cloned* iroh handles, so the
/// heavy part (scanning the folder + streaming every file into the blob store)
/// can run with **no engine lock held** — keeping the daemon responsive while a
/// large folder transfers. Produced under a brief lock by [`Engine::make_job`]
/// or [`Engine::create_open`], then run via [`PublishJob::run`]; the engine is
/// only re-locked afterwards (via [`Engine::finish_publish`]) to record the
/// resulting seqno.
pub struct PublishJob {
    share_id: String,
    folder: PathBuf,
    ignore: Vec<String>,
    doc: Doc,
    blobs: FsStore,
    signing: SigningKey,
    share_id_bytes: Vec<u8>,
    author: AuthorId,
    last_seqno: u64,
    progress: Arc<StdMutex<HashMap<String, (u64, u64)>>>,
}

impl PublishJob {
    pub fn share_id(&self) -> &str {
        &self.share_id
    }

    fn set_progress(&self, done: u64, total: u64) {
        if let Ok(mut m) = self.progress.lock() {
            m.insert(self.share_id.clone(), (done, total));
        }
    }

    /// Scan the folder, stream each file into the blob store by path, refresh the
    /// doc, and publish a freshly-signed manifest. Returns the new manifest
    /// seqno. Touches only the cloned handles, so it is safe to call without the
    /// engine lock.
    pub async fn run(&self) -> anyhow::Result<u64> {
        let (ignore_set, _bad) = IgnoreSet::compile(&self.ignore);
        // List files *without* hashing — the blob import below hashes each file
        // as it streams it in, so we read every file exactly once (vs. scanning
        // to hash and then re-reading to import). This also lets progress start
        // immediately instead of after a full hashing pass.
        let listed = scan::list_files(&self.folder, &ignore_set).context("scan folder")?;

        // Seed the live import progress (bytes) so the GUI can show a moving
        // percent while large files stream in. Cleared in `finish_publish`.
        let total_bytes: u64 = listed.iter().map(|f| f.size).sum();
        let mut done_bytes: u64 = 0;
        self.set_progress(0, total_bytes);

        // Write/refresh a doc entry per file (content -> blob), collecting the
        // authoritative file list. Files stream into the blob store by path;
        // never load a whole file into memory.
        let mut files = Vec::with_capacity(listed.len());
        let mut live_keys: HashSet<Vec<u8>> = HashSet::new();
        for f in &listed {
            // Import by *reference*, not copy: the blob store keeps only the
            // outboard (hash tree) and points at the file in place, instead of
            // duplicating the content into `…/blobs`. This is what stops a shared
            // folder from doubling on disk. The import still hashes the file to
            // build the outboard, so we get the content hash for the doc entry +
            // manifest. Hold the temp tag until the doc references the hash so the
            // freshly-imported blob can't be reclaimed in between.
            //
            // Tradeoff: if the master later edits/moves the file, the referenced
            // blob goes stale — but the reconcile loop re-imports changed files
            // (new hash + outboard) and republishes, so peers move to the new
            // blob and a stale read just fails verification rather than corrupts.
            let tag = self
                .blobs
                .blobs()
                .add_path_with_opts(AddPathOptions {
                    path: f.abs.clone(),
                    format: BlobFormat::Raw,
                    mode: ImportMode::TryReference,
                })
                .temp_tag()
                .await
                .with_context(|| format!("import {}", f.abs.display()))?;
            let hash = tag.hash();
            self.doc
                .set_hash(self.author, f.rel.as_bytes().to_vec(), hash, f.size)
                .await
                .with_context(|| format!("set doc entry {}", f.rel))?;
            files.push(manifest::FileEntry {
                path: f.rel.clone(),
                hash: hash.as_bytes().to_vec(),
                size: f.size,
                deleted: false,
            });
            live_keys.insert(f.rel.as_bytes().to_vec());
            done_bytes += f.size;
            self.set_progress(done_bytes, total_bytes);
            drop(tag);
        }

        // GC doc entries for files that no longer exist (cosmetic; the manifest
        // is authoritative). Skip the manifest control key.
        let mut existing = std::pin::pin!(self.doc.get_many(Query::all()).await?);
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
            let _ = self.doc.del(self.author, k).await;
        }

        // Build, sign, and publish the manifest.
        let seqno = self.last_seqno + 1;
        let manifest = Manifest::new(
            self.share_id_bytes.clone(),
            seqno,
            now_secs() + MANIFEST_TTL_SECS,
            files,
            self.ignore.clone(),
        );
        let signed = manifest::sign(&manifest, &self.signing).map_err(|e| anyhow!("sign: {e}"))?;
        let mut cbor = Vec::new();
        ciborium::into_writer(&signed, &mut cbor).context("encode signed manifest")?;
        self.doc
            .set_bytes(self.author, MANIFEST_KEY.to_vec(), cbor)
            .await
            .context("publish manifest")?;
        Ok(seqno)
    }
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
    /// Set while a [`PublishJob`] for this share is running off-lock, so the
    /// reconcile loop doesn't start a second concurrent publish of it.
    publishing: bool,
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
    /// Live import progress (`done_bytes`, `total_bytes`) for shares currently
    /// being published off-lock, keyed by share id. Shared with each in-flight
    /// [`PublishJob`] so [`Engine::list_summaries`] can report a moving percent.
    progress: Arc<StdMutex<HashMap<String, (u64, u64)>>>,
    /// Hashes whose orphaned owned blob copy (left by a cross-volume reference
    /// export) still needs deleting. Retried each reconcile until iroh releases
    /// the file handle — see [`try_reclaim_owned_data`].
    reclaim_pending: std::collections::HashSet<Hash>,
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
            progress: Arc::new(StdMutex::new(HashMap::new())),
            reclaim_pending: std::collections::HashSet::new(),
        };
        engine.reload_shares().await?;
        Ok(engine)
    }

    /// Re-open every persisted share's replica and resume sync. Folder content
    /// is restored from the persisted local doc/blob stores even before peers
    /// reconnect.
    async fn reload_shares(&mut self) -> anyhow::Result<()> {
        for rec in self.db.load_all()? {
            let mut key = match ShareKey::decode(&rec.key) {
                Ok(k) => k,
                Err(e) => {
                    tracing::warn!("skipping unreadable share {}: {e}", rec.share_id);
                    continue;
                }
            };
            // Master shares keep their seed in the OS keystore; load it to
            // restore write capability. If it's unavailable, run read-only.
            if rec.role_master && rec.seed_in_keyring {
                match crate::secrets::load_seed(&rec.share_id) {
                    Ok(seed) => {
                        key = ShareKey::from_master_seed(seed)
                            .with_endpoint_id(self.node.endpoint_id_bytes())
                    }
                    Err(e) => tracing::warn!(
                        "master seed for {} unavailable from keystore; running read-only: {e}",
                        rec.share_id
                    ),
                }
            }
            let quick_sig = rec.quick_sig;
            let mut state = self
                .open_share(
                    &key,
                    &PathBuf::from(&rec.folder),
                    vec![],
                    rec.ignore,
                    rec.last_seqno,
                    rec.paused,
                )
                .await?;
            // Restore the persisted change-signature so an unchanged folder isn't
            // re-imported on every restart.
            state.last_quick_sig = quick_sig;
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
            publishing: false,
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
    /// Whether any share is currently being published/indexed off-lock. Used by
    /// the daemon to keep emitting refresh events so the GUI's progress moves.
    pub fn publishing_active(&self) -> bool {
        self.progress.lock().map(|m| !m.is_empty()).unwrap_or(false)
    }

    pub fn list_summaries(&self) -> Vec<seed_ipc::ShareSummary> {
        let progress = self.progress.lock().map(|m| m.clone()).unwrap_or_default();
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
                // An active progress entry means we're importing the folder now;
                // report a moving percent. Otherwise fall back to the binary
                // healthy/syncing state.
                let (status, percent, indexed_bytes, index_total) = if s.paused {
                    (seed_ipc::ShareStatus::Paused, 0, 0, 0)
                } else if let Some(&(done, tot)) = progress.get(id) {
                    let pct = (done.min(tot) * 100).checked_div(tot).unwrap_or(0) as u8;
                    (seed_ipc::ShareStatus::Indexing, pct, done, tot)
                } else if s.last_seqno > 0 {
                    (seed_ipc::ShareStatus::Healthy, 100, 0, 0)
                } else {
                    (seed_ipc::ShareStatus::Syncing, 0, 0, 0)
                };
                let (online, total) = s.roster.lock().map(|r| r.counts()).unwrap_or((0, 0));
                // Count this device itself as a peer (always present + online), so
                // a share with no remote peers reads "1 of 1" rather than "0 of 0".
                let (online, total) = (online + 1, total + 1);
                seed_ipc::ShareSummary {
                    share_id: id.clone(),
                    name,
                    folder: s.folder.to_string_lossy().into_owned(),
                    role,
                    status,
                    percent,
                    online,
                    total,
                    paused: s.paused,
                    indexed_bytes,
                    index_total,
                    last_updated: s.last_updated,
                }
            })
            .collect()
    }

    /// Peer membership for a share (for the GUI's "view peers" dialog). The list
    /// is led by this device itself so it's consistent with the "1 of 1" count.
    pub fn peers(&self, share_id: &str) -> anyhow::Result<Vec<seed_ipc::PeerInfo>> {
        let state = self
            .shares
            .get(share_id)
            .ok_or_else(|| anyhow!("unknown share {share_id}"))?;
        let role = match state.key.role {
            Role::Master => seed_ipc::Role::Master,
            Role::Viewer => seed_ipc::Role::Viewer,
        };
        let mut out = vec![seed_ipc::PeerInfo {
            node_id: "This device".into(),
            role,
            online: true,
            last_seen: now_secs(),
            have_seqno: state.last_seqno,
        }];
        out.extend(state.roster.lock().map(|r| r.infos()).unwrap_or_default());
        Ok(out)
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
    /// viewer) key strings. Performs the initial publish. Convenience wrapper
    /// that runs every phase under the caller's lock — used by tests and any
    /// single-threaded caller. The daemon instead drives [`create_open`] →
    /// [`PublishJob::run`] → [`create_finish`] so the heavy publish runs without
    /// the engine lock held.
    ///
    /// [`create_open`]: Engine::create_open
    /// [`create_finish`]: Engine::create_finish
    pub async fn create_share(
        &mut self,
        folder: &Path,
        ignore: Vec<String>,
    ) -> anyhow::Result<CreatedShare> {
        let (created, job) = self.create_open(folder, ignore).await?;
        match job.run().await {
            Ok(seqno) => {
                self.create_finish(&created.share_id, seqno).await?;
                Ok(created)
            }
            Err(e) => {
                self.finish_publish(&created.share_id, None); // clear the guard
                Err(e)
            }
        }
    }

    /// Phase 1 of create: mint the key, open the replica, register the share
    /// (marked publishing), and return a [`PublishJob`] for the initial publish.
    /// The returned job borrows nothing from `self`, so the caller can drop the
    /// engine lock before running it.
    pub async fn create_open(
        &mut self,
        folder: &Path,
        ignore: Vec<String>,
    ) -> anyhow::Result<(CreatedShare, PublishJob)> {
        let key = ShareKey::generate_master().with_endpoint_id(self.node.endpoint_id_bytes());
        let share_id = key.share_id_hex();
        let state = self
            .open_share(&key, folder, vec![], ignore, 0, false)
            .await?;
        self.shares.insert(share_id.clone(), state);
        let job = self
            .make_job(&share_id)?
            .ok_or_else(|| anyhow!("internal: fresh master share is not publishable"))?;
        Ok((
            CreatedShare {
                share_id,
                master_key: key.encode(),
                viewer_key: key.encode_viewer(),
            },
            job,
        ))
    }

    /// Phase 3 of create: persist the share (routing the seed to the keystore)
    /// with the post-publish seqno and clear the publishing guard.
    pub async fn create_finish(&mut self, share_id: &str, seqno: u64) -> anyhow::Result<()> {
        let (key, folder, ignore) = {
            let s = self
                .shares
                .get(share_id)
                .ok_or_else(|| anyhow!("unknown share {share_id}"))?;
            (s.key.clone(), s.folder.clone(), s.ignore.clone())
        };
        self.persist_share(&key, &folder, ignore, seqno, false)
            .await?;
        self.finish_publish(share_id, Some(seqno));
        Ok(())
    }

    /// Build a [`PublishJob`] for a master share, marking it publishing so the
    /// reconcile loop won't start a second concurrent publish. Returns `None`
    /// for an unknown, paused, already-publishing, or viewer (non-master) share.
    fn make_job(&mut self, share_id: &str) -> anyhow::Result<Option<PublishJob>> {
        let blobs = self.node.blobs.clone();
        let author = self.author;
        let progress = self.progress.clone();
        let Some(state) = self.shares.get_mut(share_id) else {
            return Ok(None);
        };
        if state.paused || state.publishing {
            return Ok(None);
        }
        let Some(signing) = state.key.signing_key() else {
            return Ok(None); // viewer: nothing to publish
        };
        state.publishing = true;
        Ok(Some(PublishJob {
            share_id: share_id.to_string(),
            folder: state.folder.clone(),
            ignore: state.ignore.clone(),
            doc: state.doc.clone(),
            blobs,
            signing,
            share_id_bytes: state.key.share_id().to_vec(),
            author,
            last_seqno: state.last_seqno,
            progress,
        }))
    }

    /// Record the result of a [`PublishJob`] and clear its publishing guard.
    /// `new_seqno` is `Some` on success (advances the watermark + persists the
    /// seqno) and `None` on failure (just clears the guard).
    pub fn finish_publish(&mut self, share_id: &str, new_seqno: Option<u64>) {
        if let Ok(mut m) = self.progress.lock() {
            m.remove(share_id);
        }
        let Some(state) = self.shares.get_mut(share_id) else {
            return;
        };
        state.publishing = false;
        let Some(seqno) = new_seqno else {
            return;
        };
        state.last_seqno = seqno;
        state.last_updated = now_secs();
        let (ig, _) = IgnoreSet::compile(&state.ignore);
        let sig = scan::quick_signature(&state.folder, &ig);
        state.last_quick_sig = sig;
        let _ = self.db.set_seqno(share_id, seqno);
        // Persist the signature so a restart skips re-importing an unchanged
        // folder (an unpersisted sig would always look "changed" => republish).
        let _ = self.db.set_quick_sig(share_id, sig);
    }

    /// Plan the master shares that need a republish (folder changed since last
    /// publish), marking each publishing. The returned jobs borrow nothing from
    /// `self`, so the caller drops the engine lock before running them.
    pub fn plan_publishes(&mut self) -> Vec<PublishJob> {
        let due: Vec<String> = self
            .shares
            .iter()
            .filter(|(_, s)| !s.paused && !s.publishing && matches!(s.key.role, Role::Master))
            .filter(|(_, s)| {
                let (ig, _) = IgnoreSet::compile(&s.ignore);
                scan::quick_signature(&s.folder, &ig) != s.last_quick_sig
            })
            .map(|(id, _)| id.clone())
            .collect();
        due.into_iter()
            .filter_map(|id| self.make_job(&id).ok().flatten())
            .collect()
    }

    /// Reconcile every (non-paused) viewer share against its latest manifest.
    /// Returns the ids that changed. Viewers' content download/export streams
    /// via the blob store, but this still holds the engine lock for now.
    pub async fn apply_all_viewers(&mut self) -> Vec<String> {
        // Retry deleting any orphaned cross-volume blob copies whose handle iroh
        // has since released (drop the ones that are gone, keep the still-locked).
        if !self.reclaim_pending.is_empty() {
            let blobs_dir = self.node.blobs_dir.clone();
            self.reclaim_pending
                .retain(|&h| !try_reclaim_owned_data(&blobs_dir, h));
        }

        let ids: Vec<String> = self
            .shares
            .iter()
            .filter(|(_, s)| !s.paused && matches!(s.key.role, Role::Viewer))
            .map(|(id, _)| id.clone())
            .collect();
        let mut changed = Vec::new();
        for id in ids {
            if self.apply(&id).await.unwrap_or(false) {
                changed.push(id);
            }
        }
        changed
    }

    /// Persist a share to the DB, storing a master seed in the OS keystore when
    /// available (DB then holds only the seedless viewer key); otherwise falls
    /// back to storing the full key in the DB.
    async fn persist_share(
        &self,
        key: &ShareKey,
        folder: &Path,
        ignore: Vec<String>,
        last_seqno: u64,
        paused: bool,
    ) -> anyhow::Result<()> {
        let share_id = key.share_id_hex();
        let (db_key, seed_in_keyring) = match (key.role, key.seed_bytes()) {
            (Role::Master, Some(seed)) => match store_seed_bounded(&share_id, seed).await {
                Ok(()) => (key.encode_viewer(), true),
                Err(e) => {
                    tracing::warn!("OS keystore unavailable; storing key in DB instead: {e}");
                    (key.encode(), false)
                }
            },
            _ => (key.encode_viewer(), false),
        };
        self.db.upsert_share(&crate::db::ShareRecord {
            share_id,
            key: db_key,
            folder: folder.to_string_lossy().into_owned(),
            role_master: matches!(key.role, Role::Master),
            ignore,
            last_seqno,
            paused,
            seed_in_keyring,
            quick_sig: 0, // set by finish_publish after the first publish
        })?;
        Ok(())
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
        self.shares.insert(share_id.clone(), state);
        self.persist_share(&key, folder, vec![], 0, false).await?;
        Ok(share_id)
    }

    /// (Master) Scan the folder and publish a new signed manifest + content.
    /// Convenience wrapper that runs the whole publish under the caller's lock;
    /// used by tests and the manual `Publish` IPC. No-op for an unknown, paused,
    /// already-publishing, or viewer share. The daemon's reconcile loop instead
    /// uses [`plan_publishes`] + [`PublishJob::run`] + [`finish_publish`] to keep
    /// the heavy work off the engine lock.
    ///
    /// [`plan_publishes`]: Engine::plan_publishes
    /// [`finish_publish`]: Engine::finish_publish
    pub async fn publish(&mut self, share_id: &str) -> anyhow::Result<()> {
        let Some(job) = self.make_job(share_id)? else {
            return Ok(());
        };
        let result = job.run().await;
        self.finish_publish(share_id, result.as_ref().ok().copied());
        result.map(|_| ())
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

        // 4. Materialize each listed file whose local content differs.
        //    - unchanged file: skipped (steady state).
        //    - missing file: moved into place by *reference* (the store keeps
        //      only the outboard and points at the mirror file — 1x on disk),
        //      so a viewer never holds a second copy of the content.
        //    - file that exists but diverges (edited/corrupted): its referenced
        //      blob is now stale and can't self-repair locally, so we re-fetch
        //      the verified bytes from a peer and rewrite it — fully automatic.
        let endpoint = self.node.endpoint.clone();
        for fe in &manifest.files {
            let target = state.folder.join(rel_to_native(&fe.path));
            if file_matches(&target, &fe.hash) {
                continue;
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let arr: [u8; 32] = fe.hash.as_slice().try_into().context("bad hash len")?;
            let hash = Hash::from(arr);

            if !target.exists() {
                // First sync (owned blob) or a deleted file: move the blob into
                // place by reference. No GC is enabled, so no protecting tag is
                // needed during the move.
                if let Err(e) = self
                    .node
                    .blobs
                    .blobs()
                    .export_with_opts(ExportOptions {
                        hash,
                        mode: ExportMode::TryReference,
                        target: target.clone(),
                    })
                    .await
                {
                    tracing::debug!("reference-export {} failed; will self-heal: {e}", fe.path);
                }
            }

            if file_matches(&target, &fe.hash) {
                // The export referenced the mirror file (entry is now External).
                // Across volumes it left the downloaded copy behind; queue it for
                // reclaim so the viewer ends up 1× on any drive layout (iroh holds
                // the file handle briefly, so the deletion is retried below).
                self.reclaim_pending.insert(hash);
            } else {
                // Either the export couldn't restore it, or an existing file was
                // edited/corrupted. Do NOT re-export here: when the store already
                // references this same path, exporting would copy the corrupt
                // file onto itself. Re-fetch a verified copy from a peer instead.
                let providers = peer_providers(&state.key, &state.roster);
                self_heal_file(&endpoint, &providers, hash, &target)
                    .await
                    .with_context(|| format!("self-heal {}", fe.path))?;
            }
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
        crate::secrets::delete_seed(share_id);
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
