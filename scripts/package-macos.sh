#!/usr/bin/env bash
# Build the S.E.E.D. macOS release tarball — a self-contained, ad-hoc-signed
# "SEED Sync.app" bundle (bundled GTK). The macOS analog of package-linux.sh.
#
#   scripts/package-macos.sh                cargo build --release, bundle, package
#   scripts/package-macos.sh --skip-build   package the existing target/release bins
#
# Output: dist/seed-sync-<version>-macos-<arch>.tar.gz, whose root holds:
#   SEED Sync.app/   the app (Contents/MacOS = binaries, Contents/lib|share|etc =
#                    bundled GTK, Contents/Resources/AppIcon.icns, Info.plist)
#   seed-sync        the install/update wrapper (bootstrap for the first install)
#   LaunchAgents/    the three launchd .plist templates
#   INSTALL.txt, LICENSE
#
#   arch = the host arch (arm64 in phase 1). Universal2 (lipo) is phase 2; when
#   that lands the asset name switches to ...-macos-universal.tar.gz.
#
# Requires: cargo, Xcode CLT (install_name_tool/codesign/otool/iconutil/sips), and
# Homebrew gtk4 + libadwaita (the bundler copies their dylib closure).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PKG_SRC="$ROOT/packaging/macos"
APP_ID="io.github.steeb_k.SeedSync"
ICON_SRC="$ROOT/icon/appIcon.png"

case "$(uname -m)" in
  arm64)  SLICE=arm64 ;;
  x86_64) SLICE=x86_64 ;;
  *)      SLICE="$(uname -m)" ;;
esac

cd "$ROOT"
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
[ -n "$VERSION" ] || { echo "package-macos: could not read version from Cargo.toml" >&2; exit 1; }
NAME="seed-sync-$VERSION-macos-$SLICE"
echo "package-macos: building $NAME"

if [ "${1:-}" != "--skip-build" ]; then
  cargo build --release -p seed-daemon -p seed-gui -p seed-cli
fi

STAGE="$ROOT/dist/$NAME"
APP="$STAGE/SEED Sync.app"
CONTENTS="$APP/Contents"
rm -rf "$STAGE"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources" "$STAGE/LaunchAgents"

# Binaries live inside the bundle (Contents/MacOS). The seed-sync wrapper is NOT
# bundled here — it's a shell script (can't be sealed as bundle code) and ships at
# the tarball root, from where --install copies it to ~/.local/bin.
for b in seed-daemon seed-gui seed-cli; do
  install -m 0755 "target/release/$b" "$CONTENTS/MacOS/$b"
done

# Bundle + relocate + ad-hoc re-sign the GTK dylib closure into Contents/
# (adds lib/, share/glib-2.0/schemas, etc/fonts; re-signs the binaries too).
BUNDLE_BINDIR=MacOS "$ROOT/scripts/bundle-gtk-macos.sh" "$CONTENTS"

# Info.plist (CFBundleVersion from Cargo).
sed "s/__VERSION__/$VERSION/g" "$PKG_SRC/Info.plist" > "$CONTENTS/Info.plist"

# AppIcon.icns from the master PNG (sips renders each slot; iconutil packs).
ICONSET="$(mktemp -d)/AppIcon.iconset"; mkdir -p "$ICONSET"
for spec in 16:16x16 32:16x16@2x 32:32x32 64:32x32@2x 128:128x128 256:128x128@2x \
            256:256x256 512:256x256@2x 512:512x512 1024:512x512@2x; do
  px="${spec%%:*}"; nm="${spec##*:}"
  sips -z "$px" "$px" "$ICON_SRC" --out "$ICONSET/icon_$nm.png" >/dev/null 2>&1
done
iconutil -c icns "$ICONSET" -o "$CONTENTS/Resources/AppIcon.icns"
rm -rf "$(dirname "$ICONSET")"
echo "package-macos: wrote AppIcon.icns"

# Seal the bundle (ad-hoc). Nested dylibs/helpers are already individually signed
# by the bundler; this signs the main executable + Contents seal.
codesign --force --sign - --timestamp=none "$APP" >/dev/null 2>&1 \
  || { echo "package-macos: bundle codesign failed" >&2; exit 1; }

# Tarball-root extras: bootstrap wrapper, docs, launchd templates.
install -m 0755 "$PKG_SRC/seed-sync"   "$STAGE/seed-sync"
install -m 0644 "$PKG_SRC/INSTALL.txt" "$STAGE/INSTALL.txt"
install -m 0644 "$ROOT/LICENSE"        "$STAGE/LICENSE"
for p in daemon update gui; do
  install -m 0644 "$PKG_SRC/$APP_ID.$p.plist" "$STAGE/LaunchAgents/$APP_ID.$p.plist"
done

# Tarball (preserve the .app's signature/symlinks).
mkdir -p "$ROOT/dist"
tar -czf "$ROOT/dist/$NAME.tar.gz" -C "$ROOT/dist" "$NAME"
echo "package-macos: wrote dist/$NAME.tar.gz ($(du -sh "$ROOT/dist/$NAME.tar.gz" | awk '{print $1}'))"
