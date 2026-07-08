# seed-soak fullsize report — HDD baseline / known-issues #8 verdict (manually reconstructed)

- config: 3 masters + 3 viewers, corpus 41.9 GB/copy (6 ISOs 3–6 GB), 5400 s window,
  churn 300 s + degrade viewer-0 + conflict (gated; never armed — masters 1/2 never
  reached 100 % in-window), health 600/900 s
- root: `D:\seed-soak` … a single **spinning HDD** (ST2000DM008) hosting all six nodes
- verdict line: none — the harness froze pre-report (see "harness hang" below);
  all measurements below are from `samples.csv`, the monitor pulses, and daemon logs
  (preserved in the session scratchpad).

## Known-issues #8 verdict: the silent-wedge pathology is GONE

- **Zero silent stalls.** Original #8 signature (fullsize #2): 4 of 5 receivers
  pinned at 0–5 % for 2.5 h with zero log lines. This run: every receiver
  progressed continuously for the whole window (9–29 % reached, rate-bound by the
  disk, below).
- **3 stall-watchdog recycles total** (nodes 02, 05), all during the post-window
  phase when the byte-verify was streaming ~80 GiB off the same spindle — i.e.
  legitimately slow transfers exceeding the 15 min in-flight bound, aborted,
  resumed from verified chunks. Working as designed; not wedges.
- Zero memory events (alloc-trace armed, ≥512 MB threshold); RSS ≤ ~105 MB on all
  nodes throughout.
- Root-cause view: #8's wedges were the iroh-docs actor deadlock (#7) and the iroh
  `pending_open_paths` pathology (#11) observed from the download path — both fixed
  and independently verified. **#8 can be closed as fixed-by-#7/#11**, with the
  stall watchdog retained as the safety net.

## Throughput baseline (the disk, not the engine)

- Aggregate mirror-bytes rate across 5 receivers: **~6–9 MiB/s early, ~1–4 MiB/s
  in the ISO tail** (~1.5 MiB/s/node), matching pre-fix numbers — the fixes did
  not change fullsize throughput because the **HDD is saturated**: measured
  during the run — % disk time ~839 %, queue length ~9.5, ~21.5 MB/s device
  throughput with every payload byte touching the spindle 2–4×.
- Small/mid files complete early (nodes plateau ~25–29 % = the ISO tail), exactly
  fullsize #5's profile.
- Engine-side observation for the tuning list: while ISOs are incomplete, each
  reconcile pass's health computation issues ~1 store call per missing blob and
  re-reads multi-GB chunk bitmaps every ~750 ms tick (passes observed 240–1022 s
  in "compute health" under peak IO) — cache candidates between passes.

## Harness hang (fixed)

seed-soak froze forever after "run window over" with zero CPU/IO: an IPC
request's connect/read blocked its worker thread inside poll, where the
same-task `tokio::time::timeout` can never fire. `request_bounded` now runs the
request on its own spawned task and times out on the JoinHandle (always fires;
a stuck request leaks one abandoned task instead of the run). Minidump of the
hang kept at `D:\seed-soak-hang-2720.dmp` for offline analysis.

## PeerHealth

30 health events observed on node-00 in-window (degrade pause at t+381 s →
self/remote alerts and renotifies flowing; resume at half-window recovered).
