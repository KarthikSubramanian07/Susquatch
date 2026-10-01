#!/usr/bin/env bash
# Install (or remove) the Mac fallback-host launch agents. Switching hosts is this one command.
#
#   ops/launchd/install.sh            # load: stack (at login, kept alive), nightly backup, daily self-check
#   ops/launchd/install.sh --remove   # unload and delete them (e.g. once the Azure server is back)
#
# Only ever run ONE host at a time: two hosts pushing status and holding leases against the same venue
# account is exactly the split-brain the lease can't see across machines. Stop the other one first:
#   Azure -> Mac:  ssh server 'sudo systemctl stop susquatch'  then this script
#   Mac -> Azure:  this script --remove; docker compose ... down; then start the server
# Requires: brew install colima docker docker-compose docker-buildx rclone zstd (and `uv` for selfcheck).
set -euo pipefail
SUSQ_HOME="${SUSQ_HOME:-$(cd "$(dirname "$0")/../.." && pwd)}"
AGENTS="$HOME/Library/LaunchAgents"
LOG_DIR="$HOME/Library/Logs"
LABELS="com.susquatch.stack com.susquatch.backup com.susquatch.selfcheck"

for label in $LABELS; do
  launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
  rm -f "$AGENTS/$label.plist"
done
if [ "${1:-}" = "--remove" ]; then echo "launch agents removed"; exit 0; fi

mkdir -p "$AGENTS" "$LOG_DIR"
for label in $LABELS; do
  sed -e "s|__SUSQ_HOME__|$SUSQ_HOME|g" -e "s|__LOG_DIR__|$LOG_DIR|g" -e "s|__HOME__|$HOME|g" \
    "$SUSQ_HOME/ops/launchd/$label.plist" > "$AGENTS/$label.plist"
  plutil -lint "$AGENTS/$label.plist" >/dev/null
  launchctl bootstrap "gui/$(id -u)" "$AGENTS/$label.plist"
  echo "loaded $label"
done
echo "logs: $LOG_DIR/susquatch-*.log. Keep the Mac awake on election night (System Settings -> Battery)."
