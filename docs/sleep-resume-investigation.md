# Investigation plan: Linux device doesn't resume syncing after sleep

**Symptom (reported):** after the Linux laptop resumes from suspend, the sync
service does not start downloading from shares again until the service is manually
restarted. This was noticed alongside the distributed-download issue — and the two
very likely share a root cause (see "Interplay" below), so this is filed as a
companion to `docs/distributed-downloads.md` rather than an unrelated bug.

> Status: **investigation plan only** — not yet diagnosed or fixed. Next session.

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
