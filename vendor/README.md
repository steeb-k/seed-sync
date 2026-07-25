# Vendored, patched dependencies

Three crates.io crates are vendored here and patched in place, wired in via
`[patch.crates-io]` in the workspace `Cargo.toml`. Each carries fixes for
upstream bugs that bit us at fleet scale.

**Every patch site is marked with a literal `SEED-SYNC PATCH` comment** — that
string is the reliable way to find them all:

```bash
grep -rn "SEED-SYNC PATCH" vendor/
```

Current state (verified 2026-07-24):

| Crate | Version | Hunks | Issue | Upstream status |
|-------|---------|-------|-------|-----------------|
| `iroh` | 1.0.3 | 1 | known-issues #9 | **reported**, [iroh#4390](https://github.com/n0-computer/iroh/issues/4390) — open |
| `iroh-blobs` | 0.103.0 | 2 | known-issues #25, #9 | not reported |
| `iroh-docs` | 0.101.0 | 2 | known-issues #5 | not reported |

Issue numbers are the **current** `docs/known-issues.md` numbering. That doc was
renumbered at some point and older source comments still cite the pre-renumber
ids (they say #11 and #7 for what is now #9 and #5) — cross-reference by title,
not by number.

---

## iroh 1.0.3 — unbounded `pending_open_paths` retry queue (1 hunk)

**Why:** `open_path_on_conn` (`src/socket/remote_map/remote_state.rs`) pushes a
failed address onto `State::pending_open_paths` unconditionally when
`open_path_ensure` returns `RemoteCidsExhausted` / `MaxPathIdReached`. The
333 ms drain then re-opens each queued entry on *every* connection to that
remote, and each connection still at its path-id cap re-queues the same address
— so with C connections the queue is multiplied by ~C every cycle. With a single
connection it is steady-state, which is why this only shows up at fleet scale.

A remote whose CIDs stay exhausted (an unroutable advertised address, a dead or
overloaded peer — of which a struggling fleet has plenty) grows the queue without
bound. Our soak daemons died on the `VecDeque` doubling realloc with single
failed allocations of 5, 10, 20, 40 and 80 GiB. See known-issues #9.

**The patch:** a module-level `MAX_PENDING_OPEN_PATHS = 64` plus an
`enqueue_pending_open_path` helper that dedups on push and evicts the front entry
when full; the requeue site calls the helper instead of `push_back`.

This is deliberately the **same shape as the fix proposed in
[iroh#4390](https://github.com/n0-computer/iroh/issues/4390)**, so the hunk drops
out cleanly once upstream merges it.

**Status:** reported upstream by another operator hitting the identical stack
(their evidence: a single ~24 GB allocation on macOS, same backtrace). Introduced
in iroh 1.0.0 by [iroh#4296](https://github.com/n0-computer/iroh/pull/4296).
**Open as of 1.0.3.** Re-check that issue on every iroh bump — this is the one
patch with a live path to deletion.

Note the issue's closing observation, which we have *not* addressed: persistent
`MaxPathIdReached` suggests unreachable candidate paths are not being abandoned
to free path-id budget. Dedup+cap fixes the memory, not that root cause.

## iroh-blobs 0.103.0 — Windows cross-volume reference export (hunk 1 of 2)

**Why:** `ExportMode::TryReference` (used so a viewer references its mirror file
instead of keeping a second copy) moves the owned blob with `std::fs::rename` and
only falls back to a copy when the OS error is `EXDEV` (unix, 18). On Windows a
cross-volume move returns `ERROR_NOT_SAME_DEVICE` (17), which upstream didn't
match, so the export failed and a viewer whose mirror was on a different drive
than its data dir kept the content twice. See known-issues #25.

**The patch** (`src/store/fs.rs`, in `export_path_impl`): also treat error 17 as a
cross-volume move so it falls back to copy + sets the entry to `External`. The now-
redundant owned `.data` is then reclaimed by `seed-core`'s reclaim-retry queue
(it can't be deleted until iroh releases the file handle, ~3 s later on Windows).

Inert on Linux/macOS (they hit 18, already handled).

**Status:** still unfixed on upstream `main`; not reported.

## iroh-blobs 0.103.0 — bounded provider accept loop (hunk 2 of 2)

**Why:** `handle_connection` (`src/provider.rs`) spawns one detached task per
inbound request stream with no cap, so a fleet's swarm retry storm piles
unbounded stream tasks and send buffers onto a serving node. Soak-observed OOM
aborts. Defense-in-depth for known-issues #9.

**The patch:** `MAX_STREAMS_PER_CONNECTION = 16`; the accept loop holds a
semaphore permit for each in-flight stream so QUIC flow control pushes back
instead of the node buffering without limit. 16 matches one full swarm's part
fan-out on the requesting side.

**Status:** still unfixed on upstream `main`; not reported.

## iroh-docs 0.101.0 — sync-actor / LiveActor deadlock (2 hunks)

**Why:** the single-threaded docs sync actor deadlocked against the LiveActor at
fleet scale. Per-insert event emission *awaited* a bounded subscriber channel
while holding the actor thread, and the LiveActor that drains that channel polled
`biased;` toward an inbox whose handlers re-enter the sync actor. Under a
divergence-driven sync-report storm this froze the actor permanently; every doc
read (`get_one` / `get_many` / `open` / `subscribe`) queued behind it forever.
See known-issues #5 — 10 of 28 fleet nodes wedged before the fix, 0 after.

**The patches:**
- `src/sync.rs`, `Subscribers::send`: `try_send` with the event dropped on a full
  channel (slow consumer) instead of awaiting a bounded `send` across all
  subscribers. Also drops the then-unused `IterExt` import.
- `src/engine/live.rs`, `LiveActor::run_inner`: remove `biased;` from the
  `tokio::select!` so `replica_events_rx` drainage can't be starved by the inbox.

The app keeps a backstop regardless: `DOC_READ_TIMEOUT_SECS = 120` in
`seed-core`, so a future actor stall degrades instead of wedging.

**Status:** still unfixed on upstream `main`; not reported.

---

## Re-vendoring checklist

These are crates.io tarballs unpacked verbatim, then patched — there is no git
history to merge against, so a bump is a re-apply, not a rebase.

1. Check whether the bug is fixed upstream first. For `iroh`, that means
   [iroh#4390](https://github.com/n0-computer/iroh/issues/4390); for the others,
   read the patch site on upstream `main`. **If it's fixed, drop the vendored
   crate entirely** rather than carrying a dead patch.
2. Download and unpack the new version:
   ```bash
   curl -sL https://static.crates.io/crates/iroh/iroh-<VER>.crate | tar xz
   ```
3. Diff the old vendored tree against the *old* stock tarball to confirm you know
   every hunk you are carrying (should match `grep -rn "SEED-SYNC PATCH"`).
4. Replace the tree, preserving the `.cargo-ok` marker, and re-apply each hunk.
5. Verify the result is stock + your hunks and nothing else:
   ```bash
   diff -rq iroh-<VER> vendor/iroh    # expect only .cargo-ok + patched files
   diff -u  iroh-<VER>/src/.../file.rs vendor/iroh/src/.../file.rs
   ```
6. Bump the version in this README's table and in the `[patch.crates-io]` comment
   block in the workspace `Cargo.toml`.
7. `cargo tree -d` — confirm a single `iroh-base` resolves.
8. Re-verify the semver-exempt API surface we build against
   (`unstable-custom-transports` → `PathSelector`, `unstable-net-report` →
   `net_report()`); see `docs/iroh-1.0-api-notes.md`.
9. Run the acceptance gate (`scripts/test-acceptance.ps1`), and for any change
   touching `vendor/iroh` or the provider loop, a fleet soak — these patches
   exist because of failures that only appear at scale.
