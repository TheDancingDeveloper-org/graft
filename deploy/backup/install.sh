#!/usr/bin/env bash
# Install the graft-dev backup timer (WI-2307). Run once on the VM with sudo,
# from the checkout the units should point at:
#   sudo deploy/backup/install.sh
# Re-running it updates the units in place.
set -euo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
deploy=$(cd "$here/.." && pwd)
[ "$(id -u)" = 0 ] || { echo "install.sh: run with sudo" >&2; exit 1; }
sed "s|@DEPLOY@|$deploy|" "$here/graft-dev-backup.service" \
  > /etc/systemd/system/graft-dev-backup.service
install -m 0644 "$here/graft-dev-backup.timer" /etc/systemd/system/graft-dev-backup.timer
install -d -m 0750 /var/backups/graft-dev
systemctl daemon-reload
systemctl enable --now graft-dev-backup.timer
systemctl list-timers graft-dev-backup.timer --no-pager
