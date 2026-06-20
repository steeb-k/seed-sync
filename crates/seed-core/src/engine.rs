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
pub(crate) struct PeerRoster {
    peers: HashMap<String, PeerEntry>,
}

#[derive(Default)]
struct PeerEntry {
    neighbor: bool,
    last_seen: i64,
    /// Filled from presence broadcasts (gossip). Absent until a peer announces.
    name: Option<String>,
    role: Option<seed_ipc::Role>,
    seqno: u64,
    percent: u8,
}

impl PeerRoster {
    pub(crate) fn note(&mut self, id: &str, neighbor: Option<bool>) {
        let e = self.peers.entry(id.to_string()).or_default();
        e.last_seen = now_secs();
        if let Some(n) = neighbor {
            e.neighbor = n;
        }
    }

    /// Fold a presence broadcast into the roster: refresh name/role/health and
    /// mark the peer heard-from (online for the TTL).
    pub(crate) fn note_presence(&mut self, id: &str, p: crate::presence::Presence) {
        let e = self.peers.entry(id.to_string()).or_default();
        e.last_seen = now_secs();
        e.name = Some(p.name);
        e.role = Some(p.role);
        e.seqno = p.seqno;
        e.percent = p.percent;
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
                name: e.name.clone(),
                role: e.role.unwrap_or(seed_ipc::Role::Viewer),
                online: self.is_online(e, now),
                last_seen: e.last_seen,
                have_seqno: e.seqno,
                percent: e.percent,
            })
            .collect()
    }
}

use crate::identity::{Role, ShareKey};
use crate::node::IrohNode;
use crate::scan::{self, IgnoreSet};

/// All reserved doc keys share the `\x00` control prefix so they never collide
/// with user file paths (relative POSIX strings, never starting with NUL).
const CONTROL_PREFIX: u8 = 0;
/// Replicated, master-written ignore list (CBOR `Vec<String>`), LWW-merged across
/// masters. Viewers read it so they honor what a master chose not to sync.
const IGNORE_KEY: &[u8] = b"\x00ignore";
/// Prefix for empty-file markers: `\x00e/<relpath>` with a non-empty marker value.
/// iroh-docs filters 0-byte entries out of queries as deletion markers, so a real
/// empty file can't ride a normal entry — it gets its own (non-empty) control key.
const EMPTY_PREFIX: &[u8] = b"\x00e/";

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Default display name when the user hasn't set one: the machine hostname.
fn default_device_name() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Seed device".to_string())
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

/// One file's state as seen in the merged replica (the `R` side of the merge).
struct RemoteEntry {
    /// 32-byte BLAKE3 content hash. `Hash::EMPTY` bytes for an empty file.
    hash: Vec<u8>,
    size: u64,
    /// iroh-docs record timestamp (micros since epoch); LWW conflict tiebreak.
    ts: u64,
}

/// One file's state on local disk (the `L` side of the merge).
struct LocalEntry {
    hash: Vec<u8>,
    size: u64,
    /// Absolute path, present only when discovered by a full hashing scan (so the
    /// reconcile can import it). `None` when `L` is inferred from the base index.
    abs: Option<PathBuf>,
}

/// Read the merged file view from the doc: latest-per-key, with deletion markers
/// already excluded by the query (so a tombstoned file is simply absent). Normal
/// keys carry content; `\x00e/<path>` keys mark empty files; other control keys
/// (e.g. `\x00ignore`) are skipped.
async fn read_remote_files(doc: &Doc) -> anyhow::Result<HashMap<String, RemoteEntry>> {
    let mut out = HashMap::new();
    let mut s = std::pin::pin!(doc.get_many(Query::single_latest_per_key()).await?);
    while let Some(e) = s.next().await {
        let e = e?;
        let key = e.key();
        if key.first() == Some(&CONTROL_PREFIX) {
            if let Some(rel) = key.strip_prefix(EMPTY_PREFIX) {
                if let Ok(path) = std::str::from_utf8(rel) {
                    out.insert(
                        path.to_string(),
                        RemoteEntry {
                            hash: Hash::EMPTY.as_bytes().to_vec(),
                            size: 0,
                            ts: e.timestamp(),
                        },
                    );
                }
            }
            continue;
        }
        let Ok(path) = std::str::from_utf8(key) else {
            continue;
        };
        out.insert(
            path.to_string(),
            RemoteEntry {
                hash: e.content_hash().as_bytes().to_vec(),
                size: e.content_len(),
                ts: e.timestamp(),
            },
        );
    }
    Ok(out)
}

