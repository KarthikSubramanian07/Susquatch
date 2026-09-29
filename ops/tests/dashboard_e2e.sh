#!/usr/bin/env bash
# End to end, all local: the real Python status pusher and kill poller against the real Pages Functions
# runtime (`wrangler pages dev`, local KV), then the real summary Worker's cron handler on the same KV.
#
#   (cd dashboard && npm run dev) &      # in another terminal first
#   ops/tests/dashboard_e2e.sh
#
# Nothing touches Cloudflare or GitHub: KV is local, and the Worker runs with PUSH_TO_GITHUB=false.
set -euo pipefail
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
DASH="$REPO/dashboard"
URL="${DASH_URL:-http://localhost:8788}"
PW="$(sed -n 's/^DASHBOARD_PASSWORD=//p' "$DASH/.dev.vars")"
export SUSQ_STATUS_PUSH_TOKEN
SUSQ_STATUS_PUSH_TOKEN="$(sed -n 's/^SUSQ_STATUS_PUSH_TOKEN=//p' "$DASH/.dev.vars")"
export SUSQ_DASHBOARD_URL="$URL"
HOME_DIR="$(mktemp -d)"
export SUSQ_HOME="$HOME_DIR"
WORKER_PID=""
cleanup() { [ -n "$WORKER_PID" ] && kill "$WORKER_PID" 2>/dev/null; rm -rf "$HOME_DIR"; }
trap cleanup EXIT

check() {
  local what="$1"; shift
  if "$@"; then printf '  ok   %s\n' "$what"; else printf '  FAIL %s\n' "$what"; exit 1; fi
}
status_json() { curl -s -u "susq:$PW" "$URL/api/status"; }
field() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1], {}, {'d': d}))" "$1"; }

mkdir -p "$HOME_DIR/config" "$HOME_DIR/state" "$HOME_DIR/ops/state"
cp "$REPO/config/susq.toml" "$HOME_DIR/config/"
(cd "$DASH" && node scripts/seed.mjs empty >/dev/null)

echo "status push"
(cd "$REPO" && uv run --quiet python - <<'PY')
import os, time
from susq.contracts import from_json, to_json
from susq.v1 import ops_pb2
snap = from_json(ops_pb2.StatusSnapshot, open("contracts/fixtures/status_snapshot.json").read())
snap.header.produced_at_ns = time.time_ns()
snap.events.append("e2e: pushed by the real status pusher")
open(os.path.join(os.environ["SUSQ_HOME"], "state", "status.json"), "w").write(to_json(snap))
PY
check "anonymous status is refused" [ "$(curl -s -o /dev/null -w '%{http_code}' "$URL/api/status")" = 401 ]
check "pusher --once succeeds" sh -c "cd '$REPO' && uv run --quiet python -m susq.ops.status_push --once 2>/dev/null"
check "second push inside 100 s is refused (KV budget)" sh -c "! (cd '$REPO' && uv run --quiet python -m susq.ops.status_push --once 2>/dev/null)"
check "dashboard shows the pushed equity" [ "$(status_json | field 'd["status"]["snapshot"]["equity"]')" = "100431.5" ]
check "heartbeat fresh (not stale)" [ "$(status_json | field 'd["stale"]')" = "False" ]
check "pusher's ops health merged in" sh -c "status_json() { curl -s -u 'susq:$PW' '$URL/api/status'; }; status_json | grep -q '\"name\":\"status-writer\"'"
check "event ring has the pushed event" sh -c "curl -s -u 'susq:$PW' '$URL/api/status' | grep -q 'e2e: pushed by the real status pusher'"

echo "kill"
code="$(curl -s -o /dev/null -w '%{http_code}' -u "susq:$PW" -X POST "$URL/api/kill" -H 'Content-Type: application/json' -d '{"confirm":"kill","reason":"x"}')"
check "kill without the intent header is refused ($code)" [ "$code" = 400 ]
code="$(curl -s -o /dev/null -w '%{http_code}' -u "susq:$PW" -X POST "$URL/api/kill" -H 'Content-Type: application/json' -H 'X-Susq-Intent: kill' -H 'Origin: https://evil.example' -d '{"confirm":"kill","reason":"x"}')"
check "cross-origin kill is refused ($code)" [ "$code" = 403 ]
code="$(curl -s -o /dev/null -w '%{http_code}' -X POST "$URL/api/kill" -H "Authorization: Bearer $SUSQ_STATUS_PUSH_TOKEN" -H 'Content-Type: application/json' -H 'X-Susq-Intent: kill' -d '{"confirm":"kill","reason":"x"}')"
check "the server token cannot press Kill ($code)" [ "$code" = 403 ]
code="$(curl -s -o /dev/null -w '%{http_code}' -u "susq:$PW" -X POST "$URL/api/kill" -H 'Content-Type: application/json' -H 'X-Susq-Intent: kill' -H "Origin: $URL" -d '{"confirm":"kill","reason":"e2e drill"}')"
check "the human's confirmed kill is accepted ($code)" [ "$code" = 201 ]
check "kill poller writes ops/KILL" sh -c "cd '$REPO' && uv run --quiet python -m susq.ops.kill_poller once 2>/dev/null && grep -q 'e2e drill' '$HOME_DIR/ops/KILL'"
code="$(curl -s -o /dev/null -w '%{http_code}' -u "susq:$PW" -X DELETE "$URL/api/kill")"
check "the browser cannot un-kill ($code)" [ "$code" = 403 ]
check "make unkill clears the flag and the file" sh -c "cd '$REPO' && uv run --quiet python -m susq.ops.kill_poller unkill --yes >/dev/null 2>&1 && [ ! -e '$HOME_DIR/ops/KILL' ]"
check "flag is clear" [ "$(curl -s -u "susq:$PW" "$URL/api/status" | field 'd["kill"]["killed"]')" = "False" ]

echo "daily summary Worker (cron) on the same local KV"
(cd "$DASH" && npx wrangler dev --config ../worker/wrangler.jsonc --test-scheduled --port 8799 \
  --persist-to .wrangler/state --var PUSH_TO_GITHUB:false >/tmp/susq-worker-dev.log 2>&1) &
WORKER_PID=$!
for _ in $(seq 1 60); do grep -q "Ready on" /tmp/susq-worker-dev.log 2>/dev/null && break; sleep 1; done
check "scheduled handler runs" [ "$(curl -s -o /dev/null -w '%{http_code}' 'http://localhost:8799/cdn-cgi/handler/scheduled?cron=0+14+*+*+*')" = 200 ]
summary="$(curl -s -u "susq:$PW" "$URL/api/summary")"
check "summary visible on the dashboard" sh -c "printf '%s' '$summary' | grep -q '\"markdown\":\"### Susquatch'"
check "summary is not silent (fresh heartbeat)" sh -c "printf '%s' '$summary' | grep -q '\"silent\":null'"
echo "dashboard e2e passed"
