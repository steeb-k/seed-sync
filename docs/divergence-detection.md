# Cross-member divergence detection

A member's health answers a narrow, local question — *do I hold the blobs for my
own manifest?* — so three masters with **different** manifests can each report
"Healthy 100%" while genuinely disagreeing about what files exist. Divergence
detection adds the cross-member guarantee: surface disagreement about the fileset,
then self-heal it. It sits alongside the content-fetch reliability work in
`distributed-downloads.md`.

## Manifest fingerprint

Each node computes a deterministic fingerprint of its **desired-file view** — the
latest-per-path `(path → content-hash)` set it already builds in
`read_remote_files`: BLAKE3 over the sorted `path \0 hex(hash)` lines (empty-file
markers included, in-flight/partial state excluded). Two fully-synced replicas
produce the **same** fingerprint; any disagreement about which files exist, or
their content hashes, produces different ones.

The fingerprint is about the **manifest** (what files *should* exist), not
download progress. A large file still downloading does not change it — that's
"Syncing %", not "diverged". Cost is one hash over the merged view per reconcile
(cheap; we already iterate it for health), recomputed only when the view changes.

## Broadcast and compare

The fingerprint rides the presence payload (`Presence.manifest_fp`, serde-default
for back-compat) alongside name, health, and seqno. Each node keeps, per peer, the
peer's last fingerprint and when it last changed; a node is **in agreement** with a
peer when their fingerprints match.

## Settling window

Right after any change, members legitimately differ for a short time while the doc
propagates, so only **persistent** disagreement is flagged: fingerprints split
longer than `DIVERGENCE_SETTLE_SECS` (45 s) with no active content transfer in
flight. Three states per share:

- **Healthy** — fingerprints agree, nothing retrying, content complete.
- **Syncing** — fingerprints converging, or content downloading, or files retrying.
- **Out of sync** — fingerprints have stayed split past the window.

Tracking lives in `finish_reconcile` (`diverged_since` + the settle window); it
WARNs once per episode and clears on agreement. Two later fixes keep it from crying
wolf: a just-joined node advertises an "unknown" fingerprint until its replica is
real, and a fully-partitioned node reports `NoPeers` rather than judging an empty
peer set (known-issues #19 and #17).

## Surfacing

`ShareStatus::OutOfSync` shows in the GUI ("Out of sync — members disagree"), the
CLI, and as a per-peer fingerprint in `PeerInfo` so it is obvious which member is
the odd one out. It is logged at WARN with the diverging peer ids and fingerprints.

## Self-heal

On persistent divergence the engine acts, not just alerts. It re-kicks doc
live-sync for out-of-sync shares (`resync_diverged_docs` → `doc.start_sync`,
rate-limited) and rides the per-tick blob re-materialization each reconcile, and it
runs a periodic **deep verify** (`request_deep_verify`, a full hashing scan,
`DEEP_VERIFY_INTERVAL_SECS = 4 h`) to catch drift the change-signature misses —
in-place corruption with unchanged size+mtime, or a file deleted on disk while
still in the manifest. The self-heal plumbing had several sharp edges
(lock-held-across-await, a lost deep-verify, a full-rehash thrash on large shares);
see known-issues #1–#3.

Loopback tests: agreeing masters exchange equal fingerprints and never read
`OutOfSync`; deep verify heals same-size+mtime corruption a normal reconcile
provably misses; `resync_doc` doesn't break replication; plus the
`manifest_fingerprint` unit test.
