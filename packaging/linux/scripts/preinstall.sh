#!/bin/sh
# SEED Sync native package: pre-install. Shared by the .deb (preinst) and the
# .rpm (%pre); dpkg passes a word ("install", "upgrade", ...), rpm a count.
#
# Unlike Nullgate (a root system service), SEED Sync's daemon is a per-user
# `systemd --user` service under a per-user data dir (.local/share/seedsync in
# the account's home). A root-run scriptlet has no reliable way to see every
# desktop account's own tarball install (.local/bin/seed-daemon in that home,
# ahead of /usr/bin on most desktops' PATH), so there is nothing safe to refuse
# here. The conflict is
# instead caught client-side: `seed-sync --update`/`--install`/`--uninstall`
# refuse once /usr/bin/seed-daemon is owned by a package manager (see
# packaging/linux/seed-sync's refuse_if_pkg_managed), and docs/linux-packaging.md
# tells anyone switching from the tarball to run `seed-sync --uninstall` first.
set -e

case "${1:-}" in
  abort-upgrade) exit 0 ;;
esac

exit 0
