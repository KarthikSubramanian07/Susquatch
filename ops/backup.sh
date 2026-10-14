#!/usr/bin/env bash
# Nightly backup of state/ journal/ data/ reports/ targets/ -> the S3 backup bucket
# (Backblaze B2, 10 GB free and no card; Cloudflare R2 also works, see backup_remote_init in ops/lib.sh).
#
#   ops/backup.sh            # run a backup now (what the nightly timer runs; also `make backup`)
#   ops/backup.sh --dry-run  # build + measure the archive, upload nothing
#   ops/backup.sh --force    # run even inside the election-night priority window (normally deferred)
#
# Layout in the bucket:   <prefix>/daily/susq-<UTC>.tar.zst (+ .sha256)   newest backup_keep_daily kept
#                         <prefix>/weekly/susq-<UTC>.tar.zst              Sunday copies, backup_keep_weekly kept
#                         <prefix>/files/{data,journal}/...                the append-only bulk, one object per file
# An archive (tar + zstd) holds state/, reports/, targets/ and the journal days not yet compressed. The bulk only
# grows, so it is copied file by file and never re-uploaded; a restore is the files tree plus one archive. Full
# daily snapshots reached 1.7 GB by Oct 13 and four of them filled the free tier.
#
# Quota guard: the B2 free tier is 10 GB. If (bucket bytes + new files + this archive) would reach r2_guard_frac (80%) of
# r2_quota_bytes, the upload is REFUSED, the archive is kept locally, and an ACTION NEEDED item is filed.
# If the bucket can't be measured, the upload is refused too: never spend money on a guess.
#
# Secrets never enter the archive: .env and ops/ are not backed up, and ops/GO_LIVE (human only) never is.
set -euo pipefail
# shellcheck source=ops/lib.sh
. "$(dirname "$0")/lib.sh"
susq_init

DRY_RUN=0 FORCE=0
case "${1:-}" in --dry-run) DRY_RUN=1 ;; --force) FORCE=1 ;; esac
# The nightly slot (07:30Z) lands mid-count on election night: defer to the post-lock run (priority-off).
if [ "$FORCE" = 0 ] && in_priority_window; then
  log "inside the election-night priority window: deferred (runs after the lock, or use --force)"; exit 0
fi

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
# The archive holds what changes: state, reports, targets and the journal days still being written. The bulk
# that only grows (data/, and journal days already compressed to .zst) is uploaded once, file by file, to
# <prefix>/files/ (see "files tree" below), so a nightly archive is megabytes rather than a full multi-GB copy.
DIRS=""
for d in state journal reports targets; do
  [ -d "$SUSQ_HOME/$d" ] && DIRS="$DIRS $d"
done
[ -n "$DIRS" ] || die "nothing to back up under $SUSQ_HOME"
mkdir -p "$STAGING"
ARCHIVE="$STAGING/$NAME"
log "archiving:$DIRS"
# The bridge appends to today's journal while we read it; GNU tar reports that as exit 1 ("file changed as we
# read it"), which under pipefail failed every nightly run from Oct 2. A partially-written last line is fine for a
# backup. Exit 1 is accepted; exit 2 (a real error) still fails the run.
TAR_FLAGS=""
if tar --version 2>/dev/null | grep -q GNU; then TAR_FLAGS="--warning=no-file-changed"; fi  # BSD tar: no such flag, no such exit code
# shellcheck disable=SC2086  # word-splitting DIRS and TAR_FLAGS is intended
{ tar -C "$SUSQ_HOME" $TAR_FLAGS --exclude='*.zst' -cf - $DIRS || [ "$?" -eq 1 ]; } | zstd -q -T0 -12 -o "$ARCHIVE" -f
SUM="$(sha256_of "$ARCHIVE")"
printf '%s  %s\n' "$SUM" "$NAME" > "$ARCHIVE.sha256"
BYTES="$(file_bytes "$ARCHIVE")"
log "archive $NAME: $BYTES bytes"
record "last_attempt_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)" "last_archive_bytes=$BYTES"

if [ "$DRY_RUN" = 1 ]; then
  log "dry run: not uploading"; rm -f "$ARCHIVE" "$ARCHIVE.sha256"; exit 0
fi

backup_remote_init

