# Runbook

`runbook` · `election-night` · `kill-switch` · `restore`

What to do, when, and what "normal" looks like. Times are **ET** with UTC in brackets. The bot doesn't need you for
any of this to be safe (it fails safe on its own). This is how you help it win.

**The four commands you need:** `touch ops/KILL` (stop now) · `make unkill` (resume) · `make ps` / `make logs` (look) ·
`make restore` (rebuild). Everything else is in [OPS.md](OPS.md).

---

## 1. Oct 1: the open (12:00 ET [16:00Z])

**T−24 h (Sep 30)**

- [ ] `make costs` → `RESULT: $0/month`, and tick the human checklist (Azure for Students, **no** Pay-As-You-Go upgrade).
- [ ] `make selfcheck` → 7/7 ok, or at least nothing but `lease` (nobody trades before the open).
- [ ] Dashboard loads at `https://susquatch.pages.dev`; heartbeat green; "Today's summary" shows a date.
- [ ] Yesterday's 14:00Z comment landed on the pinned *Daily summary* issue (you got the email).
- [ ] `make backup` once by hand; `ops/restore.sh --list` shows it.
- [ ] `.env`: `SUSQ_API_KEY` set, `SUSQ_MODE=paper`, `COMPOSE_PROFILES=bridge`, `SUSQ_EXPIRES_*` filled in.
- [ ] Kill drill: press **Kill** on your phone (reason "drill"). Within about a minute `ops/KILL` exists, the button
      turns solid clay, and the summary shows the kill in risk events. Then `make unkill` over SSH and watch the
      heartbeat stay green.

**T−1 h (11:00 ET)**

- [ ] `make ps`: every service `Up`, healthchecks `healthy`.
- [ ] `ops/KILL` absent. No `ops/GO_LIVE` yet.
- [ ] Clock: `chronyc tracking` offset < 50 ms.

The bridge's own Oct 1 guide is [`bridge_py/README.md`](../bridge_py/README.md). With the key in `.env` it sleeps until the
open by itself (`make logs S=bridge` shows `sleeping … until 2026-10-01T16:00:00+00:00`).

**12:00 ET.** The bridge runs `measure_day1` on its own and writes `reports/DAY1.md`. Going live is a separate,
deliberate act: say **"go live"** in a Claude Code session (the `/go-live` pre-flight), which ends with
`echo 'go live' > ops/GO_LIVE` and `SUSQ_MODE=live`. Watch the first qualifying trade appear under *recent trades*.

## 2. Election night: Nov 3 17:00 ET → Nov 4 11:55 ET

DST ended Nov 1, so **ET = UTC−5** all night. The lock is **12:00 ET (17:00Z)**; the last order is **11:55 ET (16:55Z)**,
enforced by the risk time gate, every order's GTD expiry, cancel-all at 16:55Z, and the watchdog's post-lock `ops/KILL`.
Nothing you do can extend it, and nothing should.

