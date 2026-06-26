# Distributed content downloads

How SEED Sync fetches blob **content** between peers, why the master used to do
all the uploading, and what's left to make a single large file swarm across
multiple sources.

## Background: two sync layers

Sync happens in two independent layers (see `crates/seed-core/src/engine.rs`):

1. **Manifest** — an iroh-docs multi-writer doc replicates *metadata* (path →
   content-hash entries) over gossip. This is small and already mesh-distributed.
2. **Content** — each file is one content-addressed blob (a single BLAKE3 hash).
   The bytes are fetched separately, on demand, when a peer sees a manifest entry
   for a hash it doesn't have locally.

`ReconcileJob::materialize` is the per-file decision point: if `blobs().has(hash)`
is false, the content isn't local yet.

## The problem (pre-fix)

iroh-docs ships a **built-in content auto-downloader** whose download policy
defaults to `EverythingExcept([])` — fetch every referenced blob automatically.
Its provider discovery (`ProviderNodes` in iroh-docs `engine/live.rs`) accumulates
candidates from (a) whichever peer it synced the manifest entry *from* and
(b) gossiped `ContentReady` announcements. In practice the entry is synced from
the **master**, so the master is the candidate every receiver pulls from. With
`materialize` only passively polling `has()`, the engine had no say in provider
selection — so a 1.7 GB file went master → PC B, then master → PC C, the master
saturating its uplink as the sole source.

iroh-docs' internal downloader and its provider preference are private; there is
no hook to reorder its candidates.

## The fix (current): engine-driven, peers-first downloads

We take ownership of content fetching:

- **Disable the iroh-docs auto-downloader per replica.** On opening each share's
  doc (`Engine::open_share`) we set
  `DownloadPolicy::NothingExcept(vec![])` — download nothing automatically. This
  gates only *content*; manifest/metadata sync is untouched. The policy is local
  (per-replica, not synced), so every node sets it on open.
- **Drive the fetch from the engine.** When `materialize` finds a blob missing it
  calls `ReconcileJob::ensure_download(hash)`, which queues a download on the
  node's long-lived `iroh_blobs` `Downloader` (one actor per node, created in
  `IrohNode::spawn_with_blobs`, cloned into each job).
- **Load-balanced, live provider order** (`ReconcileJob::live_providers`): the
  online share members from the **current** roster except ourselves, peers shuffled,
  with the master kept separate so it can be deprioritized to last. Read at download
  time (dynamic discovery), not from a job-creation snapshot. iroh-blobs'
  `execute_get` walks the per-part order, resuming partial progress across providers,
  so a peer that doesn't have a range fails fast and we fall through. How these
  candidates are used depends on blob size — see "Single-blob swarm" below.
- **Idempotent across ticks.** A global `downloads_inflight: HashSet<Hash>` (shared
  engine → jobs) prevents re-queuing a hash that's already streaming. The entry is
  cleared when the download task settles; a still-missing blob is simply re-queued
  on the next reconcile (free retry on failure).

## Single-blob swarm (implemented)

A correction to an earlier assumption: iroh-blobs' `SplitStrategy::Split` does
**not** split one raw blob across providers. Read `split_request`
(`vendor/iroh-blobs/src/api/downloader.rs`, the `FiniteRequest::Get` arm): it runs
`execute_get(GetRequest::blob(hash))` — the *whole* blob from a single provider —
then the per-offset requests are no-ops. `Split` is for **HashSeqs/collections**
(one child blob per provider), not for chunking a single file. So a lone large file
(an ISO = one blob) is exactly the case `Split` can't help.

The real fix is to chunk the blob ourselves with **range requests**. iroh-blobs
exposes `GetRequest::blob_ranges(hash, ChunkRanges)` to fetch an arbitrary chunk
range of one blob, and the store reassembles bao-verified ranges into the whole.
`swarm_download` (`crates/seed-core/src/engine.rs`) uses this:

- For a blob ≥ `SWARM_MIN_SIZE` (4 MiB) with ≥ 2 online peers, split the chunk
  range `[0, ChunkNum::chunks(size))` into one contiguous part per peer (capped at
  `SWARM_MAX_PARTS` = 16, so no single member serves more than ~1/16 of a file) and
  download the parts **concurrently** (a `JoinSet`).
