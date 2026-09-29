#!/usr/bin/env bash
# Election-night CPU priority on the shared 2-OCPU host (Nov 3 18:00 ET -> Nov 4 12:00 ET).
#
#   ops/priority.sh election   # lower non-critical work (ours and the co-tenant's)
#   ops/priority.sh normal     # put every weight back exactly as recorded
#   ops/priority.sh auto       # election inside [ops].priority_window_*, else normal (run on every stack start)
#   ops/priority.sh status
#
# Mechanism: cgroup v2 CPU *weights* only (docker cpu-shares, systemd CPUWeight). Weights don't cap anything
# while the CPU is idle. Under contention the trading path (rust-live/bridge 4096, needle/nats 3072) gets
# ~30x the share of a throttled job (128). No hard caps, nothing stopped, all changes reverted by `normal`.
#
# The co-tenant (calshi) is another repo that we can't edit, so it's throttled from the outside:
#   SUSQ_COTENANT_PROJECT=calshi                       its compose project name
#   SUSQ_COTENANT_THROTTLE='backfill|research|batch'   regex of its NON-critical compose services
#   SUSQ_COTENANT_UNITS='calshi-backfill.service'      its non-critical systemd units, if any (needs root)
# Its order-handling services are deliberately left alone: throttling a trader is how you strand orders.
set -euo pipefail
# shellcheck source=ops/lib.sh
. "$(dirname "$0")/lib.sh"
susq_init

MODE="${1:-status}"
LOW=128
PROJECT="${SUSQ_COTENANT_PROJECT:-calshi}"
THROTTLE="${SUSQ_COTENANT_THROTTLE:-backfill|research|batch|scheduler}"
UNITS="${SUSQ_COTENANT_UNITS:-}"
RECORD="$OPS_STATE/priority.saved"   # "<container-id> <original cpu-shares>" per line

targets() {  # container ids to throttle: our scheduler + the co-tenant's non-critical services
  docker ps -q --filter label=com.docker.compose.project=susquatch --filter label=com.docker.compose.service=python-scheduler
  docker ps --filter "label=com.docker.compose.project=$PROJECT" \
    --format '{{.ID}} {{.Label "com.docker.compose.service"}}' | awk -v re="$THROTTLE" '$2 ~ re {print $1}'
}

election() {
  local id shares
  touch "$RECORD"
  for id in $(targets); do
    grep -q "^$id " "$RECORD" && continue       # already throttled; keep the true original
    shares="$(docker inspect -f '{{.HostConfig.CpuShares}}' "$id")"
    [ "$shares" = 0 ] && shares=1024             # 0 means "default" (1024)
    echo "$id $shares" >> "$RECORD"
    docker update --cpu-shares "$LOW" "$id" >/dev/null
    log "throttled $(docker inspect -f '{{.Name}}' "$id") $shares -> $LOW"
  done
  for unit in $UNITS; do
    systemctl set-property --runtime "$unit" CPUWeight=10 && log "throttled unit $unit"
  done
  ops_event "election-night CPU priority ON"
}

normal() {
  local id shares
  if [ -f "$RECORD" ]; then
    while read -r id shares; do
      [ -n "$id" ] || continue
      if docker update --cpu-shares "$shares" "$id" >/dev/null 2>&1; then
        log "restored $id -> $shares"
      else
        log "container $id is gone (recreated containers already have their defaults)"
      fi
    done < "$RECORD"
    rm -f "$RECORD"
    ops_event "election-night CPU priority OFF"
  fi
  for unit in $UNITS; do
    systemctl set-property --runtime "$unit" CPUWeight=100 && log "restored unit $unit"
  done
}

case "$MODE" in
  election) election ;;
  normal) normal ;;
  auto) if in_priority_window; then election; else normal; fi ;;
  status)
    docker ps --format '{{.Names}}' | while read -r n; do
      printf '%-40s %s\n' "$n" "$(docker inspect -f '{{.HostConfig.CpuShares}}' "$n")"
    done
    if in_priority_window; then echo "window: INSIDE election-night priority window"; else echo "window: outside"; fi ;;
  *) die "usage: $0 election|normal|auto|status" ;;
esac
