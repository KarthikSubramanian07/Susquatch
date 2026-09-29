#!/usr/bin/env bash
# Backup/restore drill against a real S3 API: `rclone serve s3` on localhost stands in for R2, and the
# scripts talk to it through the exact same `s3` + `provider=Cloudflare` remote they use in production.
#
#   ops/tests/backup_restore_drill.sh      (needs rclone >= 1.65, zstd, python3)
#
# Covers: upload, retention (7 daily + N weekly), Sunday promotion, the 80% quota guard, refusing when the
# bucket can't be measured, restore into a fresh home, restore over existing data (moved aside), and
# refusing a tampered archive.
set -euo pipefail
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
WORK="$(mktemp -d)"
PORT="${DRILL_PORT:-18$((RANDOM % 900 + 100))}"
SERVER_PID=""
cleanup() { [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null; rm -rf "$WORK"; }
trap cleanup EXIT

# check "<what>" <command...>: run the command; report and stop on the first failure.
check() {
  local what="$1"; shift
  if "$@"; then printf '  ok   %s\n' "$what"; else printf '  FAIL %s\n' "$what"; exit 1; fi
}
count() { find "$1" -maxdepth 1 -name "$2" | wc -l | tr -d ' '; }

# ── fake R2 ──
mkdir -p "$WORK/s3/drill-bucket"
rclone serve s3 "$WORK/s3" --addr "127.0.0.1:$PORT" --auth-key "drillkey,drillsecret" >"$WORK/s3.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 50); do
  if curl -s -o /dev/null "http://127.0.0.1:$PORT/"; then break; fi
  sleep 0.1
done

export SUSQ_R2_ACCESS_KEY_ID=drillkey SUSQ_R2_SECRET_ACCESS_KEY=drillsecret
export SUSQ_R2_ENDPOINT="http://127.0.0.1:$PORT" SUSQ_R2_BUCKET=drill-bucket SUSQ_BACKUP_PREFIX=susquatch
unset SUSQ_BACKUP_REMOTE CLOUDFLARE_ACCOUNT_ID || true

mkhome() {  # mkhome <dir> : a checkout-shaped home with the real scripts and config
  mkdir -p "$1/config" "$1/ops"
  cp "$REPO/config/susq.toml" "$1/config/"
  cp "$REPO/ops/lib.sh" "$REPO/ops/backup.sh" "$REPO/ops/restore.sh" "$1/ops/"
}

H1="$WORK/home1"; mkhome "$H1"
mkdir -p "$H1/state/bridge" "$H1/journal/2026-10-05" "$H1/data/kalshi" "$H1/reports" "$H1/targets"
echo '{"header":{"seq":"7"},"mode":"paper"}' > "$H1/state/status.json"
head -c 300000 /dev/urandom | base64 > "$H1/journal/2026-10-05/bridge.journal.jsonl"
head -c 200000 /dev/urandom > "$H1/data/kalshi/prices.parquet"
echo "# Day 1" > "$H1/reports/DAY1.md"

echo "backup"
for day in 01 02 03 04 05 06 07 08 09 10 11; do
  dow=$(( (10#$day + 2) % 7 + 1 ))   # ISO weekday: 2026-10-01 is a Thursday (4); the 4th and 11th are Sundays (7)
  SUSQ_HOME="$H1" SUSQ_BACKUP_TS="202610${day}T073000Z" SUSQ_BACKUP_DOW="$dow" "$H1/ops/backup.sh" 2>/dev/null
done
B="$WORK/s3/drill-bucket/susquatch"
check "retention keeps 7 dailies" [ "$(count "$B/daily" 'susq-*.tar.zst')" = 7 ]
check "Sundays promoted to weekly" [ "$(count "$B/weekly" 'susq-*.tar.zst')" = 2 ]
check "checksums uploaded next to archives" [ -f "$B/daily/susq-20261011T073000Z.tar.zst.sha256" ]
check "backup.json records success + bucket bytes" python3 -c "
import json, sys
r = json.load(open('$H1/ops/state/backup.json'))
sys.exit(0 if r['last_result'] == 'ok' and r['bucket_bytes'] > 0 else 1)"
check "local staging emptied after upload" [ "$(count "$H1/ops/state/backup-staging" 'susq-*')" = 0 ]
check "ops event recorded" grep -q "backup uploaded" "$H1/ops/state/events.log"

echo "quota guard"
rc=0; SUSQ_HOME="$H1" SUSQ_R2_QUOTA_BYTES=1000 SUSQ_BACKUP_TS=20261012T073000Z "$H1/ops/backup.sh" 2>/dev/null || rc=$?
check "upload refused at >= 80% of quota (exit 3)" [ "$rc" = 3 ]
check "ACTION NEEDED filed" grep -q "Backups paused: R2 would be at" "$H1/ops/state/actions/r2-quota.txt"
check "refused archive kept locally" [ -f "$H1/ops/state/backup-staging/susq-20261012T073000Z.tar.zst" ]
check "nothing uploaded" [ ! -e "$B/daily/susq-20261012T073000Z.tar.zst" ]

echo "unmeasurable bucket"
rc=0; SUSQ_HOME="$H1" SUSQ_R2_SECRET_ACCESS_KEY=wrong SUSQ_BACKUP_TS=20261013T073000Z "$H1/ops/backup.sh" 2>/dev/null || rc=$?
check "refuses when bucket usage can't be measured" [ "$rc" != 0 ]
check "and says why" grep -q "could not measure" "$H1/ops/state/actions/r2-quota.txt"

echo "recovery"
SUSQ_HOME="$H1" SUSQ_BACKUP_TS=20261014T073000Z SUSQ_BACKUP_DOW=3 "$H1/ops/backup.sh" 2>/dev/null
check "r2-quota item resolved on the next success" [ ! -e "$H1/ops/state/actions/r2-quota.txt" ]
check "older refused archives cleaned from staging" [ "$(count "$H1/ops/state/backup-staging" 'susq-*')" = 0 ]

echo "restore"
H2="$WORK/home2"; mkhome "$H2"
check "--list shows newest first" [ "$(SUSQ_HOME="$H2" "$H2/ops/restore.sh" --list 2>/dev/null | head -n1)" = "daily/susq-20261014T073000Z.tar.zst" ]
SUSQ_HOME="$H2" "$H2/ops/restore.sh" --no-start --yes 2>/dev/null
for d in state journal data reports; do
  check "fresh home: $d/ identical" diff -r "$H1/$d" "$H2/$d"
done
check "restore.json written" grep -q 'susq-20261014T073000Z' "$H2/ops/state/restore.json"
check "no GO_LIVE restored (paper until a human re-arms)" [ ! -e "$H2/ops/GO_LIVE" ]
check "no .env restored (secrets live outside backups)" [ ! -e "$H2/.env" ]

echo "restore over existing data"
echo "diverged" > "$H2/state/status.json"
SUSQ_HOME="$H2" "$H2/ops/restore.sh" --no-start --yes --archive susq-20261009T073000Z.tar.zst 2>/dev/null
ASIDE="$(find "$H2" -maxdepth 1 -name '.pre-restore-*' | head -n1)"
check "previous data moved aside, not deleted" [ "$(cat "$ASIDE/state/status.json")" = "diverged" ]
check "named archive restored" diff "$H1/state/status.json" "$H2/state/status.json"

echo "tampered archive"
printf 'x' >> "$B/daily/susq-20261014T073000Z.tar.zst"
H3="$WORK/home3"; mkhome "$H3"
rc=0; SUSQ_HOME="$H3" "$H3/ops/restore.sh" --no-start --yes 2>/dev/null || rc=$?
check "checksum mismatch aborts" [ "$rc" != 0 ]
check "and touches nothing" [ ! -d "$H3/state" ]

echo "drill passed"