prune() {  # prune <dir> <keep>
  local dir="$1" keep="$2" old
  old="$(rclone lsf "$BACKUP_PREFIX/$dir/" --include 'susq-*.tar.zst' 2>/dev/null | sort -r | tail -n +"$((keep + 1))" || true)"
  for obj in $old; do
    log "prune $dir/$obj"
    rclone deletefile "$BACKUP_PREFIX/$dir/$obj"
    rclone deletefile "$BACKUP_PREFIX/$dir/$obj.sha256" 2>/dev/null || true
  done
  # B2 keeps every deleted object as a hidden old version by default, and those still count against the 10 GB
  # free tier (5.5 GB of them had filled it by Oct 14). Remove old versions for good; current names have no -v.
  rclone delete --s3-versions --include '*-v[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]-*' \
    "$BACKUP_PREFIX/$dir/" 2>/dev/null || true
}
# Make room before measuring: keep one fewer daily than configured, so tonight's makes KEEP_DAILY.
prune daily "$((KEEP_DAILY > 1 ? KEEP_DAILY - 1 : 1))"
prune weekly "$KEEP_WEEKLY"

# Bytes of the files tree not yet in the bucket (local total minus remote total; files are only added).
local_bulk_bytes() {
  local d n total=0
  for d in data journal; do
    [ -d "$SUSQ_HOME/$d" ] || continue
    if [ "$d" = journal ]; then
      n="$(rclone size --json --include '*.zst' "$SUSQ_HOME/$d" 2>/dev/null | sed -n 's/.*"bytes":[ ]*\([0-9][0-9]*\).*/\1/p')"
    else
      n="$(rclone size --json "$SUSQ_HOME/$d" 2>/dev/null | sed -n 's/.*"bytes":[ ]*\([0-9][0-9]*\).*/\1/p')"
    fi
    total=$((total + ${n:-0}))
  done
  echo "$total"
}
remote_files_bytes() {
  rclone size --json "$BACKUP_PREFIX/files" 2>/dev/null | sed -n 's/.*"bytes":[ ]*\([0-9][0-9]*\).*/\1/p'
}

# ── quota guard ──
USED="$(remote_bytes || true)"
if [ -z "$USED" ]; then
  action_raise r2-quota "Backups paused: could not measure the backup bucket (credentials or network). Newest archive kept on the server in ops/state/backup-staging/."
  record "last_result=refused-unmeasured"
  die "could not measure bucket usage; refusing to upload"
fi
LIMIT="$(python3 -c "print(int($QUOTA * $GUARD))")"
REMOTE_FILES="$(remote_files_bytes || true)"
PENDING=$(( $(local_bulk_bytes) - ${REMOTE_FILES:-0} ))
[ "$PENDING" -gt 0 ] || PENDING=0
if [ $((USED + PENDING + BYTES)) -ge "$LIMIT" ]; then
  PCT="$(python3 -c "print(round(100 * ($USED + $PENDING + $BYTES) / $QUOTA))")"
  action_raise r2-quota "Backups paused: the backup bucket would be at ${PCT}% of the 10 GB free tier (guard is $(python3 -c "print(round($GUARD*100))")%). Prune old objects or lower retention in config/susq.toml. Newest archive kept locally."
  record "last_result=refused-quota" "bucket_bytes=$USED"
  ops_event "backup refused: bucket quota guard (${PCT}%)"
  log "REFUSED: $USED + $PENDING (new files) + $BYTES >= $LIMIT"; exit 3
fi

# ── files tree, then upload, promote, prune ──
# Size-only comparison: the bulk is append-only (new parquet partitions, newly compressed journal days), and it
# avoids a metadata request per object. copy, not sync: a file deleted on the server stays in the bucket.
# --s3-no-head skips rclone's HEAD after every upload: on a no-card B2 account those are capped at 2,500 a day,
# and the first run (about 34,000 files) hit the cap and got 403 for everything after it.
if [ -d "$SUSQ_HOME/data" ]; then
  rclone copy "$SUSQ_HOME/data" "$BACKUP_PREFIX/files/data" --size-only --s3-no-head --s3-no-check-bucket
fi
if [ -d "$SUSQ_HOME/journal" ]; then
  rclone copy "$SUSQ_HOME/journal" "$BACKUP_PREFIX/files/journal" --size-only --include '*.zst' \
    --s3-no-head --s3-no-check-bucket
fi
rclone copyto "$ARCHIVE" "$BACKUP_PREFIX/daily/$NAME"
rclone copyto "$ARCHIVE.sha256" "$BACKUP_PREFIX/daily/$NAME.sha256"
if [ "$DOW" = 7 ]; then
  rclone copyto "$BACKUP_PREFIX/daily/$NAME" "$BACKUP_PREFIX/weekly/$NAME"
  rclone copyto "$BACKUP_PREFIX/daily/$NAME.sha256" "$BACKUP_PREFIX/weekly/$NAME.sha256"
fi

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
