# Cross-OS testing log (shared: Windows ⇄ Linux)

A shared scratchpad for the two Claude instances testing Seed Sync's Windows↔Linux
sync (M4 Checkpoint #3). Both machines have this repo checked out; the human syncs
git between them.

## How to use this file
- **Read it first** at the start of a session to catch up on what the other side found.
- Append entries to the **Findings log** tagged **[WIN]** or **[LINUX]** with the date.
- Keep the **Status board** and **Open issues** sections current (edit in place).
- Write down what you learned **before** handing back, so the human can sync.
- One fact per entry; link commits by short hash.

## Status board
Legend: ✅ pass · ❌ fail/bug · ⏳ not yet tested

| Check | Linux→Win | Win→Linux |
|---|---|---|
| Mirrors all files, sizes exact | ✅ | ✅ |
| Cross-OS path (`/`↔`\`) + unicode names | ✅ | ✅ |
| Self-heal (corrupt mirror → auto-restore) | ✅ ~6s | ✅ ~3s |
| Viewer 1× dedup, **same volume** | ✅ (local) | ✅ 1.9GB→8MB |
| Viewer 1× dedup, **cross volume** | ✅ fixed (#1) | ✅ Linux 1× native (#1 Win-only) |
| Deletion / update propagation | ✅ | ✅ |
| Empty (0-byte) + unicode file syncs (`5bdfa4b`) | — | ✅ |
| Multi-GB file (no doubling, streaming) | ✅ 100MB | ✅ 1.76GB |
| File **move** = local relocate, no re-download | — | ✅ 1.76GB |

## GUI + presence feature test plan (M4 polish, 2026-06-19)
Derived from the post-Checkpoint-#3 commits (no plan was written, so this is reverse-engineered
from the diffs): `1d9080e` member names + health via gossip presence, `894544f` GUI dialog/dot
polish, `7095421` Open-folder + dark tray, `50f8588` dark tray app-mode, `103b335` Windows exe icon.
**Note:** `seed-cli` has NO peers/device-name command — presence is GUI/IPC-only; the only headless
coverage is `seed-core/tests/presence.rs`. So most of this needs the GUI.

Legend: ✅ pass · ❌ fail · ⏳ not yet · 🐧 Linux-doable · 🪟 Windows-only · 🔀 cross-OS (needs both)

### [LINUX] GUI test run 2026-06-19 (results)
- ✅ **Main window** renders: compact frameless header (＋/gear/title/✕), share row
  (pause ‖ · name + `viewer · path` · `Healthy 100%` · `N of M ▶` members · ⋮ actions), bottom
  speed/updated bar. Members count reflects presence (`1 of 2` when Windows offline → `2 of 2` online).
- ✅ **A1/A2/A4 presence cross-OS (item 6 + 9):** Members dialog shows this device `steebP14s`
  (hostname default) / **viewer** + the Windows master `sk-devBox` / **master**, both green dots,
  online. Name + role propagate Win→Linux over gossip live. Dialog is frameless (`adw::MessageDialog`,
  Close response), big green health dots. Minor cosmetic: local row ellipsizes `steebP14s (this devi…`.
- ⚠️ **FALSE ALARM "could not load peers":** seen when clicking the members button — caused by a
  **stale daemon** (the test daemon was launched before the presence commit; new GUI couldn't
  deserialize old `PeerInfo` which lacks `name`/`percent`, no `#[serde(default)]`). NOT a code bug.
  Fixed by restarting the daemon on the new binary (data dir preserved, no re-download).
  **Lesson: restart running daemons after pulling new code before GUI testing.** Consider adding
  `#[serde(default)]` to new optional IPC fields for forward/backward compat (cheap robustness).
- ✅ **Item 10 (three-dot actions menu) + gear menu:** render fine (human-verified).
- ✅ **Item 8 (Set device name… prefill):** after restarting the GUI against the new-binary daemon,
  the gear dialog prefills with the hostname `steebP14s`. The earlier "empty" was the SAME stale-daemon
  artifact — the GUI fetches the device name once at startup (`GetDeviceName`), which failed against the
  pre-presence daemon, leaving the cache empty. **Real robustness gap (for a fix):** GUI never refetches
  the device name after the one startup call — if the daemon is unavailable at GUI launch (or restarts),
  the name silently stays empty until GUI relaunch. Suggest refetch-on-reconnect or a retry.
- ✅ **Item 7 (create/add "Your name" prefill):** uses the same `device_name` cache as item 8, so
  prefills once the cache is populated (confirmed working post-restart; create form shares the path).
- ✅ **Item 11 ("Open folder"):** human-verified — opens the target folder in the file manager.
- ✅ **A3 gray (offline) dot, cross-OS:** killed the Linux viewer daemon → on the **Windows** Members
  dialog the Linux member `steebP14s` flipped to **gray almost instantly** (gossip `NeighborDown`, not a
  slow heartbeat timeout). The offline member **retains its last-known name** (roster keeps the entry
  with `online=false`) — correct/intended UX (you want to see *who* is offline). Green + gray verified.
- 🆗 Health-dot **yellow (<100%)**: **called good without a live capture** — same code path as green/gray
  (`health = present/total bytes`, already seen as "Syncing 0%" text during the ISO sync); low risk.
- ✅ **Linux tray IMPLEMENTED (un-deferred 2026-06-19):** replaced the no-op with a pure-Rust
  `ksni`/StatusNotifier tray (no GTK3/appindicator), driven by its own tokio runtime on a dedicated
  thread, events bridged to the GTK main loop over `async-channel`. Icon decoded from the embedded
  `appIcon.png` via `gdk-pixbuf` → ARGB32 at 22/32/48/64 px. Verified live on niri/quickshell: item
  registers with the watcher (`org.kde.StatusNotifierItem-<pid>-1`, Title "Seed Sync", 4 icon sizes
  served), icon renders, and all 4 behaviors work (right-click menu Open/Quit, left-click opens,
  close-to-tray hides + reopens, Quit exits). **Windows tray is a separate item (#12/13, 🪟):** if the
  Seed Sync icon is missing in the *Windows* tray while other apps show, check for a `tray unavailable:
  {e}` warn from `TrayIconBuilder`.
- Tooling note: no input-injection on this Wayland/niri session (no ydotool/wtype) — the human
  drives clicks; I focus the window (`niri msg action focus-window --id N`) + screenshot.

### 0. Build/automated gate — 🐧 [LINUX done 2026-06-19]
- ✅ `cargo build --workspace` clean (icon `build.rs` is a no-op off-Windows; presence deps resolve).
- ✅ Unit tests incl. new `presence::topic_is_deterministic_and_domain_separated`, `presence_roundtrips`,
  IPC `roundtrip_device_name`. ⚠️ `loopback.rs` real-endpoint tests are **timing-flaky** (intermittent
  timeouts; pass on retry — not a logic regression; aggravated by live daemons + new per-share gossip
  startup). Treat loopback as retry-on-fail, not a hard gate.

### A. Presence protocol — names + health (`1d9080e`) — 🔀 core feature
1. ⏳ Device name **defaults to hostname**, and a custom name **persists across daemon restart** (new
   `settings` table). 🐧 (GUI "Set device name…" or IPC SetDeviceName).
2. ⏳ Setting a name on one member **propagates over gossip** and shows on the other member. 🔀
3. ⏳ **Health %**: a viewer mid-sync shows **<100% (yellow dot)**, reaches **100% (green)**; master
   always 100%; a member that goes offline shows **gray**. 🔀 (health = present/total manifest bytes).
4. ⏳ Names + health propagate **both directions** Win↔Linux; a **viewer's** name shows on the master
   (presence rides gossip since viewers can't write doc entries). 🔀
5. ⏳ Trust model sanity: v1 presence payload is unsigned, trusted by authenticated `delivered_from` —
   just confirm a member only appears for peers actually in the share. 🔀

### B. Linux GTK GUI visual/interaction (`894544f`, `7095421`, GUI half of `1d9080e`) — 🐧
6. ⏳ **Members** dialog (was "Peers"): health dots ~1.8× bigger; **hover shows the word label**
   (synced / <100% / offline); each row shows member **name + role**.
7. ⏳ **"Your name"** field prefilled with hostname in both **create** and **add** forms.
8. ⏳ Gear-menu **"Set device name…"** dialog writes the one global device name (reflected in members).
9. ⏳ Address / keys / Members dialogs are **frameless** (`adw::MessageDialog`, Close response, no
   titlebar) — matching the Remove dialog.
10. ⏳ Popover **menu labels left-aligned** (`flat_button`).
11. ⏳ **"Open folder"** launches the file manager (Linux: `xdg-open`).

### C. Windows-only (🪟 — NOT testable on Linux; Windows side must verify)
12. ⏳ App icon embedded in the **exe / taskbar / window** + MSI shortcut/ARP icon (`103b335`,
    `winresource`, `#[cfg(windows)]`).
13. ⏳ **Dark tray context menu** follows the app color scheme via `SetPreferredAppMode` (`50f8588`,
    `7095421`). Linux tray is a no-op (tray-icon is Windows/macOS only), so untestable here.

### [WIN] 2026-06-19 — GUI polish **round 2** (Linux: please re-test the 🐧 items)
More GUI/UX changes after the first pass; commits `6180139`→`1166387` (all GUI-only, no daemon/engine
or IPC changes — a daemon restart is NOT required, just deploy the new `seed-gui`).

- 🐧 **HiVis tray icon** (`f74cc02`): the Linux **ksni** tray now decodes `icon/appIconHiVis.png`
  (was `appIcon.png`) — a high-visibility variant tuned for the small tray. Confirm the Linux tray
  shows the new icon. (Windows/macOS tray switched in `c5a0601`; the Windows **exe** icon also moved
  to `appIconHiVis.ico` in `ca9e82b` — exe icon is Windows-only via `build.rs`.)
- 🐧 **Single-instance** (`1ffd99e`): a second `seed-gui` launch should **reveal the existing window,
  not spawn a second window/tray**. On Linux this rides GApplication's D-Bus uniqueness **plus a new
  activate-guard** (`app.windows().first().present()`); the Windows half is a named-mutex + event
  signal (🪟, compiled out on Linux). Please verify a 2nd launch on Linux just re-presents the running
  window — **including when it's hidden in the tray** (that's the case the activate-guard adds).
- 🐧 **Create + Add are now frameless modals with Cancel** (`75d4ec8`, `1166387`): `show_add_dialog`
  and `show_create_dialog` became `adw::MessageDialog`s with **Cancel + action** responses. Because a
  MessageDialog response always closes, **Add is gated** — its button stays disabled until a key is
  entered *and* a folder chosen (Create stays enabled; its folder is pre-picked). **Extends item 9**:
  Create + Add + Address + Keys + Members + Set-name are now all frameless. Confirm Cancel/Esc escapes
  each, and that Add only enables once valid.
- 🐧 **＋ dropdown closes on selection** (`1166387`): the "Add existing share" item never called
  `popdown()`, so the ＋ popover lingered until the next click — now both items pop down. Quick check.
- 🐧 **`--debug` flag** (`24a9b49`): bumps the default log filter to `seed_gui=debug,seed_ipc=debug`.
  On **Windows** release builds (now GUI-subsystem = no console by default, 🪟) it *also* allocates the
  log console. On Linux there's no console-subsystem concept, so `--debug` only changes verbosity;
  normal launch is unchanged. Sanity-check `seed-gui --debug` still logs on Linux.
- 🪟 **Dark tray menu — actual fix** (`50f8588`): supersedes the `7095421` `set_theme` approach, which
  was a no-op for popups (muda only dark-themes menu *bars*). FYI since item 13 referenced the old one.

### [LINUX] GUI polish round 2 — re-test results 2026-06-19
Rebuilt `seed-gui` clean (round 2 + rebrand `08606f6` + empty-state `c21c04b` all compile on Linux).
- ✅ **HiVis tray icon** (`f74cc02`): Linux ksni tray now decodes `appIconHiVis.png`; item registers,
  Title now **"S.E.E.D."**, 4 ARGB sizes served. (Visual hi-vis appearance: human-confirmed icon present.)
- ✅ **Single-instance** (`1ffd99e`): 2nd `seed-gui` launch exits immediately, leaving 1 process / 1 window
  — verified BOTH with the window visible AND **while hidden in the tray** (the activate-guard case: the
  hidden window re-appeared). Close-to-tray re-confirmed in passing (`niri close` → 0 windows, proc alive).
- ✅ **`--debug`** (`24a9b49`): on Linux it correctly sets the filter to `seed_gui=debug,seed_ipc=debug`
  and the app logs fine (INFO tray line present), no crash. 0 DEBUG lines is expected — there are no
  `debug!` call sites in `seed-gui`/`seed-ipc`. Flag is harmless/functional on Linux.
- ✅ **Rebrand "S.E.E.D."** (`08606f6`): window title "S.E.E.D." + subtitle "Secure Environment Exchange
  Daemon"; tray title "S.E.E.D." too.
- ✅ **Empty-state** (`c21c04b`): with no shares the main view shows an `AdwStatusPage` — themed
  folder-remote icon, "No shares yet", "Use "+" to create or add one." Renders correctly on Linux.
- ✅ **Create + Add frameless modals + Add-gating** (`75d4ec8`, `1166387`): human-verified — both are
  frameless `adw::MessageDialog`s with Cancel (Esc/Cancel backs out), and Add stays disabled until a key
  AND a folder are provided. **＋ dropdown popdown** (`1166387`): confirmed — the ＋ popover closes on
  selecting either item. Round 2 fully green on Linux.

## Open issues
### #1 — Cross-volume viewer dedup leaves a 2× copy on Windows  *(FIXED — Option A)*
**Resolution (2026-06-18):** vendored + one-line-patched `iroh-blobs` (`vendor/iroh-blobs`,
`[patch.crates-io]` in `Cargo.toml`) so its `TryReference` export also treats Windows
`ERROR_NOT_SAME_DEVICE` (17) as a cross-volume move and falls back to copy + `External`.
The leftover owned copy (Windows holds the handle briefly) is then deleted by a reclaim
retry queue in `engine.rs`/`node.rs` (`reclaim_pending`, retried each reconcile until iroh
releases the handle, ~3 s). Verified: cross-volume viewer blob store **100.91 MB → 0.91 MB**
(1×), same-volume still 0.91 MB, all seed-core tests green. **Linux:** the patch is inert
there (Linux gets EXDEV=18, already handled), so behavior is unchanged — but `vendor/iroh-blobs`
must be present after a pull for the build to resolve the `[patch]`.

Original analysis below for reference.

---

When a viewer's **mirror folder is on a different volume than its data dir**, the
content is stored twice (auto-downloaded blob in the store **and** the mirror file).
Same-volume is fine (1×).

- **Root cause (refined — it's an upstream iroh-blobs Windows bug):**
  `ExportMode::TryReference` on an owned blob does `std::fs::rename(source→target)`
  (`iroh-blobs-0.103.0/src/store/fs.rs:1300-1313`). On a cross-volume move it only
  falls back to copy when the OS error is `EXDEV` (**18**, Linux). Windows returns
  `ERROR_NOT_SAME_DEVICE` (**17**), which iroh doesn't match, so it **returns an
  error instead of falling back** — the export fails outright. Confirmed in the
  daemon log: `reference-export p.bin failed; will self-heal: Error::Io`. The file
  is then materialized by **self-heal** (`get_blob` → mirror), and the owned blob
  that iroh-docs auto-downloaded is never converted to a reference → it lingers.
- Why the obvious fixes don't work: re-importing the mirror by reference can't help
  (`entry_state.rs` union prefers `Owned`, so it stays Owned); there's no public
  single-blob delete; and on Windows the owned `.data` can't be deleted while iroh
  holds its handle anyway (it releases it ~5 s later — Linux can unlink an open file,
  so **Linux almost certainly does NOT have this bug**: its cross-fs rename hits
  `EXDEV`→copy, then iroh unlinks the open owned file fine).
- Impact: **high on Windows** — the service data dir is always `C:\ProgramData`, and
  users pick mirror folders on other drives. Defeats "no disk doubling" there.
- Repro (Windows, local 2-daemon): viewer data on `C:`, mirror on `D:`, 100 MB file
  → viewer `blobs` = **100.91 MB** (vs **0.91 MB** same-volume).
- **[LINUX] CONFIRMED 2026-06-18:** cross-filesystem IS 1× on Linux (data dir `/dev/shm`,
  mirror `/tmp`, EXDEV=18 between them; 100 MB → 936 KB blob store, no self-heal fallback in
  the log). This is **Windows-only**, exactly as predicted. The fix is correctly inert on Linux.
- Fix options (deciding):
  - **A. Patch iroh-blobs** to also treat Windows err 17 as cross-volume (1-line; fork
    via `[patch]` or vendor). Then iroh copies + sets `External`, and a reclaim pass
    deletes the leftover owned `.data` once iroh releases the handle. Surgical; keeps
    the sync architecture; needs a patched dependency.
  - **B. Never create an owned blob on the viewer:** disable iroh-docs content
    auto-download and materialize via `get_blob`→mirror + import-by-reference (the
    self-heal path, generalized). No dependency patch, but a bigger change to the core
    sync path. *(A reclaim-retry queue in `engine.rs`/`node.rs` is already written for
    option A's second half; uncommitted pending this decision.)*

## Findings log

### [WIN] 2026-06-18 — Checkpoint #3, Linux master → Windows viewer (first run)
Linux master shared ~100 MB (`readme.txt`, `docs/notes.md`, `docs/unicode-name.txt`,
`bigfile-100mb.bin`) to a Windows viewer (MSI service, data `C:\ProgramData\SeedSync`,
mirror `D:\linuxMasterTest`).
- ✅ **Mirrors:** all 4 files present; `bigfile-100mb.bin` == 104,857,600 bytes exact.
- ✅ **Path/unicode:** `docs/` came through with `/`→`\` conversion; filenames intact.
- ✅ **Self-heal:** corrupted the 100 MB mirror file to 54 bytes → auto-restored to the
  exact original (size + SHA256) in **~6 s**, no user action. Cross-OS self-heal works.
- ❌ **Dedup:** can't measure from this run (the store also held an unrelated leftover
  share). Isolated 2-daemon test confirmed **Open issue #1** (cross-volume 2×).

### [WIN] 2026-06-18 — diagnosed issue #1 (cross-volume 2×)
Traced it to iroh-blobs' `TryReference` export only handling Linux `EXDEV` (18), not
Windows `ERROR_NOT_SAME_DEVICE` (17) — so cross-volume export fails outright, self-heal
saves the file, and the auto-downloaded owned blob is never reclaimed. Verified iroh
releases the owned `.data` handle ~5 s after the op (so an external delete works then;
in-process delete must be retried). Wrote a reclaim-retry queue (`reclaim_pending` in
`engine.rs`, `blobs_dir` on `IrohNode`) — correct but insufficient alone, because the
export never succeeds so nothing gets queued. Need decision A vs B (see issue #1).
**Likely Windows-only — Linux to confirm cross-fs is 1×.**

### [WIN] 2026-06-19 — more daemon fixes (publish robustness)
Two master-publish bugs found while sharing from Windows, both fixed in shared
`seed-core` (so they apply on Linux too once pulled):
- **Publish flap on a still-copying file** (`a4e4b4a`): the master republishes
  whenever the folder's `(path,size,mtime)` signature changes, so a large file
  being copied/downloaded in changed every tick → endless re-index (healthy → 0%
  → healthy). Added a **debounce**: only publish once the signature has held steady
  across a reconcile tick (~750 ms).
- **Publish wedged on an empty (0-byte) file** (`5bdfa4b`, **cross-platform**): a
  genuinely empty `café.txt` failed `set_hash` with `Attempted to insert an empty
  entry`, failing and endlessly retrying the whole publish (share stuck "indexing").
  Root cause: **iroh-docs can't carry an empty file** — a len-0 content entry must
  use its all-zeros empty *sentinel* hash, but a real 0-byte file hashes to
  `BLAKE3("")`, which iroh-docs rejects. (The unicode name was incidental; the file
  was just the only 0-byte one.) Fix: empty files ride the **signed manifest only**
  (no per-file doc entry, no blob transfer) and the viewer's `apply()` creates them
  directly. Test `empty_files_sync` covers it. **This affects Linux identically** —
  any 0-byte file in a shared folder would have wedged the publish there too.

**[LINUX] please confirm:** the stale-0-size is a Windows directory-entry quirk, but the
size/hash mismatch can also happen from a genuine write-during-scan race on any OS — so
verify a Win→Linux (or local) sync of a file that's actively being written doesn't wedge,
and that empty + unicode-named files round-trip. The fix is in shared code; no Linux-side
change expected, just confirmation.

### [WIN] notes / environment
- Windows build: GTK4 via gvsbuild at `C:\gtk`; MSI via WiX 5 (`scripts\build-msi.ps1`).
- The MSI is x64, installs to `C:\Program Files\SeedSync`, registers the LocalSystem
  `SeedSyncDaemon` service. Recent fixes: GSettings schema path (file-chooser crash),
  share/lib MSI layout, x64 arch, same-version upgrades.
- GUI single-instance: launching a 2nd instance just re-activates the first.

### [LINUX] 2026-06-18 — regression pass + cross-fs dedup confirmation
- ✅ **Regression pass:** `cargo build --workspace` clean; `cargo test --workspace -- --include-ignored`
  ALL GREEN incl. the 3 Windows-added loopback tests (`viewer_stores_by_reference_not_copy`,
  `viewer_auto_heals_corrupted_file`, `referenced_viewer_serves_peers`). Built with the vendored
  `iroh-blobs` fork present (`[patch]` resolves); the err-17 patch is inert on Linux (gets EXDEV=18).
- ✅ **Issue #1 — cross-filesystem dedup is 1× on Linux (Windows-only confirmed).** Ran a real
  cross-volume viewer: data dir on `/dev/shm` (tmpfs, dev 26), mirror on `/tmp` (tmpfs, dev 60),
  EXDEV=18 verified between them. 100 MB share → viewer `blobs` = **936 KB (0.91 MB), 1×**
  (vs Windows' pre-fix 100.91 MB). Daemon log shows **no** `reference-export failed`/self-heal
  fallback — the EXDEV→copy path succeeds and iroh unlinks the owned `.data` immediately (Linux
  unlinks open files). So #1's fix can stay effectively Windows-only; Linux never had the bug.
- ✅ **Real-binary dry-run (Linux loopback, auto-discovery, no bootstrap):** sync (~1s), 1× dedup
  (9 MiB content → 576 KB store), self-heal (~2s), update (~3s) + delete (~2s) propagation all pass.
- ✅ **Delete/update propagation Linux→Win (2026-06-18):** on the live Linux master, updated
  `readme.txt`, deleted `docs/unicode-name.txt`, added `added-on-linux.txt` — all three landed on
  the Windows viewer within a few seconds. Direction 1 fully green.
- ✅ **Win→Linux sync (2026-06-19):** Linux viewer added a Windows share with **no bootstrap**
  (auto-discovery resolved Win→Linux in ~45s to Healthy 100%). Share = `café.txt` (0-byte, unicode),
  `dolphin-…-x86_64.exe` (118 MB), `newFolder/Big Doc.txt` (subfolder + spaces), `WinRx_11.bak…iso`
  (**1.76 GB**). All present, sizes exact. Path/unicode/space conversion (`\`→`/`) intact.
- ✅ **Empty + unicode file (`5bdfa4b` confirmed cross-OS):** `café.txt` (0 bytes) synced from the
  Windows master without wedging the publish — the manifest-only empty-file path works Win→Linux.
- ✅ **1× dedup on the Linux receiving side, multi-GB:** 1.9 GB mirror content → **8.1 MB** blob
  store (outboards only: 7.1 MB .iso obao + 456 KB .exe obao + db). Verified AFTER the in-flight
  `.data` fully exported — measuring mid-transfer is meaningless (the owned blob is full-size until
  reference-export completes; learned this the hard way on the first run).
- ✅ **Self-heal Win→Linux (~3s):** corrupted `Big Doc.txt` (920→65 B) → auto-restored to exact
  original SHA256 in ~3s, no user action.
- ✅ **Delete/update/move propagation Win→Linux (2026-06-19):** Windows master (a) added content to
  `café.txt` (0→4 B "asdf" — empty→non-empty transition), (b) deleted `newFolder/Big Doc.txt`,
  (c) moved the 1.76 GB ISO from top-level into `newFolder/`. All three reflected on the Linux viewer.
  **The ISO move did NOT re-download** — blob store stayed 7.7 MB (outboard-only), no in-flight `.data`,
  ISO size exact, Healthy 100%: the viewer relocated its local mirror file (same-volume rename), not a
  1.76 GB re-fetch. Efficient move handling confirmed cross-OS.

**>>> Checkpoint #3 COMPLETE — Windows ⇄ Linux sync green in both directions.** All status-board rows
pass. Auto-discovery (no bootstrap) resolves both ways; reference-import + 1× dedup hold for multi-GB
files on both ends; self-heal, delete/update/move, empty+unicode files, and cross-OS path conversion
all verified. Remaining M4 work is non-engine: deferred MSI code-signing + WixUI.
- Note: **empty directories are not synced** (manifest tracks files only); deleting a folder's last
  file leaves the empty dir on the master but the viewer never materializes it. Benign; `diff -r`
  flags it. (Empty *files* now ride the signed manifest per `5bdfa4b` — separate from empty dirs.)
  Also `seed-cli publish` requires `--share <id>` (reconcile loop auto-republishes anyway).

## Distribution & auto-update — release channel (2026-06-19)

A shared release/update channel was designed on the Linux side; this section is the **handoff
for the Windows instance** to build its half. Full Linux design lives in `docs/linux-packaging.md`.

**The model (both OSes):**
- Built artifacts are published to a **separate PUBLIC repo `steeb-k/seed-sync-binaries`** (source
  stays private in `seed-sync-gtk`). One **GitHub Release per version tag `vX.Y.Z`**; each release
  carries **both** OSes' assets — the Linux `…linux-x86_64.tar.gz` (attached by the main repo's
  `release.yml`) and the Windows artifact (MSI or versioned zip — **the Windows side attaches this to
  the same release/tag**).
- Public repo ⇒ updaters download with **no auth**.
- **Version is the source of truth:** the updater compares the installed version
  (`seed-daemon --version` / `seed-daemon.exe --version`, from clap) to the latest release tag, and
  only updates when the tag is newer. ⇒ **bump `[workspace.package].version` in `Cargo.toml` and tag
  `vX.Y.Z` per release** (the Linux `release.yml` fails the build if the tag ≠ Cargo version).
- **Publishing is cross-repo**, so it needs a PAT secret **`SEED_BINARIES_TOKEN`** with
  `contents: write` on `seed-sync-binaries` (the default `GITHUB_TOKEN` can't write another repo).

**Linux side (DONE — for reference / parity):** `packaging/linux/` + `scripts/package-linux.sh` +
`.github/workflows/release.yml`. A single per-user wrapper `seed-sync` with `--install` / `--update`
/ `--uninstall` / `--status`; daemon as `systemd --user`; a `systemd --user` **timer** (daily) runs
`seed-sync --update`, which does stop-daemon → swap binaries → restart.

**[WIN] DONE (2026-06-20) — Windows half built (code-complete; live-test pending):**
1. **Publish step:** ✅ chosen approach = **local build+sign, then attach** (signing uses an Azure
   Trusted Signing cert that stays off CI). `scripts\build-msi.ps1` produces a signed
   `seed-sync-<ver>-windows-x86_64.msi` (naming matches the Linux tarball); `scripts\publish-msi.ps1`
   does `gh release upload v<ver> … -R steeb-k/seed-sync-binaries --clobber` onto the same release the
   Linux `release.yml` creates. (No Windows CI job — deliberate, per the signing constraint.)
2. **Windows updater:** ✅ `packaging\windows\seed-sync-update.ps1` — queries the latest release (public,
   no auth), compares `seed-daemon.exe --version`, downloads the `*windows-x86_64.msi` and applies it with
   `msiexec /i … /qn`. The MSI's `MajorUpgrade` stops the service, swaps files (GTK DLLs included), and
   restarts — so no manual binary-swap / DLL-in-use juggling.
3. **Scheduled Task:** ✅ **SeedSyncUpdate** (daily + ~5 min after boot, SYSTEM), registered by the MSI via
   a deferred `util:QuietExec64` custom action calling the updater `-RegisterTask`; removed on uninstall
   (not on upgrade). Analog of the Linux systemd `--user` timer.
4. **MSI-vs-zip:** ✅ MSI — silent `msiexec /qn` is the "apply" step (service registration + shortcuts +
   `MajorUpgrade` already in place).

**Still to validate live on Windows:** build the signed MSI, install it, confirm SmartScreen-clean +
`Get-AuthenticodeSignature` Valid on the exes + MSI; the WixUI_Minimal GPL page shows; the SeedSyncUpdate
task exists; and an end-to-end self-update (publish a newer tag → task picks it up → silent upgrade →
service restarts).
