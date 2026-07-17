# Known issues

Bugs and design caveats in the sync engine, found by audit and by the
production-readiness soaks. Each entry notes where it lives, what went wrong,
why, and the fix or disposition.

**Current status (2026-07-08, after the fleet/fullsize soak campaign):**

| # | Issue | Status |
|---|-------|--------|
| 1–4 | audit fixes (lock-across-await, lost deep-verify, rescan thrash, empty-marker LWW) | **fixed** |
| 7 | reconcile/startup wedge under fleet pressure (iroh-docs actor deadlock) | **fixed** (vendored iroh-docs patch + doc-read timeouts; soak-verified) |
| 8 | silent download wedges at multi-GB scale | **fixed** via #7 + #11 (watchdog retained) |
| 9 | presence mesh fragmentation at ~28 members | **fixed** (subset rejoin; soak-verified) |
| 10 | OutOfSync doc-resync storm (O(N²) sessions) | **fixed** (bounded kicks; soak-verified) |
| 11 | unbounded iroh path-retry queue → OOM abort | **fixed** (vendored iroh patch; soak-verified) |
| 12 | multi-master delete resurrected by a still-seeding master | **fixed** (timestamped tombstones) |
| 5 | LWW: local mtime vs doc record timestamp (publish-lag/skew sensitive) | design note, soak-evidenced |
| 6 | master-side in-place corruption propagates (master = source of truth) | design note (a deep-verify WARN is cheap future work) |
| 13 | iroh-docs `del` is prefix deletion (prefix-nested filenames collide) | design note (latent, rare, self-healing) |
| 14 | replicated ignore-list *content* never reaches peers (silent local fallback) | **open** (found 2026-07-10 during member-registry work) |
| 15 | doc writes during a virgin replica's initial sync churn the session (can re-open #12) | latent (member registry gated; ignore publish still exposed) |
| 16 | cold-join bootstrap is a single creator endpoint id — any master *should* be able to bootstrap, none can | **fixed** (share-key pkarr rendezvous + remembered members in the dial set) |
| 17 | a fully-partitioned node reports `Healthy 100%` (health of an empty peer set) | **fixed** (`ShareStatus::NoPeers`) |
| 18 | a locked OS keystore silently demotes a master to viewer — which then **reverts the user's edits** | **fixed** (held inert + auto-retry; found 2026-07-14 in the field) |
| 19 | a just-joined member's empty replica reports 100% + a fingerprint → false `OutOfSync` on every peer the instant it joins | **fixed** (advertise `0`/unknown until the replica is seen; found 2026-07-15 in the field) |

Three vendored crates carry the upstream fixes (`vendor/iroh`, `vendor/iroh-blobs`,
`vendor/iroh-docs` — see `[patch.crates-io]` in the workspace `Cargo.toml`).
**Report those bugs upstream before any iroh-stack bump.**

