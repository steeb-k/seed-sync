# Linux packaging, distribution & auto-update — maintainer guide

This is the baseline for shipping S.E.E.D. (SEED Sync) on Linux and keeping installs
up to date. It's the Linux counterpart to `docs/windows-packaging.md`. For the
release runbook and the shared distribution model, see
[`releasing.md`](releasing.md); the rest of this document is the Linux mechanics.

## Architecture (why it's built this way)
S.E.E.D. is **not a typical sandboxed GUI app** — it's a *per-user background daemon* +
GUI + CLI. The daemon continuously reads/writes **arbitrary user-chosen folders** and needs
the keyring, full network (iroh), the session bus (tray), and system GTK 4.10+/libadwaita 1.4+.

- **Flatpak was rejected:** a daemon doing continuous R/W to arbitrary folders can't use
  per-file portal grants; you'd need `--filesystem=host` + `--share=network` + secrets +
  session bus, which guts the sandbox while paying its complexity. No real benefit.
- **AppImage was rejected:** GTK4 bundling is fiddly and it does nothing for the daemon
  autostart problem.
- **Chosen: a distro-agnostic tarball** installed per-user, with the daemon run as a
  `systemd --user` service (the Linux analog of the Windows `SeedSyncDaemon` service), and a
  `systemd --user` timer for hands-off auto-update.

### Distribution / update flow

