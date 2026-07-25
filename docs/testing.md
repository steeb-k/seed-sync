# Testing

What is covered, what runs when, and what a release must pass. Read this before
trusting a green test run.

## The thing to know first

**`cargo test --workspace` runs zero integration tests.**

Every integration test in `crates/seed-core/tests/` and `crates/seed-daemon/tests/`
is `#[ignore]`d — each opens real iroh endpoints and has to run serially, so they
are opt-in by design. The consequence is not obvious and has bitten us: a full
`cargo test --workspace` compiles the entire workspace, prints a long wall of
`test result: ok`, and executes **only unit tests**. It reports success without
having synced a single file between two nodes.

```
$ cargo test --workspace
...
running 2 tests
test result: ok. 0 passed; 0 failed; 2 ignored; 0 measured
     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ every integration suite looks like this
```

So "tests pass" has, historically, meant "the code compiles and the pure functions
are right". That is worth something, and it is not remotely a statement that the app
works. Use `scripts/test-acceptance.{ps1,sh}` for that.

## Tiers

| Tier | Command | Runs | Cost | When |
|------|---------|------|------|------|
| 0 — unit | `cargo test --workspace` | pure logic: signature/settle rules, LWW resolution, tombstone rules, ignore-list codec, health policy, path handling | seconds | every commit |
| 1 — acceptance | `scripts/test-acceptance.ps1` / `.sh` | real engines, real endpoints, real files: the suites below | tens of minutes | **required before every release** |
| 2 — soak | `seed-harness` / `seed-soak` bin | multi-GB corpora, fleet scale, long-running, cross-platform | hours | before a risky release; after engine surgery |

Tier 1 is the gate. `cargo fmt` + `cargo clippy --workspace` are assumed alongside
tier 0.

## Tier 1 suites (`crates/seed-core/tests/`)

