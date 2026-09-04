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

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
use iroh_blobs::store::{GcConfig, ProtectOutcome};
use iroh_blobs::{store::fs::FsStore, BlobFormat, Hash};
use iroh_docs::{
    api::Doc,
    engine::{LiveEvent, Origin},
    store::Query,
    sync::Capability,
    AuthorId, NamespaceId, NamespaceSecret,
};
use rand::seq::SliceRandom;
use rand::Rng;
use serde::{Deserialize, Serialize};

/// How long since a peer was last heard from before we consider it offline.
/// How long since the last sign of life (presence heartbeat, doc sync activity,
/// or a neighbor-up) before we consider a peer offline. Presence is broadcast
/// about every 3s, so this tolerates several missed beats while still flipping a
/// peer offline within a few seconds of it actually leaving.
const PEER_ONLINE_TTL_SECS: i64 = 20;

/// Cap on how many peers one presence rejoin asks the gossip swarm to connect
/// (known-issues #9). iroh-gossip (HyParView) keeps a small bounded *active
/// view*; joining EVERY known member every ~6s worked as mesh repair at ≤8
/// members but destroyed the overlay at fleet scale — with 28 members the
/// constant full-set joins evicted each other's neighbors faster than the
/// swarm could stabilize, per-node membership oscillated 1/28 ↔ 25/28 for a
/// whole soak, and epidemic delivery never formed. A few random targets per
/// tick still heal a partition within a handful of ticks (the partitioned side
/// drives its own repair) while leaving the active view stable enough for
/// gossip's own shuffle to maintain.
const PRESENCE_REJOIN_SAMPLE: usize = 3;

/// Tracks the peers seen for one share, fed by the doc's live events + presence
/// gossip, plus every member *remembered* from earlier sessions (`peer_names`
/// table + doc member-records). `total` counts all known members; `online` is
/// those heard-from within [`PEER_ONLINE_TTL_SECS`].
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
    /// Last-known identity per member (full endpoint-id string), kept across
    /// disconnects and daemon restarts (via the `peer_names` table) and learned
    /// even for members never heard directly (via doc member-records, see
    /// [`MEMBER_PREFIX`]). Display fallback only: it never drives liveness,
    /// download providers, or mesh repair, so a long-gone member costs nothing
    /// but a named offline row in the member list.
    remembered: HashMap<String, RememberedPeer>,
    /// Members whose remembered identity changed since the last DB flush
    /// ([`Engine::presence_broadcasts`] drains this every presence tick).
    dirty: HashSet<String>,
    /// Unix seconds of the last *genuine* sign of life from any peer — a received
    /// doc entry, a gossip neighbor-up, a presence beat, or a doc-sync that
    /// actually **succeeded**. A *failed* sync deliberately does NOT advance this:
    /// counting failed dials as contact is what made a fully-partitioned node mark
    /// every peer "online" on each retry, flapping the whole fleet "Syncing ↔
    /// offline" while nothing connected (known-issues #23, #16). `0` = no contact
    /// yet this session.
    last_contact: i64,
    /// The most recent failed doc-sync attempt: (peer id, error, unix secs). Kept
    /// purely for diagnostics and the partition WARN; never affects liveness.
    last_sync_err: Option<(String, String, i64)>,
    /// Unix seconds of the last **presence** heartbeat heard from any peer
    /// (gossip), as distinct from [`Self::last_contact`] which also counts
    /// doc-sync. The two diverge exactly in the failure this exists to catch: the
    /// presence overlay goes silent (no beats, peers stuck at `seqno=0`) while
    /// doc-sync keeps succeeding, so the member list flaps on the TTL and only a
    /// fresh subscription heals it (known-issues #23).
    /// `0` = no presence heard yet this session.
    last_presence: i64,
    /// Outbound-dial bookkeeping for the transport-repair ladder (known-issues
    /// #36). A doc-sync we *initiated* (`Origin::Connect`) that succeeded /
    /// failed, and the last sync a member initiated *toward us*
    /// (`Origin::Accept`) that succeeded. The wedge signature is "members keep
    /// reaching us, or are otherwise provably alive, while every dial we make
    /// times out" — which no roster-level repair can fix and only a fresh
    /// endpoint clears.
    last_outbound_ok: i64,
    last_outbound_err: Option<(String, i64)>,
    /// Consecutive failed outbound dials since the last successful one.
    outbound_failures: u32,
    last_inbound_ok: i64,
    /// Unix secs the rendezvous last resolved to a *different* master whose
    /// record was published recently — proof that master is up, independent of
    /// whether we can reach it.
    rendezvous_alive: i64,
}

/// Snapshot of a share's outbound/inbound dial history (see [`PeerRoster`]).
#[derive(Debug, Clone, Default)]
pub(crate) struct DialStats {
    pub(crate) last_outbound_ok: i64,
    pub(crate) last_outbound_err: Option<(String, i64)>,
    pub(crate) outbound_failures: u32,
    pub(crate) last_inbound_ok: i64,
    pub(crate) rendezvous_alive: i64,
}

#[derive(Default)]
struct PeerEntry {
    last_seen: i64,
    /// Filled from presence broadcasts (gossip). Absent until a peer announces.
    name: Option<String>,
    role: Option<seed_ipc::Role>,
    seqno: u64,
    percent: u8,
    /// Peer's manifest fingerprint from its last presence (0 = unknown/not reported).
    manifest_fp: u64,
    /// Last doc-sync *we* initiated to this peer that succeeded / failed
    /// (unix secs; the error text is kept for the CLI and the partition WARN).
    last_dial_ok: i64,
    last_dial_err: Option<(String, i64)>,
}

/// A member's last-known identity (see [`PeerRoster::remembered`]).
#[derive(Default, Clone)]
struct RememberedPeer {
    name: String,
    master: bool,
    /// Unix secs of the last evidence this member was alive: heard directly, or
    /// the timestamp of the doc member-record that named it.
    last_seen: i64,
    /// Unix secs this identity was last confirmed. Direct presence advances it
    /// every beat, so a doc member-record (written in the past by a master) can
    /// only fill identities we have *not* observed more recently ourselves.
    updated: i64,
}

/// How much the persisted `last_seen` of a remembered member may lag its live
/// value: steady-state presence refreshes it in memory every beat (~3s), and
/// re-writing sqlite that often per member would be waste — a name/role change
/// still flushes immediately.
const REMEMBERED_LAST_SEEN_FLUSH_SECS: i64 = 300;

/// Fault-episode tracker with asymmetric hysteresis for the self-heal ladders
/// (known-issues #35). An episode *starts* on the first faulty observation, but
/// only *ends* once the condition has read healthy continuously for
/// [`HEAL_CLEAR_SECS`] — a single healthy blip does not clear it, and elapsed
/// keeps accruing from the episode's original start.
///
/// Why: both ladders act only on a *sustained* fault (120s isolated / 90s
/// presence-silent), and the raw predicates flicker under a flap — one stray
/// presence beat getting through marks the peer online for a whole 20s TTL. The
/// old episode clocks reset on every such blip, so a share flapping on a ~25s
/// period sat degraded for over an hour with every ladder disarmed (the 2026-08
/// two-member outage). The ladders exist to be restart-equivalent; a flap must
/// not be their blind spot.
#[derive(Default)]
struct EpisodeClock {
    /// Unix seconds the current fault episode began; `None` while healthy.
    since: Option<i64>,
    /// Unix seconds the condition first read healthy within the active episode;
    /// `None` while it reads faulty (or no episode is active).
    healthy_since: Option<i64>,
}

impl EpisodeClock {
    /// Fold in one observation of the fault condition. Returns the episode's
    /// elapsed seconds while an episode is active (including mid-blip), `None`
    /// once it has genuinely cleared.
    fn observe(&mut self, faulty: bool, now: i64) -> Option<i64> {
        if faulty {
            self.healthy_since = None;
            Some(now - *self.since.get_or_insert(now))
        } else if let Some(since) = self.since {
            let healthy = *self.healthy_since.get_or_insert(now);
            if now - healthy >= HEAL_CLEAR_SECS {
                self.since = None;
                self.healthy_since = None;
                None
            } else {
                Some(now - since)
            }
        } else {
            None
        }
    }
}

/// Per-share bookkeeping for the connectivity self-heal ladders
/// (known-issues #23). Default = healthy. The two episodes are
/// tracked independently because they are mutually exclusive but each needs its own
/// reset: a share that recovers *transport* (leaving total isolation) can still have
/// a dead *presence* overlay, and vice versa.
#[derive(Default)]
struct ConnHeal {
    /// Total-isolation episode (transport dead): active while the share cannot
    /// reach any member (flap-hysteretic; see [`EpisodeClock`]).
    isolated: EpisodeClock,
    /// Whether the loud partition WARN has already been logged this isolation episode
    /// (so it isn't repeated every ~6s tick).
    isolated_warned: bool,
    /// Presence-gap episode (transport alive, gossip presence silent).
    presence_gap: EpisodeClock,
    /// Unix seconds of the last presence-subscription rebuild — a shared throttle
    /// across both ladders (see [`PRESENCE_REBUILD_MIN_SECS`]).
    last_presence_rebuild: i64,
    /// Ladder-3 episode (known-issues #36): members alive, our outbound dials
    /// failing. Flap-hysteretic like the others.
    outbound: EpisodeClock,
    /// Whether the ladder-3 WARN was logged this episode.
    outbound_warned: bool,
    /// Unix seconds rung 1 (network re-probe) ran for this episode; 0 = not yet.
    rung1_at: i64,
}

/// Engine-wide state of the transport-repair ladder (rung 2/3 act on the one
/// shared endpoint, so they are throttled here, not per share).
#[derive(Default)]
struct TransportHeal {
    /// Unix seconds of the last in-process endpoint rebuild (0 = never).
    last_rebuild: i64,
    /// Current minimum spacing to the next rebuild (0 = the base
    /// [`TRANSPORT_REBUILD_MIN_SECS`]); doubles while the fault persists.
    backoff: i64,
    /// Consecutive rebuilds that *failed* (could not respawn the node).
    failures: u32,
    /// Total rebuilds this process (diagnostics / tests).
    rebuilds: u32,
    /// Set once rebuilds keep failing: the daemon should exit for its supervisor.
    fatal: Option<String>,
}

impl PeerRoster {
    /// Record activity for a peer. `neighbor` distinguishes gossip membership
    /// transitions: `Some(true)` = NeighborUp, `Some(false)` = NeighborDown,
    /// `None` = other evidence of life (remote insert / sync finished / presence).
    pub(crate) fn note(&mut self, id: &str, neighbor: Option<bool>) {
        let now = now_secs();
        let e = self.peers.entry(id.to_string()).or_default();
        if neighbor == Some(false) {
            // NeighborDown is positive evidence the peer left: force it offline
            // now rather than refreshing its liveness.
            e.last_seen = now - PEER_ONLINE_TTL_SECS - 1;
        } else {
            e.last_seen = now;
            self.last_contact = now;
        }
    }

    /// Record the outcome of a doc live-sync with a peer. A **successful** sync is
    /// genuine contact and refreshes liveness like any other sign of life; a
    /// **failed** one is NOT — it only records a diagnostic and never touches
    /// `last_seen`, so an unreachable peer ages out and stays out instead of being
    /// marked "online" on every failed retry (the phantom-liveness flap;
    /// known-issues #23).
    ///
    /// `outbound` says who dialed: `true` for a sync we initiated
    /// (`Origin::Connect`), `false` for one the peer initiated toward us
    /// (`Origin::Accept`). The split feeds the transport-repair ladder
    /// (known-issues #36): a peer that keeps reaching us while our dials to it
    /// all fail is the wedged-endpoint signature.
    pub(crate) fn note_sync_finished(
        &mut self,
        id: &str,
        outbound: bool,
        ok: bool,
        err: Option<&str>,
    ) {
        let now = now_secs();
        if ok {
            self.note(id, None);
            if outbound {
                self.last_outbound_ok = now;
                self.outbound_failures = 0;
                if let Some(e) = self.peers.get_mut(id) {
                    e.last_dial_ok = now;
                }
            } else {
                self.last_inbound_ok = now;
            }
        } else {
            let msg = err.unwrap_or_default().to_string();
            self.last_sync_err = Some((id.to_string(), msg.clone(), now));
            if outbound {
                self.last_outbound_err = Some((msg.clone(), now));
                self.outbound_failures = self.outbound_failures.saturating_add(1);
                // Only annotate a peer we already know; a failed dial must not
                // conjure a roster row (that is what made failed syncs look
                // like members, known-issues #23).
                if let Some(e) = self.peers.get_mut(id) {
                    e.last_dial_err = Some((msg, now));
                }
            }
        }
    }

    /// Record that the rendezvous resolved to another master whose record was
    /// published within the freshness window: that master is alive right now,
    /// whatever our dials say.
    pub(crate) fn note_rendezvous_alive(&mut self) {
        self.rendezvous_alive = now_secs();
    }

    /// Outbound / inbound dial history for the transport-repair ladder.
    pub(crate) fn dial_stats(&self) -> DialStats {
        DialStats {
            last_outbound_ok: self.last_outbound_ok,
            last_outbound_err: self.last_outbound_err.clone(),
            outbound_failures: self.outbound_failures,
            last_inbound_ok: self.last_inbound_ok,
            rendezvous_alive: self.rendezvous_alive,
        }
    }

    /// Unix seconds of the last genuine peer contact this session (0 = none yet).
    pub(crate) fn last_contact(&self) -> i64 {
        self.last_contact
    }

    /// The most recent failed doc-sync attempt (peer id, error, unix secs), if any.
    pub(crate) fn last_sync_err(&self) -> Option<&(String, String, i64)> {
        self.last_sync_err.as_ref()
    }

    /// Unix seconds of the last presence heartbeat heard from any peer (0 = none
    /// yet this session). Distinct from [`Self::last_contact`], which also counts
    /// doc-sync — see the [`Self::last_presence`] field.
    pub(crate) fn last_presence(&self) -> i64 {
        self.last_presence
    }

    /// Fold a presence broadcast into the roster: refresh name/role/health and
    /// mark the peer heard-from (online for the TTL). Also remembers the identity
    /// (name/role) so the member list keeps naming this member after it
    /// disconnects or we restart.
    pub(crate) fn note_presence(&mut self, id: &str, p: crate::presence::Presence) {
        let now = now_secs();
        let master = matches!(p.role, seed_ipc::Role::Master);
        self.last_contact = now;
        self.last_presence = now;
        let e = self.peers.entry(id.to_string()).or_default();
        e.last_seen = now;
        e.name = Some(p.name.clone());
        e.role = Some(p.role);
        e.seqno = p.seqno;
        e.percent = p.percent;
        e.manifest_fp = p.manifest_fp;
        let m = self.remembered.entry(id.to_string()).or_default();
        if m.name != p.name
            || m.master != master
            || now - m.last_seen >= REMEMBERED_LAST_SEEN_FLUSH_SECS
        {
            self.dirty.insert(id.to_string());
        }
        m.name = p.name;
        m.master = master;
        m.last_seen = now;
        m.updated = now;
    }

    /// Fold the doc's member registry (see [`read_member_records`]) into the
    /// remembered identities. A record applies only if it is newer than our
    /// current knowledge of that member (`updated`), so it can never override a
    /// name we heard directly more recently — it fills in members we've never
    /// heard (or heard before a rename we were offline for).
    pub(crate) fn note_member_records(&mut self, recs: &[MemberIdentity]) {
        for r in recs {
            let m = self.remembered.entry(r.id.clone()).or_default();
            if r.ts_secs > m.updated {
                m.name = r.name.clone();
                m.master = r.master;
                m.updated = r.ts_secs;
                m.last_seen = m.last_seen.max(r.ts_secs);
                self.dirty.insert(r.id.clone());
            }
        }
    }

    /// Seed the remembered identities from the `peer_names` table (share open):
    /// the member list names every known member immediately after a restart,
    /// rendered offline until heard again. Not marked dirty — it just came from
    /// the DB.
    pub(crate) fn preload_remembered(&mut self, rows: Vec<crate::db::PeerNameRow>) {
        for r in rows {
            self.remembered.entry(r.node_id).or_insert(RememberedPeer {
                name: r.name,
                master: r.role_master,
                last_seen: r.last_seen,
                updated: r.updated,
            });
        }
    }

    /// Drain the identities changed since the last flush as ready-to-upsert
    /// `peer_names` rows. Almost always empty.
    pub(crate) fn drain_dirty_names(&mut self, share_id: &str) -> Vec<crate::db::PeerNameRow> {
        let mut out = Vec::new();
        for id in self.dirty.drain() {
            let Some(m) = self.remembered.get(&id) else {
                continue;
            };
            out.push(crate::db::PeerNameRow {
                share_id: share_id.to_string(),
                node_id: id,
                name: m.name.clone(),
                role_master: m.master,
                last_seen: m.last_seen,
                updated: m.updated,
            });
        }
        out
    }

    /// Members currently online whose identity we know first-hand (heard via
    /// presence this session): what a master publishes into the doc member
    /// registry. Online-only, so a republished name is at most one presence TTL
    /// stale — an offline member may have renamed itself elsewhere since we last
    /// heard it, and a fresh doc timestamp on stale data would win LWW over a
    /// better-informed master's record.
    pub(crate) fn online_named_peers(&self) -> Vec<(String, String, bool)> {
        let now = now_secs();
        self.peers
            .iter()
            .filter(|(_, e)| self.is_online(e, now))
            .filter_map(|(id, e)| {
                let name = e.name.clone()?;
                let master = matches!(e.role?, seed_ipc::Role::Master);
                Some((id.clone(), name, master))
            })
            .collect()
    }

    fn is_online(&self, e: &PeerEntry, now: i64) -> bool {
        (now - e.last_seen) < PEER_ONLINE_TTL_SECS
    }

    /// The full peer-id strings currently known, for re-fetching content from
    /// peers during self-heal.
    fn peer_ids(&self) -> Vec<String> {
        self.peers.keys().cloned().collect()
    }

    /// Every member we know of: heard this session, *or* remembered from an earlier
    /// one. The remembered half is what makes a restart survive any single peer
    /// being down — `peer_names` has persisted every member's full endpoint id all
    /// along, but until known-issues #16 it was only ever read to *label* the
    /// roster, never to dial. See [`peer_providers`].
    fn known_peer_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.peers.keys().cloned().collect();
        for id in self.remembered.keys() {
            if !self.peers.contains_key(id) {
                ids.push(id.clone());
            }
        }
        ids
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

    /// Online peers with their self-reported sync percent, for provider ordering
    /// (fully-synced members can serve any blob; partial ones only some).
    fn online_peer_percents(&self) -> Vec<(String, u8)> {
        let now = now_secs();
        self.peers
            .iter()
            .filter(|(_, e)| self.is_online(e, now))
            .map(|(id, e)| (id.clone(), e.percent))
            .collect()
    }

    /// Known manifest fingerprints of currently-online peers, excluding `0`
    /// (unknown / not yet reported). Used to detect cross-member divergence.
    fn online_manifest_fps(&self) -> Vec<u64> {
        let now = now_secs();
        self.peers
            .values()
            .filter(|e| self.is_online(e, now) && e.manifest_fp != 0)
            .map(|e| e.manifest_fp)
            .collect()
    }

    /// Manifest fingerprints of online peers that report themselves **fully synced**
    /// (`percent >= 100`), excluding `0` (unknown / not yet reported).
    ///
    /// This — not [`Self::online_manifest_fps`] — is what divergence detection must
    /// compare against. Divergence means "we hold different filesets", but a member
    /// that is still completing its *initial* sync legitimately holds a different
    /// fileset: its replica is mid-flight and its manifest is a partial view of the
    /// share. Comparing against it turns normal joining into a fleet-wide
    /// "members disagree" alarm — which is exactly what happened when three devices
    /// joined a fresh share and every one of them reported OutOfSync before the first
    /// sync had even finished.
    ///
    /// A peer that is still catching up is *behind*, not *diverged*, and the two must
    /// not be conflated: one resolves itself, the other needs a human.
    fn settled_manifest_fps(&self) -> Vec<u64> {
        let now = now_secs();
        self.peers
            .values()
            .filter(|e| self.is_online(e, now) && e.manifest_fp != 0 && e.percent >= 100)
            .map(|e| e.manifest_fp)
            .collect()
    }

    /// (online, total). Total is every member *known* — heard this session or
    /// remembered from earlier ones — so a restart doesn't shrink "2 of 5" to
    /// "2 of 2" until everyone happens to be heard again.
    fn counts(&self) -> (u32, u32) {
        let now = now_secs();
        let online = self
            .peers
            .values()
            .filter(|e| self.is_online(e, now))
            .count() as u32;
        let remembered_only = self
            .remembered
            .keys()
            .filter(|id| !self.peers.contains_key(*id))
            .count();
        (online, (self.peers.len() + remembered_only) as u32)
    }

    fn infos(&self) -> Vec<seed_ipc::PeerInfo> {
        let now = now_secs();
        let mut out: Vec<seed_ipc::PeerInfo> = self
            .peers
            .iter()
            .map(|(id, e)| {
                // A member heard this session but not (yet) via presence — e.g.
                // discovered through doc-sync — still gets its last-known
                // identity instead of degrading to a bare endpoint id.
                let m = self.remembered.get(id);
                seed_ipc::PeerInfo {
                    node_id: id.chars().take(16).collect(),
                    name: e
                        .name
                        .clone()
                        .or_else(|| m.map(|m| m.name.clone()).filter(|n| !n.is_empty())),
                    role: e
                        .role
                        .or_else(|| m.map(|m| role_from_master(m.master)))
                        .unwrap_or(seed_ipc::Role::Viewer),
                    online: self.is_online(e, now),
                    last_seen: e.last_seen.max(m.map(|m| m.last_seen).unwrap_or(0)),
                    have_seqno: e.seqno,
                    percent: e.percent,
                    manifest_fp: e.manifest_fp,
                    unhealthy_secs: 0, // filled by Engine::peers from the health tracks
                    path: None,        // filled by Engine::annotate_peer_paths
                    last_dial_ok: e.last_dial_ok,
                    last_dial_err: e.last_dial_err.as_ref().map(|(m, _)| m.clone()),
                    last_dial_err_at: e.last_dial_err.as_ref().map(|(_, t)| *t).unwrap_or(0),
                }
            })
            .collect();
        // Members not heard at all this session (typically: since our restart)
        // stay listed under their last-known identity, offline.
        for (id, m) in &self.remembered {
            if self.peers.contains_key(id) || m.name.is_empty() {
                continue;
            }
            out.push(seed_ipc::PeerInfo {
                node_id: id.chars().take(16).collect(),
                name: Some(m.name.clone()),
                role: role_from_master(m.master),
                online: false,
                last_seen: m.last_seen,
                have_seqno: 0,
                percent: 0,
                manifest_fp: 0,
                unhealthy_secs: 0,
                path: None,
                last_dial_ok: 0,
                last_dial_err: None,
                last_dial_err_at: 0,
            });
        }
        out
    }

    /// Full-fidelity view for the health detector: unlike [`infos`](Self::infos)
    /// the node id is NOT truncated, because health episodes are keyed and
    /// persisted by the full endpoint-id string.
    fn health_snapshot(&self) -> Vec<PeerSnapshot> {
        let now = now_secs();
        self.peers
            .iter()
            .map(|(id, e)| PeerSnapshot {
                id: id.clone(),
                online: self.is_online(e, now),
                is_master: e.role == Some(seed_ipc::Role::Master),
                percent: e.percent,
                manifest_fp: e.manifest_fp,
                name: e.name.clone(),
            })
            .collect()
    }
}

/// One roster entry as the health detector sees it (full node id).
struct PeerSnapshot {
    id: String,
    online: bool,
    is_master: bool,
    percent: u8,
    manifest_fp: u64,
    name: Option<String>,
}

use crate::identity::{Role, ShareKey};
use crate::node::IrohNode;
use crate::scan::{self, IgnoreSet};

/// All reserved doc keys share the `\x00` control prefix so they never collide
/// with user file paths (relative POSIX strings, never starting with NUL).
const CONTROL_PREFIX: u8 = 0;
/// Prefix for the replicated, master-written ignore list: `\x00i/<CBOR
/// Vec<String>>` with a non-empty marker value. Like the member registry
/// (`\x00m/`), the list is encoded **in the key**, not the value — the engine
/// disables iroh-docs' content auto-downloader per replica, so the old
/// `\x00ignore` `set_bytes` form (list stored as a *value blob*) never reached
/// peers: viewers silently fell back to their *local* list and could delete
/// files a master ignored (known-issues #14). Key bytes ride doc-sync metadata,
/// so the list reaches every peer that syncs the doc. Across masters it is
/// last-writer-wins on the entry timestamp: each distinct list is a distinct
/// key and the reader takes the freshest entry under the prefix (superseded
/// lists are left behind and simply lose the timestamp comparison, like member
/// renames). Old readers skip unknown control keys, so this is wire-compatible;
/// any legacy `\x00ignore` entry is ignored and harmlessly orphaned.
const IGNORE_PREFIX: &[u8] = b"\x00i/";
/// Prefix for empty-file markers: `\x00e/<relpath>` with a non-empty marker value.
/// iroh-docs filters 0-byte entries out of queries as deletion markers, so a real
/// empty file can't ride a normal entry — it gets its own (non-empty) control key.
const EMPTY_PREFIX: &[u8] = b"\x00e/";
/// Prefix for delete tombstones: `\x00t/<relpath>` with a non-empty marker value.
/// A plain iroh-docs deletion reads as *absence*, which is indistinguishable from
/// "never seen" — so a master that hadn't yet published a path (mid initial seed)
/// would meet the file on disk, see nothing in the replica, and re-publish it,
/// resurrecting a concurrent delete fleet-wide (known-issues #12). The tombstone
/// entry's own record timestamp is the delete time, letting delete-vs-edit resolve
/// by LWW: a local file NEWER than the tombstone is a legitimate edit-after-delete
/// and republishes (clearing the tombstone); an older one is deleted. Old readers
/// skip unknown control keys, so this is wire-compatible.
const TOMBSTONE_PREFIX: &[u8] = b"\x00t/";
/// Prefix for member-registry records: `\x00m/<CBOR MemberRecord>` with a
/// non-empty marker value. The record (endpoint id + display name + role) is
/// encoded **in the key** — entry *content* can't be relied on, because the
/// engine disables iroh-docs' content auto-downloader per replica and nothing
/// would fetch the value blob (see the `set_download_policy` note in
/// `open_share`). Keys ride doc-sync metadata, so a member's last-known name
/// reaches every peer that syncs the doc, even one that never heard the member's
/// presence gossip. Masters write records (their own, plus members they hear
/// live via presence — viewers hold a read-only capability and can't write their
/// own), and only at the END of a reconcile pass whose replica has proven
/// contact with share state — a write during a virgin replica's initial sync
/// can churn the session and re-open the known-issues #12 delete-resurrection
/// race (see [`ReconcileJob::publish_member_records`]). Every member reads
/// them. Freshest entry timestamp per endpoint id wins;
/// superseded keys (renames) are left behind and simply lose the timestamp
/// comparison — a handful of ~100-byte keys per rename, never deleted (`del` is
/// prefix deletion, known-issues #13, and another master's record can't be
/// deleted anyway). Old readers skip unknown control keys, so this is
/// wire-compatible.
const MEMBER_PREFIX: &[u8] = b"\x00m/";

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

/// Bound on the post-resume `Endpoint::network_change` rebind so a wedged endpoint
/// can't stall the reconcile loop that drives [`Engine::on_resume`].
const RESUME_NETWORK_CHANGE_TIMEOUT_SECS: u64 = 10;

/// How long a manifest disagreement with an online peer must persist before the
/// share is reported "out of sync". Long enough that normal propagation lag after
/// a change (the doc replicating across members) settles without a false alarm.
const DIVERGENCE_SETTLE_SECS: i64 = 45;

/// Periodic "deep verify": how often a share is forced to do a full hashing scan
/// (disk-vs-manifest), independent of the cheap change-signature, to catch drift the
/// signature can't see (in-place corruption with unchanged size+mtime, a stale index
/// after a crash). Low frequency — it re-hashes the whole folder.
const DEEP_VERIFY_INTERVAL_SECS: i64 = 4 * 3600;

/// How long a share must stay out of sync before the self-heal escalates from the
/// cheap paths (per-tick re-materialization + the daemon's ~6s doc-resync kicks) to
/// ONE forced deep verify for the episode. Long enough that a slow-but-converging
/// peer never costs a full rehash of a multi-GB share.
const DIVERGENCE_DEEP_VERIFY_SECS: i64 = 600;

/// Upper bound on the two iroh-docs reads a reconcile pass makes before its
/// merge loop (the ignore-list lookup and the full `get_many` manifest read).
/// Both normally finish in milliseconds; the fleet soak caught them wedging
/// FOREVER on ~1/3 of a 28-node fleet (known-issues #7: the docs actor stops
/// answering under sustained peer-session pressure), which held the share's
/// `publishing` guard and silently stopped it from birth. A bounded failure
/// turns that into a WARN + clean pass failure retried next tick — the share
/// stays visibly unhealthy (health alerts fire) instead of invisibly dead.
/// Generous: ~100× the worst legitimate read on the target corpus.
const DOC_READ_TIMEOUT_SECS: u64 = 120;

