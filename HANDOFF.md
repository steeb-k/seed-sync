# Hand-off: continue on Linux (written 2026-09-17 on the Windows box)

This file is temporary. Delete it once the `ci-pipeline` branch is merged.

## Where things stand

- `main` carries two engine fixes, both committed, tested and pushed:
  - `fe4e358` known-issues #37 — a share whose folder is on a removed drive no
    longer takes the daemon down (held inert as `FolderMissing`, auto-resume,
    real exit code to the Windows SCM). Suite: `missing_folder`.
  - `2de0137` known-issues #38 — a member that lists a blob it cannot serve
    repairs itself (health probes servability; a refused fetch triggers a
    re-import; vendored iroh-blobs hunk 3). Suite: `serve_repair`.
  The live share (`8318bd1b…`) is still stuck at 89% on the two new members until
  xpsTop runs a build with #38. **Nothing needs doing on the 89% machines.**
- `ci-pipeline` (this branch) = main + the CI/packaging overhaul, implemented by
  workstream against `docs/ci-release.md` (the design and contract — read it
  first). Workstreams: W1 workflows/publish (done), W2 Linux native packages +
  repos (done, reviewed), W3 Flatpak (see the last commit on the branch for its
  state), W4 docs sweep (**not started** — CLAUDE.md, README.md, docs/releasing.md
  and the platform packaging docs still say "no CI"; `release-notes.md` still
  exists).
- The next release is a **feature bump: 0.8.0** (not 0.7.5). Version is not
  bumped yet; `CHANGELOG.md` already has the `## [0.8.0]` section.

## What is NOT verified (this box has no Linux tooling any more)

Both WSL distros here lost their disk images, and there is no Docker, nfpm,
makepkg, flatpak-builder, actionlint or shellcheck. Everything below was
checked by reading and by `bash -n` / YAML parsing only:

1. `actionlint` on `.github/workflows/{build,release,cargo-deny}.yml`.
2. `shellcheck` on `scripts/ci/*.sh`, `scripts/package-linux-native.sh`,
   `scripts/aur-prepare.sh`, `packaging/linux/scripts/*.sh`, `packaging/linux/seed-sync`.
3. `bash scripts/package-linux.sh && bash scripts/package-linux-native.sh` on
   Ubuntu 24.04 → `dist/seed-sync_<v>-1_amd64.deb`, `dist/seed-sync-<v>-1.x86_64.rpm`;
   then `dpkg -c` / `rpm -qpl` against `docs/ci-release.md` §6, `lintian`, `rpmlint`,
   `sudo apt install ./…deb`, `systemctl --user enable --now seed-daemon`,
   `seed-sync --update` must refuse, `apt remove`.
4. `packaging/arch/PKGBUILD`: `makepkg --printsrcinfo`, then a real
   `scripts/ci/arch-pkgbuild.sh` run in an `archlinux` container.
5. `bash scripts/package-flatpak.sh` → the `.flatpak` bundle installs and
   `flatpak run io.github.steeb_k.SeedSync --version` works; the tray shows up.
6. The Windows signing chain, the macOS conda-forge build and the Fedora
   container smoke test only run on hosted runners: the `v0.8.0-test1`
   rehearsal (`docs/ci-release.md` §9) is the proof.

## What only the maintainer can do (one-time)

- Fill `scripts/artifact-signing-metadata.ci.json`: the Trusted Signing
  `CodeSigningAccountName` / `CertificateProfileName` (and endpoint region)
  were never committed here.
- Repo secrets on `steeb-k/seed-sync`: `SEED_BINARIES_TOKEN` (fine-grained PAT,
  contents:write on `steeb-k/seed-sync-binaries`), `AZURE_CLIENT_ID`,
  `AZURE_TENANT_ID`, `AZURE_SUBSCRIPTION_ID` (federated credential subject
  `repo:steeb-k/seed-sync:environment:release`), `ANDROID_KEYSTORE_BASE64`,
  `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS`, `ANDROID_KEY_PASSWORD`.
- Create the GitHub environment `release` on the source repo.
- apps.kznjk.com: add `steeb-k/seed-sync-binaries` to the package poller's
  sources and its flatpak import (host side, outside this repo).
- Decide whether the .deb/.rpm should ship `/etc/xdg/autostart/…SeedSync.desktop`
  (tray autostart for every user) — currently they do not.

## Suggested order on Linux

1. `git checkout ci-pipeline`; run items 1–5 above; fix what they find.
2. Run W4 (docs sweep) — the list is in `docs/ci-release.md` §10.
3. Merge to main, bump `[workspace.package].version` to 0.8.0 and the Android
   literals, commit `release: v0.8.0`, configure secrets, push `v0.8.0-test1`.
4. When the rehearsal is clean: push `v0.8.0`, then `scripts/aur-prepare.sh
   0.8.0 ../seed-sync-aur` and push the AUR update.

Parked feature request (after 0.8.0): hover tooltip on the syncing % showing
which file is in flight, and the fetch error should name the refusing member.
