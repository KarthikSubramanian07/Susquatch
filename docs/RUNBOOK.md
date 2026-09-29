# Runbook

`runbook` · `election-night` · `kill-switch` · `restore`

What to do, when, and what "normal" looks like. Times are **ET** with UTC in brackets. The bot doesn't need you for
any of this to be safe (it fails safe on its own). This is how you help it win.

**The four commands you need:** `touch ops/KILL` (stop now) · `make unkill` (resume) · `make ps` / `make logs` (look) ·
`make restore` (rebuild). Everything else is in [OPS.md](OPS.md).

---

## 1. Oct 1: the open (12:00 ET [16:00Z])

**T−24 h (Sep 30)**

- [ ] `make costs` → `RESULT: $0/month`, and tick the human checklist (Oracle **not** Pay-As-You-Go).
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
| **17:00 (22:00Z)** | Pre-flight | `git pull && make up` only if a fix was merged today (then `make ps`). Set `COMPOSE_PROFILES=rust,election` and `make up` so `needle` is running. `make selfcheck`. Dashboard on the phone, charger in. |
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
or the instance is missing from the Oracle console (idle reclaim, capacity event, you deleted it).

1. **Is it really dead?** `ssh ubuntu@<ip>`. If you get in, `make ps` and `make logs` first; most "silence" is one
   container or the pusher's token. `sudo systemctl restart susquatch` fixes the rest.
2. **Orders are safe meanwhile.** Every resting order has a GTD expiry (≤ 5 min TTL by default), so nothing we left
   outlives the host for long. You can also trade by hand on the website.
3. **New host** (about 30 min, most of it the image build):
   - Oracle: create A1 Flex 2 OCPU / 12 GB Ubuntu 24.04 with [`ops/cloud-init.yaml`](../ops/cloud-init.yaml), or, if Oracle
     has no capacity, switch to the Mac: `ops/launchd/install.sh` (then skip to step 5).
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
| Backups paused: R2 would be at N% | Delete old objects in the bucket, or lower `backup_keep_*` in `config/susq.toml`; the next good backup clears it |
| Backups paused: could not measure the R2 bucket | R2 token expired or revoked → new token in `.env` |
| Rotate soon: <name> in Nd | Rotate the credential, update `.env` / the Cloudflare or GitHub secret, update `SUSQ_EXPIRES_<NAME>` |
| No process holds the trader lease | `make logs`; usually the trader crashed on start (bad `.env`?) |
| <service> stayed unhealthy after 3 restarts | `make logs S=<service>`; the watchdog has stopped retrying on purpose |
| Kill poller cannot read the dashboard | Cloudflare outage or rotated push token. SSH `touch ops/KILL` still works |
| Kill was pressed … but the server hasn't confirmed it | Kill poller down → `make ps`; if in doubt, SSH `touch ops/KILL` |
| Couldn't post today's summary to GitHub | Worker's `GITHUB_TOKEN` expired → new fine-grained token (Issues: read & write) |
| Status push hit its daily budget | Something is pushing in a loop: `make logs S=status-pusher` |
