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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use bao_tree::io::BaoContentItem;
use bao_tree::{ChunkNum, ChunkRanges};
use futures_lite::StreamExt;
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, EndpointId};
use iroh_blobs::api::blobs::{AddPathOptions, ExportMode, ExportOptions, ImportMode};
use iroh_blobs::api::downloader::{DownloadRequest, Downloader, SplitStrategy};
use iroh_blobs::get::request::{get_blob, GetBlobItem};
use iroh_blobs::protocol::GetRequest;
use iroh_blobs::{store::fs::FsStore, BlobFormat, Hash};
use iroh_docs::{
    api::Doc, engine::LiveEvent, store::Query, sync::Capability, AuthorId, NamespaceId,
    NamespaceSecret,
};
use rand::seq::SliceRandom;
use rand::Rng;

/// How long since a peer was last heard from before we consider it offline.
/// How long since the last sign of life (presence heartbeat, doc sync activity,
/// or a neighbor-up) before we consider a peer offline. Presence is broadcast
/// about every 3s, so this tolerates several missed beats while still flipping a
/// peer offline within a few seconds of it actually leaving.
const PEER_ONLINE_TTL_SECS: i64 = 20;

/// Tracks the peers seen for one share, fed by the doc's live events + presence
/// gossip. `total` is every distinct peer seen since the daemon started; `online`
/// is those heard-from within [`PEER_ONLINE_TTL_SECS`].
///
/// Online is deliberately **heartbeat-based, not a sticky "is a neighbor" flag**:
/// when a peer's daemon is force-stopped, iroh-gossip does not always deliver a
/// clean `NeighborDown`, so trusting a connected flag left peers wedged "online"
/// forever. Instead, every sign of life refreshes `last_seen` and the entry ages
/// out on its own; a `NeighborDown`, when it *does* arrive, force-ages the entry
/// so it flips offline at once.
#[derive(Default)]
pub(crate) struct PeerRoster {
    peers: HashMap<String, PeerEntry>,
}

#[derive(Default)]
struct PeerEntry {
    last_seen: i64,
    /// Filled from presence broadcasts (gossip). Absent until a peer announces.
    name: Option<String>,
    role: Option<seed_ipc::Role>,
    seqno: u64,
    percent: u8,
}

