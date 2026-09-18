# Cutting a release

**Releases are built and published by CI from a tag.** Pushing `vX.Y.Z` to
`steeb-k/seed-sync` runs `.github/workflows/release.yml`, which builds every
platform through `.github/workflows/build.yml`, gates on the same checks `ci.yml`
runs, and creates **one** GitHub release on the **public
`steeb-k/seed-sync-binaries`** repo with all eight assets attached in a single
call. The design, the asset contract and the per-platform jobs are in
[`ci-release.md`](ci-release.md); this page is the runbook. The per-platform
scripts still work by hand and are exactly what CI runs — building locally is
the fallback, not the process.

The auto-updaters (Windows scheduled task, Linux timer/service, macOS launchd,
the `web-install.sh` bootstraps, and apps.kznjk.com's package poller) are
version-driven: each compares the installed `seed-daemon --version` against the
release marked **Latest** on `seed-sync-binaries` and upgrades only when it is
newer. So every release must bump the version, and the newest release must be
the one marked Latest.

Per-platform mechanics: [`windows-packaging.md`](windows-packaging.md),
[`linux-packaging.md`](linux-packaging.md), [`macos-packaging.md`](macos-packaging.md),
[`android-packaging.md`](android-packaging.md).

## Distribution model

```
  steeb-k/seed-sync (source)        seed-sync-binaries (PUBLIC)          user machine
  ──────────────────────────        ───────────────────────────          ────────────
  git push v0.8.0 ─► release.yml ─►  Release "v0.8.0" (Latest)   ◄────  seed-sync --update
                     gate + build     ├─ ...linux-x86_64.tar.gz    poll   (timer/task/agent)
                     + publish        ├─ ...windows-{x86_64,arm64}.msi    compares to
                     (one gh call)    ├─ ...macos-universal.tar.gz        `seed-daemon --version`
                                      ├─ ...android-universal.apk
                                      ├─ seed-sync_<v>-1_amd64.deb ◄───  apps.kznjk.com poller
                                      ├─ seed-sync-<v>-1.x86_64.rpm      (apt/dnf/zypper/pacman
                                      └─ seed-sync-<v>-1-x86_64.pkg.tar.zst   repos)
```

- Artifacts live on a **separate public repo** so machines download with no
  auth. One release per `vX.Y.Z` carries every platform's asset; the asset names
  are the contract in [`ci-release.md` §3](ci-release.md#3-the-contract-assets-tag-versions).
- The **installed version is the source of truth**: the updaters read
  `seed-daemon --version` and compare it to the Latest release tag.
- **Linux packages** are picked up by apps.kznjk.com's poller, which verifies
  each asset's sha256, GPG-signs it and rebuilds the apt, dnf/zypper and pacman
  repositories. Nothing in this repo pushes to a package repository.
- A **dashed tag** (`v0.8.0-test1`) publishes a *prerelease*, which every updater,
  Obtainium and the package poller ignore. Rehearse every pipeline change that way.

## Versioning

- The workspace version lives once in the root `Cargo.toml` `[workspace.package]`;
  every crate inherits it. `seed-daemon --version` and the updaters' comparison
  come from this.
- Android carries the same number by hand in `android/app/build.gradle.kts`:
  `versionName` matches, `versionCode` is `MAJOR*10000 + MINOR*100 + PATCH`
  (`0.8.0` → `800`). Android refuses an APK whose `versionCode` is lower than the
  installed one, so the code must only ever climb.
- The tag is `v<version>`. The `publish` job refuses a tag whose version differs
  from `Cargo.toml` or from the Android literals, so forgetting a bump is loud.

## Release checklist

1. **Acceptance gate:** `scripts/test-acceptance.ps1` / `.sh` passes (see
   [`testing.md`](testing.md)). CI's `gate` job repeats only the unit tests and
   cargo-deny; every integration suite is `#[ignore]`d and needs real peers.
2. **Bump** the version in `Cargo.toml`, run `cargo update --workspace`, bump the
   Android `versionName`/`versionCode`, and move `CHANGELOG.md`'s `## [Unreleased]`
   items under `## [<version>] - <date>` (the release notes are cut from that
   section). Commit `release: v<version>` and push `main`.
3. **Rehearse if anything in the pipeline changed:**
   `git tag v<version>-test1 && git push origin v<version>-test1`. That publishes
   a prerelease with all eight assets that nothing installed will take. Install
   one or two of them by hand (smoke-check below), then delete the prerelease and
   the tag on both repos.
4. **Tag:** `git tag v<version> && git push origin v<version>`. The `release`
   workflow builds all five jobs, gates, and creates the Latest release with every
   asset in one call. If the `release` environment has a required reviewer,
   approve it in the Actions tab.
5. **If a platform job fails, nothing is published.** Fix on a branch, then
   dispatch `release.yml` from that branch with `tag: v<version>`: the workflow
   comes from the branch, the source from the tag.
6. **Package repositories** need nothing: apps.kznjk.com's timer signs the
   `.deb`, `.rpm` and `.pkg.tar.zst` into its repositories within
   about ten minutes (`journalctl --user -u packages-sync` on that host).
7. **AUR**, once the release is published: `scripts/aur-prepare.sh <version>
   ../seed-sync-aur`, review, commit and push the clone (details in
   [`linux-packaging.md`](linux-packaging.md)). CI's `arch` job has already proved
   that PKGBUILD builds against the tag.

### Building by hand (fallback)

Each artifact on its own OS: **Windows** `pwsh -File scripts\build-msi.ps1`
(+ `-Arch arm64`; signed when `artifact-signing-metadata.json` and an `az login`
session are present, see [`windows-packaging.md`](windows-packaging.md));
**Linux** `scripts/package-linux.sh`, then `scripts/package-linux-native.sh`
(needs nfpm, the version pinned in `build.yml`);
**Arch** `scripts/ci/arch-pkgbuild.sh` in an `archlinux` container with `/out`
mounted; **macOS** `scripts/setup-conda-macos.sh --universal` once, then
`CODESIGN_IDENTITY='Developer ID Application: …' SEED_NOTARIZE=1 scripts/package-macos.sh`
(ad-hoc without the identity — installable through the `curl | sh` path, not by
browser download); **Android** `cd android && ./gradlew :app:assembleRelease`
with `android/keystore.properties`, renamed to `seed-sync-<version>-android-universal.apk`.

Publish with `gh release create v<version> -R steeb-k/seed-sync-binaries --latest
--notes-file notes.md <all eight files>` in **one command** (no `--verify-tag`:
the binaries repo has no source, `gh` creates the tag there). Never
create-then-upload and never leave a draft: the moment a release is Latest,
updaters act on it, and a release missing an asset strands that platform.

## Smoke-check before announcing

- **Windows:** install the MSI on a clean machine; the app opens, the
  `SeedSyncDaemon` service runs, the `SeedSyncUpdate` task exists
  (`schtasks /Query /TN SeedSyncUpdate`), and `Get-AuthenticodeSignature` is `Valid`.
- **Linux packages:** once the poller has picked the release up, on a machine that
  already has the repository run `sudo apt update && sudo apt upgrade` (or
  `dnf upgrade`, `pacman -Syu`) and confirm it moves to
  `<version>`; `seed-sync --update` must refuse on a package-managed install.
  Installing the downloaded `.deb`/`.rpm` on a fresh machine must leave the
  repository configured.
- **Linux/macOS tarball:** run the `curl … | sh` one-liner; `seed-sync --status`
  shows the daemon active and the updater enabled. On macOS,
  `spctl -a -vv "/Applications/SEED Sync.app"` says `source=Notarized Developer ID`.
- **Two machines:** create a share on one, join on the other, confirm both
  directions sync and both report 100%.
- **Auto-update path:** with an older build installed, confirm the updater picks
  the release up (or force it: `seed-sync --update`; Windows
  `…\bin\seed-sync-update.ps1 -Check`).
- **Android:** on a device that has the *previous* release-signed build,
  `adb install -r seed-sync-<version>-android-universal.apk` must update **in
  place**. A signature clash means the keystore changed; stop and fix it before
  announcing (see [`android-packaging.md`](android-packaging.md)).

## Secrets and one-time setup

Listed in [`ci-release.md` §8](ci-release.md#8-secrets-and-one-time-setup-maintainer):
`SEED_BINARIES_TOKEN`, the three `AZURE_*` ids behind the OIDC federated
credential (`repo:steeb-k/seed-sync:environment:release`), the four
`ANDROID_*` keystore secrets, the six `MACOS_*` signing and notary secrets
(the Developer ID team Nullgate and Commune share), and the `release`
environment on the source repo. `~/seed-sync-signing/set-seed-sync-secrets.sh`
sets all of them but the PAT. The Windows signing account and certificate
profile names are committed in `scripts/artifact-signing-metadata.ci.json`
(the same Trusted Signing account Nullgate uses); the maintainer's local copy
stays git-ignored at the repo root. The keystore is irreplaceable from the
first shipped APK on: back it up, never rotate it.
