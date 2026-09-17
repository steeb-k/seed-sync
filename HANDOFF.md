# Hand-off: `ci-pipeline` after the Linux verification pass (2026-09-17)

This file is temporary. Delete it once the `ci-pipeline` branch is merged.

## Where things stand

- `main` (`2de0137`) carries the #37 and #38 engine fixes. Nothing more is
  coming from the Windows session.
- `ci-pipeline` = main + the CI/packaging overhaul against `docs/ci-release.md`.
  W1 (workflows/publish), W2 (Linux native packages + repos), W3 (Flatpak) and
  **W4 (docs sweep) are all done**; W4 landed in the Linux pass below.
- The next release is **0.8.0** (feature bump). The version is **not bumped yet**;
  `CHANGELOG.md` has the `## [0.8.0]` section.
- The repo was renamed `seed-sync-gtk` → `seed-sync`. Every reference on the
  branch was updated; the one that mattered is GitHub's tag archive, which now
  extracts to `seed-sync-<v>/` (verified against a real tag), so the PKGBUILD
  uses `$pkgname-$pkgver` like Nullgate's and `build.yml`'s `git archive`
  prefix and `scripts/ci/arch-pkgbuild.sh` match.

## Verified on Linux (Debian box; Ubuntu 24.04 / Fedora / Arch in rootless podman)

1. `actionlint` clean on `build.yml`, `release.yml`, `cargo-deny.yml` (and `ci.yml`).
   Every `uses:` SHA checked against its version tag on GitHub; the gvsbuild
   zip's sha256 and the llvm-mingw asset re-verified.
2. `shellcheck` clean on every new/changed script.
3. Ubuntu 24.04 container: `scripts/package-linux.sh` + `scripts/package-linux-native.sh`
   with nfpm 2.47.0 → tarball, `.deb`, `.rpm`. `dpkg -c` / `rpm -qpl` are exactly
   the §6 layout. `lintian` and `rpmlint` report exactly what Nullgate's own
   packages do (no-changelog, package-installs-apt-sources, no-manual-page,
   spelling of "iroh"/"cli", `cp` in `%post`). `apt-get install ./…deb`: binaries
   on PATH, unit + desktop + metainfo validate, `seed-sync --update` refuses,
   repo conffiles present, `apt remove` keeps them, `apt purge` drops them,
   `--reinstall` exercises the upgrade branch of postinst. In a **systemd-booted**
   container, as a real user with linger: `systemctl --user enable --now
   seed-daemon` → active, enabled, data dir created. Fedora container: the
   `build.yml` smoke script verbatim (dnf install, rpm -V, ldd, refusal, repo
   file, rpm -e) passes.
4. Arch: `scripts/ci/arch-pkgbuild.sh` in `archlinux:latest` → makepkg, namcap,
   `pacman -U`, refusal, `pacman -R` all pass; `makepkg --printsrcinfo` OK.
5. Flatpak: built with `org.flatpak.Builder` on the host. The bundle installs,
   `flatpak run --command=seed-daemon … --version` and `flatpak run … --version`
   print the version, and running the GUI in the sandbox spawns `seed-daemon`
   (socket + `daemon.log` under `~/.var/app/…/data/seedsync`).
6. `cargo fmt --check`, `cargo clippy -p seed-gui -D warnings`, `cargo test -p seed-gui`
   (7 sandbox tests) pass in the container. The host itself cannot build the
   workspace (no libdbus headers, no sudo) — use the container.

## What the pass changed (beyond W4)

- **Executable bits.** Every new script was committed `100644` from Windows;
  `release.yml` calls `scripts/ci/version-gate.sh` and `collect-assets.sh`
  directly, so the publish job would have died with "Permission denied".
- **Flatpak runtime.** GNOME 48 went EOL in 2026-03 and Flathub no longer serves
  it; the manifest is on `org.gnome.Platform//50`. `package-flatpak.sh` no
  longer installs the runtime/SDK/extension itself (the rust-stable extension is
  branched by the freedesktop SDK, `25.08`, not by GNOME); `flatpak-builder
  --install-deps-from=flathub` resolves it.
