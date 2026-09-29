#!/usr/bin/env bash
# Nightly backup: tar + zstd of state/ journal/ data/ reports/ targets/ -> Cloudflare R2 (free tier).
#
#   ops/backup.sh            # run a backup now (what the nightly timer runs; also `make backup`)
#   ops/backup.sh --dry-run  # build + measure the archive, upload nothing
#
# Layout in the bucket:   <prefix>/daily/susq-<UTC>.tar.zst (+ .sha256)   newest backup_keep_daily kept
#                         <prefix>/weekly/susq-<UTC>.tar.zst              Sunday copies, backup_keep_weekly kept
# Every backup is a full snapshot, so there are no deltas to chain and any single object restores alone.
#
# Quota guard: the R2 free tier is 10 GB. If (bucket bytes + this archive) would reach r2_guard_frac (80%) of
# r2_quota_bytes, the upload is REFUSED, the archive is kept locally, and an ACTION NEEDED item is filed.
# If the bucket can't be measured, the upload is refused too: never spend money on a guess.
#
# Secrets never enter the archive: .env and ops/ are not backed up, and ops/GO_LIVE (human only) never is.
set -euo pipefail
# shellcheck source=ops/lib.sh
. "$(dirname "$0")/lib.sh"
susq_init

DRY_RUN=0
[ "${1:-}" = "--dry-run" ] && DRY_RUN=1

QUOTA="${SUSQ_R2_QUOTA_BYTES:-$(toml_get ops r2_quota_bytes 10000000000)}"
GUARD="$(toml_get ops r2_guard_frac 0.80)"
KEEP_DAILY="$(toml_get ops backup_keep_daily 7)"
KEEP_WEEKLY="$(toml_get ops backup_keep_weekly 4)"
TS="${SUSQ_BACKUP_TS:-$(date -u +%Y%m%dT%H%M%SZ)}"
DOW="${SUSQ_BACKUP_DOW:-$(date -u +%u)}"   # 7 = Sunday
NAME="susq-${TS}.tar.zst"
STAGING="$OPS_STATE/backup-staging"
RECORD="$OPS_STATE/backup.json"
LOCK="$OPS_STATE/backup.lock"

# ── single instance (mkdir is atomic everywhere; flock isn't on macOS) ──
if ! mkdir "$LOCK" 2>/dev/null; then
  if [ -n "$(find "$LOCK" -maxdepth 0 -mmin +360 2>/dev/null)" ]; then
    log "removing stale lock (>6h)"; rmdir "$LOCK" && mkdir "$LOCK"
  else
    die "another backup is running ($LOCK)"
  fi
fi
trap 'rmdir "$LOCK" 2>/dev/null || true' EXIT

record() {  # record <json-fields...> : merge into backup.json (python3 is on every host we run on)
  python3 - "$RECORD" "$@" <<'PY'
import json, sys, pathlib
path = pathlib.Path(sys.argv[1])
try:
    rec = json.loads(path.read_text())
except Exception:
    rec = {}
for kv in sys.argv[2:]:
    k, v = kv.split("=", 1)
    rec[k] = int(v) if v.isdigit() else v
tmp = path.with_suffix(".tmp")
tmp.write_text(json.dumps(rec, indent=2) + "\n")
tmp.replace(path)
PY
}

# ── build the archive ──
DIRS=""
for d in state journal data reports targets; do
  [ -d "$SUSQ_HOME/$d" ] && DIRS="$DIRS $d"
done
[ -n "$DIRS" ] || die "nothing to back up under $SUSQ_HOME"
mkdir -p "$STAGING"
ARCHIVE="$STAGING/$NAME"
log "archiving:$DIRS"
# shellcheck disable=SC2086  # word-splitting DIRS is intended
tar -C "$SUSQ_HOME" -cf - $DIRS | zstd -q -T0 -12 -o "$ARCHIVE" -f
SUM="$(sha256_of "$ARCHIVE")"
printf '%s  %s\n' "$SUM" "$NAME" > "$ARCHIVE.sha256"
BYTES="$(file_bytes "$ARCHIVE")"
log "archive $NAME: $BYTES bytes"
record "last_attempt_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)" "last_archive_bytes=$BYTES"

if [ "$DRY_RUN" = 1 ]; then
  log "dry run: not uploading"; rm -f "$ARCHIVE" "$ARCHIVE.sha256"; exit 0
fi

# ── quota guard ──
backup_remote_init
USED="$(remote_bytes || true)"
if [ -z "$USED" ]; then
  action_raise r2-quota "Backups paused: could not measure the R2 bucket (credentials or network). Newest archive kept on the server in ops/state/backup-staging/."
  record "last_result=refused-unmeasured"
  die "could not measure bucket usage; refusing to upload"
fi
LIMIT="$(python3 -c "print(int($QUOTA * $GUARD))")"
if [ $((USED + BYTES)) -ge "$LIMIT" ]; then
  PCT="$(python3 -c "print(round(100 * ($USED + $BYTES) / $QUOTA))")"
  action_raise r2-quota "Backups paused: R2 would be at ${PCT}% of the 10 GB free tier (guard is $(python3 -c "print(round($GUARD*100))")%). Prune old objects or lower retention in config/susq.toml. Newest archive kept locally."
  record "last_result=refused-quota" "bucket_bytes=$USED"
  ops_event "backup refused: R2 quota guard (${PCT}%)"
  log "REFUSED: $USED + $BYTES >= $LIMIT"; exit 3
fi

# ── upload, promote, prune ──
rclone copyto "$ARCHIVE" "$BACKUP_PREFIX/daily/$NAME"
rclone copyto "$ARCHIVE.sha256" "$BACKUP_PREFIX/daily/$NAME.sha256"
if [ "$DOW" = 7 ]; then
  rclone copyto "$BACKUP_PREFIX/daily/$NAME" "$BACKUP_PREFIX/weekly/$NAME"
  rclone copyto "$BACKUP_PREFIX/daily/$NAME.sha256" "$BACKUP_PREFIX/weekly/$NAME.sha256"
fi

prune() {  # prune <dir> <keep>
  local dir="$1" keep="$2" old
  old="$(rclone lsf "$BACKUP_PREFIX/$dir/" --include 'susq-*.tar.zst' 2>/dev/null | sort -r | tail -n +"$((keep + 1))")"
  for obj in $old; do
    log "prune $dir/$obj"
    rclone deletefile "$BACKUP_PREFIX/$dir/$obj"
    rclone deletefile "$BACKUP_PREFIX/$dir/$obj.sha256" 2>/dev/null || true
  done
}
prune daily "$KEEP_DAILY"
prune weekly "$KEEP_WEEKLY"

AFTER="$(remote_bytes || true)"
rm -f "$ARCHIVE" "$ARCHIVE.sha256"
# Older local archives kept by refused runs are no longer needed once one upload succeeds.
find "$STAGING" -name 'susq-*.tar.zst*' -type f -delete 2>/dev/null || true
action_resolve r2-quota
record "last_success_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)" "last_result=ok" "archive=$NAME" \
  "bytes=$BYTES" "bucket_bytes=${AFTER:-$((USED + BYTES))}" "quota_bytes=$QUOTA"
ops_event "backup uploaded: $NAME ($((BYTES / 1000000)) MB)"
log "backup ok: $NAME -> $BACKUP_PREFIX/daily/"