Cross-OS test-bench issues live separately in `cross-os-testing.md` ("Open
issues"); this file is for engine/sync logic. For the divergence design see
`divergence-detection-plan.md`; for the soak evidence trail see
`production-readiness-plan.md` (soak run log) and `docs/soak-reports/`.

---

## 1. Engine lock held across a network `await` in the divergence self-heal — **FIXED**
**Tier:** confirmed · **Severity:** medium (responsiveness) · **Status:** fixed
**Where:** `crates/seed-daemon/src/main.rs:292`, `crates/seed-core/src/engine.rs:1975`

> **Fixed.** `resync_diverged_docs` was split into `Engine::diverged_doc_resyncs()`
> (builds `(share_id, doc, peers)` jobs under a brief lock, no await) + `DocResync`
> (runs `start_sync` **off-lock**), mirroring `presence_rejoins`. The daemon loop now
> collects then runs off-lock. `AddShare` got the same treatment (`add_share_open` +
> `DocResync::start`), and `open_share` no longer dials under the lock. This was the
> root cause of the macOS "a freshly-added share never syncs and reads Healthy 100%"
> report: a hung `start_sync` froze the whole reconcile loop until the daemon was
> restarted. Original write-up kept below for context.

```rust
// reconcile_loop, every ~6s (tick % 8)
daemon.engine.lock().await.resync_diverged_docs().await;
```

**Symptom:** when one or more shares are `OutOfSync`, the daemon can stall — reconcile
and IPC/GUI calls block — for as long as the divergence re-sync dials take.

**Root cause:** the temporary `MutexGuard` returned by `engine.lock().await` stays
alive for the whole `resync_diverged_docs().await` expression, which loops over the
diverged shares awaiting `doc.start_sync(peers).await` (`engine.rs:1975`) — a network
operation. So the engine mutex is held across every dial. This is the one place in
`reconcile_loop` that breaks the loop's otherwise-consistent discipline: presence
broadcasts (`main.rs:271`), presence rejoins (`:284`), and reconcile jobs (`:235-262`)
all **build a small value under a brief lock, then await off-lock.** Self-heal fires
exactly when the mesh is already unhappy, so the stall lands at the worst time.

**Suggested fix:** split `resync_diverged_docs` like `presence_rejoins` — under the
lock, collect `(share_id, doc, peers)` for out-of-sync shares and return them (no
await); drop the lock; then `start_sync` each off-lock.

---

## 2. A requested / periodic deep-verify can be silently lost (race) — **FIXED**
**Tier:** confirmed · **Severity:** medium (silent: drops a scheduled integrity scan)
**Status:** fixed
**Where:** `crates/seed-core/src/engine.rs:1928` (`request_deep_verify`), `:1938`
(`periodic_deep_verify`), clobbered at `:2168` (`finish_reconcile`)

> **Fixed** per the suggestion below: the force is now an explicit
> `ShareState.force_deep_verify` flag carried into the job as
> `ReconcileJob.force_scan` and cleared only when a *forced outcome commits*
> (`finish_reconcile`); `last_deep_verify` advances on completion, not request, so
> a lost verify can no longer skip its 4 h re-arm. Original write-up kept below.

```rust
// request_deep_verify / periodic_deep_verify force the next scan:
s.last_quick_sig = 0;
...
// finish_reconcile, unconditionally, at the end of every reconcile:
state.last_quick_sig = out.new_quick_sig;
```

**Symptom:** a deep verify the daemon "scheduled" (logged as such at `main.rs:294-298`)
sometimes never runs; in-place corruption with unchanged (size, mtime) survives, and
because `last_deep_verify` was already advanced, the share won't re-arm for ~4 h
(`DEEP_VERIFY_INTERVAL_SECS`).

**Root cause:** the force is expressed by zeroing `last_quick_sig` so the next
`run()` sees `quick_sig != last_quick_sig` and does a full scan. But a reconcile job
captures `last_quick_sig` at build time (`engine.rs:2132`) and `finish_reconcile`
unconditionally writes back its `out.new_quick_sig` (`:2168`). If
`periodic_deep_verify` (called every ~6 s from `main.rs:293`) or `request_deep_verify`
runs while a job is in flight, the `= 0` is overwritten by that job's commit — and
that job did *not* deep-verify.

**Suggested fix:** make the force explicit and finish-safe. Add a
`force_deep_verify: bool` (or `force_scan`) on `ShareState`; have the request/periodic
paths set it; have `make_reconcile_job` read-and-clear it to force `do_scan`; ensure
`finish_reconcile` never resurrects it. Removes the reliance on `last_quick_sig = 0`
surviving a concurrent commit.

---

## 3. Self-heal re-hashes the entire folder every 60 s while `OutOfSync` — **FIXED**
**Tier:** confirmed · **Severity:** medium (CPU/disk thrash on large shares)
**Status:** fixed
**Where:** `crates/seed-core/src/engine.rs:2208-2212`, constant at `:215`

> **Fixed** per the second suggested option: the 60 s rescan cadence
> (`DIVERGENCE_RESCAN_MIN_SECS`) is gone. While diverged, healing rides the cheap
> paths (per-tick blob re-materialization + the daemon's ~6 s doc-resync kicks);
> the self-heal escalates to at most **one** forced deep verify per divergence
> episode, after 10 min diverged (`DIVERGENCE_DEEP_VERIFY_SECS = 600`), with the
> episode latch cleared on re-agreement. Original write-up kept below.

```rust
// finish_reconcile, while persistently diverged:
if now_secs() - state.last_deep_verify >= DIVERGENCE_RESCAN_MIN_SECS { // 60
    state.last_quick_sig = 0; // forces a full hashing scan + full reconcile next pass
    state.last_deep_verify = now_secs();
}
```

**Symptom:** a share stuck in `OutOfSync` re-hashes every file and runs a full
reconcile on a 60 s cadence. On the intended workload (multi-GB shares, ISOs) that's
heavy and sustained.

**Root cause / amplifier:** `OutOfSync` only requires a manifest disagreement that
outlives the 45 s settle window (`DIVERGENCE_SETTLE_SECS`). A viewer that is merely
*slow* to receive a doc update — large manifest, slow link — can trip it without any
real corruption, then thrash-rehash every minute until the doc catches up. The
self-heal can degrade the responsiveness it's meant to restore.

**Suggested fix (pick one / combine):**
- Raise `DIVERGENCE_RESCAN_MIN_SECS` substantially and/or scale it with folder size /
  peer count (the divergence plan already lists this as a "possible future").
- Don't force a *full* rehash for self-heal: the cheap recovery is re-materializing
  missing/changed blobs (a normal reconcile already does this) plus the off-lock
  `resync_doc`. Reserve the full rehash for the genuine deep-verify case (in-place
  corruption), which the 4 h periodic pass already covers.

---

## 4. Empty-marker vs content-entry for one path resolved by key-sort, not LWW — **FIXED**
**Tier:** needs verification · **Severity:** medium if confirmed (data + false alarm)
**Status:** fixed
**Where:** `crates/seed-core/src/engine.rs:419-453` (`read_remote_files`)

> **Fixed** per the suggestion below: `insert_remote_lww` resolves a live content
> entry vs a live empty marker for one path by record `ts` (newer wins) with a
> deterministic tie-break (content over marker, then hash bytes) — never stream
> order, so identical docs always fingerprint identically. Unit-tested both
> insertion orders + ties; the cross-author loopback test ships with the
> multi-master suite. Original write-up kept below.

**Symptom (potential):** for a path that flips between empty and non-empty across two
masters, the wrong side can win regardless of which edit is newer; and — worst case —
two members with an identical doc could compute different `manifest_fp`, producing a
**false `OutOfSync`**.

**Root cause:** `read_remote_files` merges two key namespaces into a single map under
the same logical path — the content key `P` and the empty-file marker `\x00e/P` both
land in `out[P]`. Within one author these stay mutually exclusive (`import_one` writes
one and `del`s the other; `tombstone` dels both). But across two masters a path can
legitimately have a *live* content entry from author B and a *live* empty marker from
author A at once — neither author deletes the other's key. `out[P]` is then decided by
the insertion order of the `get_many(single_latest_per_key)` stream, **not** by the
record timestamp `ts`. So the empty↔non-empty resolution ignores LWW, and if the
stream order isn't identical across members the fingerprint can differ.

**Verify first:** is `get_many(single_latest_per_key())` ordering guaranteed (likely
key-sorted — which would make the content key `P` deterministically win over `\x00e/P`,
keeping fingerprints equal but still ignoring `ts`)? Add a cross-author loopback test:
author A truncates `P` to empty while author B keeps it non-empty; assert both members
converge to the same `manifest_fp` and the timestamp-correct winner, with no sticky
`OutOfSync`.

**Suggested fix:** if the transition matters, dedupe `out[P]` by `ts` in
`read_remote_files` (compare the content entry's timestamp against the empty marker's
and keep the newer) instead of letting stream order decide.

---

## 7. Recovery after an unclean shutdown mid-sync can wedge (startup + first reconcile) — **FIXED (soak-verified)**
**Tier:** observed in soak, root-caused by code audit · **Severity:** high (node stuck until manual intervention)
**Status:** fixed (vendored iroh-docs patch + app-side timeouts); verified by fleet soaks #7–#8 (0 wedges vs 10/28 before; #8 PASS)
**Where:** iroh-docs 0.101's single-threaded sync actor (`src/actor.rs`) deadlocking
against its `LiveActor` (`src/engine/live.rs`); surfaced in `Engine::new` →
`reload_shares` and in `ReconcileJob::run`'s doc reads.

> **Root cause (phase-watchdog data + iroh-docs code audit).** All replica ops
> — queries, opens, subscribes, AND every inbound sync session's message
> processing — serialize through ONE actor thread with a bounded(1024) action
> FIFO. During a sync insert, the actor emits subscriber events with an
> **awaited bounded-channel send while holding the actor thread**
> (`sync.rs` `Subscribers::send`). The consumer of those events, the
> `LiveActor`, polls `biased;` with its inbox FIRST — and inbox handlers
> (`IncomingSyncReport` → `has_news_for_us`, `StartSync` → `open`) call back
> into the sync actor and await its reply. Under a divergence-driven
> sync-report storm (27 peers, persistently diverged replicas) the event
> buffer fills, the sync actor blocks mid-insert, the LiveActor is stuck in an
> inbox handler waiting on the frozen actor → **hard deadlock**. Every
> subsequent `get_one`/`get_many`/`open`/`subscribe` queues forever behind it,
> while the rest of the process (IPC, gossip, blobs) stays healthy. Explains
> both variants: the cold-start wedge (first reconcile's doc reads — 10/28
> nodes in fleet soak #6, passes stuck 10–25+ min in "read ignore list" /
> "merge remote view") and the restart-under-pressure wedge (`open_share`'s
> doc open/subscribe queue behind the frozen action).
>
> **Fix (landed):**
> - *Vendored iroh-docs, two hunks*: `Subscribers::send` uses `try_send` and
>   drops the event when a subscriber is full (never blocks the actor thread);
>   the LiveActor select drops `biased;` so event drainage can't be starved by
>   the inbox. See the `[patch.crates-io]` note in the workspace `Cargo.toml`.
> - *App-side backstop*: the reconcile pass's two doc reads are bounded by
>   `DOC_READ_TIMEOUT_SECS = 120` — a wedged read fails the pass cleanly
>   (WARN + retry next tick) instead of holding `publishing` forever; the
>   60 s phase watchdog keeps naming any overrunning phase.
>
> Original write-up kept below for the observed symptoms.

**Symptoms (fullsize soak, 6 nodes × 42 GB, 2026-07-07):**
- After a hard reboot mid-sync, all nodes restarted and served IPC, but **no
  node's first reconcile pass ever committed** (health stuck at the provisional
  0%, ~zero CPU, no log output); downloads queued by the first pass completed
  and then transfer stalled fleet-wide.
- A daemon **force-killed mid-download** then restarted *while 5 peers were
  live* never completed `Engine::new` at all (>5 min, full-debug log shows the
  keystore read from `reload_shares`, then no further seed-core activity).
  The same data dirs started in ~3 s right after the reboot when all nodes
  came up together — suggesting inbound doc-sync/blob pressure during
  recovery participates in the wedge.
- Windows keystore ops also measured slow under load (20+ s per credential
  read) — an aggravator, not the cause.

**Repro recipe:** run `seed-soak fullsize`, kill one daemon mid-sync
(`Stop-Process -Force`), restart it with the other daemons still running.

**New evidence (fleet soak #3, 2026-07-07): a COLD-start variant.** 3 of 28
freshly-started nodes (fresh data dirs, no recovery involved) wedged the same
way: ~3 MB of blobs arrived, then the node went silent — `seqno=0` (never
applied a doc update), 0 %, near-zero CPU, **2 log lines total**, IPC and
presence still fine. Signature fits the share's first `ReconcileJob` never
returning: `publishing` stays set, so no new job is ever built and nothing is
ever logged. The common precondition across both variants is *first reconcile
under live pressure from a large peer set*, not unclean shutdown per se.
**Instrumentation landed:** per-pass phase breadcrumb on `ReconcileJob`
(`phase_handle`) + a daemon slow-pass watchdog that WARNs every 60 s with the
phase while a pass overruns — the next soak/repro names the exact wedged await
(doc stream read, store `has`, import, materialize, …).

**Suggested investigation:** timeout + WARN instrumentation around each await
in `reload_shares`/`open_share` (doc open, keystore, gossip subscribe,
`start_sync`) and in the first reconcile's store calls (`has`, export,
`get_many`), to pin which actor call never resolves; then check iroh-blobs /
iroh-docs recovery behavior for stores killed mid-write. Until fixed, treat
power-loss-mid-sync recovery as requiring a retry (daemon restart when idle
peers) or empty-store resync.

---

## 8. Content downloads can wedge silently at multi-GB / multi-peer scale — **FIXED (via #7 + #11)**
**Tier:** observed in soak · **Severity:** high (sync stalls indefinitely) · **Status:** closed — root causes were #7 (iroh-docs actor deadlock) and #11 (iroh path-retry queue collapse) as seen from the download path; watchdog retained as safety net
**Where:** `ensure_download` / the in-flight map (`crates/seed-core/src/engine.rs`)

> **Closed (fullsize #6, 2026-07-08, full fix stack):** the silent-wedge
> signature (receivers pinned at 0–5 % for hours, zero log lines) did not
> reproduce — every receiver progressed continuously for the whole window,
> rate-bound only by the shared spinning disk. The 3 stall-watchdog fires in
> the run were slow-transfer recycles under deliberate verify congestion
> (aborted at the 15 min bound, resumed from verified chunks — by design).
> Original write-up kept below.

**Symptoms (fullsize soak #2, 3M+3V × 42 GB, 2026-07-07,
`docs/soak-reports/2026-07-07-fullsize-2-download-stall.md`):** of five
receiving nodes, four sat at 0–5 % for 2.5 h with **zero transport errors
logged**; only the viewer that was paused and resumed mid-run (which aborts and
re-queues its downloads) went on to reach 100 %. The in-flight map dedupes by
hash and entries are only removed when the download task settles, so a wedged
future blocks its blob's re-queue forever. The health feature correctly
alerted on every stuck member throughout.

**Mitigation (landed):** `Engine::abort_stalled_downloads` — the daemon aborts
any download in flight longer than `DOWNLOAD_STALL_ABORT_SECS` (15 min); the
next reconcile re-queues it and verified chunks resume from disk. Converts a
permanent stall into a bounded hiccup.

**Root cause (open):** why the download futures hang in the first place —
suspected iroh connection/stream starvation when several nodes swarm the same
few multi-GB blobs from one provider set. Needs a focused investigation with
downloader-level tracing; also re-examine `SWARM_DEADLINE_SECS` interplay (the
soak's deadline-retry grep found 0 hits, so the per-attempt deadline may not be
firing on the wedged path).

---

## 9. Presence mesh fragments at fleet scale (~28 members) — **FIXED (soak-verified)**
**Tier:** observed in soak · **Severity:** high for the target topology (3 masters + 20–30 viewers)
**Status:** fixed (subset rejoin); verified by fleet soaks #6–#8 (membership 28.0/28 held; #8 PASS)
**Where:** `presence_rejoins` strategy (`crates/seed-core/src/engine.rs`) vs
iroh-gossip's bounded active view.

> **Fix landed** per the suggestion below: `presence_rejoins` now targets only
> the peers the roster has NOT heard within the online TTL (a peer we can't
> hear is either partitioned — exactly what a join repairs — or down, where the
> dial fails harmlessly), sampled at random and capped at
> `PRESENCE_REJOIN_SAMPLE = 3` per share per tick. Once every known member is
> heard, no rejoins are issued at all and gossip's own shuffle maintains the
> overlay; repair of a partition is driven by the partitioned side, which sees
> the low online count. Selection logic is the pure `select_rejoin_targets`
> (unit-tested: unheard-only, capped, empty when converged). Verification =
> re-run the fleet soak below. Original write-up kept for context.

**Symptoms (fleet soak, 3M+25V, scaled corpus, 2026-07-07):** per-node
membership wildly uneven and never converging (nodes see 1/28 … 25/28 online at
t+21 min); content spread starved by the fragmented rosters (many nodes <10 %
of a 0.47 GB corpus that an 8-node fleet syncs in minutes); widespread
`OutOfSync` from slow doc propagation. Presence works all-to-all at ≤8 members
(smoke run) and fragments at 28.

**Hypothesis:** every node calls `join_peers` with ALL known members every ~6 s
(`presence_rejoins`). iroh-gossip (HyParView) keeps a small bounded active
view; at 28 members the constant full-set joins evict each other's neighbors,
so the relay overlay never stabilizes and epidemic delivery breaks down. The
all-to-all repair that fixes the 3-node star is poison at fleet scale.

**Suggested fix:** rejoin with a small RANDOM subset (2–3 peers) and only when
the roster looks stale (e.g. online count far below total known), letting
gossip's own shuffle maintain the overlay; verify with the fleet soak
(`seed-soak fleet --masters 3 --viewers 25 …`) — success = every node's
membership converging to 28/28 and staying there.

---

## 10. OutOfSync doc-resync storm saturates fleet CPU (O(N²) sessions) — **FIXED**
**Tier:** observed in soak · **Severity:** high for the target topology · **Status:** fixed
**Where:** `Engine::diverged_doc_resyncs` (`crates/seed-core/src/engine.rs`), asked
every ~6 s by the daemon's presence loop.

**Symptoms (fleet soak re-run after the #9 mesh fix, 3M+25V, 2026-07-07):** the
mesh held 25–27/28 while idle (the #9 fix works), then collapsed to avg ~7/28
within a minute of churn starting; daemon CPU climbed monotonically to
300–1400 % per node; reconcile passes took 69 s on a 0.47 GB corpus; sample IPC
requests timed out fleet-wide. Node logs showed 1200+ "re-kicked doc sync"
lines in 17 min, with several completing in the same millisecond (piled-up
sessions).

**Root cause:** while a share reads OutOfSync, *every* member issued a
`doc.start_sync` kick *every ~6 s* against **all** known members (~27). Set
reconciliation runs on both ends of every session, so a mostly-diverged fleet
(which a fresh 28-node deploy or any churn wave is, thanks to the 45 s settle
window) runs O(N²) concurrent reconciliation sessions continuously. The CPU
saturation then delays presence beats past the 20 s online TTL, collapsing the
roster — which starves provider selection, prolongs OutOfSync, and feeds back
into more resync kicks. The same "repair everything, all the time" pattern as
issue #9, one layer up.

**Fix (landed with the #9 fix):** bounded repair — at most one kick per share
per `DIVERGENCE_RESYNC_KICK_SECS` (30 s), each against at most
`DOC_RESYNC_SAMPLE = 3` randomly-sampled members (pairwise reconciliation
means any one up-to-date peer heals us). Live doc sync keeps replicating on
its own between kicks; the kick is only the stalled-session nudge.

---

## 11. Unbounded path-open retry queue in iroh core → OOM abort at fleet scale — **FIXED (vendored patch)**
**Tier:** observed in soak, allocation-backtrace-confirmed · **Severity:** critical (daemon death) · **Status:** fixed via vendored iroh patch; report upstream
**Where:** `iroh 1.0.0` `src/socket/remote_map/remote_state.rs` (`pending_open_paths`,
`open_path_on_conn`); amplifiers fixed in `vendor/iroh-blobs/src/provider.rs` and
`swarm_download` (`crates/seed-core/src/engine.rs`).

**Symptoms (fleet soaks #3–#4, 3M+25V, 2026-07-07):** daemons died with
`memory allocation of N bytes failed`, N forming a doubling sequence (5, 10,
20, 40, 80 GiB). RSS grew 15–40 MB/s on nodes with *partially-synced, actively
churning* peer sets; growth continued unchanged through a share pause; blob
data on disk barely moved; deaths cascaded (each dead peer accelerated the
leak on survivors). Pre-existing (visible in soak #2's samples) — first
surfaced as OOM once the mesh + resync fixes let runs live long enough.

**Root cause (allocation backtrace via a tracing global allocator in the
daemon, `[alloc-trace]` in `seed-daemon/src/main.rs`):** the huge allocation is
`VecDeque::push_back` growth in **iroh's per-remote path-open retry queue**
(`remote_state.rs:1062`). When `open_path_ensure` fails with
`RemoteCidsExhausted` / `MaxPathIdReached`, the address is pushed to
`pending_open_paths` **without dedup** and a drain runs 333 ms later that
re-opens each entry on **all** connections — every entry that still fails is
re-pushed (multiplied per connection). A remote whose CIDs stay exhausted — a
dead, wedged, or overloaded peer, which a struggling fleet has plenty of —
turns the queue into unbounded (worse than linear) growth until the deque's
doubling realloc fails. Not fixed upstream as of iroh 1.0.2 (2026-07-06).

An earlier audit blamed iroh-blobs' uncapped provider accept loop; that is a
real unboundedness and its fix is kept as defense-in-depth, but the
allocation backtrace shows the daemon-killing growth is this queue.

**Fix (landed):**
- *Vendored iroh patch* (`vendor/iroh`, one hunk): dedup `pending_open_paths`
  on push and cap it at 64 entries. See the `[patch.crates-io]` note in the
  workspace `Cargo.toml`. **Report upstream before any iroh bump.**
- *Defense-in-depth, kept:* iroh-blobs provider accept loop capped at 16
  concurrent streams/connection (vendor patch 2); swarm part-primaries rotate
  only over members that can serve any range (fully-synced peers + master per
  seeding policy), with partial peers as fallbacks — cuts the pointless
  request storm that overloads partial nodes in the first place.

---

## 12. Multi-master delete races a peer's still-pending initial publish (deletion-as-absence) — **FIXED**
**Tier:** observed in soak · **Severity:** medium (unexpected resurrection; multi-master only) · **Status:** fixed (timestamped tombstones)
**Where:** the reconcile merge's "on disk, absent from replica → publish" branch
(`crates/seed-core/src/engine.rs`, `(Some(le), None)` master arm) interacting with
tombstones being iroh-docs *deletions* (empty entries, filtered out of reads).

> **Fixed** per the suggested durable fix: deletes now also write a
> `\x00t/<path>` control entry whose record timestamp is the delete time.
> `read_remote_files` resolves tombstones against live content by LWW
> (map-based, order-insensitive; ties favor content — the same anti-data-loss
> bias as the empty-marker tie-break), and the merge's "new local file" arm
> compares a surviving tombstone against the file's mtime: older file →
> deleted (the #12 race honored); newer file → legitimate edit-after-delete,
> republished (whose fresher record then beats the tombstone everywhere).
> Stale tombstones are never `del`-cleared (see #13) — they simply lose the
> LWW forever. Old readers skip unknown control keys, so the new keyspace is
> wire-compatible. Unit-tested (`tombstones_resolve_by_lww_content_wins_ties`)
> + integration-tested (`delete_survives_unseen_master_copy`: the exact
> soak-observed race, both directions).
>
> **Refinement (delete-then-replace, the "vanishing ISO" bug):** the mtime-only
> tie-break was too strong — a file that is *copied, extracted from an archive,
> or downloaded* keeps its **source's** mtime, which is older than the delete,
> so replacing a deleted file with a new version of the same name deleted it on
> every pass, forever, even for the member who deleted it. The tombstone now
> carries the **deleted content's hash** as its value, and the merge's "new
> local file" arm suppresses a local file only when it is the *exact deleted
> content still lingering* (same hash AND not-newer mtime — the #12 race);
> **different** content at that name is a genuine re-add and always publishes,
> regardless of mtime. Legacy tombstones (value `[1]`, no hash) and ones whose
> tiny value blob hasn't synced yet fall back to the old time-only rule,
> self-correcting once the hash is available. Extracted into the pure,
> unit-tested `tombstone_suppresses` helper and covered end-to-end by
> `replaced_file_survives_stale_mtime`. Only remaining semantic: re-adding the
> **byte-identical** deleted file with a **preserved older** mtime still reads
> as the leftover and is removed — indistinguishable by content or time, and a
> no-op for the user either way. Original write-up kept below.

**Symptom (fleet soak #7, 3M+25V):** files deleted by churn on one master while
another master was still working through its initial import/publish of the same
seeded corpus came back fleet-wide: the slower master reached the path, saw
file-on-disk + **no live replica entry** (the tombstone reads as absence, which
is indistinguishable from never-seen), classified it as a brand-new local file
and re-published it. All 28 nodes then converged — consistently — on the
resurrected copy. Steady-state deletes propagate correctly (base == remote hash
→ tombstone honored); only the concurrent-independent-seeding window is racy.

**Why it's a design caveat, not a plain bug:** deletion-as-absence cannot
distinguish "deleted" from "not yet seen", and the engine deliberately biases
toward not destroying content it can't prove was deleted.

**Suggested durable fix (future):** a timestamped tombstone control entry
(e.g. `\x00t/<path>` carrying the delete time) so a master meeting an on-disk
file can LWW the delete against its own content instead of assuming "new".
Until then: the soak harness gates churn on all masters reaching Healthy @
100 % (steady state), and users should expect that deleting a file while
another master is still doing its first full sync of the same folder may bring
it back.

---

## 13. iroh-docs `del` is PREFIX deletion — prefix-nested filenames collide
**Tier:** note (latent; found during the #12 fix) · **Severity:** low-medium (rare name shapes)
**Where:** every `doc.del(author, key)` call (`tombstone`, `import_one`'s
marker cleanup) — iroh-docs 0.101 `Doc::del` "deletes entries that match the
given author and key **prefix**", i.e. `del("foo.txt")` also clears the
entries of `foo.txt.bak` (same author) existing at that moment.

**Impact:** deleting a file whose name is a strict prefix of another file's
name (no separator required: `report` vs `report-final`; a file vs a
*directory* of the same name can't coexist on disk, so the `/` case is safe)
transiently clears the longer path's entries too. Viewers delete the longer
file; the master re-publishes it on its next scan (still on disk, reads as
new), so it self-heals with a newer record — a transient viewer-side deletion
+ resurrection, not permanent loss. The #12 tombstone fix deliberately avoids
adding new hazards: republishes do NOT `del` stale `\x00t/` markers (they
lose LWW instead), because `del("\x00t/foo")` would nuke the live tombstone
of a deleted `foobar`.

**Fix direction (future):** an exact-key delete needs upstream support (an
empty entry IS iroh-docs' prefix-tombstone primitive), or app-level keys made
prefix-free (e.g. length-prefixed / terminator-suffixed path keys — a wire
format change).

---

## 5. LWW compares local file mtime against the doc *record* timestamp
**Tier:** note · **Severity:** low (semantic; skew-sensitive) · **Observed in soak** (fleet #7)
**Where:** `crates/seed-core/src/engine.rs:1243-1244`

> **Soak evidence (2026-07-07, fleet #7):** not just theoretical — under heavy
> initial-sync load (reconcile passes lagging seconds-to-minutes), a write on
> master A followed 1.2 s later by a write on master B resolved fleet-wide to
> **A's older content**: A's doc *record* got its timestamp at publish (after
> B's file mtime), so B's LWW took A's "newer" record. The fleet stayed fully
> consistent — the ordering just didn't match wall-clock intent. Wall-clock
> ordering across masters is only honored when edits are spaced further apart
> than publish lag; ordering between edits made *after seeing* the other side
> (causal ordering) is always honored. The soak harness now asserts the causal
> form.

```rust
let local_ts = le.abs.as_ref().map(|a| mtime_micros(a)).unwrap_or(0);
if local_ts >= re.ts { /* publish local */ } else { /* take remote */ }
```

**Issue:** `local_ts` is the file's content mtime (when it was *edited*); `re.ts =
e.timestamp()` is when the doc *record* was written. Two different clocks: a delayed
publish yields a high `re.ts` for older content, and the comparison is subject to
wall-clock skew between masters. Ties (`>=`) always favor local. This is largely
inherent to multi-master LWW, but it's an undocumented, skew-sensitive decision — on
masters with skewed clocks it can pick the "wrong" winner. Flag as a deliberate
design note; no behavior change proposed.

---

## 6. Master-side in-place corruption propagates instead of healing
**Tier:** note · **Severity:** low–medium (multi-master data risk, by design)
**Where:** master branch of the reconcile merge, `crates/seed-core/src/engine.rs`
(`Some(bh) if bh == &re.hash` → "publish local"), with deep-verify at `:2208`

**Issue:** on a master, content that differs from what it published — even same-size,
same-mtime in-place corruption that a deep verify surfaces — is indistinguishable from
a legitimate edit, so it gets *published* to peers rather than healed. Only viewers
heal from the manifest. This is fundamental to "master = source of truth," but on a
multi-master share it means one corrupted master can overwrite good copies on the
others (LWW decides). Inherent, not a clean bug — but worth at least a WARN when a
deep-verify finds a content-hash change with an unchanged (size, mtime), so silent
corruption isn't silently propagated.

---

## 14. Replicated ignore-list *content* never reaches peers (silent fallback)
**Tier:** confirmed · **Severity:** low–medium (viewer may not honor a master's ignores)
**Where:** `crates/seed-core/src/engine.rs` — `read_ignore_list` vs. the
`set_download_policy(DownloadPolicy::NothingExcept(vec![]))` set in `open_share`

**Issue:** the `\x00ignore` entry stores its CBOR list as blob *content*
(`set_bytes`), but every replica disables iroh-docs' content auto-downloader
(so file blobs can be fetched engine-driven, peers-first), and nothing else
ever fetches the ignore blob. On a peer, the entry's metadata syncs but
`blobs.has(hash)` stays false forever, so `read_ignore_list` returns `None`
and the reconcile silently falls back to the locally-configured list. A viewer
with local copies of paths a master ignores can therefore delete them (the
mirror treats not-in-replica as deleted) instead of leaving them alone.

**Why it survived:** the fallback is silent and the common case (no custom
ignores, or ignores configured identically on both sides) behaves the same.

**Disposition:** open. Found while designing the `\x00m/` member registry,
which dodged the same trap by encoding its payload **in the doc key** (content
is a 1-byte marker) — see `docs/member-registry.md`. Candidate fixes: ride the
key the same way, have the reconcile `ensure_download` the ignore hash, or use
a docs download policy of "everything under `\x00`".

---

## 15. Doc writes during a virgin replica's initial sync can churn the session (latent)
**Tier:** confirmed mechanism, latent exposure · **Severity:** medium when hit (delete resurrection)
**Where:** any `doc.set_bytes` early in a `ReconcileJob` pass on a just-joined
share — today the master ignore-list publish (`run` step 1); the member
registry had it and was fixed

**Issue:** a local doc write while a share's *initial* doc-sync is still in
flight can churn/restart the sync session (`AbortReason::AlreadySyncing`-style
interplay). If a joining **master**'s first merge then runs against a
still-virgin replica, it republishes its local copies as brand-new entries
whose fresh LWW timestamps beat existing delete tombstones — the exact
resurrection race #12's tombstones exist to prevent, now re-opened from the
other side.

**Evidence:** reproduced deterministically while building the member registry
(2026-07-10): publishing a member record at step 1.5 of a joining co-master's
first pass flipped `multi_master::delete_survives_unseen_master_copy` from
~10s green to a reproducible 120s timeout (2/2 runs); disabling the write
restored baseline (A/B on identical trees). Fixed for member records by
publishing only at the END of a pass gated on the replica having proven
contact with share state (`replica_seen` in `ReconcileJob::run`).

**Remaining exposure:** a joining master whose *configured ignore list*
differs from the replicated one publishes `\x00ignore` at step 1 of its first
pass — same write-during-initial-sync window. Not observed in practice
(configured lists usually match or are empty), inferred from the same
mechanism, not separately reproduced. Candidate fix: gate the ignore publish
on `replica_seen` the same way (one-pass delay, same as member records).

---

## 16. Cold-join bootstrap is a single creator endpoint id — the creator is a silent SPOF
**Tier:** confirmed (reproduced + fixed live on a 5-member pool, 2026-07-14)
**Severity:** high (a new member cannot join at all while one specific device is offline)
**Status:** **fixed** (2026-07-14)
**Where:** `open_share`'s bootstrap resolution + `peer_providers`
(`crates/seed-core/src/engine.rs`), `ShareKeyPayload.endpoint_id`
(`crates/seed-core/src/identity.rs`), and the fix in
`crates/seed-core/src/rendezvous.rs`

**Symptom:** a device added from a share key never syncs. It shows **exactly one**
other member — no name, role "Viewer", offline, 0% — while the share itself reports
`Healthy 100%` (see #17). Its replica stays at `seqno=0` and its folder stays empty.
The daemon log is **clean**: no errors, no panics, just
`no bootstrap given; using endpoint-id discovery`. Meanwhile the established members
are perfectly healthy with each other and see nothing wrong.

**Root cause:** when a share is added without an explicit bootstrap, the *entire*
bootstrap set is one entry — the **creating** device's endpoint id, stamped into the
key at mint time (`open_share`, `engine.rs:2699-2705`):

```rust
let mut bootstrap = bootstrap;
if bootstrap.is_empty() {
    if let Some(eid) = key.endpoint_id() {   // the CREATOR's id, and only ever that
        ...
        bootstrap.push(iroh::EndpointAddr::new(pk));
    }
}
```

If that one device is offline, a joiner has **nowhere to dial**, and every other
mechanism that knows about multiple peers is downstream of first contact and therefore
never engages: `presence_rejoins` (`engine.rs:3272`) fans out to every endpoint id the
roster learned *from doc events*; the `peer_names` table (`db.rs:125`) persists every
member's full endpoint id but is loaded only to *label* the roster
(`engine.rs:2716`), never to dial; the `\x00m/` member registry rides doc-sync, which
hasn't happened. So the design intent — *any* master key holder can bootstrap a
joiner — is not implemented: **no master other than the creator is reachable by a node
that has never synced.**

The lone ghost member is the failed bootstrap itself: the roster is fed only by doc
live events (`engine.rs:3960-3966`), and `SyncFinished` fires on **failed** syncs too,
so the dead creator gets noted as a peer we know nothing about — hence nameless, and
"Viewer" only because that's the `unwrap_or` fallback for *unknown* role
(`engine.rs:311-314`).

**Confirmation:** bringing the original creating device back online resolved the pool
instantly and completely. Adding the share with an explicit `--bootstrap` ticket from
any live member also works (`seed-cli add --bootstrap`, and the GUI's Add dialog has
the same optional field, `seed-gui/src/main.rs:2062`).

**Why it took a week to find, and why it was misdiagnosed:** the pool's other four
members had all been syncing for a week and already knew each other, so only the
*most recently joined* device was ever exposed. That device happened to be the only
Windows ARM64 box, which made "ARM64" and "cold-joined from a bare key" perfectly
correlated in a sample of five, and sent the first investigation after the ARM port
(vendored iroh forks, bundled SQLite, the two-ABI bundle). **None of that was
implicated** — the daemon's SQLite store round-trips fine (`reloaded 1 share(s)`), and
the arch is irrelevant. The now-deleted `docs/arm64-triage.md` documented that false
trail.

**Fixed (2026-07-14)** in two layers, both of which were needed — the cheap one does
not actually fix the reported bug:

1. **Remembered members join the dial set** (`peer_providers`, `engine.rs`;
   `PeerRoster::known_peer_ids`). `peer_names` had persisted every member's full
   endpoint id all along, and `open_share` already loaded those rows — but only ever to
   *label* the roster, never to dial. They now seed `open_share`'s bootstrap and feed
   presence rejoin, doc resync, and content self-heal. This makes every **restart**
   resilient to any single member being down. It does **not** rescue a first-ever join,
   which has nothing remembered yet — which is exactly the case that was reported, and
   why (2) is the real fix rather than a nicety.

2. **A rendezvous derived from the share key** (`crates/seed-core/src/rendezvous.rs`).
   Every master periodically publishes its own `EndpointAddr` as a [pkarr] signed packet
   **named by the share's public key and signed with the share seed** — not with its
   device key. Any key holder resolves that name with nothing but the key it already
   has. Two properties make the share keypair the right name to use:
   - *Viewers resolve but cannot publish.* Resolving needs only `master_pub`; signing
     needs `master_seed`. `PkarrRelayClient::resolve` verifies the signature against the
     name requested, so a viewer can neither forge a record nor clobber the real one to
     deny bootstrap.
   - *No designated device.* All masters publish under one name, so the record is
     last-writer-wins — which is the self-healing property, not a flaw: a master that is
     down stops republishing, so the newest packet is by construction from one that was
     alive within the last `REPUBLISH_SECS` (120s). Nothing to elect, keep up, or fail
     over.

   The packet is *named* by a key that is nobody's node, so the publishing master's
   endpoint id rides in `UserData` while its relay URL and direct addresses use the
   record's normal address fields; `resolve` recombines them into a dialable
   `EndpointAddr`. Lookups happen only while a share can reach **nobody** (throttled to
   one per `LOOKUP_SECS`), so a healthy pool never contacts the pkarr server at all.

**Rollout:** the rendezvous only helps once the *masters* run this code — a joiner on
the new build still cannot find a pool of 0.6.x masters, because none of them publishes.
Update the masters first.

**Privacy:** the record maps a share's public key to a master's addresses, and the
pkarr server operator sees that mapping. Anyone who can resolve it already holds a share
key, so the exposure is to the server, not the world. We use n0's public server
(`N0_DNS_PKARR_RELAY_PROD`), which n0 supports for production use. Self-hosting is a
one-constant change plus an `iroh-dns-server` — note that an **iroh relay does not serve
pkarr**; it is a separate service, so a self-hosted rendezvous means a second daemon next
to the relay, not a relay setting.

**Note on tickets:** handing out a bootstrap ticket alongside the key is a manual
unblock, not a model. A ticket is a *snapshot of an address*, so it goes stale exactly
when it is needed, and it trades one single-device dependency for another.

**Tests:** `crates/seed-core/tests/rendezvous.rs` (`--ignored`; real endpoints +
internet). `joiner_with_a_dead_creator_still_syncs_via_rendezvous` reproduces this bug
exactly — a key whose baked-in creator id is dead — and proves the joiner reaches a live
master and syncs. Verified by falsification: with the rendezvous dial removed, it never
syncs.

[pkarr]: https://pkarr.org

---

## 17. A fully-partitioned node reports `Healthy 100%` (health of an empty set)
**Tier:** confirmed · **Severity:** medium (silent: total partition is indistinguishable
from perfect health) · **Status:** **fixed** (2026-07-14)
**Where:** `ShareState::isolated` + `list_summaries` + `health_alerts`,
`crates/seed-core/src/engine.rs`; `ShareStatus::NoPeers`, `crates/seed-ipc/src/lib.rs`

**Issue:** every peer comparison the engine makes filters on *online* peers — the health
percent, `converged_with_online_peers`, the consensus fingerprint — and all of them are
vacuously true of an empty set. A node that reached **no** masters therefore agreed with
everyone it could hear (nobody) and held everything it knew about (nothing), and
reported `Healthy 100%` — 100% of nothing. Every screen read "fine" on a node with no
peer connectivity and no document sync whatsoever. This is what let #16 sit undetected on
a live share for over a week, and what made the eventual symptom so confusing (a share
simultaneously reporting `Healthy 100%` and one unhealthy, nameless member).

**Fixed:** an empty comparison set is not health, it is *ignorance*. A new
`ShareStatus::NoPeers` is reported whenever a share can reach no member — ranked above
`OutOfSync`, because `diverged_since` is sticky and a node that diverged and then lost
every peer would otherwise keep insisting "members disagree" about members it can no
longer hear at all.

The subtlety that makes this more than "warn when the peer list is empty": **two** states
have zero reachable peers, and only one is a fault. A share this device *created* that
nobody has joined yet is genuinely alone and stays `Healthy` — flagging it would cry wolf
on every freshly-created share, which is how you train people to ignore the real alarm.
So `isolated()` is `online == 0 && (known > 0 || !we_minted)`.

`health_alerts` no longer treats "no online peers" as `Offline` (which *pauses* the
episode clock, so a partitioned node accrued no unhealthy time and never alerted). An
isolated share is now `OnlineDegraded` and escalates on the normal 12h track. The state is
surfaced in the GUI status label, the **tray tooltip** (the window is usually closed —
a partition only visible once you open the app is a partition nobody notices), the
Android status dot + label, and the soak harness's anomaly detector.

**Tests:** `crates/seed-core/tests/isolation.rs` (`--ignored`). Both halves are pinned:
a lone creator stays `Healthy`; a partitioned master reports `NoPeers`. Verified by
falsification — with the check disabled, the partitioned master reports `Healthy`, which
is the original symptom exactly.

---

## 18. A locked OS keystore silently demoted a master to viewer — which then reverted the user's edits
**Tier:** confirmed (reproduced in the field, then synthetically) · **Severity:** high
(**silent data loss**: the user's own writes to their own master share were overwritten)
**Status:** **fixed** (2026-07-14)
**Where:** `reload_shares` (`crates/seed-core/src/engine.rs`), `crates/seed-core/src/secrets.rs`

**Symptom (field, 2026-07-14):** a share whose files would not sync. The device showed
**itself** as a *Viewer* despite being a master, every member reported `Healthy 100%`,
and the folder held different bytes from the rest of the pool. Files replaced on that
box reverted to the pool's older copies.

**Root cause.** Master shares keep their seed in the OS keystore. On startup,
`reload_shares` loaded it to restore write capability — and if the keystore read
**failed**, it logged a WARN and *carried on with the stored seedless key*, which is by
definition a **viewer** key:

```
WARN master seed for 4741b9bf… unavailable from keystore;
     running read-only: Secret Service: unlock prompt was dismissed
INFO reloaded 1 share(s)
```

The DB row still said `role_master = 1`; only the loaded key disagreed. The trigger is a
plain startup race: the `systemd --user` daemon starts at boot, **before** the login
keyring is unlocked, the unlock prompt is dismissed, and the share runs read-only for the
rest of the process's life.

**Why this was data loss and not a graceful degradation.** A viewer does not merely
decline to publish — it treats the replica as authoritative and **reverts local edits**
(`Viewer: replica wins, always`). `materialize()` does that by calling `self_heal_file`,
which *fetches the old bytes from a peer* and writes them over the local file. So a user
editing files in what they believed was their own master share had those edits pulled
back down from the pool and silently destroyed, while every screen read `Healthy`. The
one WARN announcing it went to a log nobody reads.

The write path was already defended against exactly this keystore flakiness
(`store_seed_bounded`: bounded, with a DB fallback, because "under the Windows LocalSystem
service (session 0) the Credential Manager API can hang indefinitely"). The read path had
neither a bound nor a fallback. `docs/linux-packaging.md` even described the mitigation as
if it covered both directions — it covers one, which is why this went unnoticed.

**Fixed:**
- **A master that cannot load its write key is held INERT** — not opened at all, never
  reconciled, so it cannot touch the user's files. Read-only is not a safe fallback for a
  master; holding it inert is the only degradation that cannot lose data.
- **The fault is visible.** New `ShareStatus::KeyLocked`, surfaced in the GUI ("⚠ Write
  key locked — unlock your login keyring" — naming the *cure*, since nothing about "my
  files stopped syncing" would lead anyone to think about their keyring), the CLI, and
  Android. The share is still listed: one that quietly vanishes is its own kind of lie.
- **It recovers in place.** `Engine::retry_locked_keys` re-asks the keystore every
  `KEY_RETRY_SECS` (first attempt on the very next tick, since the keyring usually
  unlocks seconds after the daemon races the graphical login) and opens the share the
  moment the key is available — **no daemon restart**. A fault only a maintainer knows how
  to clear is, in practice, not recoverable.
- **The read is bounded** (`load_seed_bounded`: `spawn_blocking` + 5s timeout), mirroring
  the write path, so a wedged keystore can no longer stall daemon startup.

**Tests:** `crates/seed-core/tests/keystore.rs` (`--ignored`). The data-loss test needs a
**live peer** and that is not incidental: the revert works by fetching from a peer, so a
one-node test passes against the buggy code and proves nothing (the first attempt did
exactly that). With a peer online, the old behavior is caught red-handed — the user's
`"my new content"` comes back as `"original"`.

**Note.** This one bug produced the entire cluster of confusing symptoms — the
self-reported "Viewer", the unsyncable files, `Healthy 100%` everywhere. It was
*not* a corrupted share. Confirmed by the reporter: with the master not demoted, the same
operation synced exactly as expected.

---

## 19. A just-joined member reports 100% + a fingerprint → false `OutOfSync` on every peer — **FIXED**
**Tier:** confirmed (reproduced in the field, then pinned by unit test) · **Severity:** medium
(false alarm; the pool's most serious steady-state status raised the instant a member is added)
**Status:** **fixed** (2026-07-15)
**Where:** `advertised_fp` + `ReconcileJob::run`'s outcome and `Engine::finish_reconcile`'s
divergence guard (`crates/seed-core/src/engine.rs`)

**Symptom (field, 2026-07-15):** add a new member to a share and, within the settle window
(`DIVERGENCE_SETTLE_SECS`, 45 s), the share flips to **`OutOfSync` — "members disagree"** —
long before the new member has downloaded anything. The newcomer has barely started; its
"health" should not be able to condemn the pool yet.

**Root cause.** This is [#17] turned inside-out — the health of an empty *manifest* rather
than an empty *peer set*. A member freshly added to a share has not synced the doc replica
yet, so its merged manifest (`remote`) is **empty**. Two things then conspire:

- An empty manifest reports **`health == 100`** — `total_bytes == 0`, and "I hold 100 % of
  what I know about" is 100 % of nothing (the exact vacuous-truth shape as #17).
- `manifest_fingerprint(empty)` is a perfectly valid **nonzero** fingerprint (`FP_EMPTY`);
  it never returns the `0` sentinel.

So the newcomer broadcast presence with `percent = 100` **and** `manifest_fp = FP_EMPTY`.
Divergence detection compares against peers that are *settled* — `settled_manifest_fps()`,
i.e. `percent >= 100 && manifest_fp != 0` — precisely to exclude members that are merely
*behind*. The virgin newcomer passed **both** filters, so every established peer read it as a
fully-synced member holding a different fileset, and the disagreement was stable (an empty
manifest doesn't change while the doc is still in flight) → `OutOfSync` after the settle
window. It fired on both sides: the newcomer likewise saw the established peers' real
fingerprints disagree with its own `FP_EMPTY`.

**Fixed** in two guards, both keyed on the existing `replica_seen` signal ("has our replica
proven contact with share state — files, tombstones, an ignore entry, or member records"):

1. **Advertise a fingerprint only once the replica is real.** `advertised_fp(replica_seen,
   &remote)` broadcasts the documented `0` = "unknown / not yet computed" sentinel while
   virgin — which both `settled_manifest_fps` and `online_manifest_fps` already exclude — so
   a just-joined node counts as *behind*, not *diverged*, until its doc syncs.
2. **Don't judge divergence while our own fingerprint is unknown.** `self_settled` now also
   requires `state.manifest_fp != 0`; you cannot claim a peer disagrees with a fileset you
   have not synced yet.

A genuinely empty but **established** share is unaffected: its master has written an ignore
entry / member record, so `replica_seen` is true and it advertises the real `FP_EMPTY` — two
empty masters still agree on it and converge to Healthy. And the normal join lifecycle is
clean: once the doc arrives, `manifest_fp` becomes the real (matching) value, health reflects
true download %, the member shows `Syncing` at a climbing percent, then `Healthy` — never a
false `OutOfSync`.

**Tests:** `engine::tests::advertised_fp_is_zero_until_replica_seen` (the gate: virgin ⇒ 0,
seen ⇒ real fingerprint, incl. the empty-but-seen case) and
`divergence_ignores_a_virgin_peer_reporting_full_health` (the roster half: a peer at 100 %
with an unknown fingerprint is excluded from the comparison), alongside the existing
`divergence_ignores_peers_that_are_still_syncing`.

[#17]: #17-a-fully-partitioned-node-reports-healthy-100-health-of-an-empty-set

---

## 20. In-place file overwrite re-downloaded the blob twice (2× bandwidth) — **FIXED**
**Tier:** user-reported · **Severity:** medium (wasted bandwidth; no data loss) · **Status:** fixed

**Where:** `Engine::materialize` (`crates/seed-core/src/engine.rs`), the export-to-target step.

**Symptom:** a master overwrote an existing file with new content (same name, no
delete). A peer downloaded the new blob normally, reached ~99%, then created
`<name>.seedheal-tmp` and downloaded the *entire blob again* over the network
before replacing the on-disk file — twice the bandwidth for one update. Fresh
files (not previously present) synced once, as expected.

**Root cause:** `materialize` fetches the blob into the local **blob store**
(`ensure_download` — the ~99% the user saw), then exports from the store to the
target file. That export was gated on `if !target.exists()`. For an in-place
overwrite the *old* file is still on disk, so the export was skipped even though
the new blob was already complete in the store; execution fell through to
`self_heal_file`, which opens a fresh connection to a peer and re-streams the
whole blob into `<name>.seedheal-tmp`. The store already had verified bytes
(`has(hash)` was true) — the second network fetch was pure waste.

**Fix:** when the blob is complete in the store but the target holds stale
content, remove the stale target and export from the store (zero-network).
`self_heal_file` is now only the last resort for when the store export itself
can't produce matching bytes. Regression test:
`inplace_overwrite_large_file_converges` (a >4 MiB swarm-path file overwritten in
place must converge on the peer).

Related hardening (same self-heal path): the staging file was named with
`with_extension("seedheal-tmp")`, which *replaces* the extension, so `a.bin` and
`a.txt` both staged through `a.seedheal-tmp`. Heals run sequentially within a
pass so it rarely collided, but it could also shadow a real sibling sharing the
stem. Now `heal_tmp_path` appends the suffix to the full name (`a.bin` →
`a.bin.seedheal-tmp`), keeping every staging path unique (unit test
`heal_tmp_path_appends_and_is_unique`).
