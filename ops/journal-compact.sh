#!/bin/bash
# Compress bridge journal days older than yesterday in place (zstd, lossless; `zstd -d` restores).
#
# The bridge journals every price poll, about 1.2 GB a day on a 29 GB disk; the nightly backup has
# already shipped these days to B2. Today and yesterday stay plain: the daily report (07:00 PT) and
# the healthcheck read them. Linux only (bash, GNU date); runs from susq-journal-compact.timer.
set -eu
SUSQ_HOME="${SUSQ_HOME:-/opt/susquatch}"
KEEP_FROM="$(date -u -d '1 day ago' +%F)"
for dir in "$SUSQ_HOME"/journal/*/; do
    [ -d "$dir" ] || continue
    day="$(basename "$dir")"
    [[ "$day" < "$KEEP_FROM" ]] || continue
    for f in "$dir"*.jsonl; do
        [ -f "$f" ] || continue
        nice -n 19 zstd -q -T1 -9 --rm "$f"
        echo "compressed $f"
    done
done
