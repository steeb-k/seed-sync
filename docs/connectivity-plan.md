# Two members that cannot see each other: investigation and the plan for a real fix

**Date:** 2026-09-04. **Share:** `8318bd1b…` (bigDev ↔ xpsTop, both masters).
**Status (2026-09-04, later the same day):** steps 3 (transport-level repair
ladder), the CLI half of step 1, and the Windows firewall/service-recovery
provisioning are implemented and shipped as v0.7.4 — see known-issues #36 for what
landed. Relay policy (step 2) was deliberately left alone at the maintainer's
request: members must work regardless of how each one's relays are configured.
Steps 0 (capture the wedge with `iroh=debug`), 4 (roaming-peer soak) and 5
(upstream) remain open.

This is the third round on the same symptom (known-issues #23 in July, #35 in
August). This time the goal is not another heuristic on top of the last one; it is
to explain why each previous round looked fixed and was not, and to change the
approach so the next failure is diagnosable in one pass.

## 1. What is actually happening (evidence from the live fleet)

All evidence is from bigDev's installed daemon (`C:\ProgramData\SeedSync\daemon.log`,
service up since 2026-08-26 20:33 UTC, v0.7.3) plus live probes on 2026-09-04.

1. **The daemon has been cycling through the self-heal ladders every ~6 minutes
   for days.** Since the 08-26 restart the log is ~2000 lines/day of exactly eight
   message kinds: `PARTITIONED`, `presence overlay silent … rebuilt`, `rebuilt
   presence subscription`, `re-kicked doc sync`, `rendezvous … no reachable
   member`, `suspected blackhole; adding the public relays`, `members reachable
   again`, `custom relay reachable again`. Heavy flapping started 08-29 22:00 UTC,
   three days after the restart; before that there were only occasional episodes.
2. **Presence from xpsTop has not arrived for hours** — the ladder-2 gap counter
   grows monotonically (828 s → 5400 s+) *through* a rebuild every 90 s. The
   rebuild does nothing. `peer_names.last_seen` for xpsTop was last persisted
   2026-08-25 21:32 UTC (unexplained: presence *was* received this session at some
   point, since the roster shows `seqno=5 100%`; the flush path deserves a look).
3. **Every outbound dial from the daemon to xpsTop times out.** The partition WARN's
   `last dial error` is always `Failed to establish connection … timed out`.
4. **Inbound contact from xpsTop does happen, in single events ~25–30 s apart.**
   `seed-cli peers` sampled every 5 s shows xpsTop `online=true path=direct` for
   exactly one 20 s TTL after each event, then offline. So the "reachable" windows
   that end each partition episode are xpsTop reaching *us*, briefly, never us
   reaching it.
5. **xpsTop is not on this LAN.** Its n0 DNS record homes it on the public relay
   `usw1-1.relay.n0.iroh.link`; its live addresses are `68.14.236.66:42730`
   (remote public IP) and `10.99.0.9:42730` (the Nullgate overlay; bigDev is
   `10.99.0.3` on the same overlay). `xpstop.lan → 192.168.50.142` in the router's
   DNS is a stale lease. The August "LAN re-test" premise was wrong.
6. **The control experiment.** `cargo run -p seed-core --example dial_probe` (new,
   kept in-tree) binds a *fresh* endpoint on bigDev and dials xpsTop:
   - public relays, docs ALPN: **connected in 0.3 s**, direct path `10.99.0.9` active
   - **daemon's exact relay config** (iroh03 + token, read from `state.db`), docs
     ALPN: **connected in 0.3 s**
   - gossip ALPN: **connected in 0.5 s**
   - with 40 unroutable junk candidate addresses added: **connected in 1.3 s**
   - fresh endpoint → the *running daemon* (our own id): **connected in 0.0 s**

   So the network, the relay configuration, the token, xpsTop's firewall and
   bigDev's inbound all work *right now*. The one thing that cannot reach xpsTop is
   the 9-day-old daemon endpoint. **The fault is state inside the long-running iroh
   endpoint, not the network and not the engine's roster logic.**

What the wedge is, exactly, is not yet captured (the daemon's log filter excludes
`iroh` entirely, so nothing below `seed_core` is visible). From the vendored iroh
1.0.3 source the plausible sites are: the per-remote `RemoteStateActor` (holepunching
is skipped while the candidate set is unchanged; the #4390 path-lifecycle gap means
unreachable candidate paths are never abandoned), and the non-home `ActiveRelayActor`
for xpsTop's relay (created on demand, reaped after 60 s idle). Step 0 captures it.

## 2. Why the previous fixes did not work, and what they cost

