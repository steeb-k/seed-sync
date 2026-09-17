# CI release pipeline — design and contract

**Status: implemented (2026-09-17); the `v0.8.0-test1` rehearsal is the remaining
proof for the hosted-runner-only parts (§11).** This document is the authority
for how a SEED Sync release is built, signed, published and distributed. It
replaced the "releases are built locally, CI never ships" rule that `CLAUDE.md`
and `docs/releasing.md` used to carry. The pattern is Nullgate's (`steeb-k/nullgate`, `docs/ci-release.md` there),
adapted to this app's differences, which are called out below.

## 1. Goals and non-negotiables

- **One tag, one release, every asset.** Pushing `vX.Y.Z` to `steeb-k/seed-sync`
  produces a single GitHub release on **`steeb-k/seed-sync-binaries`** (not on the
  source repo — every installed updater reads that repo's `releases/latest`) with
  every asset attached in **one** `gh release create` call. Never create-then-upload,
  never a draft: the moment a release is Latest, updaters act on it.
- **The updater contract does not change.** The asset names and the tag scheme in
  §3 are consumed by installed machines. A missing or renamed asset stalls updates
  silently on that platform.
- **No private key ever enters the repo.** Windows signs through Azure Trusted
  Signing over GitHub OIDC; Android's keystore and any macOS material arrive as
  secrets; Linux repositories are signed by the pull-based host, not by CI.
- **CI proves packages, it does not just build them.** Each Linux package is
  installed and removed on a real image in the same job that built it.
- **Supply-chain hygiene as in `ci.yml` today:** every action pinned by commit SHA,
  `--locked` everywhere, third-party tools (nfpm, the gvsbuild GTK zip, llvm-mingw)
  pinned by version *and* sha256 and installed by hand rather than via actions.
- **Rehearsal channel:** a dashed tag (`v0.8.0-test1`) publishes a *prerelease*,
  which every updater ignores. Every pipeline change is rehearsed that way first.

## 2. Pipeline shape

```
ci.yml           push/PR      build+test (ubuntu)                 → nothing shipped
cargo-deny.yml   push/PR/cron  advisories/licenses/sources (--all-features)
release.yml      tag v*        gate ─► build.yml (reusable) ─► publish
                 dispatch(tag)        │ linux (tarball+deb+rpm+flatpak)
                                      │ arch  (pkg.tar.zst)
                                      │ windows (x86_64 + arm64 MSI, signed)
                                      │ macos (universal .app tarball, ad-hoc)
                                      │ android (signed universal APK)
                                      └ every job uploads a workflow artifact only
```

- `gate`: `cargo build --workspace --locked --all-targets`, `cargo test --workspace
  --locked`, cargo-deny. (The integration suites stay `#[ignore]`d and remain the
  maintainer's pre-tag gate — `scripts/test-acceptance.{ps1,sh}`; CI cannot run a
  multi-peer soak on one runner.)
- `build.yml` is `on: workflow_call` with inputs `sign` (bool), `platforms` (`all`
  or a subset of `linux arch windows macos android`), `ref`; `secrets: inherit`.
- `publish`: `needs: [gate, build]`, `environment: release`, runs only for a tag
  push or a dispatch that names a tag with `platforms == all`. Downloads all
  artifacts, runs the version gate (§4), collects **exactly** the expected asset
  set (fails naming the missing one), cuts release notes from `CHANGELOG.md`, and
  creates the release on `steeb-k/seed-sync-binaries` with the PAT
  `SEED_BINARIES_TOKEN` (`gh release create "$tag" -R steeb-k/seed-sync-binaries
  --title "v$version" --notes-file notes.md [--prerelease|--latest] dist/*`).
  The binaries repo has no source, so the tag is created there by `gh` as a
  lightweight tag on its default branch; do **not** pass `--verify-tag`.
- `workflow_dispatch` on `release.yml` takes `tag`, `publish`, `sign`, `platforms`
  so a CI bug can be fixed on a branch and re-run against an existing tag: the
  workflow comes from the dispatched branch, the source from the tag.

## 3. The contract: assets, tag, versions

Tag: `vX.Y.Z` on the source repo == `[workspace.package].version` in `Cargo.toml`
== Android `versionName`; Android `versionCode == X*10000 + Y*100 + Z`. Published
release tag on `seed-sync-binaries`: the same `vX.Y.Z`, marked **Latest** (or
prerelease for a dashed tag).

| Asset (exact name)                                   | Job     | Consumer |
|------------------------------------------------------|---------|----------|
| `seed-sync-<v>-windows-x86_64.msi`                   | windows | `packaging/windows/seed-sync-update.ps1` glob `*windows-x86_64.msi` |
| `seed-sync-<v>-windows-arm64.msi`                    | windows | same, `*windows-arm64.msi` — **no cross-arch fallback**, ship both or neither |
| `seed-sync-<v>-linux-x86_64.tar.gz`                  | linux   | `packaging/linux/seed-sync --update`, `web-install.sh` (`linux-x86_64\.tar\.gz`) |
| `seed-sync-<v>-macos-universal.tar.gz`               | macos   | `packaging/macos/seed-sync --update`, macOS bootstrap |
| `seed-sync-<v>-android-universal.apk`                | android | sideload / Obtainium (same keystore forever, `versionCode` must climb) |
| `seed-sync_<v>-1_amd64.deb`                          | linux   | **new** — apps.kznjk.com apt repo; direct download |
| `seed-sync-<v>-1.x86_64.rpm`                         | linux   | **new** — apps.kznjk.com dnf/zypper repo; direct download |
| `seed-sync-<v>-1-x86_64.pkg.tar.zst`                 | arch    | **new** — apps.kznjk.com pacman repo; AUR builds from source instead |
| `io.github.steeb_k.SeedSync-<v>-x86_64.flatpak`      | linux   | **new** — single-file bundle (`flatpak install <file>`); apps.kznjk.com flatpak repo |

The Linux tarball's *internal* layout and the MSI's internals (service name
`SeedSyncDaemon`, task `SeedSyncUpdate`, `UpgradeCode 51862F05-…`) are part of the
contract too — see `docs/linux-packaging.md` and `docs/windows-packaging.md`.

## 4. Version gate and release notes

The publish job refuses to publish when the tag's version differs from `Cargo.toml`,
or when `android/app/build.gradle.kts` `versionName`/`versionCode` disagree with it.
Release notes are the `## [X.Y.Z]` section of `CHANGELOG.md` (Keep a Changelog),
cut out with awk; the stale root `release-notes.md` goes away. A dashed tag is a
prerelease; its notes come from the `## [Unreleased]` section if no exact match.

## 5. Per-platform jobs (what each reuses, what is new)

**linux** (`ubuntu-24.04`, deliberately the oldest image that builds the GUI — the
tarball binds to its glibc 2.39 / GTK 4.14): apt deps as `ci.yml`, plus
`imagemagick` (icons) and `flatpak flatpak-builder`. Runs `scripts/package-linux.sh`
(tarball, unchanged), then **new** `scripts/package-linux-native.sh` (nfpm → .deb +
.rpm from the same staged tree), then **new** `scripts/package-flatpak.sh` (bundle).
Smoke tests in-job: `apt-get install ./…deb` → binaries on PATH, `systemctl --user`
units present under `/usr/lib/systemd/user`, desktop file + metainfo validate
(`desktop-file-validate`, `appstreamcli validate`), `seed-sync --update` **refuses**
on a package-managed install, `apt-get remove` clean; `.rpm` installed in a
`registry.fedoraproject.org/fedora:latest` container (`dnf install`, `rpm -V`,
`ldd | ! grep "not found"`, `rpm -e`); the `.flatpak` bundle installs into a
throwaway user installation and `flatpak run … --version` prints the version.

**arch** (`ubuntu-24.04` + `archlinux:latest` container): `git archive` the tag to
mimic GitHub's tarball, `makepkg` from `packaging/arch/PKGBUILD`, `namcap`,
`pacman -U` / `-R`, upload the `.pkg.tar.zst`. `options=('!lto')` is required
(Arch's default LTO flags fight the workspace's `lto = true` + `codegen-units = 1`
and the GTK link). AUR publishing stays a manual `scripts/aur-prepare.sh` +
push step after the release, exactly as Nullgate.

**windows** (`windows-2025`, `environment: release` — that is the Azure federated
credential *subject*, not a gate): gvsbuild GTK zip pinned by version+sha256 into
`C:\gtk`; MSYS2 CLANGARM64 GTK + the x86_64 host-tools mirror via
`scripts\fetch-gtk-msys2.ps1` (both — the mirror is not optional); llvm-mingw pinned;
`rustup target add aarch64-pc-windows-msvc aarch64-pc-windows-gnullvm`; build x86_64
and arm64 **before** `azure/login` (the OIDC token lives ~5 min and a service
principal has no refresh token), then log in, install the Trusted Signing dlib from
the pinned nupkg, **prove the signing chain on a 2-second probe binary**, log in
*again* immediately before `build-msi.ps1 -SkipBuild` for each arch, then
`verify-bundle.ps1` ×2 and `Get-AuthenticodeSignature` must be `Valid`. "Refuse to
ship unsigned" when `sign=true` and `AZURE_CLIENT_ID` is empty. The CI signing
metadata lives at `scripts/artifact-signing-metadata.ci.json` (committed; no secret
in it — account + profile names + `ExcludeCredentials`), the maintainer's local copy
stays git-ignored at the repo root as today.

**macos** (`macos-15`, Apple Silicon): **conda-forge GTK, not Homebrew** — this is
what keeps the macOS 11 floor on a hosted runner and is the reason the 2025
workflow was abandoned (it used Homebrew on `macos-14` and inherited a 14 floor).
`scripts/setup-conda-macos.sh --universal` with the two env dirs cached by the
script's hash (`actions/cache/save` right after creation, not in a post step),
`rustup target add x86_64-apple-darwin`, `scripts/package-macos.sh`, then assert
`lipo -archs` has both slices and `otool -l | grep -A3 LC_BUILD_VERSION` shows
`minos 11.0`. Signing stays **ad-hoc** (no Apple Developer account; the tarball +
`curl | sh` install path is not quarantined) — so `sign` has no effect here and
there is no notarization step. If a Developer ID ever exists, Nullgate's
`macos-keychain.sh` + notarytool + staple-before-tar flow is the template.

**android** (`ubuntu-24.04`): temurin JDK 17, NDK pinned (`27.3.13750724`; r27c is
withdrawn from sdkmanager), `cargo-ndk`, three Rust targets, keystore materialised
from `ANDROID_KEYSTORE_BASE64` into `android/keystore.properties` (hard failure when
`sign=true` and the secret is missing — an unsigned release APK is uninstallable
over the old one), `./gradlew --no-daemon :app:assembleRelease`, rename to
`seed-sync-<v>-android-universal.apk`, `apksigner verify --print-certs`.

## 6. Linux packaging model (the part that is genuinely new)

SEED Sync's tarball is a **per-user** install (`~/.local/bin`, `systemd --user`,
no root). The native packages keep that runtime model but place files
system-wide:

