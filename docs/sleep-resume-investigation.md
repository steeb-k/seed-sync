# Investigation plan: Linux device doesn't resume syncing after sleep

**Symptom (reported):** after the Linux laptop resumes from suspend, the sync
service does not start downloading from shares again until the service is manually
restarted. This was noticed alongside the distributed-download issue — and the two
very likely share a root cause (see "Interplay" below), so this is filed as a
companion to `docs/distributed-downloads.md` rather than an unrelated bug.

> Status: **DIAGNOSED + FIXED (2026-07-21).** Confirmed in the field and fixed as
> the "Likely fix" below predicted. See **Resolution** immediately below; the
> original investigation plan is kept underneath as the record.

## Resolution (2026-07-21)

**Confirmed in the field.** On the Linux laptop (`steebP14s`), a ~1.8 GB ISO was
stuck at "Syncing 67%" while the two always-on Windows desktops had shared it
instantly. Evidence:

- The laptop suspended **18 times in one day** (`s2idle`), some wake windows only
  ~4 min. The stuck blob's data file was last written at `16:49:36`; the kernel
  logged `PM: suspend entry` at `16:49:38` — the download froze mid-write two
  seconds later and never recovered.
- `rendezvous publish ... Error sending http request` WARNs cluster right after
  every `PM: suspend exit` — the network stack had not recovered on wake.
- Small files finished within a single wake window (so they synced fine); the large
  file could not, and iroh never re-established connectivity after resume — exactly
  hypothesis #1 (netwatch does not fire for `s2idle`, so the endpoint wakes with
  stale relay/holepunch state). Only a daemon restart cured it.

The "uploading, not downloading" symptom the user saw follows directly: during the
brief wake windows the node could not establish a stable *inbound* transfer of the
missing tail, but it could still serve the blobs it already held to the fleet.

**Fix (implemented).** The predicted resume hook:

- `Engine::on_resume()` (`crates/seed-core/src/engine.rs`): calls
  `iroh::Endpoint::network_change()` (rebind socket + re-home relay, bounded), then
  unconditionally rebuilds every active share's gossip presence subscription and
  returns a `DocResync` per share to re-kick doc live-sync. It is
  `connectivity_recoveries`' logic fired on the *known* resume edge rather than
  waiting for the isolation/presence-gap ladders to notice.
- `seed-daemon` `sleep_monitor_loop` (Linux, `cfg(target_os = "linux")`): subscribes
  to logind `org.freedesktop.login1` `PrepareForSleep`; on the resume edge (`false`)
  it calls `on_resume()` and spawns the resyncs off-lock. Best-effort — no system
  bus/logind ⇒ debug log + retry, never fails the daemon. Added `zbus` as a
  linux-only dep.

**Also hardened.** `Node::shutdown()` now calls `blobs.shutdown()` *before* the
router teardown, to flush partial-download verified-range bitfields to disk
(iroh-blobs runtime note — a plain `Router::shutdown` was leaving them ephemeral).
This lets a restart mid-download resume from what's on disk instead of re-validating
with a delay.

**A retracted mid-investigation claim.** A new test briefly appeared to show partial
progress was *lost* on restart (0% after reopen). That was a **measurement
artifact**: a just-restarted node has no peers, so `ensure_download` returns early
and nothing triggers the store to load/report the partial. The on-disk dump proved
the data *and* a non-empty verified-range bitfield persist correctly (e.g. 34 MiB +
41-byte bitfield after a 16 % freeze). There is no separate "progress lost on
restart" bug.

**Tests** (`crates/seed-core/tests/resume.rs`, `#[ignore]` — real endpoints):

- `partial_swarm_download_survives_restart` — a partial swarm download's data and
  verified-range bitfield survive an engine restart on the same data dir.
- `large_download_converges_despite_repeated_interruptions` — a large swarm download
  interrupted repeatedly (each abort mimics a suspend) still converges. This is the
  in-process synthesis of the suspend cycle: from the app's view a suspend is just
  "the in-flight transfer is cancelled and may resume against the same on-disk
  store." It does **not** reproduce the *stale-connection-after-`s2idle`* trigger
  (that needs a real suspend), which is why the resume hook's end-to-end proof is a
  single real suspend on the laptop.

---

## Original investigation plan (kept for the record)

> Status: **investigation plan only** — superseded by the Resolution above.

## Hypothesis

On suspend, the OS freezes the process and tears down the network underneath it:
QUIC sockets, the relay connection, NAT/holepunch mappings, and gossip neighbor
connections all go stale. On resume the process wakes with dead connections it
believes are live. The pieces that don't self-heal are the suspects:

