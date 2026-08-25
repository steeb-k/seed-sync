#!/usr/bin/env bash
# Build the S.E.E.D. Linux release tarball.
#
#   scripts/package-linux.sh            cargo build --release, then package
#   scripts/package-linux.sh --skip-build   package the existing target/release bins
#
# Output: dist/seed-sync-<version>-linux-x86_64.tar.gz
#
# Requires: cargo, tar, and ImageMagick (`magick` or `convert`) for icon sizes.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_ID="io.github.steeb_k.SeedSync"
PKG_SRC="$ROOT/packaging/linux"
ICON_SRC="$ROOT/icon/appIcon.png"
ICON_SIZES="16 22 24 32 48 64 128 256 512"

cd "$ROOT"
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
[ -n "$VERSION" ] || { echo "package-linux: could not read version from Cargo.toml" >&2; exit 1; }
NAME="seed-sync-$VERSION-linux-x86_64"
echo "package-linux: building $NAME"

if [ "${1:-}" != "--skip-build" ]; then
  cargo build --release -p seed-daemon -p seed-gui -p seed-cli
fi

# ImageMagick entrypoint (IM7 = magick, IM6 = convert).
if command -v magick >/dev/null 2>&1; then IM="magick"
elif command -v convert >/dev/null 2>&1; then IM="convert"
else echo "package-linux: ImageMagick (magick/convert) is required for icon generation" >&2; exit 1
fi

# Copy a text file with LF line endings. `.gitattributes` pins LF in the working
# tree, but a tree cloned before that landed (or unpacked from a zip) can still
# hold CRLF — and a CRLF wrapper ships a `#!/usr/bin/env bash\r` shebang that
# fails on the target with `env: 'bash\r': No such file or directory`. Normalize
# at package time so the artifact is right regardless of how the tree got here.
install_text() {  # install_text <mode> <src> <dst>
  tr -d '\r' < "$2" > "$3"
  chmod "$1" "$3"
}

STAGE="$ROOT/dist/$NAME"
rm -rf "$STAGE"
mkdir -p "$STAGE/bin" \
         "$STAGE/share/applications" \
         "$STAGE/share/metainfo" \
         "$STAGE/lib/systemd/user"

# Binaries
for b in seed-daemon seed-gui seed-cli; do
  install -m 0755 "target/release/$b" "$STAGE/bin/$b"
done

# Installer/updater wrapper + docs (tarball root)
install_text 0755 "$PKG_SRC/seed-sync"   "$STAGE/seed-sync"
install_text 0644 "$PKG_SRC/INSTALL.txt" "$STAGE/INSTALL.txt"
install_text 0644 "$ROOT/LICENSE"        "$STAGE/LICENSE"

# Desktop entry + AppStream metadata
install_text 0644 "$PKG_SRC/$APP_ID.desktop"       "$STAGE/share/applications/$APP_ID.desktop"
install_text 0644 "$PKG_SRC/$APP_ID.metainfo.xml"  "$STAGE/share/metainfo/$APP_ID.metainfo.xml"

# systemd --user units
install_text 0644 "$PKG_SRC/seed-daemon.service"        "$STAGE/lib/systemd/user/seed-daemon.service"
install_text 0644 "$PKG_SRC/seed-sync-update.service"   "$STAGE/lib/systemd/user/seed-sync-update.service"
install_text 0644 "$PKG_SRC/seed-sync-update.timer"     "$STAGE/lib/systemd/user/seed-sync-update.timer"

# hicolor icons, generated from the master PNG (pre-rendered so targets need no tools)
for sz in $ICON_SIZES; do
  dir="$STAGE/share/icons/hicolor/${sz}x${sz}/apps"
  mkdir -p "$dir"
  "$IM" "$ICON_SRC" -resize "${sz}x${sz}" "$dir/$APP_ID.png"
done

# Tarball
mkdir -p "$ROOT/dist"
tar -czf "$ROOT/dist/$NAME.tar.gz" -C "$ROOT/dist" "$NAME"
echo "package-linux: wrote dist/$NAME.tar.gz"
