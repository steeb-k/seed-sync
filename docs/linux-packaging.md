# Linux packaging, distribution & auto-update — maintainer guide

This is the baseline for shipping S.E.E.D. (Seed Sync) on Linux and keeping installs
up to date. It's the Linux counterpart to `docs/windows-packaging.md`.

## TL;DR for cutting a release
1. Bump the version: edit `[workspace.package].version` in the root `Cargo.toml`.
2. Commit, then tag and push:
   ```sh
   git tag v0.1.1 && git push origin v0.1.1
   ```
3. `.github/workflows/release.yml` builds the tarball and publishes it to the **public**
   `steeb-k/seed-sync-binaries` repo as a GitHub Release named after the tag.
4. Installed machines pick it up automatically within a day (the `seed-sync-update.timer`),
   or immediately with `seed-sync --update`.

That's it. The rest of this document explains the moving parts.

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
```
  main repo (private)                 seed-sync-binaries (PUBLIC)         user machine
  ───────────────────                 ──────────────────────────         ────────────
  git tag vX.Y.Z  ──►  release.yml ──►  Release "vX.Y.Z"          ◄─── seed-sync --update
   (Cargo version)     builds tarball   ├─ ...linux-x86_64.tar.gz  poll   (timer, daily)
                       publishes via    └─ ...windows...  (added    +     compares to
                       SEED_BINARIES_TOKEN   by the Windows side)  fetch  `seed-daemon --version`
```
- Artifacts live in a **separate public repo** so machines download with **no auth**. Source
  stays private in `seed-sync-gtk`.
- The **installed version is the source of truth**: the updater reads `seed-daemon --version`
  (provided by clap) and compares it to the latest release tag. So **the Cargo version must be
  bumped per release** or the updater will never see a newer version.

## One-time setup (do this once, ever)
1. **Create the public artifact repo** `steeb-k/seed-sync-binaries` (empty is fine; it just
   holds Releases). Public so the updater needs no credentials.
2. **Create a token** with `contents: write` on `seed-sync-binaries` (classic PAT with `repo`,
   or a fine-grained token scoped to that one repo), and add it to the **main repo** as the
   Actions secret **`SEED_BINARIES_TOKEN`**. The default `GITHUB_TOKEN` can't write to another
   repo, which is why this is required.
3. **Publish the bootstrap** `packaging/linux/web-install.sh` to the `seed-sync-binaries` repo
   root as **`install.sh`** (it's served via the raw URL the one-liner uses). It's stable and
   rarely changes — if you edit `web-install.sh`, re-copy it:
   ```sh
   tmp=$(mktemp -d); gh repo clone steeb-k/seed-sync-binaries "$tmp/b"
   cp packaging/linux/web-install.sh "$tmp/b/install.sh"
   git -C "$tmp/b" commit -am "update bootstrap" && git -C "$tmp/b" push
   ```

End users then install/update/remove with one command (detects state, prompts):
```sh
curl -fsSL https://raw.githubusercontent.com/steeb-k/seed-sync-binaries/main/install.sh | sh
```

## What each file is for
All packaging inputs live in `packaging/linux/` and are assembled into the tarball by
`scripts/package-linux.sh`.

| File | Purpose |
|---|---|
| `scripts/package-linux.sh` | Builds the release: `cargo build --release`, renders hicolor icon sizes from `icon/appIcon.png` (needs ImageMagick), stages the tree, and writes `dist/seed-sync-<ver>-linux-x86_64.tar.gz`. Run with `--skip-build` to repackage existing binaries. |
| `.github/workflows/release.yml` | On a `v*` tag: runs the package script on **ubuntu-24.04** (GUI needs GTK 4.10+), checks the tag matches the Cargo version, and publishes the tarball to `seed-sync-binaries`. |
| `packaging/linux/seed-sync` | **The one wrapper** — installer, updater, and uninstaller in a single script, installed to `~/.local/bin/seed-sync`. `--install [--no-auto-update] [--no-gui-autostart]` places files (from the tarball it shipped in, or downloads if run standalone), enables the daemon + update timer, adds the tray autostart entry, runs a dep check. `--update [--check]` downloads the latest, version-compares vs `seed-daemon --version`, and applies (stop daemon → swap → restart). `--uninstall [--purge]` removes everything. `--status` shows installed/latest/service state. A shared internal `apply_tree` does the atomic file placement for both install and update. |
| `packaging/linux/web-install.sh` | **The `curl \| sh` bootstrap.** POSIX sh, no args needed. Detects whether S.E.E.D. is installed and prompts (install / update / remove) via `/dev/tty`; non-interactive via `sh -s -- install\|update\|remove` or `$SEED_ACTION`. First install downloads the latest tarball and runs its `seed-sync --install`; update/remove on an existing install just delegate to the installed `seed-sync`. **Served from the `seed-sync-binaries` repo root as `install.sh`** (raw URL), mirrored from this file — re-copy it there if you change it (see below). |
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
- **ABI portability.** The tarball is dynamically linked against the build host's glibc + GTK.
  CI builds on **ubuntu-24.04** — not 22.04, because the GUI requires **GTK 4.10+** (the `v4_10`
  feature) and 22.04 only ships GTK 4.6, which fails the build. Targets therefore need **GTK 4.10+ /
  libadwaita 1.4+** and a correspondingly modern **glibc (≥ 2.39)** — in practice Ubuntu 24.04+,
  Fedora 39+, Debian 13+, or a rolling distro (Arch/CachyOS). Older distros can't run the app anyway
  (no GTK 4.10+), so this floor isn't an extra restriction. If a fleet consolidates on one distro,
  prefer a real native package then (see Future work).
- **Version bump is mandatory per release** — the updater is version-driven. CI fails the
  release if the tag doesn't match the Cargo version (guard in `release.yml`).
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
- macOS packaging.

See also: `docs/windows-packaging.md` (the MSI side) and the release/update handoff section
in `docs/cross-os-testing.md` (what the Windows updater must mirror).
