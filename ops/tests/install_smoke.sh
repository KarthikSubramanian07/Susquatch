#!/usr/bin/env bash
# Smoke-test ops/install.sh and ops/cloud-init.yaml in a throwaway Ubuntu 24.04 container (native arch).
#
#   ops/tests/install_smoke.sh      (needs docker; ~5 min, mostly apt)
#
# Real: apt packages, Docker's apt repo + compose/buildx plugins, users/groups, .env creation + upserts,
# unit templating, swap/ufw/sysctl *commands*, idempotency (runs twice), and `cloud-init schema`.
# Shimmed (a container has no systemd/kernel): systemctl, ufw, timedatectl, sysctl, swapon, mkswap, fallocate.
set -euo pipefail
REPO="$(cd "$(dirname "$0")/../.." && pwd)"

docker run --rm -i -v "$REPO:/src:ro" ubuntu:24.04 bash -s <<'IN_CONTAINER'
set -euo pipefail
for tool in systemctl ufw timedatectl sysctl swapon mkswap fallocate; do
  printf '#!/bin/sh\necho "[shim] %s $*" >> /var/log/shims.log\n' "$tool" > "/usr/local/sbin/$tool"
  chmod +x "/usr/local/sbin/$tool"
done
apt-get update -qq && apt-get install -y -qq git >/dev/null
id ubuntu >/dev/null 2>&1 || useradd -m ubuntu
# A checkout-shaped copy (the worktree's .git is a pointer file, so make a real repo).
mkdir -p /opt/susquatch
tar -C /src --exclude=./.git --exclude=./.venv --exclude=./target --exclude=./build --exclude='*/node_modules' \
  --exclude=./journal --exclude=./state --exclude=./data --exclude=./reports --exclude=./ops/state -cf - . \
  | tar -C /opt/susquatch -xf -
git -C /opt/susquatch init -q && git -C /opt/susquatch add -A >/dev/null && \
  git -C /opt/susquatch -c user.email=t@t -c user.name=t commit -qm snapshot
rm -f /opt/susquatch/.env

for run in 1 2; do
  echo "── install run $run"
  SUSQ_HOME=/opt/susquatch SUSQ_NO_START=1 /opt/susquatch/ops/install.sh > "/tmp/install-$run.log" 2>&1 \
    || { tail -40 "/tmp/install-$run.log"; exit 1; }
done

ok() { printf '  ok   %s\n' "$1"; }
bad() { printf '  FAIL %s\n' "$1"; exit 1; }
check() { local what="$1"; shift; if "$@"; then ok "$what"; else bad "$what"; fi; }

check "docker + compose + buildx installed" sh -c '{ docker --version && docker compose version && docker buildx version; } >/dev/null'
check "rclone, zstd, jq, chrony installed" sh -c 'command -v rclone zstd jq >/dev/null && dpkg -s chrony >/dev/null'
check "ubuntu in docker group" sh -c 'id -nG ubuntu | grep -qw docker'
check ".env created 0600" [ "$(stat -c %a /opt/susquatch/.env)" = 600 ]
check ".env owned by ubuntu" [ "$(stat -c %U /opt/susquatch/.env)" = ubuntu ]
check "SUSQ_UID upserted once" [ "$(grep -c '^SUSQ_UID=' /opt/susquatch/.env)" = 1 ]
check "DOCKER_GID upserted" grep -q '^DOCKER_GID=[0-9]' /opt/susquatch/.env
check "units templated (no @ placeholders left)" sh -c '! grep -l "@SUSQ_" /etc/systemd/system/susq*.s* /etc/systemd/system/susquatch.service'
check "stack unit points at the checkout" grep -q 'project-directory /opt/susquatch ' /etc/systemd/system/susquatch.service
check "timers enabled" grep -q 'enable susquatch.service susq-backup.timer' /var/log/shims.log
check "ufw deny-by-default + SSH" sh -c 'grep -q "ufw default deny incoming" /var/log/shims.log && grep -q "ufw allow OpenSSH" /var/log/shims.log'
check "no auto-reboot" grep -q '"false"' /etc/apt/apt.conf.d/52susquatch-no-reboot
check "docker daemon.json live-restore" grep -q live-restore /etc/docker/daemon.json
check "runtime dirs owned by ubuntu" [ "$(stat -c %U /opt/susquatch/ops/state/actions)" = ubuntu ]
check "fstab swap line added once" [ "$(grep -c '^/swapfile ' /etc/fstab)" = 1 ]

apt-get install -y -qq cloud-init systemd >/dev/null 2>&1
# The shim shadows systemctl; systemd-analyze is the real one and checks unit syntax + ExecStart paths.
(cd /etc/systemd/system && systemd-analyze verify susquatch.service susq-*.service susq-*.timer > /tmp/verify.log 2>&1 || true)
cat /tmp/verify.log
check "systemd units parse (no unknown keys / bad values)" \
  sh -c '! grep -Ei "unknown key|invalid|failed to parse|bad unit file setting|not executable|syntax" /tmp/verify.log'
check "systemd timers parse" sh -c 'systemd-analyze calendar "2026-11-03 23:00:00 UTC" "*-*-* 07:30:00 UTC" >/dev/null'
check "cloud-init.yaml passes cloud-init schema" sh -c 'cloud-init schema -c /opt/susquatch/ops/cloud-init.yaml >/dev/null'
echo "install smoke passed"
IN_CONTAINER
