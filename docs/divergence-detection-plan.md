# Plan: cross-member divergence detection (next patch)

Status: **design only** — not implemented. Companion to the reliability work in
`docs/distributed-downloads.md` and the gate-poisoning fix.

## Problem
A member's "health" answers a narrow, local question — *"do I hold the blobs for
my own manifest?"* — so three masters with **different** manifests can each report
"Healthy 100%" while genuinely disagreeing about what files exist. The gate-poisoning
fix removes one *cause* of that divergence, and the "honest local status" change makes
a node that's *locally* unsettled (skipping/retrying files) stop reading "Healthy".
But neither makes a node aware that it **disagrees with its peers**. That's this
patch: surface disagreement, and self-heal it.

## Approach: a manifest fingerprint, compared across members

### 1. Fingerprint
Each node computes a deterministic fingerprint of its **desired-file view** — the
latest-per-path `(path → content-hash)` set it already builds in
`read_remote_files`. Define it as BLAKE3 over the sorted `path \0 hex(hash)` lines
(include the empty-file markers; exclude in-flight/partial state). Two fully-synced
replicas produce the **same** fingerprint; any disagreement about which files exist,
or their content hashes, produces different ones.

Key property: the fingerprint is about the **manifest** (what files *should* exist),
not content download progress. A large file still downloading does **not** change the
fingerprint — that's "Syncing %", not "diverged". So this signal is specifically
"we don't agree on the fileset," which is exactly the failure we hit.

Cost: one extra hash over the merged view per reconcile (cheap; we already iterate it
for health). Recompute only when the view changes.

### 2. Broadcast + compare
Add the fingerprint to the presence payload (alongside name/health/seqno). Each node
keeps, per peer, the peer's last fingerprint and when it last changed. A node is
**in agreement** with a peer when fingerprints match.

### 3. Settling window (avoid false alarms)
Right after any change, members legitimately differ for a short time while the doc
propagates. So only flag **persistent** disagreement: fingerprints that have stayed
split for longer than a settling window (start ~30–60 s; tune) **and** with no active
content transfer in flight. Distinguish three states per share:
- **Healthy** — fingerprints agree, nothing retrying, content complete.
- **Syncing** — fingerprints converging, or content downloading, or files retrying.
- **⚠ Out of sync** — fingerprints have stayed split past the window.

### 4. Surface it
- Per-share status gets an "out of sync" variant (`ShareStatus`), shown distinctly in
  the GUI (e.g. a warning color + "Members disagree — N of M"); the CLI prints it too.
- The peers list shows each member's fingerprint (short) so it's obvious *which*
  member is the odd one out.
- Log it at WARN with the diverging peer ids + fingerprints.

### 5. Self-heal tie-in
On persistent divergence, don't just alert — act:
- Re-bootstrap the doc live-sync + presence mesh for the share (kick the existing
  rejoin path), in case replication stalled.
- Trigger a reconcile + per-file self-heal so each side re-pulls what it's missing.
- **Deep verify** (periodic, low frequency): recompute disk-vs-manifest by hashing,
  not just checking blob-presence — this catches the class the current health misses
  (e.g. a file deleted on disk but still in the manifest with its blob retained).
  Expensive, so run it rarely / on-divergence / on-demand, not every tick.

## Open questions to settle before coding
- Exact fingerprint domain: include tombstones? per-author vs merged-latest only?
  (Merged-latest is what users perceive; start there.)
- Settling-window value and whether it should scale with share size / peer count.
- Auto-recover aggressiveness: alert-only first, or re-bootstrap immediately? Lean
  alert + gentle re-bootstrap; avoid thrash.
- Multi-master vs viewer semantics (viewers can't write, so a viewer "disagreeing"
  is really "behind"; treat as Syncing unless stuck).
- How the GUI should present it without crying wolf during normal large syncs.

## Test plan
- 3-master loopback: make one master's manifest differ (e.g. a partition or a
  withheld update) and assert all three report "out of sync" within the window, then
  converge to Healthy once reconnected.
- False-alarm guard: a normal large-file sync must stay "Syncing", never trip
  "out of sync" (fingerprint unchanged during content download).
- Settling window: a fresh small change must not trip the alarm before it propagates.
- Deep-verify: a file deleted on disk but left in the manifest is detected and healed.

## Why staged after this patch
The fingerprint + UI + self-heal is a meaningful surface-area change (presence wire
format, a new status, GUI, settling heuristics) with real false-alarm risk if rushed.
The current patch already (a) removes the divergence cause we actually hit and (b)
stops a locally-unsettled node from lying about being Healthy. This patch adds the
cross-member guarantee on top, deliberately.
