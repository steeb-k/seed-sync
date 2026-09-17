#!/bin/sh
# SEED Sync native package: pre-remove. Shared by the .deb (prerm) and the .rpm
# (%preun). Only acts on a real removal, never on an upgrade.
#
# Because seed-daemon.service is a per-user unit, there is no single system
# session for a root scriptlet to stop or disable it in — the same reasoning
# as postinstall.sh. Removal warns and leaves stopping the service, per
# account, to the user.
set -e

case "${1:-}" in
  remove|0) ;;            # dpkg removal / rpm erase
  *) exit 0 ;;             # upgrade, deconfigure, failed-upgrade, rpm count >= 1
esac

cat >&2 <<'EOF'
seed-sync: this does not stop a running per-user seed-daemon service.
seed-sync: if SEED Sync is running for your account, stop it yourself:
seed-sync:     systemctl --user disable --now seed-daemon
EOF

# rpm erase: take back the repository definitions postinstall added, unless
# they were edited. (The .deb's are conffiles: kept on remove, deleted on
# purge.) `cmp` is not guaranteed present (minimal openSUSE images), so compare
# contents with a plain read.
REPO_DIR=/usr/share/seed-sync/repo
same() { [ -f "$1" ] && [ -f "$2" ] && [ "$(cat "$1")" = "$(cat "$2")" ]; }
if [ "${1:-}" = 0 ] && [ -d "$REPO_DIR" ]; then
  if same "$REPO_DIR/kznjk.repo" /etc/yum.repos.d/kznjk.repo; then rm -f /etc/yum.repos.d/kznjk.repo; fi
  if same "$REPO_DIR/kznjk-zypper.repo" /etc/zypp/repos.d/kznjk.repo; then rm -f /etc/zypp/repos.d/kznjk.repo; fi
fi

exit 0
