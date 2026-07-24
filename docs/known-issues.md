# Known issues

Bugs and design caveats in the sync engine, found by audit and by the
production-readiness soaks. Each entry records where it lives, what went wrong,
why, and the fix or current disposition.

| # | Issue | Status |
|---|-------|--------|
| 1–4 | audit fixes (lock-across-await, lost deep-verify, rescan thrash, empty-marker LWW) | fixed |
| 5 | reconcile/startup wedge under fleet pressure (iroh-docs actor deadlock) | fixed (vendored iroh-docs patch + doc-read timeouts) |
| 6 | silent download wedges at multi-GB scale | fixed via #5 + #9 (watchdog retained) |
| 7 | presence mesh fragmentation at ~28 members | fixed (subset rejoin) |
| 8 | OutOfSync doc-resync storm (O(N²) sessions) | fixed (bounded kicks) |
| 9 | unbounded iroh path-retry queue → OOM abort | fixed (vendored iroh patch) |
| 10 | multi-master delete resurrected by a still-seeding master | fixed (timestamped tombstones) |
| 11 | iroh-docs `del` is prefix deletion (prefix-nested filenames collide) | design note (latent, rare, self-healing) |
| 12 | LWW: local mtime vs doc record timestamp (publish-lag/skew sensitive) | design note |
| 13 | master-side in-place corruption propagates (master = source of truth) | design note (now WARNs on the corruption signature) |
| 14 | replicated ignore-list *content* never reaches peers (silent local fallback) | fixed (key-encoded `\x00i/` list) |
| 15 | doc writes during a virgin replica's initial sync churn the session | fixed (ignore publish now gated like the member registry) |
| 16 | cold-join bootstrap was a single creator endpoint id | fixed (share-key pkarr rendezvous + remembered members) |
| 17 | a fully-partitioned node reports `Healthy 100%` | fixed (`ShareStatus::NoPeers`) |
| 18 | a locked OS keystore silently demotes a master to viewer, reverting edits | fixed (held inert + auto-retry) |
| 19 | a just-joined member's empty replica trips false `OutOfSync` | fixed (advertise unknown until the replica is seen) |
| 20 | in-place file overwrite re-downloaded the blob twice | fixed |
| 21 | large downloads never resume after suspend/resume | fixed (Linux logind + Win/macOS wall-clock-gap watchdog) |
| 22 | blob store never garbage-collects; store grows unbounded | fixed (hourly daemon GC pass, replica-derived live set) |
| 23 | fleet-wide silent isolation (phantom liveness + dead presence overlay) | fixed |
| 24 | empty directories are not mirrored (files-only manifest) | design note |
| 25 | cross-volume viewer dedup left a 2× copy on Windows | fixed (vendored iroh-blobs patch) |
| 26 | settings IPC is a stub the GUI never calls | deferred |
| 27 | presence is unsigned — name/health/fingerprint spoofable by members | accepted risk (v1) |
| 28 | full stat-walk per share every reconcile tick | design note (measure, then adaptive backoff) |
| 29 | swarm deadline is fixed regardless of blob size | design note |
| 30 | a file written *during* a reconcile pass stops propagating (settled-signature absorb) | fixed |

Three vendored crates carry upstream fixes (`vendor/iroh`, `vendor/iroh-blobs`,
`vendor/iroh-docs` — see `[patch.crates-io]` in the workspace `Cargo.toml`).
Report those bugs upstream before any iroh-stack bump. The divergence-detection
design lives in `divergence-detection.md`.

---

## 1. Engine lock held across a network await in the divergence self-heal

`resync_diverged_docs` looped over out-of-sync shares awaiting
`doc.start_sync(peers)` — a network op — while holding the engine mutex, because
the `MutexGuard` from `engine.lock().await` lived for the whole expression. So
reconcile and IPC/GUI calls could stall for as long as the self-heal dials took,
and the stall landed exactly when the mesh was already unhappy. This was the one
place in `reconcile_loop` that broke its otherwise-consistent "build a small
value under a brief lock, then await off-lock" discipline. It was the root cause
of the macOS "a freshly-added share never syncs and reads Healthy 100%" report:
a hung `start_sync` froze the reconcile loop until a restart.

Fixed by splitting `resync_diverged_docs` into `Engine::diverged_doc_resyncs()`
(collects `(share_id, doc, peers)` jobs under a brief lock, no await) and a
`DocResync` step that runs `start_sync` off-lock, mirroring `presence_rejoins`.
`AddShare` got the same treatment (`add_share_open` + `DocResync::start`), and
`open_share` no longer dials under the lock.

## 2. A scheduled deep-verify could be silently lost to a race

The force to deep-verify was expressed by zeroing `last_quick_sig` so the next
`run()` would see a signature change and do a full scan. But a reconcile job
captured `last_quick_sig` at build time and `finish_reconcile` unconditionally
wrote back its own `new_quick_sig`. If `periodic_deep_verify` (every ~6 s) or
`request_deep_verify` fired while a job was in flight, the `= 0` was overwritten
by that job's commit — and that job had not deep-verified. In-place corruption
with unchanged (size, mtime) survived, and because `last_deep_verify` had already
advanced, the share wouldn't re-arm for ~4 h.

Fixed by making the force explicit and finish-safe: a `ShareState.force_deep_verify`
flag carried into the job as `ReconcileJob.force_scan`, cleared only when a forced
outcome commits. `last_deep_verify` now advances on completion, not on request.