impl PeerRoster {
    /// Record activity for a peer. `neighbor` distinguishes gossip membership
    /// transitions: `Some(true)` = NeighborUp, `Some(false)` = NeighborDown,
    /// `None` = other evidence of life (remote insert / sync finished / presence).
    pub(crate) fn note(&mut self, id: &str, neighbor: Option<bool>) {
        let e = self.peers.entry(id.to_string()).or_default();
        if neighbor == Some(false) {
            // NeighborDown is positive evidence the peer left: force it offline
            // now rather than refreshing its liveness.
            e.last_seen = now_secs() - PEER_ONLINE_TTL_SECS - 1;
        } else {
            e.last_seen = now_secs();
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
        (now - e.last_seen) < PEER_ONLINE_TTL_SECS
    }

    /// The full peer-id strings currently known, for re-fetching content from
    /// peers during self-heal.
    fn peer_ids(&self) -> Vec<String> {
        self.peers.keys().cloned().collect()
    }

    /// Peer-id strings currently considered online (heard-from within the TTL).
    /// Used to pick live content providers for a download — read at download time
    /// so it reflects who is actually around now, not a stale job snapshot.
    fn online_peer_ids(&self) -> Vec<String> {
        let now = now_secs();
        self.peers
            .iter()
            .filter(|(_, e)| self.is_online(e, now))
            .map(|(id, _)| id.clone())
            .collect()
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

/// Blobs at or above this size are fetched as a **swarm** — the chunk range is
/// split across several providers and the parts streamed concurrently — provided
/// there are at least two online peers to split across. Below it (or with one
/// source), a blob is fetched whole from a single provider: chunk-splitting
/// overhead isn't worth it for small files.
const SWARM_MIN_SIZE: u64 = 4 * 1024 * 1024; // 4 MiB
/// Upper bound on how many concurrent parts one blob is split into, to cap
/// connection/range fan-out regardless of how many peers are online. So no single
/// member serves more than ~1/N of a file when at least this many peers are around.
const SWARM_MAX_PARTS: usize = 16;
/// Cold-start relief: a part waits a *random* number of swarm rounds (`0..N`)
/// before it may fall back to the master, fetching from peers only until then. The
/// randomization desynchronizes downloaders so they pull different parts off the
/// master first, become partial seeders, and trade the rest among themselves
/// instead of every node hammering the master. (If there are no peers at all, the
/// master is used immediately — no point waiting.)
const SWARM_MASTER_GRACE_ROUNDS: u32 = 6;
/// Pause between swarm rounds, giving peers time to seed each other before retry.
const SWARM_ROUND_BACKOFF_MS: u64 = 400;
/// Overall wall-clock bound on one swarm attempt. On timeout we return an error so
/// the reconcile loop re-queues; already-fetched ranges persist, so the next
/// attempt resumes rather than restarting. Bounds the worst case (a peer that
/// can't actually be reached) without losing progress.
const SWARM_DEADLINE_SECS: u64 = 300;

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

/// Online non-master peers (shuffled) and the master id, with `self` removed, read
/// from the *live* roster — so a download picks who is actually around now, not a
/// stale snapshot. The master is returned separately so callers can deprioritize
/// it; its id comes from the share key, so it's a fallback even if presence hasn't
/// (re)converged on it.
fn live_providers_from(
    roster: &Arc<StdMutex<PeerRoster>>,
    self_id: EndpointId,
    master_id: Option<EndpointId>,
) -> (Vec<EndpointId>, Option<EndpointId>) {
    let mut peers: Vec<EndpointId> = Vec::new();
    if let Ok(r) = roster.lock() {
        for s in r.online_peer_ids() {
            if let Ok(id) = s.parse::<EndpointId>() {
                if id != self_id && Some(id) != master_id && !peers.contains(&id) {
                    peers.push(id);
                }
            }
        }
    }
    peers.shuffle(&mut rand::thread_rng());
    let master = master_id.filter(|m| *m != self_id);
    (peers, master)
}

/// Fetch one blob as a **swarm**: split its chunk range into one contiguous part
/// per peer (capped at [`SWARM_MAX_PARTS`]) and pull the parts concurrently, each
/// part preferring a distinct peer. The store reassembles the bao-verified ranges;
/// when every part has landed the entry is whole and [`Blobs::has`] reports it.
///
/// Runs in **rounds** so it adapts as peers gain content (dynamic discovery):
/// every round re-reads the live roster, re-issues the still-missing parts (already
/// complete parts no-op cheaply), and pauses briefly so peers can seed each other.
///
/// **Cold-start relief:** each part waits a random `0..SWARM_MASTER_GRACE_ROUNDS`
/// rounds before it's allowed to fall back to the master — until then it pulls from
/// peers only. Because the grace is randomized per part *and* per downloader,
/// different nodes pull different parts off the master first, become partial
/// seeders, and trade the rest among themselves, instead of every node downloading
/// the whole file from the master. With no peers at all, the master is used at once.
async fn swarm_download(
    downloader: &Downloader,
    blobs: &FsStore,
    hash: Hash,
    size: u64,
    roster: &Arc<StdMutex<PeerRoster>>,
    self_id: EndpointId,
    master_id: Option<EndpointId>,
) -> anyhow::Result<()> {
    let total = ChunkNum::chunks(size).0; // number of bao chunks covering the blob
    let (peers0, _) = live_providers_from(roster, self_id, master_id);
    let parts = peers0
        .len()
        .min(SWARM_MAX_PARTS)
        .min(total.max(1) as usize)
        .max(1);
    let per = total.div_ceil(parts as u64);
    let ranges: Vec<(u64, u64)> = (0..parts)
        .map(|i| (i as u64 * per, ((i as u64 + 1) * per).min(total)))
        .filter(|(lo, hi)| lo < hi)
        .collect();
    // Per-part master grace (rounds), randomized so downloaders desynchronize.
    let grace: Vec<u32> = {
        let mut rng = rand::thread_rng();
        ranges
            .iter()
            .map(|_| rng.gen_range(0..SWARM_MASTER_GRACE_ROUNDS))
            .collect()
    };

    let work = async {
        let mut round: u32 = 0;
        loop {
            if blobs.blobs().has(hash).await.unwrap_or(false) {
                return Ok(());
            }
            let (peers, master) = live_providers_from(roster, self_id, master_id);
            let mut set = tokio::task::JoinSet::new();
            for (idx, &(lo, hi)) in ranges.iter().enumerate() {
                // Peers first (this part's assigned peer at the front), master only
                // once this part's grace has elapsed (or if there are no peers).
                let mut plist: Vec<EndpointId> = Vec::new();
                if !peers.is_empty() {
                    let primary = peers[idx % peers.len()];
                    plist.push(primary);
                    plist.extend(peers.iter().copied().filter(|p| *p != primary));
                }
                if peers.is_empty() || round >= grace[idx] {
                    if let Some(m) = master {
                        if !plist.contains(&m) {
                            plist.push(m);
                        }
                    }
                }
                if plist.is_empty() {
                    continue;
                }
                let req =
                    GetRequest::blob_ranges(hash, ChunkRanges::from(ChunkNum(lo)..ChunkNum(hi)));
                let dl = downloader.clone();
                set.spawn(async move {
                    dl.download_with_opts(DownloadRequest::new(req, plist, SplitStrategy::None))
                        .await
                });
            }
            // Drain the round; per-part errors (a peer that didn't have its range
            // yet) are expected and ignored — the next round retries with fresh
            // providers, and completed ranges persist.
            while set.join_next().await.is_some() {}

            if blobs.blobs().has(hash).await.unwrap_or(false) {
                return Ok(());
            }
            round = round.saturating_add(1);
            tokio::time::sleep(Duration::from_millis(SWARM_ROUND_BACKOFF_MS)).await;
        }
    };

    match tokio::time::timeout(Duration::from_secs(SWARM_DEADLINE_SECS), work).await {
        Ok(r) => r,
        Err(_) => Err(anyhow!("swarm download of {hash} timed out")),
    }
}

/// Convert a 32-byte hash slice into an iroh [`Hash`].
fn to_hash(bytes: &[u8]) -> anyhow::Result<Hash> {
    let arr: [u8; 32] = bytes.try_into().context("bad hash len")?;
    Ok(Hash::from(arr))
}

/// What a [`ReconcileJob::run`] decided, applied back into engine state under the
/// lock by [`Engine::finish_reconcile`]: index mutations to persist, the new
/// folder signature, this node's recomputed health, and any blob copies to reclaim.
/// Bookkeeping for a content download currently in flight, so it can be both
/// deduplicated and **cancelled** (e.g. when its share is paused) instead of
/// running to completion. Keyed by blob hash in the shared in-flight map.
struct InflightDownload {
    /// Which share kicked it off (so pausing that share can cancel just its
    /// downloads).
    share_id: String,
    /// Aborts the detached download task. Aborting drops the download future
    /// (and, for a swarm, its `JoinSet` of part tasks), closing the connections;
    /// already-fetched chunks persist on disk and resume on the next attempt.
    abort: tokio::task::AbortHandle,
}

pub struct ReconcileOutcome {
    changed: bool,
    health: u8,
    new_quick_sig: u64,
    index_sets: Vec<(String, Vec<u8>)>,
    index_dels: Vec<String>,
    reclaim: Vec<Hash>,
    /// Relative paths present on disk that couldn't be read this pass (locked by
    /// another process, permission denied). Carried back into [`ShareState`] and
    /// retried cheaply every tick — see [`ReconcileJob::prev_skipped`].
    skipped: Vec<String>,
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
    /// The share master's endpoint id (from the key), if known. Deprioritized to
    /// last in the download candidate order so peers are tried first and the
    /// master isn't the exclusive content source.
    master_id: Option<EndpointId>,
    /// This node's own endpoint id, filtered out of the candidate set (no point
    /// dialing ourselves).
    self_id: EndpointId,
    /// Content downloader (cloned node handle) and the shared in-flight map, used
    /// by [`ReconcileJob::ensure_download`] to fetch missing blobs from a
    /// load-balanced provider set (and to cancel them on pause).
    downloader: Downloader,
    downloads_inflight: Arc<StdMutex<HashMap<Hash, InflightDownload>>>,
    /// Live peer roster, read at download time to pick current online providers
    /// (dynamic discovery), rather than relying on the job-creation snapshot in
    /// `providers`.
    roster: Arc<StdMutex<PeerRoster>>,
    /// Paths skipped on the previous pass because they couldn't be read (locked,
    /// permission denied). Retried cheaply each tick: a still-locked file fails its
    /// `open` instantly; a now-readable one is hashed once and published. Ensures a
    /// locked file never permanently blocks the share, even if it's silently
    /// unlocked later with no other folder change (which wouldn't re-trigger a scan).
    prev_skipped: Vec<String>,
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

    /// Live content providers for this job — see [`live_providers_from`].
    fn live_providers(&self) -> (Vec<EndpointId>, Option<EndpointId>) {
        live_providers_from(&self.roster, self.self_id, self.master_id)
    }

    /// Bytes of `hash` already present on disk, including a partially-downloaded
    /// blob (chunk granularity). Lets health/percent reflect real progress on a
    /// large in-flight file. Returns 0 if the blob is unknown or the query fails.
    async fn local_bytes(&self, hash: Hash) -> u64 {
        match self
            .blobs
            .remote()
            .local_for_request(GetRequest::blob(hash))
            .await
        {
            Ok(info) => info.local_bytes(),
            Err(_) => 0,
        }
    }

    /// Ensure a download of `hash` (of `size` bytes) is in flight. Idempotent: a
    /// hash already downloading is skipped, so repeated reconcile ticks don't pile
    /// up duplicate fetches. Runs detached; the next reconcile re-checks `has()` and
    /// re-queues if it's still missing (free retry — already-fetched ranges resume).
    ///
    /// Large blobs with ≥2 online peers are fetched as a **swarm**: the chunk range
    /// is split into one contiguous part per peer and the parts streamed
    /// concurrently, each part preferring a distinct peer (master last). This is the
    /// real multi-source download — a single big file (e.g. an ISO, one blob) is
    /// pulled from several members at once instead of saturating one. The store
    /// reassembles the verified ranges into the complete blob. Small blobs (or when
    /// only one source is available) take the simple whole-blob path.
    fn ensure_download(&self, hash: Hash, size: u64) {
        // Already downloading this blob? (cheap pre-check)
        {
            let inflight = match self.downloads_inflight.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            if inflight.contains_key(&hash) {
                return;
            }
        }
        let (peers, master) = self.live_providers();
        if peers.is_empty() && master.is_none() {
            // Nobody to pull from yet (no peers, we are the only/master node).
            return;
        }
        // Full fallback set for the simple path: master last so peers are preferred.
        let fallback: Vec<EndpointId> = peers.iter().copied().chain(master).collect();
        let swarm = size >= SWARM_MIN_SIZE && peers.len() >= 2;

        let downloader = self.downloader.clone();
        let blobs = self.blobs.clone();
        let roster = self.roster.clone();
        let self_id = self.self_id;
        let master_id = self.master_id;
        let inflight = self.downloads_inflight.clone();
        let share = self.share_id.clone();
        let handle = tokio::spawn(async move {
            let res = if swarm {
                swarm_download(&downloader, &blobs, hash, size, &roster, self_id, master_id).await
            } else {
                downloader
                    .download_with_opts(DownloadRequest::new(hash, fallback, SplitStrategy::None))
                    .await
                    .map_err(|e| anyhow!("{e}"))
            };
            if let Err(e) = res {
                tracing::debug!("download {hash} for share {share} failed (will retry): {e}");
            }
            if let Ok(mut g) = inflight.lock() {
                g.remove(&hash);
            }
        });

        // Register the abort handle so a pause can cancel this transfer. If another
        // tick registered the same hash while we were spawning, cancel this duplicate.
        let abort = handle.abort_handle();
        match self.downloads_inflight.lock() {
            Ok(mut inflight) => {
                if inflight.contains_key(&hash) {
                    abort.abort();
                } else {
                    inflight.insert(
                        hash,
                        InflightDownload {
                            share_id: self.share_id.clone(),
                            abort,
                        },
                    );
                }
            }
            Err(_) => abort.abort(),
        }
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
            // We drive the fetch ourselves (iroh-docs' auto-downloader is disabled
            // per share): large blobs swarm across peers, small ones pull whole, and
            // the master is deprioritized so it isn't the exclusive source.
            // Idempotent across ticks; re-checked next reconcile.
            self.ensure_download(hash, size);
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
        // Build the local view, plus the candidate paths to (re)attempt reading this
        // pass: when scanning, whatever the scan couldn't read; otherwise the set
        // skipped on the previous pass.
        let (mut local, skip_candidates): (HashMap<String, LocalEntry>, Vec<String>) = if do_scan {
            let mut m = HashMap::new();
            let (scanned, skipped) = scan::scan(&self.folder, &ignore_set)?;
            for sf in scanned {
                m.insert(
                    sf.entry.path.clone(),
                    LocalEntry {
                        hash: sf.entry.hash,
                        size: sf.entry.size,
                        abs: Some(sf.abs_path),
                    },
                );
            }
            (m, skipped)
        } else {
            // No full scan this tick — the on-disk content equals our recorded base,
            // except for files we previously couldn't read; retry just those below.
            let m = self
                .base
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
                .collect();
            (m, self.prev_skipped.clone())
        };

        // Targeted retry of previously-unreadable files: hash ONLY those (cheap — a
        // still-locked file fails its open instantly; a now-readable one is hashed
        // once and folded into `local` so it publishes/syncs this pass). A file no
        // longer on disk is dropped (the normal delete path handles its manifest
        // entry). This guarantees a locked file is always retried and never blocks the
        // rest of the share, even after a silent unlock that wouldn't change the
        // folder signature. (On a full-scan tick the scan already tried these, so we
        // just carry forward the ones still unreadable.)
        let mut still_skipped: Vec<String> = Vec::new();
        for rel in skip_candidates {
            if local.contains_key(&rel) {
                continue;
            }
            let abs = self.folder.join(rel_to_native(&rel));
            if do_scan {
                // Scan already attempted (and failed) to read it this pass.
                if abs.exists() {
                    still_skipped.push(rel);
                }
                continue;
            }
            match scan::hash_file(&abs) {
                Ok((hash, size)) => {
                    local.insert(
                        rel,
                        LocalEntry {
                            hash,
                            size,
                            abs: Some(abs),
                        },
                    );
                }
                Err(_) => {
                    if abs.exists() {
                        still_skipped.push(rel);
                    }
                }
            }
        }

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
                        // remote delete): publish it. A per-file import failure (the
                        // file got locked between scan and import, an odd entry, etc.)
                        // must NOT abort the whole pass — skip it and retry next tick.
                        match self.import_one(&path, abs).await {
                            Ok(h) => {
                                imported_bytes += le.size;
                                self.set_progress(imported_bytes, imported_bytes);
                                index_sets.push((path, h));
                                changed = true;
                            }
                            Err(e) => {
                                tracing::warn!("skip publishing {path} (will retry): {e:#}");
                                continue;
                            }
                        }
                    }
                }

                // In the replica, absent from the *scan*.
                (None, Some(re)) => {
                    // "Absent from scan" can mean genuinely deleted OR present-on-disk
                    // but skipped because it couldn't be read (locked/unreadable). Only
                    // a file that is truly gone from disk is a deletion; a still-present
                    // unreadable file must be left alone (don't tombstone, don't
                    // re-download over it) and retried on a later pass.
                    let on_disk = self.folder.join(rel_to_native(&path)).exists();
                    if on_disk {
                        // Present but unreadable this pass: leave as-is.
                    } else if self.is_master
                        && do_scan
                        && b.map(|bh| bh == &re.hash).unwrap_or(false)
                    {
                        // Master, full scan saw it genuinely gone while base+replica
                        // agreed → the user deleted it: propagate the tombstone.
                        self.tombstone(&path).await;
                        index_dels.push(path);
                        changed = true;
                    } else {
                        match self
                            .materialize(&path, &re.hash, re.size, &mut reclaim)
                            .await
                        {
                            Ok(true) => {
                                index_sets.push((path, re.hash.clone()));
                                changed = true;
                            }
                            Ok(false) => {}
                            Err(e) => {
                                tracing::warn!("skip syncing {path} (will retry): {e:#}");
                                continue;
                            }
                        }
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
                        // Viewer: replica wins, always. A per-file failure (locked
                        // target, etc.) is skipped, not fatal to the whole pass.
                        match self
                            .materialize(&path, &re.hash, re.size, &mut reclaim)
                            .await
                        {
                            Ok(true) => {
                                index_sets.push((path, re.hash.clone()));
                                changed = true;
                            }
                            Ok(false) => {}
                            Err(e) => tracing::warn!("skip syncing {path} (will retry): {e:#}"),
                        }
                        continue;
                    }
                    // Master three-way merge. Every per-file op below skips on error
                    // (logs + moves on) rather than aborting the whole reconcile.
                    match b {
                        Some(bh) if bh == &le.hash => {
                            // Local untouched, remote changed → take remote.
                            match self
                                .materialize(&path, &re.hash, re.size, &mut reclaim)
                                .await
                            {
                                Ok(true) => {
                                    index_sets.push((path, re.hash.clone()));
                                    changed = true;
                                }
                                Ok(false) => {}
                                Err(e) => {
                                    tracing::warn!("skip syncing {path} (will retry): {e:#}");
                                    continue;
                                }
                            }
                        }
                        Some(bh) if bh == &re.hash => {
                            // Remote untouched, local changed → publish local.
                            if let Some(abs) = le.abs.as_ref() {
                                match self.import_one(&path, abs).await {
                                    Ok(h) => {
                                        index_sets.push((path, h));
                                        changed = true;
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "skip publishing {path} (will retry): {e:#}"
                                        );
                                        continue;
                                    }
                                }
                            }
                        }
                        _ => {
                            // Both changed (or unknown base) → last-writer-wins.
                            let local_ts = le.abs.as_ref().map(|a| mtime_micros(a)).unwrap_or(0);
                            if local_ts >= re.ts {
                                if let Some(abs) = le.abs.as_ref() {
                                    match self.import_one(&path, abs).await {
                                        Ok(h) => {
                                            index_sets.push((path, h));
                                            changed = true;
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                "skip publishing {path} (will retry): {e:#}"
                                            );
                                            continue;
                                        }
                                    }
                                }
                            } else {
                                match self
                                    .materialize(&path, &re.hash, re.size, &mut reclaim)
                                    .await
                                {
                                    Ok(true) => {
                                        index_sets.push((path, re.hash.clone()));
                                        changed = true;
                                    }
                                    Ok(false) => {}
                                    Err(e) => {
                                        tracing::warn!("skip syncing {path} (will retry): {e:#}");
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Tidy now-empty directories a viewer/master delete may have left behind.
        prune_empty_dirs(&self.folder);

        // Health = the fraction of the merged desired view we actually hold locally,
        // for *every* role. A source master that already has all its content computes
        // 100 naturally; a master or viewer still fetching missing blobs reports the
        // real percentage instead of a misleading 100. An empty view is 100.
        //
        // For an incomplete file we count the chunk bytes already on disk (not 0), so
        // the percent climbs with real progress on a large in-flight blob instead of
        // staying flat until it finishes and jumping straight to done. The blob store
        // tracks this at chunk granularity (that's how the resumable swarm works), so
        // it's accurate and survives restarts.
        let mut total_bytes: u64 = 0;
        let mut present_bytes: u64 = 0;
        for re in remote.values() {
            total_bytes += re.size;
            if re.size == 0 {
                continue;
            }
            let hash = to_hash(&re.hash)?;
            if self.blobs.blobs().has(hash).await? {
                present_bytes += re.size;
            } else {
                present_bytes += self.local_bytes(hash).await.min(re.size);
            }
        }
        let health = if total_bytes == 0 {
            100
        } else {
            (present_bytes.min(total_bytes) * 100 / total_bytes) as u8
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
            skipped: still_skipped,
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
    /// Paths skipped last pass because they couldn't be read (locked/unreadable);
    /// fed into the next [`ReconcileJob`] so they're retried cheaply until readable.
    /// In-memory only — rediscovered by the first full scan after a restart.
    skipped: Vec<String>,
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
    /// Content downloads currently in flight, keyed by blob hash, each with its
    /// share id and an abort handle. Shared with each [`ReconcileJob`] so a hash
    /// isn't re-queued every reconcile tick while its blob streams in, AND so a
    /// pause can cancel the running transfer. Global (content is addressed by hash,
    /// so the same blob referenced from two shares is fetched once). Entries are
    /// removed when the task settles or is cancelled; a still-missing blob is
    /// re-queued on the next tick (free retry, resuming from on-disk chunks).
    downloads_inflight: Arc<StdMutex<HashMap<Hash, InflightDownload>>>,
    /// Hashes whose orphaned owned blob copy (left by a cross-volume reference
    /// export) still needs deleting. Retried each reconcile until iroh releases
    /// the file handle — see [`try_reclaim_owned_data`].
    reclaim_pending: std::collections::HashSet<Hash>,
    /// This device's display name, broadcast in presence and shown to other
    /// members. Cached from `settings["device_name"]` (default: hostname) so
    /// per-tick broadcasts and `peers()` don't hit the DB. One global name.
    device_name: StdMutex<String>,
    /// Global "pause all activity" switch. When set, the reconcile loop builds no
    /// jobs for any share (regardless of each share's own `paused` flag) and every
    /// summary reports `Paused`. Persisted in `settings["paused_all"]` so it
    /// survives a daemon restart and new shares added while paused stay paused.
    paused_all: StdMutex<bool>,
    /// Transient "suspend sync" gate, separate from the user's pause switch. Set
    /// by the host (e.g. Android's Wi-Fi-only / charging-only policy) to halt
    /// reconcile without touching the user's pause state. Deliberately *not*
    /// persisted: the host recomputes it from live conditions on every start.
    sync_suspended: StdMutex<bool>,
}

impl Engine {
    /// Bootstrap the engine against a data directory, reloading any persisted
    /// shares (so the daemon is restart-safe).
    pub async fn new(data_dir: &Path) -> anyhow::Result<Self> {
        Self::new_with_blobs(data_dir, &data_dir.join("blobs")).await
    }

    /// Like [`new`](Self::new) but with the blob store rooted at an explicit
    /// `blobs_dir` (the rest of the layout — `state.db`, `node.key`, `docs/` —
    /// stays under `data_dir`). Used on Android to co-locate `blobs/` with the
    /// synced folders on shared storage; see [`IrohNode::spawn_with_blobs`].
    pub async fn new_with_blobs(data_dir: &Path, blobs_dir: &Path) -> anyhow::Result<Self> {
        let node = IrohNode::spawn_with_blobs(data_dir, blobs_dir).await?;
        let author = node.docs_api().author_default().await?;
        let db = crate::db::Db::open(&data_dir.join("state.db"))?;
        let device_name = db
            .get_setting("device_name")?
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(default_device_name);
        let paused_all = db.get_setting("paused_all")?.as_deref() == Some("1");
        let mut engine = Self {
            node,
            author,
            shares: HashMap::new(),
            db,
            progress: Arc::new(StdMutex::new(HashMap::new())),
            downloads_inflight: Arc::new(StdMutex::new(HashMap::new())),
            reclaim_pending: std::collections::HashSet::new(),
            device_name: StdMutex::new(device_name),
            paused_all: StdMutex::new(paused_all),
            sync_suspended: StdMutex::new(false),
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

    /// Whether the global "pause all activity" switch is set.
    pub fn paused_all(&self) -> bool {
        self.paused_all.lock().map(|p| *p).unwrap_or(false)
    }

    /// Set + persist the global "pause all activity" switch. While set, no share
    /// reconciles. Resuming clears the switch; the caller may additionally clear
    /// the per-share `paused` flags (see [`Engine::set_paused`]) so a full resume
    /// gets everything running again.
    pub fn set_paused_all(&self, paused: bool) -> anyhow::Result<()> {
        self.db
            .set_setting("paused_all", if paused { "1" } else { "0" })?;
        if let Ok(mut p) = self.paused_all.lock() {
            *p = paused;
        }
        if paused {
            self.cancel_all_downloads();
        }
        Ok(())
    }

    /// Whether the transient host sync gate is suspending sync (e.g. Android is
    /// off Wi-Fi while "sync only on Wi-Fi" is enabled).
    pub fn sync_suspended(&self) -> bool {
        self.sync_suspended.lock().map(|s| *s).unwrap_or(false)
    }

    /// Set the transient sync gate. While set, no share reconciles (no folder
    /// hashing, no blob transfer), but the user's pause state and per-share flags
    /// are untouched, so clearing the gate resumes whatever wasn't user-paused.
    /// Not persisted — the host re-applies it from live conditions on each start.
    pub fn set_sync_suspended(&self, suspended: bool) {
        if let Ok(mut s) = self.sync_suspended.lock() {
            *s = suspended;
        }
        if suspended {
            self.cancel_all_downloads();
        }
    }

    /// Resume everything: clear the global switch and every per-share pause flag.
    pub fn resume_all(&mut self) -> anyhow::Result<()> {
        self.set_paused_all(false)?;
        let ids: Vec<String> = self.shares.keys().cloned().collect();
        for id in ids {
            if self.shares.get(&id).map(|s| s.paused).unwrap_or(false) {
                self.set_paused(&id, false)?;
            }
        }
        Ok(())
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

        // Disable iroh-docs' built-in content auto-downloader for this replica. Its
        // provider discovery favors whichever peer it synced the manifest entry
        // from — in practice the master — so every file would funnel through the
        // master. We instead drive blob fetches from the engine
        // ([`ReconcileJob::ensure_download`]) with a peers-first provider set. This
        // gates only *content* fetching; doc/metadata sync is untouched. The policy
        // is local (per-replica, not synced), so every node sets it on open.
        doc.set_download_policy(iroh_docs::store::DownloadPolicy::NothingExcept(vec![]))
            .await
            .context("disable docs content auto-download")?;

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
            // Provisional until the first reconcile computes real completeness. Start
            // at 0 (incomplete) for every role so a freshly-added master that still
            // has content to fetch never briefly reads a misleading 100.
            health: 0,
            presence,
            skipped: Vec::new(),
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
        // While globally paused, every share reads as paused so the GUI shows its
        // "Syncing Paused" page and the per-row controls agree.
        let paused_all = self.paused_all();
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
                let (status, percent, indexed_bytes, index_total) = if s.paused || paused_all {
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
                    paused: s.paused || paused_all,
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
            percent: state.health,
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
            let role = match s.key.role {
                Role::Master => seed_ipc::Role::Master,
                Role::Viewer => seed_ipc::Role::Viewer,
            };
            let percent = s.health;
            let p = crate::presence::Presence {
                v: crate::presence::PRESENCE_V,
                name: name.clone(),
                role,
                seqno: s.last_seqno,
                percent,
                ts,
                // Stamp our own endpoint id so receivers attribute this presence to
                // us directly, not to whichever member relayed it through the swarm.
                from: Some(self.node.endpoint_id_bytes()),
            };
            out.push(crate::presence::PresenceBroadcast::new(
                h.sender.clone(),
                &p,
            ));
        }
        out
    }

    /// Build this tick's gossip re-join requests — one per share with a live presence
    /// channel — asking the swarm to connect to every member doc-sync has discovered.
    /// Call under the engine lock (cheap: clones the gossip sender + snapshots the
    /// roster); run the results off-lock via [`PresenceRejoin::join`].
    ///
    /// This repairs the presence mesh: gossip's one-shot bootstrap leaves a partitioned
    /// star (the creator bootstraps with nothing; leaves only dial the creator), so
    /// without this, presence reaches 3+ member pools asymmetrically. The peer set comes
    /// from [`peer_providers`] (the master id carried in the key + every endpoint id the
    /// roster learned from doc events), minus ourselves.
    ///
    /// [`PresenceRejoin::join`]: crate::presence::PresenceRejoin::join
    pub fn presence_rejoins(&self) -> Vec<crate::presence::PresenceRejoin> {
        let self_id = self.node.endpoint.id();
        let mut out = Vec::new();
        for s in self.shares.values() {
            let Some(h) = s.presence.as_ref() else {
                continue;
            };
            let peers: Vec<EndpointId> = peer_providers(&s.key, &s.roster)
                .into_iter()
                .filter(|id| *id != self_id)
                .collect();
            if peers.is_empty() {
                continue;
            }
            out.push(crate::presence::PresenceRejoin::new(
                h.sender.clone(),
                peers,
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
        // Global pause and the transient host sync gate (Wi-Fi-only / charging-only)
        // both suspend all sync activity regardless of per-share state.
        if self.paused_all() || self.sync_suspended() {
            return Ok(None);
        }
        let blobs = self.node.blobs.clone();
        let endpoint = self.node.endpoint.clone();
        let downloader = self.node.downloader.clone();
        let self_id = self.node.endpoint.id();
        let downloads_inflight = self.downloads_inflight.clone();
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
        let master_id = state
            .key
            .endpoint_id()
            .and_then(|eid| EndpointId::from_bytes(&eid).ok());
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
            master_id,
            self_id,
            downloader,
            downloads_inflight,
            roster: state.roster.clone(),
            prev_skipped: state.skipped.clone(),
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
        state.skipped = out.skipped;
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
    /// Abort in-flight content downloads belonging to `share_id` so a large
    /// transfer stops promptly when the share is paused (rather than running to
    /// completion). Already-fetched chunks stay on disk and resume on unpause.
    fn cancel_downloads_for_share(&self, share_id: &str) {
        if let Ok(mut inflight) = self.downloads_inflight.lock() {
            inflight.retain(|_hash, dl| {
                if dl.share_id == share_id {
                    dl.abort.abort();
                    false
                } else {
                    true
                }
            });
        }
    }

    /// Abort ALL in-flight content downloads (global pause / sync-suspend).
    fn cancel_all_downloads(&self) {
        if let Ok(mut inflight) = self.downloads_inflight.lock() {
            for (_hash, dl) in inflight.drain() {
                dl.abort.abort();
            }
        }
    }

    pub fn set_paused(&mut self, share_id: &str, paused: bool) -> anyhow::Result<()> {
        let state = self
            .shares
            .get_mut(share_id)
            .ok_or_else(|| anyhow!("unknown share {share_id}"))?;
        state.paused = paused;
        self.db.set_paused(share_id, paused)?;
        // Stop any transfer already running for this share; the reconcile gate
        // (make_reconcile_job) prevents new ones while paused.
        if paused {
            self.cancel_downloads_for_share(share_id);
        }
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
