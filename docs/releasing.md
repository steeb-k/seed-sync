# Cutting a release

Releases are built **locally, on each platform's own machine**, and published as
a GitHub Release on the **public** `steeb-k/seed-sync-binaries` repo. There is no
GitHub Actions / CI path — the old `.github/workflows/release.yml` was removed in
favor of local builds (notably so the macOS asset can target an OS floor well
below what the hosted `macos-14` runner allowed).

The auto-updaters (Windows scheduled task, Linux timer/service, macOS launchd, and
the `web-install.sh` bootstrap) are **version-driven**: each compares the installed
`seed-daemon --version` against the latest release tag on `seed-sync-binaries` and
upgrades only when the release is newer. So every release **must** bump the version.

## 1. Bump the version

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

## 2. Build the artifacts (each on its own OS)

Only build the platforms you have a machine for — the release is assembled
incrementally, so you can attach more assets to the same release later. Every
artifact is named `seed-sync-<ver>-<platform>`.

**Windows MSI** (Windows, GTK + Azure signing set up — see
[`windows-packaging.md`](windows-packaging.md)):

```pwsh
cargo build --release
az login   # your user must hold the Azure Artifact Signing signer role
# point ARTIFACT_SIGNING_DLIB at Azure.CodeSigning.Dlib.dll (from the
# Microsoft.ArtifactSigning.Client NuGet)
pwsh -File scripts\build-msi.ps1 -SkipBuild
# -> target\wix\seed-sync-<ver>-windows-x86_64.msi
# verify: signtool verify /pa target\wix\seed-sync-<ver>-windows-x86_64.msi
```

The whole bundle → sign exes → wix → sign-MSI chain runs locally; `-SkipBuild`
reuses an existing `target\release\*.exe`. Local signing uses your interactive
`az` session (your user must hold the signer role). The `ExcludeCredentials` list
in `artifact-signing-metadata.json` pins the dlib to `AzureCliCredential` so it
uses that session instead of probing IMDS (which otherwise hangs ~1 hr).

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

**macOS universal** (macOS, both arches via Rosetta/second Homebrew — see
[`macos-packaging.md`](macos-packaging.md)):

```sh
scripts/package-macos.sh            # -> seed-sync-<ver>-macos-universal.tar.gz
```

## 3. Publish to seed-sync-binaries

Publishing needs write access to `steeb-k/seed-sync-binaries`: either be logged in
via `gh auth login` as an account with `repo` scope (the maintainer's `steeb-k`
account is), or export a `SEED_BINARIES_TOKEN` PAT (`contents: write` on that repo)
and pass it to `gh` via `GH_TOKEN`.

Create the release on the binaries repo and attach whatever you built (`gh`
creates the `vX.Y.Z` tag on *that* repo):

```sh
gh release create vX.Y.Z \
  --repo steeb-k/seed-sync-binaries \
  --title vX.Y.Z \
  --notes-file release-notes.md \
  seed-sync-<ver>-windows-x86_64.msi \
  seed-sync-<ver>-android-universal.apk
# add assets to the same release as they get built on each OS:
gh release upload vX.Y.Z --repo steeb-k/seed-sync-binaries seed-sync-<ver>-linux-x86_64.tar.gz
gh release upload vX.Y.Z --repo steeb-k/seed-sync-binaries seed-sync-<ver>-macos-universal.tar.gz
```

The release marked **Latest** on `seed-sync-binaries` is what the updaters fetch
(`releases/latest`). Make sure the newest version is the one published last / not
left as a draft or pre-release.

## 4. Release notes

Write `release-notes.md` (the `--notes-file` above). Keep it consistent with the
downloads list and the minimum OS / Linux deps:

```markdown
**S.E.E.D. (SEED Sync) vX.Y.Z** — P2P mirrored-folder sync.

### Downloads
- **Linux x86_64** — `seed-sync-<ver>-linux-x86_64.tar.gz`
- **macOS universal** (Apple Silicon + Intel) — `seed-sync-<ver>-macos-universal.tar.gz`
- **Windows x86_64** (signed MSI) — `seed-sync-<ver>-windows-x86_64.msi`
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
