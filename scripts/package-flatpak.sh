#!/usr/bin/env bash
# Build the S.E.E.D. Flatpak bundle: a single-file `.flatpak` built from
# packaging/flatpak/io.github.steeb_k.SeedSync.yml (see that manifest's own
# comments, and docs/linux-packaging.md's Flatpak section, for the design).
#
#   scripts/package-flatpak.sh
#
# Output: dist/io.github.steeb_k.SeedSync-<version>-x86_64.flatpak
#
# Requires: flatpak, flatpak-builder. Installs the GNOME 48 runtime/SDK + the
# rust-stable SDK extension from Flathub into the user installation if they
# aren't already present (first run only; subsequent runs skip straight to
# the build). No --skip-build: flatpak-builder always does its own
# (cached-by-checksum) incremental build, so there's nothing extra to skip.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_ID="io.github.steeb_k.SeedSync"
MANIFEST="$ROOT/packaging/flatpak/$APP_ID.yml"
RUNTIME_BRANCH="48"

cd "$ROOT"

command -v flatpak >/dev/null 2>&1 || {
  echo "package-flatpak: 'flatpak' not found on PATH." >&2
  exit 1
}
command -v flatpak-builder >/dev/null 2>&1 || {
  echo "package-flatpak: 'flatpak-builder' not found on PATH." >&2
  exit 1
}

VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
[ -n "${VERSION:-}" ] || { echo "package-flatpak: could not read version from Cargo.toml" >&2; exit 1; }

# Flathub remote + runtime/SDK/extension, user installation, only if missing —
# a maintainer building repeatedly shouldn't re-resolve/re-pull every time.
flatpak remote-add --if-not-exists --user flathub https://dl.flathub.org/repo/flathub.flatpakrepo

need_ref() { ! flatpak info --user "$1" >/dev/null 2>&1; }

REFS=(
  "org.gnome.Platform//$RUNTIME_BRANCH"
  "org.gnome.Sdk//$RUNTIME_BRANCH"
  "org.freedesktop.Sdk.Extension.rust-stable//$RUNTIME_BRANCH"
)
MISSING=()
for ref in "${REFS[@]}"; do
  need_ref "$ref" && MISSING+=("$ref")
done
if [ "${#MISSING[@]}" -gt 0 ]; then
  echo "package-flatpak: installing missing runtime(s): ${MISSING[*]}"
  flatpak install --user -y flathub "${MISSING[@]}"
fi

mkdir -p dist
REPO="$ROOT/dist/flatpak-repo"
BUILD="$ROOT/dist/flatpak-build"
BUNDLE="$ROOT/dist/$APP_ID-$VERSION-x86_64.flatpak"

echo "package-flatpak: building $APP_ID $VERSION"
flatpak-builder --force-clean --user --install-deps-from=flathub \
  --repo="$REPO" "$BUILD" "$MANIFEST"

rm -f "$BUNDLE"
flatpak build-bundle "$REPO" "$BUNDLE" "$APP_ID"

# The publish pipeline (docs/ci-release.md §3) matches this asset by exact
# name; catch a naming drift here rather than at release time.
EXPECTED="dist/$APP_ID-$VERSION-x86_64.flatpak"
[ -f "$ROOT/$EXPECTED" ] || {
  echo "package-flatpak: expected bundle '$EXPECTED' was not produced" >&2
  exit 1
}

echo "package-flatpak: wrote $EXPECTED"
