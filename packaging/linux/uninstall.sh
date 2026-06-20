#!/usr/bin/env bash
# S.E.E.D. (Seed Sync) — per-user uninstaller.
#
#   ./uninstall.sh           remove the app; keep synced-share settings + data
#   ./uninstall.sh --purge   also remove ~/.local/share/SeedSync (data + state)
set -euo pipefail

BIN_DIR="$HOME/.local/bin"
APP_ID="io.github.steeb_k.SeedSync"
UNIT_DIR="$HOME/.config/systemd/user"
APPS_DIR="$HOME/.local/share/applications"
AUTOSTART_DIR="$HOME/.config/autostart"
ICONS_DIR="$HOME/.local/share/icons/hicolor"
METAINFO_DIR="$HOME/.local/share/metainfo"
# Lowercase: the `directories` crate lowercases the app name on Linux.
DATA_DIR="$HOME/.local/share/seedsync"

PURGE=0
[ "${1:-}" = "--purge" ] && PURGE=1

log() { printf '%s\n' "uninstall: $*"; }
have() { command -v "$1" >/dev/null 2>&1; }
uctl() { systemctl --user "$@" 2>/dev/null || true; }

uctl disable --now seed-sync-update.timer
uctl disable --now seed-daemon.service
uctl stop seed-sync-update.service

rm -f "$UNIT_DIR/seed-daemon.service" \
      "$UNIT_DIR/seed-sync-update.service" \
      "$UNIT_DIR/seed-sync-update.timer"
uctl daemon-reload

rm -f "$BIN_DIR/seed-daemon" "$BIN_DIR/seed-gui" "$BIN_DIR/seed-cli" "$BIN_DIR/seed-sync-update"
rm -f "$APPS_DIR/$APP_ID.desktop" "$AUTOSTART_DIR/$APP_ID.desktop"
rm -f "$METAINFO_DIR/$APP_ID.metainfo.xml"
find "$ICONS_DIR" -type f -name "$APP_ID.png" -delete 2>/dev/null || true

have gtk-update-icon-cache && gtk-update-icon-cache -qtf "$ICONS_DIR" 2>/dev/null || true
have update-desktop-database && update-desktop-database -q "$APPS_DIR" 2>/dev/null || true

log "removed binaries, units, launcher, icons."

if [ "$PURGE" = 1 ]; then
  rm -rf "$DATA_DIR"
  log "purged $DATA_DIR (note: any master seed stored in the OS keyring is left intact)."
else
  log "kept your data + share settings at $DATA_DIR (use --purge to remove)."
fi
