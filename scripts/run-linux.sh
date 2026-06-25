#!/usr/bin/env bash
# Build and launch the SEED Sync GTK GUI on Linux (incl. WSL2/WSLg).
#
# Self-checks the Linux dev environment (Rust/cargo, pkg-config, GTK4/libadwaita dev
# packages, a display) and refers to docs/dev-environment.md if anything's missing.
#
# Usage:
#   bash scripts/run-linux.sh               # build (release) + run daemon + GUI
#   bash scripts/run-linux.sh --skip-build  # run without rebuilding
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DOCREF="-> See docs/dev-environment.md (Linux/WSL section) for setup."
fail() { printf 'ERROR: %s\n%s\n' "$*" "$DOCREF" >&2; exit 1; }
warn() { printf 'WARN:  %s\n' "$*" >&2; }
step() { printf '==> %s\n' "$*"; }

SKIP_BUILD=0
[ "${1:-}" = "--skip-build" ] && SKIP_BUILD=1

# ----------------------------------------------------------------- env checks ---
step "Checking Linux dev environment"
# cargo (source rustup env if needed)
if ! command -v cargo >/dev/null 2>&1; then
  [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
fi
command -v cargo      >/dev/null 2>&1 || fail "Rust/cargo not found. Install rustup: https://sh.rustup.rs"
command -v pkg-config >/dev/null 2>&1 || fail "pkg-config not found. (sudo apt install pkg-config build-essential)"

missing=""
pkg-config --exists gtk4         || missing="$missing libgtk-4-dev"
pkg-config --exists libadwaita-1 || missing="$missing libadwaita-1-dev"
pkg-config --exists dbus-1       || missing="$missing libdbus-1-dev"
[ -n "$missing" ] && fail "Missing GTK dev packages. Run: sudo apt install$missing"

# GTK >= 4.10 needed for the gtk4 v4_10 feature
gtkver="$(pkg-config --modversion gtk4)"
case "$gtkver" in
  4.[0-9].*|4.10.*|4.1[0-9].*|4.[2-9][0-9].*) : ;;  # 4.10+ ok (covers 4.1x/4.2x)
esac
gtk_major_minor="${gtkver%.*}"
if [ "${gtk_major_minor%%.*}" = "4" ] && [ "${gtk_major_minor#*.}" -lt 10 ] 2>/dev/null; then
  fail "gtk4 $gtkver is too old (need >= 4.10). Use Ubuntu 24.04+ or a distro with GTK 4.10+."
fi

# vendored iroh-blobs must be present (Cargo.toml [patch.crates-io])
[ -f "$ROOT/vendor/iroh-blobs/Cargo.toml" ] || fail "vendor/iroh-blobs missing — the [patch.crates-io] won't resolve. Check out the full repo."

# a display to render into
if [ -z "${WAYLAND_DISPLAY:-}" ] && [ -z "${DISPLAY:-}" ]; then
  fail "No display found (WAYLAND_DISPLAY/DISPLAY unset). Under WSL ensure WSLg is present; otherwise run inside a desktop session."
fi

# WSL: no HW GL via EGL -> use the software renderer for a reliable window
if grep -qi microsoft /proc/version 2>/dev/null; then
  export GSK_RENDERER="${GSK_RENDERER:-cairo}"
  warn "WSL detected: software rendering (GSK_RENDERER=$GSK_RENDERER). GPU GL isn't available via EGL in WSL — fine for this app (see docs)."
fi

# ---------------------------------------------------------------------- build ---
cd "$ROOT"
if [ "$SKIP_BUILD" -eq 0 ]; then
  step "Building (cargo build --release -p seed-daemon -p seed-gui -p seed-cli)"
  cargo build --release -p seed-daemon -p seed-gui -p seed-cli
fi
for b in seed-daemon seed-gui; do
  [ -x "target/release/$b" ] || fail "target/release/$b missing — build failed or use without --skip-build."
done

# ------------------------------------------------------------------------- run ---
DATA="$(mktemp -d)"
SOCK="$DATA/seed.sock"
cleanup() { kill "${DPID:-}" 2>/dev/null || true; }
trap cleanup EXIT

step "Starting daemon (isolated data dir: $DATA)"
./target/release/seed-daemon run --data-dir "$DATA" --socket "$SOCK" >"$DATA/daemon.log" 2>&1 &
DPID=$!
for _ in $(seq 1 40); do [ -S "$SOCK" ] && break; sleep 0.25; done
[ -S "$SOCK" ] || warn "daemon socket not up yet — launching GUI anyway (see $DATA/daemon.log)."

step "Launching GUI (close the window to exit; daemon stops automatically)"
SEED_SOCKET="$SOCK" ./target/release/seed-gui
