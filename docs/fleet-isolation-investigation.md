# Investigation plan: fleet-wide silent isolation with phantom "Syncing" flapping

**Symptom (reported 2026-07-21, ongoing):** files added on one master never reach the
other members. The GUI on every online member flaps in a loop between "members
syncing" and "all members offline"; each node reports every member unhealthy except
itself. Opening the GUI *appears* to eventually get things moving, but no files
actually transfer — only restarting services does anything. Two of four members are
online with no real connectivity change on either.

> Status: **narrowed by live experiment (2026-07-21, below) to "the custom-relay
> path on a long-lived endpoint blackholes all connectivity while looking
> connected".** The relay server itself and the path selector are exonerated for
> fresh endpoints. Root cause of the blackhole (server routing state vs client
> relay-actor state) not yet pinned — needs the instrumented re-add experiment.
> Fix list at the bottom is actionable now.

## Live experiments (2026-07-21, on the still-sick installed daemon)

Run while the daemon on bigDev was in its 2.5-day isolated state — the sick
process itself was the test subject.

1. **Relay server exonerated for fresh clients.** `relay_probe` (the example,
   extended with a `--selector` flag) against `iroh03.kznjk.com:8443` **with the
   real token**: all four phases pass — WS+token connection, QUIC address
   discovery, an actual **data round-trip through the relay** between two fresh
   endpoints, and token-less rejection. Matches the report that another iroh app
   (Nullgate, `~/iRohDP`) uses the same relay without issue.
2. **`PreferMyRelaySelector` exonerated (solo).** Same probe with the production
   selector installed on both endpoints (preferred = iroh03): all phases pass.
3. **The smoking gun.** With the daemon still isolated *at that very moment*
   (its rendezvous retries failing while the probes above succeeded), a live
   `seed-cli relay-remove` — no restart, same endpoint, same process — produced
   **first successful peer contact 49 seconds later** (lilDev, `path=direct`).
   The user had observed the same on the other members: removing the relay got
   them "immediately syncing".
4. **iroh's relay client pings every 15 s** and tears down on ping timeout
   (`PING_INTERVAL`, `RunError::PingTimeout` in
   `vendor/iroh/src/socket/transports/relay/actor.rs`) — so the session was
   genuinely ping-alive the whole time. The blackhole is in *forwarding/
   coordination*, which pings don't exercise: with iroh03 configured, relayed
   dials AND hole-punched direct paths (whose coordination rides the relay)
   all fail, while `home_relay_status().is_connected()` stays true and the
   fallback watchdog therefore never fires.

5. **Post-heal residue: the gossip overlay stays dead.** After the relay
   removal, doc-sync connections form freely (direct paths, real transfers) —
   but every peer still shows `seqno=0`: **no presence heartbeat is ever
   heard**, so peers age out on the 20 s TTL between transient sync sessions
   and the member list keeps flapping online↔offline (~every 15–30 s, observed
   for 4+ minutes; even winARM-LT flickered through). The long-lived endpoint's
   iroh-gossip presence overlay does not re-form on its own after prolonged
   isolation, despite `presence_rejoins` running every tick — only a service
   restart (fresh gossip subscriptions) fully heals. So the incident wedges
   TWO layers with different recovery behavior: the relay path (healed live by
   the settings change) and the gossip overlay (healed only by restart). The
   self-heal ladder in the fix list must therefore include gossip
   re-subscription, not just presence re-joins.

Remaining unknown — which end holds the rotten state:
- **Server-side:** iroh-relay per-client routing state gone stale for
  long-established / reconnected endpoint ids (fresh ids work, which would
  explain both probes and Nullgate). Check iroh03's iroh-relay version and logs.
