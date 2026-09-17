# Linux packaging, distribution & auto-update — maintainer guide

This is the baseline for shipping S.E.E.D. (SEED Sync) on Linux and keeping installs
up to date. It's the Linux counterpart to `docs/windows-packaging.md`. For the
release runbook and the shared distribution model, see
[`releasing.md`](releasing.md); the rest of this document is the Linux mechanics.

## Architecture (why it's built this way)
S.E.E.D. is **not a typical sandboxed GUI app** — it's a *per-user background daemon* +
GUI + CLI. The daemon continuously reads/writes **arbitrary user-chosen folders** and needs
the keyring, full network (iroh), the session bus (tray), and system GTK 4.10+/libadwaita 1.4+.

- **Flatpak needs a wide sandbox, not a narrow one.** A daemon doing continuous R/W to
  arbitrary user-chosen folders can't use per-file portal grants — those hand back a
  one-shot, revocable handle to a single picked path, useless for "keep this folder
  mirrored indefinitely" — so the Flatpak build (added later, see
  [Flatpak](#flatpak) below) asks for `--filesystem=host` outright instead of pretending
  otherwise. It's still worth shipping: for users who prefer Flatpak's sandboxing and
  auto-update over the per-user tarball, most of the sandbox still holds (no arbitrary
  binary execution, network/bus access are the normal grants, GPU/display access is the
  normal portals) even though the filesystem grant is broad.
- **AppImage was rejected:** GTK4 bundling is fiddly and it does nothing for the daemon
  autostart problem.
- **Chosen: a distro-agnostic tarball** installed per-user, with the daemon run as a
  `systemd --user` service (the Linux analog of the Windows `SeedSyncDaemon` service), and a
  `systemd --user` timer for hands-off auto-update.

### Distribution / update flow

The public-repo, version-driven, CI-built distribution model is shared across all
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
- **A host firewall silently degrades the node to outbound-only.** With ufw/firewalld
  in default-deny (ufw's default when enabled), inbound QUIC dials are dropped and
  peers can only ever reach this node via relay-coordinated holepunching — which
  *looks* fine until the relay path degrades, and then two firewalled members flap
  in ~25 s online/offline cycles instead of connecting directly (the 2026-08
  two-member outage: Windows box with no service firewall rule + a ufw'd laptop).
  The daemon binds an **ephemeral UDP port**, so there is nothing stable to allow;
  ufw also cannot allow by program. Until a fixed-port setting exists (Future
  work), the options are a subnet-scoped allow (e.g.
  `sudo ufw allow from 192.168.50.0/24 comment 'SEED Sync LAN peers'`) or living
  with holepunch-only reachability. The installer deliberately does not touch the
  firewall — it's per-user and must not sudo. The Windows MSI *does* install a
  per-program inbound rule (see `windows-packaging.md` §3).

## Native packages (.deb / .rpm)
The tarball above is the release baseline; `.deb` and `.rpm` packages are also built,
for fleets that would rather use their distro's package manager than a per-user
self-updating install. They keep the same **per-user, no-root** daemon model as the
tarball — a package only changes *where the files live*, never who runs the daemon:

| Path | Content |
|------|---------|
| `/usr/bin/{seed-daemon,seed-gui,seed-cli,seed-sync}` | binaries + the wrapper (kept for `seed-sync --status`; see below) |
| `/usr/lib/systemd/user/seed-daemon.service` | a package variant of the unit — `ExecStart=/usr/bin/seed-daemon run` instead of the tarball's `%h/.local/bin/seed-daemon run` |
| `/usr/share/applications/io.github.steeb_k.SeedSync.desktop` | a package variant — `Exec=seed-gui`, no `__BIN__` placeholder to rewrite |
| `/usr/share/metainfo/io.github.steeb_k.SeedSync.metainfo.xml` | with a `<releases>` block generated from `CHANGELOG.md` |
| `/usr/share/icons/hicolor/<size>x<size>/apps/io.github.steeb_k.SeedSync.png` | same sizes as the tarball |
| `/usr/share/doc/seed-sync/copyright` (.deb) / `/usr/share/licenses/seed-sync/LICENSE` (.rpm) | license, each format's own convention |

Because `seed-daemon.service` is still a **`systemd --user`** unit, there is no single
system-level action a root postinstall scriptlet can take to start it — unlike a
typical root-daemon package, nothing here calls `systemctl enable --now` for you.
`postinstall`/`preremove` print the `systemctl --user …` command and otherwise leave
every user session alone; enabling it is a one-line, per-account step:
```sh
systemctl --user enable --now seed-daemon
```
The packages **do not** ship the update timer/service — a package-managed install
updates through the package manager, not `seed-sync --update`. `seed-sync` is still
installed (for `--status`, and so anyone still holding a copy of the wrapper gets a
clear answer), but it refuses `--install`/`--update`/`--uninstall` once it sees
`/usr/bin/seed-daemon` owned by a package manager (`refuse_if_pkg_managed`, checked
with `dpkg -S` / `rpm -qf` / `pacman -Qo`), naming the right command instead:
```
$ seed-sync --update
seed-sync: SEED Sync was installed as a dpkg-managed package (/usr/bin/seed-daemon); this wrapper does not manage it.
seed-sync: update with: sudo apt update && sudo apt upgrade; remove with: sudo apt remove seed-sync
```
Running the tarball installer over a package (or vice versa) is a real conflict — a
per-user `~/.local/bin` is typically *ahead* of `/usr/bin` on desktop `$PATH`s, so the
two would silently fight over which binary answers first. If you're switching from
the tarball to a package, run `seed-sync --uninstall` (as that user) first; your share
keys and data in `~/.local/share/seedsync` are untouched either way. If you find out
the hard way — package installed, tarball still in `~/.local/bin` — that cleanup
still works: the refusal only applies when `~/.local/bin/seed-daemon` is *absent*, so
with both present the wrapper keeps managing your per-user copy, which is also the
one your `$PATH` is running.

**Builder:** [nfpm](https://github.com/goreleaser/nfpm), driven by
`packaging/linux/nfpm.yaml`, fed from the tree `scripts/package-linux.sh` stages:
```sh
scripts/package-linux.sh              # stages dist/seed-sync-<v>-linux-x86_64/
scripts/package-linux-native.sh       # -> dist/seed-sync_<v>-1_amd64.deb, dist/seed-sync-<v>-1.x86_64.rpm
```
`package-linux-native.sh` copies the staged tree to `dist/native/tree` (nfpm's
content paths take no variables, so the path nfpm.yaml references is fixed), overlays
the `/usr/bin` unit and desktop file from `packaging/linux/pkg/`, generates the
metainfo `<releases>` block from `CHANGELOG.md`'s `## [X.Y.Z] - YYYY-MM-DD` headings
(a single entry for the current version if the repo has no `CHANGELOG.md` yet; every
`<release>` gets a `date`, falling back to the build date, because `appstreamcli
validate` flags one without), pins the wrapper's shebang to `/bin/bash` (`env` is a
`lintian`/`rpmlint` finding; the tarball keeps it), and calls `nfpm package` twice.
It requires the staged tree to already exist and requires `nfpm` on `PATH`; it does
not build or stage anything itself, and it works on a copy — the staged tarball tree
is never modified.

Dependencies are declared by soname/package so they resolve correctly on whichever
distro installs them: deb `libgtk-4-1 (>= 4.10)`, `libadwaita-1-0 (>= 1.4)`,
`libdbus-1-3`, `libc6 (>= 2.39)`, `libgcc-s1` (Rust's unwinder); rpm the equivalent sonames
(`libgtk-4.so.1()(64bit)` etc.) since Fedora and openSUSE name the packages
differently. deb compresses with xz (every dpkg reads it; `lintian` rejects a
zstd `data.tar`), rpm with zstd.

Maintainer scripts (`packaging/linux/scripts/{pre,post}{install,remove}.sh`, shared
between the .deb and .rpm scriptlet forms) never touch a user session:
- `preinstall` is a no-op scriptlet-arg dispatcher — there's no reliable system-wide
  way to detect a per-user tarball install from a root pre-install hook the way
  Nullgate's root-daemon packaging can check `/usr/local/bin`.
- `postinstall` self-configures the rpm repository (below) on a **first install
  only**, then prints the `systemctl --user enable --now seed-daemon` hint.
- `preremove` warns that it isn't stopping anything running, and on an rpm erase
  (not an upgrade) takes back the repository files it added, unless they were edited.
- There is no `postremove`: `seed-daemon.service` lives under
  `/usr/lib/systemd/user`, which the *system* manager never loads, so there is no
  system-level `daemon-reload` to run (and lintian flags an empty `postrm`).

## Arch Linux / AUR
`packaging/arch/PKGBUILD` builds `seed-sync` from the **source tarball of the
tagged release** (`https://github.com/steeb-k/seed-sync/archive/refs/tags/v$pkgver.tar.gz`,
which extracts to `$pkgname-$pkgver`), not a binary package:
`depends=(bash dbus gdk-pixbuf2 glib2 glibc gtk4 hicolor-icon-theme libadwaita libgcc pango)`, `makedepends=(cargo pkgconf imagemagick)` (icons are
rendered from `icon/appIcon.png` at build time, the same `ICON_SIZES` as
`package-linux.sh`), `options=('!lto')` (makepkg's LTO compiles `ring`'s C bits to
GCC LTO bitcode, which rustc's lld linker can't read — this bit both Nullgate and
would bite SEED Sync identically), and `cargo build --release --locked -p seed-daemon
-p seed-gui -p seed-cli`. It installs the same `/usr`-rooted layout as the .deb/.rpm,
including the per-user unit and the `packaging/linux/seed-sync` wrapper (kept for
`--status`/`refuse_if_pkg_managed`, same reasoning as above).
`packaging/arch/seed-sync.install` follows Arch convention — no service is
enabled/started automatically — and prints the same `systemctl --user` hints as the
.deb/.rpm scripts, plus a note to remove any per-user tarball install first.

**CI proof:** `scripts/ci/arch-pkgbuild.sh`, run in an `archlinux:latest` container
against a `git archive` of the tag (standing in for the not-yet-published tag
download): `makepkg`, `namcap`, `pacman -U`, a check that `seed-daemon --version` and
the unit's `ExecStart` are right, that `seed-sync --update` refuses
(`refuse_if_pkg_managed`), then `pacman -R`. The built `.pkg.tar.zst` is copied out
for the release to attach.

**AUR workflow** (manual, after a release is published — this package does not
build itself into the AUR):
```sh
git clone ssh://aur@aur.archlinux.org/seed-sync.git ../seed-sync-aur   # once
scripts/aur-prepare.sh 0.8.0 ../seed-sync-aur
cd ../seed-sync-aur && git diff && git commit -am "seed-sync 0.8.0" && git push
```
`aur-prepare.sh` copies `PKGBUILD`/`seed-sync.install` into the clone, sets `pkgver`
(and `pkgrel`), replaces the in-repo `sha256sums=('SKIP')` with the real sha256 of
the now-published tag archive, and regenerates `.SRCINFO` (via `makepkg
--printsrcinfo`, or an `archlinux` container if `makepkg` isn't available locally).
It never commits or pushes — review the diff first.

## Flatpak
`packaging/flatpak/io.github.steeb_k.SeedSync.yml` builds a single-file `.flatpak`
bundle — a third distribution channel alongside the tarball and the native
packages above, for users who'd rather install and auto-update through Flatpak.
It targets `org.gnome.Platform`/`org.gnome.Sdk` branch `50` (GTK4 + libadwaita
come from the runtime) plus the `org.freedesktop.Sdk.Extension.rust-stable` SDK
extension to build. Build it with:
```sh
scripts/package-flatpak.sh
```
which has flatpak-builder install the runtime/SDK/extension from Flathub into the user installation
on first run, then writes `dist/io.github.steeb_k.SeedSync-<version>-x86_64.flatpak`
(the exact name the release pipeline's asset table expects — see
`docs/ci-release.md` §3). Install a downloaded bundle with:
```sh
flatpak install --user ./io.github.steeb_k.SeedSync-<v>-x86_64.flatpak
```

### Permissions, and why
The manifest's `finish-args` comment block has the full reasoning per
permission; in short:
- `--share=network` — iroh (QUIC + relay fallback), the whole point of the app.
- `--socket=wayland` / `--socket=fallback-x11`, `--device=dri` — normal GTK4
  display + GPU access.
- `--socket=session-bus` — the Linux tray (`crates/seed-gui/src/tray.rs`'s
  `ksni` backend) registers a StatusNotifierItem with the desktop shell, which
  by default means *owning* a per-instance well-known bus name
  (`org.kde.StatusNotifierItem-<pid>-<n>`) that can't be pre-authorized with a
  narrower `--own-name=` pattern — see the manifest comment for the exact
  `ksni` internals this is based on, and the `disable_dbus_name(true)` escape
  hatch that could narrow this to a plain `--talk-name` in a later pass.
- `--talk-name=org.freedesktop.secrets` — the OS keystore for share master
  seeds (`crates/seed-core/src/secrets.rs`) goes through the `keyring` crate's
  Secret Service backend on Linux.
- `--talk-name=org.freedesktop.portal.Background` — reserved for a future
  `RequestBackground` portal call; not used yet (see next section).
- `--filesystem=host` — the daemon mirrors arbitrary, runtime-chosen folders
  continuously; no portal file-grant model supports that. This is also what
  makes `~/.config/autostart` directly writable from inside the sandbox (see
  below) without a separate `xdg-config` filesystem permission.

No `--filesystem=xdg-run/seed-sync:create`: the IPC socket lives under
`$XDG_DATA_HOME/seedsync/seed.sock` (`default_data_dir`/`default_socket` in
`crates/seed-daemon/src/main.rs`), not `$XDG_RUNTIME_DIR`, and `$XDG_DATA_HOME`
is already the sandbox's own per-app data directory with no extra permission
needed — see the data directory paragraph below.

### In-sandbox daemon supervision
There is no `systemd --user` reachable inside the sandbox, so nothing brings
`seed-daemon` up the way the tarball's unit or the Windows service does.
`crates/seed-gui/src/flatpak.rs` covers this:
- `in_flatpak()` detects the sandbox via `/.flatpak-info`, which Flatpak
  bind-mounts into every sandboxed process.
- `ensure_daemon(socket)` — a no-op outside Flatpak — checks whether anything
  is listening on the daemon's socket and, if not, spawns
  `seed-daemon run` as a child of the GUI (resolved next to the running
  `seed-gui` binary, both installed to `/app/bin`), with its stdout/stderr
  appended to `$XDG_DATA_HOME/seedsync/daemon.log` (on Linux, `seed-daemon run`
  only ever logs to stdout — see `init_logging` in
  `crates/seed-daemon/src/main.rs` — so without this redirect a
  GUI-spawned child's output would just go nowhere).
- It's called once at GUI startup, before the first IPC connection, and again
  from the "Daemon Not Started" page's retry button.
- For the daemon to survive a reboot without the user opening the GUI by hand,
  `ensure_daemon` also writes an XDG autostart entry to
  `~/.config/autostart/io.github.steeb_k.SeedSync.desktop` running
  `flatpak run io.github.steeb_k.SeedSync --hidden` — the same mechanism and
  directory the non-Flatpak install already uses for the tray
  (`packaging/linux/seed-sync`'s `AUTOSTART_DIR`), rather than the Background
  portal's `ashpd` binding, which is not a dependency of this workspace
  (`Cargo.lock` has no `ashpd` entry) and wasn't worth adding for one call.
  Same file name as the tarball install's entry, so the two can collide on an
  account that has both: an existing entry whose `Exec=` is not
  `flatpak run io.github.steeb_k.SeedSync` is left alone (the tarball's
  `seed-sync --install`/`--uninstall` own it); only a missing entry, or a
  stale one the Flatpak wrote, is (re)written.
  That file is read/written via the literal `$HOME` env var rather than
  `directories::BaseDirs::config_dir()` (`$XDG_CONFIG_HOME`), because Flatpak
  always redirects `$XDG_CONFIG_HOME` to the sandboxed
  `~/.var/app/<id>/config` — invisible to the host session manager that reads
  autostart entries — while `$HOME` itself is not redirected, and
  `--filesystem=host` makes the real `~/.config/autostart` directly writable.

### Data directory and socket path
Unchanged from the tarball/native install: the data dir + socket are chosen by
the `directories` crate the same way everywhere (`ProjectDirs::from("io.github",
"steeb_k","SeedSync")`), which resolves via `$XDG_DATA_HOME`. Flatpak sets
`$XDG_DATA_HOME` to `~/.var/app/io.github.steeb_k.SeedSync/data` inside the
sandbox (its standard per-app data isolation, independent of the
`--filesystem=host` grant), so the data dir lands at
**`~/.var/app/io.github.steeb_k.SeedSync/data/seedsync`** — holding `state.db`,
`blobs/`, `docs/`, `node.key` and `seed.sock`, exactly like
`~/.local/share/seedsync` does outside Flatpak. `seed-gui` and `seed-daemon`
compute this independently (each has its own `ProjectDirs::from(...)` call —
see `default_socket()` in `crates/seed-gui/src/main.rs` and
`default_data_dir()`/`default_socket()` in `crates/seed-daemon/src/main.rs`)
but always agree, in the sandbox or out of it, because both read the same
inputs. No code changes were needed for the sandbox case, and no
`$XDG_RUNTIME_DIR` involvement — the socket is a regular file under the data
directory, not under the runtime directory.

### Updates
Updating is Flatpak's own job (`flatpak update`, or automatic updates if the
user's Flatpak setup does that) — `seed-sync --update` is **not** shipped
inside the Flatpak build, and the bundle carries no update timer/service.

### Flathub prerequisites (not done here)
This manifest is built for our own distribution (a self-published bundle,
apps.kznjk.com's future flatpak repo — see `docs/ci-release.md` §7) and is
**not** Flathub-submission-ready:
- **`cargo-sources.json`.** The manifest's `build-options.build-args:
  [--share=network]` lets `cargo build` fetch crates from crates.io during the
  build — fine for a build we run ourselves, but Flathub's build sandbox has no
  network access at all. A Flathub submission needs a `cargo-sources.json`
  generated from this workspace's `Cargo.lock` by
  [`flatpak-cargo-generator`](https://github.com/flatpak/flatpak-builder-tools),
  added as an extra `sources:` entry, with `--share=network` removed.
- **Screenshots + a `<releases>` block** in
  `packaging/linux/io.github.steeb_k.SeedSync.metainfo.xml` — Flathub requires
  both; neither exists yet (the `.deb`/`.rpm` packages already generate a
  `<releases>` block from `CHANGELOG.md` at package time — see "Native
  packages" above — the same generation would need wiring into the Flatpak
  build too).
- A Flathub review of the permission set above, which tends to push back hard
  on `--filesystem=host` and `--socket=session-bus` — expect that
  conversation, not a rubber stamp.

## Package repositories (apps.kznjk.com)
Releases also reach apt, dnf, zypper and pacman through signed repositories on
**apps.kznjk.com** — the same host, and the same repositories, that Nullgate
(`steeb-k/nullgate`) publishes to. **Nothing in this repo publishes to them**: a
systemd timer on that host (`sync-packages.py` in `~/flatpak-repo`) polls a source
repo's `releases/latest` every 10 minutes, downloads the release assets, verifies
them against GitHub's sha256 digests, signs them and rebuilds the repositories;
prereleases are ignored. **A host-side change is required before SEED Sync's
packages show up there:** add `steeb-k/seed-sync-binaries` (the binaries repo SEED
Sync publishes releases to — see `docs/releasing.md`) to that poller's source list
(it currently polls only `steeb-k/nullgate`); the `.deb`/`.rpm`/`.pkg.tar.zst`
asset types are already generically handled once a repo is in that list. Until it
lands, the packages are still downloadable straight from the GitHub release and
install by file — they just don't self-update through a repo yet.

| Repo | URL | Client setup |
|------|-----|--------------|
| apt | `https://apps.kznjk.com/deb`, suite `stable`, component `main` | `/etc/apt/sources.list.d/kznjk.sources` + `/etc/apt/keyrings/kznjk-packages.asc` |
| dnf | `https://apps.kznjk.com/rpm/$basearch` | `/etc/yum.repos.d/kznjk.repo` |
| zypper | same repository | `/etc/zypp/repos.d/kznjk.repo` |
| pacman | `https://apps.kznjk.com/arch/$arch`, `[kznjk]` | by hand: `pacman-key --add` + `--lsign-key`, then a `[kznjk]` section — no package edits `pacman.conf` |

All are signed by **apps.kznjk.com Package Repositories**, fingerprint
`07E6 2212 5389 C2E1 94BA 7A5A 7F75 E425 2FDB 9F8D` — the same key Nullgate's
packages carry, since it's the same host and repositories serving both projects'
assets side by side.

**Installing a downloaded package adds the repository**, like Chrome or VS Code do,
so nobody is stranded on the version they downloaded. The definitions live in
`packaging/linux/repo/` and must stay byte-identical to the copies the site serves
(see that directory's `README.md`):
- **`.deb`**: `kznjk.sources` and the key are **conffiles**. `apt remove` keeps them
  (so the repository still verifies), `apt purge` deletes them, and deleting them by
  hand is a lasting opt-out because dpkg doesn't restore a removed conffile.
- **`.rpm`**: dnf and zypper read different directories, and only one exists on a
  given system, so the package ships both definitions under
  `/usr/share/seed-sync/repo/` and `postinstall` copies the right one on a **first
  install only**, never over an existing `kznjk.repo`. Erasing takes it back unless
  it was edited.
- **Arch** gets no automatic setup: a package editing `pacman.conf` is not
  acceptable there, so the `.pkg.tar.zst` ships no repository definition. Add it by
  hand — import and locally sign the key, then append the section to
  `/etc/pacman.conf`:
  ```sh
  sudo pacman-key --recv-keys 7F75E4252FDB9F8D          # or: curl -fsSL https://apps.kznjk.com/rpm/RPM-GPG-KEY-kznjk-packages | sudo pacman-key --add -
  sudo pacman-key --lsign-key 7F75E4252FDB9F8D
  ```
  ```ini
  [kznjk]
  Server = https://apps.kznjk.com/arch/$arch
  ```
  Then `sudo pacman -Sy seed-sync`. The AUR package builds from source instead and
  needs none of this.

## Future work (not built)
- A **fixed-listen-port setting** for the daemon (iroh supports binding a chosen
  port), so firewalled hosts can open exactly one UDP port instead of a subnet
  allow. Would also make the ufw guidance above a one-liner.
- A GUI "Check for updates" affordance (the daemon/GUI could surface the timer's result).
- Code signing / minisign-style artifact signatures for tamper-evidence.
- macOS mirrors this model (script/tarball + launchd, bundled GTK, ad-hoc signed).
  See `docs/macos-packaging.md`.

See also: `docs/windows-packaging.md` (the MSI side) and `docs/releasing.md` (the
cross-platform release runbook and the shared distribution model).
