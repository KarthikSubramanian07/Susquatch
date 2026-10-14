#!/usr/bin/env bash
# One command: a fresh server -> fully restored from the latest backup in the bucket, stack running.
#
#   ops/restore.sh                        # newest backup (daily or weekly), then `compose up`
#   ops/restore.sh --archive susq-20261102T073000Z.tar.zst
#   ops/restore.sh --list                 # show what's available
#   ops/restore.sh --no-start             # restore files only
#   ops/restore.sh --yes                  # don't ask before moving existing data aside
#
# Prerequisite: ops/install.sh has run (repo cloned, .env filled in). That's the whole drill:
#   cloud-init (or install.sh) -> fill .env -> ops/restore.sh      target: < 30 min end to end.
#
# Nothing is deleted. Existing state/ journal/ data/ reports/ targets/ are moved to .pre-restore-<UTC>/.
# The archive's sha256 is verified before anything is touched.
# Live trading stays OFF after a restore: ops/GO_LIVE is never backed up, so the stack comes back in paper
# mode until a human re-arms it. The processes reconcile orders and positions from the venue on start.
set -euo pipefail
# shellcheck source=ops/lib.sh
. "$(dirname "$0")/lib.sh"
susq_init

ARCHIVE="" START=1 YES=0 LIST=0
while [ $# -gt 0 ]; do
  case "$1" in
    --archive) ARCHIVE="$2"; shift 2 ;;
    --no-start) START=0; shift ;;
    --yes|-y) YES=1; shift ;;
    --list) LIST=1; shift ;;
    -h|--help) sed -n '2,19p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

T0="$(date +%s)"
backup_remote_init

listing() {  # "<dir>/<name>" for every archive, newest first (names sort by UTC timestamp)
  local dir
  for dir in daily weekly; do
    rclone lsf "$BACKUP_PREFIX/$dir/" --include 'susq-*.tar.zst' 2>/dev/null | sed "s|^|$dir/|"
  done | sort -t/ -k2 -r
}

if [ "$LIST" = 1 ]; then listing; exit 0; fi

if [ -n "$ARCHIVE" ]; then
  OBJ="$(listing | grep -F "/$ARCHIVE" | head -n1 || true)"
else
  OBJ="$(listing | head -n1 || true)"
fi
[ -n "$OBJ" ] || die "no backup found under $BACKUP_PREFIX (try --list)"
NAME="${OBJ#*/}"
log "restoring $OBJ"

STAGING="$OPS_STATE/restore-staging"
mkdir -p "$STAGING"
rclone copyto "$BACKUP_PREFIX/$OBJ" "$STAGING/$NAME"
rclone copyto "$BACKUP_PREFIX/$OBJ.sha256" "$STAGING/$NAME.sha256"
WANT="$(cut -d' ' -f1 < "$STAGING/$NAME.sha256")"
GOT="$(sha256_of "$STAGING/$NAME")"
[ "$WANT" = "$GOT" ] || die "checksum mismatch for $NAME (want $WANT, got $GOT); nothing changed"
log "checksum ok ($GOT)"

EXISTING=""
for d in state journal data reports targets; do
  if [ -d "$SUSQ_HOME/$d" ] && [ -n "$(ls -A "$SUSQ_HOME/$d" 2>/dev/null)" ]; then EXISTING="$EXISTING $d"; fi
done
if [ -n "$EXISTING" ] && [ "$YES" != 1 ]; then
  printf 'Existing data in:%s\nIt will be moved aside (not deleted). Continue? [y/N] ' "$EXISTING" >&2
  read -r reply
  case "$reply" in y|Y|yes) ;; *) die "aborted; nothing changed" ;; esac
fi

if [ -f "$SUSQ_HOME/ops/compose.yaml" ] && command -v docker >/dev/null 2>&1; then
  log "stopping the stack"
  compose down --remove-orphans >/dev/null 2>&1 || true
fi

if [ -n "$EXISTING" ]; then
  ASIDE="$SUSQ_HOME/.pre-restore-$(date -u +%Y%m%dT%H%M%SZ)"
  mkdir -p "$ASIDE"
  for d in $EXISTING; do mv "$SUSQ_HOME/$d" "$ASIDE/$d"; done
  log "previous data moved to $ASIDE"
fi

# The append-only bulk (data/, compressed journal days) lives in <prefix>/files/ since the incremental backups;
# archives from before that carry it inside the tar and have no files tree to fetch.
if rclone lsf "$BACKUP_PREFIX/files/" >/dev/null 2>&1 && [ -n "$(rclone lsf "$BACKUP_PREFIX/files/" 2>/dev/null)" ]; then
  log "fetching the files tree"
  rclone copy "$BACKUP_PREFIX/files" "$SUSQ_HOME" --size-only
fi
zstd -q -dc "$STAGING/$NAME" | tar -C "$SUSQ_HOME" -xf -
rm -f "$STAGING/$NAME" "$STAGING/$NAME.sha256"
for d in state journal data reports targets ops/state; do mkdir -p "$SUSQ_HOME/$d"; done
ops_event "restored from backup $NAME"
printf '{"restored_from": "%s", "restored_at_utc": "%s"}\n' "$NAME" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  > "$OPS_STATE/restore.json"

if [ "$START" = 1 ] && command -v docker >/dev/null 2>&1; then
  log "starting the stack (first start builds images natively; allow ~10-15 min on a 2-vCPU server)"
  compose up -d --build --remove-orphans
fi

ELAPSED=$(( $(date +%s) - T0 ))
cat >&2 <<EOF

  restored $NAME in $((ELAPSED / 60))m$((ELAPSED % 60))s

  next:
    make logs                    # watch the stack come up and reconcile
    make selfcheck               # disk, clock, lease, backups
    curl the dashboard           # heartbeat should go green within ~3 min
    echo 'go live' > ops/GO_LIVE # ONLY if you were live before (GO_LIVE is never backed up)
EOF