/// Read the replicated ignore list (`\x00ignore`), if a master has published one
/// and its content has arrived. `None` means "use the locally-configured list".
async fn read_ignore_list(doc: &Doc, blobs: &FsStore) -> anyhow::Result<Option<Vec<String>>> {
    let Some(entry) = doc
        .get_one(Query::single_latest_per_key().key_exact(IGNORE_KEY))
        .await?
    else {
        return Ok(None);
    };
    let hash = entry.content_hash();
    if !blobs.blobs().has(hash).await? {
        return Ok(None);
    }
    let bytes = blobs.blobs().get_bytes(hash).await?;
    let list: Vec<String> =
        ciborium::from_reader(bytes.as_ref()).context("decode replicated ignore list")?;
    Ok(Some(list))
}

/// Local file mtime as micros since the Unix epoch (for LWW vs. a doc timestamp).
fn mtime_micros(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Convert a 32-byte hash slice into an iroh [`Hash`].
fn to_hash(bytes: &[u8]) -> anyhow::Result<Hash> {
    let arr: [u8; 32] = bytes.try_into().context("bad hash len")?;
    Ok(Hash::from(arr))
}

/// What a [`ReconcileJob::run`] decided, applied back into engine state under the
/// lock by [`Engine::finish_reconcile`]: index mutations to persist, the new
/// folder signature, this node's recomputed health, and any blob copies to reclaim.
pub struct ReconcileOutcome {
    changed: bool,
    health: u8,
    new_quick_sig: u64,
    index_sets: Vec<(String, Vec<u8>)>,
    index_dels: Vec<String>,
    reclaim: Vec<Hash>,
}

impl ReconcileOutcome {
    /// Whether this pass mutated the local folder or the replica.
    pub fn changed(&self) -> bool {
        self.changed
    }
}

/// A self-contained unit of reconcile work holding *cloned* iroh handles, so the
/// heavy part (hashing the folder, streaming blobs in/out) runs with **no engine
/// lock held**. Produced under a brief lock by [`Engine::make_reconcile_job`] or
/// [`Engine::create_open`], run via [`ReconcileJob::run`], then committed by
/// [`Engine::finish_reconcile`].
///
/// It performs one bidirectional, last-writer-wins reconcile between the local
/// folder (`L`), the merged doc replica (`R`), and the per-path base index (`B` =
/// what we last reconciled). A **master** writes local changes back into the
/// replica and materializes remote ones; a **viewer** holds a read-only doc
/// capability and only mirrors the replica to disk (local edits are reverted).
pub struct ReconcileJob {
    share_id: String,
    folder: PathBuf,
    is_master: bool,
    configured_ignore: Vec<String>,
    doc: Doc,
    blobs: FsStore,
    author: AuthorId,
    endpoint: Endpoint,
    providers: Vec<EndpointId>,
    base: HashMap<String, Vec<u8>>,
    last_quick_sig: u64,
    progress: Arc<StdMutex<HashMap<String, (u64, u64)>>>,
}

impl ReconcileJob {
    pub fn share_id(&self) -> &str {
        &self.share_id
    }

    fn set_progress(&self, done: u64, total: u64) {
        if let Ok(mut m) = self.progress.lock() {
            m.insert(self.share_id.clone(), (done, total));
        }
    }

    /// Import a local file into the blob store (by reference) and write the
    /// matching doc entry. Empty files ride the `\x00e/<path>` control keyspace
    /// (iroh-docs filters 0-byte entries out as deletions). Returns the stored
    /// 32-byte hash.
    async fn import_one(&self, path: &str, abs: &Path) -> anyhow::Result<Vec<u8>> {
        let tag = self
            .blobs
            .blobs()
            .add_path_with_opts(AddPathOptions {
                path: abs.to_path_buf(),
                format: BlobFormat::Raw,
                mode: ImportMode::TryReference,
            })
            .temp_tag()
            .await
            .with_context(|| format!("import {}", abs.display()))?;
        let hash = tag.hash();
        let size = match self.blobs.blobs().status(hash).await {
            Ok(iroh_blobs::api::proto::BlobStatus::Complete { size }) => size,
            _ => 0,
        };
        if size == 0 {
            // Empty file: mark it in the control keyspace and clear any stale
            // normal entry left from when it had content.
            let mut k = EMPTY_PREFIX.to_vec();
            k.extend_from_slice(path.as_bytes());
            self.doc
                .set_bytes(self.author, k, vec![1u8])
                .await
                .with_context(|| format!("mark empty {path}"))?;
            let _ = self.doc.del(self.author, path.as_bytes().to_vec()).await;
            drop(tag);
            return Ok(Hash::EMPTY.as_bytes().to_vec());
        }
        self.doc
            .set_hash(self.author, path.as_bytes().to_vec(), hash, size)
            .await
            .with_context(|| format!("set doc entry {path}"))?;
        let mut ek = EMPTY_PREFIX.to_vec();
        ek.extend_from_slice(path.as_bytes());
        let _ = self.doc.del(self.author, ek).await;
        drop(tag);
        Ok(hash.as_bytes().to_vec())
    }

    /// Tombstone a file (and its empty-marker) in the replica.
    async fn tombstone(&self, path: &str) {
        let _ = self.doc.del(self.author, path.as_bytes().to_vec()).await;
        let mut ek = EMPTY_PREFIX.to_vec();
        ek.extend_from_slice(path.as_bytes());
        let _ = self.doc.del(self.author, ek).await;
    }

    /// Materialize a remote file to disk if its content has arrived. Returns
    /// `Ok(true)` once the file on disk matches `hash_bytes` (or is created empty),
    /// `Ok(false)` if the content is still downloading. Pushes a hash onto
    /// `reclaim` when a cross-volume reference export left an owned copy behind.
    async fn materialize(
        &self,
        path: &str,
        hash_bytes: &[u8],
        size: u64,
        reclaim: &mut Vec<Hash>,
    ) -> anyhow::Result<bool> {
        let target = self.folder.join(rel_to_native(path));
        if size == 0 {
            let present = std::fs::metadata(&target)
                .map(|m| m.is_file() && m.len() == 0)
                .unwrap_or(false);
            if !present {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::File::create(&target)?;
            }
            return Ok(true);
        }
        if file_matches(&target, hash_bytes) {
            return Ok(true);
        }
        let hash = to_hash(hash_bytes)?;
        if !self.blobs.blobs().has(hash).await? {
            return Ok(false); // content still downloading
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if !target.exists() {
            if let Err(e) = self
                .blobs
                .blobs()
                .export_with_opts(ExportOptions {
                    hash,
                    mode: ExportMode::TryReference,
                    target: target.clone(),
                })
                .await
            {
                tracing::debug!("reference-export {path} failed; will self-heal: {e}");
            }
        }
        if file_matches(&target, hash_bytes) {
            reclaim.push(hash);
            Ok(true)
        } else {
            // Existing file diverged (edited/corrupted): re-fetch verified bytes.
            self_heal_file(&self.endpoint, &self.providers, hash, &target)
                .await
                .with_context(|| format!("self-heal {path}"))?;
            Ok(true)
        }
    }

    /// Run one bidirectional reconcile pass. Touches only cloned handles + disk, so
    /// it is safe to call without the engine lock.
    pub async fn run(&self) -> anyhow::Result<ReconcileOutcome> {
        // 1. Effective ignore set: the replicated `\x00ignore` is authoritative
        //    (so viewers honor what a master ignored, e.g. don't delete those
        //    files). A master (re)publishes its configured list when it drifts.
        let live_ignore = read_ignore_list(&self.doc, &self.blobs).await?;
        let effective_ignore = if self.is_master {
            if live_ignore.as_deref() != Some(self.configured_ignore.as_slice()) {
                let mut cbor = Vec::new();
                ciborium::into_writer(&self.configured_ignore, &mut cbor)
                    .context("encode ignore list")?;
                self.doc
                    .set_bytes(self.author, IGNORE_KEY.to_vec(), cbor)
                    .await
                    .context("publish ignore list")?;
            }
            self.configured_ignore.clone()
        } else {
            live_ignore.unwrap_or_else(|| self.configured_ignore.clone())
        };
        let (ignore_set, _bad) = IgnoreSet::compile(&effective_ignore);

        // 2. Merged remote view.
        let remote = read_remote_files(&self.doc).await?;

        // 3. Local view. Hashing the whole folder is costly, so only do it when the
        //    cheap (path,size,mtime) signature changed since last reconcile;
        //    otherwise the on-disk content equals our recorded base. Both roles
        //    scan: a master to publish local edits, a viewer to detect (and revert)
        //    local drift.
        let quick_sig = scan::quick_signature(&self.folder, &ignore_set);
        let do_scan = quick_sig != self.last_quick_sig;
        let local: HashMap<String, LocalEntry> = if do_scan {
            let mut m = HashMap::new();
            for sf in scan::scan(&self.folder, &ignore_set)? {
                m.insert(
                    sf.entry.path.clone(),
                    LocalEntry {
                        hash: sf.entry.hash,
                        size: sf.entry.size,
                        abs: Some(sf.abs_path),
                    },
                );
            }
            m
        } else {
            self.base
                .iter()
                .map(|(p, h)| {
                    (
                        p.clone(),
                        LocalEntry {
                            hash: h.clone(),
                            size: 0,
                            abs: None,
                        },
                    )
                })
                .collect()
        };

        // Seed import progress so the GUI shows a moving percent on a big first
        // import (masters only; cleared in finish_reconcile).
        if self.is_master && do_scan {
            let total: u64 = local.values().map(|l| l.size).sum();
            self.set_progress(0, total);
        }

        let mut keys: HashSet<String> = HashSet::new();
        keys.extend(remote.keys().cloned());
        keys.extend(local.keys().cloned());
        keys.extend(self.base.keys().cloned());

        let mut index_sets: Vec<(String, Vec<u8>)> = Vec::new();
        let mut index_dels: Vec<String> = Vec::new();
        let mut reclaim: Vec<Hash> = Vec::new();
        let mut changed = false;
        let mut imported_bytes: u64 = 0;

        for path in keys {
            let l = local.get(&path);
            let r = remote.get(&path);
            let b = self.base.get(&path);

            match (l, r) {
                // Gone from disk and replica: drop any base record.
                (None, None) => {
                    if b.is_some() {
                        index_dels.push(path);
                    }
                }

                // On disk, absent from the replica.
                (Some(le), None) => {
                    if !self.is_master {
                        // Viewer: not in the merged view → revert (delete) it.
                        let target = self.folder.join(rel_to_native(&path));
                        let _ = std::fs::remove_file(&target);
                        if b.is_some() {
                            index_dels.push(path);
                        }
                        changed = true;
                    } else if b.map(|bh| bh == &le.hash).unwrap_or(false) {
                        // Master, unchanged since base, now gone from replica →
                        // a remote delete: remove it locally.
                        let target = self.folder.join(rel_to_native(&path));
                        let _ = std::fs::remove_file(&target);
                        index_dels.push(path);
                        changed = true;
                    } else if let Some(abs) = le.abs.as_ref() {
                        // Master, brand-new local file (or locally edited after a
                        // remote delete): publish it.
                        let h = self.import_one(&path, abs).await?;
                        imported_bytes += le.size;
                        self.set_progress(imported_bytes, imported_bytes);
                        index_sets.push((path, h));
                        changed = true;
                    }
                }

                // In the replica, absent from disk.
                (None, Some(re)) => {
                    if self.is_master && do_scan && b.map(|bh| bh == &re.hash).unwrap_or(false) {
                        // Master, full scan saw it genuinely gone while base+replica
                        // agreed → the user deleted it: propagate the tombstone.
                        self.tombstone(&path).await;
                        index_dels.push(path);
                        changed = true;
                    } else if self
                        .materialize(&path, &re.hash, re.size, &mut reclaim)
                        .await?
                    {
                        index_sets.push((path, re.hash.clone()));
                        changed = true;
                    }
                }

                // Present on both sides.
                (Some(le), Some(re)) => {
                    if le.hash == re.hash {
                        if b != Some(&le.hash) {
                            index_sets.push((path, le.hash.clone()));
                        }
                        continue;
                    }
                    if !self.is_master {
                        // Viewer: replica wins, always.
                        if self.materialize(&path, &re.hash, re.size, &mut reclaim).await? {
                            index_sets.push((path, re.hash.clone()));
                            changed = true;
                        }
                        continue;
                    }
                    // Master three-way merge.
                    match b {
                        Some(bh) if bh == &le.hash => {
                            // Local untouched, remote changed → take remote.
                            if self.materialize(&path, &re.hash, re.size, &mut reclaim).await? {
                                index_sets.push((path, re.hash.clone()));
                                changed = true;
                            }
                        }
                        Some(bh) if bh == &re.hash => {
                            // Remote untouched, local changed → publish local.
                            if let Some(abs) = le.abs.as_ref() {
                                let h = self.import_one(&path, abs).await?;
                                index_sets.push((path, h));
                                changed = true;
                            }
                        }
                        _ => {
                            // Both changed (or unknown base) → last-writer-wins.
                            let local_ts =
                                le.abs.as_ref().map(|a| mtime_micros(a)).unwrap_or(0);
                            if local_ts >= re.ts {
                                if let Some(abs) = le.abs.as_ref() {
                                    let h = self.import_one(&path, abs).await?;
                                    index_sets.push((path, h));
                                    changed = true;
                                }
                            } else if self
                                .materialize(&path, &re.hash, re.size, &mut reclaim)
                                .await?
                            {
                                index_sets.push((path, re.hash.clone()));
                                changed = true;
                            }
                        }
                    }
                }
            }
        }

        // Tidy now-empty directories a viewer/master delete may have left behind.
        prune_empty_dirs(&self.folder);

        // Health: present / total bytes of the merged desired view. A master is the
        // source, so always 100.
        let mut total_bytes: u64 = 0;
        let mut present_bytes: u64 = 0;
        for re in remote.values() {
            total_bytes += re.size;
            if re.size == 0 || self.blobs.blobs().has(to_hash(&re.hash)?).await? {
                present_bytes += re.size;
            }
        }
        let health = if self.is_master {
            100
        } else {
            (present_bytes.min(total_bytes) * 100)
                .checked_div(total_bytes)
                .unwrap_or(100) as u8
        };

        // Recompute the signature *after* our disk writes so the next tick sees a
        // settled folder rather than re-scanning our own changes.
        let new_quick_sig = scan::quick_signature(&self.folder, &ignore_set);

        Ok(ReconcileOutcome {
            changed,
            health,
            new_quick_sig,
            index_sets,
            index_dels,
            reclaim,
        })
    }
}

/// In-memory per-share state.
struct ShareState {
    key: ShareKey,
    folder: PathBuf,
    doc: Doc,
    ignore: Vec<String>,
    /// Local "generation" counter, bumped on each changed reconcile and broadcast
    /// in presence so peers can see this node moving (no longer a trust watermark).
    last_seqno: u64,
    /// Cheap (path,size,mtime) signature of the folder after the last reconcile;
    /// lets the next pass skip a full hashing scan when nothing changed on disk.
    last_quick_sig: u64,
    /// When paused, the reconcile loop skips this share.
    paused: bool,
    /// Set while a [`ReconcileJob`] for this share is running off-lock, so the
    /// reconcile loop doesn't start a second concurrent publish of it.
    publishing: bool,
    /// Live peer membership, updated by the doc event task + presence gossip.
    roster: Arc<StdMutex<PeerRoster>>,
    /// Unix seconds of the last successful publish (master) or applied update
    /// (viewer); 0 if none yet this session.
    last_updated: i64,
    /// This member's own sync health 0..=100, broadcast in presence. A master is
    /// always 100 (content source); a viewer's is set by [`Engine::apply`].
    health: u8,
    /// Presence gossip for this share (broadcasts our name + health, receives
    /// peers'). `None` if the gossip subscribe failed. Aborted on drop.
    presence: Option<crate::presence::PresenceHandle>,
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
    /// This device's display name, broadcast in presence and shown to other
    /// members. Cached from `settings["device_name"]` (default: hostname) so
    /// per-tick broadcasts and `peers()` don't hit the DB. One global name.
    device_name: StdMutex<String>,
}

impl Engine {
    /// Bootstrap the engine against a data directory, reloading any persisted
    /// shares (so the daemon is restart-safe).
    pub async fn new(data_dir: &Path) -> anyhow::Result<Self> {
        let node = IrohNode::spawn(data_dir).await?;
        let author = node.docs_api().author_default().await?;
        let db = crate::db::Db::open(&data_dir.join("state.db"))?;
        let device_name = db
            .get_setting("device_name")?
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(default_device_name);
        let mut engine = Self {
            node,
            author,
            shares: HashMap::new(),
            db,
            progress: Arc::new(StdMutex::new(HashMap::new())),
            reclaim_pending: std::collections::HashSet::new(),
            device_name: StdMutex::new(device_name),
        };
        engine.reload_shares().await?;
        Ok(engine)
    }

    /// This device's display name (cached; default = hostname).
    pub fn device_name(&self) -> String {
        self.device_name
            .lock()
            .map(|n| n.clone())
            .unwrap_or_else(|_| default_device_name())
    }

    /// Set + persist this device's display name (global — shown in every share).
    /// An empty name resets to the hostname default. Returns the resolved name.
    pub fn set_device_name(&self, name: &str) -> anyhow::Result<String> {
        let name = name.trim();
        let resolved = if name.is_empty() {
            default_device_name()
        } else {
            name.to_string()
        };
        self.db.set_setting("device_name", &resolved)?;
        if let Ok(mut n) = self.device_name.lock() {
            *n = resolved.clone();
        }
        Ok(resolved)
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
                        // Preserve the creating node's endpoint id carried in the
                        // stored (seedless) key: a master added from someone else's
                        // master key holds *their* id as its only bootstrap hint, so
                        // stamping our own id here would strand it after a restart.
                        // Fall back to our own id only for legacy keys that carried
                        // none (a self-created master's key already holds its own).
                        let eid = key.endpoint_id().unwrap_or(self.node.endpoint_id_bytes());
                        key = ShareKey::from_master_seed(seed).with_endpoint_id(eid);
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

        // If no explicit bootstrap was given, any node added from a share key can
        // reach the *creating* node by its endpoint id (carried in the key) via n0
        // DNS discovery — build an address with just the id and let discovery
        // resolve it. This applies to a master added from a master key just as much
        // as to a viewer (multi-master): both must dial the creator for doc sync.
        // The creating master's own key carries its own id, so skip dialing
        // ourselves.
        let mut bootstrap = bootstrap;
        if bootstrap.is_empty() {
            if let Some(eid) = key.endpoint_id() {
                if let Ok(pk) = iroh::EndpointId::from_bytes(&eid) {
                    if pk != self.node.endpoint.id() {
                        tracing::info!("no bootstrap given; using endpoint-id discovery");
                        bootstrap.push(iroh::EndpointAddr::new(pk));
                    }
                }
            }
        }

        // Subscribe (keeps live sync alive + feeds the peer roster) and register
        // the namespace for serving + connect to any bootstrap peers.
        let roster = Arc::new(StdMutex::new(PeerRoster::default()));
        spawn_event_task(&doc, roster.clone()).await?;

        // Presence: a per-share gossip topic carrying each member's name + health.
        // Bootstrap it with the same peers as the doc (the master endpoint id + any
        // explicit bootstrap addrs), minus ourselves — a master's key carries its
        // own endpoint id, and dialing yourself warns. Best-effort: a subscribe
        // failure must not fail opening the share.
        let self_id = self.node.endpoint.id();
        let mut presence_bootstrap: Vec<EndpointId> = bootstrap.iter().map(|a| a.id).collect();
        if let Some(eid) = key.endpoint_id() {
            if let Ok(pk) = EndpointId::from_bytes(&eid) {
                presence_bootstrap.push(pk);
            }
        }
        presence_bootstrap.retain(|id| *id != self_id);
        presence_bootstrap.sort();
        presence_bootstrap.dedup();
        let presence = match crate::presence::spawn_presence(
            &self.node.gossip,
            crate::presence::presence_topic(&key.share_id()),
            presence_bootstrap,
            self_id,
            roster.clone(),
        )
        .await
        {
            Ok(h) => Some(h),
            Err(e) => {
                tracing::warn!("presence gossip subscribe failed for share: {e}");
                None
            }
        };

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
            // Master is always 100 (source); a viewer's is recomputed by apply().
            health: if matches!(key.role, Role::Master) {
                100
            } else {
                0
            },
            presence,
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
                // report a moving percent. Otherwise derive health from whether
                // this node holds all of the merged view's content: a master is
                // the source (always 100); a viewer reports its present/total %.
                let (status, percent, indexed_bytes, index_total) = if s.paused {
                    (seed_ipc::ShareStatus::Paused, 0, 0, 0)
                } else if let Some(&(done, tot)) = progress.get(id) {
                    let pct = (done.min(tot) * 100).checked_div(tot).unwrap_or(0) as u8;
                    (seed_ipc::ShareStatus::Indexing, pct, done, tot)
                } else if s.health >= 100 {
                    (seed_ipc::ShareStatus::Healthy, 100, 0, 0)
                } else {
                    (seed_ipc::ShareStatus::Syncing, s.health, 0, 0)
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
            name: Some(self.device_name()),
            role,
            online: true,
            last_seen: now_secs(),
            have_seqno: state.last_seqno,
            percent: if matches!(state.key.role, Role::Master) {
                100
            } else {
                state.health
            },
        }];
        out.extend(state.roster.lock().map(|r| r.infos()).unwrap_or_default());
        Ok(out)
    }

    /// Build this tick's presence broadcasts — one per share with a live presence
    /// channel. Call under the engine lock (cheap: clones the gossip sender +
    /// pre-encodes); send the results off-lock via [`PresenceBroadcast::send`].
    pub fn presence_broadcasts(&self) -> Vec<crate::presence::PresenceBroadcast> {
        let name = self.device_name();
        let ts = now_secs();
        let mut out = Vec::new();
        for s in self.shares.values() {
            let Some(h) = s.presence.as_ref() else {
                continue;
            };
            let (role, percent) = match s.key.role {
                Role::Master => (seed_ipc::Role::Master, 100),
                Role::Viewer => (seed_ipc::Role::Viewer, s.health),
            };
            let p = crate::presence::Presence {
                v: crate::presence::PRESENCE_V,
                name: name.clone(),
                role,
                seqno: s.last_seqno,
                percent,
                ts,
            };
            out.push(crate::presence::PresenceBroadcast::new(
                h.sender.clone(),
                &p,
            ));
        }
        out
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
    /// viewer) key strings. Performs the initial reconcile (imports the folder
    /// into the replica). Convenience wrapper that runs every phase under the
    /// caller's lock — used by tests. The daemon instead drives [`create_open`] →
    /// [`ReconcileJob::run`] → [`finish_reconcile`] so the heavy import runs
    /// without the engine lock held.
    ///
    /// [`create_open`]: Engine::create_open
    /// [`finish_reconcile`]: Engine::finish_reconcile
    pub async fn create_share(
        &mut self,
        folder: &Path,
        ignore: Vec<String>,
    ) -> anyhow::Result<CreatedShare> {
        let (created, job) = self.create_open(folder, ignore).await?;
        let outcome = job.run().await;
        self.finish_reconcile(&created.share_id, outcome.ok());
        Ok(created)
    }

    /// Phase 1 of create: mint the key, open the replica, persist the share, and
    /// return a [`ReconcileJob`] for the initial import. The returned job borrows
    /// nothing from `self`, so the caller can drop the engine lock before running.
    pub async fn create_open(
        &mut self,
        folder: &Path,
        ignore: Vec<String>,
    ) -> anyhow::Result<(CreatedShare, ReconcileJob)> {
        let key = ShareKey::generate_master().with_endpoint_id(self.node.endpoint_id_bytes());
        let share_id = key.share_id_hex();
        let state = self
            .open_share(&key, folder, vec![], ignore.clone(), 0, false)
            .await?;
        self.shares.insert(share_id.clone(), state);
        self.persist_share(&key, folder, ignore, 0, false).await?;
        let job = self
            .make_reconcile_job(&share_id)?
            .ok_or_else(|| anyhow!("internal: fresh master share is not reconcilable"))?;
        Ok((
            CreatedShare {
                share_id,
                master_key: key.encode(),
                viewer_key: key.encode_viewer(),
            },
            job,
        ))
    }

    /// Build a [`ReconcileJob`] for a share, marking it busy so the reconcile loop
    /// won't start a second concurrent pass. Returns `None` for an unknown, paused,
    /// or already-running share. A first-tick debounce gates whether local edits
    /// are treated as authoritative this round (so a file mid-copy isn't imported).
    pub fn make_reconcile_job(&mut self, share_id: &str) -> anyhow::Result<Option<ReconcileJob>> {
        let blobs = self.node.blobs.clone();
        let endpoint = self.node.endpoint.clone();
        let author = self.author;
        let progress = self.progress.clone();
        let base = self.db.get_index(share_id).unwrap_or_default();
        let Some(state) = self.shares.get_mut(share_id) else {
            return Ok(None);
        };
        if state.paused || state.publishing {
            return Ok(None);
        }
        let is_master = matches!(state.key.role, Role::Master);
        let providers = peer_providers(&state.key, &state.roster);
        state.publishing = true;
        Ok(Some(ReconcileJob {
            share_id: share_id.to_string(),
            folder: state.folder.clone(),
            is_master,
            configured_ignore: state.ignore.clone(),
            doc: state.doc.clone(),
            blobs,
            author,
            endpoint,
            providers,
            base,
            last_quick_sig: state.last_quick_sig,
            progress,
        }))
    }

    /// Commit a [`ReconcileJob`] result and clear its busy guard. `outcome` is
    /// `Some` on success (persists the index mutations, health, and signature) and
    /// `None` on failure (just clears the guard).
    pub fn finish_reconcile(&mut self, share_id: &str, outcome: Option<ReconcileOutcome>) {
        if let Ok(mut m) = self.progress.lock() {
            m.remove(share_id);
        }
        let Some(out) = outcome else {
            if let Some(state) = self.shares.get_mut(share_id) {
                state.publishing = false;
            }
            return;
        };
        // Persist index mutations (path -> last-reconciled hash) outside the share
        // borrow.
        for (path, hash) in &out.index_sets {
            let _ = self.db.set_index_entry(share_id, path, hash);
        }
        for path in &out.index_dels {
            let _ = self.db.del_index_entry(share_id, path);
        }
        for h in out.reclaim {
            self.reclaim_pending.insert(h);
        }
        let Some(state) = self.shares.get_mut(share_id) else {
            return;
        };
        state.publishing = false;
        state.health = out.health;
        state.last_quick_sig = out.new_quick_sig;
        let _ = self.db.set_quick_sig(share_id, out.new_quick_sig);
        if out.changed {
            state.last_seqno = state.last_seqno.saturating_add(1);
            state.last_updated = now_secs();
            let _ = self.db.set_seqno(share_id, state.last_seqno);
        }
    }

    /// Reconcile every non-paused share under the engine lock (convenience for
    /// tests and the manual Publish IPC). Returns the ids that changed. The daemon
    /// loop instead uses [`make_reconcile_job`] + [`ReconcileJob::run`] +
    /// [`finish_reconcile`] to keep the heavy work off the lock.
    ///
    /// [`make_reconcile_job`]: Engine::make_reconcile_job
    /// [`finish_reconcile`]: Engine::finish_reconcile
    pub async fn reconcile_all(&mut self) -> Vec<String> {
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
            .filter(|(_, s)| !s.paused)
            .map(|(id, _)| id.clone())
            .collect();
        let mut changed = Vec::new();
        for id in ids {
            if self.reconcile(&id).await.unwrap_or(false) {
                changed.push(id);
            }
        }
        changed
    }

    /// Reconcile a single share under the engine lock.
    pub async fn reconcile(&mut self, share_id: &str) -> anyhow::Result<bool> {
        let Some(job) = self.make_reconcile_job(share_id)? else {
            return Ok(false);
        };
        match job.run().await {
            Ok(o) => {
                let changed = o.changed;
                self.finish_reconcile(share_id, Some(o));
                Ok(changed)
            }
            Err(e) => {
                self.finish_reconcile(share_id, None);
                Err(e)
            }
        }
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

    /// Ids of every share the engine currently holds (for the daemon loop to
    /// schedule per-share reconciles).
    pub fn share_ids(&self) -> Vec<String> {
        self.shares.keys().cloned().collect()
    }

    /// Retry deleting any orphaned cross-volume blob copies whose file handle iroh
    /// has since released. Call once per reconcile tick (cheap when empty).
    pub fn retry_reclaims(&mut self) {
        if self.reclaim_pending.is_empty() {
            return;
        }
        let blobs_dir = self.node.blobs_dir.clone();
        self.reclaim_pending
            .retain(|&h| !try_reclaim_owned_data(&blobs_dir, h));
    }

    /// Force a reconcile of one share (manual `Publish` IPC / tests). Thin wrapper
    /// over [`reconcile`](Engine::reconcile); no-op for a paused/unknown share.
    pub async fn publish(&mut self, share_id: &str) -> anyhow::Result<()> {
        self.reconcile(share_id).await.map(|_| ())
    }

    /// Back-compat alias used by tests: reconcile one share. In the multi-master
    /// model every node reconciles the same way, so this is just [`reconcile`].
    ///
    /// [`reconcile`]: Engine::reconcile
    pub async fn apply(&mut self, share_id: &str) -> anyhow::Result<bool> {
        self.reconcile(share_id).await
    }

    /// Back-compat alias used by tests: reconcile every non-paused share.
    pub async fn apply_all_viewers(&mut self) -> Vec<String> {
        self.reconcile_all().await
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