## 3. Self-heal re-hashed the whole folder every 60 s while OutOfSync

While a share stayed `OutOfSync`, the self-heal re-hashed every file and ran a
full reconcile on a 60 s cadence (`DIVERGENCE_RESCAN_MIN_SECS`). On the intended
workload — multi-GB shares, ISOs — that was heavy and sustained. Worse, a viewer
merely *slow* to receive a doc update (large manifest, slow link) could trip
`OutOfSync` without any corruption and thrash-rehash every minute until the doc
caught up, degrading the responsiveness the self-heal was meant to restore.

Fixed by dropping the 60 s rescan. While diverged, healing now rides the cheap
paths (per-tick blob re-materialization plus the daemon's ~6 s doc-resync kicks)
and escalates to at most one forced deep verify per divergence episode, after
10 min diverged (`DIVERGENCE_DEEP_VERIFY_SECS = 600`), with the latch cleared on
re-agreement.

## 4. Empty-marker vs content-entry for one path resolved by key-sort, not LWW

`read_remote_files` merges two key namespaces into one logical path: the content
key `P` and the empty-file marker `\x00e/P` both land in `out[P]`. Within one
author they stay mutually exclusive, but across two masters a path can have a live
content entry from author B and a live empty marker from author A at once. `out[P]`
was then decided by the `get_many` stream order, not by record timestamp — so the
empty↔non-empty resolution ignored LWW, and if stream order differed across
members the `manifest_fp` could differ, producing a false `OutOfSync`.

Fixed: `insert_remote_lww` resolves content-vs-marker for one path by record `ts`
(newer wins) with a deterministic tie-break (content over marker, then hash bytes),
never stream order, so identical docs always fingerprint identically. Unit-tested
both insertion orders and ties; a cross-author loopback test ships with the
multi-master suite.

## 5. Recovery after an unclean shutdown mid-sync could wedge

Observed in the fullsize and fleet soaks (2026-07-07): after a hard reboot
mid-sync, nodes restarted and served IPC but no first reconcile pass ever
committed (health stuck at the provisional 0 %, near-zero CPU, no log output);
queued downloads finished and then transfer stalled fleet-wide. A cold-start
variant hit 3 of 28 fresh nodes with no recovery involved. The common precondition
was a first reconcile under live pressure from a large peer set.

Root cause (phase-watchdog data + an iroh-docs code audit): all replica ops —
queries, opens, subscribes, and every inbound sync session's message processing —
serialize through one actor thread with a bounded action FIFO. During a sync
insert the actor emits subscriber events with an awaited bounded-channel send
while holding the actor thread; the `LiveActor` that consumes those events polls
`biased;` with its inbox first, and inbox handlers call back into the sync actor
and await its reply. Under a divergence-driven sync-report storm the event buffer
fills, the sync actor blocks mid-insert, the LiveActor is stuck waiting on the
frozen actor — a hard deadlock. Every later `get_one`/`open`/`subscribe` queues
behind it while IPC, gossip, and blobs stay healthy.

Fixed with two vendored iroh-docs hunks — `Subscribers::send` uses `try_send` and
drops the event when a subscriber is full (never blocks the actor thread), and the
LiveActor select drops `biased;` so event drainage can't be starved — plus an
app-side backstop: the reconcile pass's two doc reads are bounded by
`DOC_READ_TIMEOUT_SECS = 120`, so a wedged read fails the pass cleanly and retries
next tick. Verified by fleet soaks (0 wedges vs 10/28 before).

## 6. Content downloads could wedge silently at multi-GB / multi-peer scale

In the fullsize soak (2026-07-07), four of five receiving nodes sat at 0–5 % for
2.5 h with zero transport errors logged; only the viewer paused and resumed
mid-run (which aborts and re-queues downloads) reached 100 %. The in-flight map
dedupes by hash and clears only when a download task settles, so a wedged future
blocked its blob's re-queue forever.

The underlying causes were #5 (the iroh-docs actor deadlock) and #9 (the iroh
path-retry queue collapse) as seen from the download path; both are fixed and the
signature no longer reproduces. Retained as a safety net:
`Engine::abort_stalled_downloads` aborts any download in flight longer than
`DOWNLOAD_STALL_ABORT_SECS` (15 min); the next reconcile re-queues it and verified
chunks resume from disk, converting a permanent stall into a bounded hiccup.

## 7. Presence mesh fragmented at fleet scale (~28 members)

In the fleet soak (3 masters + 25 viewers, 2026-07-07) per-node membership was
wildly uneven and never converged (nodes saw 1/28 … 25/28 online); content spread
starved on the fragmented rosters, and slow doc propagation produced widespread
`OutOfSync`. Presence worked all-to-all at ≤8 members and fragmented at 28.

Root cause: every node called `join_peers` with all known members every ~6 s.
iroh-gossip (HyParView) keeps a small bounded active view, so at 28 members the
constant full-set joins evicted each other's neighbours and the relay overlay
never stabilized. The all-to-all repair that fixes a 3-node star is poison at
fleet scale.

Fixed: `presence_rejoins` now targets only peers not heard within the online TTL,
sampled at random and capped at `PRESENCE_REJOIN_SAMPLE = 3` per share per tick.
Once every known member is heard, no rejoins are issued and gossip's own shuffle
maintains the overlay; a partition is repaired by the partitioned side, which sees
the low online count. Selection is the unit-tested pure `select_rejoin_targets`.
Membership held 28/28 in soaks #6–#8.

