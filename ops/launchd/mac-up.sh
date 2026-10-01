#!/usr/bin/env bash
# Mac fallback host: make sure colima (arm64 VM, same aarch64 images as the Azure server) is running, then bring the
# compose stack up. launchd re-runs this on failure (KeepAlive SuccessfulExit=false).
set -euo pipefail
# shellcheck source=ops/lib.sh
. "$(dirname "$0")/../lib.sh"
# This script lives one level below ops/, so point susq_init at the checkout explicitly.
SUSQ_HOME="${SUSQ_HOME:-$(cd "$(dirname "$0")/../.." && pwd)}"
susq_init

# Sizing: the whole stack fits in 2 CPU / 6 GB (the Azure host is 2 vCPU / 4 GB; the stack measures ~400 MB).
if ! colima status >/dev/null 2>&1; then
  log "starting colima"
  colima start --cpu "${SUSQ_COLIMA_CPU:-2}" --memory "${SUSQ_COLIMA_MEM_GB:-6}" --arch aarch64
fi
docker info >/dev/null 2>&1 || die "docker not reachable through colima yet"

# The socket's group inside the colima VM, so the watchdog's socket proxy can read it.
DOCKER_GID="$(colima ssh -- stat -c %g /var/run/docker.sock 2>/dev/null || echo 0)"
export DOCKER_GID SUSQ_UID="${SUSQ_UID:-$(id -u)}" SUSQ_GID="${SUSQ_GID:-$(id -g)}"
for d in journal state data reports targets ops/state/actions; do mkdir -p "$SUSQ_HOME/$d"; done
# The bridge mounts .env read-only; make sure there is one (fill it in afterwards).
[ -f "$SUSQ_HOME/.env" ] || install -m 600 "$SUSQ_HOME/.env.example" "$SUSQ_HOME/.env"

compose up -d --build --remove-orphans
"$SUSQ_HOME/ops/priority.sh" auto || true
log "stack up on the Mac fallback host"
