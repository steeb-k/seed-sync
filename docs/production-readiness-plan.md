# Production readiness plan (health tracking + multi-master validation)

The final push before production. Target topology: **2–3 masters serving 20–30
viewers**. Three deliverables:

1. **Engine bug fixes** — `known-issues.md` #2–#4, confirmed unfixed at `ef04a82`.
2. **Long-term peer-health tracking + notifications** — a member that is *online
   but degraded* (sync % < 100, or participating in an out-of-sync share) for
   **12+ hours** raises a toast/OS notification on itself **and** on every
   master, repeating every 8 h (≈2–3×/day) until healthy. Plain offline never
   alarms.
3. **Automated multi-master test suite + watched soaks** — near-simultaneous
   multi-master writes, thousands of 1 KB–100 MB files, ~6 × 3–6 GB ISOs;
   degraded-health reporting and notification delivery asserted end-to-end.

Status legend: `[ ]` todo · `[~]` in progress · `[x]` landed (commit)

## Phases

- `[x]` **Phase 1 — deep-verify force flag** (known-issues #2): explicit
  `force_deep_verify` on `ShareState`, cleared only when a forced scan
  *completes*; `last_deep_verify` advances on completion, not on request.
  (`ae42016`)
- `[x]` **Phase 2 — OutOfSync self-heal rescan policy** (#3): drop the 60 s
  full-rehash cadence; escalate to **one** deep verify per divergence episode
  after 10 min diverged (`DIVERGENCE_DEEP_VERIFY_SECS = 600`). (`e40b3cb`)
- `[x]` **Phase 3 — empty-marker vs content LWW** (#4): `read_remote_files`
  resolves a live content key vs live empty marker for one path by record
  timestamp with a deterministic tie-break, never stream order. (`bad7983`)
- `[x]` **Phase 4 — test harness**: `crates/seed-harness` (deterministic corpus
  generator, daemon-process helpers), shared in-process `Cluster` helpers under
  `crates/seed-core/tests/common/`, and the `multi_master.rs` suite — all six
  tests green, incl. 3-master concurrent burst + 1096-file convergence.
  (`d4dae31`)
- `[x]` **Phase 5 — peer health tracking**: persisted `peer_health` table,
  injectable `HealthPolicy` (12 h / 8 h / 24 h; `SEED_HEALTH_*` env overrides),
  `Engine::health_alerts()` detector (master-majority fingerprint consensus,
  pause-not-reset timer), `tests/health.rs` — 4/4 green.
- `[x]` **Phase 6 — IPC `PeerHealth` event + daemon emit**: new event +
  `GetPeerHealth` + `seed-cli peer-health`; daemon emits off-lock; dead IPC
  variants removed; `seed-daemon/tests/health_ipc.rs` green (both masters
  alerted over real IPC, recovery pair, split-brain self-alert).
- `[x]` **Phase 7 — GUI/OS notifications**: `notify-rust` backend +
  `adw::Toast`; self / remote / recovered copy; peers flyout shows the
  unhealthy duration. Manual visual check on Win + Linux still pending.
- `[~]` **Phase 8 — soaks**: `seed-soak` bin (fleet | fullsize | clean) built;
  full-size content soak (3 masters + 3 viewers, ~210 GB, real 3–6 GB ISOs)
  and fleet soak (3 masters + 25 viewers, scaled corpus) on `D:\` still to run,
  each with a committed report.

## Decisions log

- **Unhealthy definition** (user): online-but-degraded only — sync % < 100 or
  out-of-sync participation. Plain offline never starts the timer.
- **Timer semantics**: *pause-not-reset* — going offline pauses accrual
  (`accum_secs`), returning online resumes it; continuously offline > 24 h
  (`offline_reset_secs`) deletes the episode. A degraded peer can't dodge the
  12 h alert by bouncing.
- **Attribution**: remote-peer degradation via fingerprint requires a **strict
  majority among online masters' fingerprints**; tie or no online masters →
  no remote fp attribution (self-alerts still fire). Misattribution is worse
  than falling back to self-alerts.
- **Who alerts**: every node for itself; only masters for remote peers (so 25
  viewers don't all nag about one broken peer).
- **No presence wire change**: `percent`/`manifest_fp`/`role` already gossip;
  `PRESENCE_V` stays 2.
- **Degrade mechanism in tests**: `Pause` mid-download — deterministic,
  reversible, and a paused share keeps broadcasting its frozen presence
  (verified: `presence_broadcasts` ignores `paused`).
- **OS notifications**: `notify-rust` v4 on all three desktops
  (`gio::Notification` is unreliable on Windows). Windows toasts show generic
  branding until the MSI registers an AUMID — packaging follow-up, not a
  blocker.
- **Deep-verify force**: cleared on *completed forced outcome* only — clearing
  at job-build time would lose the force if the job failed.

## Soak run log

| Date | Run | Config | Report | Outcome |
|------|-----|--------|--------|---------|
| 2026-07-06 | smoke | 3M+5V, scaled 0.47 GB, 8 min, churn+degrade+conflict, health 60/120s | (dir cleaned) | Sync + health pipeline PASS: all nodes Healthy + byte-identical; degraded→renotify→recovered events on masters; verdict line read FAIL only from a harness bug (race-file exclusion), fixed in the next soak commit |
| 2026-07-07 | fullsize #1 | 3M+3V, full 41.9 GB/copy (6 ISOs 3–6 GB), 1 h window, churn+degrade+conflict, health 900/900s | interrupted (Windows rebooted mid-verify; no report) | **FINDING: presence mesh collapsed at t+91s under sustained 42 GB transfer load and never recovered for the whole hour** — every node read `online: 1 of 6` while doc/content sync continued; health events therefore 0, and swarm provider selection degraded to single-source fetches (nodes only 6–30 % after 1 h). Root cause: presence ran on the reconcile loop's tail → fixed (dedicated presence loop). Salvage restart also exposed known-issues #7 (unclean-shutdown recovery wedge). |
| 2026-07-07 | fullsize #2 | 3M+3V, full 41.9 GB/copy, 90 min window, churn+degrade+conflict, health 600/900s | `docs/soak-reports/2026-07-07-fullsize-2-download-stall.md` | FAIL (sync), **health pipeline PASS**: mesh stayed up all run, named degraded alerts at exactly 600s + renotifies + self-alert + recoveries. **FINDING (known-issues #8): downloads wedge silently** — 4 of 5 receivers pinned at 0–5 % for 2.5 h, zero errors logged; the once-paused viewer (pause aborts + re-queues downloads) was the only node to reach 100 %. Stall watchdog (15 min abort + re-queue) landed; root cause open. |
| 2026-07-07 | fullsize #3 | same config | aborted at ~t+2100 (diagnosis complete) | Watchdog alone insufficient: it recycled **5000+** downloads — the engine queued a task for every missing blob at once (~4000/node) and the ISOs head-of-line-blocked the pile. Root cause of #8 identified → `MAX_INFLIGHT_DOWNLOADS = 12` back-pressure landed. |
| 2026-07-07 | fullsize #4 | same config, + download cap | aborted at ~t+4600 (behavior established) | Cap works: steady climb everywhere (vs #3's flatline), no wedges. New bottleneck exposed: master appended last in provider lists → the first finished peer became the fleet's **sole seeder** while the master idled. → balanced-seeding policy landed (master rotates in post-grace, retires at ≥3 fully-synced peers, liveness valve). |
| 2026-07-07 | fullsize #5 | same config, + balanced seeding | `docs/soak-reports/` (see below) | **Zero watchdog fires, zero swarm-deadline timeouts, health pipeline exact** — reliability layer holds. Sync reached ~28 % (front-runners) in the 90 min window: small/mid files fast and even, the six 3–6 GB ISOs throughput-bound on one shared disk. Remaining work is bulk-transfer performance tuning (swarm pacing/slot allocation), not correctness. |
| 2026-07-07 | fleet (3M+25V) | scaled corpus 0.47 GB, 60 min window, churn 300s + degrade + conflict, health 600/900s | `docs/soak-reports/2026-07-07-fleet-28-mesh-fragmentation.md` | FAIL — **FINDING (known-issues #9): presence mesh fragments at 28 members.** Per-node membership oscillated 1/28 ↔ 25/28 all run (all-to-all `join_peers` every 6 s thrashes iroh-gossip's bounded active view); content spread starved by fragmented rosters (0/28 byte-identical of a corpus 8 nodes sync in minutes); health events 0 on node-00 (flapping "offline" pauses every episode clock). Works at ≤8 members; fix direction in known-issues #9. |

## Next engineering (from the soaks, ordered)

0. **known-issues #9 — presence mesh fragmentation at fleet scale.** Blocks the
   target topology outright (3 masters + 20–30 viewers): rejoin with a small
   random subset instead of all-to-all `join_peers` every 6 s, verify with the
   fleet soak (success = membership converges to 28/28 and holds).
1. **Bulk-transfer throughput at ISO scale** — with reliability fixed (cap +
   watchdog + balanced seeding), 3–6 GB blobs move correctly but slowly
   (~1.5 MB/s/node on the shared-disk fleet). Tune: swarm round pacing /
   backoff, per-part sizing, slot allocation (reserve slots for small files vs
   ISOs), possibly fewer concurrent ISO swarms per node so each gets real
   bandwidth. Validate on real separate machines — the single shared disk in
   the soak understates production throughput.
2. **known-issues #7** — unclean-shutdown recovery wedge (startup + first
   reconcile under live peer pressure). Has a repro recipe.
3. **known-issues #8 root cause** — why individual download futures could sit
   unfinished (mitigated by cap+watchdog; understand the iroh-level behavior).

## Deferred / stretch

- Settings/preferences UI (notification opt-out; `GetSettings`/`SetSettings`
  IPC exists but is unused by the GUI).
- Presence signing (see `usability-findings.md` #4).
- Adaptive scan backoff for `quick_signature` (decide from fleet-soak CPU data).
- Android unhealthy-peer notifications (`EngineService.kt` channel plumbing
  exists; needs an `on_peer_health` on the seed-mobile `EventListener`).
- Scale `SWARM_DEADLINE_SECS` with blob size if the full-size soak shows
  repeated deadline retries on 6 GB blobs.
- Windows AUMID registration in the MSI for branded toasts.

## See also

- `usability-findings.md` — audit findings from this push.
- `known-issues.md` — the engine audit this plan fixes (#2–#4).
- `divergence-detection-plan.md` — the divergence machinery the health feature
  builds on.