| ET (UTC) | What happens | You |
|---|---|---|
| **17:00 (22:00Z)** | Pre-flight | `git pull && make up` only if a fix was merged today (then `make ps`). **Stage the 2026 needle prior** from the current fair values, inside the always-on scheduler container (it has `state/`, `data/` and `config/` mounted). From `/opt/susq`: (1) `docker compose --project-directory . -f ops/compose.yaml exec python-scheduler python -m susq.replay spec2026 --market-map state/market_map.json --fair state/fair_live.json --history data/replay/county_results_2022.csv --history data/replay/county_results_2024.csv --poll-closing config/poll_closing.toml --out state/needle/general_2026.toml`, (2) `docker compose --project-directory . -f ops/compose.yaml exec python-scheduler python -m susq.replay prior state/needle/general_2026.toml --out state/needle/prior.json`. It should print 69 races (every Cup Senate and Governor race) and list only House races as uncovered. If `data/replay/county_results_*.csv` are missing (first run on a fresh disk), run `docker compose --project-directory . -f ops/compose.yaml exec python-scheduler python -m susq.replay download 2022 --market-map state/market_map.json --out data/replay/county_results_2022.csv` and the same for 2024 first (about 5 min each; per-state files are cached under `data/replay/medsl/`). The needle reads the prior only at start, so stage it before `make up` below. Rehearse with `specs/sample_2022.toml`; the same two steps run on Oct 31 for the final pre-night prior. **Arm the election-night executor:** in `config/susq.toml`, under `[strategies.en]`, change the line `enabled = false` to `enabled = true`. That is the only line to flip (every other knob in that section is already set to its documented value); the executor ships off and nothing else turns it on. rust-live reads the file at start, so do this before `make up`; the bridge ignores the section. In `.env` set `COMPOSE_PROFILES=rust,election` and `SUSQ_LIVE_FLAGS=--election` (rust-live runs the results pollers), then `make up` so `needle` is running and rust-live restarts with the executor armed. `make selfcheck`. Dashboard on the phone, charger in. |
| 17:15 | Drill | `needle` heartbeat tile green. Replay smoke if you have one queued. Confirm `targets` tile says *accepted*. |
| **18:00 (23:00Z)** | Priority window opens (automatic) | `make priority` shows our scheduler and calshi's backfill/research at weight 128. First polls close (IN, KY). |
| 19:00–20:00 | GA, VA, NC, OH, then the big East wave | Watch *events*. A `needle silent` or `seq gap` event means the executor already stopped acting on posteriors; you do nothing unless it lasts more than 10 min. |
| 20:00–23:00 | Most of the map reports. Rolls west. | Stage should read **emerge**. Positions table is the scoreboard. |
| **23:00 (04:00Z)** | West Coast closes (CA, OR, WA) | CA SoS feed is the one that matters now. |
| 01:00 (06:00Z) | AK / HI | Things slow. Late mail batches keep landing. |
| **02:30 (07:30Z)** | Nightly backup slot | **Deferred** on purpose (window). It runs at the lock instead. |
| 07:00 (12:00Z) | Mail / provisional batches | Check the summary's ACTION NEEDED at 09:00 ET (14:00Z) as usual. |
| **11:30 (16:30Z)** | Wind-down | Nothing new should be opening. Expect cancels in *events*. |
| **11:55 (16:55Z)** | **HARD STOP** | Gateways refuse new orders; cancel-all fires; watchdog writes `ops/KILL` ("post-lock time gate"). Dashboard shows **Killed**. **Do not un-kill.** |
| **12:00 (17:00Z)** | Lock | Priority window closes and weights are restored (automatic), then the post-lock backup runs. |
| after | Archive | `make backup` once more by hand; `ops/restore.sh --list` shows it. Then the post-contest checklist in PLAN.md §8. |

**When to press Kill on election night.** Trust the breakers first; they're faster than you. Press it when you see
something the breakers can't: positions moving against a race that's already called, fills at absurd prices, a
market map that's obviously inverted (D/R swapped), or the dashboard showing trades while `needle` is red. Write
what you saw in the reason. It goes into `ops/KILL` and tomorrow's summary.

**What healthy looks like at 22:00 ET:** heartbeat < 3 min and green, `needle` ok with detail "posteriors …",
`kalshi` ok, `status-writer` ok, stage **emerge**, events every few minutes, no clay anywhere except the Kill button.

**API capacity on the night.** The platform gives each **account** one rate budget shared by every key:
**100 reads and 30 writes a minute** (changelog Oct 1; more keys add nothing). A `429` carries `Retry-After: 60`, and
the trading engine has its own throttle under the same code, so one 429 is a minute of lost writes. What that means:

- Only run **one** trader on the night. The bridge and rust-live use the same key, so running both splits the one
  budget even though the lease stops them trading together. The pre-cutover paper run (§2a) is the one planned
  overlap, and `[platform] max_rps = 1.0` halves each process's share for exactly that; the night profile raises it
  to 2.0 once the bridge is stopped.
- Don't poll from your laptop with the same key while the bot is working (a `curl` loop is a read the bot can't make).
  The dashboard reads our own status files, not the platform, so it is free.
- Books come over realtime (`tournament:{id}:market:{market_id}`, one-second batches, full `asOf`-versioned books),
  so REST reads are for reconciliation, not for watching prices. The bulk `/exchanges/prices` call covers 100 markets
  per read.
- 30 writes a minute is one order every 2 s, or one 50-order batch. The executor's limiter is tuned to 100/30; if the
  summary shows `RATE_LIMITED` in events, something else is spending the budget.

## 2a. Cutover: bridge to rust-live (Mon Nov 2, 10:00 ET; never on Nov 3)

Election night runs in rust-live: only it has the election-night executor, the results pollers and the needle
watchdog. The bridge trades until Nov 2. Do the switch on a quiet weekday morning, a full day before the night, so
anything odd has 24 hours to surface.

**Preconditions (all must hold).**
- The rust-live paper run (`SUSQ_LIVE_CMD=paper-run`, `SUSQ_LIVE_FLAGS=--election`, beside the bridge) has run at least
  72 hours with no OOM kills (`sudo journalctl -k | grep -c "Killed process"` unchanged), no sustained 429s in either
  trader, and its paper equity tracking the bridge's live equity within the drift you can explain from fills.
