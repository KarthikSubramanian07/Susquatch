#!/usr/bin/env bash
# Election-night dress rehearsal on the VM (docs/RUNBOOK.md §2b), with checks, then the step-5 record.
#
#   ops/rehearsal.sh                 # rehearsal #1: replay at 10x, needle stopped 60 s, bus kill drill
#   ops/rehearsal.sh --chaos         # rehearsal #2: the same plus --chaos-drop/--chaos-malformed-every/--chaos-kill
#   ops/rehearsal.sh --minutes 30    # stop the replay after 30 wall minutes (default: the whole timeline, ~102 min)
#   options: --speed X  --needle-stop-at MIN (default 15)  --kill-at MIN (default 30, 0 = no kill drill)
#            --no-stage (reuse state/needle/prior.json and the staged timeline)
#
# Before it: rust-live runs `paper-run` WITHOUT --election (.env: SUSQ_LIVE_CMD=paper-run, SUSQ_LIVE_FLAGS=
# empty, then `docker compose ... up -d --no-deps rust-live`). The script checks this and stops if not; it
# never edits .env. Afterwards put SUSQ_LIVE_FLAGS=--election back by hand.
#
# What it does, in order: stage the prior (ops/needle-prior.sh, the §2 pre-flight steps) and build the timeline
# from the same spec; start the needle; arm [strategies.en] in config/susq.toml and recreate rust-live with the
# executor's refusal reasons logged; publish the timeline into NATS from the scheduler container; stop the needle
# for 60 s mid-run; kill rust-live over NATS `ops.kill` and un-kill it by restarting it; then print the record.
#
# Safety: it never touches the bridge (every compose call names its service and passes --no-deps). The kill drill
# is NATS `ops.kill`, which only rust-live subscribes to, never `ops/KILL`, which the live bridge obeys too. On exit,
# however it exits, config/susq.toml is restored byte for byte and rust-live is recreated disarmed.
#
# SUSQ_COMPOSE overrides the compose command; SUSQ_REHEARSAL_SKIP_TRADER=1 skips every rust-live step (only for
# trying the script on a machine without the platform API key, where rust-live cannot start).
set -euo pipefail
SUSQ_HOME="${SUSQ_HOME:-$(cd "$(dirname "$0")/.." && pwd)}"
cd "$SUSQ_HOME"
read -r -a C <<< "${SUSQ_COMPOSE:-docker compose --project-directory . -f ops/compose.yaml}"
SKIP_TRADER="${SUSQ_REHEARSAL_SKIP_TRADER:-0}"

CHAOS=0 MINUTES=0 SPEED=10 STOP_AT=15 KILL_AT=30 STAGE=1
while [ $# -gt 0 ]; do
  case "$1" in
    --chaos) CHAOS=1 ;;
    --minutes) MINUTES="$2"; shift ;;
    --speed) SPEED="$2"; shift ;;
    --needle-stop-at) STOP_AT="$2"; shift ;;
    --kill-at) KILL_AT="$2"; shift ;;
    --no-stage) STAGE=0 ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
  shift
done

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUT="reports/rehearsal/$STAMP"          # host path; the containers see it at /susq/$OUT
mkdir -p "$OUT"
LOG="$OUT/rehearsal.log"
log() { printf '%s %s\n' "$(date -u +%H:%M:%SZ)" "$*" | tee -a "$LOG" >&2; }
die() { log "STOP: $*"; exit 1; }
now_ns() { python3 -c 'import time; print(time.time_ns())'; }
sched() { "${C[@]}" exec -T python-scheduler "$@"; }
live() { "${C[@]}" exec -T rust-live "$@"; }

CFG=config/susq.toml
TIMELINE=data/replay/rehearsal_2026.jsonl
CFG_BACKUP="$OUT/susq.toml.orig"
PIDS=()
ARMED=0
NEEDLE_STOPPED=0

