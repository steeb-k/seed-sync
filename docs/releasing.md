# Cutting a release

Releases are fully automated by `.github/workflows/release.yml`. To cut one:

```sh
# 1. Bump the workspace version (single source of truth) and refresh the lockfile
#    Edit Cargo.toml [workspace.package] version, then:
cargo update --workspace
git commit -am "release: vX.Y.Z"
git push origin main

# 2. Tag and push — this triggers the release workflow
git tag vX.Y.Z
git push origin vX.Y.Z
```

The tag **must** match the `Cargo.toml` version (each job sanity-checks this and
fails otherwise). Pushing the tag runs four jobs that publish to the **public**
`steeb-k/seed-sync-binaries` repo as a GitHub Release for the tag:

| Job | Output | Runner |
| --- | --- | --- |
| `linux` | `seed-sync-<ver>-linux-x86_64.tar.gz` | ubuntu-24.04 |
| `macos` | `seed-sync-<ver>-macos-universal.tar.gz` (arm64+x86_64) | macos-14 |
| `windows` | `seed-sync-<ver>-windows-x86_64.msi` (signed) | windows-latest |
| `notes` | the release body (min OS versions + Linux deps) | ubuntu-24.04 |

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
