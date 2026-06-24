# Cutting a release

There are two paths:

- **Automated (normal):** push a `vX.Y.Z` tag → `.github/workflows/release.yml`
  builds and publishes every platform. Use this when GitHub Actions credits are
  available.
- **Manual / local (no CI):** build each platform on a local machine and publish
  to `seed-sync-binaries` with `gh`. Use this when CI minutes are exhausted (see
  [Manual / local release](#manual--local-release-no-ci-minutes) below).

Either way the artifacts land in the **public** `steeb-k/seed-sync-binaries`
repo as a GitHub Release for the tag.

## Automated release

```sh
# 1. Bump the workspace version (single source of truth) and refresh the lockfile
#    Edit Cargo.toml [workspace.package] version, then:
cargo update --workspace
#    Also bump android/app/build.gradle.kts (versionName + versionCode; see
#    docs/android-packaging.md).
git commit -am "release: vX.Y.Z"
git push origin main

# 2. Tag and push — this triggers the release workflow
git tag vX.Y.Z
git push origin vX.Y.Z
```

The tag **must** match the `Cargo.toml` version (each job sanity-checks this and
fails otherwise). Pushing the tag runs jobs that publish to `seed-sync-binaries`:

| Job | Output | Runner |
| --- | --- | --- |
| `linux` | `seed-sync-<ver>-linux-x86_64.tar.gz` | ubuntu-24.04 |
| `macos` | `seed-sync-<ver>-macos-universal.tar.gz` (arm64+x86_64) | macos-14 |
| `windows` | `seed-sync-<ver>-windows-x86_64.msi` (signed) | windows-latest |
| `notes` | the release body (min OS versions + Linux deps) | ubuntu-24.04 |

> **Android is not in the CI workflow yet** — it is built and published manually
> (see below). Folding it into `release.yml` is a TODO once CI credits return.

## Manual / local release (no CI minutes)

When Actions credits are exhausted, build each platform locally and publish with
`gh`. **This flow is intended to be temporary** — prefer the automated path when
credits return.

Publishing needs write access to `steeb-k/seed-sync-binaries`: either be logged
in via `gh auth login` as an account with `repo` scope (the maintainer's
`steeb-k` account is), or export a `SEED_BINARIES_TOKEN` PAT (`contents: write`
on that repo) and pass it to `gh` via `GH_TOKEN`.

> ⚠️ Pushing a `vX.Y.Z` tag to `steeb-k/seed-sync-gtk` still triggers
> `release.yml`, which **will fail** without CI credits. In this flow, **don't
> push the tag** — create the release directly on `seed-sync-binaries` with `gh`
> (it creates the tag on *that* repo). Push the source tag later, when CI is back.

### 1. Bump the version

Same as the automated step 1 (edit `Cargo.toml`, `cargo update --workspace`,
bump `android/app/build.gradle.kts`), then commit — but **do not push a tag**.

### 2. Build the artifacts (each on its own OS)

Only build the platforms you have a machine for; the release can be assembled
incrementally (upload more assets to the same release later).

**Windows MSI** (Windows, GTK + Azure signing set up — see
[`windows-packaging.md`](windows-packaging.md)):

```pwsh
cargo build --release
az login   # your user must hold the Trusted Signing signer role
# point ARTIFACT_SIGNING_DLIB at Azure.CodeSigning.Dlib.dll (Microsoft.ArtifactSigning.Client NuGet)
pwsh -File scripts\build-msi.ps1 -SkipBuild
# -> target\wix\seed-sync-<ver>-windows-x86_64.msi
```

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

### 3. Publish to seed-sync-binaries

Create the release on the binaries repo and attach whatever you built:

```sh
gh release create vX.Y.Z \
  --repo steeb-k/seed-sync-binaries \
  --title vX.Y.Z \
  --notes-file release-notes.md \
  seed-sync-<ver>-windows-x86_64.msi \
  seed-sync-<ver>-android-universal.apk
# add the linux/macos tarballs to the same release as they get built:
gh release upload vX.Y.Z --repo steeb-k/seed-sync-binaries seed-sync-<ver>-linux-x86_64.tar.gz
```

Keep the release body (`release-notes.md`) consistent with the `notes` job in
`release.yml` (downloads list + min OS / Linux deps).

Each job is independent: if one platform fails, the others still publish. The
`notes` job runs last (`needs: [linux, macos, windows]`, `if: always()`) and is the
single writer of the release body, so it never races the asset uploads.

Min OS / deps published in the notes: macOS 14 (Sonoma), Windows 10 x64, and the
Linux GTK runtime packages. macOS/Windows bundle everything; Linux does not (see
the per-OS `docs/*-packaging.md`).

## Windows signing (Azure Artifact Signing via OIDC)

The MSI and the three EXEs are signed remotely by Azure Artifact Signing (formerly
Trusted Signing) — no cert or secret is stored in CI. The runner authenticates as
an Azure AD service principal via GitHub OIDC and the signtool dlib signs against
the service. One-time setup (already done) is documented in the `windows:` job
header in `release.yml`; the secrets are `AZURE_CLIENT_ID` / `AZURE_TENANT_ID` /
`AZURE_SUBSCRIPTION_ID`, and `SEED_BINARIES_TOKEN` (PAT with `contents: write` on
the binaries repo) publishes the assets.

The Windows job order is **deliberate** and load-bearing — don't reshuffle it:

1. **Build before `azure/login`.** The GitHub OIDC client assertion `azure/login`
   exchanges is valid only ~5 min and a federated SP login has no refresh token, so
   signing must happen within minutes of login. The ~15-min `cargo build` runs
   first (its own step), then login, then `build-msi.ps1 -SkipBuild` (bundle + sign
   + wix + sign MSI, ~2 min).
2. **GTK from the prebuilt gvsbuild zip**, not from source (from-source needs a VS
   toolchain the runner doesn't expose and breaks on gettext). Pinned `GVSBUILD_VER`.
3. **`ExcludeCredentials` in `artifact-signing-metadata.json`** leaves only
   `AzureCliCredential`. Without it the dlib's `DefaultAzureCredential` probes IMDS
   (present on Azure-hosted runners but with no identity) and signtool hangs ~1 hr.
4. **wix extensions added unconditionally** at the engine version (`build-msi.ps1`);
   a name-only "present?" check is unsafe because the runner's preinstalled WiX
   registers a different version.

### Version pins to maintain

- `GVSBUILD_VER` in `release.yml` (prebuilt GTK4 bundle) — bump from
  [wingtk/gvsbuild releases](https://github.com/wingtk/gvsbuild/releases).
- `Microsoft.ArtifactSigning.Client` version in `release.yml` (the signing dlib).
- These move slowly; bump only when needed.

## Testing the Windows pipeline locally

The whole bundle → sign → wix → sign-MSI chain runs locally (skips the 15-min
compile when `target/release/*.exe` already exist). This catches packaging/wix/
signing issues without a CI round-trip:

```pwsh
cargo build --release            # if target\release\*.exe aren't current
# point ARTIFACT_SIGNING_DLIB at Azure.CodeSigning.Dlib.dll from the
# Microsoft.ArtifactSigning.Client NuGet, and `az login` first
pwsh -File scripts\build-msi.ps1 -SkipBuild
# verify: signtool verify /pa target\wix\seed-sync-<ver>-windows-x86_64.msi
```

Note: local signing uses your interactive `az` session (your user must hold the
signer role); CI uses the service principal. The `ExcludeCredentials` list makes
both paths resolve to the Azure CLI credential.