## 8. OutOfSync doc-resync storm saturated fleet CPU (O(N²) sessions)

After the #7 mesh fix, the mesh held while idle then collapsed to ~7/28 within a
minute of churn; daemon CPU climbed to 300–1400 % per node, reconcile passes took
69 s on a 0.47 GB corpus, and IPC timed out fleet-wide. Logs showed 1200+
"re-kicked doc sync" lines in 17 min. While a share read `OutOfSync`, every member
issued a `doc.start_sync` kick every ~6 s against all ~27 known members; set
reconciliation runs on both ends, so a mostly-diverged fleet ran O(N²) concurrent
sessions continuously. The CPU load then delayed presence beats past the online
TTL, collapsing the roster — the same "repair everything, all the time" pattern as
#7, one layer up.

Fixed with bounded repair: at most one kick per share per
`DIVERGENCE_RESYNC_KICK_SECS` (30 s), each against at most `DOC_RESYNC_SAMPLE = 3`
randomly-sampled members (pairwise reconciliation means any one up-to-date peer
heals us). Live doc sync keeps replicating between kicks.

## 9. Unbounded path-open retry queue in iroh core → OOM abort at fleet scale

In fleet soaks #3–#4 daemons died with `memory allocation of N bytes failed`, N
doubling (5, 10, 20, 40, 80 GiB). RSS grew 15–40 MB/s on nodes with
partially-synced, churning peer sets; growth continued through a share pause while
blob data on disk barely moved, and deaths cascaded.

Root cause (allocation backtrace via a tracing allocator): the growth is
`VecDeque::push_back` in iroh's per-remote path-open retry queue
(`remote_state.rs`). When `open_path_ensure` fails with
`RemoteCidsExhausted`/`MaxPathIdReached`, the address is pushed to
`pending_open_paths` without dedup and a drain 333 ms later re-opens each entry on
all connections, re-pushing every one that still fails. A remote whose CIDs stay
exhausted — a dead or overloaded peer, of which a struggling fleet has plenty —
turns the queue into unbounded growth until the deque's doubling realloc fails.

Fixed with a vendored iroh patch (`vendor/iroh`, one hunk): dedup
`pending_open_paths` on push and cap it at 64 entries. Report upstream before any
iroh bump. Kept as defense-in-depth: the iroh-blobs provider accept loop is capped
at 16 concurrent streams per connection, and swarm part-primaries rotate only over
members that can serve a range.

## 10. Multi-master delete raced a peer's still-pending initial publish

In fleet soak #7, files deleted by churn on one master while another master was
still importing/publishing the same seeded corpus came back fleet-wide: the slower
master reached the path, saw file-on-disk with no live replica entry (a tombstone
reads as absence, indistinguishable from never-seen), classified it as new local
content and re-published it. Steady-state deletes propagate correctly; only the
concurrent-independent-seeding window was racy. Deletion-as-absence cannot tell
"deleted" from "not yet seen", and the engine deliberately biases toward not
destroying content it can't prove was deleted.

Fixed with timestamped tombstones: a delete also writes a `\x00t/<path>` control
entry carrying the delete time and the deleted content's hash. `read_remote_files`
resolves tombstones against live content by LWW (order-insensitive; ties favour
content), and the merge's "new local file" arm suppresses a local file only when
it is the exact deleted content still lingering (same hash and not-newer mtime) —
different content at that name is a genuine re-add and always publishes. This also
fixed the "vanishing ISO": a file copied or extracted keeps its source's older
mtime, so the earlier mtime-only rule deleted every replace-after-delete forever.
Legacy tombstones (no hash) fall back to the time-only rule. Covered by
`tombstone_suppresses`, `delete_survives_unseen_master_copy`, and
`replaced_file_survives_stale_mtime`.

## 11. iroh-docs `del` is prefix deletion — prefix-nested filenames collide

iroh-docs 0.101's `Doc::del` deletes entries matching the author and key *prefix*,
so `del("foo.txt")` also clears `foo.txt.bak` (same author) existing at that
moment. Deleting a file whose name is a strict prefix of another's (`report` vs
`report-final`) transiently clears the longer path's entries too; viewers delete
the longer file and the master re-publishes it on its next scan, so it self-heals
with a newer record — a transient viewer-side deletion, not permanent loss. Latent,
rare, found during the #10 fix. The #10 tombstone fix deliberately avoids new
hazards here: republishes do not `del` stale `\x00t/` markers (they lose LWW
instead), because `del("\x00t/foo")` would nuke the live tombstone of a deleted
`foobar`. A real fix needs upstream exact-key delete, or prefix-free app keys (a
wire-format change).

## 12. LWW compares local file mtime against the doc record timestamp