- Both dress rehearsals (§2b) passed, and the needle tile was green throughout the paper run.
- `make backup` succeeded today; `make selfcheck` is 7/7.

**Steps.**
1. `make backup`, then note equity, cash and open-order count from the dashboard.
2. `docker compose --project-directory . -f ops/compose.yaml stop bridge`. Stopping does **not** cancel its orders:
   the far-quote ladders (about 300 resting, 24 h TTL) and anything else stay on the book. Its lease lapses within
   `[lease] ttl_s` (30 s).
3. Edit `.env` in place (keep the inode; the containers bind-mount it): `COMPOSE_PROFILES=rust,election`,
   `SUSQ_LIVE_CMD=run`, `SUSQ_LIVE_FLAGS=--election`. `SUSQ_MODE=live` and `ops/GO_LIVE` are already set from Oct 1.
4. `git pull` to pick up the night-profile commit (below), then
   `docker compose --project-directory . -f ops/compose.yaml up -d --no-build --remove-orphans`.
5. Watch for 15 minutes: rust-live logs `susq-live running mode="live"`, takes `ops/state/trader.lease`, and its first
   reconcile cancels every open order it did not place (*events*: `cancelled untracked order …`). At 30 writes a minute
   that is about 10 minutes for 300 ladders; the far quoter then re-rests its own ladders on its next passes. Positions
   carry over (they are account-level and read from REST).
6. Check: `make ps` (bridge absent), dashboard trader = rust-live, equity unchanged within the far-quote cancels,
   no `RATE_LIMITED` events after the cancels finish.

**Night profile (merge as one commit on Nov 2, before step 4).** With one trader the whole budget is ours:
`[platform] max_rps = 2.0` (was 1.0, split for the paper run); `[strategies.farquote] interval_s = 3600` so top-ups
leave writes for the executor. Do **not** merge it while the bridge and the paper run both trade: each would take
the full budget.

