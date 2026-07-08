# Known issues

Open bugs and design caveats in the sync engine, found by audit rather than by a
failing test. Each entry notes where it lives, what goes wrong, why, and a suggested
fix. **#1–#4 are now fixed** (see each entry and `production-readiness-plan.md`);
#5–#6 remain design notes; **#7 (unclean-shutdown recovery wedge) is open**, found
by the full-size soak.

Scope: healing + multi-master behavior. The cross-member divergence *detection*
(manifest fingerprint determinism, the 45 s settle window, the false-alarm guard)
audited clean; the open items below are mostly in the newer **self-heal / deep-verify**
plumbing added in `5e5e4d8`.

All references confirmed against HEAD `fc7cd01` (after `5e5e4d8` "self-heal on
divergence + periodic deep verify"). Cross-OS test-bench issues live separately in
`cross-os-testing.md` ("Open issues"); this file is for engine/sync logic.

> For cross-OS / packaging issues see `cross-os-testing.md`. For the divergence design
> see `divergence-detection-plan.md`.

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

## 7. Recovery after an unclean shutdown mid-sync can wedge (startup + first reconcile)
**Tier:** observed in soak · **Severity:** high (node stuck until manual intervention)
**Where:** startup path (`Engine::new` → `reload_shares`) and the first
`ReconcileJob::run` after recovery; exact wedge point not yet isolated.

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

**Suggested investigation:** timeout + WARN instrumentation around each await
in `reload_shares`/`open_share` (doc open, keystore, gossip subscribe,
`start_sync`) and in the first reconcile's store calls (`has`, export,
`get_many`), to pin which actor call never resolves; then check iroh-blobs /
iroh-docs recovery behavior for stores killed mid-write. Until fixed, treat
power-loss-mid-sync recovery as requiring a retry (daemon restart when idle
peers) or empty-store resync.

---

## 8. Content downloads can wedge silently at multi-GB / multi-peer scale — **MITIGATED**
**Tier:** observed in soak · **Severity:** high (sync stalls indefinitely) · **Status:** watchdog mitigation landed; root cause open
**Where:** `ensure_download` / the in-flight map (`crates/seed-core/src/engine.rs`)

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

## 9. Presence mesh fragments at fleet scale (~28 members)
**Tier:** observed in soak · **Severity:** high for the target topology (3 masters + 20–30 viewers)
**Where:** `presence_rejoins` strategy (`crates/seed-core/src/engine.rs`) vs
iroh-gossip's bounded active view.

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

## 5. LWW compares local file mtime against the doc *record* timestamp
**Tier:** note · **Severity:** low (semantic; skew-sensitive)
**Where:** `crates/seed-core/src/engine.rs:1243-1244`

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