1. **iroh endpoint / relay.** Does iroh 1.0's `netwatch` fire a network-change event
   on resume and rebind the socket + re-establish the home relay? Suspend/resume
   does not always look like a normal interface change on Linux, so this may not
   trigger. If the endpoint's relay/addresses are stale, peers can't reach this node
   and vice-versa.
2. **gossip subscriptions.** Doc live-sync and presence both ride iroh-gossip. If
   neighbor connections die on suspend and gossip doesn't re-bootstrap, the node is
   silently partitioned: no presence heartbeats in or out, no doc updates.
3. **the peer roster ages out.** Online status is heartbeat-based — presence is
   broadcast ~every 3 s and a peer ages to offline after `PEER_ONLINE_TTL_SECS`
   (20 s) without contact (`PeerRoster`, `crates/seed-core/src/engine.rs`). After a
   resume with dead gossip, every peer ages out and the roster goes empty.

A manual service restart fixes it because it rebuilds *everything* from scratch:
new endpoint, fresh relay, re-subscribed gossip, re-joined presence, re-bootstrapped
doc sync. That points at "some long-lived handle didn't recover," not at persistent
state.

## Interplay with distributed downloads

This is why it's filed here. Content downloads now pick providers from the **live**
roster at download time (`ReconcileJob::live_providers`). If the roster is empty or
shows only the master after a resume (because presence/gossip didn't recover), then:

- swarm needs ≥ 2 online peers → it won't trigger; and
- `live_providers` returns no peers → the download falls back to the **master only**.

So a half-recovered post-resume state would reproduce *exactly* the reported
"two members already had the file, only the master was hit" behavior. The flip side
is the good news: because discovery is now dynamic, **if** the roster recovers on
its own, the next reconcile tick will pick up the peers and downloads will
distribute again with no restart. That makes "does the roster repopulate after
resume?" the single most informative question to answer first.

## Diagnosis steps

1. **Instrument and reproduce on the laptop.** Run the daemon with
   `RUST_LOG=seed_core=debug,iroh=info,iroh_gossip=info,iroh_relay=info` and capture
   a full suspend→resume cycle. Look for, in order after resume:
   - any iroh netwatch / relay-reconnect log lines (did the endpoint notice?);
   - presence heartbeats resuming (peers re-entering the roster);
   - doc live-sync neighbor up/down events;
   - whether `peers()` / roster online count returns to non-zero on its own, and how
     long it takes (vs. the 20 s TTL).
   A quick scripted probe: after resume, poll the IPC `peers`/status every few
   seconds and log online counts — this alone answers "does it self-heal, and when?"
2. **Localize the stuck layer.** From the logs, determine which is true:
   (a) endpoint/relay never recovers (no addresses) → transport layer;
   (b) endpoint fine but gossip neighbors never come back → gossip layer;
   (c) gossip fine but our reconcile/roster logic doesn't act on it → our layer.
3. **Check iroh's own resume handling.** Confirm whether iroh 1.0 exposes a way to
   force re-discovery / relay reconnect (e.g. an endpoint "network change" nudge),
   and whether `netwatch` is expected to catch suspend/resume on Linux. This decides
   whether the fix is "tell iroh to re-evaluate the network" vs. "rebuild our gossip
   subscriptions."

## Likely fix (to validate after diagnosis)

- **Listen for resume and trigger recovery.** On Linux, subscribe to logind's
  `org.freedesktop.login1` `PrepareForSleep(false)` D-Bus signal (fires on resume),
  or drop a `systemd-sleep` hook, and on resume run a recovery routine: nudge the
  endpoint to re-evaluate the network / reconnect the relay, and re-bootstrap gossip
  + presence + doc sync for every share. There is already a periodic presence-mesh
  repair (`build_presence_rejoins` / `PresenceRejoin`, `crates/seed-core/src/engine.rs`)
  — confirm it also covers **doc** gossip, and if a resume signal is available,
  trigger that repair immediately instead of waiting for the next periodic pass.
- **Fallback if no resume signal:** a watchdog in the reconcile loop that detects a
  prolonged drop to zero online peers despite known share members and proactively
  re-bootstraps, rather than waiting passively.
- **Verify:** with the swarm fix already in place, the acceptance test is simply
  "after resume, peers reappear in the roster within N seconds and a pending
  download distributes across them, with no service restart."

## Notes for whoever picks this up

- Reproduce on the actual laptop — this won't show up in loopback tests (no real
  suspend, all localhost).
- The fix is in `seed-core`/`seed-daemon` (the daemon owns the resume signal
  subscription); the GUI is unaffected.
- Worth checking other platforms while here: macOS (`NSWorkspace` sleep/wake
  notifications) and Windows (`WM_POWERBROADCAST` / service `SERVICE_CONTROL_*`)
  likely have the same latent issue.
