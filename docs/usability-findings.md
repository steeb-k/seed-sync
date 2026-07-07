# Usability & robustness findings

Findings from the production-readiness audit (see
`production-readiness-plan.md`), distinct from the engine-logic bugs in
`known-issues.md`. Each entry notes where it lives and its disposition.

## 1. Dead IPC event variants: `ShareStatus`, `Membership`
**Where:** `crates/seed-ipc/src/lib.rs` (`IpcEvent`), GUI arms at
`crates/seed-gui/src/main.rs:140-142`.
The daemon only ever emits `ShareListChanged`, `LastUpdated`, and `Throughput`.
`IpcEvent::ShareStatus` and `IpcEvent::Membership` are defined and matched but
never sent — dead wire surface that misleads readers about what the daemon
pushes. **Disposition:** removed in Phase 6 alongside adding `PeerHealth`.

## 2. `ShareStatus::Error` is never produced
**Where:** `crates/seed-ipc/src/lib.rs`, display mapping in
`seed-gui/src/main.rs` and `seed-mobile/src/lib.rs`.
`list_summaries` never assigns `Error`; a failed reconcile just logs and leaves
the share on its previous status, so a persistently failing share can keep
reading "Healthy". **Disposition:** variant removed in Phase 6; a real
error-surfacing design is future work (the `retrying` count covers the
common locked-file case).

## 3. Settings IPC is a stub and the GUI never calls it
**Where:** `IpcRequest::GetSettings`/`SetSettings` +
`Settings { use_relays, custom_relay_url }` in `seed-ipc`; daemon handlers
return defaults; no preferences window exists in the GUI (the gear popover has
four fixed actions). **Disposition:** deferred — becomes the natural home for
a notification opt-out toggle later.

## 4. Presence is unsigned — health/name/fingerprint spoofable by members
**Where:** `crates/seed-core/src/presence.rs:17-21` (documented there).
Any share member can broadcast presence claiming another member's identity,
name, health, or fingerprint. File *content* stays safe (manifest is signed),
but after the peer-health feature this includes forging "healthy" or
triggering false unhealthy alerts on masters. **Disposition:** accepted risk
for v1 (members are trusted in the intended deployment); a signing pass over
presence is tracked as deferred work.

## 5. Full stat-walk per share every 750 ms
**Where:** `scan::quick_signature` called from every reconcile tick
(`seed-daemon` tick = 750 ms).
Each tick stat-walks the entire share tree. At thousands of files — and
especially ~28 daemons sharing one disk in the fleet soak — this is measurable
sustained CPU/IO. **Disposition:** measure in the Phase 8 fleet soak; if
significant, add adaptive backoff (e.g. stretch the scan interval while the
signature is stable, snap back on doc events).

## 6. Sub-second same-path multi-master writes are a real LWW race
**Where:** reconcile merge, `crates/seed-core/src/engine.rs` (~1243); see
`known-issues.md` #5.
LWW compares local file mtime against the doc record timestamp; two masters
editing the same path within <1 s (or with skewed clocks) can pick either
winner. Distinct-path concurrent writes are safe. **Disposition:** documented
limitation; tests use ≥1.1 s separation for same-path conflicts and the soak's
sub-second conflict scenario is observed, not asserted.

## 7. `SWARM_DEADLINE_SECS = 300` vs 3–6 GB ISOs over WAN
**Where:** `crates/seed-core/src/engine.rs` swarm constants.
A single swarmed blob attempt must finish in 5 minutes; a 6 GB ISO therefore
needs ~20 MB/s sustained per attempt. Retries resume via chunk ranges, so
slower links make progress across attempts, but with deadline-retry noise in
logs and status. **Disposition:** the full-size soak counts deadline retries;
if they recur on LAN-class links, scale the deadline with blob size
(tracked in the plan's deferred list).

## 8. Peer detail is poll-only while the flyout is open
**Where:** `seed-gui/src/main.rs:1338-1348` (GetPeers on flyout open + 2 s
timer). Fine today; noted because the peer-health feature adds per-peer data
that should stay fresh — Phase 6's event push covers the alerting path, so no
change needed here. **Disposition:** no action.