- **Flatpak manifest** installed the desktop file through a temp file in a
  directory that did not exist yet; it now installs
  `packaging/linux/pkg/io.github.steeb_k.SeedSync.desktop` directly.
- **`seed-gui --version`** did not exist (unknown flags were ignored), so the CI
  Flatpak smoke test would have launched the GUI and hung; it now prints
  `seed-gui <version>` and exits before any GTK setup.
- **Flatpak autostart entry clobbered the tarball's.** Both installs write
  `~/.config/autostart/io.github.steeb_k.SeedSync.desktop`; the sandbox code
  overwrote any differing file (it did so on this box). It now leaves an entry
  whose `Exec=` is not `flatpak run …` alone; two unit tests cover it.
- **Signing metadata** filled from Nullgate's (`wus` endpoint, account
  `skz-code`, profile `ddrx-pcsvc`); the federated credential subject is
  `repo:steeb-k/seed-sync:environment:release`.
- **Lint-driven:** empty `postremove` dropped (lintian `maintainer-script-empty`);
  preinstall comment reworded (`rpmlint use-of-home-in-%pre` fires on `~/` in a
  comment); PKGBUILD `depends` aligned with Nullgate's full list (namcap wanted
  `bash`) and `makedepends` `git` → `pkgconf`; the metainfo `<releases>` block
  now stops at the package's own version (it listed the unreleased 0.8.0).

## Not verified here (needs the `v0.8.0-test1` rehearsal)

The Windows signing chain, the macOS conda-forge build and the hosted Fedora
step. The tray icon in the Flatpak: this box's session has no
`org.kde.StatusNotifierWatcher` at all (the host tarball GUI cannot show a tray
here either), so the sandboxed GUI got as far as the bus reply "no watcher"
(`no StatusNotifier tray yet … ServiceUnknown`), which proves the session-bus
grant but not the icon. Check it on a KDE/GNOME-with-AppIndicator session.

## What only the maintainer can do (one-time)

- Repo secrets on `steeb-k/seed-sync`: `SEED_BINARIES_TOKEN`, `AZURE_CLIENT_ID`,
  `AZURE_TENANT_ID`, `AZURE_SUBSCRIPTION_ID` (federated credential subject
  `repo:steeb-k/seed-sync:environment:release`), `ANDROID_KEYSTORE_BASE64`,
  `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS`, `ANDROID_KEY_PASSWORD`.
- Create the GitHub environment `release` on the source repo.
- apps.kznjk.com: add `steeb-k/seed-sync-binaries` to the package poller's
  sources and its flatpak import.
- Decide whether the .deb/.rpm/Arch packages should ship
  `/etc/xdg/autostart/io.github.steeb_k.SeedSync.desktop` (tray autostart for
  every user, as Nullgate's packages do). They still do not; the reviewer's
  recommendation and conditions from the previous hand-off stand (separate file,
  `Exec=seed-gui --hidden`, `TryExec`, `NoDisplay=true`, `config|noreplace` /
  `backup=()`, documented `Hidden=true` opt-out).
- The source repo carries `v1.1.0`–`v1.1.2` tags from 2026-06 in `main`'s
  history. They do not collide with `v0.8.0`, but `release.yml` triggers on any
  `v*` push, so never re-push one.

## Suggested order

1. Merge `ci-pipeline` to `main` (`ci.yml` must be green).
2. Bump `[workspace.package].version` to 0.8.0 and the Android literals
   (`versionName = "0.8.0"`, `versionCode = 800`), date the changelog section,
   commit `release: v0.8.0`.
3. Configure the secrets and the `release` environment; push `v0.8.0-test1`.
4. When the rehearsal is clean: push `v0.8.0`, then `scripts/aur-prepare.sh
   0.8.0 ../seed-sync-aur` and push the AUR update.

Parked (after 0.8.0): hover tooltip on the syncing % naming the file in flight;
the fetch error should name the refusing member; `crates/seed-daemon/src/main.rs`'s
default `RUST_LOG` string has whitespace inside `iroh_gossip=warn` (the daemon
logs "ignoring … error parsing level filter" on every start).