The merge compares the local file's content mtime against the doc *record*
timestamp (`re.ts`, when the record was written) — two different clocks. A delayed
publish yields a high `re.ts` for older content, and the comparison is subject to
wall-clock skew between masters; ties favour local. Soak evidence (fleet #7): under
heavy initial-sync load, a write on master A followed 1.2 s later by a write on
master B resolved fleet-wide to A's older content, because A's record got its
timestamp at publish, after B's file mtime. The fleet stayed fully consistent; the
ordering just didn't match wall-clock intent. Wall-clock ordering across masters is
only honoured when edits are spaced further apart than publish lag; causal ordering
(edits made after seeing the other side) is always honoured, and the soak harness
asserts the causal form. Largely inherent to multi-master LWW; a deliberate design
note, no behaviour change proposed.

## 13. Master-side in-place corruption propagates instead of healing

On a master, content that differs from what it published — even same-size,
same-mtime in-place corruption that a deep verify surfaces — is indistinguishable
from a legitimate edit, so it gets published to peers rather than healed. Only
viewers heal from the manifest. This is fundamental to "master = source of truth",
but on a multi-master share it means one corrupted master can overwrite good copies
on the others (LWW decides). Inherent, not a clean bug, so the disposition stays a
design note — but the silent part is now addressed: a master emits a WARN before
republishing content whose hash changed under a *forced* deep verify while the
folder's (path, size, mtime) signature held steady (`is_silent_corruption_scan`
gates it, so a normal edit — which moves the signature — stays quiet). The publish
still happens (a master is the source of truth); it is no longer silent. The gate
logic is unit-tested by `silent_corruption_scan_only_on_forced_verify_with_steady_signature`.

## 14. Replicated ignore-list content never reaches peers (silent fallback)

The `\x00ignore` entry stores its CBOR list as blob *content* (`set_bytes`), but
every replica disables iroh-docs' content auto-downloader (so file blobs are
fetched engine-driven, peers-first) and nothing else fetches the ignore blob. On a
peer the entry's metadata syncs but `blobs.has(hash)` stays false forever, so
`read_ignore_list` returns `None` and the reconcile silently falls back to the
locally-configured list. A viewer with local copies of paths a master ignores can
therefore delete them (the mirror treats not-in-replica as deleted). The fallback
is silent and the common case (no custom ignores, or identical lists) behaves the
same, which is why it survived. Found while designing the `\x00m/` member
registry, which dodged the same trap by encoding its payload in the doc key (see
`member-registry.md`).

Fixed by riding the key the same way: the list is now encoded into a `\x00i/` +
CBOR doc key (`ignore_list_key`/`decode_ignore_list`) with a marker value, so it
syncs as doc metadata and reaches every peer with no blob fetch. `read_ignore_list`
reads the `\x00i/` prefix and takes the freshest entry by record timestamp
(last-writer-wins across masters). The legacy `\x00ignore` value-blob entry is
ignored and harmlessly orphaned; old readers skip the unknown control key
(wire-compatible). Covered by `ignore_list_key_roundtrips_and_rejects_foreign_keys`
(unit) and `viewer_honors_replicated_ignore_list` (loopback, `--ignored`).

## 15. Doc writes during a virgin replica's initial sync can churn the session

