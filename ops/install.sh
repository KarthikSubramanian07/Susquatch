#!/usr/bin/env bash
# Fresh Ubuntu 24.04 aarch64 (Oracle Always Free A1) -> a Susquatch host. Idempotent: safe to re-run.
#
#   sudo ops/install.sh                   # from a checkout (it installs itself where it is)
#   sudo SUSQ_REPO_URL=... ops/install.sh # or let it clone into $SUSQ_HOME (cloud-init does this)
#
# What it does, in order:
#   packages (git, zstd, jq, rclone, python3), Docker Engine + compose/buildx plugins from Docker's repo,
#   chrony (clock), ufw (deny inbound except SSH), unattended security upgrades WITHOUT auto-reboot,
#   a swapfile (native Rust builds on 12 GB), the checkout + .env placeholder (0600), and the systemd units:
#   susquatch.service (the stack), nightly backup, daily self-check, election-night priority on/off.
#
# Inputs (env): SUSQ_HOME=/opt/susquatch  SUSQ_USER=ubuntu  SUSQ_REPO_URL  SUSQ_BRANCH=main
#   Private repo access: a read-only deploy key at /root/.ssh/susq_deploy_key (cloud-init writes it),
#   or SUSQ_GIT_TOKEN (fine-grained, contents:read) for HTTPS. Neither is ever written into the repo.
set -euo pipefail

[ "$(id -u)" = 0 ] || { echo "run as root (sudo)" >&2; exit 1; }
HERE="$(cd "$(dirname "$0")/.." && pwd)"
if [ -d "$HERE/.git" ] && [ -z "${SUSQ_HOME:-}" ]; then SUSQ_HOME="$HERE"; fi
SUSQ_HOME="${SUSQ_HOME:-/opt/susquatch}"
SUSQ_USER="${SUSQ_USER:-ubuntu}"
SUSQ_BRANCH="${SUSQ_BRANCH:-main}"
SUSQ_REPO_URL="${SUSQ_REPO_URL:-git@github.com:KarthikSubramanian07/Susquatch.git}"
export DEBIAN_FRONTEND=noninteractive
step() { printf '\n== %s\n' "$*"; }

step "base packages"
apt-get update -q
apt-get install -y -q ca-certificates curl git jq zstd rclone python3 chrony ufw unattended-upgrades

step "docker engine (official repo, arm64)"
if ! command -v docker >/dev/null || ! docker compose version >/dev/null 2>&1; then
  install -m 0755 -d /etc/apt/keyrings
  curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
  chmod a+r /etc/apt/keyrings/docker.asc
  # shellcheck disable=SC1091
  . /etc/os-release
  echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu ${VERSION_CODENAME} stable" \
    > /etc/apt/sources.list.d/docker.list
  apt-get update -q
  apt-get install -y -q docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
fi
# live-restore: containers keep running while dockerd restarts (unattended upgrades patch it).
# Log rotation for anything not using our per-service limits.
if [ ! -f /etc/docker/daemon.json ]; then
  printf '%s\n' '{' '  "live-restore": true,' '  "log-driver": "json-file",' \
    '  "log-opts": { "max-size": "10m", "max-file": "3" }' '}' > /etc/docker/daemon.json
  systemctl restart docker
fi
systemctl enable --now docker
id -nG "$SUSQ_USER" | grep -qw docker || usermod -aG docker "$SUSQ_USER"

step "clock (chrony)"
systemctl enable --now chrony
timedatectl set-timezone UTC

step "firewall (ufw: deny inbound except SSH)"
# Note: Docker-published ports bypass ufw. The compose file publishes only 127.0.0.1 metrics ports.
ufw default deny incoming >/dev/null
ufw default allow outgoing >/dev/null
ufw allow OpenSSH >/dev/null
ufw --force enable >/dev/null
ufw status verbose

step "unattended security upgrades (no automatic reboot: never mid-election-night)"
printf '%s\n' 'APT::Periodic::Update-Package-Lists "1";' 'APT::Periodic::Unattended-Upgrade "1";' \
  > /etc/apt/apt.conf.d/20auto-upgrades
printf '%s\n' 'Unattended-Upgrade::Automatic-Reboot "false";' > /etc/apt/apt.conf.d/52susquatch-no-reboot
systemctl enable --now unattended-upgrades

step "swap (4 GB; native Rust/C++ builds on 12 GB RAM)"
if ! swapon --show | grep -q /swapfile; then
  [ -f /swapfile ] || { fallocate -l 4G /swapfile && chmod 600 /swapfile && mkswap /swapfile >/dev/null; }
  swapon /swapfile
  grep -q '^/swapfile ' /etc/fstab || echo '/swapfile none swap sw 0 0' >> /etc/fstab
fi
sysctl -q vm.swappiness=10
echo 'vm.swappiness=10' > /etc/sysctl.d/60-susquatch.conf

