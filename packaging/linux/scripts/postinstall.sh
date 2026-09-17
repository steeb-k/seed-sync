#!/bin/sh
# SEED Sync native package: post-install. Shared by the .deb (postinst) and the
# .rpm (%post).
#
# SEED Sync's daemon is a per-user `systemd --user` service
# (/usr/lib/systemd/user/seed-daemon.service), not a root system service, so
# this script — which runs as root, with no guaranteed user session attached —
# never calls `systemctl --user` or otherwise reaches into anyone's session.
# It only self-configures the rpm repository on first install (the .deb's are
# conffiles dpkg places itself) and prints how to enable the unit; each account
# that wants SEED Sync running enables it for themselves.
set -e

fresh=0
case "${1:-}" in
  configure)
    # dpkg: $2 is the previously configured version, empty on a first install.
    [ -z "${2:-}" ] && fresh=1
    ;;
  abort-*) exit 0 ;;
  1) fresh=1 ;;          # rpm: first install
  [0-9]*) fresh=0 ;;     # rpm: upgrade (2 or more instances)
  *) exit 0 ;;
esac

# rpm, first install only: add the apps.kznjk.com repository for whichever of
# dnf and zypper this system has, unless one is already configured under that
# name. An upgrade never re-adds it, so deleting it is a lasting opt-out (dpkg
# gives the .deb's conffiles the same behaviour).
REPO_DIR=/usr/share/seed-sync/repo
if [ "$fresh" = 1 ] && [ -d "$REPO_DIR" ]; then
  if [ -d /etc/yum.repos.d ] && [ ! -e /etc/yum.repos.d/kznjk.repo ]; then
    cp "$REPO_DIR/kznjk.repo" /etc/yum.repos.d/kznjk.repo
  fi
  if [ -d /etc/zypp/repos.d ] && [ ! -e /etc/zypp/repos.d/kznjk.repo ]; then
    cp "$REPO_DIR/kznjk-zypper.repo" /etc/zypp/repos.d/kznjk.repo
  fi
fi

cat >&2 <<'EOF'
seed-sync: SEED Sync runs as a per-user service; it does not start on its own.
seed-sync: sign in as each account that should run it and enable it there:
seed-sync:     systemctl --user daemon-reload
seed-sync:     systemctl --user enable --now seed-daemon
seed-sync: then launch "S.E.E.D." from the app menu, or run: seed-gui
EOF

exit 0