| Path | Content |
|------|---------|
| `/usr/bin/seed-daemon`, `/usr/bin/seed-gui`, `/usr/bin/seed-cli`, `/usr/bin/seed-sync` | binaries + the wrapper |
| `/usr/lib/systemd/user/seed-daemon.service` | `ExecStart=/usr/bin/seed-daemon run` (a package variant of the unit: the tarball's uses `%h/.local/bin`) |
| `/usr/share/applications/io.github.steeb_k.SeedSync.desktop` | `Exec=seed-gui` (no `__BIN__` placeholder in the package variant) |
| `/usr/share/metainfo/io.github.steeb_k.SeedSync.metainfo.xml` | with a `<releases>` block generated from `CHANGELOG.md` |
| `/usr/share/icons/hicolor/<sz>x<sz>/apps/io.github.steeb_k.SeedSync.png` | as the tarball |
| `/usr/share/seed-sync/repo/…` + `/etc/apt/sources.list.d/kznjk.sources` + `/etc/apt/keyrings/kznjk-packages.asc` | self-configuring repo definitions (§7) |

The updater timer units are **not** installed by packages: package-managed
installs update through the package manager, and `seed-sync --update` refuses
(`refuse_if_pkg_managed`: binary under `/usr/bin` and owned by dpkg/rpm/pacman).
`postinstall` prints how to enable the user service (`systemctl --user enable --now
seed-daemon`) and never touches user sessions. Builder: **nfpm** (one
`packaging/linux/nfpm.yaml` with deb/rpm overrides: deb depends `libgtk-4-1 (>=
4.10)`, `libadwaita-1-0 (>= 1.4)`, `libdbus-1-3`, `libc6 (>= 2.39)`; rpm depends by
soname; deb compression xz, rpm zstd), fed from the tree `package-linux.sh` stages.

**Arch:** `packaging/arch/PKGBUILD` builds from the source tarball of the tag
(`depends=(bash dbus gdk-pixbuf2 glib2 glibc gtk4 hicolor-icon-theme libadwaita libgcc pango)`, `makedepends=(cargo pkgconf imagemagick)`), installs
the same layout, ships `seed-sync.install` with the enable hint. `scripts/ci/arch-pkgbuild.sh`
runs it in a container; `scripts/aur-prepare.sh` rewrites `pkgver`/`sha256sums` and
regenerates `.SRCINFO` for the AUR clone.

**Flatpak:** `packaging/flatpak/io.github.steeb_k.SeedSync.yml` on
`org.gnome.Platform//50` (GTK4 + libadwaita come from the runtime; 48 went end-of-life in 2026-03 and Flathub no longer serves it; the SDK adds
`org.freedesktop.Sdk.Extension.rust-stable`). This app cannot use portal file
grants — the daemon does continuous R/W on whole folders — so the manifest asks
for `--filesystem=host`, `--share=network`, `--socket=session-bus` (the tray is a
StatusNotifier item), `--talk-name=org.kde.StatusNotifierWatcher`,
`--talk-name=org.freedesktop.secrets`, `--talk-name=org.freedesktop.portal.Background`,
and `--filesystem=xdg-run/seed-sync` for the socket. Inside the sandbox there is no
`systemd --user`, so **the GUI supervises the daemon**: when `/.flatpak-info` exists
and the socket is absent, `seed-gui` spawns `seed-daemon run` as a child and asks
the Background portal for autostart (`--hidden`); the data dir is the sandbox's
`XDG_DATA_HOME` (`~/.var/app/io.github.steeb_k.SeedSync/data/seedsync`). Updates
are Flatpak's job (`seed-sync --update` is not shipped in the Flatpak). CI builds
with `flatpak-builder --repo=repo` and exports a single-file bundle
(`flatpak build-bundle`); the manifest uses `build-options: { build-args:
[--share=network] }` so cargo can fetch — acceptable for our own repo/bundle, and
documented as the thing that must become a `cargo-sources.json` (via
`flatpak-cargo-generator`) before a Flathub submission. Flathub is a later,
separate step.

`docs/linux-packaging.md` currently explains why Flatpak was rejected; that section
is rewritten to "why the Flatpak needs these permissions" instead.

## 7. Distribution repositories (pull-based, host-side)

Nothing in this repo pushes to a package repository. **apps.kznjk.com** (the same
host that serves Nullgate's apt/dnf/zypper/pacman/flatpak repositories, from
`~/flatpak-repo` with `sync-packages.py` on a timer) polls a GitHub repo's
`releases/latest`, verifies each asset's sha256 against GitHub's digest, GPG-signs
it and rebuilds the repositories; prereleases are ignored. **Host-side change
required, outside this repo:** add `steeb-k/seed-sync-binaries` to that poller's
source list (it currently polls `steeb-k/nullgate`) and add the `.flatpak` bundle
to its flatpak repo import. Until that is done the packages are still downloadable
from the release page and install by file.

The `.deb`/`.rpm` ship the client-side repo definitions so a downloaded file
self-subscribes (Chrome/VS Code pattern): `packaging/linux/repo/kznjk.sources`,
`kznjk.repo`, `kznjk-zypper.repo` and the **public** key
`kznjk-packages.asc` — byte-identical copies of Nullgate's
`packaging/linux/repo/` (same host, same key, fingerprint `07E6 2212 5389 C2E1 94BA
7A5A 7F75 E425 2FDB 9F8D`). Arch gets no automatic repo config (a package editing
`pacman.conf` is not acceptable); the `[kznjk]` block is documented for manual add.

## 8. Secrets and one-time setup (maintainer)

| Secret | Used by | Notes |
|--------|---------|-------|
| `SEED_BINARIES_TOKEN` | publish | fine-grained PAT, `contents: write` on `steeb-k/seed-sync-binaries` only |
| `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_SUBSCRIPTION_ID` | windows | service principal with "Artifact Signing Certificate Profile Signer" on the cert profile; federated credential subject `repo:steeb-k/seed-sync:environment:release` (register both the legacy and immutable-ID spellings) |
| `ANDROID_KEYSTORE_BASE64`, `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS`, `ANDROID_KEY_PASSWORD` | android | the existing, irreplaceable `seedsync-release.jks` |

Plus: a GitHub **environment** named `release` on the source repo; the `CHANGELOG.md`
convention; and the host-side poller change in §7. No macOS secrets until a
Developer ID exists.

## 9. Rollout

1. Land the pipeline on `main` with `ci.yml` green.
2. Configure the secrets and the `release` environment.
3. Bump to `0.8.0` (feature release: #37 + #38), write the changelog section.
4. Push `v0.8.0-test1` → a prerelease on the binaries repo; inspect every asset,
   install the .deb/.rpm/.pkg/.flatpak on real machines, confirm the MSI signature
   and the APK certificate; updaters must ignore it.
5. Push `v0.8.0` → Latest. Watch the installed fleet update; run
   `scripts/aur-prepare.sh 0.8.0 ../seed-sync-aur` and push the AUR update.

## 10. Workstreams and ownership (for the implementers)

Disjoint file ownership so the workstreams can run in parallel:

- **W1 — workflows & publish:** `.github/workflows/{build,release}.yml`,
  `cargo-deny.yml` (drop `arguments: ""` so `--all-features` applies),
  `scripts/ci/*.sh|ps1` (windows probe, arch container script, publish helpers),
  `scripts/artifact-signing-metadata.ci.json`, `CHANGELOG.md`, and this doc's §2–5
  and §8 kept accurate.
- **W2 — Linux native packages & repos:** `packaging/linux/nfpm.yaml`,
  `packaging/linux/scripts/{preinstall,postinstall,preremove}.sh`, package-variant unit and
  desktop file, `packaging/linux/repo/*`, `packaging/arch/{PKGBUILD,seed-sync.install}`,
  `scripts/package-linux-native.sh`, `scripts/ci/arch-pkgbuild.sh`,
  `scripts/aur-prepare.sh`, `packaging/linux/seed-sync` (`refuse_if_pkg_managed`),
  metainfo `<releases>` generation, `docs/linux-packaging.md` §deb/rpm/arch/repos.
- **W3 — Flatpak:** `packaging/flatpak/*`, `scripts/package-flatpak.sh`, the
  GUI's in-sandbox daemon supervision (`crates/seed-gui/src/main.rs`, a small
  `flatpak.rs`), `crates/seed-daemon` socket-path handling under `XDG_RUNTIME_DIR`,
  `docs/linux-packaging.md` §Flatpak.
- **W4 — docs sweep (after W1–W3):** `CLAUDE.md`, `README.md`, `docs/releasing.md`,
  `docs/{windows,macos,android}-packaging.md`, `docs/dev-environment.md`,
  `docs/testing.md` (CI gate vs acceptance gate), remove `release-notes.md`.

## 11. Verification criteria (for the reviewers)

- Workflows: `actionlint` clean; every `uses:` pinned to a 40-char SHA with a
  version comment; no job uploads or creates a release except `publish`; the
  asset-name set in `publish` equals §3 exactly; the OIDC login precedes signing by
  seconds, not minutes; `sign=true` with a missing secret is a hard failure.
- Packages (locally, in WSL Ubuntu-24.04 / the archlinux distro on the dev box):
  `nfpm package` succeeds for deb and rpm from the staged tree; `dpkg -c` / `rpm -qpl`
  show exactly the §6 layout; `lintian` and `rpmlint` report nothing worse than
  known-acceptable; the Arch PKGBUILD builds in the archlinux distro;
  `desktop-file-validate` and `appstreamcli validate --no-net` pass.
- Flatpak: manifest passes `flatpak-builder --show-manifest`/lint; if
  `flatpak-builder` is available locally, the bundle builds and `flatpak run
  io.github.steeb_k.SeedSync --version` works; the GUI's sandbox path is unit-tested
  (`/.flatpak-info` detection, spawn-if-absent) without needing Flatpak.
- Contract: grep-proof that every updater/bootstrap regex still matches the asset
  names; `packaging/linux/seed-sync --update` refuses under `/usr/bin`.
- Anything that can only be proven on a hosted runner (Azure signing, macOS
  conda build, Fedora container smoke test) is listed explicitly as "verified by
  the `v0.8.0-test1` rehearsal", not claimed.