step "checkout at $SUSQ_HOME"
if [ ! -d "$SUSQ_HOME/.git" ]; then
  mkdir -p "$(dirname "$SUSQ_HOME")"
  if [ -f /root/.ssh/susq_deploy_key ]; then
    GIT_SSH_COMMAND="ssh -i /root/.ssh/susq_deploy_key -o StrictHostKeyChecking=accept-new" \
      git clone --branch "$SUSQ_BRANCH" "$SUSQ_REPO_URL" "$SUSQ_HOME"
  elif [ -n "${SUSQ_GIT_TOKEN:-}" ]; then
    https_url="https://github.com/${SUSQ_REPO_URL#git@github.com:}"
    # Same scheme actions/checkout uses. Passed with -c, so the token is never persisted to .git/config.
    basic="$(printf 'x-access-token:%s' "$SUSQ_GIT_TOKEN" | base64 -w0)"
    git -c "http.extraHeader=Authorization: Basic ${basic}" clone --branch "$SUSQ_BRANCH" "$https_url" "$SUSQ_HOME"
  else
    echo "no deploy key (/root/.ssh/susq_deploy_key) and no SUSQ_GIT_TOKEN: can't clone a private repo" >&2
    exit 1
  fi
fi
git config --global --get-all safe.directory | grep -qx "$SUSQ_HOME" \
  || git config --global --add safe.directory "$SUSQ_HOME"   # root touching a user-owned checkout
# Whoever cloned (us or cloud-init), later `git pull`s run as $SUSQ_USER with the same read-only key.
if [ -f /root/.ssh/susq_deploy_key ]; then
  install -d -m 700 -o "$SUSQ_USER" -g "$SUSQ_USER" "/home/$SUSQ_USER/.ssh"
  install -m 600 -o "$SUSQ_USER" -g "$SUSQ_USER" /root/.ssh/susq_deploy_key "/home/$SUSQ_USER/.ssh/susq_deploy_key"
  git -C "$SUSQ_HOME" config core.sshCommand \
    "ssh -i /home/$SUSQ_USER/.ssh/susq_deploy_key -o StrictHostKeyChecking=accept-new"
fi
chown -R "$SUSQ_USER:$SUSQ_USER" "$SUSQ_HOME"

step ".env (secrets stay here, never in git)"
ENV_FILE="$SUSQ_HOME/.env"
if [ ! -f "$ENV_FILE" ]; then
  install -m 600 -o "$SUSQ_USER" -g "$SUSQ_USER" "$SUSQ_HOME/.env.example" "$ENV_FILE"
  echo "created $ENV_FILE from .env.example: fill it in (docs/OPS.md, Secrets)"
fi
upsert() {  # upsert KEY VALUE: set a machine-derived value, never touching human-entered ones
  if grep -q "^$1=" "$ENV_FILE"; then sed -i "s|^$1=.*|$1=$2|" "$ENV_FILE"; else echo "$1=$2" >> "$ENV_FILE"; fi
}
upsert SUSQ_UID "$(id -u "$SUSQ_USER")"
upsert SUSQ_GID "$(id -g "$SUSQ_USER")"
upsert DOCKER_GID "$(getent group docker | cut -d: -f3)"
chmod 600 "$ENV_FILE"

step "runtime directories"
for d in journal state data reports targets ops/state/actions; do
  install -d -o "$SUSQ_USER" -g "$SUSQ_USER" "$SUSQ_HOME/$d"
done

step "systemd units"
for unit in "$SUSQ_HOME"/ops/systemd/*.service "$SUSQ_HOME"/ops/systemd/*.timer; do
  sed -e "s|@SUSQ_HOME@|$SUSQ_HOME|g" -e "s|@SUSQ_USER@|$SUSQ_USER|g" "$unit" > "/etc/systemd/system/$(basename "$unit")"
done
systemctl daemon-reload
systemctl enable susquatch.service susq-backup.timer susq-selfcheck.timer susq-priority-on.timer susq-priority-off.timer
systemctl start susq-backup.timer susq-selfcheck.timer susq-priority-on.timer susq-priority-off.timer

step "start the stack (paper mode; live needs SUSQ_MODE=live AND ops/GO_LIVE)"
if [ "${SUSQ_NO_START:-0}" = 1 ]; then
  echo "SUSQ_NO_START=1: not starting. Later: sudo systemctl start susquatch"
else
  systemctl restart susquatch.service
fi

cat <<EOF

Susquatch host ready.
  1. Fill in $ENV_FILE (docs/OPS.md, Secrets), then: sudo systemctl restart susquatch
  2. Restoring from a backup? $SUSQ_HOME/ops/restore.sh
  3. make -C $SUSQ_HOME selfcheck ; make -C $SUSQ_HOME costs
EOF