| Suite | Covers |
|-------|--------|
| `loopback` | the core bidirectional mirror: add, edit, delete, rename, nesting, empty files, locked files, viewer revert, ignore lists |
| `live_folder` | writes that land **during** a reconcile pass — overwrite, create, delete, repeated rewrite, co-master (known-issues #30) |
| `multi_master` | concurrent writes across co-masters, LWW conflicts, in-place overwrite between passes, large corpora, rescan policy |
| `health` | health/status reporting, including that a partitioned node doesn't claim `Healthy` |
| `presence` / `member_names` / `discovery` / `rendezvous` | roster, online status, remembered member identities, cold join |
| `isolation` | partition detection and self-heal |
| `resume` | download resume across suspend/restart |
| `gc` | blob-store GC against a replica-derived live set |
| `tombstone_race` | a master that joins holding a copy of a deleted file must not resurrect the delete, even when it reconciles before its replica syncs (known-issues #10) |
| `persistence` / `keystore` | state survives restart; locked-keystore behaviour |
| `seed-daemon/loopback_ipc`, `health_ipc` | the same through the real IPC surface the GUI uses |

## How to write a test that would have caught a real bug

The mid-pass overwrite bug (known-issues #30) is the worked example, and the reason
this section exists. There *was* a test for in-place overwrite —
`inplace_overwrite_large_file_converges` — and it passed the whole time the app was
losing a 1.9 GB file. It overwrote the file **between** reconcile passes.

The app does not run between passes. It runs continuously, on a folder the user is
touching at the same time. So:

**Test the concurrent case, not the quiescent one.** If a scenario has a "and
meanwhile the engine was busy" variant, that variant is the one users hit. A pass on
a multi-GB share takes 80 seconds; anything a user does has an 80-second window to
land inside one.

**Make the interleaving deterministic, not timed.** Do not sleep and hope. The
engine exposes seams for this — `ReconcileJob::debug_before_settle` runs a closure
inside a pass, at the point a mid-pass write matters. Add seams rather than races;
a flaky test gets muted and then it protects nothing.

```rust
let job = engine.make_reconcile_job(&id)?.expect("reconcilable");
let outcome = job.debug_before_settle(move || {
    std::fs::write(&target, &new_bytes).unwrap();   // the user, mid-pass
}).run().await?;
engine.finish_reconcile(&id, Some(outcome));
// then drive ordinary ticks and require convergence
```

**Assert on what the user sees, not on internals.** `Cluster::converged` requires
byte-identical folders *and* agreeing fingerprints *and* no node reporting
`OutOfSync`/`NoPeers`. Checking that a hash got published would have passed for #30's
sibling failure; checking that the peer's file on disk changed would not have.

**Assert the status line is honest.** `live_folder.rs::assert_healthy_nodes_agree`
requires that any two nodes claiming 100% on the same manifest hold identical
folders. The single worst property of #30 was not that sync stalled — it was that
everything reported `Healthy 100%` while it did. A stall you can see is a bug; a
stall you can't see is a data-loss story. Prefer assertions of the form "if the app
*claims* X, then X is true".

**Prove the test fails without the fix.** Revert the fix, run the test, confirm it
fails, restore. A regression test that has never been seen red is a guess.

**Make a failing test able to explain itself.** known-issues #10's reopening is the
worked example. `delete_survives_unseen_master_copy` *did* catch a real
data-loss bug — but it drove its convergence loop with `let _ = engine.reconcile(..)`,
discarding every error, and no integration test installed a tracing subscriber. So
all it could report was "the files still disagree", which read like a hang and cost a
day of misdirected triage. Once the loop printed reconcile errors and a periodic
`A.x/B.x` heartbeat, the answer was immediate: 451 passes, zero errors, healthy sync
of the *other* file — not a hang at all, but a stable convergence on the wrong state.
Two cheap habits pay for themselves here:

- Never `let _ =` a drive-loop call. Print the error (deduplicated, so a repeating
  one doesn't flood) and include the last one in the assertion message.
- Call `common::init_tracing()` at the top of the test. It's a no-op unless `RUST_LOG`
  is set, so tests stay quiet and fast by default:
  `RUST_LOG=seed_core=debug cargo test -p seed-core --test multi_master -- --ignored --nocapture`

**Clean up OS-level state, not just temp dirs.** Creating a master share writes a
seed to the **OS keystore**, and only `Engine::remove_share` deletes it — tests tear
down with `shutdown()`, so for a long time every master share a test created leaked
one credential permanently. Windows caps credentials per logon session (~512), and
past the cap `CredWrite` fails with `ERROR_NOT_ENOUGH_MEMORY` (8), at which point the
engine silently stores master keys in its DB instead and the `keystore` suite can no
longer establish its preconditions. This box reached **639** stale entries at roughly
34 per acceptance run before anyone noticed, and the resulting failure looked
convincingly like "the box is broken" rather than "we leaked". Bind
`common::SecretGuard::new(&created.share_id)` right after every `create_share`;
`Cluster` carries one already. It is a `Drop` guard on purpose — red runs are exactly
when tests leak, so cleanup must survive a panic.

**A load-dependent failure is a race, not a flake.** The same bug reproduced 4/4
under the full gate and passed standalone, which is exactly what "one side wins a
race more often when the box is busy" looks like. Before muting anything, reach for
a seam that makes the ordering deterministic — for #10 that was `add_share_open`,
which opens a joining master's replica *without* starting live-sync, so it provably
reconciles blind (`tests/tombstone_race.rs`).

## Known coverage gaps

Honest list; not yet written.

- **Live-folder coverage is single-file.** Mid-pass mutation is tested one path at a
  time. A user unpacking an archive mutates hundreds of paths across many passes.
- **No test drives the real daemon loop.** Tier 1 calls `Engine::reconcile` directly.
  The 750 ms tick, the slow-pass watchdog, the busy-guard (`state.publishing`) and
  the periodic deep verify are only exercised in tier 2 / in production. The 4-hour
  `DEEP_VERIFY_INTERVAL_SECS` in particular means a bug that only the deep verify
  repairs looks fine in a 2-minute test.
- **Mid-pass writes on the *receiving* side** (user edits a file while a viewer is
  materializing it) are untested.
- **Windows file-locking during sync** — a file open in another app while the engine
  wants to write it — is unit-tested in `scan.rs` but not end-to-end.
- **No GUI test at all.** Everything the user actually looks at is unverified.
- **Cross-platform pairs are untested in CI** (there is no CI). Windows↔Linux and
  Windows↔Android sync is only ever exercised by hand.

## Soak (tier 2)

`crates/seed-harness` owns corpus generation (`corpus::CorpusSpec`) and daemon-process
drivers; `seed-soak` is the runner. Multi-GB and fleet-scale runs are what surfaced
known-issues #5–#9. Notes on running soaks on the maintainer box (disk class matters
for the numbers) are in the maintainer's own notes, not here.

**Do not trust a soak's headline verdict on its own — read the convergence section
too.** Known-issues #33 is open: the hourly GC sweep (`GC_INTERVAL_SECS = 3600`)
drops every node to `Syncing 98%` on the next health sample and it never recovers,
so *any* run longer than about an hour fails on `all nodes Healthy at end: false`
no matter how well it went. A run that reports FAIL but lists every node
`byte-identical ✓` with no anomalies has almost certainly hit this and nothing else.
Two full soaks were spent proving that once. The `iroh_blobs::store::gc=debug` line
in `seed-soak` puts the sweep in the timeline so the correlation is visible rather
than inferred.