# ── exit: restore the config, disarm rust-live, leave the needle running ─────────────────────────────────
cleanup() {
  local rc=$?
  set +e
  for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
  if [ -f "$CFG_BACKUP" ]; then
    if ! cmp -s "$CFG_BACKUP" "$CFG"; then
      cat "$CFG_BACKUP" > "$CFG"   # same inode: the containers bind-mount config/
      log "restored $CFG"
    fi
  fi
  if [ "$NEEDLE_STOPPED" = 1 ]; then "${C[@]}" --profile election start needle >/dev/null 2>&1; fi
  if [ "$ARMED" = 1 ] && [ "$SKIP_TRADER" != 1 ]; then
    log "recreating rust-live disarmed (default log filter)"
    env -u RUST_LOG "${C[@]}" --profile rust up -d --no-build --no-deps --force-recreate rust-live >>"$LOG" 2>&1
  fi
  if grep -A12 '^\[strategies.en\]' "$CFG" | grep -q '^enabled = true'; then
    log "WARNING: $CFG still has [strategies.en] enabled = true; run: git checkout $CFG"
  fi
  log "rehearsal output in $OUT (exit $rc). Put SUSQ_LIVE_FLAGS=--election back in .env when done."
}
trap cleanup EXIT
trap 'exit 130' INT TERM

oom_count() {
  { journalctl -k --no-pager 2>/dev/null || sudo -n journalctl -k --no-pager 2>/dev/null || dmesg 2>/dev/null; } |
    grep -c "Killed process" || true
}

# ── 0. pre-flight ─────────────────────────────────────────────────────────────────────────────────────────
[ -f ops/compose.yaml ] || die "run from the checkout (ops/compose.yaml not found)"
[ -n "$("${C[@]}" ps --status running -q nats)" ] || die "nats is not running"
[ -n "$("${C[@]}" ps --status running -q python-scheduler)" ] || die "python-scheduler is not running"
if [ "$SKIP_TRADER" != 1 ]; then
  [ -n "$("${C[@]}" --profile rust ps --status running -q rust-live)" ] || die "rust-live is not running"
  # shellcheck disable=SC2016  # expanded inside the container
  cmd="$(live sh -c 'printf "%s %s" "${SUSQ_LIVE_CMD:-run}" "${SUSQ_LIVE_FLAGS:-}"')"
  case "$cmd" in
    "paper-run "*) ;;
    *) die "rust-live runs '$cmd'; the rehearsal needs paper-run (.env SUSQ_LIVE_CMD=paper-run)" ;;
  esac
  case "$cmd" in
    *--election*) die "rust-live runs with --election; set SUSQ_LIVE_FLAGS= in .env and recreate rust-live first" ;;
  esac
fi
n_en_false="$(awk '/^\[/{s=($0=="[strategies.en]")} s && /^enabled = false$/' "$CFG" | wc -l | tr -d ' ')"
[ "$n_en_false" = 1 ] || die "$CFG: expected exactly one 'enabled = false' under [strategies.en]"
git diff --quiet -- "$CFG" 2>/dev/null || log "note: $CFG has local edits; they are kept and restored as they are"
cp -p "$CFG" "$CFG_BACKUP"
OOM_BEFORE="$(oom_count)"
free -m > "$OUT/free.before.txt" 2>/dev/null || true
log "pre-flight ok: chaos=$CHAOS speed=$SPEED minutes=${MINUTES:-all} needle stop at ${STOP_AT}m kill at ${KILL_AT}m"

# ── 1. stage the prior and the timeline ──────────────────────────────────────────────────────────────────
if [ "$STAGE" = 1 ]; then
  log "staging the prior (ops/needle-prior.sh)"
  SUSQ_HOME="$SUSQ_HOME" ops/needle-prior.sh 2>&1 | tee -a "$LOG"
  log "building the timeline from the same spec"
  sched python -m susq.replay build state/needle/general_2026.toml --out "$TIMELINE" 2>&1 | tee -a "$LOG"
fi
[ -s "$TIMELINE" ] || die "$TIMELINE missing (run without --no-stage)"
if [ -z "$("${C[@]}" --profile election ps --status running -q needle)" ]; then
  log "starting the needle"
  "${C[@]}" --profile election up -d --no-build --no-deps needle >>"$LOG" 2>&1 || die "needle did not start (image built? docker compose --profile election build needle)"
fi

# ── 2. arm the executor and recreate rust-live with its refusal reasons logged ───────────────────────────
awk '/^\[/{s=($0=="[strategies.en]")} s && /^enabled = false$/ {print "enabled = true"; next} {print}' \
  "$CFG_BACKUP" > "$OUT/susq.toml.armed"