- **Client-side:** the relay transport actor's forwarding path wedged in the
  long-lived endpoint (note the `UNDELIVERABLE_DATAGRAM_TIMEOUT` "Dropping
  datagrams to send" path in the same actor) — though this alone doesn't explain
  why *inbound* relayed dials from peers also failed during the incident.

## Evidence (bigDev, installed 0.6.7, `C:\ProgramData\SeedSync\daemon.log`, 7/15–7/21)

1. **This node has been fully isolated since its service restart at 2026-07-19
   02:23 UTC** — 2,488 `rendezvous: share … has no reachable member; bootstrapping
   from master …` lines, one every ~60–120 s, continuously through to the moment of
   this investigation. Before that restart (7/17 21:02 UTC) it had live contact
   ("2 online peer(s) disagree"). The engine itself is healthy the whole time:
   reconcile passes commit, periodic deep verifies run on schedule.
2. **The peers are alive and have internet.** The pkarr rendezvous record this node
   resolves is *fresh* and alternates between the other two masters (`1f4137766c` /
   lilDev early on 7/21, `0f4b6c4987` / steebP14s later) — they are republishing
   every 120 s successfully. Publishing and resolving against n0's pkarr server
   works from every side. **Only direct node-to-node connections never form.**
3. **Every bootstrap dial fails silently.** ~2,500 `doc.start_sync(addr)` calls
   returned `Ok` (only 2 warnings in the entire 47 MB log) — iroh reports dial
   failure asynchronously as a failed `SyncFinished` event, and nothing logs those.
   There is currently **zero diagnostic output for the actual failure.**
4. **The phantom "Syncing ↔ all offline" loop is our own failed dials.**
   `LiveEvent::SyncFinished` fires on *failed* syncs too (already noted in
   known-issues #16 for the phantom-member case), and the doc-event task counts it
   as a sign of life: `roster.note(&peer, None)` refreshes `last_seen`
   (`engine.rs` doc-event task). So each failed rendezvous dial marks that master
   **online for `PEER_ONLINE_TTL_SECS` (20 s)**, after which it ages out — until
   the next dial ~60–120 s later. Every isolated node runs the same loop, so every
   GUI shows members flickering online/"Syncing"/offline with no real connectivity
   anywhere. The GUI-open correlation is an observation effect (status is only
   visible when the pane is open); the user's follow-up confirms no files move.
5. **All members home on the custom relay** `https://iroh03.kznjk.com:8443`
   (token set, mode `Preferred`, per `seed-cli relays` and `state.db`). The relay
   fallback watchdog has **never fired once in the whole log** — the endpoint
   believed a custom-relay connection was up continuously. A token-less
   `relay-test` from the daemon confirms the server answers its unauthenticated
   discovery services ("Discovery only. No relay connections" — expected without a
   token; the authenticated probe was not run in this session).
6. **This node's published address contains no public IP** — decoding its
   `node-addr` ticket yields the relay URL + three private/LAN addresses
   (172.27.32.1 virtual adapter, 192.168.50.1, 192.168.50.209). Cross-LAN
   reachability therefore rides ~entirely on relay forwarding (plus
   relay-coordinated hole-punching).
7. **Status output is lying while this is happening.** With all 3 members
   offline for days and pending remote files it knows nothing about, `seed-cli
   list` reports the share `Healthy 100%` (the phantom-life flapping keeps
   defeating the #17 `NoPeers` state, and health-of-what-we-know reads 100% when
   the missing files' doc entries never arrived).

Timeline note: the custom-relays feature (including the always-installed
`PreferMyRelaySelector` path selector) landed 2026-07-10 and reached the fleet in
the 0.6.5→0.6.7 updates on 7/17 — i.e. **new connection-layer code shipped fleet-wide
days before the onset.** The relay `iroh03.kznjk.com` is a token-protected test
server on the same LAN as bigDev.

## Hypotheses, ranked

- **H1 — relay-path blackhole.** Everyone's home relay is iroh03; if its
  client↔client forwarding is broken while its connection acceptance and discovery
  services still answer, the whole fleet goes mutually unreachable while every node
  believes its relay is fine. Sub-cases: (a) zombie websockets — the server dropped
  client state (restart, proxy/NAT timeout) but clients still read "connected";
  (b) the server forwards nothing (bug/overload); (c) token auth accepted for the
  connection but traffic dropped; (d) hairpin problems for the on-LAN client.
  Fits: fleet-wide symmetry, "no connection change", restart-helps (fresh
  connection), watchdog never firing (its `connected_custom` check trusts
  `home_relay_status`, which is exactly the thing that would be wrong).
- **H2 — no public addresses in published records.** If every member's record
  carries only LAN addrs + the relay URL (as bigDev's does), then whenever the
  relay path is sick there is *no* fallback path at all, and hole-punching (which
  needs the relay for coordination anyway) can't rescue it. This is an amplifier of
  H1 more than an independent cause — but worth confirming whether QAD via the
  custom relay is populating observed public addresses at all.
- **H3 — `PreferMyRelaySelector` mis-selection.** The custom path selector is
  installed unconditionally and is new fleet-wide code. A bug that fails to select
  a usable path on fresh connections (e.g. around paths with missing stats) would
  produce exactly "dials never complete, everything else healthy". Needs an audit
  against iroh's default selector semantics + a two-machine test.
- **H4 — gossip join can't dial.** `RendezvousDial` hands the full resolved
  `EndpointAddr` to `doc.start_sync` but only the bare endpoint **id** to the
  presence gossip join. If the id isn't resolvable via discovery at that moment,
  presence can't even try. Wouldn't explain doc-sync failing, but would keep
  presence dead even when doc sync limps.

## Diagnosis steps (in order of information-per-minute)

1. **Look at the relay server** (LAN access available): uptime, logs, connection
   count, restarts around 7/17–7/19. Then restart it while two isolated members are
   watching each other. Heals fleet-wide with no client restarts → server-side
   state (H1a/b). No change → client-side zombie or path selection.
2. **Authenticated probe from the daemon:** `seed-cli relay-test --url
   https://iroh03.kznjk.com:8443 --token <token>` (token is in `state.db`
   `relay_settings`). Proves the full WebSocket+auth handshake from this box.
3. **Discriminating dial test:** from this box, bind a *throwaway* endpoint with
   the same relay settings, resolve the rendezvous record, dial the resolved master
   (adapt `crates/seed-core/tests/rendezvous.rs`). Fresh endpoint connects while
   the long-running daemon can't → zombie long-lived endpoint state; both fail →
   relay/path problem, go deeper with step 4.
4. **Debug capture on two machines:** restart daemons with
   `RUST_LOG=seed_core=debug,iroh=debug,iroh_relay=debug,iroh_gossip=info` and
   capture one failing dial cycle from both ends; look at path candidates, selector
   decisions, relay frames sent/received.
5. **Bisect the custom-relay feature:** clear relay settings on two members (falls
   back to n0 defaults). If they immediately see each other, H1/H3 confirmed and
   narrowed; re-add settings with mode `only` vs `preferred` to separate selector
   from server.

## Implementation status (2026-07-21)

Landed in `seed-core` / `seed-daemon` (build + `seed-core` lib tests green):

- **Fix #1 — phantom liveness [DONE].** The doc-event task now inspects
  `SyncEvent.result`: a `SyncFinished` only refreshes roster liveness when the
  sync actually **succeeded**. A failure records a diagnostic (`last_sync_err`)
  and never touches `last_seen`, so an unreachable peer ages out and stays out.
  New `PeerRoster::note_sync_finished` + `last_contact` tracking. Acceptance test
  `failed_sync_never_marks_a_peer_online` pins it (zero online-flaps from failed
  dials). This also restores honest status (#6): with no phantom online peers,
  `isolated()`/`NoPeers` reports the partition instead of `Healthy 100%`.
- **Fix #2 — visibility [DONE].** Failed syncs log at debug (`doc sync with … failed`);
  the partition self-heal emits one loud WARN per episode naming the contradiction
  ("cannot reach any of N known members for Ns; last heard a peer …; last dial
  error …"). `last_sync_err`/`last_contact` are the diagnostic surface. (Surfacing
  "last contact" in the IPC `list`/GUI/tray is deliberately deferred — it touches
  seed-ipc/gui/mobile and the WARN + honest `NoPeers` already make the incident
  visible.)
- **Fix #3 — self-heal ladders [DONE].** New `Engine::connectivity_recoveries`
  (driven every ~6s from the presence loop), two independent ladders:
  - *Ladder 1 — total isolation (transport dead).* A share isolated past
    `ISOLATION_HEAL_SECS` (120s) forces the public-relay fallback and WARNs; past
    `ISOLATION_PRESENCE_REBUILD_SECS` (210s) it rebuilds the presence subscription
    and re-kicks doc sync.
  - *Ladder 2 — presence overlay dead while transport is alive* (the "doc-sync
    works / gossip dead" case, see below). Keyed on presence staleness rather than
    isolation: transport fresh (`last_contact` within
    `PRESENCE_TRANSPORT_FRESH_SECS`=60s) but no presence heartbeat within
    `PRESENCE_HEARD_TTL_SECS` (20s), sustained for `PRESENCE_GAP_HEAL_SECS` (90s),
    → rebuild the presence subscription (throttled by `PRESENCE_REBUILD_MIN_SECS`).
    Presence-vs-transport is now tracked separately in the roster
    (`last_presence` vs `last_contact`), because a successful doc-sync marks a peer
    online and would otherwise hide the dead overlay from ladder 1. Pure decision
    `presence_overlay_dead` is unit-tested
    (`presence_overlay_dead_only_when_transport_alive_and_presence_silent`).

  **Deferred:** in-process endpoint rebuild — higher risk, and the two ladders
  cover both observed failure modes.
- **Fix #4 — watchdog hardening [DONE].** `relay_watchdog` takes a
  `force_fallback` signal; while set (by #3) it treats the custom relay as
  unusable and adds the public relays **even though `is_connected()` is true** —
  the blackhole case. Honored only in `Preferred` mode. This is the automatic,
  reversible equivalent of the manual `relay-remove` that healed it live.
- **Fix #5 H4 [DONE].** `RendezvousDial` now runs `doc.start_sync` (carrying the
  full resolved address) **before** the gossip presence join, seeding the
  endpoint's remote map so the join-by-id is dialable.

Not yet done: the two-machine instrumented re-add capture to pin server-vs-client
root cause (diagnosis step 4), the soak anomaly-detector signature, and the
IPC/GUI "last contact" surface.

## Fixes (ordered; the first two are unconditional)

1. **Stop counting failed syncs as life** (the phantom-flap bug). In the doc-event
   task, only `note()` a peer for `SyncFinished` when the sync actually succeeded
   (inspect the event's result field); failed attempts should at most update a
   "last attempted, last error" diagnostic surface. Kills the fleet-wide fake
   "Syncing ↔ offline" loop and stops phantom liveness from suppressing `NoPeers`,
   pausing health-episode clocks, and feeding `live_providers` with dead peers.
2. **Make connection failure visible.** Log failed `SyncFinished` (peer, error,
   rate-limited); log rendezvous-dial outcomes end-to-end; add per-share "last
   successful peer contact" to `list`/GUI. A node that has resolved fresh
   rendezvous records (proof peers are alive) while establishing zero connections
   for N minutes should WARN loudly and surface it in the tray — that contradiction
   is the precise signature of this incident and is currently invisible.
3. **Self-heal instead of hand-holding (the actual complaint).** Escalation ladder
   on the "peers provably alive but nothing connects" signal: (a) force a relay
   reconnect / net-report re-probe; (b) rebuild gossip subscriptions + doc sync
   sessions; (c) last resort, rebuild the endpoint (the in-process equivalent of
   the service restart that empirically fixes it). This subsumes the
   sleep-resume plan's zero-online watchdog (`docs/sleep-resume-investigation.md`)
   — same family, different trigger.
4. **Harden the relay watchdog against half-dead connections.** `Preferred` mode's
   fallback keys off `home_relay_status().is_connected()`; add an end-to-end check
   (e.g. relay round-trip or "any successful peer traffic lately") so a zombie
   connection still triggers the public-relay fallback.
5. **Root-cause fix per diagnosis outcome** (server fix for H1, record/QAD fix for
   H2, selector fix for H3, pass the full addr to the gossip join for H4 — the H4
   one-liner is cheap and safe regardless).
6. **Status truthfulness:** `NoPeers`/isolation must win over vacuous `Healthy
   100%` once phantom liveness is gone; consider showing "N members unreachable
   for <duration>" rather than a percent when nothing has connected.

## Acceptance tests

- 3-node pool on the custom relay; **stop the relay mid-sync** → all nodes fall
  back to public relays within ~1 min and keep syncing; restart relay → traffic
  returns to it (watchdog both directions).
- **Zombie-connection sim** (firewall drop of the established relay TCP flow, no
  RST) → node detects blackhole and re-homes without a service restart.
- Failed dials produce **zero** roster online-flaps (pin with a unit test on the
  doc-event → roster path).
- The soak harness's anomaly detector flags "fresh rendezvous record + 0
  connections for 10 min" as a failure signature.

## Notes

- The "very small PNG files never arrived" report is this same incident, not a
  size-related bug: the doc entries carrying them never reached this node because
  no connection has formed since 7/19.
- Immediate operational unblock (no code): restart the iroh03 relay, then restart
  daemons on the affected members — and note which of the two steps was actually
  needed, since that's diagnosis step 1 for free.