- Each part prefers a **distinct peer** (part `i` → `peers[i]`); the master is
  fallback only, never a primary.
- Providers come from `live_providers`, which reads the **live roster at download
  time** (online peers shuffled, master separate) — dynamic discovery, not the
  job-creation snapshot.
- Below the threshold, or with < 2 peers, it takes the simple whole-blob path.

### Rounds + cold-start relief

`swarm_download` runs in **rounds**: each round re-reads the live roster, re-issues
the still-missing parts (completed parts no-op cheaply via `execute_get`'s local
check), and pauses briefly (`SWARM_ROUND_BACKOFF_MS`) so peers can seed one another.
This is what makes the cold start — a brand-new file that only the master holds, with
many peers pulling at once — distribute instead of every node downloading a full copy
from the master:

- Each part gets a **random per-part, per-node grace** of `0..SWARM_MASTER_GRACE_ROUNDS`
  rounds during which it pulls from **peers only**; only after its grace may it fall
  back to the master (and immediately if there are no peers at all).
- Because the grace is randomized independently on every node, different nodes pull
  *different* parts off the master first, become partial seeders, and trade the rest
  among themselves. The master ends up uploading ≈ one copy total rather than one per
  node.

A whole attempt is bounded by `SWARM_DEADLINE_SECS`; on timeout it errors and the
reconcile loop re-queues, resuming from the ranges already on disk. Idempotency and
the disabled iroh-docs auto-downloader are unchanged from above.

## Measured

Byte-accounting loopback tests (`crates/seed-core/tests/loopback.rs`):

| Scenario | Result |
| --- | --- |
| **Two seeders already hold the file, fresh viewer joins** (`large_blob_swarms_across_two_seeders`, master offline, 8 MiB) | seeder1 ≈ 4.33 MB, seeder2 ≈ 4.33 MB — **~50/50 split, correct reassembly** |
| Same test with swarm disabled (control) | one seeder serves 8.6 MB, the other 70 B — single-source |
| **Concurrent cold start, 3 viewers, only master holds it** (`cold_start_relief_spreads_master_load`, 8 MiB) | master uploads **1.04×** the file total (≈ one copy); viewers trade the rest |
| Same test with relief disabled (grace = 0, control) | master uploads **3.09×** — a full copy to each viewer |
| **Staggered** (one viewer holds it, second joins) | second sources from the peer; master ≈ 0 |

The reported case — *"two members already had the file, only the master was hit"* —
is fixed (a new member pulls from both seeders), and the harder concurrent cold start
now sheds load too (≈ 1× instead of N×).

## What still doesn't help (future work)

- **Finer-grained mutual seeding.** Cold-start relief works by rationing the master
  via randomized grace, but parts are still contiguous and assignment is static
  within a round. A true rarest-first piece exchange (live "who has which chunk"
  signalling) would converge faster and handle churn better. Note we *lost*
  iroh-docs' `ContentReady` availability gossip by disabling its downloader, so such
  a scheme must reintroduce its own signal (or probe peers via `Remote::observe`'s
  bitfield).
- **Reachability.** A peer can only be a provider if this node can dial it
  (`ConnectionPool` calls `endpoint.connect(id)`, relying on the addr book /
  discovery). If peers can't reach each other (NAT, or stale addresses after
  resume-from-sleep), the swarm degrades to master-only regardless of policy — see
  `docs/sleep-resume-investigation.md`.

## Health reporting

Related fix: a share's health (the `percent` in IPC summaries / peer info, shown in
the GUI as "Syncing N%" / the peer health dot) now reflects **actual local
completeness for every role**. Previously a master was hardcoded to 100%, so a
co-master that had synced the manifest but was still fetching content misleadingly
read "Healthy 100%". Health is computed in `ReconcileJob::run` as present/total bytes
of the merged view; a source master that holds everything still computes 100
naturally, but any node still downloading reports the real percentage. The GUI was
already faithfully rendering whatever the engine reported — the bug was the hardcode,
now removed (`engine.rs`, plus the `peers()` and presence-broadcast paths).
Guarded by `health_reflects_incomplete_master`.