A local doc write while a share's *initial* doc-sync is still in flight can
churn/restart the sync session. If a joining master's first merge then runs against
a still-virgin replica, it republishes its local copies as brand-new entries whose
fresh LWW timestamps beat existing delete tombstones — re-opening the #10
resurrection race from the other side. Reproduced deterministically while building
the member registry (2026-07-10): publishing a member record early in a joining
co-master's first pass flipped `delete_survives_unseen_master_copy` from ~10 s
green to a reproducible 120 s timeout. Fixed for member records by publishing only
at the end of a pass gated on `replica_seen`. The remaining exposure — a joining
master whose configured ignore list differed from the replicated one published
`\x00ignore` at step 1 of its first pass — is now closed the same way: the
key-encoded ignore publish (see #14) is deferred to the end of the pass and gated
on `replica_seen || we_minted`. A joining master waits for its initial sync; a
fresh creator (authoritative empty replica, nothing to sync from) still bootstraps
its list on pass 1, and that first `\x00i/` entry is what flips `replica_seen` true
thereafter. Guarded against regression by `delete_survives_unseen_master_copy`.

## 16. Cold-join bootstrap was a single creator endpoint id

Reproduced and fixed live on a 5-member pool (2026-07-14). A device added from a
share key never synced: it showed exactly one other member (nameless, "Viewer",
offline, 0 %) while the share itself reported `Healthy 100%` (see #17), its replica
stayed at `seqno=0`, and its folder stayed empty — with a clean daemon log.

Root cause: a share added without an explicit bootstrap had a bootstrap set of one
entry — the *creating* device's endpoint id, stamped into the key at mint time. If
that device was offline, a joiner had nowhere to dial, and every mechanism that
knows multiple peers is downstream of first contact and never engaged. So the
design intent — any master key holder can bootstrap a joiner — was not implemented.
The lone ghost member was the failed bootstrap itself, noted as a peer because
`SyncFinished` fires on failed syncs too.

Fixed in two layers. First, remembered members join the dial set: `peer_names` had
persisted every member's endpoint id all along but was only ever used to *label*
the roster; those rows now seed `open_share`'s bootstrap and feed presence rejoin,
doc resync, and content self-heal — making every restart resilient to any single
member being down. That doesn't rescue a first-ever join (nothing remembered yet),
which is the second layer: a rendezvous derived from the share key
(`rendezvous.rs`). Every master periodically publishes its `EndpointAddr` as a
[pkarr] signed packet named by the share's public key and signed with the share
seed. Any key holder resolves that name with nothing but the key it already has;
viewers can resolve (needs only `master_pub`) but cannot publish or forge (signing
needs `master_seed`). All masters publish under one name, so the record is
last-writer-wins — self-healing, since a down master stops republishing and the
newest packet is by construction from one alive within the last `REPUBLISH_SECS`
(120 s). Lookups happen only while a share can reach nobody, so a healthy pool never
contacts the pkarr server. The rendezvous only helps once the masters run this code,
so update masters first. Test:
`joiner_with_a_dead_creator_still_syncs_via_rendezvous`
(`crates/seed-core/tests/rendezvous.rs`, `--ignored`).

[pkarr]: https://pkarr.org

## 17. A fully-partitioned node reports `Healthy 100%` (health of an empty set)

Every peer comparison the engine makes filters on *online* peers — the health
percent, `converged_with_online_peers`, the consensus fingerprint — and all are
vacuously true of an empty set. A node that reached no masters agreed with everyone
it could hear (nobody) and held everything it knew about (nothing), reporting
`Healthy 100%` — 100 % of nothing. This is what let #16 sit undetected on a live
share for over a week.

Fixed (2026-07-14): an empty comparison set is ignorance, not health. A new
`ShareStatus::NoPeers` is reported whenever a share can reach no member, ranked
above `OutOfSync` (which is sticky). The subtlety is that two states have zero
reachable peers and only one is a fault: a share this device *created* that nobody
has joined yet is genuinely alone and stays `Healthy` — so `isolated()` is
`online == 0 && (known > 0 || !we_minted)`. `health_alerts` no longer treats "no
online peers" as `Offline` (which paused the episode clock); an isolated share is
`OnlineDegraded` and escalates on the normal 12 h track. Surfaced in the GUI label,
the tray tooltip, the Android status dot, and the soak anomaly detector. Test:
`crates/seed-core/tests/isolation.rs` (`--ignored`).

## 18. A locked OS keystore silently demoted a master to viewer, reverting edits

Reproduced in the field then synthetically (2026-07-14). Master shares keep their
seed in the OS keystore; on startup `reload_shares` loaded it to restore write
capability, and if the read failed it logged a WARN and carried on with the stored
seedless key — which is by definition a *viewer* key. The trigger is a plain
startup race: the `systemd --user` daemon starts at boot before the login keyring
is unlocked, the unlock prompt is dismissed, and the share runs read-only for the
rest of the process's life. This was data loss, not graceful degradation: a viewer
treats the replica as authoritative and reverts local edits via `self_heal_file`,
which fetches the old bytes from a peer and writes them over the local file. So a
user editing files in what they believed was their own master share had those edits
pulled back down and destroyed, while every screen read `Healthy`. The write path
was already defended against this keystore flakiness; the read path had neither a
bound nor a fallback.

Fixed: a master that cannot load its write key is held **inert** — not opened,
never reconciled, so it cannot touch the user's files (read-only is not a safe
fallback for a master). The fault is visible via `ShareStatus::KeyLocked` ("Write
key locked — unlock your login keyring", naming the cure) in the GUI, CLI, and
Android, and the share stays listed. It recovers in place: `retry_locked_keys`
re-asks the keystore every `KEY_RETRY_SECS` and opens the share the moment the key
is available, no restart. The read is now bounded (`load_seed_bounded`,
`spawn_blocking` + 5 s timeout) like the write path. The data-loss test needs a live
peer — the revert works by fetching from one, so a one-node test passes against the
buggy code and proves nothing (`crates/seed-core/tests/keystore.rs`, `--ignored`).

## 19. A just-joined member's empty replica trips false `OutOfSync`

Reproduced in the field (2026-07-15), then pinned by unit test. Add a new member
and, within the settle window (`DIVERGENCE_SETTLE_SECS`, 45 s), the share flips to
`OutOfSync` — "members disagree" — long before the newcomer has downloaded
anything. This is #17 turned inside-out: the health of an empty *manifest* rather
than an empty peer set. A freshly-added member hasn't synced the replica, so its
merged manifest is empty, which reports `health == 100` (100 % of nothing) and a
perfectly valid nonzero `manifest_fingerprint` (`FP_EMPTY`). Divergence detection
compares against members that are settled (`percent >= 100 && manifest_fp != 0`)
precisely to exclude ones that are merely behind — and the virgin newcomer passed
both filters, so every established peer read it as a fully-synced member holding a
different fileset.

Fixed with two guards keyed on `replica_seen`: `advertised_fp` broadcasts the
documented `0` = "unknown" sentinel while virgin (which the settled/online filters
already exclude), so a just-joined node counts as behind, not diverged; and
`self_settled` now also requires `state.manifest_fp != 0`, since you cannot claim a
peer disagrees with a fileset you have not synced yet. An empty but *established*
share is unaffected (its master has written an ignore entry or member record, so
`replica_seen` is true). Tests: `advertised_fp_is_zero_until_replica_seen`,
`divergence_ignores_a_virgin_peer_reporting_full_health`.

## 20. In-place file overwrite re-downloaded the blob twice

User-reported. A master overwrote an existing file with new content (same name, no
delete). A peer downloaded the new blob to ~99 %, then created `<name>.seedheal-tmp`
and downloaded the entire blob again over the network before replacing the on-disk
file. `materialize` fetches the blob into the local store then exports it to the
target, but that export was gated on `if !target.exists()`; for an in-place
overwrite the old file is still on disk, so the export was skipped and execution
fell through to `self_heal_file`, which re-streams the whole blob — pure waste,
since the store already had verified bytes.

Fixed: when the blob is complete in the store but the target holds stale content,
remove the stale target and export from the store (zero-network); `self_heal_file`
is now only the last resort. Related hardening: the staging file used
`with_extension`, which *replaces* the extension, so `a.bin` and `a.txt` both staged
through `a.seedheal-tmp`; `heal_tmp_path` now appends the suffix to the full name.
Tests: `inplace_overwrite_large_file_converges`, `heal_tmp_path_appends_and_is_unique`.

## 21. Large downloads never resume after suspend/resume

User-reported, field-verified fixed in v0.6.9. On a laptop that suspends
frequently, a large file (e.g. a 1.8 GB ISO) stuck mid-sync at some percent and
never finished, while small files synced fine and always-on desktops shared the
same file instantly. The node often appeared to upload while stalled on download,
and only a daemon restart cured it.

Root cause: on an `s2idle` resume the OS tears down QUIC sockets, the relay
connection, and gossip neighbours, but iroh's `netwatch` does not fire a
network-change event for that resume — so the endpoint wakes believing dead
connections are live and never re-establishes. A large transfer can't finish inside
one short wake window; small files finish in a single window, so they look fine.

Fixed: `Engine::on_resume()` calls `iroh::Endpoint::network_change()` (rebind
socket + re-home relay, bounded), unconditionally rebuilds every active share's
gossip presence subscription, and returns a `DocResync` per share to re-kick doc
live-sync — the in-process equivalent of the restart that used to be the only cure.
It is triggered by a `seed-daemon` listener on logind's `PrepareForSleep` signal
(Linux only; best-effort). `Node::shutdown()` now also flushes a partial download's
verified-range bitfield to disk before router teardown so a restart mid-download
resumes from disk. Field-verified 2026-07-21: a 1.8 GB ISO completed across three
real suspend/resume cycles with no restart. Tests:
`crates/seed-core/tests/resume.rs` (`--ignored`).

Windows and macOS are now wired too. Rather than the native power events
(`WM_POWERBROADCAST` / `NSWorkspace`), which need a message pump / run loop the
daemon has in neither its console nor its Windows-service mode, non-Linux platforms
run a dependency-free **wall-clock-gap watchdog** (`sleep_monitor_loop`,
`#[cfg(not(target_os = "linux"))]`): a suspend freezes the monotonic sleep timer
while wall time keeps advancing, so a wall gap far larger than the ~20 s tick means
the machine just resumed. Both it and the Linux logind path call the shared
`drive_resume` → `Engine::on_resume`. A spurious trigger from a forward clock jump
is harmless (`on_resume` is idempotent); detection latency is at most one tick. The
native OS hooks remain a possible future refinement if immediate, jitter-proof
detection is ever needed. Power-event behaviour is validated by real suspend on
each platform (the watchdog branch also compiles as the Windows build's active
path).

## 22. Blob store never garbage-collects (unbounded growth)

Found 2026-07-21 during cleanup. Disk-only; no correctness impact. GC primitives
exist in `vendor/iroh-blobs/src/store/gc.rs` (`gc_run_once`, `run_gc`, `GcConfig`)
but were never invoked from `seed-core`/`seed-daemon`. Orphaned blobs — most
visibly incomplete partials left over from a removed share — sat on disk
indefinitely (observed: two stranded partials totalling ~2.5 GB from a deleted
share). Complete blobs are reference-exported (`ExportMode::TryReference`) into the
share folder rather than duplicated, so the reclaimable garbage is mainly stranded
partials and outboards.

Fixed: the blob store now runs GC with an hourly sweep (`GC_INTERVAL_SECS`), wired
via `FsStore::load_with_opts` in `node.rs`. The sweep's protected ("live") set is
recomputed from **the live replicas** — `Engine::gc_refresh_job` (built under a
brief lock, run off-lock every ~2 min from the daemon's periodic loop) enumerates
`Query::all()` over every share's doc and unions **every** entry's content hash
(all keys and versions, including the tiny marker-value blob that control keys
`\x00m/`/`\x00i/`/`\x00e/`/`\x00t/` point at), plus in-flight download targets. The
set is published to a shared `GcProtect` whose store-side `add_protected` callback
copies it in synchronously (no `await`, so it meets the callback's `Send + Sync`
bound). Completeness is handled by construction (all entries, not the `sync_index`
cache) and fail-closed: no set published, or any replica read error, aborts the
sweep rather than deleting against an unknown set. Even so the sweep only ever
touches the blob *store*, never a synced folder, so a stale set at worst costs a
blob re-import/re-download. Removing a share drops its doc from the next refresh, so
its orphaned blobs become reclaimable. Test: `crates/seed-core/tests/gc.rs`
(`--ignored`) proves the live set covers a share's content and empties when the
share is removed.

## 23. Fleet-wide silent isolation (phantom liveness + dead presence overlay)

User-reported (field). Files stopped propagating fleet-wide and every member
flapped `Syncing ⇄ all offline`, not recovering until a manual `relay-remove` or a
service restart. Root-caused to a custom relay that answered its own handshake/ping
while forwarding no client↔client traffic, so `is_connected()` stayed `true` and
nothing self-healed. This is the machinery the resume fix (#21) extends via
`Engine::on_resume`.

Two independent failure modes. First, phantom liveness: the doc-event task
refreshed a peer's `last_seen` on every `SyncFinished`, including *failed* syncs, so
a partitioned node marked every peer online on each retry and reported a vacuous
`Healthy 100%` instead of the honest `NoPeers` — so the isolation ladder never
engaged. Second, a dead presence overlay while transport is alive: after a partition
doc-sync often recovers (a successful `SyncFinished` marks peers online) while the
gossip presence overlay stays silently dead (peers stick at `seqno=0`, the member
list flaps on the TTL), and because doc-sync keeps the share non-isolated the
total-isolation ladder never engaged.

Fixed: liveness is honest — `note_sync_finished` refreshes `last_seen`/`last_contact`
only when the sync actually succeeded, restoring real `NoPeers` status
(`failed_sync_never_marks_a_peer_online`). `isolation_recoveries` is generalized to
`connectivity_recoveries` with two ladders: ladder 1 (total isolation, forces
public-relay fallback) and ladder 2 (presence overlay dead while transport is alive)
which rebuilds the gossip presence subscription in-process. Presence liveness is now
tracked separately from transport (`last_presence` vs `last_contact`).

## 24. Empty directories are not mirrored

The manifest tracks files only, so an empty directory is never materialized on a
peer: deleting a folder's last file leaves the empty dir on the master while the
viewer never creates it. Benign — a recursive diff flags it — and distinct from
empty *files*, which do sync (they ride the signed manifest; see the empty-file
handling around `\x00e/`). Design note, no change planned.

## 25. Cross-volume viewer dedup left a 2× copy on Windows

Fixed via the vendored iroh-blobs patch. When a viewer's mirror folder is on a
different volume than its data dir, `ExportMode::TryReference` does a
`std::fs::rename(source→target)` and only falls back to copy on `EXDEV` (18, Linux).
Windows returns `ERROR_NOT_SAME_DEVICE` (17), which upstream iroh-blobs doesn't
match, so the export failed outright, the file was materialized by self-heal, and
the auto-downloaded owned blob was never converted to a reference — so it lingered
as a full second copy (measured 100.91 MB vs 0.91 MB same-volume for a 100 MB file).
The vendored patch (`vendor/iroh-blobs`, `[patch.crates-io]`) treats err 17 as a
cross-volume move and falls back to copy + `External`; a reclaim retry queue in
`engine.rs`/`node.rs` then deletes the leftover owned copy once Windows releases the
handle (~3 s). The patch is inert on Linux, which gets `EXDEV` and was never
affected.

## 26. Settings IPC is a stub the GUI never calls

`IpcRequest::GetSettings`/`SetSettings` and `Settings { use_relays,
custom_relay_url }` are defined in `seed-ipc`, but the daemon handlers return
defaults and no preferences window exists (the gear popover has a fixed set of
actions). Dead wire surface for now. Deferred — it becomes the natural home for a
notification opt-out toggle and similar per-device settings later.

## 27. Presence is unsigned — name/health/fingerprint spoofable by members

Presence payloads (`crates/seed-core/src/presence.rs`) are unsigned, so any share
member can broadcast presence claiming another member's identity, name, health, or
manifest fingerprint. File *content* stays safe — the manifest is signed by the
namespace key — but a member can forge "healthy" or trigger a false unhealthy alert
on masters. Accepted risk for v1: members are trusted in the intended deployment. A
signing pass over presence is tracked as deferred work.

## 28. Full stat-walk per share every reconcile tick

`scan::quick_signature` runs from every reconcile tick (750 ms) and stat-walks the
entire share tree. At thousands of files — and especially the ~28 daemons sharing
one disk in the fleet soak — this is measurable sustained CPU/IO. Design note: if it
proves significant, add adaptive backoff (stretch the scan interval while the
signature is stable, snap back on doc events).

## 29. Swarm deadline is fixed regardless of blob size

`SWARM_DEADLINE_SECS = 300` gives a single swarmed-blob attempt five minutes, so a
6 GB ISO needs ~20 MB/s sustained per attempt. Retries resume via chunk ranges, so
slower links still make progress across attempts, but with deadline-retry noise in
the logs and status. Design note: if deadline retries recur on LAN-class links,
scale the deadline with blob size.

## 30. A file written *during* a reconcile pass stops propagating

User-reported, field-diagnosed on the live system. A 1.88 GB ISO was copied over an
existing file in a share. It did not reach the other members. Every member reported
`Healthy 100%` throughout, and the peers' fingerprints agreed — the fleet was
confidently, consistently wrong.

**Field evidence** (master `bigDev`, share `ef455b63…`, ~3 h after the overwrite):

```
$ seed-cli peers --share ef455b63…
This device  name=bigDev     Master  online=true  100%
0f4b6c4987b481a4  name=steebP14s  Master  online=true  100%  path=direct
1f4137766c083f76  name=lilDev     Viewer  online=true  100%  path=direct

$ cargo run -p seed-core --example showindex -- state.db ef455b63… WinRx
WinRx_11_25H2.iso
  base=52f91659…  DIFFERS  disk=63c6426c…
```

Every member online over a *direct* path, every member at 100%, and the master's own
base index disagreeing with its own disk. The blob store had no entry for
`63c6426c…` at all — newest content blob predated the overwrite by two days — so the
new bytes were never imported, let alone published. Nothing was broken about
connectivity; the engine had simply stopped looking at the file.

The two diagnostics used are kept as examples: `examples/hashfile.rs` (what hash
would we publish for this file) and `examples/showindex.rs` (what does the persisted
base index say, and does it still match disk).

**Mechanism.** A reconcile pass ends by re-walking the folder and recording that walk
as the share's "settled" `quick_signature`:

```rust
let new_quick_sig = scan::quick_signature(&self.folder, &ignore_set, &skipped_set);
```

The intent was right — the pass writes files itself (materialize, revert, delete),
and re-scanning our own writes on the next tick is pure churn. But the walk cannot
tell our writes from anyone else's. A pass on this share takes **80 seconds**
(measured: the 4-hourly deep-verify passes log `pass took 79s`…`85s`), and copying
a ~1.9 GB file onto a spinning disk takes minutes. So the user's write lands inside
the pass, and the end-of-pass walk absorbs the new `(size, mtime)` into the baseline
**without the pass ever having hashed the new content**.

The next tick then computes `quick_sig == last_quick_sig`, sets `do_scan = false`,
and builds its local view from the *base index* — the old hash. Local and remote
agree on the old hash, so there is nothing to do. Forever. The master is blind to a
file sitting in plain sight on its own disk.

Nothing recovers it except an unrelated edit moving the signature, or the periodic
deep verify — `DEEP_VERIFY_INTERVAL_SECS = 4 * 3600`. **Worst-case propagation delay
for an overwritten file was four hours**, and the field log shows exactly that
signature: between restarts, the only reconcile passes long enough to be logged are
the ones the 4-hourly deep verify forced.

**Why every member still said `Healthy 100%`.** Two independent reasons, both of
which had to be fixed:

* The **master** was not lying by its own lights — it genuinely believed the old
  hash was current, so its manifest, its fingerprint and its health were
  self-consistent. Health could not catch this.
* **Health never consulted the filesystem.** It scored the merged manifest against
  `blobs.has(hash)` — the *blob store*. A member that has fetched the content but
  failed to write it to disk (locked target, failed export, a self-heal that never
  completed) therefore reported a full 100% over a stale file. That is a second,
  independent way for `Healthy 100%` to sit on top of wrong bytes, and it is what
  would have hidden the receiving half of this report.

**Fix.**

1. `scan::signature_map` now returns the per-path `(size, mtime)` the walk saw, and
   `scan::settled_signature(before, after, wrote)` computes the settled value from
   only what the pass can vouch for: paths **we** wrote (absorbed — that is the
   churn the signature exists to prevent) and paths whose metadata is unchanged
   since the pass's opening walk. Anything else drifted under us: it is excluded and
   the result is domain-separated with a drift tag, so the next pass's plain walk
   *cannot* compare equal and the full scan is forced. Mid-pass deletes need the tag
   rather than exclusion — there is no surviving path to exclude, and without it the
   next walk agrees and the delete never propagates. The reconcile tracks `wrote`
   across every disk-mutating branch of the merge.
2. Health is now a claim about the **mirrored folder**, not the blob store: a path
   scores full credit only when the post-pass index says the file on disk carries
   the manifest's hash *and* the blob is complete. Otherwise it scores its fetched
   bytes, capped strictly below full — an unwritten file can no longer reach 100%.
   Paths we published or tombstoned this pass are excluded from the comparison,
   since the pass-start remote view is stale for exactly those.

**Tests.** `crates/seed-core/tests/live_folder.rs` — a suite for writes that land
*during* a pass, delivered deterministically through the new
`ReconcileJob::debug_before_settle` seam rather than by racing a background writer:
mid-pass overwrite (different size and same size), create, delete, a file rewritten
across four consecutive passes, and the co-master path. Each also asserts
`assert_healthy_nodes_agree` — any two nodes claiming 100% on the same manifest must
hold byte-identical folders. Unit coverage of the settle rule itself is in
`scan.rs` (`settled_signature_*`).

**Open sub-question: the deep verify did not rescue it either.** The 4-hourly
`periodic_deep_verify` is the designed safety net for exactly this state — it sets
`force_deep_verify`, which makes the next pass scan regardless of the signature, and
a forced scan of a file whose disk hash differs from its base *must* reach the
"remote untouched, local changed → publish local" branch. The log shows one firing
at 19:30:43 UTC, 11 minutes after the overwrite, and the pass completing in 83 s —
but no import happened (no new blob, base unchanged), and 83 s is exactly the
duration of every other deep-verify pass on this share going back days, i.e. the
duration of a pass that finds nothing to do. Either the force wasn't applied to the
pass that ran, or the forced pass didn't scan. **Not explained.** The fix above
addresses the cause of the stuck state, not this failure of the net beneath it;
`ShareState::full_scans` (`debug_full_scans`) is the counter to instrument next, and
it is not currently reachable over IPC. Worth resolving before relying on the deep
verify as a backstop for anything else.

**No user-facing recovery existed.** `Engine::publish` — behind the GUI's Publish
button and `seed-cli publish` — is just `reconcile`, which honours the signature
gate. So the one control a user would reach for when a file isn't syncing could not
fix this state. The workaround, before the fix, is to change *anything* else in the
folder: that moves the signature and forces the full scan.

**Note on test coverage.** The pre-existing `inplace_overwrite_large_file_converges`
covers overwrite-*between*-passes and passed throughout; the whole class of
mid-pass mutation was untested. Every integration test in `seed-core` is also
`#[ignore]`d and so does not run under a plain `cargo test --workspace` — see
`docs/testing.md`.