Every mechanism added in July and August operates *above* the transport:

| Mechanism | What it does when the endpoint is wedged |
|---|---|
| Ladder 1 (isolation → force public-relay fallback) | Flips the relay map every few minutes; each flip re-runs net-report. No effect on the wedged remote state. Manufactured the "suspected blackhole" / "relay reachable again" log lines that sent the July *and* August diagnoses after the relay. |
| Ladder 1 rung 2 / ladder 2 (rebuild presence subscription, re-kick doc sync) | A gossip re-subscribe is a local actor hand-off; the re-kick is another dial that times out. Fires every 90 s forever (the gap counter proves it never heals). |
| #35 hysteresis (`EpisodeClock`) | Correctly keeps the ladders armed under a flap — so they now fire *reliably* and *uselessly*, producing the 6-minute cycle. |
| Rendezvous dial, presence rejoin, diverged re-sync | More dials into the same wedged remote state. |

None of them is wrong in isolation; none of them touches the thing that is broken.
The tell is in the history: **every round was validated right after a daemon
restart** (July: fixes shipped and re-tested after restarts; August: "60/60 online,
54/54 direct" measured on a just-restarted service), and a restart is exactly what
clears the real fault. The daemon then degraded again over 1–3 days each time. The
ladders were built to be "restart-equivalent" but never restart the one component
that needs it.

They also actively obscure the problem: ~2000 log lines a day, three of which
literally name the wrong culprit (`PARTITIONED`, `suspected blackhole`, `custom relay
reachable again`), while the one measurement that would have settled it (a fresh
endpoint can connect; this one cannot) had no tooling.

Two independent defects also surfaced and should be fixed, but neither explains the
core failure (the probe succeeds with both in place):

- **Firewall profile gap on bigDev.** The 0.7.3 auto-update *did* run the MSI's
  firewall custom action (its msiexec log shows both exceptions installed under
  the single name `SEED Sync daemon`, the second replacing the first), so the one
  rule on the box is the MSI's — and it covers **Private only**. The Nullgate
  overlay adapter is categorised **Public**, so unsolicited inbound from xpsTop
  over `10.99.0.9` was not covered by any rule for the installed binary. That
  plausibly makes xpsTop→bigDev contact bursty (allowed only as stateful replies
  to our own sends). Fixed in 0.7.4: the MSI adds a Public exception and the
  service re-asserts an all-profile rule for its own path on every start.
- **Log filter blind spot.** The default filter `seed_daemon=info,seed_core=info`
  drops every `iroh*` WARN. Relay-connection failures, path errors and gossip
  actor errors are invisible in production.

## 3. The plan

Ordered so that each step is verifiable on the live fleet before the next.

### Step 0 — capture the wedge (no code change)

1. On xpsTop, before anything else: `seed-cli peers`, `seed-cli relays`, its
   daemon log for the last day, `sudo ufw status`. We have only one side's view.
2. On bigDev, set the service environment (`HKLM\SYSTEM\CurrentControlSet\Services\
   SeedSyncDaemon\Environment`, REG_MULTI_SZ) to
   `RUST_LOG=seed_daemon=info,seed_core=info,iroh=debug,iroh_gossip=debug,iroh_docs=info`
   and restart the service. Expected: xpsTop online continuously within a minute
   (this is the restart that has "fixed" it every time). Leave it running.
3. When the flap returns (history says 1–3 days), the log will show for remote
   `7e533d17` which paths were tried, the relay actor's state, and whether
   holepunching was skipped. That decides which rung in step 3 is the right one
   and gives the upstream report its evidence. The `dial_probe` control experiment
   is re-run at the same time.

### Step 1 — observability, so the next failure is a one-pass diagnosis

- Extend `PeerInfo` (seed-ipc) with transport facts, filled from the engine and
  `Endpoint::remote_info`: `last_dial_ok` (ts + path), `last_dial_err` (ts + text),
  `last_presence_rx`, `last_doc_sync_ok`, `addrs` (addr, usage). `seed-cli peers`
  prints them; the GUI member row gets a tooltip / details pane.
- Add `seed-cli dial <peer-id>` → `IpcRequest::DialProbe`: the daemon binds a
  throwaway endpoint with its own relay settings and dials the peer, exactly what
  `dial_probe` does, and returns the result next to the daemon's own last dial
  outcome. This is the control experiment as a first-class tool: "fresh endpoint
  OK / mine times out" is the wedge signature; "both fail" is the network.
- Default log filter gains `iroh=warn,iroh_gossip=warn,iroh_docs=warn`, and a
  `SEED_LOG` override is read from a file in the data dir at startup so a service
  can be put into debug without registry surgery.
- Replace the per-tick ladder chatter with one INFO line per **per-peer**
  transition (`peer X reachable via direct/relay` ↔ `peer X unreachable: <last
  error>`), and one line per repair action taken.

### Step 2 — remove what adds noise without repairing

- **Relay policy becomes static.** `Preferred` keeps the public relays in the map
  permanently; the custom relay is preferred by the existing path selector.
  Delete `force_relay_fallback`, the watchdog's add/remove dance and the two
  misleading log lines. The watchdog survives only as a periodic reachability
  probe for the GUI ("your relay has been unreachable since …"). `Only` mode is
  unchanged.
- **Ladder 2 rebuilds at most once per episode**, then escalates to step 3. A
  rebuild that did not help within one TTL will not help on the 40th try.
- **Ladder 1's public-relay rung goes away** with the static policy; its WARN
  becomes the per-peer transition line above.
- `EpisodeClock` stays (it is correct); it now feeds the per-peer state machine
  instead of the ladders.

### Step 3 — repair the actual fault: a transport-level ladder

Per peer, driven by "this member is alive (rendezvous record published within
2 × `REPUBLISH_SECS`, or presence/doc contact from it within a minute) but every
dial from us has failed for `N` minutes":

