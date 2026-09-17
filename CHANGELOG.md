# Changelog

All notable changes to SEED Sync. Format follows [Keep a Changelog](https://keepachangelog.com).
Pre-1.0; prereleases are tagged `v<version>-test<N>`.

## [Unreleased]

## [0.8.0] - 2026-09-17 (not yet released)

### Added
- **Release builds move to GitHub Actions.** `.github/workflows/release.yml` builds every
  platform via `.github/workflows/build.yml` and publishes one GitHub release — on the public
  `steeb-k/seed-sync-binaries` repo, exactly as the old local-build workflow did — with all nine
  assets attached in a single call, after a version gate (tag == workspace version == Android
  `versionName`/`versionCode`). A dashed tag (`v<ver>-test<N>`) publishes a prerelease every
  updater ignores, for rehearsing a pipeline change before it ships. Windows is still signed with
  Azure Trusted Signing over OIDC and Android with the release keystore; macOS stays ad-hoc
  (no Apple Developer account). See `docs/ci-release.md`.
- **Native Linux packages, with updates.** Every release now also carries a `.deb` (Debian 13+,
  Ubuntu 24.04+), an `.rpm` (Fedora 40+, openSUSE Tumbleweed), an Arch package
  (`.pkg.tar.zst`, also published to the AUR) and a single-file Flatpak bundle. The `.deb`/`.rpm`
  ship signed apt/dnf/zypper repository definitions so a downloaded package self-subscribes to
  future updates; Arch users add the `[kznjk]` repository by hand or build the `seed-sync` AUR
  package. The packages install under `/usr`, stage the (per-user, `systemd --user`) daemon unit
  without enabling it, and refuse to install over the tarball's `~/.local/bin` install and vice
  versa (`seed-sync --update` now detects a package-managed install and declines to touch it).
  CI installs and removes the `.deb` under a real apt, installs and removes the `.rpm` on Fedora,
  builds the PKGBUILD in an Arch container, and installs/runs the Flatpak bundle, before any
  release is published.

### Fixed
- **A share whose folder is on a removed drive no longer kills the daemon on every start**
  (known-issues #37). Losing a share's folder — an unplugged external drive, an unmounted
  network share — used to abort startup silently (`exit 0`), taking down every *other* share on
  the same daemon along with it, and the Windows service's failure-action restart loop never
  broke out because the daemon "succeeded" each time. The share is now held inert as
  `FolderMissing`, is never destructively recreated, and resumes on its own once the folder
  reappears; the rest of the daemon starts normally, and a startup failure that really is fatal
  now carries a real exit code to the Windows Service Control Manager instead of exit 0.
- **A member that lists a blob it cannot actually serve no longer refuses every peer's fetch of
  it forever** (known-issues #38). A metadata-only health check could credit a blob as `Healthy`
  even when its underlying data was missing or unreadable (a pre-#33 GC edge case, a moved or
  deleted `data/<hash>.data`), so the re-import repair that should have caught it never ran; the
  serving side also never recorded that it had refused a fetch, so nothing triggered a repair on
  its own. The health pass now proves servability with an actual read before crediting a blob,
  caches that per share so steady state stays cheap, and a refused fetch is now itself the
  trigger for a debounced repair pass. Getting the repair to actually take also required a hunk
  in the vendored `iroh-blobs` fork (hunk 3 — see `vendor/README.md`): a blob handle poisoned by
  a failed open or an idle store `persist()` was never reloaded, a re-import over a poisoned
  handle used to merge instead of replace (so it kept pointing at the missing owned file), and
  `observe()` on a poisoned handle could panic the store's actor thread. Covered by a new
  tier-1 red-then-green suite, `crates/seed-core/tests/serve_repair.rs`.

## [0.7.4] - 2026-09-04

### Fixed
- The daemon now provisions its own inbound firewall rule and Windows service recovery actions
  on every start (not just at install time), so a deleted rule, a moved install path, or an MSI
  whose firewall custom action misfired heals on the next service start instead of needing a
  reinstall.
- Service recovery actions need `SERVICE_START` to actually take effect; the captive-portal probe
  is quieter about routine failures.
- Repair a wedged transport in-process instead of waiting for a full daemon restart
  (known-issues #36).

### Changed
- The Android build now generates its UniFFI bindings from the release host build (not debug),
  saving several GB of `target/` on machines building both the MSI and the APK back to back.

## [0.7.3] - 2026-08-26

### Fixed
- The MSI's firewall custom action only ever installed one of its two per-profile rules (each
  profile's exception needs its own distinct `Name`, or the custom action silently replaces the
  earlier one) — both are now installed.
- A flapping share could disarm the #23 connectivity self-heal ladders, because their episode
  clocks reset on every blip instead of accumulating toward the retry rungs (known-issues #35).

## [0.7.2] - 2026-08-25

### Fixed
- The MSI now installs an inbound Windows Firewall rule for the daemon; without it, a service
  never gets the interactive firewall prompt, so inbound QUIC dials were silently dropped and a
  share leaned entirely on relay-coordinated holepunching.
- A removed or paused share kept reconciling after it was gone, instead of stopping cleanly.
