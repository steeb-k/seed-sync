#!/usr/bin/env bash
# Build the S.E.E.D. macOS release tarball — a self-contained, ad-hoc-signed
# "SEED Sync.app" bundle (bundled GTK). The macOS analog of package-linux.sh.
#
#   scripts/package-macos.sh                cargo build --release, bundle, package
#   scripts/package-macos.sh --skip-build   package the existing per-arch release bins
#
# Output: dist/seed-sync-<version>-macos-<arch>.tar.gz
#   arch = "universal" when an x86_64 Homebrew (/usr/local) + the x86_64 Rust target
#   are present (each Mach-O is lipo'd arm64+x86_64); otherwise "arm64".
#
# Universal2 needs a SECOND, x86_64 Homebrew at /usr/local (under Rosetta) with
# gtk4 + libadwaita installed — Homebrew GTK is single-arch, so the x86_64 dylib
# closure comes from there and is lipo'd against the arm64 closure from /opt/homebrew.
#
# Requires: cargo, Xcode CLT (install_name_tool/codesign/otool/lipo/iconutil/sips),
# Homebrew gtk4 + libadwaita (arm64; plus x86_64 at /usr/local for universal).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PKG_SRC="$ROOT/packaging/macos"
APP_ID="io.github.steeb_k.SeedSync"
ICON_SRC="$ROOT/icon/appIcon.png"
ARM_BREW="$(brew --prefix)"          # arm64 Homebrew (/opt/homebrew)
X86_BREW="/usr/local"                # x86_64 Homebrew (Rosetta), if present
SKIP_BUILD=0; [ "${1:-}" = "--skip-build" ] && SKIP_BUILD=1

cd "$ROOT"
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
[ -n "$VERSION" ] || { echo "package-macos: could not read version from Cargo.toml" >&2; exit 1; }

# Universal iff the x86_64 brew has GTK AND the x86_64 Rust target is installed.
UNIVERSAL=0
if [ -x "$X86_BREW/bin/brew" ] && [ -d "$X86_BREW/opt/gtk4" ] \
   && rustup target list --installed 2>/dev/null | grep -q '^x86_64-apple-darwin$'; then
  UNIVERSAL=1
fi
[ "$UNIVERSAL" = 1 ] && SLICE=universal || SLICE=arm64
NAME="seed-sync-$VERSION-macos-$SLICE"
echo "package-macos: building $NAME ($([ "$UNIVERSAL" = 1 ] && echo 'arm64 + x86_64 lipo' || echo 'arm64 only'))"

ARM_TGT="aarch64-apple-darwin"
X86_TGT="x86_64-apple-darwin"
ARM_BIN="$ROOT/target/$ARM_TGT/release"
X86_BIN="$ROOT/target/$X86_TGT/release"

if [ "$SKIP_BUILD" != 1 ]; then
  cargo build --release --target "$ARM_TGT" -p seed-daemon -p seed-gui -p seed-cli
  if [ "$UNIVERSAL" = 1 ]; then
    echo "package-macos: building x86_64 slice against $X86_BREW GTK"
    # pkg-config must resolve the x86_64 GTK closure with NO arm64 leakage, so set
    # PKG_CONFIG_LIBDIR (replaces the default search path) to the x86_64 brew's
    # per-formula pkgconfig dirs (covers keg-only) + linked lib/share + Homebrew's
    # per-macOS-version stubs for system libs (zlib/libffi/expat/…). ALLOW_CROSS
    # because the host (arm64) != target (x86_64).
    macos_maj="$(sw_vers -productVersion | cut -d. -f1)"
    x86_pc="$(ls -d "$X86_BREW"/opt/*/lib/pkgconfig 2>/dev/null | tr '\n' ':')$X86_BREW/lib/pkgconfig:$X86_BREW/share/pkgconfig:$X86_BREW/Homebrew/Library/Homebrew/os/mac/pkgconfig/$macos_maj"
    PKG_CONFIG_LIBDIR="$x86_pc" \
    PKG_CONFIG_ALLOW_CROSS=1 \
      cargo build --release --target "$X86_TGT" -p seed-daemon -p seed-gui -p seed-cli
  fi