**Rollback (any time before Nov 3 17:00 ET).** Stop rust-live; set `.env` back to `COMPOSE_PROFILES=bridge`,
`SUSQ_LIVE_CMD=paper-run`, `SUSQ_LIVE_FLAGS=`; revert the night-profile commit; `make up`. The bridge's reconcile
keeps every open order it finds (rust-live's included) instead of cancelling it, so nothing is lost; its far quoter
re-rests on its own cadence.

## 2b. Dress rehearsals (Oct 26-27 and Oct 29-30)

Both run on the VM beside the live trader, with rust-live in `paper-run` **without** `--election` (set
`SUSQ_LIVE_FLAGS=` for the rehearsal) and the needle on the bus: the replay is the results feed, and the pollers would
only add stale-feed gates, since before Nov 3 16:00 ET they poll every 30 min (`config/enr_sources.toml`
`idle_until_utc`). Put `--election` back afterwards so the soak keeps exercising the adapters.
1. Stage the rehearsal prior and timeline: `python -m susq.replay spec2026 …` then `prior` and `build` (§2 pre-flight
   commands) into `state/needle/prior.json` and `data/replay/`; restart `needle`.
2. In the VM checkout only, set `[strategies.en] enabled = true` (rust-live paper reads it; the bridge ignores the
   section) and restart rust-live. Revert the file when the rehearsal ends (`git checkout config/susq.toml`).
3. Publish the timeline into NATS from the scheduler container at 10x (`python -m susq.replay publish
   /susq/data/replay/<file>.jsonl --nats nats://nats:4222 --speed 10`); rehearsal #2 adds `--chaos-drop`,
   `--chaos-malformed-every` and `--chaos-kill`.
4. Mid-run, `docker compose … stop needle` for 60 s: the executor must cancel its working orders and place nothing
   until the needle is healthy again; then `touch ops/KILL` once and un-kill.
5. Record: posteriors received, election-night intents placed, fills on paper, 429s, `free -m`, kernel OOM count,
   and every refusal reason. Pass = orders only on races with edge, none while the needle was down, no OOM, no
   sustained 429s.

## 3. Kill and un-kill

| To | Do | Takes effect |
|---|---|---|
| Stop, from anywhere | Dashboard → **Kill** → reason → **Kill now** | ~10 s poll + up to ~60 s KV propagation |
| Stop, right now | `ssh … 'touch /opt/susquatch/ops/KILL'` (any text = the reason) | next gateway check (< 1 s) |
| Resume | `ssh …` then `cd /opt/susquatch && make unkill` → type `un-kill` | next gateway check. Gateways reconcile from REST, then trade |
| Resume while Cloudflare is down | `docker compose --project-directory . -f ops/compose.yaml stop kill-poller`, then `make unkill ARGS=--local-only`; start the poller again once Cloudflare is back and the flag is cleared | a running poller re-creates `ops/KILL` while the flag is set. That's the point |

Un-kill clears the dashboard flag **first**, then removes `ops/KILL`; if the flag can't be cleared it removes nothing.
The browser can't un-kill, by design: if your phone is stolen, the worst anyone can do with it is stop the bot.

Killed state checklist before un-killing: What tripped it (reason in the banner and `ops/KILL`)? Is the cause gone? Is
it before 11:55 ET Nov 4? After that, never un-kill.

## 4. Restore: the server is gone, or wrong

Symptoms: the dashboard shows the clay **Server silent since …** banner; the 14:00Z summary leads with it; SSH times out
or the VM is stopped or missing in the Azure portal (`az vm show -d -g susquatch -n susquatch --query powerState`):
the student credit ran out and the subscription was disabled, a host failure, or you deleted it.

1. **Is it really dead?** `ssh ubuntu@<ip>`. If you get in, `make ps` and `make logs` first; most "silence" is one
   container or the pusher's token. `sudo systemctl restart susquatch` fixes the rest.
2. **Orders are safe meanwhile.** Every resting order has a GTD expiry (≤ 5 min TTL by default), so nothing we left
   outlives the host for long. You can also trade by hand on the website.
3. **New host** (about 30 min, most of it the image build):
   - Azure: if the VM only stopped, `az vm start -g susquatch -n susquatch` and go to step 5. Otherwise create a new
     `Standard_B2pls_v2` with [`ops/cloud-init.yaml`](../ops/cloud-init.yaml) as custom data ([OPS.md §6](OPS.md) has the
     exact `az` commands). If the subscription is disabled (credit gone), switch to the Mac: `ops/launchd/install.sh`,
     then step 4 in the Mac checkout (its `.env` is already there).
   - `ssh ubuntu@<new-ip>`, `nano /opt/susquatch/.env` (paste from your password manager).
4. **Restore:** `cd /opt/susquatch && make restore` (newest archive; `ARGS="--list"` to choose, `ARGS="--archive susq-…"` for a
   specific one). It verifies the checksum, moves anything present aside, restores, and starts the stack.
5. **Verify:** `make ps` → healthy; `make selfcheck`; dashboard heartbeat green within ~3 min; the lease is taken (`make selfcheck`
   shows `lease ok held by …`).
6. **Re-arm live, only if it was live:** `echo 'go live' > ops/GO_LIVE`. Backups never contain it, so a restored host is
   always paper until you say so.
7. Only one host at a time. If the old one comes back, `sudo systemctl disable --now susquatch` on it.

**Drill it once.** Before Oct 1, and again before Nov 3: restore the latest backup onto the Mac with
`SUSQ_HOME=/tmp/drill ops/restore.sh --no-start` and diff it against the server. The automated version is
`ops/tests/backup_restore_drill.sh`.

## 5. When the summary says…

| ACTION NEEDED says | Do |
|---|---|
| Server silent since … | §4 |
| Backups paused: the backup bucket would be at N% | Delete old objects in the B2 bucket, or lower `backup_keep_*` in `config/susq.toml`; the next good backup clears it |
| Backups paused: could not measure the backup bucket | B2 application key expired or revoked → new key in `.env` (`SUSQ_R2_*`) |
| Rotate soon: <name> in Nd | Rotate the credential, update `.env` / the Cloudflare or GitHub secret, update `SUSQ_EXPIRES_<NAME>` |
| No process holds the trader lease | `make logs`; usually the trader crashed on start (bad `.env`?) |
| <service> stayed unhealthy after 3 restarts | `make logs S=<service>`; the watchdog has stopped retrying on purpose |
| Kill poller cannot read the dashboard | Cloudflare outage or rotated push token. SSH `touch ops/KILL` still works |
| Kill was pressed … but the server hasn't confirmed it | Kill poller down → `make ps`; if in doubt, SSH `touch ops/KILL` |
| Couldn't post today's summary to GitHub | Worker's `GITHUB_TOKEN` expired → new fine-grained token (Issues: read & write) |
| Status push hit its daily budget | Something is pushing in a loop: `make logs S=status-pusher` |
