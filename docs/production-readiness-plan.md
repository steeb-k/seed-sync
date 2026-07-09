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
- `[x]` **Phase 8 — soaks**: `seed-soak` bin (fleet | fullsize | midsize |
  clean) built; **fleet soak (3M+25V) PASSED 2026-07-08** after the
  fleet-scale fixes (#9–#11, #7); **fullsize soak (3M+3V, 42 GB/copy) PASSED
  2026-07-08** on the split-disk config (6/6 byte-identical, converged
  in-window); throughput question closed by the split-disk measurement
  campaign (see the soak run log + next-engineering item 1).

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
| 2026-07-07 | fleet #2 | same config, + subset presence rejoin (#9 fix) | aborted at ~t+1000s (diagnosis complete) | Mesh fix works at idle: membership held 25–27/28 for the pre-load phase (vs run 1 oscillating from t=0). **FINDING (known-issues #10): OutOfSync doc-resync storm** — once churn started, every diverged node kicked `start_sync` against all 27 members every 6 s (1200+ sessions in 17 min), CPU hit 300–1400 %/daemon, presence beats starved past the 20 s TTL, membership collapsed to avg ~7/28. → bounded repair landed: ≤1 kick per share per 30 s, ≤3 sampled peers per kick. |
| 2026-07-07 | fleet #3 | same config, + resync throttle (#10 fix) | `docs/soak-reports/2026-07-07-fleet-3-provider-oom.md` | FAIL (sync), **mesh + health verified under load**: membership held 26–27/28 through churn onset (vs #2's collapse), dips tracked real daemon deaths exactly; 90+ PeerHealth events (vs 0 in run 1); resync kicks 661×3-peer vs #2's 1235×27-peer (~20× less session load). **FINDING (known-issues #11): OOM aborts** — 7 daemons died on 5–80 GiB allocation failures, RSS +15–40 MB/s on partially-synced nodes (grew through pause; disk flat). First attributed to iroh-blobs' uncapped provider accept loop → 16-stream cap + servable-primary rotation landed as defense-in-depth. Also: 3 nodes hit the known-issues #7 wedge **cold-start variant** (first reconcile never returns; seqno=0, silent) → phase watchdog landed for the next run. |
| 2026-07-07 | fleet #4 (15 min) | same config, + provider cap + servable primaries | aborted (diagnosis) | Leak reduced but not fixed: RSS sane through the window (max 430 MB vs #3's 6.6 GB at same age) then exploded ~12 MB/s as transfers began succeeding en masse; 18/28 OOM deaths. **Phase watchdog paid off immediately**: the #7 cold-start wedge is passes stuck 25+ min inside iroh-docs reads (`read ignore list`, doc `get_many` stream) — docs actor unresponsive under fleet pressure; slow paths dominated by `compute health` store calls. |
| 2026-07-07 | fleet #5 (alloc-trace) | same config, daemon built with ≥512 MB allocation-backtrace allocator | (diagnosis run) | **known-issues #11 root cause captured in one backtrace**: the doubling allocation is `VecDeque::push_back` in **iroh 1.0.0's per-remote `pending_open_paths` retry queue** (`remote_state.rs:1062`) — no dedup + per-connection re-push every 333 ms multiplies the queue without bound whenever a remote's CIDs stay exhausted (dead/wedged/overloaded peers). Not fixed upstream through iroh 1.0.2. → vendored iroh with a one-hunk dedup+cap patch; iroh-blobs/provider + swarm fixes kept as defense-in-depth. |
| 2026-07-07 | fleet #6 (25 min) | same config, + vendored-iroh queue patch | `docs/soak-reports/2026-07-07-fleet-6-oom-fixed-convergence-tail.md` | **known-issues #11 VERIFIED FIXED**: 28/28 alive to the end, 0 OOM, 0 alloc-traces, RSS max 105→726 MB (plausible working set; runs #3–#5 were multi-GB and dying by the same age). Mesh held 27–28/28; sync ~3× faster (64 % at t+846 vs 39 % at t+1096 in #3). Verdict FAIL on convergence tail only: **10 nodes had reconcile passes wedged >10 min inside iroh-docs reads** (phase watchdog data) — known-issues #7 is now the top blocker; one node 0 % all run. |
| 2026-07-08 | fleet #7 (full 60 min) | same config, + vendored iroh-docs deadlock patches + doc-read timeouts (#7 fix) | `docs/soak-reports/2026-07-08-fleet-7-consistent-harness-races.md` | **Every fleet-scale goal met**: membership 28.0/28 and holding, pct avg 100 mid-churn, RSS max 841 MB, 0 OOM, 0 wedges, **all nodes Healthy at end** — #7 fix verified (no doc-read wedges; prior run had 10/28). Verdict FAIL only on 4 verify lines, **identical on all 28 nodes** (fleet fully byte-consistent with itself): churn deletes resurrected by masters still mid-initial-publish (deletion-as-absence race → new known-issues #12) and the ordered-conflict write inverted by publish lag (#5, now soak-evidenced). Both are documented multi-master semantics the harness asserted naively → harness now gates churn/conflict on all masters Healthy @ 100 % and asserts *causal* conflict ordering. |
| 2026-07-08 | fleet #8 (full 60 min) | same config, + gated harness | `docs/soak-reports/2026-07-08-fleet-8-PASS.md` | **PASS** (with benign anomalies: transient IPC sample timeouts under initial-sync load, by-design conflict notes). **28/28 byte-identical, all Healthy at end, 55 PeerHealth events, 0 swarm-deadline hits, 0 OOM, 0 wedges**; membership 28.0/28 and pct 100 held through 7 churn rounds + degrade/recover + conflict. The target topology (3 masters + 25 viewers) is validated end-to-end. |
| 2026-07-08 | fullsize #6 (HDD) | 3M+3V, 41.9 GB/copy, 90 min window, full fix stack | `docs/soak-reports/2026-07-08-fullsize-6-hdd-baseline.md` (reconstructed) | **known-issues #8 verdict: silent-wedge pathology GONE** (continuous progress everywhere; 3 watchdog recycles were slow-transfer resumes under verify congestion) → #8 closable as fixed-by-#7/#11. Throughput unchanged from pre-fix (~1.5 MiB/s/node) because the **single HDD is saturated** (839 % disk time, queue ~9.5, ~21.5 MB/s device ceiling) — the disk, not the engine, is the measurement. Health-loop bitmap re-reads flagged for the tuning list. Harness bug found+fixed: run froze pre-report on an unpreemptable IPC wait (`request_bounded` now JoinHandle-bounded; minidump kept). Next: split-root SSD/HDD A/B. |
| 2026-07-08 | fullsize #7 (split-disk A/B) | 3M+3V, 41.9 GB/copy: seeder+3 nodes on NVMe SSD, 2 viewers on the HDD | `docs/soak-reports/2026-07-08-fullsize-7-split-disk-PASS.md` | **PASS — 6/6 byte-identical, all Healthy, first fullsize to fully converge in-window.** SSD receivers each pulled the full 42 GB in ~20–35 min (~35–45 MiB/s/node; the degraded viewer caught up 42 GB in ~15 min after resume); the 2 HDD viewers finished in ~75 min at ~20–26 MiB/s combined once the swarm matured. **Bulk-transfer verdict: the engine was never the bottleneck — all prior "1.5 MiB/s/node" numbers were six nodes thrashing one spindle.** Remaining tuning is HDD graceful degradation (IO shaping: large-blob slot class, provider-count-scaled part fan-out, health-loop bitmap caching) + a look at ~2.5-core CPU per daemon during 40 MiB/s transfers. |
| 2026-07-08 | midsize ceiling | 1M+2V (~3.9 GiB/copy): seeder+viewer on SSD, one LONE viewer on the HDD | `docs/soak-reports/2026-07-08-midsize-ceiling-PASS.md` | **PASS, no anomalies.** SSD viewer: full corpus in <30 s (**>130 MiB/s**). Lone HDD viewer: done at t+126 s (**~35–40 MiB/s uncontended**) — the single-spindle engine ceiling. Confirms the contention-scaling story (1 node ≈ 35–40, 2 ≈ 10–13 each, 6 ≈ 1.5 each MiB/s) and closes the throughput question: per-node performance is disk-class on realistic one-node-per-disk deployments. |
| 2026-07-08 | fleet #9 (ungated churn) | 3M+25V, `--no-scenario-gate`: churn deletes race still-seeding masters from t+300 (the known-issues #12 scenario, post-fix) | (verify race; rerun below) | **#12 evidence positive**: ten churn rounds' deletes honored by all 28 nodes, zero resurrections. Verdict FAIL only because round 11 fired at t+3606 — after window close — and the convergence gate passed inside the 45 s settle window while 2 masters were still applying it (verify caught them stale: missing the round's ADD, stale rewrites — the *opposite* of a #12 signature). → harness quiet tail: no churn round in the final 120 s. |
| 2026-07-08 | fleet #10 (ungated churn, rerun) | same config, + quiet tail | `docs/soak-reports/2026-07-08-fleet-10-ungated-churn-PASS.md` | **PASS — known-issues #12 fix verified end-to-end at fleet scale**: 28/28 byte-identical, all Healthy; ~31 churn deletes raced still-seeding members across 10 rounds (`--no-scenario-gate`) and every one was honored — zero resurrections. The scenario that produced fleet #7's resurrections now passes ungated. |

## Next engineering (from the soaks, ordered)

0. `[x]` **Fleet-scale stability — DONE, soak PASS (fleet #8, 2026-07-08).**
   Fixed + verified at the target topology: #9 subset presence rejoin, #10
   bounded doc-resync kicks, #11 vendored-iroh `pending_open_paths` dedup+cap
   (+ iroh-blobs 16-stream provider cap, servable-only swarm primaries,
   ≥512 MB alloc-trace allocator), #7 vendored iroh-docs deadlock patches
   (try_send events + fair LiveActor polling) + 120 s doc-read timeouts +
   60 s phase watchdog. New documented caveats: #12 (delete vs still-seeding
   master resurrection), #5 evidence (publish-lag LWW inversion); harness now
   gates mutation scenarios on all-masters-Healthy and asserts causal conflict
   ordering. **Follow-up: report the iroh and iroh-docs bugs upstream
   (n0-computer) before any iroh-stack bump.**
1. `[x]` **Bulk-transfer throughput at ISO scale — RESOLVED by measurement
   (2026-07-08 split-disk campaign).** The engine was never slow: per-node
   rates are disk-class — **>130 MiB/s on NVMe, ~35–40 MiB/s on an
   uncontended spinning HDD** (midsize ceiling run); the historic
   "1.5 MiB/s/node" was six daemons seek-thrashing one spindle (839 % disk
   time, queue ~9.5). Contention scaling measured: 1 HDD node ≈ 35–40 MiB/s,
   2 nodes ≈ 10–13 MiB/s each, 6 nodes ≈ 1.5 MiB/s each.
   **Landed:** large-blob slot class (`MAX_INFLIGHT_LARGE = 2` of the 12
   download slots) — a 2-HDD-viewer midsize A/B measured **no change**
   (~15 MiB/s/node both sides, PASS both runs); kept anyway because it bounds
   the un-benchmarked worst case (many multi-GB blobs writing concurrently on
   one spindle), completes individual files sooner (earlier seeding for the
   rest of a fleet), and showed zero cost. Also fixed en route: the
   servable-primary pool degenerated to a possibly-offline master when no
   peer's percent had gossiped yet (caught by
   `large_blob_swarms_across_two_seeders`); the restriction now applies only
   when a confirmed-full peer exists. **Still open (low priority):**
   health-loop chunk-bitmap caching (per-tick `local_bytes` re-reads during a
   big-blob tail) and the ~2.5-core CPU per daemon during 40 MiB/s transfers.
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
