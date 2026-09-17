#!/bin/sh
# SEED Sync native package: post-remove. Shared by the .deb (postrm) and the
# .rpm (%postun).
#
# Nothing to reload here: seed-daemon.service lives under
# /usr/lib/systemd/user, which the *system* manager never loads, so there is
# no system-level `systemctl daemon-reload` this scriptlet could usefully run.
# Each user's own `systemctl --user daemon-reload` (or their next login) picks
# up the removal.
set -e
exit 0