cat "$OUT/susq.toml.armed" > "$CFG"
ARMED=1
log "armed [strategies.en] in $CFG (restored on exit)"
T0_NS="$(now_ns)"
if [ "$SKIP_TRADER" != 1 ]; then
  since="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  RUST_LOG="info,async_nats=warn,susq_live::en=debug" \
    "${C[@]}" --profile rust up -d --no-build --no-deps --force-recreate rust-live >>"$LOG" 2>&1
  "${C[@]}" logs -f --no-log-prefix --since "$since" rust-live > "$OUT/rust-live.log" 2>&1 &
  PIDS+=($!)
  log "waiting for rust-live to finish its REST bootstrap"
  for _ in $(seq 120); do grep -q "susq-live running" "$OUT/rust-live.log" && break; sleep 5; done
  grep -q "susq-live running" "$OUT/rust-live.log" || die "rust-live did not reach 'susq-live running' in 10 min"
fi

# ── samplers: bus tap (scheduler), executor metrics, status file, memory ─────────────────────────────────
TAP_S=$(( MINUTES > 0 ? MINUTES * 60 + 120 : 4 * 3600 ))
sched timeout "$TAP_S" python - "/susq/$OUT/tap.json" <<'PY' > /dev/null 2>&1 &
import asyncio, json, sys, time
import nats
from susq.contracts import bus_pb2
out = sys.argv[1]
s = {"posterior_msgs": 0, "races": [], "heartbeats": 0, "heartbeat_not_ok": {}, "shift_msgs": 0, "county_batches": 0}
races = set()
async def main():
    nc = await nats.connect("nats://nats:4222")
    async def on(m):
        if m.subject == "needle.race_posteriors":
            s["posterior_msgs"] += 1
            try:
                races.update(r.race_id for r in bus_pb2.RacePosteriors.FromString(m.data).races)
            except Exception:
                pass
        elif m.subject == "needle.heartbeat":
            s["heartbeats"] += 1
            try:
                st = bus_pb2.Heartbeat.FromString(m.data).status
            except Exception:
                st = "undecodable"
            if st != "ok":
                s["heartbeat_not_ok"][st] = s["heartbeat_not_ok"].get(st, 0) + 1
        elif m.subject == "needle.national_shift":
            s["shift_msgs"] += 1
        elif m.subject == "enr.county_results":
            s["county_batches"] += 1
    for subj in ("needle.>", "enr.county_results"):
        await nc.subscribe(subj, cb=on)
    while True:
        await asyncio.sleep(10)
        s["races"] = sorted(races)
        with open(out + ".tmp", "w") as f:
            json.dump(s, f)
        import os; os.replace(out + ".tmp", out)
asyncio.run(main())
PY
PIDS+=($!)
(
  while :; do
    ts="$(date -u +%s)"
    if [ "$SKIP_TRADER" != 1 ]; then
      m="$(live curl -fsS 127.0.0.1:9101/metrics 2>/dev/null | awk '/^susq_(needle_healthy|needle_posteriors|killed) /{printf "%s=%s ", $1, $2}')"
      echo "$ts $m" >> "$OUT/metrics.txt"
      [ -f state/status.paper.json ] && cp state/status.paper.json "$OUT/status.$ts.json"
    fi
    free -m 2>/dev/null | awk -v t="$ts" '/^Mem:/{print t, $2, $3, $7}' >> "$OUT/free.txt" 2>/dev/null
    sleep 15
  done
) &
PIDS+=($!)