/// Minimum spacing between doc-resync kicks for one out-of-sync share. The kick
/// (`doc.start_sync`) restarts a possibly-stalled replication session, and every
/// session runs iroh-docs set reconciliation on BOTH ends — so issuing one per
/// share every ~6s (the daemon's ask cadence) from every diverged member is an
/// O(N²) CPU storm at fleet scale. The first 28-node soak after the mesh fix
/// measured it directly: 1200+ kicks in 17 min, daemons at 300–1400% CPU,
/// presence beats starved past the online TTL, roster collapse. Live doc sync
/// keeps replicating on its own between kicks; this only bounds the *repair
/// nudge* rate.
const DIVERGENCE_RESYNC_KICK_SECS: i64 = 30;

/// Cap on how many peers one doc-resync kick syncs against, sampled at random
/// from the known members. Set reconciliation is pairwise — any one peer that
/// holds the newer entries heals us — so syncing with all ~27 members per kick
/// buys nothing over a few and multiplies the fleet-wide session count by the
/// member count. Same bounded-repair philosophy as [`PRESENCE_REJOIN_SAMPLE`].
const DOC_RESYNC_SAMPLE: usize = 3;

/// Provable-partition self-heal (known-issues #23). A share
/// that can reach **no** member while it *has* members to reach, continuously for
/// longer than this, is treated as a real partition (not normal churn) and the
/// recovery ladder engages: a loud WARN plus the endpoint-wide public-relay
/// fallback (in case a custom relay is silently blackholing). Generous on purpose —
/// rendezvous, mesh rejoin, and doc resync all get first crack well inside it.
const ISOLATION_HEAL_SECS: i64 = 120;

/// Second rung of the ladder: once partitioned this long, rebuild the share's
/// gossip/presence subscription and re-kick doc sync. The presence overlay does
/// not always re-form on its own after a prolonged partition even once transport
/// recovers — observed in the field, where presence stayed dead until a restart.
const ISOLATION_PRESENCE_REBUILD_SECS: i64 = 210;

/// Presence-overlay self-heal, the transport-alive twin of the isolation ladder
/// (known-issues #23). After a partition, doc-sync can
/// recover (successful `SyncFinished` events mark peers online) while the gossip
/// presence overlay stays silently dead — the swarm reports healthy (subscribe
/// alive, `join_peers`/broadcast return Ok) yet delivers no beats, so peers stick
/// at `seqno=0` and flap on the TTL, and only a fresh subscription heals it. Since
/// doc-sync keeps the share *non-isolated*, the isolation ladder above never fires;
/// this one keys on presence staleness instead.
///
/// "Transport is alive" = we've had genuine peer contact (doc-sync or presence)
/// within this window.
const PRESENCE_TRANSPORT_FRESH_SECS: i64 = 60;

/// "The overlay isn't delivering" = no presence heartbeat heard within this window
/// (presence is broadcast ~every 3s, so a healthy overlay never approaches it).
const PRESENCE_HEARD_TTL_SECS: i64 = 20;

/// How long "transport alive but presence silent" must persist before rebuilding
/// the subscription. Long enough that normal startup (presence arrives in seconds)
/// and brief gossip hiccups never trigger a rebuild.
const PRESENCE_GAP_HEAL_SECS: i64 = 90;

/// Rebuild the presence subscription at most this often per share — a shared
/// throttle across both the isolation and presence-gap ladders, so a stubborn
/// overlay retries periodically without thrashing the gossip actor.
const PRESENCE_REBUILD_MIN_SECS: i64 = 90;

/// A self-heal fault episode ends only after its condition has read healthy for
/// this long *continuously* ([`EpisodeClock`]; known-issues #35). Must comfortably
/// exceed [`PEER_ONLINE_TTL_SECS`]: a single delivered presence beat reads healthy
/// for one full TTL, so anything shorter lets a ~25s flap clear the episode on
/// every blip and permanently disarm the ladders. 3× the TTL; the cost of the
/// hysteresis on a genuine recovery is at most one redundant (idempotent) rebuild.
const HEAL_CLEAR_SECS: i64 = 60;

/// Transport-repair ladder (known-issues #36). The two #23 ladders repair
/// things *above* the transport (gossip subscription, doc-sync session, relay
/// map); none of them can help when the iroh endpoint itself has stopped being
/// able to reach a member — the field signature was a daemon that could not
/// dial a peer for days while a fresh endpoint on the same host reached it in
/// 0.3 s. The only remedy that ever worked was a restart, so this ladder does
/// the restart's work in-process, in two rungs, and only when the peer is
/// provably alive (so a genuinely offline fleet never churns the endpoint).
///
/// Rung 1 after this long of "alive but every outbound dial fails": re-probe
/// the network (`Endpoint::network_change`, the same rebind `on_resume` does)
/// and re-kick the share.
const OUTBOUND_DEAD_SECS: i64 = 300;
/// Rung 2: if rung 1 did not restore outbound dials within another
/// [`OUTBOUND_DEAD_SECS`], rebuild the whole iroh node in-process
/// ([`Engine::rebuild_transport`]).
const OUTBOUND_REBUILD_SECS: i64 = 600;
/// Consecutive failed outbound dials before the condition counts at all — a
/// single timeout is weather.
const OUTBOUND_MIN_FAILURES: u32 = 3;
/// "Provably alive" = any genuine contact from a member, a doc-sync a member
/// initiated toward us, or a fresh rendezvous record from another master,
/// within this window.
const PEER_ALIVE_WINDOW_SECS: i64 = 600;
/// Rebuilds are spaced at least this far apart, doubling while the fault
/// persists up to [`TRANSPORT_REBUILD_MAX_SECS`]; the spacing resets once no
/// share is in an outbound-dead episode.
const TRANSPORT_REBUILD_MIN_SECS: i64 = 900;
const TRANSPORT_REBUILD_MAX_SECS: i64 = 7200;
/// After this many consecutive *failed* rebuilds the engine reports a fatal
/// transport ([`Engine::transport_fatal`]) and the daemon exits for its
/// supervisor (service recovery / `systemd` `Restart=on-failure`) — rung 3.
const TRANSPORT_REBUILD_MAX_FAILURES: u32 = 2;

/// Stall watchdog: an in-flight download older than this is presumed wedged and is
/// aborted so the next reconcile re-queues it (verified chunks persist on disk, so
/// a healthy-but-slow fetch that gets recycled resumes where it left off — the cost
/// is one connection re-setup, the win is that a hung future can't block its blob
/// forever). Swarm attempts already self-bound at [`SWARM_DEADLINE_SECS`] per try;
/// this bounds the task *around* the retries, catching hangs the deadline can't.
const DOWNLOAD_STALL_ABORT_SECS: u64 = 900;

/// Back-pressure on content fetches: at most this many downloads in flight at
/// once (globally — content is hash-addressed and the map is shared). Without a
/// cap, a share with thousands of missing files queues a task per blob into the
/// iroh downloader simultaneously; the multi-GB blobs head-of-line-block the
/// rest, nothing settles, and the fleet reads 0% for hours (full-size soak #3:
/// the stall watchdog recycled 5000+ downloads that were queued, not moving).
/// The reconcile tick re-offers still-missing blobs constantly, so free slots
/// refill within a tick — this needs no queue of its own, and it restores the
/// watchdog's meaning (a capped slot idle 15 min really is wedged).
const MAX_INFLIGHT_DOWNLOADS: usize = 12;

/// Of the [`MAX_INFLIGHT_DOWNLOADS`] slots, at most this many may hold LARGE
/// blobs (≥ [`SWARM_MIN_SIZE`]) at once. Many concurrent multi-GB downloads
/// interleave writes at scattered offsets across several huge files — a
/// workload that collapses spinning disks into seek thrash (measured: one
/// lone HDD node syncs at ~35–40 MiB/s, six contending nodes at ~1.5 MiB/s
/// each; within one node the same physics applies to its own concurrent
/// ISOs). Two at a time keeps per-file writes mostly sequential and finishes
/// individual files sooner (better for swarm part-trading too), while the
/// remaining slots keep small files flowing.
const MAX_INFLIGHT_LARGE: usize = 2;

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

/// Read a master seed from the OS keystore without letting it block or hang the
/// caller. The mirror of [`store_seed_bounded`], and needed for the same reason: the
/// keystore call is synchronous, and under the Windows LocalSystem service (session 0)
/// the Credential Manager API can hang indefinitely. The startup path used to call
/// `secrets::load_seed` directly on the runtime, so a wedged keystore could stall
/// daemon startup rather than merely fail it.
async fn load_seed_bounded(share_id: &str) -> anyhow::Result<[u8; 32]> {
    let share_id = share_id.to_owned();
    let handle = tokio::task::spawn_blocking(move || crate::secrets::load_seed(&share_id));
    match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
        Ok(Ok(inner)) => inner,
        Ok(Err(join_err)) => Err(anyhow!("keystore task failed: {join_err}")),
        Err(_) => Err(anyhow!("keystore read timed out after 5s")),
    }
}

/// A master share whose write key the OS keystore would not give us. Held inert (see
/// [`Engine::locked`]) rather than opened read-only, and retried.
struct LockedShare {
    record: crate::db::ShareRecord,
    /// Unix seconds of the last keystore retry; throttles it to one per
    /// [`KEY_RETRY_SECS`].
    last_retry: i64,
    /// Why the key is unavailable, for the UI and the log (e.g. "unlock prompt was
    /// dismissed").
    reason: String,
}

/// How often a locked master share re-asks the OS keystore for its write key. The
/// keyring typically unlocks on graphical login, minutes to hours after a headless
/// boot, so this must keep trying for the life of the process — but cheaply.
const KEY_RETRY_SECS: i64 = 30;

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

