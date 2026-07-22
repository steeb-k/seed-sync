# Cutting a release

Releases are built **locally, on each platform's own machine**, and published as
a GitHub Release on the **public** `steeb-k/seed-sync-binaries` repo. There is no
GitHub Actions / CI path — the old `.github/workflows/release.yml` was removed in
favor of local builds (notably so the macOS asset can target an OS floor well
below what the hosted `macos-14` runner allowed).

The auto-updaters (Windows scheduled task, Linux timer/service, macOS launchd, and
the `web-install.sh` bootstrap) are version-driven: each compares the installed
`seed-daemon --version` against the latest release tag on `seed-sync-binaries` and
upgrades only when the release is newer. So every release must bump the version.

## Distribution model

All four platforms share one distribution model; the per-OS packaging docs describe
only the mechanics on top of it.

```
  dev machines (local builds)         seed-sync-binaries (PUBLIC)         user machine
  ───────────────────────────         ──────────────────────────         ────────────
  package-linux.sh  ──► gh release ──►  Release "vX.Y.Z"          ◄─── seed-sync --update
  build-msi.ps1         create/upload   ├─ ...linux-x86_64.tar.gz  poll   (timer/task/agent)
  gradlew assembleRel   (per platform)  ├─ ...windows-x86_64.msi    +     compares to
  package-macos.sh                      └─ ...android...   APK     fetch  `seed-daemon --version`
```

- Artifacts are published to a **separate public repo** (`steeb-k/seed-sync-binaries`)
  so machines download with no auth; source stays private in `seed-sync-gtk`. One
  GitHub Release per `vX.Y.Z` tag carries every platform's asset.
- The **installed version is the source of truth**: the updater reads
  `seed-daemon --version` and compares it to the latest release tag, so the Cargo
  version must be bumped per release or no machine ever sees a newer build.
- **No CI.** Every artifact is built and signed locally on its own platform's machine
  and attached to the release; the old GitHub Actions workflow was removed.

## 1. Create the draft release FIRST — before building anything

**Do this at the very start of every release, not at the end.** Cutting a release is a
long, multi-machine job (three or four platform builds, minutes each); if the GitHub
release page only appears after all of them finish, it's invisible for the whole run and
there's nowhere for assets to land as they complete. So create it up front, as a **draft**,
the moment you know the target version:

```sh
# You know vX.Y.Z before you build — it's a decision, not a build output.
gh release create vX.Y.Z \
  --repo steeb-k/seed-sync-binaries \
  --title vX.Y.Z \
  --notes-file release-notes.md \
  --draft
# -> prints the draft's URL. Assets get attached to it in step 3 as each build lands;
#    the notes can be refined any time before you finalize.
```

Draft, not published, because the release marked **Latest** is what every updater fetches
(`releases/latest`) — you don't want a half-built release going Latest with only some
platforms' assets attached. It stays a draft, invisible to updaters, until step 4.

> If `release-notes.md` isn't written yet, create the draft with a placeholder and edit the
> notes before finalizing (`gh release edit vX.Y.Z --repo … --notes-file release-notes.md`).
> The point is that the page exists from minute one.

## 2. Bump the version

The workspace version in `Cargo.toml` is the single source of truth.

```sh
# Edit Cargo.toml [workspace.package] version, then refresh the lockfile:
cargo update --workspace
```

Also bump `android/app/build.gradle.kts`:

- `versionName` — matches the workspace version (e.g. `"0.3.4"`).
- `versionCode` — `MAJOR*10000 + MINOR*100 + PATCH` (so `0.3.4` → `304`,
  `1.2.0` → `10200`). Monotonic and decodable; see `docs/android-packaging.md`.
  > Android refuses to install an APK whose `versionCode` is **lower** than the
  > one already on the device. If you ever lower the version line (as in the
  > 1.x → 0.x rollback), bump past the old code or uninstall first.

Then commit:

```sh
git commit -am "release: vX.Y.Z"
git push origin main
```

You can tag the source repo too if you like history (`git tag vX.Y.Z && git push
origin vX.Y.Z`) — with CI gone, the tag triggers nothing. It is **not** required;
the release on `seed-sync-binaries` carries its own tag, created by `gh` below.

## 3. Build the artifacts — and upload each to the draft as it lands

Only build the platforms you have a machine for — the release is assembled
incrementally, so you can attach more assets to the same (draft) release later. Every
artifact is named `seed-sync-<ver>-<platform>`. **As each build finishes, upload it to the
draft right away** rather than saving them all for the end:

```sh
gh release upload vX.Y.Z --repo steeb-k/seed-sync-binaries <artifact>
```

**Windows MSI** (Windows, GTK + Azure signing set up — see
[`windows-packaging.md`](windows-packaging.md)):

