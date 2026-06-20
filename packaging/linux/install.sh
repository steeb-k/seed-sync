#!/usr/bin/env bash
# S.E.E.D. (Seed Sync) — per-user installer (no root).
#
#   ./install.sh                     install + enable daemon and daily auto-update
#   ./install.sh --no-auto-update    skip the daily update timer
#   ./install.sh --no-gui-autostart  don't auto-launch the tray GUI at login
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
BIN_DIR="$HOME/.local/bin"
APP_ID="io.github.steeb_k.SeedSync"
UNIT_DIR="$HOME/.config/systemd/user"
AUTOSTART_DIR="$HOME/.config/autostart"

AUTO_UPDATE=1
GUI_AUTOSTART=1
for arg in "$@"; do
  case "$arg" in
    --no-auto-update)   AUTO_UPDATE=0 ;;
    --no-gui-autostart) GUI_AUTOSTART=0 ;;
    --system) echo "install.sh: --system (root install) is not supported in this build; use the per-user install." >&2; exit 2 ;;
    -h|--help) sed -n '2,7p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "install.sh: unknown argument: $arg" >&2; exit 2 ;;
  esac
done

log() { printf '%s\n' "install: $*"; }
have() { command -v "$1" >/dev/null 2>&1; }
uctl_ok() { systemctl --user "$@" 2>/dev/null; }

# --- runtime dependency check (warn, don't block) ---------------------------
if have ldd; then
  missing="$(ldd "$HERE/bin/seed-gui" 2>/dev/null | awk '/not found/{print $1}' | sort -u || true)"
  if [ -n "$missing" ]; then
    echo "install: WARNING — missing shared libraries:" >&2
    printf '  %s\n' $missing >&2
    echo "  Install GTK 4.10+, libadwaita 1.4+, and libdbus-1 from your distro, then re-run." >&2
  fi
fi

# --- place binaries + desktop/icon/unit assets (shared logic) ---------------
log "installing files"
bash "$HERE/seed-sync-update" --apply-from "$HERE"

# --- enable the daemon (and timer) via systemd --user -----------------------
if uctl_ok daemon-reload; then
  if uctl_ok enable --now seed-daemon.service; then
    log "seed-daemon enabled (auto-starts at login)"
  else
    log "WARNING: could not enable seed-daemon via systemd --user; start it manually: seed-daemon run"
  fi
  if [ "$AUTO_UPDATE" = 1 ]; then
    uctl_ok enable --now seed-sync-update.timer && log "daily auto-update enabled" \
      || log "WARNING: could not enable the update timer"
  fi
else
  log "WARNING: systemd --user unavailable; the daemon was not started automatically."
fi

# --- GUI tray autostart at login (mirrors the Windows --hidden autostart) ---
if [ "$GUI_AUTOSTART" = 1 ] && [ -f "$HOME/.local/share/applications/$APP_ID.desktop" ]; then
  mkdir -p "$AUTOSTART_DIR"
  sed -e "s|^Exec=.*|Exec=$BIN_DIR/seed-gui --hidden|" \
      "$HOME/.local/share/applications/$APP_ID.desktop" > "$AUTOSTART_DIR/$APP_ID.desktop"
  echo "X-GNOME-Autostart-enabled=true" >> "$AUTOSTART_DIR/$APP_ID.desktop"
  log "tray GUI will start at login"
fi

# --- PATH hint --------------------------------------------------------------
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) log "NOTE: $BIN_DIR is not on your PATH — add it to run seed-cli/seed-sync-update directly." ;;
esac

log "done. Launch \"S.E.E.D.\" from your app menu, or run: seed-gui"