fi

STAGE="$ROOT/dist/$NAME"
APP="$STAGE/SEED Sync.app"
CONTENTS="$APP/Contents"
rm -rf "$STAGE"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources" "$STAGE/LaunchAgents"

# --- arm64 .app: binaries + bundled arm64 GTK closure (this is the base tree) ---
for b in seed-daemon seed-gui seed-cli; do
  install -m 0755 "$ARM_BIN/$b" "$CONTENTS/MacOS/$b"
done
BUNDLE_BREW="$ARM_BREW" BUNDLE_BINDIR=MacOS "$ROOT/scripts/bundle-gtk-macos.sh" "$CONTENTS"

# --- universal: build a parallel x86_64 .app, then lipo each Mach-O into the base.
if [ "$UNIVERSAL" = 1 ]; then
  echo "package-macos: bundling x86_64 closure + lipo'ing into the universal app"
  X86_STAGE="$(mktemp -d)/x86"
  X86_CONTENTS="$X86_STAGE/Contents"
  mkdir -p "$X86_CONTENTS/MacOS"
  for b in seed-daemon seed-gui seed-cli; do
    install -m 0755 "$X86_BIN/$b" "$X86_CONTENTS/MacOS/$b"
  done
  BUNDLE_BREW="$X86_BREW" BUNDLE_BINDIR=MacOS "$ROOT/scripts/bundle-gtk-macos.sh" "$X86_CONTENTS"

  # lipo every Mach-O present in both trees (binaries, dylibs, pixbuf loaders).
  lipo_merge() {
    local rel="$1" arm="$CONTENTS/$1" x86="$X86_CONTENTS/$1"
    [ -f "$x86" ] || { echo "package-macos: WARNING x86_64 missing $rel (arm64-only in fat binary)"; return; }
    lipo -create "$arm" "$x86" -output "$arm.fat" && mv -f "$arm.fat" "$arm"
  }
  for b in seed-daemon seed-gui seed-cli; do lipo_merge "MacOS/$b"; done
  for f in "$CONTENTS"/lib/*.dylib; do lipo_merge "lib/$(basename "$f")"; done
  for f in "$CONTENTS"/lib/gdk-pixbuf-2.0/2.10.0/loaders/*.so; do
    lipo_merge "lib/gdk-pixbuf-2.0/2.10.0/loaders/$(basename "$f")"
  done
  # x86_64-only dylibs the arm64 closure didn't pull in (rare) — copy + sign them.
  for f in "$X86_CONTENTS"/lib/*.dylib; do
    base="$(basename "$f")"
    [ -f "$CONTENTS/lib/$base" ] || { cp -f "$f" "$CONTENTS/lib/$base"; echo "package-macos: added x86_64-only $base"; }
  done
  rm -rf "$(dirname "$X86_STAGE")"

  # lipo invalidated every ad-hoc signature — re-sign inside-out.
  echo "package-macos: re-signing after lipo"
  for f in "$CONTENTS"/lib/*.dylib "$CONTENTS"/lib/gdk-pixbuf-2.0/2.10.0/loaders/*.so; do
    codesign --force --sign - --timestamp=none "$f" >/dev/null 2>&1 || { echo "re-sign failed: $f" >&2; exit 1; }
  done
  for b in seed-gui seed-daemon seed-cli; do
    codesign --force --sign - --timestamp=none "$CONTENTS/MacOS/$b" >/dev/null 2>&1
  done
fi

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

# Seal the bundle (ad-hoc). Nested dylibs/helpers are already individually signed.
codesign --force --sign - --timestamp=none "$APP" >/dev/null 2>&1 \
  || { echo "package-macos: bundle codesign failed" >&2; exit 1; }

# Verify the arch(es) actually landed.
echo "package-macos: seed-gui arches -> $(lipo -archs "$CONTENTS/MacOS/seed-gui" 2>/dev/null)"

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