1. **Rung A — drop our state for that remote.** Close every connection we hold to
   the peer (docs `leave` + `start_sync` again, gossip quit/rejoin for its topics),
   wait past iroh's 60 s remote-actor idle timeout so the `RemoteStateActor` is
   reaped, then redial with a full `EndpointAddr` from the rendezvous record.
   Cheap, in-process, and whether it suffices is exactly what the step-0 capture
   tells us.
2. **Rung B — rebuild the iroh endpoint in-process** (the item deferred in July).
   `IrohNode` is re-spawned (same `node.key`, same blob store, fresh
   endpoint/gossip/docs), every share is re-opened, IPC and GUI state survive.
   This is what a service restart does, without the restart.
3. **Rung C — exit for the supervisor.** If rung B cannot be made safe quickly,
   the daemon exits with a distinct code and the service manager restarts it
   (Windows service recovery / `systemd` `Restart=on-failure`). Downloads resume
   (#21). This rung is worth having regardless, as the last resort.

The trigger deliberately requires evidence the peer is alive, so a genuinely
offline fleet never churns the endpoint.

### Step 4 — tests that would have caught this

- **Roaming-peer integration test** (tier 1, `#[ignore]` like the rest): peer B
  rebinds to a new port / changes its home relay / drops and re-adds an overlay
  address N times over the run; assert A re-establishes a fresh connection and
  presence within X s after each change, and that the per-peer transition log
  fires exactly once per change. This is the only test shape that exercises a
  *long-lived* endpoint against a *changing* remote, which is the field topology
  (a roaming laptop over an overlay).
- Unit tests for the per-peer state machine (trigger only when the peer is
  provably alive; rung escalation; single rebuild per episode).
- The acceptance gate gains a soak variant that runs two daemons for 24 h with B
  restarting hourly, graded on "A never reports B unreachable for > 2 min while
  B is up".

### Step 5 — upstream

With the step-0 capture and the roaming test as a repro, file against iroh 1.0.3
(`RemoteStateActor` holepunch/path lifecycle, cross-referencing #4390's closing
note). Keep our rung A/B regardless: a sync daemon must survive a transport-layer
bug without a human restarting it.

## 4. What is *not* the cause (so it is not re-litigated)

- The custom relay `iroh03`. A fresh endpoint homed on it reaches xpsTop in 0.3 s.
- The token. Same experiment.
- xpsTop's firewall. Its overlay address accepts our probe's unsolicited initial.
- The relay map lacking the peer's relay. iroh connects to a remote's relay URL on
  demand whether or not it is in the map (`relay/actor.rs`,
  `active_relay_handle`); only the auth token lookup depends on the map.
- Stale candidate addresses on their own. 40 junk addresses cost 1 s on a fresh
  endpoint.
- The GUI. Pause/unpause only resets the heal clocks; the brief recovery after it
  is xpsTop's side re-dialling.

## 5. Open questions

- `peer_names.last_seen` for xpsTop stuck at 2026-08-25 although presence was
  received this session: is `flush_peer_names` losing writes?
- Whether xpsTop's daemon shows the mirror image (its dials to bigDev timing out
  while bigDev's inbound works). Step 0 item 1.
- Whether the Public-profile gap on the overlay adapter is what makes xpsTop's
  inbound contact bursty. Testable with a subnet-scoped rule while the daemon is
  otherwise healthy.
