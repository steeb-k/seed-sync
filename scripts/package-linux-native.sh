#!/usr/bin/env bash
# Build the S.E.E.D. .deb and .rpm from the tree scripts/package-linux.sh stages.
#
#   scripts/package-linux.sh && scripts/package-linux-native.sh
#
# Output (distro-conventional names, from nfpm):
#   dist/seed-sync_<version>-1_amd64.deb
#   dist/seed-sync-<version>-1.x86_64.rpm
#
# Requires nfpm on PATH: https://github.com/goreleaser/nfpm/releases
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

APP_ID="io.github.steeb_k.SeedSync"

command -v nfpm >/dev/null 2>&1 || {
  echo "package-linux-native: nfpm not found on PATH." >&2
  echo "  Download nfpm_<ver>_Linux_x86_64.tar.gz from https://github.com/goreleaser/nfpm/releases" >&2
  exit 1
}

VERSION="$(grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
[ -n "${VERSION:-}" ] || { echo "package-linux-native: could not read version from Cargo.toml" >&2; exit 1; }

STAGE="dist/seed-sync-${VERSION}-linux-x86_64"
[ -x "$STAGE/bin/seed-daemon" ] || {
  echo "package-linux-native: $STAGE is missing; run scripts/package-linux.sh first" >&2
  exit 1
}

# nfpm.yaml reads from these fixed paths (nfpm expands no variables in content
# src paths).
NATIVE="dist/native"
rm -rf "$NATIVE"
mkdir -p "$NATIVE/metainfo"
cp -a "$STAGE" "$NATIVE/tree"
# Distribution policy wants a fixed interpreter in /usr/bin, not `env`
# (lintian/rpmlint: env-script-interpreter). The tarball keeps `env`.
sed -i '1s|^#!/usr/bin/env bash$|#!/bin/bash|' "$NATIVE/tree/seed-sync"
head -n1 "$NATIVE/tree/seed-sync" | grep -qx '#!/bin/bash' || {
  echo "package-linux-native: could not pin the seed-sync shebang to /bin/bash" >&2
  exit 1
}

# Package-variant unit: /usr/bin instead of the tarball's %h/.local/bin. Ship
# the checked-in pkg/ copy as-is, but assert it says what we expect so a stale
# edit there fails loudly instead of silently shipping the wrong ExecStart.
cp "packaging/linux/pkg/seed-daemon.service" "$NATIVE/seed-daemon.service"
grep -qx 'ExecStart=/usr/bin/seed-daemon run' "$NATIVE/seed-daemon.service" || {
  echo "package-linux-native: packaging/linux/pkg/seed-daemon.service does not ExecStart /usr/bin/seed-daemon" >&2
  exit 1
}

# Package-variant desktop file (Exec=seed-gui, no __BIN__ placeholder) overlays
# the staged tree's tarball copy.
cp "packaging/linux/pkg/$APP_ID.desktop" "$NATIVE/tree/share/applications/$APP_ID.desktop"
grep -qx 'Exec=seed-gui' "$NATIVE/tree/share/applications/$APP_ID.desktop" || {
  echo "package-linux-native: packaging/linux/pkg/$APP_ID.desktop does not Exec=seed-gui" >&2
  exit 1
}

# Generate a <releases> block from CHANGELOG.md (Keep a Changelog: "## [X.Y.Z]
# - YYYY-MM-DD" headings), inserted before </component>. Tolerate CHANGELOG.md's
# absence (not every checkout has one yet) with a single release for the current
# version. Every <release> carries a date: appstreamcli validate (which CI runs
# on the installed package) flags one that does not, so an undated heading — or
# no CHANGELOG.md at all — falls back to the build date rather than to nothing.
# A section for a version NEWER than the one being built (the next release's
# notes, written before the bump) is left out: the newest <release> must be the
# package's own version.
METAINFO_SRC="packaging/linux/$APP_ID.metainfo.xml"
METAINFO_OUT="$NATIVE/metainfo/$APP_ID.metainfo.xml"
RELEASES_FILE="$(mktemp)"
trap 'rm -f "$RELEASES_FILE"' EXIT

TODAY="$(date -u +%Y-%m-%d)"
{
  echo '  <releases>'
  if [ -f CHANGELOG.md ] && grep -qE '^## \[[0-9]+\.[0-9]+\.[0-9]+\]' CHANGELOG.md; then
    grep -E '^## \[[0-9]+\.[0-9]+\.[0-9]+\]' CHANGELOG.md | while IFS= read -r line; do
      ver="$(printf '%s' "$line" | sed -E 's/^## \[([0-9]+\.[0-9]+\.[0-9]+)\].*/\1/')"
      # Skip a version newer than $VERSION: sort -V puts $VERSION last iff ver <= VERSION.
      [ "$(printf '%s\n%s\n' "$ver" "$VERSION" | sort -V | tail -n1)" = "$VERSION" ] || continue
      date="$(printf '%s' "$line" | grep -oE '[0-9]{4}-[0-9]{2}-[0-9]{2}' || true)"
      echo "    <release version=\"$ver\" date=\"${date:-$TODAY}\"/>"
    done
  else
    echo "    <release version=\"$VERSION\" date=\"$TODAY\"/>"
  fi
  echo '  </releases>'
} > "$RELEASES_FILE"

# tr: this repo's working tree can hold CRLF for files checked out before
# .gitattributes pinned eol=lf (same reason package-linux.sh normalizes what it
# stages); the generated block is LF, so mixing the two would ship a metainfo
# file with both.
tr -d '\r' < "$METAINFO_SRC" | awk -v relfile="$RELEASES_FILE" '
  /<\/component>/ { while ((getline line < relfile) > 0) print line; close(relfile) }
  { print }
' > "$METAINFO_OUT"

grep -q '<releases>' "$METAINFO_OUT" || {
  echo "package-linux-native: failed to insert a <releases> block into $METAINFO_OUT" >&2
  exit 1
}

export SEED_VERSION="$VERSION"
rm -f "dist/seed-sync_${VERSION}-1_amd64.deb" "dist/seed-sync-${VERSION}-1.x86_64.rpm"
nfpm package -f packaging/linux/nfpm.yaml -p deb -t dist/
nfpm package -f packaging/linux/nfpm.yaml -p rpm -t dist/

for f in "dist/seed-sync_${VERSION}-1_amd64.deb" "dist/seed-sync-${VERSION}-1.x86_64.rpm"; do
  [ -f "$f" ] || { echo "package-linux-native: nfpm did not write $f" >&2; ls -la dist >&2; exit 1; }
  echo "package-linux-native: wrote $f"
done