/// Every endpoint id we could dial for a share: the creating master (its id is
/// carried in the share key) first, then every member the roster knows — heard this
/// session *or* remembered from an earlier one.
///
/// The remembered half matters more than it looks. The creator's id is the only
/// thing a bare share key carries, so before known-issues #16 this set collapsed to
/// exactly one device whenever the roster was cold — and a joiner or a just-restarted
/// node whose creator happened to be offline had nowhere to dial at all, no matter
/// how many other masters were up. `peer_names` had persisted every member's full
/// endpoint id all along; it was simply never read for dialing. Including it here
/// fixes every case *except* the first-ever join (where there is nothing remembered
/// yet) — that one needs [`crate::rendezvous`].
///
/// Feeds presence mesh repair, doc resync, and content self-heal. Offline entries
/// are harmless: the dial just fails.
fn peer_providers(key: &ShareKey, roster: &Arc<StdMutex<PeerRoster>>) -> Vec<EndpointId> {
    let mut ids = Vec::new();
    if let Some(eid) = key.endpoint_id() {
        if let Ok(id) = EndpointId::from_bytes(&eid) {
            ids.push(id);
        }
    }
    if let Ok(r) = roster.lock() {
        for s in r.known_peer_ids() {
            if let Ok(id) = s.parse::<EndpointId>() {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

/// Whether a share's gossip **presence** overlay is dead while its transport is
/// alive (the ladder-2 condition of [`Engine::connectivity_recoveries`]): we have
/// members to hear, we are not totally isolated, we've had genuine peer contact
/// recently (doc-sync or presence), yet no presence heartbeat has arrived within
/// the TTL. Pure so the decision is unit-testable without a live gossip actor.
///
/// The sustained-duration guard ([`PRESENCE_GAP_HEAL_SECS`]) lives in the caller,
/// so this only reports the instantaneous "bad" condition — at startup, presence
/// normally arrives within seconds and the episode never matures.
fn presence_overlay_dead(
    known: u32,
    isolated: bool,
    last_contact: i64,
    last_presence: i64,
    now: i64,
) -> bool {
    known > 0
        && !isolated
        && last_contact != 0
        && now - last_contact <= PRESENCE_TRANSPORT_FRESH_SECS
        && (last_presence == 0 || now - last_presence > PRESENCE_HEARD_TTL_SECS)
}

/// Whether a share's members are provably alive while every doc-sync dial *we*
/// make fails (the ladder-3 condition of [`Engine::connectivity_recoveries`],
/// known-issues #36). Pure so it is unit-testable.
///
/// "Alive" is any of: genuine contact from a member (`last_contact`: presence,
/// a remote insert, a neighbor-up), a sync a member initiated toward us that
/// succeeded, or the rendezvous resolving to another master with a fresh
/// record — all within [`PEER_ALIVE_WINDOW_SECS`]. "Dials fail" is the most
/// recent outbound sync error being newer than the last outbound success with
/// at least [`OUTBOUND_MIN_FAILURES`] consecutive failures. The sustained-time
/// guard lives in the caller's [`EpisodeClock`].
fn outbound_dead(known: u32, last_contact: i64, dial: &DialStats, now: i64) -> bool {
    let Some((_, err_at)) = dial.last_outbound_err.as_ref() else {
        return false;
    };
    let alive = [last_contact, dial.last_inbound_ok, dial.rendezvous_alive]
        .iter()
        .any(|t| *t != 0 && now - *t <= PEER_ALIVE_WINDOW_SECS);
    known > 0
        && *err_at > dial.last_outbound_ok
        && dial.outbound_failures >= OUTBOUND_MIN_FAILURES
        && alive
}

/// Rebuild one share's gossip/presence subscription from its current known-member
/// bootstrap, replacing (and so aborting) the old handle. The remedy for a wedged
/// presence overlay that reports healthy but delivers nothing — the in-process
/// equivalent of the fresh subscription a restart gives it. Subscribing is a local
/// gossip-actor hand-off (not a network dial), so it is safe to await under the
/// engine lock, exactly as [`Engine::open_share`] does. Returns whether it
/// succeeded (for the caller's log line).
async fn rebuild_presence(
    gossip: &iroh_gossip::net::Gossip,
    self_id: EndpointId,
    share_id: &str,
    s: &mut ShareState,
) -> bool {
    let bootstrap: Vec<EndpointId> = s
        .roster
        .lock()
        .map(|r| {
            r.known_peer_ids()
                .iter()
                .filter_map(|x| x.parse::<EndpointId>().ok())
                .filter(|pk| *pk != self_id)
                .collect()
        })
        .unwrap_or_default();
    let topic = crate::presence::presence_topic(&s.key.share_id());
    match crate::presence::spawn_presence(gossip, topic, bootstrap, self_id, s.roster.clone()).await
    {
        Ok(h) => {
            // Replacing the handle drops the old one, aborting its (stalled) task.
            s.presence = Some(h);
            true
        }
        Err(e) => {
            tracing::warn!("presence rebuild for {share_id} failed: {e:#}");
            false
        }
    }
}

/// Pick which peers a presence rejoin should ask the swarm to connect: the
/// candidates the roster has NOT heard from within the online TTL, capped at
/// [`PRESENCE_REJOIN_SAMPLE`] chosen uniformly at random. A peer we can't hear
/// is either partitioned from us — exactly what a join repairs — or genuinely
/// down, where the dial fails harmlessly. Peers we already hear are never
/// dialed: re-joining a live member does nothing for delivery and only churns
/// gossip's bounded active view (the known-issues #9 fragmentation). Returns
/// empty when every candidate is heard — a converged mesh needs no repair, the
/// swarm's own shuffle maintains it from there.
fn select_rejoin_targets<R: Rng>(
    rng: &mut R,
    candidates: Vec<EndpointId>,
    online: &HashSet<String>,
) -> Vec<EndpointId> {
    let mut unheard: Vec<EndpointId> = candidates
        .into_iter()
        .filter(|id| !online.contains(&id.to_string()))
        .collect();
    unheard.shuffle(rng);
    unheard.truncate(PRESENCE_REJOIN_SAMPLE);
    unheard
}

/// How long a single self-heal dial may take before it is treated as a failure.
const SELF_HEAL_DIAL_SECS: u64 = 15;

/// Note a provider as unreachable for the remainder of the current reconcile pass.
fn mark_dead(dead: &StdMutex<HashSet<String>>, pid: EndpointId) {
    dead.lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(pid.to_string());
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
    dead: &StdMutex<HashSet<String>>,
) -> anyhow::Result<()> {
    if providers.is_empty() {
        anyhow::bail!("no known providers to repair {}", target.display());
    }
    // Providers that already failed to *connect* earlier in this same pass are
    // skipped rather than re-dialed (known-issues #34). Each dial costs up to
    // SELF_HEAL_DIAL_SECS, and a pass merges one path at a time, so re-dialing a
    // provider that has no addressing information turns a folder of N files into
    // N * 15s of pure stall — the fleet-visible symptom being a pass that never
    // returns while logging "will retry" forever. A provider is marked dead only
    // on a connect failure: one that connects but can't serve this blob is still
    // worth trying for the next one.
    let live: Vec<EndpointId> = {
        let d = dead.lock().unwrap_or_else(|e| e.into_inner());
        providers
            .iter()
            .copied()
            .filter(|pid| !d.contains(&pid.to_string()))
            .collect()
    };
    if live.is_empty() {
        anyhow::bail!(
            "all {} provider(s) unreachable this pass; repair of {} deferred",
            providers.len(),
            target.display()
        );
    }
    let mut last_err = None;
    for pid in live {
        // Bound the dial so a self-heal can't hang the whole reconcile pass if a
        // provider is unreachable/stalled (e.g. during a multi-master churn storm).
        let conn = match tokio::time::timeout(
            Duration::from_secs(SELF_HEAL_DIAL_SECS),
            endpoint.connect(EndpointAddr::new(pid), iroh_blobs::ALPN),
        )
        .await
        {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                mark_dead(dead, pid);
                last_err = Some(anyhow!("connect {pid}: {e}"));
                continue;
            }
            Err(_) => {
                mark_dead(dead, pid);
                last_err = Some(anyhow!("connect {pid}: timed out"));
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

/// Staging path for a self-heal download: the target's FULL file name with a
/// `.seedheal-tmp` suffix appended. Deliberately not `with_extension`, which
/// REPLACES the extension — so `a.bin` and `a.txt` would both stage through
/// `a.seedheal-tmp` and could collide (and the temp could shadow a real sibling
/// that happens to share the stem). Appending keeps the staging path unique per
/// file and preserves the original name.
fn heal_tmp_path(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".seedheal-tmp");
    target.with_file_name(name)
}

/// Stream a blob from a connection, writing its verified leaves into a temp file
/// next to `target`, then atomically replace `target`. Streams chunk-by-chunk
/// (no whole-file in memory), and the content is bao-verified against `hash` as
/// it arrives, so a bad peer cannot write wrong bytes.
async fn fetch_blob_to_file(conn: &Connection, hash: Hash, target: &Path) -> anyhow::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let tmp = heal_tmp_path(target);
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

/// Insert `entry` for `path`, resolving a collision between the content key `P`
/// and the empty-file marker `\x00e/P` by record timestamp (LWW), newer wins.
/// Within one author the two keys are mutually exclusive, but across two masters
/// a path that flips empty↔non-empty can have a live entry under *both* keys —
/// neither author deletes the other's. Deciding by stream order (the old
/// behavior) ignored which edit was newer and, worse, could make two members
/// with identical docs compute different merged views → different
/// `manifest_fingerprint`s → a false OutOfSync. Tie on equal ts breaks
/// deterministically: content beats the empty marker, then larger hash bytes —
/// never insertion order, so every member resolves identically.
fn insert_remote_lww(out: &mut HashMap<String, RemoteEntry>, path: String, entry: RemoteEntry) {
    use std::collections::hash_map::Entry;
    match out.entry(path) {
        Entry::Vacant(v) => {
            v.insert(entry);
        }
        Entry::Occupied(mut o) => {
            let cur = o.get();
            if (entry.ts, entry.size != 0, &entry.hash) > (cur.ts, cur.size != 0, &cur.hash) {
                o.insert(entry);
            }
        }
    }
}

/// The merged replica view: the desired fileset plus the delete tombstones that
/// currently WIN their path (no live content entry is newer). `tombstones` maps
/// path → delete time (record micros) so the merge can LWW a tombstone against a
/// local file's mtime — the known-issues #12 case where the path is absent from
/// `files` but "absent because deleted" must beat "on disk, so publish it".
struct RemoteView {
    files: HashMap<String, RemoteEntry>,
    tombstones: HashMap<String, u64>,
}

/// Resolve delete tombstones against the merged content view, in place:
/// a tombstone strictly NEWER than the path's live entry deletes it from
/// `files`; otherwise the content survives and the tombstone is dropped (ties
/// favor content — same anti-data-loss bias as the empty-marker tie-break).
/// Runs over complete maps, so the outcome is independent of doc stream order
/// and every member computes the same view (and thus the same fingerprint).
fn resolve_tombstones(
    files: &mut HashMap<String, RemoteEntry>,
    tombstones: &mut HashMap<String, u64>,
) {
    tombstones.retain(|path, tts| match files.get(path) {
        Some(re) if re.ts >= *tts => false, // content newer (or tie): delete loses
        Some(_) => {
            files.remove(path);
            true
        }
        None => true,
    });
}

/// Should a delete tombstone suppress (delete) a local file that is present on
/// disk but absent from the replica? Yes only when the file is *the exact
/// deleted content still lingering* — same hash as the tombstone AND not newer
/// than the delete. Different content at the same name is a genuine re-add and
/// must publish: a file mtime is not a reliable "when I re-added this" clock
/// (copy / extract-from-archive / download all preserve the source's older
/// mtime), so keying suppression on mtime alone deletes a legitimately replaced
/// file forever. `deleted_hash` is `None` for a legacy tombstone (no stored
/// hash) or one whose value blob hasn't synced yet → fall back to the
/// time-only rule, which self-corrects once the hash is known.
fn tombstone_suppresses(
    deleted_hash: Option<&[u8]>,
    local_hash: &[u8],
    mtime: u64,
    tts: u64,
) -> bool {
    let older = mtime <= tts;
    match deleted_hash {
        Some(h) => h == local_hash && older,
        None => older,
    }
}

/// Read the merged file view from the doc: latest-per-key, with deletion markers
/// already excluded by the query. Normal keys carry content; `\x00e/<path>` keys
/// mark empty files; `\x00t/<path>` keys are delete tombstones (resolved against
/// content by [`resolve_tombstones`]); other control keys (e.g. `\x00ignore`) are
/// skipped. A path live under both content keyspaces resolves by LWW via
/// [`insert_remote_lww`].
async fn read_remote_files(doc: &Doc) -> anyhow::Result<RemoteView> {
    let mut files = HashMap::new();
    let mut tombstones: HashMap<String, u64> = HashMap::new();
    let mut s = std::pin::pin!(doc.get_many(Query::single_latest_per_key()).await?);
    while let Some(e) = s.next().await {
        let e = e?;
        let key = e.key();
        if key.first() == Some(&CONTROL_PREFIX) {
            if let Some(rel) = key.strip_prefix(EMPTY_PREFIX) {
                if let Ok(path) = std::str::from_utf8(rel) {
                    insert_remote_lww(
                        &mut files,
                        path.to_string(),
                        RemoteEntry {
                            hash: Hash::EMPTY.as_bytes().to_vec(),
                            size: 0,
                            ts: e.timestamp(),
                        },
                    );
                }
            } else if let Some(rel) = key.strip_prefix(TOMBSTONE_PREFIX) {
                if let Ok(path) = std::str::from_utf8(rel) {
                    let ts = e.timestamp();
                    tombstones
                        .entry(path.to_string())
                        .and_modify(|t| *t = (*t).max(ts))
                        .or_insert(ts);
                }
            }
            continue;
        }
        let Ok(path) = std::str::from_utf8(key) else {
            continue;
        };
        insert_remote_lww(
            &mut files,
            path.to_string(),
            RemoteEntry {
                hash: e.content_hash().as_bytes().to_vec(),
                size: e.content_len(),
                ts: e.timestamp(),
            },
        );
    }
    resolve_tombstones(&mut files, &mut tombstones);
    Ok(RemoteView { files, tombstones })
}

/// Encode an ignore list into its doc key: `\x00i/` + CBOR. Equal lists encode
/// to equal keys, so republishing an unchanged list is idempotent.
fn ignore_list_key(list: &[String]) -> Vec<u8> {
    let mut k = IGNORE_PREFIX.to_vec();
    // A list of strings is infallible to serialize.
    let _ = ciborium::into_writer(list, &mut k);
    k
}

/// Decode a `\x00i/` doc key back into an ignore list (`None`: not an ignore
/// key, or a future encoding this version can't read).
fn decode_ignore_list(key: &[u8]) -> Option<Vec<String>> {
    let tail = key.strip_prefix(IGNORE_PREFIX)?;
    ciborium::from_reader(tail).ok()
}

/// Read the replicated ignore list (`\x00i/…`), if a master has published one.
/// The list rides the doc *key* (see [`IGNORE_PREFIX`]); across masters the
/// freshest entry by record timestamp wins (LWW). `None` means no master has
/// published a list — the caller falls back to the locally-configured one.
async fn read_ignore_list(doc: &Doc) -> anyhow::Result<Option<Vec<String>>> {
    let mut best: Option<(Vec<String>, u64)> = None;
    let mut s = std::pin::pin!(
        doc.get_many(Query::single_latest_per_key().key_prefix(IGNORE_PREFIX))
            .await?
    );
    while let Some(e) = s.next().await {
        let e = e?;
        let Some(list) = decode_ignore_list(e.key()) else {
            continue;
        };
        let ts = e.timestamp();
        match &best {
            Some((_, t)) if *t >= ts => {}
            _ => best = Some((list, ts)),
        }
    }
    Ok(best.map(|(list, _)| list))
}

/// Map a remembered `master` flag back to the IPC role enum.
fn role_from_master(master: bool) -> seed_ipc::Role {
    if master {
        seed_ipc::Role::Master
    } else {
        seed_ipc::Role::Viewer
    }
}

/// Current member-record format version, for forward-compat (readers skip keys
/// they can't decode, so a future shape bump is wire-safe).
const MEMBER_RECORD_V: u8 = 1;

/// One member-registry record, CBOR-encoded into a `\x00m/` doc key (see
/// [`MEMBER_PREFIX`] for why the key, not the value, carries the data).
///
/// Trust: like presence, a record's name/role is claimed, not proven — any
/// master-key holder can write any member's record (the replica signs entries
/// with the shared namespace key, not per-device). That matches the share trust
/// model: masters are trusted with the folder's *content*, so trusting them
/// with display names adds nothing new. File content stays verified.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct MemberRecord {
    /// Format version.
    v: u8,
    /// The member's endpoint id (raw bytes).
    id: [u8; 32],
    /// The member's self-chosen display name, as last heard via presence.
    name: String,
    /// Whether the member holds a master key (writes) or is a viewer (mirrors).
    master: bool,
}

/// Encode a member record into its doc key: `\x00m/` + CBOR. Equal records
/// encode to equal keys, so republishing an unchanged identity is idempotent.
fn member_record_key(rec: &MemberRecord) -> Vec<u8> {
    let mut k = MEMBER_PREFIX.to_vec();
    // A struct of scalars + String is infallible to serialize.
    let _ = ciborium::into_writer(rec, &mut k);
    k
}

/// Decode a `\x00m/` doc key back into a member record (`None`: not a member
/// key, or a future format this version can't read).
fn decode_member_record(key: &[u8]) -> Option<MemberRecord> {
    let tail = key.strip_prefix(MEMBER_PREFIX)?;
    ciborium::from_reader(tail).ok()
}

/// A member identity from the doc's member registry, resolved to the roster's
/// key form: full endpoint-id string, display name, role, and the doc entry's
/// timestamp (unix secs) — the freshest record per member.
pub(crate) struct MemberIdentity {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) master: bool,
    pub(crate) ts_secs: i64,
}

/// Read the doc's member registry: the freshest record per endpoint id.
/// Superseded keys (old names) lose the timestamp comparison and are ignored.
async fn read_member_records(doc: &Doc) -> anyhow::Result<Vec<MemberIdentity>> {
    let mut best: HashMap<[u8; 32], (MemberRecord, u64)> = HashMap::new();
    let mut s = std::pin::pin!(
        doc.get_many(Query::single_latest_per_key().key_prefix(MEMBER_PREFIX))
            .await?
    );
    while let Some(e) = s.next().await {
        let e = e?;
        let Some(rec) = decode_member_record(e.key()) else {
            continue;
        };
        let ts = e.timestamp();
        match best.get(&rec.id) {
            Some((_, t)) if *t >= ts => {}
            _ => {
                best.insert(rec.id, (rec, ts));
            }
        }
    }
    Ok(best
        .into_values()
        .filter_map(|(rec, ts)| {
            let id = EndpointId::from_bytes(&rec.id).ok()?;
            Some(MemberIdentity {
                id: id.to_string(),
                name: rec.name,
                master: rec.master,
                // Doc record timestamps are wall-clock micros (the LWW clock).
                ts_secs: (ts / 1_000_000) as i64,
            })
        })
        .collect())
}

/// Whether a scan is a *forced* deep verify running against an unchanged
/// (path, size, mtime) folder signature — the condition under which any surfaced
/// content-hash change is silent in-place corruption rather than a normal edit
/// (known-issues #13). A signature-driven scan (`force_scan == false`), or a
/// forced one where the signature itself moved, reflects a legitimate metadata
/// change and rides ordinary change detection, so it is not this signal.
fn is_silent_corruption_scan(force_scan: bool, quick_sig: u64, last_quick_sig: u64) -> bool {
    force_scan && quick_sig == last_quick_sig
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
) -> (Vec<EndpointId>, usize, Option<EndpointId>) {
    // (peer, self-reported percent). Percent is the peer's own health claim from
    // presence — a serving-capability heuristic, not a guarantee (it's measured
    // against the manifest the peer *knows*), so it orders candidates and gates
    // master participation but never removes anyone from the fallback chain.
    let mut peers: Vec<(EndpointId, u8)> = Vec::new();
    if let Ok(r) = roster.lock() {
        for (s, percent) in r.online_peer_percents() {
            if let Ok(id) = s.parse::<EndpointId>() {
                if id != self_id && Some(id) != master_id && !peers.iter().any(|(p, _)| *p == id) {
                    peers.push((id, percent));
                }
            }
        }
    }
    peers.shuffle(&mut rand::thread_rng());
    // Fully-synced peers first (they can serve anything); stable sort keeps the
    // shuffle within each group so load still spreads.
    peers.sort_by_key(|(_, pct)| std::cmp::Reverse(*pct >= 100));
    let full_peers = peers.iter().filter(|(_, pct)| *pct >= 100).count();
    let master = master_id.filter(|m| *m != self_id);
    (
        peers.into_iter().map(|(id, _)| id).collect(),
        full_peers,
        master,
    )
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
    let (peers0, _, _) = live_providers_from(roster, self_id, master_id);
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
            let (peers, full_peers, master) = live_providers_from(roster, self_id, master_id);
            let mut set = tokio::task::JoinSet::new();
            for (idx, &(lo, hi)) in ranges.iter().enumerate() {
                // Master participation policy (the original seeder must neither
                // be hammered nor become a single point of slowness):
                //  - before this part's grace elapses: peers only (cold-start
                //    relief), unless there are no peers at all;
                //  - after grace, while FEWER than 3 fully-synced peers exist:
                //    the master joins the rotation as an EQUAL candidate —
                //    appended-last meant the first finisher became the fleet's
                //    sole seeder while the master sat idle (soak finding);
                //  - once ≥3 fully-synced peers can do the same job: the master
                //    drops out entirely and the peer swarm carries it;
                //  - desperation valve: a part still incomplete several rounds
                //    past its grace re-admits the master as a last-resort tail.
                //    "Fully synced" is self-reported against the manifest a peer
                //    KNOWS, so a fresh master-authored blob may exist nowhere
                //    else — without this valve that blob could never propagate.
                let mut candidates: Vec<EndpointId> = peers.clone();
                let rotate_master_in = peers.is_empty() || (round >= grace[idx] && full_peers < 3);
                if rotate_master_in {
                    if let Some(m) = master {
                        if !candidates.contains(&m) {
                            candidates.push(m);
                        }
                    }
                }
                if candidates.is_empty() {
                    continue;
                }
                // Primary rotation pool: members that can serve ANY range — the
                // fully-synced peers (sorted first in `peers`) plus the master
                // when rotated in. Rotating primaries through *partial* peers
                // hammered them with range requests they mostly had to reject
                // (25 requesters × 16 parts × 400 ms rounds), which is the
                // request storm behind the provider-side OOM (known-issues
                // #11). Partial peers stay in the fallback chain so part
                // trading still works.
                //
                // The restriction only applies when at least one FULL peer is
                // actually known: a peer whose percent hasn't gossiped yet
                // reads 0, and with `full_peers == 0` the pool would
                // degenerate to just the master — possibly offline —
                // collapsing every part onto one fallback order and defeating
                // the swarm split (caught by
                // `large_blob_swarms_across_two_seeders`). With no
                // confirmed-full peer the rotation uses everyone: exactly the
                // desired cold-start part-trading phase.
                let mut servable: Vec<EndpointId> = peers[..full_peers.min(peers.len())].to_vec();
                if rotate_master_in {
                    if let Some(m) = master {
                        if !servable.contains(&m) {
                            servable.push(m);
                        }
                    }
                }
                let pool: &[EndpointId] = if full_peers == 0 || servable.is_empty() {
                    &candidates
                } else {
                    &servable
                };
                let primary = pool[idx % pool.len()];
                let mut plist = vec![primary];
                plist.extend(candidates.iter().copied().filter(|p| *p != primary));
                if !rotate_master_in && round >= grace[idx] + SWARM_MASTER_GRACE_ROUNDS {
                    if let Some(m) = master {
                        if !plist.contains(&m) {
                            plist.push(m);
                        }
                    }
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

/// Deterministic fingerprint of the merged "desired files" view — the latest-per-
/// path `(path → content-hash)` set. Two members whose manifests fully agree compute
/// the SAME value; any disagreement about which files exist (or their hashes) yields
/// a different value. It's over the manifest only (not download progress), so a file
/// still transferring doesn't change it. Returns a non-zero u64 (even for an empty
/// view), reserving `0` as the "unknown / not reported" sentinel on the wire.
fn manifest_fingerprint(remote: &HashMap<String, RemoteEntry>) -> u64 {
    let mut entries: Vec<(&str, &[u8])> = remote
        .iter()
        .map(|(p, re)| (p.as_str(), re.hash.as_slice()))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut h = blake3::Hasher::new();
    h.update(b"seed-sync/manifest-fp/v1");
    for (path, hash) in entries {
        h.update(path.as_bytes());
        h.update(&[0u8]);
        h.update(hash);
        h.update(&[0u8]);
    }
    let fp = u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().unwrap());
    // Never return the 0 sentinel (vanishingly unlikely, but keep it impossible).
    if fp == 0 {
        1
    } else {
        fp
    }
}

/// The manifest fingerprint to **advertise** in presence: the real fingerprint of
/// `remote` once our replica has proven contact with the share (`replica_seen`),
/// else the `0` sentinel meaning "unknown / not yet computed".
///
/// Why the gate exists (known-issues #19). A node freshly added to a share has not
/// synced the doc replica yet, so its merged `remote` is EMPTY — and an empty
/// manifest fingerprints to a perfectly valid nonzero value, *and* reports
/// `health == 100` (100% of nothing; `total_bytes == 0` in [`ReconcileJob::run`]).
/// So before this gate a just-joined node broadcast `percent = 100` + `FP_EMPTY`,
/// and every settled peer read it as a fully-synced member whose fileset disagreed,
/// tripping a false OutOfSync within the settle window the moment a member joined.
/// It is [known-issues #17] inside-out: the health of an empty *manifest* rather than
/// an empty *peer set*. Advertising `0` while virgin puts the node in the documented
/// "unknown" state that both [`PeerRoster::settled_manifest_fps`] and
/// [`PeerRoster::online_manifest_fps`] already exclude from comparison — so it counts
/// as *behind*, not *diverged*, until its replica is real.
///
/// A genuinely empty but **established** share is unaffected: its master has written
/// an ignore entry / member record, so `replica_seen` is true and it advertises the
/// real `FP_EMPTY`. Two empty masters still agree on it and converge to Healthy.
fn advertised_fp(replica_seen: bool, remote: &HashMap<String, RemoteEntry>) -> u64 {
    if replica_seen {
        manifest_fingerprint(remote)
    } else {
        0
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
    /// Whether this blob counts against the [`MAX_INFLIGHT_LARGE`] class
    /// (size ≥ [`SWARM_MIN_SIZE`] at queue time).
    large: bool,
    /// Aborts the detached download task. Aborting drops the download future
    /// (and, for a swarm, its `JoinSet` of part tasks), closing the connections;
    /// already-fetched chunks persist on disk and resume on the next attempt.
    abort: tokio::task::AbortHandle,
    /// When the task was spawned, for the stall watchdog
    /// ([`Engine::abort_stalled_downloads`]): the in-flight map deduplicates by
    /// hash, so a download future that wedges without settling would otherwise
    /// block that blob's re-queue *forever* — observed in the full-size soak,
    /// where nodes sat at 0–5% for hours until a pause/resume (which aborts and
    /// re-queues) unstuck them.
    started: std::time::Instant,
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
    /// Fingerprint of the merged manifest this pass, broadcast in presence and
    /// compared across members to detect divergence. See [`manifest_fingerprint`].
    manifest_fp: u64,
    /// This pass ran because a deep verify was pending ([`ReconcileJob::force_scan`]).
    /// Tells [`Engine::finish_reconcile`] to clear the pending flag and advance
    /// `last_deep_verify` — completion, not request, satisfies the force.
    forced_scan: bool,
    /// This pass did a full hashing scan (forced or signature-triggered). Feeds the
    /// per-share scan counter used by tests/soaks to assert rescan policy.
    did_full_scan: bool,
    /// The doc member registry as read this pass (self excluded), folded into the
    /// roster's remembered identities by [`Engine::finish_reconcile`].
    member_records: Vec<MemberIdentity>,
}

impl ReconcileOutcome {
    /// Whether this pass mutated the local folder or the replica.
    pub fn changed(&self) -> bool {
        self.changed
    }
}

/// Marker error returned by [`ReconcileJob::run`] when the pass was cancelled
/// mid-flight because its share was removed or paused (known-issues #34).
///
/// It is distinct from a genuine failure so the daemon can log it quietly, and
/// so the caller commits *nothing*: a cancelled pass must not write index rows
/// for a share whose DB rows were just deleted, which would resurrect it in
/// persistence.
#[derive(Debug)]
pub struct ReconcileCancelled;

impl std::fmt::Display for ReconcileCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("reconcile pass cancelled (share removed or paused)")
    }
}

impl std::error::Error for ReconcileCancelled {}

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
    /// Whether *this* device minted the share key. Lets a fresh creator publish
    /// its ignore list on pass 1 (its empty replica is authoritative, not virgin)
    /// while a joining master waits for `replica_seen` — see the ignore-list
    /// publish gate in [`ReconcileJob::run`] (known-issues #15).
    we_minted: bool,
    configured_ignore: Vec<String>,
    /// This device's display name at job build, published into the doc member
    /// registry (masters only) so peers keep a last-known name for us.
    device_name: String,
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
    /// The engine generation this job was built in (see [`Engine::generation`]).
    generation: u64,
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
    /// A deep verify is pending: do a full hashing scan regardless of the quick
    /// signature. Read (not cleared) from `ShareState.force_deep_verify` at job
    /// build; cleared by `finish_reconcile` only when this job's outcome commits.
    force_scan: bool,
    progress: Arc<StdMutex<HashMap<String, (u64, u64)>>>,
    /// Diagnostic breadcrumb for the slow-pass watchdog: which step [`run`] is
    /// currently in (updated as the pass moves through its awaits). Exists to
    /// localize known-issues #7: passes observed never returning under live
    /// fleet pressure, with nothing logged — the daemon reads this via
    /// [`ReconcileJob::phase_handle`] and WARNs with it while a pass overruns.
    ///
    /// [`run`]: ReconcileJob::run
    phase: Arc<StdMutex<String>>,
    /// Test seam: invoked after the merge, immediately before the settle walk.
    /// A pass takes real time and the folder is live throughout it, so the
    /// mid-pass write that known-issues #30 turns on is otherwise only reachable
    /// by racing a timer. See [`ReconcileJob::debug_before_settle`].
    debug_before_settle: Option<Arc<dyn Fn() + Send + Sync>>,
    /// GC's protected-set handle, so every blob this pass puts in the store is
    /// shielded from a sweep that fires before the next live-set refresh sees it
    /// (known-issues #33). See [`GcProtect::note_added`].
    gc_protect: GcProtect,
    /// Shared with the owning `ShareState`: set when the share is removed or
    /// paused so this pass stops instead of running to completion against a
    /// share that no longer exists (known-issues #34). A job is a *snapshot* —
    /// it clones the doc, folder and store handles and runs off the engine lock —
    /// so without this flag nothing the engine does can reach a running pass.
    cancel: Arc<AtomicBool>,
    /// Providers that failed to connect during THIS pass; see [`self_heal_file`].
    /// Per-job, which is per-pass, so an unreachable peer is dialed once and then
    /// skipped for the remaining paths instead of once per file.
    dead_providers: Arc<StdMutex<HashSet<String>>>,
}

impl ReconcileJob {
    pub fn share_id(&self) -> &str {
        &self.share_id
    }

    /// Run `hook` after this pass's merge but before it records the folder as
    /// settled — the window in which a real user's write lands mid-pass. Exists so
    /// the mid-pass-overwrite regression can be tested deterministically instead of
    /// by racing a background writer against a multi-second scan.
    #[doc(hidden)]
    pub fn debug_before_settle<F>(mut self, hook: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.debug_before_settle = Some(Arc::new(hook));
        self
    }

    /// Cloneable handle to this job's current-phase breadcrumb, so a watchdog can
    /// report where an overrunning pass is stuck while [`run`](Self::run) holds
    /// `&self`.
    /// The engine generation this job belongs to; pass it back to
    /// [`Engine::finish_reconcile`] so a job that outlived a transport rebuild
    /// cannot commit stale results into the reopened share.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn phase_handle(&self) -> Arc<StdMutex<String>> {
        self.phase.clone()
    }

    /// Whether this pass has been cancelled (share removed or paused).
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Bail out of the pass if it has been cancelled. Called at the top of each
    /// per-file loop iteration: every branch below can await network I/O, so a
    /// per-file granularity bounds how long a cancelled pass keeps running.
    fn bail_if_cancelled(&self) -> anyhow::Result<()> {
        if self.cancelled() {
            return Err(ReconcileCancelled.into());
        }
        Ok(())
    }

    fn set_phase(&self, p: impl Into<String>) {
        if let Ok(mut g) = self.phase.lock() {
            *g = p.into();
        }
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
    /// Put a file that is *already correct on disk* back into the blob store,
    /// without touching the replica.
    ///
    /// A blob can go missing from under a correct file (known-issues #33: a GC
    /// sweep firing before the live-set refresh has seen it). Nothing else repairs
    /// that — `materialize` returns early on a file that hashes correctly, and a
    /// master's scan finds local == remote and moves on — so the content stays on
    /// disk while the node silently loses the ability to *serve* it. When every
    /// member sweeps the same blob, as a fleet on one schedule does, the content
    /// becomes unfetchable for anyone joining later even though every existing copy
    /// is intact.
    ///
    /// This is a local store operation and is correct for viewers as much as
    /// masters: it publishes nothing, it only restores what we can hand to a peer.
    async fn reimport_local(&self, abs: &Path) -> anyhow::Result<Hash> {
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
            .with_context(|| format!("re-import {}", abs.display()))?;
        let hash = tag.hash();
        self.gc_protect.note_added(hash);
        Ok(hash)
    }

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
        // The temp tag dies with this function; from then on the only thing keeping
        // this blob alive is GC's protected set, which is a snapshot that may
        // predate it by up to ~120 s (known-issues #33).
        self.gc_protect.note_added(hash);
        let size = match self.blobs.blobs().status(hash).await {
            Ok(iroh_blobs::api::proto::BlobStatus::Complete { size }) => size,
            _ => 0,
        };
        // NOTE: a stale `\x00t/<path>` tombstone is deliberately NOT deleted
        // when (re)publishing: the fresh content record's newer timestamp wins
        // the LWW against it (see `read_remote_files`), and `doc.del` is
        // PREFIX deletion — clearing `\x00t/foo` would also nuke the live
        // tombstone of a deleted `foobar`.
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

    /// Tombstone a file (and its empty-marker) in the replica, leaving a
    /// timestamped `\x00t/<path>` marker so a member that never saw this path
    /// can tell "deleted" from "never seen" (known-issues #12). The marker's
    /// record timestamp is the delete time for delete-vs-edit LWW; its *value*
    /// is the deleted content's hash, so the reconcile can tell "the exact
    /// deleted file is still on our disk" (suppress it) from "different content
    /// re-added at the same name" (a real re-add — publish it, even when a
    /// copy/extract/download gave it a stale mtime older than the delete).
    async fn tombstone(&self, path: &str, deleted_hash: &[u8]) {
        let mut tk = TOMBSTONE_PREFIX.to_vec();
        tk.extend_from_slice(path.as_bytes());
        let _ = self
            .doc
            .set_bytes(self.author, tk, deleted_hash.to_vec())
            .await;
        let _ = self.doc.del(self.author, path.as_bytes().to_vec()).await;
        let mut ek = EMPTY_PREFIX.to_vec();
        ek.extend_from_slice(path.as_bytes());
        let _ = self.doc.del(self.author, ek).await;
    }

    /// The content hash recorded in a path's live delete tombstone, if the
    /// marker's value blob has arrived and looks like a hash (32 bytes). Returns
    /// `None` for a legacy tombstone (value `[1]`, written before we stored the
    /// hash) or when the tiny value blob hasn't synced yet — callers then fall
    /// back to the time-only rule, self-correcting on a later pass.
    async fn tombstone_hash(&self, path: &str) -> Option<Vec<u8>> {
        let mut tk = TOMBSTONE_PREFIX.to_vec();
        tk.extend_from_slice(path.as_bytes());
        let entry = self
            .doc
            .get_one(Query::single_latest_per_key().key_exact(tk))
            .await
            .ok()??;
        let h = entry.content_hash();
        if !self.blobs.blobs().has(h).await.unwrap_or(false) {
            return None;
        }
        let bytes = self.blobs.blobs().get_bytes(h).await.ok()?;
        (bytes.len() == 32).then(|| bytes.to_vec())
    }

    /// Live content providers for this job — see [`live_providers_from`].
    fn live_providers(&self) -> (Vec<EndpointId>, usize, Option<EndpointId>) {
        live_providers_from(&self.roster, self.self_id, self.master_id)
    }

    /// Read the doc member registry (`\x00m/`, see [`MEMBER_PREFIX`]): the
    /// freshest record per member, own record included (the publish diff needs
    /// it; the roster fold-in filters it out). Best-effort: the registry is a
    /// display aid, so trouble here must never fail the file reconcile (a
    /// wedged doc actor will fail the manifest read right after anyway).
    async fn read_member_registry(&self) -> Vec<MemberIdentity> {
        match read_member_records(&self.doc).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("member-registry read for {} failed: {e:#}", self.share_id);
                Vec::new()
            }
        }
    }

    /// Publish what the member registry is missing (masters only): our own
    /// identity, plus members we can hear live right now whose record is absent
    /// or stale.
    ///
    /// MUST only run late in a pass whose replica has proven contact with the
    /// share's state (see the `replica_seen` gate in [`run`]): a doc write
    /// while our own *initial* doc-sync is still in flight can abort/restart
    /// the session (`AbortReason::AlreadySyncing` churn), and a joining master
    /// whose first merge then runs against a still-virgin replica republishes
    /// its local copies as brand-new — with fresh LWW timestamps that
    /// resurrect concurrent deletes fleet-wide (exactly the known-issues #12
    /// race the tombstones exist to prevent; caught by
    /// `multi_master::delete_survives_unseen_master_copy`).
    ///
    /// [`run`]: ReconcileJob::run
    async fn publish_member_records(&self, registry: &[MemberIdentity]) {
        if !self.is_master {
            return;
        }
        let mut desired = vec![(
            self.self_id.to_string(),
            MemberRecord {
                v: MEMBER_RECORD_V,
                id: *self.self_id.as_bytes(),
                name: self.device_name.clone(),
                master: true,
            },
        )];
        let heard = self
            .roster
            .lock()
            .map(|r| r.online_named_peers())
            .unwrap_or_default();
        for (id_str, name, master) in heard {
            let Ok(eid) = id_str.parse::<EndpointId>() else {
                continue;
            };
            desired.push((
                id_str,
                MemberRecord {
                    v: MEMBER_RECORD_V,
                    id: *eid.as_bytes(),
                    name,
                    master,
                },
            ));
        }
        let have: HashMap<&str, (&str, bool)> = registry
            .iter()
            .map(|m| (m.id.as_str(), (m.name.as_str(), m.master)))
            .collect();
        for (id_str, rec) in desired {
            // Only write when the registry disagrees with first-hand
            // knowledge: rewriting an unchanged record would just bump its
            // LWW timestamp (and doc churn) for nothing.
            if have.get(id_str.as_str()) == Some(&(rec.name.as_str(), rec.master)) {
                continue;
            }
            if let Err(e) = self
                .doc
                .set_bytes(self.author, member_record_key(&rec), vec![1u8])
                .await
            {
                tracing::debug!("member-registry publish for {id_str} failed: {e:#}");
            }
        }
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
        // Already downloading this blob, or all download slots busy? The next
        // reconcile tick re-offers every still-missing blob, so a full window
        // is back-pressure, not a drop. Large blobs additionally contend for
        // their own smaller class ([`MAX_INFLIGHT_LARGE`]) so a queue of ISOs
        // can't monopolize the disk with scattered concurrent writes.
        let large = size >= SWARM_MIN_SIZE;
        {
            let inflight = match self.downloads_inflight.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            if inflight.contains_key(&hash) || inflight.len() >= MAX_INFLIGHT_DOWNLOADS {
                return;
            }
            if large && inflight.values().filter(|d| d.large).count() >= MAX_INFLIGHT_LARGE {
                return;
            }
        }
        let (peers, _full_peers, master) = self.live_providers();
        if peers.is_empty() && master.is_none() {
            // Nobody to pull from yet (no peers, we are the only/master node).
            return;
        }
        // Full fallback set for the simple path: fully-synced peers first (they
        // can serve anything), then partial peers, master last. Sequential
        // fallback means the master is dialed only when every peer failed —
        // that's already "don't hammer the original seeder".
        let fallback: Vec<EndpointId> = peers.iter().copied().chain(master).collect();
        let swarm = size >= SWARM_MIN_SIZE && peers.len() >= 2;

        let downloader = self.downloader.clone();
        let blobs = self.blobs.clone();
        let roster = self.roster.clone();
        let self_id = self.self_id;
        let master_id = self.master_id;
        let inflight = self.downloads_inflight.clone();
        let share = self.share_id.clone();
        let gc_protect = self.gc_protect.clone();
        let handle = tokio::spawn(async move {
            let res = if swarm {
                swarm_download(&downloader, &blobs, hash, size, &roster, self_id, master_id).await
            } else {
                downloader
                    .download_with_opts(DownloadRequest::new(hash, fallback, SplitStrategy::None))
                    .await
                    .map_err(|e| anyhow!("{e}"))
            };
            match res {
                // Freshly fetched content is exactly what the live-set snapshot is
                // too old to know about, and a sweep landing here would delete a
                // blob we just spent bandwidth on (known-issues #33).
                Ok(_) => gc_protect.note_added(hash),
                Err(e) => {
                    tracing::debug!("download {hash} for share {share} failed (will retry): {e}")
                }
            }
            if let Ok(mut g) = inflight.lock() {
                g.remove(&hash);
            }
        });

        // Register the abort handle so a pause can cancel this transfer. If another
        // tick registered the same hash (or filled the last slot) while we were
        // spawning, cancel this duplicate.
        let abort = handle.abort_handle();
        match self.downloads_inflight.lock() {
            Ok(mut inflight) => {
                if inflight.contains_key(&hash)
                    || inflight.len() >= MAX_INFLIGHT_DOWNLOADS
                    || (large
                        && inflight.values().filter(|d| d.large).count() >= MAX_INFLIGHT_LARGE)
                {
                    abort.abort();
                } else {
                    inflight.insert(
                        hash,
                        InflightDownload {
                            share_id: self.share_id.clone(),
                            large,
                            abort,
                            started: std::time::Instant::now(),
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
        // The blob is complete in the local store (has(hash) above), so writing it
        // to the target is a zero-network local export. `export_with_opts` won't
        // overwrite an existing path; for an in-place overwrite the OLD file is
        // still on disk, so remove the stale copy first and export the new bytes
        // from the store.
        //
        // This export was previously gated on `!target.exists()`, which meant an
        // in-place overwrite (or any diverged-but-present file) SKIPPED it and
        // fell through to `self_heal_file` below — re-fetching the entire blob
        // over the network even though it was already complete in the local store.
        // That doubled the bandwidth of every replaced file. Export from the store
        // instead; `self_heal_file` is now only the last resort when the store
        // export itself can't produce matching bytes.
        if target.exists() {
            let _ = std::fs::remove_file(&target);
        }
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
        if file_matches(&target, hash_bytes) {
            reclaim.push(hash);
            Ok(true)
        } else {
            // Store export didn't yield matching bytes (rare): pull a verified copy
            // from a peer as a last resort.
            self_heal_file(
                &self.endpoint,
                &self.providers,
                hash,
                &target,
                &self.dead_providers,
            )
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
        self.set_phase("read ignore list (doc keys)");
        let live_ignore = tokio::time::timeout(
            Duration::from_secs(DOC_READ_TIMEOUT_SECS),
            read_ignore_list(&self.doc),
        )
        .await
        .map_err(|_| anyhow!("ignore-list doc read timed out after {DOC_READ_TIMEOUT_SECS}s"))??;
        // Evidence of a non-virgin replica for the publish gates below: an ignore
        // entry can only exist if we've synced someone's state (or published our
        // own on an earlier pass).
        let replica_had_ignore = live_ignore.is_some();
        // A master's effective list is always its own configured list; a viewer
        // honors the replicated one (so it won't delete files a master ignored),
        // falling back to local only until a master has published. A master's
        // publish of a *drifted* list is DEFERRED to the end of the pass and gated
        // like the member registry (known-issues #15): a doc write during a virgin
        // replica's initial sync can churn the session and resurrect concurrent
        // deletes. `effective_ignore` doesn't depend on that write, so the scan can
        // proceed with it now.
        let ignore_needs_publish =
            self.is_master && live_ignore.as_deref() != Some(self.configured_ignore.as_slice());
        let effective_ignore = if self.is_master {
            self.configured_ignore.clone()
        } else {
            live_ignore.unwrap_or_else(|| self.configured_ignore.clone())
        };
        let (ignore_set, _bad) = IgnoreSet::compile(&effective_ignore);

        // 1.5. Member registry (`\x00m/`): read the replicated last-known member
        //      identities for the roster fold-in (any role). READ ONLY here —
        //      publishing waits until the end of the pass, gated on the replica
        //      having proven contact with share state (see `replica_seen`
        //      below). Best-effort inside, but timeout-bounded like the other
        //      doc reads so a wedged docs actor is caught by the phase
        //      watchdog, not waited on forever.
        self.set_phase("member registry (doc read)");
        let member_records = tokio::time::timeout(
            Duration::from_secs(DOC_READ_TIMEOUT_SECS),
            self.read_member_registry(),
        )
        .await
        .unwrap_or_default();

        // 2. Merged remote view (desired fileset + winning delete tombstones).
        self.set_phase("merge remote view (doc get_many stream)");
        let remote_view = tokio::time::timeout(
            Duration::from_secs(DOC_READ_TIMEOUT_SECS),
            read_remote_files(&self.doc),
        )
        .await
        .map_err(|_| anyhow!("manifest doc read timed out after {DOC_READ_TIMEOUT_SECS}s"))??;
        let remote = remote_view.files;
        let tombstones = remote_view.tombstones;

        // Has our replica proven contact with the share's state? Computed HERE,
        // before the merge, because the merge's "new local file" arm needs it
        // too — not just the deferred publishes at the end of the pass.
        //
        // A virgin replica cannot distinguish "absent because deleted" from
        // "absent because we haven't synced yet": `tombstones` is empty either
        // way. Publishing a pre-existing local file in that state can outrun an
        // inbound tombstone, and because the publish carries a NEWER record
        // timestamp than the delete, `resolve_tombstones` then hands the path to
        // the content and drops the tombstone — permanently resurrecting the
        // delete on every member, with no pass that ever repairs it. See
        // known-issues #10.
        let replica_seen = replica_had_ignore
            || !remote.is_empty()
            || !tombstones.is_empty()
            || !member_records.is_empty();

        // 3. Local view. Hashing the whole folder is costly, so only do it when the
        //    cheap (path,size,mtime) signature changed since last reconcile;
        //    otherwise the on-disk content equals our recorded base. Both roles
        //    scan: a master to publish local edits, a viewer to detect (and revert)
        //    local drift.
        // Exclude the files we couldn't process last pass from the change-signature:
        // they're chased by the targeted retry below, and counting them here would let
        // a skipped file mask the folder as "settled" and suppress full scans (the
        // gate-poisoning bug). The end-of-pass signature excludes the same way.
        let prev_skipped_set: HashSet<String> = self.prev_skipped.iter().cloned().collect();
        self.set_phase("scan folder");
        // Keep the per-path metadata this walk saw, not just its hash: the
        // end-of-pass settle compares against it to tell our own disk writes from a
        // file the user changed while the pass was running (known-issues #30).
        let (quick_sig, sig_before) =
            scan::signature_map(&self.folder, &ignore_set, &prev_skipped_set);
        let do_scan = self.force_scan || quick_sig != self.last_quick_sig;
        // Corruption signal for known-issues #13 (see [`is_silent_corruption_scan`]).
        let deep_verify_unchanged_meta =
            is_silent_corruption_scan(self.force_scan, quick_sig, self.last_quick_sig);
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
        self.set_phase("retry previously-skipped files");
        let mut still_skipped: Vec<String> = Vec::new();
        for rel in skip_candidates {
            self.bail_if_cancelled()?;
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
        // Parent directories of files we actually delete this tick. Only these are
        // considered for empty-dir cleanup afterwards — an empty folder the user
        // created (and no synced file was ever removed from) is left alone, so it
        // no longer vanishes on the next reconcile.
        let mut emptied_parents: HashSet<PathBuf> = HashSet::new();
        // Paths whose on-disk bytes *this pass* wrote or removed. The settle walk
        // absorbs their new metadata instead of treating it as user drift — see
        // [`scan::settled_signature`].
        let mut wrote: HashSet<String> = HashSet::new();
        // Paths whose local content we published this pass, and paths we tombstoned.
        // Both mean "our disk is the new truth here", so the health accounting below
        // must not score them against the pass-start (now stale) remote view.
        let mut published: HashSet<String> = HashSet::new();
        let mut removed: HashSet<String> = HashSet::new();

        for path in keys {
            self.bail_if_cancelled()?;
            // Per-file breadcrumb: every branch below can await store/doc/network
            // ops (import, materialize, tombstone), and #7-style wedges are
            // per-call, so name the exact file being merged.
            self.set_phase(format!("merge {path}"));
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
                        if let Some(p) = target.parent() {
                            emptied_parents.insert(p.to_path_buf());
                        }
                        wrote.insert(path.clone());
                        if b.is_some() {
                            index_dels.push(path);
                        }
                        changed = true;
                    } else if b.map(|bh| bh == &le.hash).unwrap_or(false) {
                        // Master, unchanged since base, now gone from replica →
                        // a remote delete: remove it locally.
                        let target = self.folder.join(rel_to_native(&path));
                        let _ = std::fs::remove_file(&target);
                        if let Some(p) = target.parent() {
                            emptied_parents.insert(p.to_path_buf());
                        }
                        wrote.insert(path.clone());
                        index_dels.push(path);
                        changed = true;
                    } else if let Some(abs) = le.abs.as_ref() {
                        // Master, local file absent from the replica. Before
                        // treating it as brand-new, check for a delete
                        // tombstone (known-issues #12): "absent because
                        // deleted" must not read as "never seen".
                        //
                        // Only suppress the local file when it is *the exact
                        // deleted content* still lingering on our disk (same
                        // hash as the tombstone) AND not newer than the delete.
                        // Different content at this name is a genuine re-add /
                        // replace and must publish — a file mtime is NOT a
                        // reliable "when did I re-add this" signal, since copy,
                        // extract-from-archive and download all preserve the
                        // *source's* older mtime, which would otherwise lose the
                        // LWW to the tombstone forever and delete the re-added
                        // file on every pass (even for the member who deleted
                        // it). A legacy tombstone (no stored hash) or one whose
                        // value blob hasn't synced yet falls back to the
                        // time-only rule.
                        if let Some(&tts) = tombstones.get(&path) {
                            let deleted_hash = self.tombstone_hash(&path).await;
                            if tombstone_suppresses(
                                deleted_hash.as_deref(),
                                &le.hash,
                                mtime_micros(abs),
                                tts,
                            ) {
                                let _ = std::fs::remove_file(abs);
                                if let Some(p) = abs.parent() {
                                    emptied_parents.insert(p.to_path_buf());
                                }
                                wrote.insert(path.clone());
                                if b.is_some() {
                                    index_dels.push(path);
                                }
                                changed = true;
                                continue;
                            }
                        }
                        // Hold the publish until the replica has proven contact
                        // with the share (known-issues #10). On a virgin replica
                        // "absent from the replica" is ambiguous — it may simply
                        // be unsynced — and the tombstone check above is blind,
                        // so publishing here can outrun an inbound delete and
                        // resurrect it permanently (the publish timestamp beats
                        // the tombstone, so the tombstone is dropped for good).
                        // A share we minted ourselves is exempt: its replica is
                        // authoritatively empty, so there is nothing to wait for.
                        // Anything genuinely new is published on a later pass,
                        // once the initial sync has landed — a bounded delay in
                        // exchange for never silently undoing a delete.
                        if !(replica_seen || self.we_minted) {
                            still_skipped.push(path.clone());
                            continue;
                        }
                        // Brand-new local file (or locally edited after a
                        // remote delete): publish it. A per-file import failure (the
                        // file got locked between scan and import, an odd entry, etc.)
                        // must NOT abort the whole pass — skip it and retry next tick.
                        match self.import_one(&path, abs).await {
                            Ok(h) => {
                                imported_bytes += le.size;
                                self.set_progress(imported_bytes, imported_bytes);
                                published.insert(path.clone());
                                index_sets.push((path, h));
                                changed = true;
                            }
                            Err(e) => {
                                tracing::warn!("skip publishing {path} (will retry): {e:#}");
                                // On disk but not published: exclude from the gate so it
                                // can't mark the folder "clean", and retry it next pass.
                                still_skipped.push(path.clone());
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
                        // agreed → the user deleted it: propagate the tombstone,
                        // recording the deleted content's hash so a later re-add of
                        // *different* content at this name isn't mistaken for the
                        // deleted file lingering.
                        self.tombstone(&path, &re.hash).await;
                        removed.insert(path.clone());
                        index_dels.push(path);
                        changed = true;
                    } else {
                        match self
                            .materialize(&path, &re.hash, re.size, &mut reclaim)
                            .await
                        {
                            Ok(true) => {
                                wrote.insert(path.clone());
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
                                wrote.insert(path.clone());
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
                                    wrote.insert(path.clone());
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
                            // known-issues #13: if a forced deep verify surfaced this
                            // change while (size, mtime) held steady, the local bytes
                            // changed in place with no metadata change — likely silent
                            // corruption, and a master publishes it over good peer
                            // copies rather than healing. Can't be auto-distinguished
                            // from a deliberate same-size+mtime edit, so warn loudly
                            // instead of propagating it silently.
                            if deep_verify_unchanged_meta {
                                tracing::warn!(
                                    "deep verify: {path} content hash changed with \
                                     unchanged size+mtime on a master — publishing to \
                                     peers (possible in-place corruption; known-issues #13)"
                                );
                            }
                            if let Some(abs) = le.abs.as_ref() {
                                match self.import_one(&path, abs).await {
                                    Ok(h) => {
                                        published.insert(path.clone());
                                        index_sets.push((path, h));
                                        changed = true;
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "skip publishing {path} (will retry): {e:#}"
                                        );
                                        still_skipped.push(path.clone());
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
                                            published.insert(path.clone());
                                            index_sets.push((path, h));
                                            changed = true;
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                "skip publishing {path} (will retry): {e:#}"
                                            );
                                            still_skipped.push(path.clone());
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
                                        wrote.insert(path.clone());
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

        // Tidy directories that a delete this tick actually emptied — NOT every
        // empty directory under the root. A folder the user just created (no synced
        // file ever removed from it) is left in place instead of being nuked on the
        // next reconcile, which is what made new folders "vanish".
        prune_emptied_ancestors(&emptied_parents, &self.folder);

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
        self.set_phase("compute health (blob store has/local_bytes)");
        // What our *disk* holds per path as this pass leaves it: the base index
        // (path → last hash we reconciled to disk) with this pass's mutations
        // applied. Health is a claim about the mirrored folder, so having the blob
        // is necessary but not sufficient — a peer that fetched the content and
        // then failed to write it reported a confident `Healthy 100%` over a stale
        // file, which is exactly how a silently-unpropagated overwrite hid on the
        // receiving side (known-issues #30).
        let mut on_disk: HashMap<&str, &[u8]> = self
            .base
            .iter()
            .map(|(p, h)| (p.as_str(), h.as_slice()))
            .collect();
        for p in &index_dels {
            on_disk.remove(p.as_str());
        }
        for (p, h) in &index_sets {
            on_disk.insert(p.as_str(), h.as_slice());
        }
        let mut total_bytes: u64 = 0;
        let mut present_bytes: u64 = 0;
        // Which paths health is still holding against us, and which predicate said
        // so. A share parked below 100% over a folder that is byte-for-byte correct
        // is otherwise inexplicable from outside the process — two full soak cycles
        // went to guessing at exactly that (known-issues #33). Name the paths.
        let mut short: Vec<String> = Vec::new();
        // Counted separately from `short`, which is capped: reporting the capped
        // length as "N path(s) outstanding" would understate a big backlog as
        // exactly 20 every time.
        let mut short_total: usize = 0;
        // Index rows this pass proved stale by hashing the file itself. Collected
        // rather than pushed straight onto `index_sets`, which `on_disk` borrows.
        let mut index_repairs: Vec<(String, Vec<u8>)> = Vec::new();
        for (path, re) in &remote {
            // `remote` was read at the top of the pass, so it is stale for paths we
            // published or tombstoned since — for those, our disk *is* the new
            // truth and there is nothing outstanding to hold against it.
            if published.contains(path) || removed.contains(path) {
                continue;
            }
            total_bytes += re.size;
            if re.size == 0 {
                continue;
            }
            let hash = to_hash(&re.hash)?;
            let indexed = on_disk.get(path.as_str()) == Some(&re.hash.as_slice());
            let in_store = self.blobs.blobs().has(hash).await?;
            if indexed && in_store {
                present_bytes += re.size;
                continue;
            }
            // The index says we don't hold it. Ask the *disk* before docking the file.
            // The index can lag what the folder actually holds — a path the scan
            // skipped because the user was mid-write, a pass that exported the bytes
            // and then bailed — and `materialize` already treats a correctly-hashing
            // file as done and queues no fetch. So if health disagreed with repair
            // here, it would report a deficit that nothing will ever clear: a share
            // parked below 100% over a byte-perfect folder, forever, with
            // `retrying=0` and no way to tell from the outside (known-issues #33).
            //
            // Size is checked first so a large file mid-overwrite (old bytes on disk,
            // new blob still downloading) is not re-hashed every pass only to be
            // rejected; it only costs a `stat` per outstanding path.
            let target = self.folder.join(rel_to_native(path));
            let same_size = std::fs::metadata(&target)
                .map(|m| m.is_file() && m.len() == re.size)
                .unwrap_or(false);
            if same_size && file_matches(&target, &re.hash) {
                // The folder is right. Two different things can still be wrong, and
                // they need different repairs:
                //
                //  - the index lagged        → record the hash we just proved
                //  - the blob left the store → put it back from disk
                //
                // The second is not bookkeeping. A blob we do not hold is a blob we
                // cannot *serve*, so a peer needing this file cannot get it from us;
                // when a whole fleet sweeps on one schedule, nobody can. Counting it
                // as present without restoring it would report `Healthy 100%` over
                // content the share can no longer hand out — known-issues #17's rule
                // ("if the app claims X, then X is true") from the other side. So
                // credit it only once it is genuinely servable again.
                let servable = in_store
                    || match self.reimport_local(&target).await {
                        Ok(h) => h == hash,
                        Err(e) => {
                            tracing::warn!(
                                "{path}: bytes on disk are correct but the blob is gone from the \
                                 store and re-importing it failed (peers cannot fetch this file \
                                 from us until it succeeds): {e:#}"
                            );
                            false
                        }
                    };
                if servable {
                    present_bytes += re.size;
                    // Record the truth we just established, so the next pass takes
                    // the cheap indexed path above instead of re-hashing forever.
                    index_repairs.push((path.clone(), re.hash.clone()));
                    continue;
                }
            }
            // Genuinely outstanding. Count the chunk bytes already fetched so the
            // percent climbs with real download progress, but never let it reach
            // full credit — an unwritten file is not a mirrored file.
            let local = self.local_bytes(hash).await;
            present_bytes += local.min(re.size.saturating_sub(1));
            short_total += 1;
            if short.len() < 20 {
                // Both predicates are reported because "the index disagrees" and
                // "the blob is gone" are very different faults with one symptom.
                short.push(format!(
                    "{path} (size={} indexed={indexed} in_store={in_store} local={local})",
                    re.size
                ));
            }
        }
        // `on_disk` borrows `index_sets`; done with it, so the repairs can land.
        drop(on_disk);
        index_sets.extend(index_repairs);

        // An empty view is 100% (100% of nothing); otherwise the fraction held.
        // `checked_div` folds the `total_bytes == 0` guard into the division.
        let health = (present_bytes.min(total_bytes) * 100)
            .checked_div(total_bytes)
            .unwrap_or(100) as u8;
        if !short.is_empty() {
            // Own target so this can be turned on alone — `seed_core=debug` across a
            // 28-node soak buries it, and this line is the whole point of looking.
            //   RUST_LOG=seed_core=info,seed_core::health=debug
            tracing::debug!(
                target: "seed_core::health",
                "reconcile {}: health {}% ({}/{} bytes) — {} path(s) outstanding{}: {}",
                self.share_id,
                health,
                present_bytes,
                total_bytes,
                short_total,
                if short_total > short.len() {
                    format!(" (first {} shown)", short.len())
                } else {
                    String::new()
                },
                short.join(", ")
            );
        }

        still_skipped.sort();
        still_skipped.dedup();
        if !still_skipped.is_empty() {
            tracing::warn!(
                "reconcile {}: {} file(s) unreadable/unpublished this pass, will retry (do_scan={})",
                self.share_id,
                still_skipped.len(),
                do_scan
            );
        }

        // Recompute the signature *after* our disk writes so the next tick sees a
        // settled folder rather than re-scanning our own changes. Exclude the files
        // we couldn't read or publish this pass — counting them would let the gate
        // read "clean" while the manifest is actually behind (the poisoning bug); the
        // targeted retry chases them instead, and any genuinely new add/delete still
        // flips the signature and triggers a full scan.
        let skipped_set: HashSet<String> = still_skipped.iter().cloned().collect();
        self.set_phase("finalize (settle signature)");
        if let Some(hook) = self.debug_before_settle.as_ref() {
            hook();
        }
        let (_, sig_after) = scan::signature_map(&self.folder, &ignore_set, &skipped_set);
        // Only absorb what this pass can vouch for. A file the user wrote *while the
        // pass was running* is deliberately left out, so the next pass's signature
        // can't match this one and the full scan is forced — see
        // [`scan::settled_signature`] and known-issues #30.
        let (new_quick_sig, drifted) = scan::settled_signature(&sig_before, &sig_after, &wrote);
        if !drifted.is_empty() {
            tracing::info!(
                "reconcile {}: {} path(s) changed on disk during the pass ({}); \
                 forcing a rescan next tick",
                self.share_id,
                drifted.len(),
                drifted
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }

        // Member-registry publish, LAST and gated on the same `replica_seen`
        // computed before the merge: only once the replica has proven contact
        // with the share's state (synced files, tombstones, an ignore entry, or
        // existing member records). A virgin replica means our initial doc-sync
        // may still be in flight, and a doc write can churn that session — see
        // [`publish_member_records`] for the failure this prevents. Genesis
        // costs one pass of delay: the creator's own ignore entry satisfies the
        // gate from pass 2 on.
        //
        // [`publish_member_records`]: ReconcileJob::publish_member_records

        // Ignore-list publish (masters only), LAST and gated like the member
        // registry (known-issues #15): a drifted list is written only once the
        // replica has proven contact with the share (`replica_seen`) OR this
        // device minted the share. A fresh creator's replica is authoritatively
        // empty (there is nothing to sync *from*), so it may bootstrap its list
        // immediately — that first `\x00i/` entry is also what flips `replica_seen`
        // true from pass 2 on. A *joining* master instead waits for its initial
        // sync, so a step-1 write can't churn the session and resurrect concurrent
        // deletes the way an early member-record write did.
        if ignore_needs_publish && (replica_seen || self.we_minted) {
            self.set_phase("ignore list (publish)");
            let key = ignore_list_key(&self.configured_ignore);
            match tokio::time::timeout(
                Duration::from_secs(DOC_READ_TIMEOUT_SECS),
                self.doc.set_bytes(self.author, key, vec![1u8]),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    tracing::warn!("ignore-list publish for {} failed: {e:#}", self.share_id)
                }
                Err(_) => tracing::warn!(
                    "ignore-list publish for {} timed out after {DOC_READ_TIMEOUT_SECS}s",
                    self.share_id
                ),
            }
        }

        if replica_seen {
            self.set_phase("member registry (publish)");
            let _ = tokio::time::timeout(
                Duration::from_secs(DOC_READ_TIMEOUT_SECS),
                self.publish_member_records(&member_records),
            )
            .await;
        }

        // The roster fold-in never includes our own record — `Engine::peers`
        // synthesizes the "This device" row from the live device name.
        let mut member_records = member_records;
        let self_str = self.self_id.to_string();
        member_records.retain(|m| m.id != self_str);

        Ok(ReconcileOutcome {
            changed,
            health,
            new_quick_sig,
            index_sets,
            index_dels,
            reclaim,
            skipped: still_skipped,
            manifest_fp: advertised_fp(replica_seen, &remote),
            forced_scan: self.force_scan,
            did_full_scan: do_scan,
            member_records,
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
    /// Raised to stop an in-flight [`ReconcileJob`] for this share. Cloned into
    /// every job built from this state, so removing or pausing the share reaches
    /// the pass that is already running off-lock (known-issues #34).
    cancel: Arc<AtomicBool>,
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
    /// This member's manifest fingerprint (latest reconcile). Broadcast in presence
    /// and compared with peers' to detect cross-member divergence.
    manifest_fp: u64,
    /// Unix seconds since which this member's fingerprint has disagreed with an
    /// online peer's, or `None` while in agreement. Only a disagreement that
    /// persists past [`DIVERGENCE_SETTLE_SECS`] is reported (so normal propagation
    /// lag doesn't false-alarm).
    diverged_since: Option<i64>,
    /// Fingerprint of the *current* disagreement (our manifest fp + the settled peers'
    /// fps). While a share is still converging this changes every pass, which restarts
    /// the settle clock — so only a disagreement that stays *the same* for the whole
    /// window is reported. Meaningless while `diverged_since` is `None`.
    divergence_sig: u64,
    /// Whether we've already logged a WARN for the current divergence episode, to
    /// avoid repeating it every tick. Cleared when agreement returns.
    diverged_alerted: bool,
    /// Unix seconds of the last *completed* deep verify (full hashing scan).
    /// Advanced by [`Engine::finish_reconcile`] when a forced scan commits, so a
    /// verify that never ran can't push the next one out a whole interval.
    last_deep_verify: i64,
    /// A deep verify (full hashing scan) is pending for this share. Set by
    /// [`Engine::request_deep_verify`], [`Engine::periodic_deep_verify`], and the
    /// divergence self-heal; carried into the next [`ReconcileJob`] and cleared by
    /// [`Engine::finish_reconcile`] only when a forced scan actually completed —
    /// an in-flight unforced job or a failed job can no longer swallow the request
    /// (the old `last_quick_sig = 0` force was clobbered by any concurrent commit).
    force_deep_verify: bool,
    /// The current divergence episode has already escalated to its one forced deep
    /// verify (see [`DIVERGENCE_DEEP_VERIFY_SECS`]). Cleared when agreement returns,
    /// so the next episode may escalate again.
    diverged_deep_verified: bool,
    /// Full hashing scans committed this session (signature-triggered or forced).
    /// Diagnostic: lets tests and soaks assert the rescan policy (e.g. that an
    /// OutOfSync share isn't re-hashing on a cadence).
    full_scans: u64,
    /// Unix seconds of the last doc-resync kick issued for this share while out
    /// of sync; throttles the self-heal to one kick per
    /// [`DIVERGENCE_RESYNC_KICK_SECS`] (the daemon *asks* every ~6s).
    last_doc_resync: i64,
    /// Whether *this* device minted the share key (its endpoint id is the one baked
    /// into the key). Distinguishes "a share I created that nobody has joined yet",
    /// where having no members is the expected steady state, from "a share I joined
    /// but cannot reach anyone in" — which is a partition (see [`Self::isolated`]).
    we_minted: bool,
    /// Unix seconds of the last rendezvous lookup for this share; throttles it to one
    /// per [`crate::rendezvous::LOOKUP_SECS`] (the daemon *asks* every ~6s).
    last_rendezvous: i64,
    /// Unix seconds of the last rendezvous publish (masters only); throttles it to
    /// one per [`crate::rendezvous::REPUBLISH_SECS`].
    last_rendezvous_publish: i64,
    /// Connectivity self-heal state (known-issues #23).
    heal: ConnHeal,
}

impl ShareState {
    /// Whether this member's manifest has disagreed with a peer's for longer than
    /// the settle window (the reported "out of sync" condition).
    fn is_out_of_sync(&self) -> bool {
        self.diverged_since
            .map(|t| now_secs() - t >= DIVERGENCE_SETTLE_SECS)
            .unwrap_or(false)
    }

    /// Whether every online peer that advertises a manifest fingerprint agrees with
    /// ours. Gates "Healthy": a share whose online peers report a *different* fileset
    /// (e.g. our doc replica hasn't finished syncing theirs yet) must not read
    /// Healthy 100% just because we locally hold everything *we currently know about* —
    /// that's the "reports Healthy 100% with 0 of the agreed files" failure. Peers that
    /// haven't broadcast a fingerprint yet (fp == 0) are ignored, and with no
    /// comparable peers this is `true` (solo/offline — nothing to disagree with).
    /// Mirrors the disagreement test in [`Engine::finish_reconcile`], but without the
    /// settle window: it only downgrades Healthy→Syncing (benign), never raises the
    /// OutOfSync alarm.
    fn converged_with_online_peers(&self) -> bool {
        self.roster
            .lock()
            .map(|r| {
                r.online_manifest_fps()
                    .iter()
                    .all(|fp| *fp == self.manifest_fp)
            })
            .unwrap_or(true)
    }

    /// Whether this node can reach **no member at all** of a share that ought to have
    /// members — i.e. it is partitioned from the pool.
    ///
    /// This is the condition that used to be indistinguishable from perfect health
    /// (known-issues #17). Every peer-comparison in the engine — the health percent,
    /// [`Self::converged_with_online_peers`], the consensus fingerprint — is computed
    /// over the set of *online* peers, and each is vacuously satisfied by an empty
    /// set. A totally partitioned node therefore agreed with everyone it could hear
    /// (nobody), held everything it knew about (nothing), and reported `Healthy 100%`.
    /// That is what hid known-issues #16 on a live share for over a week.
    ///
    /// An empty comparison set is not health, it is *ignorance* — with one genuine
    /// exception, which is why this is not simply "no online peers": a share this
    /// device **created** that nobody has joined yet is legitimately alone, and must
    /// keep reading Healthy. Every other case — we joined someone else's share, or we
    /// created one that members *did* join and can now reach none of them — is a
    /// partition and says so.
    fn isolated(&self) -> bool {
        let (online, known) = self.roster.lock().map(|r| r.counts()).unwrap_or((0, 0));
        online == 0 && (known > 0 || !self.we_minted)
    }
}

/// One due long-term-health notification from [`Engine::health_alerts`]:
/// either "unhealthy past the threshold" (first alert or a renotify) or
/// "recovered" after a previously-alerted episode cleared.
#[derive(Debug, Clone)]
pub struct PeerHealthAlert {
    pub share_id: String,
    /// Display name of the share (its folder name).
    pub share_name: String,
    /// Full endpoint id of the member, or `""` when it is this device.
    pub node_id: String,
    /// The member's self-chosen display name, if it has announced one.
    pub name: Option<String>,
    /// The member's last self-reported sync percent.
    pub percent: u8,
    /// Total accrued online-degraded seconds (0 for a recovery).
    pub unhealthy_secs: i64,
    pub is_self: bool,
    pub recovered: bool,
}

/// How often the blob store runs a garbage-collection sweep (known-issues #22).
/// Orphaned blobs — most visibly incomplete partials stranded by a removed share
/// — are disk-only waste, so an hourly reclaim is ample; a healthy store barely
/// notices it. The very first sweep only runs after this long, by which point
/// thousands of reconcile passes have refreshed the live set.
const GC_INTERVAL_SECS: u64 = 3600;

/// The set of blob hashes GC must NOT delete, recomputed from the live replicas
/// and shared with the blob store's GC loop (which lives inside the store, spawned
/// before the [`Engine`] exists). The store's `add_protected` callback copies the
/// published set in synchronously — so its future holds no `await` and trivially
/// satisfies the callback's `Send + Sync` bound — while the async work of reading
/// the replicas happens here, on the engine's schedule (see [`GcRefreshJob`]).
///
/// Fail-closed: until the first set is published the callback aborts the sweep,
/// so GC never runs against an unknown live set.
///
/// **The published set is a snapshot, and the sweep does not wait for it.** The
/// daemon recomputes it every ~120 s (`presence_loop`, every 40th 3 s tick) while
/// the store's GC loop fires on its own independent hourly timer. So a blob
/// imported or downloaded *after* the last refresh is referenced by the replica
/// but absent from the set the sweep reads — and gets deleted. That is not
/// hypothetical: a 28-node fleet soak lost 18 currently-referenced files' blobs on
/// **every** node at the first sweep, leaving content that no member could serve
/// (known-issues #33). It never came back, because nothing re-imports a file whose
/// bytes on disk are already correct.
///
/// [`Self::note_added`] closes that window: every hash this node puts in the store
/// is protected for [`RECENT_PROTECT_SECS`] regardless of the snapshot, which is
/// comfortably longer than the refresh interval, so by the time a hash ages out of
/// `recent` a real refresh has either picked it up from the replica or established
/// that nothing references it.
#[derive(Clone, Default)]
pub(crate) struct GcProtect {
    live: Arc<StdMutex<Option<Arc<HashSet<Hash>>>>>,
    /// Hashes recently added to the store by this node, with the instant of the
    /// add — protection for content newer than the published snapshot.
    recent: Arc<StdMutex<Vec<(Hash, std::time::Instant)>>>,
}

/// How long a freshly stored blob is protected from GC on the strength of having
/// just been written. Must exceed the live-set refresh interval (~120 s) by enough
/// to absorb a slow or failed refresh — a replica read that times out keeps the
/// *prior* set, so more than one interval can pass without new content appearing in
/// it. Ten minutes costs a few hundred hashes of memory at most.
const RECENT_PROTECT_SECS: u64 = 600;

impl GcProtect {
    /// Publish a freshly computed live set for the next GC sweep.
    fn publish(&self, set: HashSet<Hash>) {
        if let Ok(mut g) = self.live.lock() {
            *g = Some(Arc::new(set));
        }
    }

    /// Record that `hash` was just written to the blob store, protecting it from
    /// the next sweep even though no live-set refresh has seen it yet.
    pub(crate) fn note_added(&self, hash: Hash) {
        if let Ok(mut g) = self.recent.lock() {
            let now = std::time::Instant::now();
            g.retain(|(_, t)| now.duration_since(*t).as_secs() < RECENT_PROTECT_SECS);
            g.push((hash, now));
        }
    }

    /// The recently-added hashes still inside their protection window.
    fn recent_hashes(&self) -> Vec<Hash> {
        self.recent
            .lock()
            .map(|g| {
                let now = std::time::Instant::now();
                g.iter()
                    .filter(|(_, t)| now.duration_since(*t).as_secs() < RECENT_PROTECT_SECS)
                    .map(|(h, _)| *h)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The current live set, or `None` if none has been published yet.
    fn current(&self) -> Option<Arc<HashSet<Hash>>> {
        self.live.lock().ok().and_then(|g| g.clone())
    }

    /// Build the blob store's [`GcConfig`]: an hourly sweep whose protected set is
    /// this handle's most-recently published live set, copied in synchronously.
    fn gc_config(&self) -> GcConfig {
        let protect = self.clone();
        GcConfig {
            interval: Duration::from_secs(GC_INTERVAL_SECS),
            add_protected: Some(Arc::new(move |live: &mut HashSet<Hash>| {
                // Sync body, no await: the future is trivially Send + Sync.
                let outcome = match protect.current() {
                    Some(set) => {
                        live.extend(set.iter().copied());
                        // Plus anything stored since that snapshot was taken. The
                        // sweep fires on its own timer and will not wait for a
                        // refresh, so without this every blob written in the last
                        // ~120 s is unprotected — see [`GcProtect`].
                        live.extend(protect.recent_hashes());
                        ProtectOutcome::Continue
                    }
                    // No set yet (or lock poisoned) → skip this sweep entirely
                    // rather than delete against an unknown live set.
                    None => ProtectOutcome::Abort,
                };
                Box::pin(async move { outcome })
            })),
        }
    }
}

/// A prepared blob-GC live-set refresh: doc handles + in-flight download hashes
/// snapshotted under a brief engine lock, so the actual replica reads run
/// off-lock (mirroring [`DocResync`]/`presence_rejoins`). The daemon builds one
/// per periodic tick via [`Engine::gc_refresh_job`] and awaits [`run`](Self::run).
pub struct GcRefreshJob {
    docs: Vec<Doc>,
    inflight: Vec<Hash>,
    protect: GcProtect,
}

impl GcRefreshJob {
    /// Enumerate every entry of every replica and publish the union of their
    /// content hashes (plus in-flight download targets) as the GC live set. A
    /// replica read failure leaves the previously published set in place — better
    /// a slightly stale set than none (which would abort GC forever on a transient
    /// hiccup); GC only ever reclaims store blobs, never folder content.
    pub async fn run(self) {
        let mut live: HashSet<Hash> = HashSet::with_capacity(self.inflight.len());
        live.extend(self.inflight);
        for doc in &self.docs {
            if !read_referenced_hashes(doc, &mut live).await {
                tracing::debug!("gc live-set refresh: a replica read failed; keeping prior set");
                return;
            }
        }
        self.protect.publish(live);
    }
}

/// Insert into `live` the content hash of **every** entry of `doc` — all keys and
/// all versions, content and control alike. Completeness is safety-critical: any
/// referenced blob left out could be swept, including the tiny marker-value blob
/// that control keys (`\x00m/`, `\x00i/`, `\x00e/`, `\x00t/`) point at. Returns
/// `false` on a read error/timeout so the caller keeps the prior set.
async fn read_referenced_hashes(doc: &Doc, live: &mut HashSet<Hash>) -> bool {
    let stream = match tokio::time::timeout(
        Duration::from_secs(DOC_READ_TIMEOUT_SECS),
        doc.get_many(Query::all()),
    )
    .await
    {
        Ok(Ok(s)) => s,
        _ => return false,
    };
    let mut s = std::pin::pin!(stream);
    loop {
        match tokio::time::timeout(Duration::from_secs(DOC_READ_TIMEOUT_SECS), s.next()).await {
            Ok(Some(Ok(e))) => {
                live.insert(e.content_hash());
            }
            Ok(Some(Err(_))) | Err(_) => return false,
            Ok(None) => return true,
        }
    }
}

/// The engine owns the iroh node and the set of shares.
pub struct Engine {
    node: IrohNode,
    author: AuthorId,
    shares: HashMap<String, ShareState>,
    /// Master shares whose write key could not be loaded from the OS keystore, held
    /// **inert**: not opened, never reconciled, so they cannot touch the user's files.
    ///
    /// The old behavior was to open them read-only, which is not a degradation but a
    /// data-loss bug: a viewer treats the replica as authoritative and *reverts local
    /// edits*, so a user writing to what they believed was their own master share had
    /// those writes silently rolled back while every screen said Healthy. Seen in the
    /// field when a `systemd --user` daemon started at boot, before the login keyring
    /// was unlocked ("Secret Service: unlock prompt was dismissed").
    ///
    /// Retried by [`Engine::retry_locked_keys`], so unlocking the keyring restores the
    /// share without a daemon restart.
    locked: HashMap<String, LockedShare>,
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
    /// Long-term unhealthy-member thresholds (12 h / 8 h / 24 h in production;
    /// seconds in tests via env or [`Engine::set_health_policy`]).
    health_policy: crate::health::HealthPolicy,
    /// Open degraded-member episodes, mirroring the `peer_health` table. Keyed
    /// `(share_id, full node id)`, `""` = this device. See [`crate::health`].
    health_tracks: crate::health::Tracks,
    /// This device's custom relay settings, cached from
    /// `settings["relay_settings"]` and shared with the fallback watchdog task.
    relay_settings: Arc<StdMutex<crate::relays::RelaySettings>>,
    /// Whether the watchdog has added the public relays to the live map because
    /// no custom relay was reachable (see [`crate::relays::relay_watchdog`]).
    relay_fallback: Arc<AtomicBool>,
    /// Set by [`Engine::isolation_recoveries`] when a share is provably partitioned
    /// (reaches no member despite having members and a live rendezvous). Tells the
    /// watchdog to add the public relays even though the custom relay reads
    /// *connected* — the blackhole case that `is_connected()` alone can't catch
    /// (known-issues #23).
    force_relay_fallback: Arc<AtomicBool>,
    /// The watchdog task, aborted on [`Engine::shutdown`] so tests that build
    /// many engines don't accumulate tickers.
    relay_watchdog: tokio::task::AbortHandle,
    /// Where the node lives, kept so [`Engine::rebuild_transport`] can respawn
    /// the iroh stack on the same key, blob store and docs directory.
    data_dir: PathBuf,
    blobs_dir: PathBuf,
    /// Bumped by every [`Engine::rebuild_transport`]. A [`ReconcileJob`] built
    /// before a rebuild holds handles into the *old* node; its result is fenced
    /// off by [`Engine::finish_reconcile`] so it cannot commit into the fresh
    /// share state.
    generation: u64,
    /// Rung 2/3 throttling for the transport-repair ladder (known-issues #36).
    transport: TransportHeal,
    /// Live set for the blob store's GC loop (known-issues #22). Republished from
    /// the current replicas each periodic tick via [`Engine::gc_refresh_job`];
    /// the store's GC callback copies it in before each sweep.
    gc_protect: GcProtect,
}

/// A deferred kick of one share's doc live-sync, built under the engine lock (cheap:
/// clones the `Doc` handle + snapshots the peer set) and run **off** the lock.
///
/// Starting live-sync calls `Doc::start_sync`, which dials the peers over the network.
/// Holding the engine mutex across that dial would freeze the reconcile loop and every
/// IPC request whenever a peer is slow or unreachable, so the network work is split out
/// exactly like [`crate::presence::PresenceRejoin`]. Built by
/// [`Engine::diverged_doc_resyncs`] (periodic self-heal) and [`Engine::add_share_open`]
/// (adding a share).
pub struct DocResync {
    share_id: String,
    doc: Doc,
    peers: Vec<iroh::EndpointAddr>,
}

impl DocResync {
    /// Kick doc live-sync, propagating any error to the caller. No-op when there are
    /// no peers to dial.
    pub async fn start(self) -> anyhow::Result<()> {
        if self.peers.is_empty() {
            return Ok(());
        }
        self.doc
            .start_sync(self.peers)
            .await
            .context("start sync")?;
        Ok(())
    }

    /// Best-effort variant for the periodic self-heal loop: logs instead of returning
    /// the error (a re-kick that fails this tick is retried on the next).
    pub async fn run(self) {
        let id = self.share_id.clone();
        match self.start().await {
            Ok(()) => tracing::info!("re-kicked doc sync for out-of-sync share {id}"),
            Err(e) => tracing::debug!("resync doc for {id}: {e:#}"),
        }
    }
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
        // The relay settings live in state.db and feed endpoint construction,
        // so the DB opens before the node spawns (creating the data dir, which
        // the node used to do).
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("create data dir {}", data_dir.display()))?;
        let db = crate::db::Db::open(&data_dir.join("state.db"))?;
        let relay_settings = crate::relays::load_relay_settings(&db);
        // The blob store's GC loop is spawned inside the store, so its live-set
        // protector must exist before the node does; the engine keeps a clone to
        // republish the set from the live replicas (known-issues #22).
        let gc_protect = GcProtect::default();
        let node = IrohNode::spawn_with_blobs(
            data_dir,
            blobs_dir,
            &relay_settings,
            Some(gc_protect.gc_config()),
        )
        .await?;
        let author = node.docs_api().author_default().await?;
        let relay_settings = Arc::new(StdMutex::new(relay_settings));
        let relay_fallback = Arc::new(AtomicBool::new(false));
        let force_relay_fallback = Arc::new(AtomicBool::new(false));
        let relay_watchdog = tokio::spawn(crate::relays::relay_watchdog(
            node.endpoint.clone(),
            relay_settings.clone(),
            relay_fallback.clone(),
            force_relay_fallback.clone(),
        ))
        .abort_handle();
        let device_name = db
            .get_setting("device_name")?
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(default_device_name);
        let paused_all = db.get_setting("paused_all")?.as_deref() == Some("1");
        let health_tracks = crate::health::load_tracks(&db);
        let mut engine = Self {
            node,
            author,
            shares: HashMap::new(),
            locked: HashMap::new(),
            db,
            progress: Arc::new(StdMutex::new(HashMap::new())),
            downloads_inflight: Arc::new(StdMutex::new(HashMap::new())),
            reclaim_pending: std::collections::HashSet::new(),
            device_name: StdMutex::new(device_name),
            paused_all: StdMutex::new(paused_all),
            sync_suspended: StdMutex::new(false),
            health_policy: crate::health::HealthPolicy::from_env(),
            health_tracks,
            relay_settings,
            relay_fallback,
            force_relay_fallback,
            relay_watchdog,
            data_dir: data_dir.to_path_buf(),
            blobs_dir: blobs_dir.to_path_buf(),
            generation: 0,
            transport: TransportHeal::default(),
            gc_protect,
        };
        engine.reload_shares().await?;
        // Prime the GC live set from the shares just loaded, so an early sweep
        // (unlikely within the first hour, but possible) protects real content
        // rather than aborting.
        engine.gc_refresh_job().run().await;
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

    /// This device's custom relay configuration (empty = iroh defaults).
    pub fn relay_settings(&self) -> crate::relays::RelaySettings {
        self.relay_settings
            .lock()
            .expect("relay settings poisoned")
            .clone()
    }

    /// Replace the custom relay configuration, persist it, and apply it to the
    /// **live** endpoint: the relay map is diffed in place and the path
    /// selector's preferred set updated, so no daemon restart is needed. The
    /// endpoint re-picks its home relay from the new map within a net-report
    /// cycle (~30s); existing connections migrate rather than drop.
    pub async fn set_relay_settings(
        &self,
        settings: crate::relays::RelaySettings,
    ) -> anyhow::Result<()> {
        use crate::relays;

        // Validate fully before touching disk or the endpoint.
        let custom = relays::relay_configs(&settings)?;
        relays::save_relay_settings(&self.db, &settings)?;

        let endpoint = &self.node.endpoint;
        let custom_urls: BTreeSet<iroh::RelayUrl> = custom.iter().map(|c| c.url.clone()).collect();
        self.node.preferred_relays.set(custom_urls.clone());

        // Desired steady-state map: the custom relays, or the defaults when
        // none are configured. Fallback (if warranted) re-engages via the
        // watchdog rather than being preserved across a settings change.
        let desired: Vec<Arc<iroh::RelayConfig>> = if custom.is_empty() {
            relays::default_relay_configs()
        } else {
            custom.into_iter().map(Arc::new).collect()
        };
        let desired_urls: BTreeSet<iroh::RelayUrl> =
            desired.iter().map(|c| c.url.clone()).collect();

        // Insert first, then remove strays, so the map is never empty.
        for cfg in desired {
            endpoint.insert_relay(cfg.url.clone(), cfg).await;
        }
        let old_settings = std::mem::replace(
            &mut *self.relay_settings.lock().expect("relay settings poisoned"),
            settings,
        );
        let mut stale: BTreeSet<iroh::RelayUrl> = relays::default_relay_configs()
            .iter()
            .map(|c| c.url.clone())
            .collect();
        if let Ok(urls) = relays::relay_urls(&old_settings) {
            stale.extend(urls);
        }
        for url in stale.difference(&desired_urls) {
            endpoint.remove_relay(url).await;
        }
        self.relay_fallback.store(false, Ordering::Relaxed);

        let current = self.relay_settings.lock().expect("relay settings poisoned");
        tracing::info!(
            "relay settings updated: {} custom relay(s), mode {:?}",
            current.servers.len(),
            current.mode
        );
        Ok(())
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
            self.cancel_all_reconciles();
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
            self.cancel_all_reconciles();
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
            // Master shares keep their seed in the OS keystore; load it to restore
            // write capability.
            //
            // If it's unavailable, the share is held INERT — not opened at all — and
            // retried by `retry_locked_keys`. It emphatically must not be opened
            // read-only, which is what this used to do. Read-only is not a safe
            // fallback for a master: a viewer treats the replica as authoritative and
            // *reverts local edits*, so a user writing to what they believe is their
            // own master share would have those writes silently rolled back, with
            // nothing but one WARN to say so. Seen in the field on a `systemd --user`
            // daemon that started at boot, before the login keyring was unlocked
            // ("Secret Service: unlock prompt was dismissed").
            if rec.role_master && rec.seed_in_keyring {
                match load_seed_bounded(&rec.share_id).await {
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
                    Err(e) => {
                        let share_id = rec.share_id.clone();
                        tracing::error!(
                            "master seed for {share_id} unavailable from keystore; holding the \
                             share INERT (not syncing) until the key is available — unlock your \
                             login keyring: {e:#}"
                        );
                        self.locked.insert(
                            share_id,
                            LockedShare {
                                record: rec,
                                // 0, not `now`: retry on the very first tick rather than
                                // sitting locked for a throttle interval. The keyring
                                // often unlocks seconds after the daemon starts (the
                                // daemon races the graphical login), and that is the
                                // single most likely moment to recover.
                                last_retry: 0,
                                reason: format!("{e:#}"),
                            },
                        );
                        continue;
                    }
                }
            }
            let quick_sig = rec.quick_sig;
            let (mut state, boot) = self
                .open_share(
                    &key,
                    &PathBuf::from(&rec.folder),
                    vec![],
                    rec.ignore,
                    rec.last_seqno,
                    rec.paused,
                )
                .await?;
            // Startup path: the reconcile loop and IPC accept loop only start after
            // reload finishes, so there's no lock contention yet — kick live-sync
            // inline. Best-effort: a share whose peer can't be dialed now is retried
            // by the periodic self-heal (see `diverged_doc_resyncs`).
            if let Err(e) = state.doc.start_sync(boot).await {
                tracing::warn!(
                    "start sync for reloaded share {} failed: {e:#}",
                    rec.share_id
                );
            }
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
    ) -> anyhow::Result<(ShareState, Vec<iroh::EndpointAddr>)> {
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

        // Subscribe (keeps live sync alive + feeds the peer roster) and register
        // the namespace for serving + connect to any bootstrap peers.
        let roster = Arc::new(StdMutex::new(PeerRoster::default()));
        // Seed the roster with the members remembered from earlier sessions, so
        // the list names everyone (offline) right away instead of showing bare
        // endpoint ids — or nothing — until each member is heard again.
        match self.db.load_peer_names(&key.share_id_hex()) {
            Ok(rows) => {
                if let Ok(mut r) = roster.lock() {
                    r.preload_remembered(rows);
                }
            }
            Err(e) => tracing::debug!("load remembered peer names: {e:#}"),
        }

        // If no explicit bootstrap was given, dial every member we know of by
        // endpoint id, letting n0 DNS discovery resolve each to an address: the
        // *creating* device (its id is the one thing a bare share key carries), plus
        // every member remembered from an earlier session. This applies to a master
        // added from a master key just as much as to a viewer (multi-master): both
        // must reach *some* member to sync the doc. Our own id is dropped — a
        // creating master's key carries its own, and dialing yourself warns.
        //
        // Dialing only the creator is known-issues #16: it made the creating device a
        // silent single point of failure for every later join and restart. Adding the
        // remembered members closes that for any node that has synced at least once.
        // A first-ever join has nothing remembered yet, and is covered instead by
        // [`crate::rendezvous`].
        let self_id = self.node.endpoint.id();
        let mut bootstrap = bootstrap;
        if bootstrap.is_empty() {
            let mut ids: Vec<EndpointId> = Vec::new();
            if let Some(eid) = key.endpoint_id() {
                if let Ok(pk) = EndpointId::from_bytes(&eid) {
                    ids.push(pk);
                }
            }
            if let Ok(r) = roster.lock() {
                ids.extend(
                    r.known_peer_ids()
                        .iter()
                        .filter_map(|s| s.parse::<EndpointId>().ok()),
                );
            }
            ids.retain(|pk| *pk != self_id);
            ids.sort();
            ids.dedup();
            if !ids.is_empty() {
                tracing::info!(
                    "no bootstrap given; using endpoint-id discovery for {} known member(s)",
                    ids.len()
                );
                bootstrap.extend(ids.into_iter().map(iroh::EndpointAddr::new));
            }
        }
        spawn_event_task(&doc, roster.clone(), self_id).await?;

        // Presence: a per-share gossip topic carrying each member's name + health.
        // Bootstrap it with the same peers as the doc (the master endpoint id + any
        // explicit bootstrap addrs), minus ourselves — a master's key carries its
        // own endpoint id, and dialing yourself warns. Best-effort: a subscribe
        // failure must not fail opening the share.
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

        // NOTE: we deliberately do NOT `doc.start_sync(bootstrap)` here. Starting
        // live-sync dials the bootstrap peers over the network; doing that while the
        // caller holds the engine lock would freeze the reconcile loop and all IPC if
        // a peer is slow or unreachable. Callers start sync themselves — inline where
        // there's no lock contention (create/reload), off-lock in the daemon's
        // AddShare handler via [`DocResync`]. The resolved bootstrap set (which may
        // include the endpoint-id discovery hint added above) is returned for that.
        Ok((
            ShareState {
                key: key.clone(),
                folder: folder.to_path_buf(),
                doc,
                ignore,
                last_seqno,
                last_quick_sig: 0,
                paused,
                publishing: false,
                cancel: Arc::new(AtomicBool::new(false)),
                roster,
                last_updated: 0,
                // Provisional until the first reconcile computes real completeness. Start
                // at 0 (incomplete) for every role so a freshly-added master that still
                // has content to fetch never briefly reads a misleading 100.
                health: 0,
                presence,
                skipped: Vec::new(),
                manifest_fp: 0,
                diverged_since: None,
                divergence_sig: 0,
                diverged_alerted: false,
                // Stagger the first periodic deep verify a full interval out, so a
                // restart doesn't re-hash every share immediately.
                last_deep_verify: now_secs(),
                force_deep_verify: false,
                diverged_deep_verified: false,
                full_scans: 0,
                last_doc_resync: 0,
                we_minted: key.endpoint_id() == Some(self.node.endpoint_id_bytes()),
                last_rendezvous: 0,
                last_rendezvous_publish: 0,
                heal: ConnHeal::default(),
            },
            bootstrap,
        ))
    }

    pub fn endpoint_addr(&self) -> iroh::EndpointAddr {
        self.node.addr()
    }

    /// This node's iroh endpoint. Exposed for [`crate::rendezvous`]'s publish/resolve,
    /// which borrow the endpoint's TLS trust anchors and DNS resolver.
    pub fn endpoint(&self) -> &iroh::Endpoint {
        &self.node.endpoint
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
                let retrying = s.skipped.len() as u32;
                let out_of_sync = s.is_out_of_sync();
                let (status, percent, indexed_bytes, index_total) = if s.paused || paused_all {
                    (seed_ipc::ShareStatus::Paused, 0, 0, 0)
                } else if let Some(&(done, tot)) = progress.get(id) {
                    let pct = (done.min(tot) * 100).checked_div(tot).unwrap_or(0) as u8;
                    (seed_ipc::ShareStatus::Indexing, pct, done, tot)
                } else if s.isolated() {
                    // Ranked above OutOfSync deliberately. Divergence is a claim about
                    // what our *peers* hold, and `diverged_since` is sticky — so a node
                    // that diverged and then lost every peer would keep insisting
                    // "members disagree" about members it can no longer hear at all.
                    // Being partitioned is both the truer statement and the one the
                    // user has to fix first: no other condition can even be assessed,
                    // let alone repaired, until this node can reach somebody.
                    (seed_ipc::ShareStatus::NoPeers, s.health, 0, 0)
                } else if out_of_sync {
                    // Members disagree on the fileset past the settle window — the most
                    // serious steady-state condition; never read "Healthy".
                    (seed_ipc::ShareStatus::OutOfSync, s.health, 0, 0)
                } else if retrying > 0 {
                    // Files we can't read/publish yet (locked/unreadable) are being
                    // retried — the share is NOT settled even if content % looks full.
                    // Never read "Healthy" in this state.
                    (seed_ipc::ShareStatus::Syncing, s.health, 0, 0)
                } else if s.health >= 100 && s.converged_with_online_peers() {
                    (seed_ipc::ShareStatus::Healthy, 100, 0, 0)
                } else if s.health >= 100 {
                    // Content-complete against the manifest we currently hold, but an
                    // online peer advertises a different fingerprint — we haven't
                    // converged toward the agreed fileset yet (our doc replica is still
                    // catching up). Show Syncing rather than a premature Healthy 100%,
                    // and cap the bar below 100 so it doesn't read "done". Persistent
                    // disagreement escalates to OutOfSync above once past the settle
                    // window.
                    (seed_ipc::ShareStatus::Syncing, 99, 0, 0)
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
                    retrying,
                }
            })
            // Shares held inert because their write key is locked in the OS keystore are
            // NOT in `self.shares` — they were never opened. They must still be listed:
            // a share that silently vanishes from the UI is its own kind of lie, and the
            // user needs to see *why* it isn't syncing (and that the cure is to unlock
            // their keyring, which nothing else would ever suggest).
            .chain(self.locked.values().map(|l| {
                let folder = PathBuf::from(&l.record.folder);
                seed_ipc::ShareSummary {
                    share_id: l.record.share_id.clone(),
                    name: folder
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| l.record.share_id.clone()),
                    folder: l.record.folder.clone(),
                    role: seed_ipc::Role::Master,
                    status: seed_ipc::ShareStatus::KeyLocked,
                    percent: 0,
                    online: 1,
                    total: 1,
                    paused: l.record.paused,
                    indexed_bytes: 0,
                    index_total: 0,
                    last_updated: 0,
                    retrying: 0,
                }
            }))
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
        let now = now_secs();
        let mut out = vec![seed_ipc::PeerInfo {
            node_id: "This device".into(),
            name: Some(self.device_name()),
            role,
            online: true,
            last_seen: now,
            have_seqno: state.last_seqno,
            percent: state.health,
            manifest_fp: state.manifest_fp,
            unhealthy_secs: self
                .health_tracks
                .get(&(share_id.to_string(), String::new()))
                .map(|r| crate::health::accrued(r, now))
                .unwrap_or(0),
            path: None,
            last_dial_ok: 0,
            last_dial_err: None,
            last_dial_err_at: 0,
        }];
        out.extend(state.roster.lock().map(|r| r.infos()).unwrap_or_default());
        // Health episodes are keyed by the FULL endpoint id; `infos()` shows the
        // 16-char short form. Match by prefix (collision odds are negligible and
        // the consequence is a cosmetic duration on the wrong row).
        for p in out.iter_mut().skip(1) {
            p.unhealthy_secs = self
                .health_tracks
                .iter()
                .find(|((s, n), _)| s == share_id && n.starts_with(&p.node_id))
                .map(|(_, r)| crate::health::accrued(r, now))
                .unwrap_or(0);
        }
        Ok(out)
    }

    /// Annotate a [`Engine::peers`] result with how this device currently
    /// reaches each online member: a direct (hole-punched / LAN) path, or via
    /// which relay. Reads the endpoint's per-remote transport snapshot, which
    /// is async — kept separate from `peers()` so membership queries stay sync.
    /// Members without a recent iroh connection keep `path: None`.
    pub async fn annotate_peer_paths(&self, share_id: &str, peers: &mut [seed_ipc::PeerInfo]) {
        let Some(state) = self.shares.get(share_id) else {
            return;
        };
        // `PeerInfo::node_id` is the 16-char short form; the endpoint wants the
        // full id. Recover it from the roster keys by prefix.
        let full_ids: Vec<String> = state
            .roster
            .lock()
            .map(|r| r.peer_ids())
            .unwrap_or_default();
        for p in peers.iter_mut() {
            if !p.online || p.node_id == "This device" {
                continue;
            }
            let Some(full) = full_ids.iter().find(|f| f.starts_with(&p.node_id)) else {
                continue;
            };
            let Ok(id) = full.parse::<EndpointId>() else {
                continue;
            };
            let Some(info) = self.node.endpoint.remote_info(id).await else {
                continue;
            };
            let mut relay: Option<String> = None;
            let mut direct = false;
            for a in info.addrs() {
                if !matches!(a.usage(), iroh::endpoint::TransportAddrUsage::Active) {
                    continue;
                }
                match a.addr() {
                    iroh::TransportAddr::Ip(_) => direct = true,
                    iroh::TransportAddr::Relay(url) => {
                        relay = Some(url.host_str().unwrap_or(url.as_str()).to_string());
                    }
                    _ => {}
                }
            }
            // A direct path outranks a coexisting relay path (iroh's selector
            // always prefers it, so that's where the traffic actually flows).
            p.path = if direct {
                Some(seed_ipc::PeerPath::Direct)
            } else {
                relay.map(seed_ipc::PeerPath::Relay)
            };
        }
    }

    /// Replace the long-term health thresholds (tests/soaks shrink hours to
    /// seconds). Production uses the env/default policy set at construction.
    pub fn set_health_policy(&mut self, policy: crate::health::HealthPolicy) {
        self.health_policy = policy;
    }

    /// Open long-term-health episodes for a share (the `GetPeerHealth` IPC
    /// poll). One entry per member with an open episode; an empty list means
    /// nobody has been degraded long enough to track.
    pub fn peer_health(&self, share_id: &str) -> anyhow::Result<Vec<seed_ipc::PeerHealthInfo>> {
        let state = self
            .shares
            .get(share_id)
            .ok_or_else(|| anyhow!("unknown share {share_id}"))?;
        let snap = state
            .roster
            .lock()
            .map(|r| r.health_snapshot())
            .unwrap_or_default();
        let now = now_secs();
        Ok(self
            .health_tracks
            .iter()
            .filter(|((s, _), _)| s == share_id)
            .map(|((_, node_id), row)| {
                let peer = snap.iter().find(|p| &p.id == node_id);
                seed_ipc::PeerHealthInfo {
                    node_id: node_id.clone(),
                    name: if node_id.is_empty() {
                        Some(self.device_name())
                    } else {
                        peer.and_then(|p| p.name.clone())
                    },
                    online: node_id.is_empty() || peer.map(|p| p.online).unwrap_or(false),
                    percent: row.last_percent,
                    unhealthy_secs: crate::health::accrued(row, now),
                    alerted: row.last_notified_at > 0,
                }
            })
            .collect())
    }

    /// Run the long-term health detector over every share and return the
    /// notifications due this pass. Synchronous, no awaits, transition-only DB
    /// writes — call under a brief engine lock and emit the results off-lock.
    ///
    /// Semantics (see [`crate::health`]): a member is degraded while *online but
    /// not fully synced* (percent < 100, or fingerprint off the master-majority
    /// consensus); offline pauses its clock. Every node self-reports; only
    /// masters produce alerts about *other* members, so one broken viewer nags
    /// its own operator and the masters — not the whole fleet.
    pub fn health_alerts(&mut self) -> Vec<PeerHealthAlert> {
        use crate::health::{consensus_fp, observe, Observation, TrackEvent};
        let now = now_secs();
        let policy = self.health_policy;
        let paused_all = self.paused_all();
        let mut alerts = Vec::new();
        for (share_id, state) in self.shares.iter() {
            let share_name = state
                .folder
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| share_id.clone());
            let snap = state
                .roster
                .lock()
                .map(|r| r.health_snapshot())
                .unwrap_or_default();
            let self_master = matches!(state.key.role, Role::Master);

            let mut votes: Vec<u64> = snap
                .iter()
                .filter(|p| p.online && p.is_master && p.manifest_fp != 0)
                .map(|p| p.manifest_fp)
                .collect();
            if self_master && state.manifest_fp != 0 {
                votes.push(state.manifest_fp);
            }
            let consensus = consensus_fp(&votes);

            // Self: degraded only while unpaused with someone online to sync
            // against — a lone or user-paused node never nags its operator.
            //
            // With one exception, and it is the whole point of known-issues #17:
            // being unable to hear *anyone* is not the same as being alone. The
            // old rule lumped them together and treated both as Offline, which
            // pauses the episode clock — so a node partitioned from its entire
            // pool accrued no unhealthy time and never raised an alert. A share
            // we joined (or one whose members we have known and can now reach
            // none of) is degraded, and says so on the same 12h escalation as any
            // other long-term fault. A share we created that nobody has joined is
            // still genuinely alone, and still stays quiet.
            let any_online_peer = snap.iter().any(|p| p.online);
            let self_obs = if state.paused || paused_all {
                Observation::Offline
            } else if state.isolated() {
                Observation::OnlineDegraded
            } else if !any_online_peer {
                Observation::Offline
            } else if state.health < 100 || state.is_out_of_sync() {
                Observation::OnlineDegraded
            } else {
                Observation::OnlineHealthy
            };
            if let Some(ev) = observe(
                &mut self.health_tracks,
                &self.db,
                &policy,
                now,
                share_id,
                "",
                self_obs,
                state.health,
            ) {
                alerts.push(PeerHealthAlert {
                    share_id: share_id.clone(),
                    share_name: share_name.clone(),
                    node_id: String::new(),
                    name: None,
                    percent: state.health,
                    unhealthy_secs: match ev {
                        TrackEvent::Degraded(secs) => secs,
                        TrackEvent::Recovered => 0,
                    },
                    is_self: true,
                    recovered: ev == TrackEvent::Recovered,
                });
            }

            if !self_master {
                continue;
            }
            for p in &snap {
                let obs = if !p.online {
                    Observation::Offline
                } else if p.percent < 100
                    || (p.manifest_fp != 0 && consensus.is_some_and(|f| p.manifest_fp != f))
                {
                    Observation::OnlineDegraded
                } else {
                    Observation::OnlineHealthy
                };
                if let Some(ev) = observe(
                    &mut self.health_tracks,
                    &self.db,
                    &policy,
                    now,
                    share_id,
                    &p.id,
                    obs,
                    p.percent,
                ) {
                    alerts.push(PeerHealthAlert {
                        share_id: share_id.clone(),
                        share_name: share_name.clone(),
                        node_id: p.id.clone(),
                        name: p.name.clone(),
                        percent: p.percent,
                        unhealthy_secs: match ev {
                            TrackEvent::Degraded(secs) => secs,
                            TrackEvent::Recovered => 0,
                        },
                        is_self: false,
                        recovered: ev == TrackEvent::Recovered,
                    });
                }
            }
            // Episodes for peers the roster no longer knows at all (e.g. after a
            // restart rebuilt it from gossip): observe them offline so they pause
            // and eventually expire rather than lingering forever.
            let seen: HashSet<&str> = snap.iter().map(|p| p.id.as_str()).collect();
            let vanished: Vec<String> = self
                .health_tracks
                .keys()
                .filter(|(s, n)| s == share_id && !n.is_empty() && !seen.contains(n.as_str()))
                .map(|(_, n)| n.clone())
                .collect();
            for node_id in vanished {
                let _ = observe(
                    &mut self.health_tracks,
                    &self.db,
                    &policy,
                    now,
                    share_id,
                    &node_id,
                    Observation::Offline,
                    0,
                );
            }
        }
        for a in &alerts {
            if a.recovered {
                tracing::info!(
                    "peer-health: share '{}' {} recovered (back in sync)",
                    a.share_name,
                    if a.is_self {
                        "this device"
                    } else {
                        a.name.as_deref().unwrap_or(&a.node_id)
                    },
                );
            } else {
                tracing::warn!(
                    "peer-health: share '{}' {} unhealthy for {}s ({}%)",
                    a.share_name,
                    if a.is_self {
                        "this device"
                    } else {
                        a.name.as_deref().unwrap_or(&a.node_id)
                    },
                    a.unhealthy_secs,
                    a.percent,
                );
            }
        }
        alerts
    }

    /// Build this tick's presence broadcasts — one per share with a live presence
    /// channel. Call under the engine lock (cheap: clones the gossip sender +
    /// pre-encodes); send the results off-lock via [`PresenceBroadcast::send`].
    ///
    /// Piggybacked here (both hosts call this every ~3s, for every share
    /// including paused ones): flush remembered-identity changes to the
    /// `peer_names` table. Almost always a no-op; when not, a handful of tiny
    /// WAL upserts.
    pub fn presence_broadcasts(&self) -> Vec<crate::presence::PresenceBroadcast> {
        self.flush_peer_names();
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
                manifest_fp: s.manifest_fp,
            };
            out.push(crate::presence::PresenceBroadcast::new(
                h.sender.clone(),
                &p,
            ));
        }
        out
    }

    /// Persist remembered-identity changes (see [`PeerRoster::drain_dirty_names`])
    /// to the `peer_names` table, so last-known member names survive a daemon
    /// restart. Change-driven: no dirty identities, no DB writes.
    fn flush_peer_names(&self) {
        for (share_id, s) in &self.shares {
            let rows = match s.roster.lock() {
                Ok(mut r) => r.drain_dirty_names(share_id),
                Err(_) => continue,
            };
            for row in rows {
                if let Err(e) = self.db.upsert_peer_name(&row) {
                    tracing::debug!("persist peer name for {}: {e:#}", row.node_id);
                }
            }
        }
    }

    /// Build this tick's gossip re-join requests — one per share whose presence
    /// roster is missing members — asking the swarm to connect to a small random
    /// sample of the peers we can't currently hear. Call under the engine lock
    /// (cheap: clones the gossip sender + snapshots the roster); run the results
    /// off-lock via [`PresenceRejoin::join`].
    ///
    /// This repairs the presence mesh: gossip's one-shot bootstrap leaves a partitioned
    /// star (the creator bootstraps with nothing; leaves only dial the creator), so
    /// without this, presence reaches 3+ member pools asymmetrically. The candidate set
    /// comes from [`peer_providers`] (the master id carried in the key + every endpoint
    /// id the roster learned from doc events), minus ourselves; targets are the
    /// not-heard-from subset, capped at [`PRESENCE_REJOIN_SAMPLE`] — joining the full
    /// set every tick fragmented the overlay at fleet scale (known-issues #9). Once
    /// every known member is heard, this returns nothing for the share and gossip's
    /// own shuffle maintains the mesh.
    ///
    /// [`PresenceRejoin::join`]: crate::presence::PresenceRejoin::join
    pub fn presence_rejoins(&self) -> Vec<crate::presence::PresenceRejoin> {
        let self_id = self.node.endpoint.id();
        let mut rng = rand::thread_rng();
        let mut out = Vec::new();
        for s in self.shares.values() {
            let Some(h) = s.presence.as_ref() else {
                continue;
            };
            let candidates: Vec<EndpointId> = peer_providers(&s.key, &s.roster)
                .into_iter()
                .filter(|id| *id != self_id)
                .collect();
            if candidates.is_empty() {
                continue;
            }
            let online: HashSet<String> = match s.roster.lock() {
                Ok(r) => r.online_peer_ids().into_iter().collect(),
                Err(_) => continue,
            };
            let peers = select_rejoin_targets(&mut rng, candidates, &online);
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

    /// Force the next reconcile of `share_id` to do a full hashing scan (deep
    /// verify): re-examine every file on disk against the manifest rather than
    /// trusting the cheap change-signature. Catches drift the signature can't see
    /// (in-place corruption with unchanged size+mtime, a stale index) and re-asserts
    /// / re-materializes to re-converge. Cheap to request — it just sets a pending
    /// flag; `last_deep_verify` advances only when the forced scan completes.
    pub fn request_deep_verify(&mut self, share_id: &str) {
        if let Some(s) = self.shares.get_mut(share_id) {
            s.force_deep_verify = true;
        }
    }

    /// Force a deep verify of every non-paused share whose last one is older than
    /// [`DEEP_VERIFY_INTERVAL_SECS`]. Called periodically by the daemon; returns the
    /// ids that were due (for logging). A no-op for shares verified recently or
    /// with a verify already pending (so a pending one isn't re-logged every call).
    pub fn periodic_deep_verify(&mut self) -> Vec<String> {
        let now = now_secs();
        let due: Vec<String> = self
            .shares
            .iter()
            .filter(|(_, s)| {
                !s.paused
                    && !s.force_deep_verify
                    && now - s.last_deep_verify >= DEEP_VERIFY_INTERVAL_SECS
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &due {
            if let Some(s) = self.shares.get_mut(id) {
                s.force_deep_verify = true;
            }
        }
        due
    }

    /// Abort in-flight downloads older than [`DOWNLOAD_STALL_ABORT_SECS`] so the
    /// next reconcile re-queues them (chunks persist; resumes where it left off).
    /// The in-flight map dedupes by hash, so without this sweep a download future
    /// that wedges without settling blocks its blob permanently — the full-size
    /// soak hit exactly that (nodes pinned at 0–5% for hours; a manual
    /// pause/resume, which aborts + re-queues, was the only unstick). Sync +
    /// brief-lock safe; the daemon runs it on the presence-loop cadence. Returns
    /// how many were recycled (0 in healthy operation).
    pub fn abort_stalled_downloads(&self) -> usize {
        let mut aborted = 0;
        if let Ok(mut inflight) = self.downloads_inflight.lock() {
            inflight.retain(|hash, dl| {
                if dl.started.elapsed().as_secs() >= DOWNLOAD_STALL_ABORT_SECS {
                    dl.abort.abort();
                    tracing::warn!(
                        "aborting stalled download {hash} (share {}, in flight {}s) — \
                         re-queued next tick, verified chunks resume",
                        dl.share_id,
                        dl.started.elapsed().as_secs(),
                    );
                    aborted += 1;
                    false
                } else {
                    true
                }
            });
        }
        aborted
    }

    /// Full hashing scans committed for a share this session. Diagnostic for tests
    /// and soaks asserting the rescan policy; not part of the IPC surface.
    #[doc(hidden)]
    pub fn debug_full_scans(&self, share_id: &str) -> u64 {
        self.shares.get(share_id).map(|s| s.full_scans).unwrap_or(0)
    }

    /// Whether a deep verify is pending (requested but not yet committed) for a
    /// share. Diagnostic counterpart to [`Engine::request_deep_verify`].
    #[doc(hidden)]
    pub fn debug_deep_verify_pending(&self, share_id: &str) -> bool {
        self.shares
            .get(share_id)
            .map(|s| s.force_deep_verify)
            .unwrap_or(false)
    }

    /// Re-kick a share's doc live-sync against its currently-known peers, in case
    /// replication stalled. Best-effort: a no-op when no peers are known, and errors
    /// are returned for the caller to log rather than being fatal.
    pub async fn resync_doc(&self, share_id: &str) -> anyhow::Result<()> {
        let self_id = self.node.endpoint.id();
        let (doc, peers) = {
            let s = self
                .shares
                .get(share_id)
                .ok_or_else(|| anyhow!("unknown share {share_id}"))?;
            let peers: Vec<iroh::EndpointAddr> = peer_providers(&s.key, &s.roster)
                .into_iter()
                .filter(|pid| *pid != self_id)
                .map(iroh::EndpointAddr::new)
                .collect();
            (s.doc.clone(), peers)
        };
        if peers.is_empty() {
            return Ok(());
        }
        doc.start_sync(peers).await.context("resync doc")?;
        Ok(())
    }

    /// Self-heal step for the daemon: re-kick doc live-sync for every share that is
    /// currently out of sync with a peer. Pairs with the forced deep verify done in
    /// [`finish_reconcile`] (re-assert local truth) to drive re-convergence.
    /// Build off-lock re-sync jobs for every out-of-sync share, WITHOUT awaiting.
    /// Call under a brief engine lock and run the results off-lock via
    /// [`DocResync::run`].
    ///
    /// Replaces the old `resync_diverged_docs`, which held the engine mutex across
    /// every `start_sync` dial: a slow or unreachable peer would freeze the whole
    /// reconcile loop (and every IPC request) until the dial returned — the one place
    /// the loop broke its otherwise-consistent "build under the lock, await off-lock"
    /// discipline. This mirrors [`presence_rejoins`].
    ///
    /// Bounded like the mesh repair: at most one kick per share per
    /// [`DIVERGENCE_RESYNC_KICK_SECS`], against at most [`DOC_RESYNC_SAMPLE`]
    /// randomly-sampled members. The unthrottled version (every ask, all ~27
    /// peers) ran pairwise set reconciliation everywhere at once and saturated
    /// the 28-node fleet's CPU within minutes of load.
    ///
    /// [`presence_rejoins`]: Engine::presence_rejoins
    pub fn diverged_doc_resyncs(&mut self) -> Vec<DocResync> {
        let self_id = self.node.endpoint.id();
        let mut rng = rand::thread_rng();
        let now = now_secs();
        let mut out = Vec::new();
        for (id, s) in self.shares.iter_mut() {
            if s.paused || !s.is_out_of_sync() {
                continue;
            }
            if now - s.last_doc_resync < DIVERGENCE_RESYNC_KICK_SECS {
                continue;
            }
            let mut peers: Vec<iroh::EndpointAddr> = peer_providers(&s.key, &s.roster)
                .into_iter()
                .filter(|pid| *pid != self_id)
                .map(iroh::EndpointAddr::new)
                .collect();
            if peers.is_empty() {
                continue;
            }
            peers.shuffle(&mut rng);
            peers.truncate(DOC_RESYNC_SAMPLE);
            s.last_doc_resync = now;
            out.push(DocResync {
                share_id: id.clone(),
                doc: s.doc.clone(),
                peers,
            });
        }
        out
    }

    /// Each master's periodic rendezvous publish: advertise this device's address
    /// under the share's public key, so any key holder can find a live master without
    /// the creating device being up (known-issues #16). Built under the engine lock,
    /// awaited off it — see [`DocResync`] for why that split is mandatory.
    ///
    /// Masters only: signing the record needs the share seed, which is exactly what a
    /// viewer key does not carry.
    ///
    /// Deliberately **not** gated on pause. A pause means "stop syncing this folder",
    /// not "make this pool unjoinable" — and suppressing the advertisement while
    /// paused would carve out a fresh #16-shaped hole, where a joiner is stranded
    /// because the one master that was up happened to be paused. The record is a
    /// reachability advertisement, not sync work, and costs one small HTTP PUT per
    /// [`rendezvous::REPUBLISH_SECS`].
    pub fn rendezvous_publishes(&mut self) -> Vec<crate::rendezvous::RendezvousPublish> {
        let endpoint = self.node.endpoint.clone();
        // No relay home and no direct addresses yet (startup, or offline): there is
        // nothing dialable to advertise. Bail *before* the throttle is stamped —
        // stamping here would burn the attempt and leave a just-restarted master
        // unadvertised for a full republish interval, which is the exact window a
        // joiner is most likely to be waiting in.
        if endpoint.addr().addrs.is_empty() {
            return Vec::new();
        }
        let now = now_secs();
        let mut out = Vec::new();
        for (id, s) in self.shares.iter_mut() {
            let Some(seed) = s.key.seed_bytes() else {
                continue; // viewer: cannot sign the record
            };
            if now - s.last_rendezvous_publish < crate::rendezvous::REPUBLISH_SECS {
                continue;
            }
            s.last_rendezvous_publish = now;
            out.push(crate::rendezvous::RendezvousPublish {
                share_id: id.clone(),
                endpoint: endpoint.clone(),
                seed,
            });
        }
        out
    }

    /// Rendezvous lookups for shares that can currently reach **no** member: resolve
    /// whichever master published most recently and bootstrap doc sync + presence
    /// gossip from it.
    ///
    /// This is the path that rescues a node the rest of the engine cannot: a
    /// first-ever join whose creator is offline has nothing in `peer_names` to fall
    /// back on (so [`peer_providers`] is just the dead creator), no doc replica (so
    /// the member registry is empty), and no gossip contact (so presence is silent).
    /// Every existing repair mechanism is downstream of first contact. This one is
    /// not: it needs only the share's public key, which every key holder has.
    ///
    /// Throttled to one lookup per share per [`rendezvous::LOOKUP_SECS`] and skipped
    /// entirely once *any* member is reachable — a healthy pool never touches the
    /// pkarr server.
    pub fn rendezvous_dials(&mut self) -> Vec<crate::rendezvous::RendezvousDial> {
        let endpoint = self.node.endpoint.clone();
        let paused_all = self.paused_all();
        let now = now_secs();
        let mut out = Vec::new();
        for (id, s) in self.shares.iter_mut() {
            if s.paused || paused_all || !s.isolated() {
                continue;
            }
            if now - s.last_rendezvous < crate::rendezvous::LOOKUP_SECS {
                continue;
            }
            s.last_rendezvous = now;
            out.push(crate::rendezvous::RendezvousDial {
                share_id: id.clone(),
                endpoint: endpoint.clone(),
                master_pub: s.key.master_pub_bytes(),
                doc: s.doc.clone(),
                presence: s.presence.as_ref().map(|h| h.sender.clone()),
                roster: s.roster.clone(),
            });
        }
        out
    }

    /// Connectivity self-heal (known-issues #23): two
    /// independent ladders that between them cover both failure modes seen in the
    /// 2026-07 field incident, so neither needs a human to clear.
    ///
    /// **Ladder 1 — total isolation (transport dead).** A share that reaches *no*
    /// member while it *has* members to reach — rendezvous keeps resolving live
    /// masters, presence rejoin and doc resync keep firing, yet nothing connects — is
    /// a real partition. This is what happened when every member homed on a custom
    /// relay that answered its own handshake/ping while forwarding no client↔client
    /// traffic: `is_connected()` stayed true, the fallback watchdog never engaged, and
    /// only a human removing the relay or restarting fixed it. Past
    /// [`ISOLATION_HEAL_SECS`] we log one loud WARN and set the endpoint-wide
    /// force-fallback signal (the automatic, reversible equivalent of that manual
    /// `relay-remove`); past [`ISOLATION_PRESENCE_REBUILD_SECS`] we also rebuild
    /// presence + re-kick doc sync.
    ///
    /// **Ladder 2 — presence overlay silent while transport is alive.** After a
    /// partition, doc-sync often recovers (successful `SyncFinished` events mark peers
    /// online) while the gossip presence overlay stays silently dead: peers stick at
    /// `seqno=0` and the member list flaps on the TTL. Because doc-sync keeps the
    /// share *non-isolated*, ladder 1 never fires. Ladder 2 keys on presence
    /// staleness instead — transport fresh (we've had peer contact recently) but no
    /// presence heartbeat for a sustained window — and rebuilds the subscription,
    /// which is the only thing a restart really does for this case.
    ///
    /// Both ladders run on flap-hysteretic [`EpisodeClock`]s (known-issues #35):
    /// a healthy blip shorter than [`HEAL_CLEAR_SECS`] neither ends an episode nor
    /// resets its elapsed time, so a share oscillating faster than the ladder
    /// thresholds still gets healed — and the forced-relay fallback stays latched
    /// through the flap instead of thrashing per blip.
    ///
    /// Returns [`DocResync`] jobs to run off-lock (ladder 1's doc re-kick), mirroring
    /// [`Engine::retry_locked_keys`]. Subscribing to gossip is a local actor hand-off
    /// (not a network dial), so rebuilding the handle inline under the lock is
    /// consistent with [`Engine::open_share`], which does the same.
    pub async fn connectivity_recoveries(&mut self) -> Vec<DocResync> {
        let paused_all = self.paused_all();
        let self_id = self.node.endpoint.id();
        let gossip = self.node.gossip.clone();
        let now = now_secs();
        let mut any_partitioned = false;
        let mut jobs = Vec::new();
        // Ladder 3 (known-issues #36) bookkeeping, resolved after the loop
        // because rung 1 and rung 2 act on the shared endpoint.
        let mut rung1: Vec<String> = Vec::new();
        let mut want_rebuild = false;
        let mut any_outbound_episode = false;

        for (id, s) in self.shares.iter_mut() {
            if s.paused || paused_all {
                s.heal = ConnHeal::default();
                continue;
            }
            // One roster snapshot drives both ladders.
            let (known, last_contact, last_presence, last_err, dial) = s
                .roster
                .lock()
                .map(|r| {
                    (
                        r.counts().1,
                        r.last_contact(),
                        r.last_presence(),
                        r.last_sync_err().map(|(p, e, _)| format!("{p}: {e}")),
                        r.dial_stats(),
                    )
                })
                .unwrap_or((0, 0, 0, None, DialStats::default()));
            let isolated = s.isolated();

            // --- Ladder 1: total isolation ---
            // The episode clock is flap-hysteretic (known-issues #35): a healthy
            // blip shorter than HEAL_CLEAR_SECS keeps the episode — and with it
            // the forced-relay fallback and the periodic rebuilds — engaged, and
            // elapsed accrues from the episode's true start.
            match s.heal.isolated.observe(isolated, now) {
                Some(elapsed) if elapsed >= ISOLATION_HEAL_SECS => {
                    any_partitioned = true;
                    if !s.heal.isolated_warned {
                        s.heal.isolated_warned = true;
                        let contact = if last_contact == 0 {
                            "no peer heard this session".to_string()
                        } else {
                            format!("last heard a peer {}s ago", now - last_contact)
                        };
                        let err = last_err.unwrap_or_else(|| "no sync attempts recorded".into());
                        tracing::warn!(
                            "share {id}: PARTITIONED — cannot reach any of {known} known \
                             member(s) for {elapsed}s ({contact}; last dial error — {err}); \
                             forcing public-relay fallback \
                             (known-issues #23)"
                        );
                    }
                    if elapsed >= ISOLATION_PRESENCE_REBUILD_SECS
                        && now - s.heal.last_presence_rebuild >= PRESENCE_REBUILD_MIN_SECS
                    {
                        s.heal.last_presence_rebuild = now;
                        if rebuild_presence(&gossip, self_id, id, s).await {
                            tracing::info!(
                                "rebuilt presence subscription for partitioned share {id}"
                            );
                        }
                        let peers: Vec<iroh::EndpointAddr> = peer_providers(&s.key, &s.roster)
                            .into_iter()
                            .filter(|pid| *pid != self_id)
                            .map(iroh::EndpointAddr::new)
                            .collect();
                        if !peers.is_empty() {
                            jobs.push(DocResync {
                                share_id: id.clone(),
                                doc: s.doc.clone(),
                                peers,
                            });
                        }
                    }
                }
                Some(_) => {} // episode active but under the ladder threshold
                None => {
                    // Episode over: the share has been reachable continuously for
                    // HEAL_CLEAR_SECS. Log the recovery once, so an outage reads
                    // as a bracketed episode in the log instead of trailing off.
                    if std::mem::take(&mut s.heal.isolated_warned) {
                        tracing::info!(
                            "share {id}: members reachable again — partition episode over \
                             (stable for {HEAL_CLEAR_SECS}s)"
                        );
                    }
                }
            }

            // --- Ladder 2: presence overlay silent while transport is alive ---
            let overlay_dead =
                presence_overlay_dead(known, isolated, last_contact, last_presence, now);
            if let Some(gap) = s.heal.presence_gap.observe(overlay_dead, now) {
                if gap >= PRESENCE_GAP_HEAL_SECS
                    && now - s.heal.last_presence_rebuild >= PRESENCE_REBUILD_MIN_SECS
                {
                    s.heal.last_presence_rebuild = now;
                    if rebuild_presence(&gossip, self_id, id, s).await {
                        tracing::warn!(
                            "share {id}: presence overlay silent for {gap}s while doc-sync is \
                             working (peers stuck at seqno=0); rebuilt the presence \
                             subscription (known-issues #23)"
                        );
                    }
                }
            }

            // --- Ladder 3: members alive, our outbound dials dead (known-issues #36) ---
            let dead = outbound_dead(known, last_contact, &dial, now);
            match s.heal.outbound.observe(dead, now) {
                Some(elapsed) => {
                    any_outbound_episode = true;
                    if elapsed >= OUTBOUND_DEAD_SECS && !s.heal.outbound_warned {
                        s.heal.outbound_warned = true;
                        let (err, err_age) = dial
                            .last_outbound_err
                            .as_ref()
                            .map(|(m, t)| (m.clone(), now - t))
                            .unwrap_or_default();
                        let age = |t: i64| {
                            if t == 0 {
                                "never".to_string()
                            } else {
                                format!("{}s ago", now - t)
                            }
                        };
                        tracing::warn!(
                            "share {id}: members are alive (contact {}, inbound sync {}, \
                             rendezvous {}) but every outbound dial has failed for {elapsed}s \
                             ({} consecutive; last {err_age}s ago: {err}) — transport repair \
                             engaging (known-issues #36)",
                            age(last_contact),
                            age(dial.last_inbound_ok),
                            age(dial.rendezvous_alive),
                            dial.outbound_failures,
                        );
                    }
                    if elapsed >= OUTBOUND_DEAD_SECS && s.heal.rung1_at == 0 {
                        s.heal.rung1_at = now;
                        rung1.push(id.clone());
                    }
                    if elapsed >= OUTBOUND_REBUILD_SECS
                        && s.heal.rung1_at != 0
                        && now - s.heal.rung1_at >= OUTBOUND_DEAD_SECS
                    {
                        want_rebuild = true;
                    }
                }
                None => {
                    if std::mem::take(&mut s.heal.outbound_warned) {
                        tracing::info!(
                            "share {id}: outbound dials succeeding again — transport episode over"
                        );
                    }
                    s.heal.rung1_at = 0;
                }
            }
        }

        // Ladder 3, rung 1: the cheap restart-equivalent — rebind the socket,
        // re-home the relay, and let iroh re-probe every remote (exactly what
        // `on_resume` does for a known teardown), then re-kick the share.
        if !rung1.is_empty() {
            tracing::warn!(
                "transport repair rung 1: re-probing the network for {} share(s)",
                rung1.len()
            );
            let _ = tokio::time::timeout(
                Duration::from_secs(RESUME_NETWORK_CHANGE_TIMEOUT_SECS),
                self.node.endpoint.network_change(),
            )
            .await;
            for id in &rung1 {
                if let Some(s) = self.shares.get_mut(id) {
                    s.heal.last_presence_rebuild = now;
                    let _ = rebuild_presence(&gossip, self_id, id, s).await;
                    let peers: Vec<iroh::EndpointAddr> = peer_providers(&s.key, &s.roster)
                        .into_iter()
                        .filter(|pid| *pid != self_id)
                        .map(iroh::EndpointAddr::new)
                        .collect();
                    if !peers.is_empty() {
                        jobs.push(DocResync {
                            share_id: id.clone(),
                            doc: s.doc.clone(),
                            peers,
                        });
                    }
                }
            }
        }
        if !any_outbound_episode {
            self.transport.backoff = 0;
        }
        // Ladder 3, rung 2: rung 1 did not bring outbound dials back — rebuild
        // the iroh node in-process. Spaced with doubling backoff so a member
        // that is alive-but-unreachable for a network reason we cannot fix
        // costs at most one rebuild per backoff window.
        if want_rebuild {
            let min_gap = if self.transport.backoff == 0 {
                TRANSPORT_REBUILD_MIN_SECS
            } else {
                self.transport.backoff
            };
            if self.transport.last_rebuild == 0 || now - self.transport.last_rebuild >= min_gap {
                self.transport.last_rebuild = now;
                self.transport.backoff = (min_gap * 2).min(TRANSPORT_REBUILD_MAX_SECS);
                self.transport.rebuilds += 1;
                let n = self.transport.rebuilds;
                tracing::warn!(
                    "transport repair rung 2: rebuilding the iroh endpoint in-process \
                     (rebuild #{n}; the daemon-restart equivalent, known-issues #36)"
                );
                match self.rebuild_transport().await {
                    Ok(took) => {
                        self.transport.failures = 0;
                        tracing::info!(
                            "transport rebuilt in {:.1}s: {} share(s) reopened on the same \
                             endpoint id; outbound dials will be re-evaluated",
                            took.as_secs_f32(),
                            self.shares.len()
                        );
                    }
                    Err(e) => {
                        self.transport.failures += 1;
                        tracing::error!(
                            "transport rebuild failed ({} consecutive): {e:#}",
                            self.transport.failures
                        );
                        if self.transport.failures >= TRANSPORT_REBUILD_MAX_FAILURES {
                            self.transport.fatal = Some(format!(
                                "{} consecutive in-process transport rebuilds failed; last: {e:#}",
                                self.transport.failures
                            ));
                        }
                    }
                }
                // Every share was just recreated: the re-kick jobs collected above
                // hold handles into the old node.
                jobs.clear();
                self.force_relay_fallback.store(false, Ordering::Relaxed);
                return jobs;
            }
        }

        // Endpoint-wide: fall back to the public relays while ANY share is provably
        // partitioned, and stop once none is (the watchdog then re-homes on the
        // custom relay). Cleared here rather than left latched so a genuine custom
        // relay reclaims the home slot as soon as connectivity returns.
        self.force_relay_fallback
            .store(any_partitioned, Ordering::Relaxed);
        jobs
    }

    /// Tear down and respawn the whole iroh stack **in-process** — rung 2 of the
    /// transport-repair ladder (known-issues #36) and the exact work a daemon
    /// restart does at the transport layer: same `node.key` (the endpoint id
    /// does not change), same blob store and docs directory, fresh endpoint /
    /// gossip / docs actors, every share reopened from the DB and re-kicked.
    ///
    /// Everything that held a handle into the old node is invalidated: in-flight
    /// downloads and reconcile passes are cancelled first, and any pass that
    /// still completes afterwards is fenced off by the generation bump (see
    /// [`Engine::finish_reconcile`]). Runs under the engine lock; takes a few
    /// seconds. Returns how long it took.
    pub async fn rebuild_transport(&mut self) -> anyhow::Result<Duration> {
        let started = std::time::Instant::now();
        self.cancel_all_downloads();
        self.cancel_all_reconciles();
        self.relay_watchdog.abort();
        // Dropping the share states aborts their presence receive tasks and
        // releases their doc handles; the docs actor itself goes with the node.
        self.shares.clear();
        self.locked.clear();
        self.reclaim_pending.clear();
        if let Ok(mut m) = self.progress.lock() {
            m.clear();
        }
        self.generation += 1;

        let settings = self
            .relay_settings
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        self.node
            .rebuild(
                &self.data_dir,
                &self.blobs_dir,
                &settings,
                Some(self.gc_protect.gc_config()),
            )
            .await
            .context("rebuild iroh node")?;
        self.author = self.node.docs_api().author_default().await?;

        // The watchdog and its flags belong to the endpoint that just died.
        self.relay_fallback.store(false, Ordering::Relaxed);
        self.force_relay_fallback.store(false, Ordering::Relaxed);
        self.relay_watchdog = tokio::spawn(crate::relays::relay_watchdog(
            self.node.endpoint.clone(),
            self.relay_settings.clone(),
            self.relay_fallback.clone(),
            self.force_relay_fallback.clone(),
        ))
        .abort_handle();

        // Give the fresh endpoint a moment to home on a relay so the reopened
        // shares' first dials and the next rendezvous publish carry a full
        // address. Bounded: an offline host must not stall the engine lock.
        let _ = tokio::time::timeout(Duration::from_secs(10), self.node.endpoint.online()).await;
        self.reload_shares().await.context("reopen shares")?;
        self.gc_refresh_job().run().await;
        Ok(started.elapsed())
    }

    /// Set once the transport-repair ladder has given up on in-process rebuilds
    /// ([`TRANSPORT_REBUILD_MAX_FAILURES`] consecutive failures). The daemon
    /// should log it and exit non-zero so its supervisor restarts the process.
    pub fn transport_fatal(&self) -> Option<String> {
        self.transport.fatal.clone()
    }

    /// How many in-process transport rebuilds this engine has performed.
    pub fn transport_rebuilds(&self) -> u32 {
        self.transport.rebuilds
    }

    /// Force a full connectivity re-establish after a system resume.
    ///
    /// Suspend freezes the process and tears down the network underneath it: QUIC
    /// sockets, the home relay, NAT/holepunch mappings, and gossip neighbor
    /// connections all go stale. On an `s2idle` resume iroh's `netwatch` does not
    /// reliably fire (a suspend/resume does not always look like a normal interface
    /// change on Linux), so the endpoint wakes believing dead connections are live
    /// and nothing self-heals — only a restart fixes it. That is fatal for a
    /// frequently-suspending laptop pulling a large file: every suspend kills the
    /// in-flight transfer and the download never converges
    /// (known-issues #21).
    ///
    /// This is that restart, in-process, triggered by the resume edge rather than by
    /// the degradation ladders in [`Engine::connectivity_recoveries`] noticing after
    /// the fact: re-probe the network (rebind the socket + re-home the relay) and
    /// unconditionally rebuild every active share's presence subscription. Returns a
    /// [`DocResync`] per share so the caller re-kicks doc live-sync off-lock, exactly
    /// like the other recovery paths.
    pub async fn on_resume(&mut self) -> Vec<DocResync> {
        // Rebind the magic socket and re-establish the home relay. Bounded so a
        // wedged endpoint can't stall the caller (the reconcile loop). Harmless if
        // the network didn't actually change or iroh already noticed.
        let _ = tokio::time::timeout(
            Duration::from_secs(RESUME_NETWORK_CHANGE_TIMEOUT_SECS),
            self.node.endpoint.network_change(),
        )
        .await;

        let paused_all = self.paused_all();
        let self_id = self.node.endpoint.id();
        let gossip = self.node.gossip.clone();
        let now = now_secs();
        let mut jobs = Vec::new();
        for (id, s) in self.shares.iter_mut() {
            if s.paused || paused_all {
                continue;
            }
            // Unconditional: resume is a known teardown, so don't wait for the
            // isolation / presence-gap ladders to detect it. Reset their episode
            // timers (this rebuild counts as the throttled one) so they don't fire a
            // redundant second rebuild moments later.
            s.heal.isolated = EpisodeClock::default();
            s.heal.isolated_warned = false;
            s.heal.presence_gap = EpisodeClock::default();
            s.heal.last_presence_rebuild = now;
            if rebuild_presence(&gossip, self_id, id, s).await {
                tracing::info!("resume: rebuilt presence subscription for share {id}");
            }
            let peers: Vec<iroh::EndpointAddr> = peer_providers(&s.key, &s.roster)
                .into_iter()
                .filter(|pid| *pid != self_id)
                .map(iroh::EndpointAddr::new)
                .collect();
            if !peers.is_empty() {
                jobs.push(DocResync {
                    share_id: id.clone(),
                    doc: s.doc.clone(),
                    peers,
                });
            }
        }
        jobs
    }

    /// Re-ask the OS keystore for the write key of every share held inert by
    /// [`Engine::locked`], and open any whose key has become available.
    ///
    /// This is what makes the failure recoverable *in place*. The keyring is typically
    /// unlocked at graphical login — seconds to hours after a headless boot — so the
    /// daemon must notice that itself. Without this, the only cure is restarting the
    /// daemon, which nothing about the symptom would ever suggest: "my files aren't
    /// syncing" does not lead anyone to "your keyring was locked when the service
    /// started". A fault that only a maintainer knows how to clear is, in practice,
    /// not recoverable at all.
    ///
    /// Returns a [`DocResync`] per recovered share so the caller starts live-sync off
    /// the engine lock, exactly like [`Engine::add_share_open`].
    pub async fn retry_locked_keys(&mut self) -> Vec<DocResync> {
        if self.locked.is_empty() {
            return Vec::new();
        }
        let now = now_secs();
        let due: Vec<String> = self
            .locked
            .iter()
            .filter(|(_, l)| now - l.last_retry >= KEY_RETRY_SECS)
            .map(|(id, _)| id.clone())
            .collect();

        let mut out = Vec::new();
        for id in due {
            let Some(l) = self.locked.get_mut(&id) else {
                continue;
            };
            l.last_retry = now;
            let seed = match load_seed_bounded(&id).await {
                Ok(seed) => seed,
                Err(e) => {
                    // Still locked. Debug, not warn: on a box whose keyring is never
                    // unlocked this would otherwise log every 30s forever, and the loud
                    // ERROR was already emitted once at startup.
                    tracing::debug!("master seed for {id} still unavailable: {e:#}");
                    continue;
                }
            };

            let locked = self.locked.remove(&id).expect("present: checked above");
            let rec = locked.record;
            let key = match ShareKey::decode(&rec.key) {
                Ok(k) => {
                    let eid = k.endpoint_id().unwrap_or(self.node.endpoint_id_bytes());
                    ShareKey::from_master_seed(seed).with_endpoint_id(eid)
                }
                Err(e) => {
                    tracing::error!(
                        "recovered seed for {id} but its stored key is unreadable: {e}"
                    );
                    continue;
                }
            };

            match self
                .open_share(
                    &key,
                    &PathBuf::from(&rec.folder),
                    vec![],
                    rec.ignore,
                    rec.last_seqno,
                    rec.paused,
                )
                .await
            {
                Ok((mut state, boot)) => {
                    tracing::info!(
                        "master seed for {id} recovered from keystore; share is syncing again"
                    );
                    state.last_quick_sig = rec.quick_sig;
                    let doc = state.doc.clone();
                    self.shares.insert(id.clone(), state);
                    out.push(DocResync {
                        share_id: id,
                        doc,
                        peers: boot,
                    });
                }
                Err(e) => {
                    tracing::error!("recovered seed for {id} but opening the share failed: {e:#}");
                }
            }
        }
        out
    }

    /// Why a share is being held inert (the keystore error), if it is.
    pub fn locked_reason(&self, share_id: &str) -> Option<&str> {
        self.locked.get(share_id).map(|l| l.reason.as_str())
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

    /// Test/diagnostic: how many blob hashes the current GC live set protects, or
    /// `None` if none has been published yet (`Some(0)` = published but empty).
    pub fn debug_gc_live_set_len(&self) -> Option<usize> {
        self.gc_protect.current().map(|s| s.len())
    }

    /// Test/diagnostic: drop one path from the local index (path → last hash we
    /// reconciled to disk) without touching the folder or the replica.
    ///
    /// This is the seam for known-issues #33: it puts the share into the state
    /// where the index lags a file that is *correct on disk*, which is what a pass
    /// that skipped an unreadable mid-write path leaves behind. Reached
    /// organically it is a race; reached this way it is deterministic. Health must
    /// still report 100%, because the repair path (`materialize`) already
    /// considers such a file done and will never queue a fetch for it.
    pub fn debug_forget_index_entry(&mut self, share_id: &str, path: &str) {
        let _ = self.db.del_index_entry(share_id, path);
    }

    /// Test/diagnostic: whether this node can actually **serve** a path's content —
    /// i.e. holds its blob, not merely a correct file on disk. The two come apart,
    /// which is the whole point of [`Self::debug_drop_blob_for`].
    pub async fn debug_can_serve(&self, share_id: &str, path: &str) -> bool {
        let idx = self.db.get_index(share_id).unwrap_or_default();
        let Some(h) = idx.get(path) else {
            return false;
        };
        let Ok(hash) = to_hash(h) else {
            return false;
        };
        self.node.blobs.blobs().has(hash).await.unwrap_or(false)
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
        let generation = job.generation();
        let outcome = job.run().await;
        self.finish_reconcile(&created.share_id, generation, outcome.ok());
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
        let (state, boot) = self
            .open_share(&key, folder, vec![], ignore.clone(), 0, false)
            .await?;
        // A fresh master's bootstrap is empty (its key carries only its own id, which
        // is filtered out), so this dials nothing and returns immediately even under
        // the lock.
        state.doc.start_sync(boot).await.context("start sync")?;
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
        let device_name = self.device_name();
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
        // A fresh pass always starts uncancelled. Clearing here (rather than on
        // every resume path) keeps one invariant: the flag only ever stops the
        // pass that is running *right now*. Both this and every setter run under
        // the engine lock, so a cancel can never be lost to a rebuild race.
        state.cancel.store(false, Ordering::Relaxed);
        let cancel = state.cancel.clone();
        Ok(Some(ReconcileJob {
            share_id: share_id.to_string(),
            folder: state.folder.clone(),
            is_master,
            we_minted: state.we_minted,
            configured_ignore: state.ignore.clone(),
            device_name,
            doc: state.doc.clone(),
            blobs,
            author,
            endpoint,
            providers,
            master_id,
            self_id,
            downloader,
            downloads_inflight,
            generation: self.generation,
            roster: state.roster.clone(),
            prev_skipped: state.skipped.clone(),
            base,
            last_quick_sig: state.last_quick_sig,
            force_scan: state.force_deep_verify,
            progress,
            phase: Arc::new(StdMutex::new("queued".to_string())),
            debug_before_settle: None,
            gc_protect: self.gc_protect.clone(),
            cancel,
            dead_providers: Arc::new(StdMutex::new(HashSet::new())),
        }))
    }

    /// Commit a [`ReconcileJob`] result and clear its busy guard. `outcome` is
    /// `Some` on success (persists the index mutations, health, and signature) and
    /// `None` on failure (just clears the guard).
    ///
    /// `generation` is the job's [`ReconcileJob::generation`]. A job built before
    /// an in-process transport rebuild ran against handles that no longer exist
    /// and against share state that has since been recreated; its outcome (or
    /// its failure) is dropped here rather than committed or allowed to clear
    /// the fresh state's busy guard.
    pub fn finish_reconcile(
        &mut self,
        share_id: &str,
        generation: u64,
        outcome: Option<ReconcileOutcome>,
    ) {
        if let Ok(mut m) = self.progress.lock() {
            m.remove(share_id);
        }
        if generation != self.generation {
            tracing::debug!(
                "reconcile {share_id}: dropping result from generation {generation} \
                 (engine is at {})",
                self.generation
            );
            return;
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
        // Replicated last-known member identities read this pass → remembered
        // names (persisted to `peer_names` on the next presence tick's flush).
        if !out.member_records.is_empty() {
            if let Ok(mut r) = state.roster.lock() {
                r.note_member_records(&out.member_records);
            }
        }
        state.manifest_fp = out.manifest_fp;
        state.last_quick_sig = out.new_quick_sig;
        let _ = self.db.set_quick_sig(share_id, out.new_quick_sig);
        if out.forced_scan {
            // A pending deep verify actually ran and committed: satisfy it. A force
            // set *during* this (unforced) job stays pending for the next one, and a
            // failed job (outcome None above) never clears it.
            state.force_deep_verify = false;
            state.last_deep_verify = now_secs();
        }
        if out.did_full_scan {
            state.full_scans = state.full_scans.saturating_add(1);
        }
        if out.changed {
            state.last_seqno = state.last_seqno.saturating_add(1);
            state.last_updated = now_secs();
            let _ = self.db.set_seqno(share_id, state.last_seqno);
        }

        // Cross-member divergence: compare our manifest fingerprint with those of peers
        // that are **settled**. Two guards, both learned the hard way when three devices
        // joined a fresh share and every one of them cried "members disagree" within a
        // minute, long before the first sync had finished:
        //
        // 1. Only compare *fully-synced* members, and only when we are fully synced
        //    ourselves. A member mid-initial-sync holds a partial manifest by
        //    definition — it is *behind*, not *diverged*. Those are different
        //    conditions with different cures (one waits, one needs a human), and
        //    conflating them makes the alarm meaningless exactly when a share is new.
        //
        // 2. The disagreement must be *stable*, not merely present. While a master is
        //    still importing, its manifest legitimately grows every pass, so peers
        //    trail it — a disagreement that keeps *changing* is propagation in
        //    progress, not divergence. So the settle clock restarts whenever the shape
        //    of the disagreement changes, and only an unchanging disagreement can age
        //    past the window. That is what "persistent" was always supposed to mean.
        let peer_fps = state
            .roster
            .lock()
            .map(|r| r.settled_manifest_fps())
            .unwrap_or_default();
        // `manifest_fp != 0` is the other half of guard 1: our own `health == 100` is
        // vacuously true while our replica is still virgin (an empty manifest is 100%
        // of nothing), so without this a just-joined node would itself cry "members
        // disagree" about the established peers whose real fingerprints differ from our
        // unknown one. We set `manifest_fp = 0` until `replica_seen` for exactly this
        // reason — you cannot judge a fileset you have not synced yet.
        let self_settled = state.health >= 100 && state.manifest_fp != 0;
        let disagrees = self_settled && peer_fps.iter().any(|fp| *fp != state.manifest_fp);
        if disagrees {
            // Fingerprint the disagreement itself (our fp + the peers' fps). While
            // anything is still moving this changes, and the clock restarts.
            let sig = {
                let mut fps: Vec<u64> = peer_fps.clone();
                fps.sort_unstable();
                let mut h = blake3::Hasher::new();
                h.update(b"seed-sync/divergence-sig/v1");
                h.update(&state.manifest_fp.to_le_bytes());
                for fp in &fps {
                    h.update(&fp.to_le_bytes());
                }
                u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().unwrap())
            };
            if state.diverged_since.is_none() || state.divergence_sig != sig {
                state.divergence_sig = sig;
                state.diverged_since = Some(now_secs());
                state.diverged_alerted = false;
                state.diverged_deep_verified = false;
            }
            let elapsed = now_secs() - state.diverged_since.unwrap_or_else(now_secs);
            if elapsed >= DIVERGENCE_SETTLE_SECS {
                if !state.diverged_alerted {
                    state.diverged_alerted = true;
                    tracing::warn!(
                        "share {share_id}: manifest OUT OF SYNC with peer(s) for {elapsed}s \
                         (our fp={:016x}; {} online peer(s) disagree) — members hold different \
                         filesets",
                        state.manifest_fp,
                        peer_fps
                            .iter()
                            .filter(|fp| **fp != state.manifest_fp)
                            .count(),
                    );
                }
                // Self-heal escalation, at most ONCE per divergence episode: normal
                // reconciles keep re-materializing missing blobs every tick and the
                // daemon re-kicks doc live-sync every ~6s, which heals the common
                // propagation-lag divergence with zero rehashes. Only when the
                // disagreement outlives those cheap paths do we force one deep verify
                // (full hashing scan) to re-assert local truth — the old 60s cadence
                // re-hashed multi-GB shares every minute for the whole episode.
                if elapsed >= DIVERGENCE_DEEP_VERIFY_SECS && !state.diverged_deep_verified {
                    state.diverged_deep_verified = true;
                    state.force_deep_verify = true;
                    tracing::info!("share {share_id}: self-heal — forcing one deep verify");
                }
            }
        } else {
            // Back in agreement (or no comparable peers): clear the episode.
            if state.diverged_alerted {
                tracing::info!("share {share_id}: manifest back in sync with peers");
            }
            state.diverged_since = None;
            state.divergence_sig = 0;
            state.diverged_alerted = false;
            state.diverged_deep_verified = false;
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
        let generation = job.generation();
        match job.run().await {
            Ok(o) => {
                let changed = o.changed;
                self.finish_reconcile(share_id, generation, Some(o));
                Ok(changed)
            }
            Err(e) => {
                self.finish_reconcile(share_id, generation, None);
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
    /// Add an existing share from its key, opening the replica and starting live-sync
    /// inline. Convenience wrapper used by tests and the mobile facade. The daemon
    /// instead drives [`add_share_open`] → [`DocResync::start`] so the network dial
    /// runs off the engine lock (see [`DocResync`]).
    ///
    /// [`add_share_open`]: Engine::add_share_open
    pub async fn add_share(
        &mut self,
        key_str: &str,
        folder: &Path,
        bootstrap: Vec<iroh::EndpointAddr>,
    ) -> anyhow::Result<String> {
        let (share_id, sync) = self.add_share_open(key_str, folder, bootstrap).await?;
        sync.start().await?;
        Ok(share_id)
    }

    /// Phase 1 of add: decode the key, open the replica, and persist the share —
    /// WITHOUT starting live-sync. Returns the share id and a [`DocResync`] the caller
    /// runs **off-lock** to kick the network dial. Mirrors [`create_open`]'s two-phase
    /// shape so the daemon never holds the engine mutex across a `start_sync` (which
    /// would stall the reconcile loop and every IPC request on a slow/unreachable
    /// peer — the bug that made a freshly-added share look permanently stuck).
    ///
    /// [`create_open`]: Engine::create_open
    pub async fn add_share_open(
        &mut self,
        key_str: &str,
        folder: &Path,
        bootstrap: Vec<iroh::EndpointAddr>,
    ) -> anyhow::Result<(String, DocResync)> {
        let key = ShareKey::decode(key_str).context("decode share key")?;
        let share_id = key.share_id_hex();
        let (state, boot) = self
            .open_share(&key, folder, bootstrap, vec![], 0, false)
            .await?;
        let doc = state.doc.clone();
        self.shares.insert(share_id.clone(), state);
        self.persist_share(&key, folder, vec![], 0, false).await?;
        Ok((
            share_id.clone(),
            DocResync {
                share_id,
                doc,
                peers: boot,
            },
        ))
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

    /// Stop the reconcile pass running for one share, if any. The pass checks the
    /// flag between files, so it stops within one file's work rather than running
    /// the folder to completion.
    fn cancel_reconcile_for_share(&self, share_id: &str) {
        if let Some(state) = self.shares.get(share_id) {
            state.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Stop every in-flight reconcile pass (global pause / sync-suspend). Without
    /// this, "pause everything" only stopped downloads while the passes kept
    /// merging, materializing and publishing.
    fn cancel_all_reconciles(&self) {
        for state in self.shares.values() {
            state.cancel.store(true, Ordering::Relaxed);
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
            // ...and the pass itself. The gate only blocks the *next* job; a pass
            // already running off-lock owns cloned handles and would otherwise
            // keep writing to the folder for the rest of its walk.
            self.cancel_reconcile_for_share(share_id);
        }
        Ok(())
    }

    /// Remove a share from the engine and persistence. Optionally delete its
    /// local folder contents.
    /// Build a blob-GC live-set refresh (known-issues #22): snapshot the current
    /// replica doc handles + in-flight download hashes under the brief engine lock;
    /// the daemon runs the returned job off-lock to republish the GC live set,
    /// mirroring the build-under-lock / run-off-lock split of
    /// [`Engine::diverged_doc_resyncs`].
    pub fn gc_refresh_job(&self) -> GcRefreshJob {
        let docs = self.shares.values().map(|s| s.doc.clone()).collect();
        let inflight = self
            .downloads_inflight
            .lock()
            .map(|m| m.keys().copied().collect())
            .unwrap_or_default();
        GcRefreshJob {
            docs,
            inflight,
            protect: self.gc_protect.clone(),
        }
    }

    pub async fn remove_share(&mut self, share_id: &str, delete_files: bool) -> anyhow::Result<()> {
        // Order matters: stop the work BEFORE dropping the state, because a running
        // [`ReconcileJob`] is a snapshot holding its own clones of the doc, folder,
        // blob store and downloader. Dropping `ShareState` — or `doc.leave()`ing —
        // does not reach it, so a removed share kept merging, self-healing and
        // writing files into a folder the user had just detached, invisibly: the
        // engine map, the DB, the CLI and the GUI all correctly reported no shares
        // while the pass ran on (known-issues #34).
        self.cancel_downloads_for_share(share_id);
        self.cancel_reconcile_for_share(share_id);
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
        self.relay_watchdog.abort();
        self.node.shutdown().await
    }
}

/// Subscribe to a doc's live events in a background task. This keeps the
/// live-sync session active and feeds the peer roster (neighbor up/down, remote
/// inserts, sync completions).
async fn spawn_event_task(
    doc: &Doc,
    roster: Arc<StdMutex<PeerRoster>>,
    self_id: EndpointId,
) -> anyhow::Result<()> {
    let mut events = doc.subscribe().await?;
    let self_id = self_id.to_string();
    tokio::spawn(async move {
        while let Some(ev) = events.next().await {
            if let Ok(e) = &ev {
                // Never record OURSELVES as a peer. The roster is the member list, and
                // this device is counted separately (`Engine::peers` leads with "This
                // device", and the counts add one for it) — so a self entry here shows
                // up as a phantom extra member that no other node can see.
                //
                // It is reachable: `SyncFinished` fires on *failed* syncs too, so any
                // path that dials our own endpoint id notes us as a peer. Guarding at
                // the roster kills the whole class, not just the one caller that did it.
                let peer = match e {
                    LiveEvent::NeighborUp(pk) | LiveEvent::NeighborDown(pk) => pk.to_string(),
                    LiveEvent::InsertRemote { from, .. } => from.to_string(),
                    LiveEvent::SyncFinished(se) => se.peer.to_string(),
                    _ => String::new(),
                };
                if !peer.is_empty() && peer != self_id {
                    if let Ok(mut r) = roster.lock() {
                        match e {
                            LiveEvent::NeighborUp(_) => r.note(&peer, Some(true)),
                            LiveEvent::NeighborDown(_) => r.note(&peer, Some(false)),
                            // A sync event fires on BOTH success and failure. Only a
                            // success proves the peer is reachable; a failed dial must
                            // not refresh liveness, or a fully-partitioned node marks
                            // every peer online for the TTL on each retry and the fleet
                            // flaps "Syncing ↔ offline" while nothing actually connects
                            // (known-issues #23, #16).
                            LiveEvent::SyncFinished(se) => {
                                let outbound = matches!(se.origin, Origin::Connect(_));
                                match &se.result {
                                    Ok(_) => r.note_sync_finished(&peer, outbound, true, None),
                                    Err(err) => r.note_sync_finished(
                                        &peer,
                                        outbound,
                                        false,
                                        Some(err.as_str()),
                                    ),
                                }
                            }
                            _ => r.note(&peer, None),
                        }
                    }
                    // A failed sync is invisible today (it returns Ok from start_sync
                    // and only surfaces as this event); log it so a silent partition
                    // is diagnosable. Debug-level: it fires on every retry.
                    if let LiveEvent::SyncFinished(se) = e {
                        if let Err(err) = &se.result {
                            let dir = match se.origin {
                                Origin::Connect(_) => "outbound",
                                Origin::Accept => "inbound",
                            };
                            tracing::debug!("{dir} doc sync with {peer} failed: {err}");
                        }
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

/// Remove directories that a delete just emptied, walking upward from each toward
/// (but never including or past) `root`. `remove_dir` only succeeds on a truly
/// empty directory, so a folder that still holds other files — or an empty folder
/// the user created that isn't in `dirs` — is left untouched. This is deliberately
/// scoped to the parents of files removed this tick: a blanket sweep of every empty
/// directory under `root` would delete user-created empty folders (they have no
/// manifest representation), which is exactly the "new folders vanish" bug.
fn prune_emptied_ancestors(dirs: &HashSet<PathBuf>, root: &Path) {
    for start in dirs {
        let mut dir = start.as_path();
        // Only touch paths strictly under the share root.
        while dir != root && dir.starts_with(root) {
            // Stop as soon as a directory isn't empty (or is gone / unreadable):
            // remove_dir fails harmlessly and there's nothing above worth trying.
            if std::fs::remove_dir(dir).is_err() {
                break;
            }
            match dir.parent() {
                Some(parent) => dir = parent,
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Endpoint ids are ed25519 public keys, so derive valid ones from signing
    /// keys rather than arbitrary bytes (which aren't on-curve).
    fn eid(seed: u8) -> EndpointId {
        let bytes = ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
            .verifying_key()
            .to_bytes();
        EndpointId::from_bytes(&bytes).unwrap()
    }

    /// A share that *flaps* — one stray presence beat marking the peer online
    /// for a TTL every ~30s — must not reset the self-heal episode clocks: that
    /// blind spot left the 2026-08 two-member outage flapping for over an hour
    /// with every ladder disarmed (known-issues #35). A healthy blip shorter
    /// than [`HEAL_CLEAR_SECS`] keeps the episode alive, and elapsed accrues
    /// from the episode's original start.
    #[test]
    fn heal_episode_survives_flap() {
        let mut c = EpisodeClock::default();
        let t0 = 1_000_000;
        assert_eq!(c.observe(true, t0), Some(0));
        // 25s-period flap: 20s faulty, then a 5s healthy blip, repeated.
        let mut now = t0;
        for _ in 0..20 {
            now += 20;
            assert!(c.observe(true, now).is_some());
            now += 5;
            assert_eq!(
                c.observe(false, now),
                Some(now - t0),
                "a sub-hysteresis blip must not clear the episode"
            );
        }
        // The flap ran long enough that every ladder threshold has been crossed.
        assert!(now - t0 > ISOLATION_HEAL_SECS + ISOLATION_PRESENCE_REBUILD_SECS);
    }

    /// The flip side of the hysteresis: sustained health ends the episode, stays
    /// clear, and a later fault starts a *fresh* episode from its own start.
    #[test]
    fn heal_episode_clears_only_after_sustained_health() {
        let mut c = EpisodeClock::default();
        let t0 = 1_000_000;
        c.observe(true, t0);
        assert!(c.observe(false, t0 + 10).is_some());
        assert!(c.observe(false, t0 + 10 + HEAL_CLEAR_SECS - 1).is_some());
        assert_eq!(c.observe(false, t0 + 10 + HEAL_CLEAR_SECS), None);
        assert_eq!(c.observe(false, t0 + 500), None);
        assert_eq!(c.observe(true, t0 + 600), Some(0));
    }

    /// A share that never faults never has an episode.
    #[test]
    fn heal_episode_noop_while_healthy() {
        let mut c = EpisodeClock::default();
        assert_eq!(c.observe(false, 5), None);
        assert_eq!(c.observe(false, 500), None);
    }

    /// Mesh-repair target selection (known-issues #9): dial only peers we can't
    /// currently hear, never more than [`PRESENCE_REJOIN_SAMPLE`] of them, and
    /// nothing at all once the roster hears everyone — the old full-set join
    /// every tick evicted gossip's bounded active view at fleet scale and
    /// fragmented the overlay.
    #[test]
    fn rejoin_targets_sample_unheard_only() {
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let all: Vec<EndpointId> = (1u8..=28).map(eid).collect();

        // Everyone heard → converged mesh, no repair traffic at all.
        let heard_all: HashSet<String> = all.iter().map(|id| id.to_string()).collect();
        assert!(select_rejoin_targets(&mut rng, all.clone(), &heard_all).is_empty());

        // Nobody heard (cold start / full partition) → a capped random sample,
        // not the full set.
        let heard_none = HashSet::new();
        let picked = select_rejoin_targets(&mut rng, all.clone(), &heard_none);
        assert_eq!(picked.len(), PRESENCE_REJOIN_SAMPLE);

        // Partially heard → targets drawn only from the unheard remainder.
        let heard: HashSet<String> = all[..24].iter().map(|id| id.to_string()).collect();
        let picked = select_rejoin_targets(&mut rng, all.clone(), &heard);
        assert_eq!(picked.len(), PRESENCE_REJOIN_SAMPLE);
        for id in &picked {
            assert!(
                !heard.contains(&id.to_string()),
                "must never dial a peer we already hear"
            );
        }

        // Fewer unheard than the cap → all of them, never padded with heard peers.
        let heard: HashSet<String> = all[..27].iter().map(|id| id.to_string()).collect();
        assert_eq!(
            select_rejoin_targets(&mut rng, all.clone(), &heard),
            vec![all[27]]
        );
    }

    /// Acceptance test for the phantom-liveness fix
    /// (known-issues #23): a **failed** doc-sync must produce
    /// zero roster online-flaps. Before the fix, a fully-partitioned node counted
    /// every failed retry as contact, marking peers "online" for the 20s TTL and
    /// flapping the whole fleet "Syncing ↔ offline" while nothing connected.
    #[test]
    fn failed_sync_never_marks_a_peer_online() {
        let mut r = PeerRoster::default();
        let peer = eid(7).to_string();

        // A failed dial: not contact. No online peer, no advanced last-contact, but
        // the error is recorded for diagnostics.
        r.note_sync_finished(&peer, true, false, Some("connection timed out"));
        assert_eq!(r.counts().0, 0, "a failed sync must not mark a peer online");
        assert_eq!(r.last_contact(), 0, "a failed sync is not genuine contact");
        assert!(
            r.last_sync_err().is_some(),
            "the failure is kept for diagnostics"
        );

        // Repeated failures (the retry loop) never flip it online either.
        for _ in 0..5 {
            r.note_sync_finished(&peer, true, false, Some("connection timed out"));
        }
        assert_eq!(
            r.counts().0,
            0,
            "repeated failed syncs must not flap online"
        );

        // A *successful* sync IS contact and marks the peer online.
        r.note_sync_finished(&peer, true, true, None);
        assert_eq!(r.counts().0, 1, "a successful sync marks the peer online");
        assert!(r.last_contact() > 0, "success advances last-contact");
    }

    /// Ladder-3 trigger (known-issues #36): only "members provably alive AND our
    /// outbound dials failing repeatedly" counts; a dead fleet, a single timeout,
    /// or a recent outbound success never does.
    #[test]
    fn outbound_dead_needs_alive_members_and_repeated_dial_failures() {
        let now = 10_000;
        let failing = DialStats {
            last_outbound_ok: now - 900,
            last_outbound_err: Some(("timed out".into(), now - 5)),
            outbound_failures: 6,
            last_inbound_ok: now - 30,
            rendezvous_alive: 0,
        };
        assert!(
            outbound_dead(1, 0, &failing, now),
            "inbound syncs land while every outbound dial fails: the wedge signature"
        );
        assert!(
            !outbound_dead(0, 0, &failing, now),
            "no known members: nothing to repair"
        );

        // Nobody alive: a fleet that is simply off must not churn the endpoint.
        let nobody = DialStats {
            last_inbound_ok: 0,
            ..failing.clone()
        };
        assert!(!outbound_dead(1, 0, &nobody, now));
        assert!(
            !outbound_dead(1, now - PEER_ALIVE_WINDOW_SECS - 1, &nobody, now),
            "contact older than the alive window does not count"
        );
        assert!(
            outbound_dead(1, now - 10, &nobody, now),
            "any genuine contact within the window counts as alive"
        );
        assert!(
            outbound_dead(
                1,
                0,
                &DialStats {
                    rendezvous_alive: now - 60,
                    ..nobody.clone()
                },
                now
            ),
            "a fresh rendezvous record from another master counts as alive"
        );

        // One timeout is weather; a recent outbound success clears it.
        assert!(!outbound_dead(
            1,
            0,
            &DialStats {
                outbound_failures: 1,
                ..failing.clone()
            },
            now
        ));
        assert!(!outbound_dead(
            1,
            0,
            &DialStats {
                last_outbound_ok: now - 2,
                ..failing.clone()
            },
            now
        ));
        assert!(!outbound_dead(1, now - 10, &DialStats::default(), now));
    }

    /// Ladder-2 trigger (known-issues #23): the presence
    /// overlay is judged dead only when transport is alive but presence is silent —
    /// the exact "doc-sync works, seqno stuck at 0, members flap" shape — and never
    /// on a healthy share, a totally-isolated one, or a solo/empty one.
    #[test]
    fn presence_overlay_dead_only_when_transport_alive_and_presence_silent() {
        let now = 10_000;
        // Transport fresh (5s ago), no presence ever heard, members known → dead.
        assert!(presence_overlay_dead(2, false, now - 5, 0, now));
        // Transport fresh, presence heard 30s ago (> TTL) → dead.
        assert!(presence_overlay_dead(2, false, now - 5, now - 30, now));

        // Healthy: presence heard 3s ago → not dead.
        assert!(!presence_overlay_dead(2, false, now - 3, now - 3, now));
        // Totally isolated (ladder 1 owns it) → not ladder 2.
        assert!(!presence_overlay_dead(2, true, now - 5, 0, now));
        // Solo / nobody known → nothing to hear, not a fault.
        assert!(!presence_overlay_dead(0, false, now - 5, 0, now));
        // Transport itself stale (no recent contact) → isolation territory, not this.
        assert!(!presence_overlay_dead(2, false, now - 300, 0, now));
    }

    fn presence(name: &str, role: seed_ipc::Role) -> crate::presence::Presence {
        crate::presence::Presence {
            v: crate::presence::PRESENCE_V,
            name: name.into(),
            role,
            seqno: 0,
            percent: 100,
            ts: 0,
            from: None,
            manifest_fp: 0,
        }
    }

    /// Divergence detection must ignore members that are still catching up.
    ///
    /// A peer mid-initial-sync holds a partial manifest by definition, so its
    /// fingerprint disagrees with everyone's — and comparing against it turned a
    /// perfectly normal join into a fleet-wide "members disagree" alarm within a
    /// minute, on every node, before the first sync had even finished. A member that
    /// is *behind* is not a member that has *diverged*: the first fixes itself, the
    /// second needs a human, and an alarm that cannot tell them apart is noise.
    #[test]
    fn divergence_ignores_peers_that_are_still_syncing() {
        let syncing = eid(1).to_string();
        let settled = eid(2).to_string();
        let mut roster = PeerRoster::default();

        let mut p = presence("Joining", seed_ipc::Role::Master);
        p.percent = 42; // still pulling content down
        p.manifest_fp = 0xAAAA; // partial view of the share
        roster.note_presence(&syncing, p);

        let mut q = presence("Settled", seed_ipc::Role::Master);
        q.percent = 100;
        q.manifest_fp = 0xBBBB;
        roster.note_presence(&settled, q);

        // The Healthy gate still sees everyone online, including the joiner.
        let online = roster.online_manifest_fps();
        assert_eq!(
            online.len(),
            2,
            "both peers are online and advertise a manifest"
        );

        // Divergence only ever compares the settled one.
        let fps = roster.settled_manifest_fps();
        assert_eq!(
            fps,
            vec![0xBBBB],
            "a peer that is still syncing must not be compared for divergence — it is \
             behind, not diverged, and treating it as diverged makes every new share \
             cry OutOfSync while it is still joining"
        );
    }

    /// Known-issues #19, the roster half: a just-joined member whose replica is still
    /// virgin reports `percent == 100` (an empty manifest is 100% of nothing) but,
    /// post-fix, advertises `manifest_fp == 0` (unknown). Such a peer must be excluded
    /// from the divergence comparison exactly as a still-downloading one is — otherwise
    /// its empty manifest reads as a settled member that disagrees, which is what
    /// tripped a false OutOfSync the moment a member was added.
    #[test]
    fn divergence_ignores_a_virgin_peer_reporting_full_health() {
        let virgin = eid(1).to_string();
        let settled = eid(2).to_string();
        let mut roster = PeerRoster::default();

        let mut p = presence("Joining", seed_ipc::Role::Master);
        p.percent = 100; // empty manifest ⇒ health 100 (100% of nothing)
        p.manifest_fp = 0; // but nothing synced yet ⇒ the "unknown" sentinel
        roster.note_presence(&virgin, p);

        let mut q = presence("Settled", seed_ipc::Role::Master);
        q.percent = 100;
        q.manifest_fp = 0xBBBB;
        roster.note_presence(&settled, q);

        assert_eq!(
            roster.settled_manifest_fps(),
            vec![0xBBBB],
            "a virgin peer at 100% with an unknown (0) fingerprint must not be compared \
             for divergence — its empty manifest is not a settled fileset, and treating \
             it as one is what made every fresh join cry OutOfSync"
        );
    }

    #[test]
    fn member_record_key_roundtrips() {
        let rec = MemberRecord {
            v: MEMBER_RECORD_V,
            id: *eid(7).as_bytes(),
            name: "Laptop".into(),
            master: true,
        };
        let key = member_record_key(&rec);
        assert!(key.starts_with(MEMBER_PREFIX));
        assert_eq!(decode_member_record(&key), Some(rec.clone()));
        // Identical identity → identical key, so republishing is idempotent.
        assert_eq!(key, member_record_key(&rec));
        // Non-member control keys and garbage tails are skipped, not errors.
        assert_eq!(decode_member_record(b"\x00t/some/file"), None);
        assert_eq!(decode_member_record(b"\x00m/not-cbor\xff"), None);
    }

    /// The core of the feature: a member's name survives its disconnect (entry
    /// ages out but keeps the name), and — via the drain/preload cycle that the
    /// `peer_names` table persists — a daemon restart, where it renders as a
    /// named offline row instead of vanishing or degrading to an endpoint id.
    #[test]
    fn roster_remembers_identity_across_disconnect_and_restart() {
        let id = eid(1).to_string();
        let mut roster = PeerRoster::default();
        roster.note_presence(&id, presence("Laptop", seed_ipc::Role::Master));
        let infos = roster.infos();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name.as_deref(), Some("Laptop"));
        assert!(infos[0].online);

        // Disconnect (NeighborDown force-ages the entry): offline, still named.
        roster.note(&id, Some(false));
        let infos = roster.infos();
        assert!(!infos[0].online);
        assert_eq!(infos[0].name.as_deref(), Some("Laptop"));

        // "Restart": a fresh roster preloaded from the drained rows lists the
        // member offline under its last-known identity, and counts it.
        let rows = roster.drain_dirty_names("share1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Laptop");
        assert!(rows[0].role_master);
        assert_eq!(rows[0].share_id, "share1");
        // Drained means drained: nothing left to flush.
        assert!(roster.drain_dirty_names("share1").is_empty());

        let mut fresh = PeerRoster::default();
        fresh.preload_remembered(rows);
        let infos = fresh.infos();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name.as_deref(), Some("Laptop"));
        assert!(matches!(infos[0].role, seed_ipc::Role::Master));
        assert!(!infos[0].online);
        assert_eq!(fresh.counts(), (0, 1));
        // Preloaded identities are not dirty (they just came from the DB).
        assert!(fresh.drain_dirty_names("share1").is_empty());

        // The member comes back online: the live entry takes over seamlessly
        // (one row, not a duplicate).
        fresh.note_presence(&id, presence("Laptop", seed_ipc::Role::Master));
        let infos = fresh.infos();
        assert_eq!(infos.len(), 1);
        assert!(infos[0].online);
        assert_eq!(fresh.counts(), (1, 1));
    }

    /// A peer discovered via doc-sync only (no presence heard yet — the
    /// asymmetric-gossip case) must fall back to its remembered identity
    /// instead of showing a bare endpoint id.
    #[test]
    fn doc_discovered_peer_falls_back_to_remembered_name() {
        let id = eid(2).to_string();
        let mut roster = PeerRoster::default();
        roster.preload_remembered(vec![crate::db::PeerNameRow {
            share_id: "s".into(),
            node_id: id.clone(),
            name: "NAS".into(),
            role_master: false,
            last_seen: 50,
            updated: 50,
        }]);
        // Doc live event (InsertRemote): liveness without identity.
        roster.note(&id, None);
        let infos = roster.infos();
        assert_eq!(infos.len(), 1);
        assert!(infos[0].online);
        assert_eq!(infos[0].name.as_deref(), Some("NAS"));
        assert!(matches!(infos[0].role, seed_ipc::Role::Viewer));
    }

    /// Doc member-records fill identities we lack, but never override fresher
    /// first-hand knowledge (presence advances `updated` every beat).
    #[test]
    fn member_records_fill_but_never_override_fresher_knowledge() {
        let id = eid(3).to_string();
        let mut roster = PeerRoster::default();

        // Unknown member: the doc record names it.
        roster.note_member_records(&[MemberIdentity {
            id: id.clone(),
            name: "Desktop".into(),
            master: true,
            ts_secs: 100,
        }]);
        let infos = roster.infos();
        assert_eq!(infos[0].name.as_deref(), Some("Desktop"));
        assert_eq!(infos[0].last_seen, 100);

        // A record older than what we already applied is ignored.
        roster.note_member_records(&[MemberIdentity {
            id: id.clone(),
            name: "Stale".into(),
            master: false,
            ts_secs: 90,
        }]);
        assert_eq!(roster.infos()[0].name.as_deref(), Some("Desktop"));

        // Direct presence beats any doc record written before it...
        roster.note_presence(&id, presence("Desktop (live)", seed_ipc::Role::Master));
        roster.note_member_records(&[MemberIdentity {
            id: id.clone(),
            name: "Desktop".into(),
            master: true,
            ts_secs: now_secs() - 1,
        }]);
        assert_eq!(roster.infos()[0].name.as_deref(), Some("Desktop (live)"));

        // ...and the live name also wins the display merge outright while the
        // peer is in the session roster.
        assert!(roster.infos()[0].online);
    }

    /// Masters publish only members they hear *online right now* — republishing
    /// an offline member's name with a fresh LWW timestamp could beat a
    /// better-informed master's record.
    #[test]
    fn online_named_peers_excludes_offline_and_nameless() {
        let named = eid(4).to_string();
        let aged = eid(5).to_string();
        let nameless = eid(6).to_string();
        let mut roster = PeerRoster::default();
        roster.note_presence(&named, presence("A", seed_ipc::Role::Viewer));
        roster.note_presence(&aged, presence("B", seed_ipc::Role::Viewer));
        roster.note(&aged, Some(false)); // NeighborDown: force-aged offline
        roster.note(&nameless, None); // doc-sync discovery, no identity
        let out = roster.online_named_peers();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], (named, "A".to_string(), false));
    }

    fn re(hash: &[u8], size: u64) -> RemoteEntry {
        RemoteEntry {
            hash: hash.to_vec(),
            size,
            ts: 0,
        }
    }

    /// The fingerprint is the whole basis of divergence detection: it must be equal
    /// exactly when two members agree on the fileset (path → content-hash), and it
    /// must be insensitive to map order and to LWW timestamps. A wrong fingerprint
    /// here would mean either silent missed divergence or constant false alarms.
    #[test]
    fn manifest_fingerprint_matches_iff_filesets_match() {
        let mut a = HashMap::new();
        a.insert("docs/x.txt".to_string(), re(&[1u8; 32], 10));
        a.insert("y.bin".to_string(), re(&[2u8; 32], 20));

        // Same entries, different insertion order, different ts → same fingerprint.
        let mut b = HashMap::new();
        b.insert("y.bin".to_string(), {
            let mut e = re(&[2u8; 32], 20);
            e.ts = 9999;
            e
        });
        b.insert("docs/x.txt".to_string(), re(&[1u8; 32], 10));
        assert_eq!(
            manifest_fingerprint(&a),
            manifest_fingerprint(&b),
            "agreeing manifests must fingerprint equal (order/ts-insensitive)"
        );

        // Different content hash for a path → different fingerprint.
        let mut c = HashMap::new();
        c.insert("docs/x.txt".to_string(), re(&[9u8; 32], 10));
        c.insert("y.bin".to_string(), re(&[2u8; 32], 20));
        assert_ne!(
            manifest_fingerprint(&a),
            manifest_fingerprint(&c),
            "a changed content hash must change the fingerprint"
        );

        // Different fileset (a missing file) → different fingerprint.
        let mut d = HashMap::new();
        d.insert("docs/x.txt".to_string(), re(&[1u8; 32], 10));
        assert_ne!(
            manifest_fingerprint(&a),
            manifest_fingerprint(&d),
            "a different fileset must change the fingerprint"
        );

        // Never the 0 (= "unknown") sentinel, even for an empty view.
        let empty: HashMap<String, RemoteEntry> = HashMap::new();
        assert_ne!(manifest_fingerprint(&empty), 0);
    }

    /// Known-issues #19: a node must not *advertise* a fingerprint until its replica
    /// has proven contact with the share. A virgin replica is empty, and an empty
    /// manifest fingerprints to a valid nonzero value while reporting health 100 — so
    /// broadcasting it made every settled peer count a just-joined member as a
    /// fully-synced disagreement and cry OutOfSync the instant it joined. The gate
    /// makes a virgin node advertise the `0` "unknown" sentinel that the divergence
    /// comparison already excludes; an *established* empty share (replica seen) still
    /// advertises the real empty fingerprint so two empty masters converge to Healthy.
    /// known-issues #33: the protected set the GC sweep reads is a *snapshot*, and
    /// the sweep does not wait for it to be refreshed.
    ///
    /// The daemon recomputes it from the replicas every ~120 s; the store's sweep
    /// fires on its own hourly timer. A blob imported or downloaded in between is
    /// referenced by the replica but absent from the snapshot, so the sweep deleted
    /// it. That is not theoretical: a 28-node fleet soak lost the blobs of 18
    /// currently-referenced files on **28 of 28 nodes** at the first sweep — every
    /// file still byte-perfect on disk, and no member left able to serve the
    /// content. Nothing re-imports a file whose bytes are already correct, so it
    /// never came back.
    ///
    /// Exercised through the real `add_protected` callback, because the wiring is
    /// the part that was wrong.
    #[tokio::test]
    async fn gc_protects_blobs_stored_since_the_last_live_set_refresh() {
        let protect = GcProtect::default();
        let refreshed = Hash::from([1u8; 32]);
        let stored_after = Hash::from([2u8; 32]);

        // A refresh happened, and saw only what the replica held at that moment…
        protect.publish(HashSet::from([refreshed]));
        // …then this node stored another blob, as a churning share does constantly.
        protect.note_added(stored_after);

        let cfg = protect.gc_config();
        let cb = cfg
            .add_protected
            .expect("a protect callback is always installed");
        let mut live: HashSet<Hash> = HashSet::new();
        assert!(matches!(cb(&mut live).await, ProtectOutcome::Continue));

        assert!(
            live.contains(&refreshed),
            "the published live set must be protected"
        );
        assert!(
            live.contains(&stored_after),
            "a blob stored since the last refresh must survive the next sweep — it is \
             referenced content, and the snapshot is simply too old to know it"
        );
    }

    /// Fail-closed: with no live set ever published, a sweep must be aborted rather
    /// than run against an unknown protected set (which would delete everything).
    #[tokio::test]
    async fn gc_aborts_when_no_live_set_has_been_published() {
        let protect = GcProtect::default();
        protect.note_added(Hash::from([9u8; 32]));
        let cfg = protect.gc_config();
        let cb = cfg
            .add_protected
            .expect("a protect callback is always installed");
        let mut live: HashSet<Hash> = HashSet::new();
        assert!(
            matches!(cb(&mut live).await, ProtectOutcome::Abort),
            "without a published live set the sweep must abort, not proceed on the \
             recently-added hashes alone"
        );
    }

    #[test]
    fn advertised_fp_is_zero_until_replica_seen() {
        let mut populated = HashMap::new();
        populated.insert("docs/x.txt".to_string(), re(&[1u8; 32], 10));
        let empty: HashMap<String, RemoteEntry> = HashMap::new();

        // Virgin replica (doc-sync still in flight): advertise "unknown", never a
        // fingerprint of the empty set — whatever `remote` happens to hold.
        assert_eq!(
            advertised_fp(false, &empty),
            0,
            "a virgin replica must advertise the 0 (unknown) sentinel, not FP_EMPTY, \
             so settled peers treat it as behind rather than diverged"
        );
        assert_eq!(advertised_fp(false, &populated), 0);

        // Replica seen: advertise the real fingerprint. Crucially the empty-but-seen
        // case (a genuinely empty, established share) advertises the *real* nonzero
        // empty fingerprint — two such masters agree on it and stay Healthy.
        assert_eq!(advertised_fp(true, &empty), manifest_fingerprint(&empty));
        assert_ne!(advertised_fp(true, &empty), 0);
        assert_eq!(
            advertised_fp(true, &populated),
            manifest_fingerprint(&populated)
        );
    }

    /// Known-issues #14: the replicated ignore list rides the doc *key*
    /// (`\x00i/` + CBOR), not a value blob, so it must survive an exact
    /// encode→decode round-trip and reject foreign/legacy keys. A non-ignore
    /// control key (e.g. the old `\x00ignore` value-blob form, or a member
    /// record) must decode to `None` so the prefix reader skips it.
    #[test]
    fn ignore_list_key_roundtrips_and_rejects_foreign_keys() {
        for list in [
            vec![],
            vec!["*.tmp".to_string()],
            vec!["a".to_string(), "b/c".to_string(), "d e".to_string()],
        ] {
            let key = ignore_list_key(&list);
            assert!(key.starts_with(IGNORE_PREFIX), "key must carry the prefix");
            assert_eq!(
                decode_ignore_list(&key),
                Some(list.clone()),
                "list must survive the key round-trip verbatim"
            );
        }

        // Equal lists → equal keys (republish is idempotent, no LWW churn).
        let l = vec!["x".to_string(), "y".to_string()];
        assert_eq!(ignore_list_key(&l), ignore_list_key(&l.clone()));

        // Foreign / legacy control keys are not ignore keys.
        assert_eq!(decode_ignore_list(b"\x00ignore"), None);
        assert_eq!(decode_ignore_list(b"\x00m/whatever"), None);
        assert_eq!(decode_ignore_list(b"some/user/path"), None);
        // Right prefix but undecodable CBOR tail → None (forward-compat: skip).
        assert_eq!(decode_ignore_list(b"\x00i/\xff\xff\xff"), None);
    }

    /// Known-issues #13: a master must warn (not silently propagate) when a
    /// *forced* deep verify surfaces a content change while the folder's
    /// (path,size,mtime) signature held steady — the fingerprint of in-place
    /// corruption. The WARN must NOT fire for ordinary edits: a signature-driven
    /// scan (metadata moved) or a forced verify whose signature also moved is a
    /// legitimate change that ordinary detection already explains.
    #[test]
    fn silent_corruption_scan_only_on_forced_verify_with_steady_signature() {
        // Corruption signature: forced verify, unchanged signature.
        assert!(is_silent_corruption_scan(true, 42, 42));
        // Not forced (signature drove the scan) → a real edit, not corruption.
        assert!(!is_silent_corruption_scan(false, 7, 7));
        assert!(!is_silent_corruption_scan(false, 8, 7));
        // Forced, but the signature ALSO moved → metadata changed, so a surfaced
        // hash change is an ordinary edit, not the silent-corruption signal.
        assert!(!is_silent_corruption_scan(true, 8, 7));
    }

    /// Delete-vs-content resolution (known-issues #12): a tombstone deletes a
    /// path from the merged view only when strictly newer than the live entry;
    /// content wins ties (anti-data-loss bias, matching the empty-marker
    /// tie-break). Surviving tombstones stay available for the merge's
    /// local-file LWW; losing ones are dropped. Map-based, so stream order
    /// can't influence the outcome (fingerprint determinism).
    #[test]
    fn tombstones_resolve_by_lww_content_wins_ties() {
        let entry = |ts: u64| RemoteEntry {
            hash: vec![7u8; 32],
            size: 10,
            ts,
        };

        // Tombstone newer than content → path deleted, tombstone kept.
        let mut files = HashMap::from([("p".to_string(), entry(100))]);
        let mut tombs = HashMap::from([("p".to_string(), 200u64)]);
        resolve_tombstones(&mut files, &mut tombs);
        assert!(!files.contains_key("p"), "newer delete must win");
        assert_eq!(tombs.get("p"), Some(&200));

        // Content newer than tombstone → content survives, tombstone dropped.
        let mut files = HashMap::from([("p".to_string(), entry(300))]);
        let mut tombs = HashMap::from([("p".to_string(), 200u64)]);
        resolve_tombstones(&mut files, &mut tombs);
        assert_eq!(files["p"].ts, 300, "newer content must survive");
        assert!(tombs.is_empty());

        // Tie → content wins (deterministic anti-data-loss bias).
        let mut files = HashMap::from([("p".to_string(), entry(200))]);
        let mut tombs = HashMap::from([("p".to_string(), 200u64)]);
        resolve_tombstones(&mut files, &mut tombs);
        assert!(files.contains_key("p"));
        assert!(tombs.is_empty());

        // Tombstone for a path with no live entry (the #12 "never saw it"
        // case) → kept, so the merge can veto a local re-publish.
        let mut files = HashMap::new();
        let mut tombs = HashMap::from([("gone".to_string(), 50u64)]);
        resolve_tombstones(&mut files, &mut tombs);
        assert_eq!(tombs.get("gone"), Some(&50));
    }

    /// The self-heal staging path appends `.seedheal-tmp` to the full file name
    /// rather than replacing the extension, so siblings sharing a stem get
    /// distinct temps and the temp can't shadow a real file.
    #[test]
    fn heal_tmp_path_appends_and_is_unique() {
        use std::path::Path;
        let bin = heal_tmp_path(Path::new("/share/a.bin"));
        let txt = heal_tmp_path(Path::new("/share/a.txt"));
        assert_eq!(bin.file_name().unwrap(), "a.bin.seedheal-tmp");
        assert_eq!(txt.file_name().unwrap(), "a.txt.seedheal-tmp");
        assert_ne!(bin, txt, "siblings sharing a stem must not collide");
        // Extensionless names get the suffix too, staying in the same dir.
        let noext = heal_tmp_path(Path::new("/share/README"));
        assert_eq!(noext.file_name().unwrap(), "README.seedheal-tmp");
        assert_eq!(noext.parent(), Path::new("/share/README").parent());
    }

    /// A tombstone-vs-local-file decision (the reconcile arm that bit the
    /// "deleted ISO, pasted a new one, it kept vanishing" bug). Different
    /// content at the deleted name must publish even with a stale (older) mtime;
    /// only the *exact deleted content* still on disk is suppressed.
    #[test]
    fn tombstone_only_suppresses_the_same_deleted_content() {
        let deleted = [7u8; 32];
        let replacement = [9u8; 32]; // a genuinely different file at the same name

        // The reported bug: re-added file has DIFFERENT content but an mtime
        // OLDER than the delete (copy/extract/download preserved it). It must
        // still publish — never be suppressed.
        assert!(
            !tombstone_suppresses(Some(&deleted), &replacement, 100, 200),
            "different content with a stale mtime is a real re-add — must publish"
        );
        // Different content with a fresh mtime: also publishes.
        assert!(!tombstone_suppresses(
            Some(&deleted),
            &replacement,
            300,
            200
        ));

        // The race the tombstone exists for: the *exact* deleted file still on
        // disk, not newer than the delete → suppress it.
        assert!(
            tombstone_suppresses(Some(&deleted), &deleted, 100, 200),
            "the same deleted content lingering (older) must be removed"
        );
        // Same content but re-created strictly after the delete → keep (publish).
        assert!(!tombstone_suppresses(Some(&deleted), &deleted, 300, 200));

        // Legacy tombstone (no stored hash) falls back to the time-only rule:
        // older → suppress, newer → publish. Preserves prior behavior until the
        // hash is known.
        assert!(tombstone_suppresses(None, &replacement, 100, 200));
        assert!(!tombstone_suppresses(None, &replacement, 300, 200));
    }

    /// Cross-master empty↔non-empty flip: when both the content key and the
    /// empty-marker key are live for one path, the merged view must keep the
    /// newer record — and resolve identically regardless of stream order, or two
    /// members with the same doc would fingerprint differently (false OutOfSync).
    #[test]
    fn remote_lww_resolves_empty_vs_content_by_timestamp() {
        let content = |ts: u64| RemoteEntry {
            hash: vec![7u8; 32],
            size: 10,
            ts,
        };
        let empty = |ts: u64| RemoteEntry {
            hash: Hash::EMPTY.as_bytes().to_vec(),
            size: 0,
            ts,
        };

        // Newer empty marker beats older content, in both insertion orders.
        let mut a = HashMap::new();
        insert_remote_lww(&mut a, "p".into(), content(100));
        insert_remote_lww(&mut a, "p".into(), empty(200));
        let mut b = HashMap::new();
        insert_remote_lww(&mut b, "p".into(), empty(200));
        insert_remote_lww(&mut b, "p".into(), content(100));
        assert_eq!(a["p"].size, 0, "newer truncation must win");
        assert_eq!(a["p"].ts, b["p"].ts, "resolution must be order-insensitive");
        assert_eq!(manifest_fingerprint(&a), manifest_fingerprint(&b));

        // Newer content beats older empty marker.
        let mut c = HashMap::new();
        insert_remote_lww(&mut c, "p".into(), empty(100));
        insert_remote_lww(&mut c, "p".into(), content(200));
        assert_eq!(c["p"].size, 10, "newer content must win");

        // Equal ts: deterministic tie-break (content over marker), either order.
        let mut d = HashMap::new();
        insert_remote_lww(&mut d, "p".into(), empty(300));
        insert_remote_lww(&mut d, "p".into(), content(300));
        let mut e = HashMap::new();
        insert_remote_lww(&mut e, "p".into(), content(300));
        insert_remote_lww(&mut e, "p".into(), empty(300));
        assert_eq!(d["p"].size, 10);
        assert_eq!(manifest_fingerprint(&d), manifest_fingerprint(&e));
    }

    /// Regression for "new folders vanish": empty-dir cleanup after a delete must
    /// remove ONLY the directories that a deleted file actually emptied, and must
    /// leave a user-created empty folder (never in `emptied_parents`) untouched.
    #[test]
    fn prune_emptied_ancestors_scoped_to_deleted_parents() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // A folder the user just created, still empty. Must survive.
        let user_dir = root.join("keep-me");
        fs::create_dir_all(&user_dir).unwrap();

        // A nested dir a delete just emptied: keep/ still holds a file, gone/ does not.
        let keep = root.join("a/keep");
        let gone = root.join("a/gone");
        fs::create_dir_all(&keep).unwrap();
        fs::create_dir_all(&gone).unwrap();
        fs::write(keep.join("f.txt"), b"x").unwrap();

        let mut emptied: HashSet<PathBuf> = HashSet::new();
        emptied.insert(gone.clone()); // the deleted file lived under a/gone

        prune_emptied_ancestors(&emptied, root);

        assert!(!gone.exists(), "a/gone was emptied by a delete → removed");
        assert!(
            keep.exists(),
            "a/keep still holds a file → left alone (walk-up stops at non-empty)"
        );
        assert!(
            root.join("a").exists(),
            "a/ still contains keep/ → not removed"
        );
        assert!(
            user_dir.exists(),
            "a user-created empty folder isn't in emptied_parents → must NOT vanish"
        );
    }
}
