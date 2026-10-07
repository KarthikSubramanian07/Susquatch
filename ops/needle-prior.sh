#!/bin/bash
# Rebuild the 2026 needle prior from the current fair values and restart the needle (if it runs).
#
# The prior's race margins come from state/fair_live.json, so a prior built weeks early is stale on
# the night. susq-needle-prior.timer runs this on Oct 31 and at 16:15 ET on Nov 3, before polls
# close (docs/RUNBOOK.md §2 pre-flight). It runs inside the always-on python-scheduler container,
# which has state/, data/ and config/ mounted; the county history CSVs must already be under
# data/replay/ (python -m susq.replay download, see the runbook). California House history
# (data/replay/house_results_<year>.csv, one small state file each) is downloaded here when missing;
# without it the CA House seats are listed as uncovered and the rest of the prior still builds. The
# old prior is kept as prior.prev.json, and a failed build leaves the running prior untouched.
set -euo pipefail
SUSQ_HOME="${SUSQ_HOME:-/opt/susquatch}"
cd "$SUSQ_HOME"
C=(docker compose --project-directory . -f ops/compose.yaml)
history=()
for y in 2022 2024; do
    [ -s "data/replay/county_results_$y.csv" ] || { echo "missing data/replay/county_results_$y.csv" >&2; exit 1; }
    history+=(--history "data/replay/county_results_$y.csv")
done
for y in 2022 2024; do
    house="data/replay/house_results_$y.csv"
    if [ ! -s "$house" ]; then
        "${C[@]}" exec -T python-scheduler python -m susq.replay download "$y" CA --house-only --out "$house" ||
            echo "warning: no CA House history for $y; CA House seats may be uncovered" >&2
    fi
    if [ -s "$house" ]; then history+=(--history "$house"); fi
done
"${C[@]}" exec -T python-scheduler python -m susq.replay spec2026 \
    --market-map state/market_map.json --fair state/fair_live.json "${history[@]}" \
    --poll-closing config/poll_closing.toml --out state/needle/general_2026.next.toml
"${C[@]}" exec -T python-scheduler python -m susq.replay prior state/needle/general_2026.next.toml \
    --out state/needle/prior.next.json
races=$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))["races"]))' state/needle/prior.next.json)
if [ "$races" -lt 60 ]; then
    echo "new prior has only $races races; keeping the running prior" >&2
    exit 1
fi
[ -f state/needle/prior.json ] && cp state/needle/prior.json state/needle/prior.prev.json
mv state/needle/general_2026.next.toml state/needle/general_2026.toml
mv state/needle/prior.next.json state/needle/prior.json
echo "staged prior with $races races"
if docker ps --format '{{.Names}}' | grep -qx susquatch-needle-1; then
    "${C[@]}" --profile election restart needle
    echo "needle restarted on the new prior"
fi
