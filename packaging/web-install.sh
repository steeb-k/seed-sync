#!/bin/sh
# S.E.E.D. (Seed Sync) bootstrap — install, update, or remove in one command, on
# Linux OR macOS. This is the canonical script hosted at:
#
#   curl -fsSL https://steeb-k.github.io/seed-install.sh | sh
#
# It detects the OS, selects the matching release asset, unpacks it, and runs the
# bundled `seed-sync --install`. After the first install, manage everything with
# the `seed-sync` command. Interactive when run from a terminal; otherwise defaults
# to install/update. Non-interactive override:
#   ... | sh -s -- install     (or update / remove)
#   SEED_ACTION=install ... | sh
#
# Source of truth: packaging/web-install.sh in seed-sync-gtk. Mirror to
# steeb-k.github.io/seed-install.sh (canonical) and seed-sync-binaries/install.sh.
set -eu

REPO="${SEED_BINARIES_REPO:-steeb-k/seed-sync-binaries}"
API="https://api.github.com/repos/$REPO/releases/latest"

say()  { printf '%s\n' "$*"; }
die()  { printf '%s\n' "seed-sync: error: $*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# --- OS detection: pick the asset regex + the unpacked-tree root marker. --------
OS="$(uname -s)"
case "$OS" in
  Linux)
    ASSET_RE='https://[^"]+linux-x86_64\.tar\.gz'
    ASSET_DESC="linux-x86_64.tar.gz"
    find_root() { find "$1" -maxdepth 2 -type d -name bin -exec dirname {} ';' | head -n1; }
    ;;
  Darwin)
    case "$(uname -m)" in arm64) HOST=arm64 ;; *) HOST=x86_64 ;; esac
    ASSET_RE="https://[^\"]+macos-(universal|$HOST)\\.tar\\.gz"
    ASSET_DESC="macos-$HOST (or -universal) tarball"
    find_root() { find "$1" -maxdepth 2 -type d -name "SEED Sync.app" -exec dirname {} ';' | head -n1; }
    ;;
  *) die "unsupported OS: $OS (Linux and macOS only)" ;;
esac

have curl || die "curl is required"
have tar  || die "tar is required"

# Branding banner: the SEED wordmark with a vertical neon-green->neon-purple
# gradient, the full name centered beneath. Truecolor only on a real terminal
# (and unless NO_COLOR is set); otherwise printed plain.
print_banner() {
  art=' @@@@@@        @@@@@@@@       @@@@@@@@       @@@@@@@
@@@@@@@        @@@@@@@@       @@@@@@@@       @@@@@@@@
!@@            @@!            @@!            @@!  @@@
!@!            !@!            !@!            !@!  @!@
!!@@!!         @!!!:!         @!!!:!         @!@  !@!
 !!@!!!        !!!!!:         !!!!!:         !@!  !!!
     !:!       !!:            !!:            !!:  !!!
    !:!   :!:  :!:       :!:  :!:       :!:  :!:  !:!
:::: ::   :::   :: ::::  :::   :: ::::  :::   :::: ::
:: : :    :::  : :: ::   :::  : :: ::   :::  :: :  :   '
  sub='Secure Environment Exchange Daemon'
  width=$(printf '%s\n' "$art" | awk '{ n=length($0); if (n>m) m=n } END { print m+0 }')
  pad=$(( (width - ${#sub}) / 2 ))
  [ "$pad" -lt 0 ] && pad=0
  indent=$(printf "%${pad}s" '')
  if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    total=$(printf '%s\n' "$art" | wc -l)
    [ "$total" -lt 2 ] && total=2
    i=0
    printf '%s\n' "$art" | while IFS= read -r line; do
      r=$(( 57  + 131 * i / (total - 1) ))
      g=$(( 255 - 236 * i / (total - 1) ))
      b=$(( 20  + 234 * i / (total - 1) ))
      printf '\033[1;38;2;%d;%d;%dm%s\033[0m\n' "$r" "$g" "$b" "$line"
      i=$(( i + 1 ))
    done
    printf '\033[1;38;2;188;19;254m%s%s\033[0m\n\n' "$indent" "$sub"
  else
    printf '%s\n' "$art"
    printf '%s%s\n\n' "$indent" "$sub"
  fi
}
print_banner

INSTALLED=""
if have seed-daemon; then
  INSTALLED="$(seed-daemon --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -n1 || true)"
fi

# Pick the action: CLI arg > $SEED_ACTION > interactive menu > sane default.
ACTION="${1:-${SEED_ACTION:-}}"
if [ -z "$ACTION" ]; then
  if (exec 3</dev/tty) 2>/dev/null; then
    say "S.E.E.D. (Seed Sync)"
    if [ -n "$INSTALLED" ]; then
      say "  Installed: v$INSTALLED"
      say ""
      say "  1) Update to the latest version"
      say "  2) Remove"
      say "  3) Cancel"
      printf "Choose [1]: "
      read ans < /dev/tty || ans=3
      case "${ans:-1}" in 1|"") ACTION=update ;; 2) ACTION=remove ;; *) say "Cancelled."; exit 0 ;; esac
    else
      say "  Not installed."
      say ""
      say "  1) Install"
      say "  2) Cancel"
      printf "Choose [1]: "
      read ans < /dev/tty || ans=2
      case "${ans:-1}" in 1|"") ACTION=install ;; *) say "Cancelled."; exit 0 ;; esac
    fi
  else
    ACTION=install   # piped, no terminal: install (idempotent — also upgrades)
  fi
fi

case "$ACTION" in
  install|update|--install|--update)     ACTION=install_or_update ;;
  remove|uninstall|--remove|--uninstall) ACTION=remove ;;
  cancel) say "Cancelled."; exit 0 ;;
  *) die "unknown action '$ACTION' (use install | update | remove)" ;;
esac

# Update/remove of an existing install: delegate to the installed wrapper, which
# already knows how to download + version-check + swap (no work to duplicate here).
if have seed-sync; then
  case "$ACTION" in
    install_or_update) exec seed-sync --update ;;
    remove)            exec seed-sync --uninstall ;;
  esac
fi
[ "$ACTION" = remove ] && die "S.E.E.D. is not installed"

# First-time install: fetch the latest release tarball and run its wrapper.
URL="$(curl -fsSL "$API" | grep -oE "\"browser_download_url\": *\"$ASSET_RE\"" | sed -E 's/.*"(https[^"]+)".*/\1/' | head -n1)"
[ -n "$URL" ] || die "no $ASSET_DESC in the latest release of $REPO"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
say "Downloading $URL"
curl -fsSL -o "$TMP/pkg.tgz" "$URL" || die "download failed"
tar -xzf "$TMP/pkg.tgz" -C "$TMP" || die "extract failed"
ROOT="$(find_root "$TMP")"
[ -n "$ROOT" ] || die "downloaded archive has an unexpected layout"
"$ROOT/seed-sync" --install