The public-repo, version-driven, no-CI distribution model is shared across all
platforms and documented once in [`releasing.md`](releasing.md#distribution-model).
On Linux the updater is `seed-sync --update`, run daily by `seed-sync-update.timer`.

## One-time setup (do this once, ever)
1. **Create the public artifact repo** `steeb-k/seed-sync-binaries` (empty is fine; it just
   holds Releases). Public so the updater needs no credentials.
2. **Get publish access to `seed-sync-binaries`** — either `gh auth login` as an account with
   `repo` scope (the maintainer's `steeb-k` account has it), or create a token with
   `contents: write` on `seed-sync-binaries` (classic PAT with `repo`, or a fine-grained token
   scoped to that one repo) and pass it to `gh` via `GH_TOKEN` / `SEED_BINARIES_TOKEN` when
   publishing locally.
3. **Publish the bootstrap** `packaging/linux/web-install.sh` to its two served locations (both
   stable, rarely change). It's mirrored — re-copy to both if you edit `web-install.sh`:
   - **`steeb-k.github.io/seed-install.sh`** — the canonical end-user URL (GitHub Pages, served from
     the `steeb-k.github.io` repo root). That repo's Pages deploys via its `deploy-kodi-repository.yml`
     workflow, whose `paths:` filter includes `seed-install.sh`, so pushing an updated copy auto-deploys.
   - **`seed-sync-binaries/install.sh`** — a raw-URL fallback (`raw.githubusercontent.com/.../main/install.sh`).
   ```sh
   # github.io (canonical):
   tmp=$(mktemp -d); gh repo clone steeb-k/steeb-k.github.io "$tmp/s"
   cp packaging/linux/web-install.sh "$tmp/s/seed-install.sh"
   git -C "$tmp/s" commit -am "update bootstrap" && git -C "$tmp/s" push   # auto-deploys via Pages workflow
   # binaries-repo fallback:
   tmp2=$(mktemp -d); gh repo clone steeb-k/seed-sync-binaries "$tmp2/b"
   cp packaging/linux/web-install.sh "$tmp2/b/install.sh"
   git -C "$tmp2/b" commit -am "update bootstrap" && git -C "$tmp2/b" push
   ```

End users then install/update/remove with one command (detects state, prompts):
```sh
curl -fsSL https://steeb-k.github.io/seed-install.sh | sh
```

## What each file is for
All packaging inputs live in `packaging/linux/` and are assembled into the tarball by
`scripts/package-linux.sh`.

| File | Purpose |
|---|---|
| `scripts/package-linux.sh` | Builds the release: `cargo build --release`, renders hicolor icon sizes from `icon/appIcon.png` (needs ImageMagick), stages the tree, and writes `dist/seed-sync-<ver>-linux-x86_64.tar.gz`. Run with `--skip-build` to repackage existing binaries. |
| (release publishing) | Built locally — run `scripts/package-linux.sh` on **Ubuntu 24.04** (GUI needs GTK 4.10+; WSL works) and publish the tarball to `seed-sync-binaries` with `gh`. See [`releasing.md`](releasing.md). There is no CI workflow. |
| `packaging/linux/seed-sync` | **The one wrapper** — installer, updater, and uninstaller in a single script, installed to `~/.local/bin/seed-sync`. `--install [--no-auto-update] [--no-gui-autostart]` places files (from the tarball it shipped in, or downloads if run standalone), enables the daemon + update timer, adds the tray autostart entry, runs a dep check. `--update [--check]` downloads the latest, version-compares vs `seed-daemon --version`, and applies (stop daemon → swap → restart). `--uninstall [--purge]` removes everything. `--status` shows installed/latest/service state. A shared internal `apply_tree` does the atomic file placement for both install and update. |
| `packaging/linux/web-install.sh` | **The `curl \| sh` bootstrap.** POSIX sh, no args needed. Detects whether S.E.E.D. is installed and prompts (install / update / remove) via `/dev/tty`; non-interactive via `sh -s -- install\|update\|remove` or `$SEED_ACTION`. First install downloads the latest tarball and runs its `seed-sync --install`; update/remove on an existing install just delegate to the installed `seed-sync`. **Served at `steeb-k.github.io/seed-install.sh`** (canonical) and `seed-sync-binaries/install.sh` (raw fallback), mirrored from this file — re-copy to both if you change it (see One-time setup). |
| `packaging/linux/seed-daemon.service` | `systemd --user` unit that runs `seed-daemon run`, restarts on failure, and auto-starts at login (`WantedBy=default.target`). |
| `packaging/linux/seed-sync-update.service` | `systemd --user` **oneshot** that runs `seed-sync --update` (invoked by the timer). |
| `packaging/linux/seed-sync-update.timer` | `systemd --user` timer: shortly after login + daily, with a randomized delay and `Persistent=true` (catches up if the machine was off). |
| `packaging/linux/io.github.steeb_k.SeedSync.desktop` | App-menu launcher. `Exec=__BIN__/seed-gui` — the `__BIN__` placeholder is rewritten to the real `~/.local/bin` at install time. The filename + `Icon=` match the GTK app id so the window/tray/icon associate. |
| `packaging/linux/io.github.steeb_k.SeedSync.metainfo.xml` | AppStream metadata (name/summary/categories) for software centers. |
| `packaging/linux/INSTALL.txt` | End-user readme shipped inside the tarball. |

### Tarball layout (what users extract)
```
seed-sync-<ver>-linux-x86_64/
├── bin/{seed-daemon,seed-gui,seed-cli}
├── seed-sync                   # the wrapper; also copied into ~/.local/bin on install
├── INSTALL.txt
├── lib/systemd/user/{seed-daemon.service,seed-sync-update.service,seed-sync-update.timer}
└── share/
    ├── applications/io.github.steeb_k.SeedSync.desktop
    ├── metainfo/io.github.steeb_k.SeedSync.metainfo.xml
    └── icons/hicolor/<size>x<size>/apps/io.github.steeb_k.SeedSync.png
```

### Where things land on the user's machine (per-user, no root)
```
~/.local/bin/                     seed-daemon, seed-gui, seed-cli, seed-sync
~/.config/systemd/user/           seed-daemon.service, seed-sync-update.{service,timer}
~/.config/autostart/              io.github.steeb_k.SeedSync.desktop (tray, --hidden)
~/.local/share/applications/      io.github.steeb_k.SeedSync.desktop (launcher)
~/.local/share/icons/hicolor/...  app icon
~/.local/share/metainfo/          AppStream metadata
~/.local/share/seedsync/          DATA: state.db, blobs/, docs/, node.key, seed.sock
```
The data dir + socket are chosen by the `directories` crate; on Linux `ProjectDirs::from(
"io.github","steeb_k","SeedSync")` resolves the data dir to **`~/.local/share/seedsync`**
(the crate lowercases the app name on Linux — note the lowercase). The GUI and daemon agree
on it by default, so no socket args are needed. `SEED_SOCKET` overrides it if ever necessary.

## How an upgrade actually happens
1. `seed-sync-update.timer` fires → runs `seed-sync --update`.
2. It GETs `https://api.github.com/repos/steeb-k/seed-sync-binaries/releases/latest`
   (public), reads the tag, and compares to `seed-daemon --version`.
3. If newer: download the tarball to a temp dir, extract, then `apply_tree`:
   `systemctl --user stop seed-daemon` → atomically replace each binary (temp + `mv`, safe
   even while the GUI/daemon hold the old inode) → refresh `.desktop`/icons/units **only if
   changed** → `daemon-reload` if a unit changed → `systemctl --user start seed-daemon`.
4. A running GUI reconnects on its own (2 s retry loop); its binary updates on next launch.
5. Any failure aborts before the swap, leaving the current install intact.

Self-replacement of `seed-sync` is safe: `apply_tree` writes the new file to a temp name and
`mv`'s it into place, so the running shell keeps its open fd to the old inode for that run.

## Caveats / gotchas for maintainers
- **The tarball must ship LF line endings, and nothing on Linux tells you otherwise.**
  Packaging from a CRLF working tree (a Windows clone with `core.autocrlf=true`, or WSL
  packaging a `/mnt/c` checkout) copies `packaging/linux/*` verbatim, so the wrapper's
  shebang becomes `#!/usr/bin/env bash\r` and the web installer dies on the user's machine
  with `env: 'bash\r': No such file or directory` — after a successful download, which makes
  it read like a broken release rather than a broken checkout. This bit **v0.7.1**: every
  text file in that tarball (wrapper, `INSTALL.txt`, the three systemd units, the desktop
  entry, the metainfo XML) shipped CRLF. Two guards now: `.gitattributes` pins `eol=lf` in
  the working tree on every platform, and `package-linux.sh` copies text files through
  `install_text`, which strips CR at package time. To check a built tarball:
  `tar -xzf dist/*.tar.gz -O */seed-sync | head -1 | od -c | head -1`.
  **`.gitattributes` does not repair a clone that was already CRLF**, and it hides the fact
  that it hasn't: `text=auto` normalizes on read, so the blobs compare equal and `git status`
  reports a *clean* tree while the files on disk still have CRLF. A checkout predating the
  attributes file — or any clone made with `core.autocrlf=true` — therefore stays corrupt
  until it is re-checked-out: `git rm --cached -r . && git reset --hard` (commit or stash
  first; this touches every tracked file). Verify with
  `git ls-files -z | xargs -0 grep -lIU $'\r'` — `android/gradlew.bat` is the only permitted
  match (a clone predating `.gitattributes` may have it as LF and match nothing). Keep the
  `-I`: without it the icons, the Gradle jar and the `.xcf` sources match on stray CR bytes
  and bury the real hits.
  `install_text` is what actually makes packaging safe from such a tree; prefer it over `cp`
  for any new text file added to the tarball.
- **An update must cycle the tray GUI, not just the daemon.** `seed-sync --update`
  restarts `seed-daemon.service`, but the GUI is a plain user process — left alone it
  keeps running the **old binary** against the new daemon indefinitely. `apply_tree`
  now stops `seed-gui` before the swap and relaunches it `--hidden` afterwards.
- **Relaunching the GUI must escape the update unit's cgroup.** The daily update runs
  from `seed-sync-update.service` (`Type=oneshot`); systemd tears down that unit's
  cgroup when it finishes, killing any plain background fork with it. `start_gui_hidden`
  uses `systemd-run --user` (transient unit) and only falls back to `setsid`. It also
  no-ops when no `DISPLAY`/`WAYLAND_DISPLAY` is visible, rather than spawning a GUI
  that immediately dies.
- **ABI portability.** The tarball is dynamically linked against the build host's glibc + GTK.
  Build on **Ubuntu 24.04** — not 22.04, because the GUI requires **GTK 4.10+** (the `v4_10`
  feature) and 22.04 only ships GTK 4.6, which fails the build. Targets therefore need **GTK 4.10+ /
  libadwaita 1.4+** and a correspondingly modern **glibc (≥ 2.39)** — in practice Ubuntu 24.04+,
  Fedora 39+, Debian 13+, or a rolling distro (Arch/CachyOS). Older distros can't run the app anyway
  (no GTK 4.10+), so this floor isn't an extra restriction. If a fleet consolidates on one distro,
  prefer a real native package then (see Future work).
- **Version bump is mandatory per release** — the updater is version-driven. Bump the Cargo
  version (and `android/app/build.gradle.kts`) before building, or installed machines never
  see a newer release.
- **systemd --user requires a user session bus.** On headless/SSH boxes without a logind
  session, `systemctl --user` may be unavailable; `install.sh` warns and still places files.
  The keyring (secret service) likewise needs the session — the engine already falls back to
  DB-stored keys after a 5 s timeout if it's absent.
- **The updater is per-user.** A root/`--system` install would need a root updater + a
  system unit; not built (noted in `install.sh`).
- **Icons are pre-rendered at package time** (build host has ImageMagick) so target machines
  need no tools. If you change `icon/appIcon.png`, the next release regenerates all sizes.

## Future work (not built)
- Native per-distro packages (`.deb` via `cargo-deb`, Arch `PKGBUILD`, `.rpm` via
  `cargo-generate-rpm`) + self-hosted apt/pacman repos, if a fleet standardizes.
- A GUI "Check for updates" affordance (the daemon/GUI could surface the timer's result).
- Code signing / minisign-style artifact signatures for tamper-evidence.
- macOS mirrors this model (script/tarball + launchd, bundled GTK, ad-hoc signed).
  See `docs/macos-packaging.md`.

See also: `docs/windows-packaging.md` (the MSI side) and `docs/releasing.md` (the
cross-platform release runbook and the shared distribution model).