```pwsh
cargo build --release
az login   # your user must hold the Azure Artifact Signing signer role
# point ARTIFACT_SIGNING_DLIB at Azure.CodeSigning.Dlib.dll (from the
# Microsoft.ArtifactSigning.Client NuGet)
pwsh -File scripts\build-msi.ps1 -SkipBuild
# -> target\wix\seed-sync-<ver>-windows-x86_64.msi
pwsh -File scripts\build-msi.ps1 -Arch arm64
# -> target\wix\seed-sync-<ver>-windows-arm64.msi   (cross-built here; see windows-packaging.md)
# verify: signtool verify /pa target\wix\seed-sync-<ver>-windows-x86_64.msi
```
Ship **both** Windows MSIs or neither: the updater picks its asset from the machine's OS
architecture and will not fall back across architectures, so a release carrying only the x86_64 MSI
leaves every ARM64 install sitting on its current version.

The whole bundle → sign exes → wix → sign-MSI chain runs locally; `-SkipBuild`
reuses an existing `target\release\*.exe`. Signing uses your interactive `az`
session (your user must hold the signer role); see
[`windows-packaging.md`](windows-packaging.md) §3.1/§3.3 for the Azure setup.

**Android APK** (any OS with the Android toolchain — see
[`android-packaging.md`](android-packaging.md)):

```pwsh
cd android; .\gradlew.bat clean :app:assembleRelease
# -> android\app\build\outputs\apk\release\app-release.apk
# rename on upload to seed-sync-<ver>-android-universal.apk
```

**Linux tarball** (Linux or WSL, GTK dev packages installed):

```sh
scripts/package-linux.sh            # -> seed-sync-<ver>-linux-x86_64.tar.gz
```

**macOS universal** (macOS, GTK sourced from conda-forge — see
[`macos-packaging.md`](macos-packaging.md)):

```sh
scripts/package-macos.sh            # -> seed-sync-<ver>-macos-universal.tar.gz
```

Publishing/uploading needs write access to `steeb-k/seed-sync-binaries`: either be logged in
via `gh auth login` as an account with `repo` scope (the maintainer's `steeb-k`
account is), or export a `SEED_BINARIES_TOKEN` PAT (`contents: write` on that repo)
and pass it to `gh` via `GH_TOKEN`. (`gh` created the `vX.Y.Z` tag on *that* repo back in
step 1.)

## 4. Finalize — publish the draft as Latest

Only once every platform you're shipping has its asset attached, flip the draft to a
published, Latest release:

```sh
# refine notes if they were a placeholder, then publish:
gh release edit vX.Y.Z --repo steeb-k/seed-sync-binaries --notes-file release-notes.md
gh release edit vX.Y.Z --repo steeb-k/seed-sync-binaries --draft=false --latest
```

The release marked **Latest** on `seed-sync-binaries` is what the updaters fetch
(`releases/latest`). Finalizing last — never leaving the newest version as a draft or
pre-release, never marking it Latest before its assets are all attached — is what keeps a
half-built release from being handed to every updater.

## 5. Release notes

Write `release-notes.md` (the `--notes-file` above). Keep it consistent with the
downloads list and the minimum OS / Linux deps:

```markdown
**S.E.E.D. (SEED Sync) vX.Y.Z** — P2P mirrored-folder sync.

### Downloads
- **Linux x86_64** — `seed-sync-<ver>-linux-x86_64.tar.gz`
- **macOS universal** (Apple Silicon + Intel) — `seed-sync-<ver>-macos-universal.tar.gz`
- **Windows x86_64** (signed MSI) — `seed-sync-<ver>-windows-x86_64.msi`
- **Windows ARM64** (signed MSI) — `seed-sync-<ver>-windows-arm64.msi`
- **Android** (universal APK) — `seed-sync-<ver>-android-universal.apk`

### System requirements

**Linux** — GTK is *not* bundled; install the runtime packages first:
- GTK 4.10+, libadwaita 1.4+, libdbus-1
- Debian/Ubuntu: `libgtk-4-1 libadwaita-1-0 libdbus-1-3`
- Fedora: `gtk4 libadwaita dbus-libs`
- Arch: `gtk4 libadwaita dbus`

**macOS** — set the floor to whatever your local build targets (building locally
lets you go well below the old CI floor of macOS 14). GTK4 + libadwaita are
bundled in the app; no Homebrew or other runtime needed.

**Windows** — **Windows 10 (64-bit)** or later. GTK4 + libadwaita and all
libraries are bundled in the signed MSI; no separate runtime install required.

**Android** — Android 11 (API 30) or later.
```

macOS/Windows bundle their whole runtime; Linux does not (see the per-OS
`docs/*-packaging.md`).