# ── 3. publish the timeline ──────────────────────────────────────────────────────────────────────────────
PUB=(python -m susq.replay publish "/susq/$TIMELINE" --nats nats://nats:4222 --speed "$SPEED")
if [ "$CHAOS" = 1 ]; then
  # Replay seconds: a 10-minute feed drop at 1 h, a malformed payload every 25 deliveries, the publisher killed
  # at 2 h and back 2 minutes later (at 10x these are 6 wall minutes, then 12).
  PUB+=(--chaos-drop 3600:600 --chaos-malformed-every 25 --chaos-kill 7200:120)
fi
if [ "$MINUTES" -gt 0 ]; then PUB=(timeout "$(( MINUTES * 60 ))" "${PUB[@]}"); fi
log "publishing: ${PUB[*]}"
PUB_START="$(date -u +%s)"
sched "${PUB[@]}" > "$OUT/publish.log" 2>&1 &
PUB_PID=$!
PIDS+=("$PUB_PID")

wait_until() { local t=$(( PUB_START + $1 * 60 )); while [ "$(date -u +%s)" -lt "$t" ] && kill -0 "$PUB_PID" 2>/dev/null; do sleep 5; done; }
metric() { live curl -fsS 127.0.0.1:9101/metrics 2>/dev/null | awk -v k="$1" '$1==k{print $2}'; }

# ── 4a. stop the needle for 60 s ─────────────────────────────────────────────────────────────────────────
wait_until "$STOP_AT"
STOP_NS="$(now_ns)"
log "stopping the needle for 60 s"
"${C[@]}" --profile election stop needle >>"$LOG" 2>&1
NEEDLE_STOPPED=1
sleep 30
if [ "$SKIP_TRADER" != 1 ]; then
  log "needle down 30 s: susq_needle_healthy=$(metric susq_needle_healthy) susq_needle_posteriors=$(metric susq_needle_posteriors)"
fi
sleep 30
"${C[@]}" --profile election start needle >>"$LOG" 2>&1
NEEDLE_STOPPED=0
START_NS="$(now_ns)"
log "needle started again"
sleep 30
[ "$SKIP_TRADER" != 1 ] && log "needle back 30 s: susq_needle_healthy=$(metric susq_needle_healthy) susq_needle_posteriors=$(metric susq_needle_posteriors)"

# ── 4b. kill drill over NATS ops.kill (only rust-live listens), un-kill by restart ───────────────────────
KILL_NS=0 UNKILL_NS=0
if [ "$KILL_AT" -gt 0 ]; then
  wait_until "$KILL_AT"
  log "kill drill: publishing ops.kill"
  KILL_NS="$(now_ns)"
  sched python -c '
import asyncio, nats
from susq.contracts import bus_pb2
async def main():
    nc = await nats.connect("nats://nats:4222")
    k = bus_pb2.Kill(reason="dress rehearsal kill drill", issuer="human")
    await nc.publish("ops.kill", k.SerializeToString())
    await nc.flush()
    await nc.close()
asyncio.run(main())' >>"$LOG" 2>&1
  sleep 20
  if [ "$SKIP_TRADER" = 1 ]; then
    KILL_AT=0   # nothing here to restart; a local harness lifts its own kill
  fi
fi
if [ "$KILL_AT" -gt 0 ]; then
  log "killed: susq_killed=$(metric susq_killed) ($(grep -c 'kill switch triggered' "$OUT/rust-live.log" || true) kill log lines)"
  log "un-kill: restarting rust-live (a bus kill is sticky until restart)"
  since="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  "${C[@]}" --profile rust restart rust-live >>"$LOG" 2>&1
  # A follower ends when its container stops: follow the restarted process too.
  "${C[@]}" logs -f --no-log-prefix --since "$since" rust-live >> "$OUT/rust-live.log" 2>&1 &
  PIDS+=($!)
  for _ in $(seq 120); do [ "$(grep -c 'susq-live running' "$OUT/rust-live.log")" -ge 2 ] && break; sleep 5; done
  UNKILL_NS="$(now_ns)"
  log "rust-live running again"
fi

# ── wait for the replay to finish ────────────────────────────────────────────────────────────────────────
log "waiting for the replay to finish"
wait "$PUB_PID" || true
sleep 30
END_NS="$(now_ns)"
cat "$OUT/publish.log" | tee -a "$LOG"
OOM_AFTER="$(oom_count)"
free -m > "$OUT/free.after.txt" 2>/dev/null || true

# ── 5. the record ────────────────────────────────────────────────────────────────────────────────────────
python3 - "$OUT" "$T0_NS" "$STOP_NS" "$START_NS" "$KILL_NS" "$UNKILL_NS" "$END_NS" "$OOM_BEFORE" "$OOM_AFTER" \
  "$SKIP_TRADER" "$(awk '/^\[needle\]/{s=1} s && $1=="heartbeat_timeout_s"{print $3; exit}' "$CFG")" <<'PY' | tee "$OUT/record.txt"
import glob, json, os, re, sys
out, t0, stop, start, kill, unkill, end, oom0, oom1, skip, hb = sys.argv[1:12]
t0, stop, start, kill, unkill, end = map(int, (t0, stop, start, kill, unkill, end))
hb_ns = int(float(hb or 10) * 1e9)
skip = skip == "1"
S = 1e9

def load(p, default):
    try:
        with open(p) as f:
            return json.load(f)
    except Exception:
        return default

tap = load(os.path.join(out, "tap.json"), {})
intents = {}
try:
    for line in open("state/rust-live-paper/intents.jsonl"):
        try:
            i = json.loads(line)
        except ValueError:
            continue
        if i.get("strategy") != "en":
            continue
        created = int(i.get("createdAtNs", i.get("created_at_ns", 0)) or 0)
        if created >= t0:
            intents[i.get("intentId", i.get("intent_id"))] = (created, i)
except FileNotFoundError:
    pass
down = [c for c, _ in intents.values() if stop + hb_ns <= c <= start]
killed = [c for c, _ in intents.values() if kill and kill <= c <= (unkill or end)]

trades = {}
for p in sorted(glob.glob(os.path.join(out, "status.*.json"))):
    for t in load(p, {}).get("recentTrades", []):
        if t.get("strategy") == "en" and int(t.get("tsNs", 0)) >= t0:
            trades[(t.get("tsNs"), t.get("title"), t.get("quantity"), t.get("price"))] = t

metrics = []
try:
    for line in open(os.path.join(out, "metrics.txt")):
        kv = dict(x.split("=", 1) for x in line.split()[1:] if "=" in x)
        metrics.append(kv)
except FileNotFoundError:
    pass
max_post = max((float(m.get("susq_needle_posteriors", 0)) for m in metrics), default=0)
unhealthy_samples = sum(1 for m in metrics if m.get("susq_needle_healthy") == "0")

log_text = ""
try:
    log_text = open(os.path.join(out, "rust-live.log"), errors="replace").read()
except FileNotFoundError:
    pass
n429 = len(re.findall(r"rate limited: backing off", log_text))
reasons = {}
def fold(s):
    s = re.sub(r"^en [^:]+: ", "", s.strip())
    return re.sub(r"\d+(\.\d+)?", "#", s)
for line in log_text.splitlines():
    m = re.search(r'strategy="?en"?.*?note=(.*)$', line)
    if m:
        k = "note: " + fold(m.group(1))
    else:
        m = re.search(r'order blocked.*strategy=(?:Some\()?"?en"?\)?.*?reason=(.*?)(?: \w+=|$)', line)
        if not m:
            continue
        k = "blocked: " + fold(m.group(1))
    reasons[k] = reasons.get(k, 0) + 1
events = [l for l in log_text.splitlines() if "needle unhealthy: cancelling" in l or "KILL:" in l]

free = []
try:
    free = [l.split() for l in open(os.path.join(out, "free.txt"))]
except FileNotFoundError:
    pass
min_avail = min((int(f[3]) for f in free if len(f) == 4), default=None)
oom = int(oom1 or 0) - int(oom0 or 0)

print("── rehearsal record (docs/RUNBOOK.md §2b step 5) ──")
print(f"posteriors on the bus:          {tap.get('posterior_msgs', 0)} messages, {len(tap.get('races', []))} races; "
      f"{tap.get('county_batches', 0)} county batches; heartbeats {tap.get('heartbeats', 0)}, not ok {tap.get('heartbeat_not_ok', {})}")
if not skip:
    print(f"posteriors at the executor:     up to {max_post:.0f} races actionable at once; needle unhealthy in {unhealthy_samples} of {len(metrics)} samples")
    print(f"election-night intents placed:  {len(intents)}")
    print(f"  while the needle was down:    {len(down)}   (must be 0)")
    print(f"  while killed:                 {len(killed)}   (must be 0)")
    print(f"paper fills (en):               {len(trades)}")
    for t in list(trades.values())[:20]:
        print(f"    {t.get('title')}: {t.get('action')} {t.get('side')} x{t.get('quantity')} @ {t.get('price')}")
    print(f"429s (rust-live):               {n429}")
    print(f"needle-down / kill events:      {len(events)}")
    for e in events[:10]:
        print(f"    {e[-160:]}")
print(f"memory:                         min available {min_avail} MB; free -m after:")
for line in open(os.path.join(out, "free.after.txt")) if os.path.exists(os.path.join(out, "free.after.txt")) else []:
    print("    " + line.rstrip())
print(f"kernel OOM kills during run:    {oom}")
if not skip:
    print("refusal reasons (count):")
    for k, v in sorted(reasons.items(), key=lambda kv: -kv[1])[:40]:
        print(f"    {v:>7}  {k[:150]}")
ok = tap.get("posterior_msgs", 0) > 0 and oom == 0
if not skip:
    ok = ok and max_post > 0 and not down and not killed and n429 < 5
print("PASS" if ok else "FAIL", "(posteriors reached the executor, nothing placed while the needle was down or killed, "
      "no OOM, no sustained 429s; orders only on races with edge: read the fills above)")
PY
