# Election night: data layer, replays and drills

*Written 2026-09-28. Everything below marked "verified" was fetched or tested on that date. Anything
not verified says so.*

Election night is where P(1st) gets decided (SPEC §3 Phase 3), so this layer is built to survive
messy public feeds.

```
 public results sites                    crates/enr (Rust, inside rust-live)                 bus / needle
 ┌──────────────────────┐  poll (jitter,   ┌─────────────────────────────────────────┐  mpsc   ┌──────────────┐
 │ CA SoS JSON          │  ETag, backoff)  │ adapters → validate → tag vote type →   │ ──────► │ bus crate →  │
 │ Clarity/Scytl ENR    │ ───────────────► │ dedupe → monotonic/corrections →        │ Batch   │ NATS         │
 │ other SoS (JSON map) │                  │ cross-source check → header(seq)        │         │ enr.county_  │
 └──────────────────────┘                  └─────────────────────────────────────────┘         │ results      │
                                              │ watch<EnrHealth>: disputed races, per-feed   └──────┬───────┘
                                              ▼ health (executor refuses disputed/stale)            ▼
                                           enr-capture → data/enr/<source>/<date>/          needle (C++)
 py/susq/replay (Python)
   history + conventions → timeline → replay.jsonl ──► runner (N×, chaos) ──► NATS  or  needle (susq_models / reference)
   history + race priors → NeedlePrior ─────────────────────────────────────► drill → reports/drills/<date>.md
```

## Quick start

```bash
# Rust: pollers, fixtures, fault and property tests
cargo test -p susq-enr

# Capture a CA SoS window (raw payloads + normalized batches under data/enr/, gitignored)
cargo run -p susq-enr --bin enr-capture -- --config config/enr_sources.toml --duration-s 18000

# Python: build a replay + prior, run a drill (with chaos), publish to a local NATS
uv run pytest py/tests/replay
uv run python -m susq.replay drill py/susq/replay/specs/sample_2022.toml \
    --chaos-drop 1800:900 --chaos-malformed-every 7 --chaos-kill 5400:600
uv run python -m susq.replay build py/susq/replay/specs/sample_2022.toml --out data/replay/sample.jsonl
uv run python -m susq.replay publish data/replay/sample.jsonl --nats nats://127.0.0.1:4222 --speed 120
```

---

## 1. Public results sources

### 1.1 California Secretary of State (verified, live)

**Documents.** The Election Night Data Feed Guide set lives at `https://cms.cdn.sos.ca.gov/media/`:
media guide v14, v16 (the brief's link) and v17 through v20. v20 is "June 2, 2026 / Updated for 2026 Primary Election"
and is the current one. The API itself is specified in `3-api-documentation-v20.pdf`. v13, v15 and v21 and above return 403.

**Two equivalent JSON paths.** Both return the same record shape.

| Path | Example | Notes |
|---|---|---|
| Bulk files, one per contest type | `https://media.sos.ca.gov/media/us-rep.json`, `governor.json`, `state-senate.json`, … | Every statewide/districtwide row **plus one row per county**. Served from S3 behind CloudFront and Imperva, with `ETag` and `Last-Modified`. A conditional GET returns **304** (verified with both `If-None-Match` and `If-Modified-Since`). This is our primary. |
| REST API | `https://api.sos.ca.gov/returns/governor`, `/returns/governor/county/alameda`, `/returns/us-rep/district/22`, `/returns/us-rep/district/all` | AWS API Gateway. No auth. `?f=csv` returns **UTF-16LE with no byte-order mark**. |
| Query API | `https://api.sos.ca.gov/returns/query?r=%5B%2202000000000059%22%5D` | JSON array of ≤ 10 race ids; more than 10 returns a 500. The id is a 12-character contest id plus county number (59 = statewide). The brackets must be URL-encoded; raw brackets gave a 400. |

The endpoint lists for the June 2026 primary are `media.sos.ca.gov/media/json-endpoints.csv` and `api-endpoints.csv`
(2,118 lines). The 2026 general versions are **not posted yet**; re-check in mid-October.

**Record format** (verbatim, `GET /returns/governor/county/alameda`, 2026-09-28; the county object comes before the statewide one):

```json
[
  {
    "raceTitle": "Governor - Alameda County Results",
    "Reporting": "0% (0 of 627) precincts reporting",
    "ReportingTime": "September 28, 2026, 2:11 p.m.",
    "candidates": [
      { "Name": "Xavier Becerra", "Party": "Dem", "Votes": "0", "Percent": "0.0", "incumbent": false },
      { "Name": "Steve Hilton", "Party": "Rep", "Votes": "0", "Percent": "0.0", "incumbent": false }
    ]
  },
  { "raceTitle": "Governor - Statewide Results", "Reporting": "1.5% (335 of 23,095) precincts reporting", "…": "…" }
]
```

- Counts are strings with thousands separators.
- `Reporting` is free text, and we parse `(n of m)` out of it.
- `ReportingTime` is Pacific wall-clock time in AP style ("p.m.", "noon"). It is PDT in October and PST on election night, so the adapter uses the IANA zone `America/Los_Angeles`, not a fixed offset.
- The feed gives **no vote-type split**, so CA results are tagged `MIXED`.
- Contest names differ by path: the bulk files say "United States Representative District 13" and the REST API says "U.S. House of Representatives District 13". `config/enr_sources.toml` maps both.
- Every House district, including single-county ones, has per-county rows.

**Status endpoint gotcha.** `/returns/status` and any unknown status slug fall back to a **stale Sept 16 2024 dataset**.
Use `/returns/status/gubernatorial-general`. Its fields are `precinctsReporting`, `precinctsTotal`, `voterTurnout`,
`totalRegisteredVoters`, `reportType` (R = Election Night, F = Semi-Final, U = Update) and `lastReportTimestamp` (`YYYYMMDDHHmm`).
`/returns/us-senate` returns 500 because there is no CA Senate race in 2026.

**Cadence** (Media Guide v20, verbatim): "The election night data files are created approximately every five minutes after
the polls close at 8:00 p.m. Pacific Time on Election Day… County elections officials are required to send initial results
to the Secretary of State's office no more than two hours after they begin tallying the votes (no later than 10:00 p.m.)."
We poll every 20 s ± 20 %; with conditional GETs that costs almost nothing.

**Test data is live now.** On 2026-09-28 the API and the bulk files already served 2026 *general* test data: Becerra vs Hilton for
governor, McGuire vs Gallagher for CA-1, and random-looking counts, with first report 10:49 and last 2:11 p.m. PT. `enr-capture`
ran against it on 2026-09-28 and produced 202 county results (58 governor rows plus 144 House county rows), with no rejects and
no unresolved counties.

**Test-window dates: resolved (2026-09-29). The brief's Oct 22 / Oct 29 / Nov 1 are the *2025 special election's* dates.**

Every published CA SoS testing schedule follows one rule: **Wednesday 13 days before, Wednesday 6 days before, Saturday 3 days before**
the election, 8:00 a.m. to 12:00 p.m. PT. (The Saturday prose says "until 1:00 p.m.", but every table says noon.)

| Schedule doc | Election | Test dates | Offsets |
|---|---|---|---|
| v17 | 2024 presidential primary (Tue Mar 5) | Wed Feb 21, Wed Feb 28, Sat Mar 2 | −13, −6, −3 |
| v18 | 2024 general (Tue Nov 5) | Wed Oct 23, Wed Oct 30, Sat Nov 2 | −13, −6, −3 |
| **v19** | **2025 statewide special (Tue Nov 4)** | **Wed Oct 22, Wed Oct 29, Sat Nov 1** (the brief's dates) | −13, −6, −3 |
| v20 | 2026 primary (Tue Jun 2) | Wed May 20, Wed May 27, Sat May 30 | −13, −6, −3 |
| (v21, not yet posted) | **2026 general (Tue Nov 3)** | **Wed Oct 21, Wed Oct 28, Sat Oct 31** | −13, −6, −3 |

- 2026 windows in UTC: **15:00–19:00Z** (PDT is still in effect on Oct 31; DST ends Nov 1).
- **Automated:** `ops/systemd/susq-enr-capture.timer` runs `enr-capture` (CA sources only) from 14:50Z for 5.5 h on all three dates.
  If v21 lands with different dates, edit the three `OnCalendar` lines.
- **Action:** once `https://cms.cdn.sos.ca.gov/media/4-testing-schedule-v21.pdf` stops returning 403, confirm the dates. The daily self-check can
  poll it. Meanwhile, 2026 general test data is already being served, so ad-hoc `enr-capture` runs are useful now.

### 1.2 Clarity / Scytl ENR (verified on real 2020–2024 elections)

The URL layout, with `{host}` being `results.enr.clarityelections.com` or a state-branded domain on the same platform
(for example `www.enr-scvotes.org` or `electionresults.iowa.gov`):

```
{host}/{ST}[/{County}]/{electionId}/current_ver.txt             -> "356048"
{host}/{ST}[/{County}]/{electionId}/{ver}/json/en/summary.json  -> contests: C name, K key, CH choices, P parties, V votes, TP/PR units
{host}/{ST}[/{County}]/{electionId}/{ver}/json/details.json     -> per contest K: P counties[], V[county][choice], T[county]
{host}/{ST}[/{County}]/{electionId}/{ver}/json/en/electionsettings.json -> settings.electiondetails (istestmode, participatingcounties, ...)
{host}/{ST}[/{County}]/{electionId}/{ver}/reports/detailxml.zip -> detail.xml: Contest > Choice > VoteType > County votes
{host}/{ST}/elections.json                                      -> election discovery (IA lists 126082 = 2026 primary)
```

- A snapshot under `{ver}` is immutable, so the adapter polls only `current_ver.txt` and fetches a snapshot **only when the version changes**.
- A version that goes **down** is refused and counted as a `version_regression`.
- The version only advances after a snapshot parses completely, so a corrupt publish is retried rather than skipped.

`summary.json` (WV 2024, abbreviated):
`{"C":"U.S. PRESIDENT","K":"100","VF":1,"TP":55,"PR":53,"CH":["DONALD J. TRUMP","KAMALA D. HARRIS",…],"P":["REP","DEM",…],"V":[533556,214309,…],"T":762390}`.
In `details.json`, `V[i]` is aligned with `summary.CH` for the same `K` (verified on Barbour County: 5071 / 1199).

**Vote types live only in `detail.xml`, and their labels are free text** set by each jurisdiction:

| Source | Labels seen | Our tag |
|---|---|---|
| SC 2024 | Election Day, Early Voting, Absentee By Mail, Failsafe, Failsafe Provisional, Provisional, **"Provionsal"** (a typo, 42 times) | E-day, Early, Mail, E-day, Provisional, Provisional, Provisional |
| Fulton County GA 2024 | Election Day Votes, Advance Voting Votes, Absentee by Mail Votes, Provisional Votes (plus contest-level Overvotes/Undervotes, which are ignored) | E-day, Early, Mail, Provisional |
| WV 2024 | "Election Day" only, although it covers all ballots | **MIXED**: one label per contest means the source isn't really splitting |
| CO 2020 | "Total Votes" | MIXED |

Other Clarity details:
- County sites can mark cells `"protected"` (a privacy filter). We treat that as malformed, never as zero.
- `electionsettings.json` → `electiondetails.istestmode` flags test data. The adapter does not read it yet (see §6).
- Georgia's statewide Clarity ids (105369, 115465, 54042) now return 403, because **Georgia moved to Enhanced Voting** (§1.3). Fulton County's own Clarity site still works.

### 1.3 Georgia: Enhanced Voting (verified on 2024 data)

- `https://results.sos.ga.gov/results/public/api/elections/Georgia/2024NovGen/data/ballot-item/<id>` returns
  `ballotItemWithBreakdown.breakdownResults[]`, one per county. Each has `ballotOptions[].groupResults[]`, one per vote method
  (Advanced Voting, Election Day, Absentee by Mail, Provisional), and `precinctReportingStatus`.
- This is what the results site itself calls. It is unauthenticated but **not a published API**, so paths and ids can change.
- It is wired through the generic JSON mapper as `ga_sos` (disabled until the 2026 ids exist), using the `candidate_breakdown` pivot.
- The fixture test asserts the official Fulton County numbers per method: Harris advance 297,293, E-day Trump 25,789 and so on.

### 1.4 Battleground results sites

| State | Official site | Platform | Machine-readable endpoint | Confidence |
|---|---|---|---|---|
| CA | api.sos.ca.gov, media.sos.ca.gov | Custom SoS | §1.1 | High (tested) |
| GA | results.sos.ga.gov | Enhanced Voting | §1.3 (2024 verified; 2026 ids not yet published) | High |
| SC | enr-scvotes.org | Clarity | `/SC/122436/359624/…` fetched; 2026 id not listed yet | High |
| IA | electionresults.iowa.gov/IA/ | Clarity | `/IA/elections.json`; `/IA/121508/current_ver.txt` = 344769 | High |
| NC | er.ncsbe.gov | Custom NCSBE | `s3.amazonaws.com/dl.ncsbe.gov/ENRS/2024_11_05/results_pct_20241105.zip` (206 OK) | High |
| NE | electionresults.nebraska.gov | Custom (ASP.NET) | `/resultsCSV.aspx?text=All&type={All,CG,SW,…}&map={CTY,DIST}` | High on endpoint |
| AZ | results.arizona.vote | Custom SoS | UNVERIFIED (403 to curl) | Medium |
| NV | silverstateelection.nv.gov | Custom SoS | UNVERIFIED (Incapsula bot wall) | Medium |
| TX | results.texas-election.com | Custom (Angular behind Cloudflare) | UNVERIFIED | Medium |
| OH | liveresults.ohiosos.gov | Custom SoS | UNVERIFIED (redirects off-season) | Medium |
| MI | mielections.us/election/results/ | Custom SoS | UNVERIFIED (timed out) | Medium |
| KS | sos.ks.gov ENR page | Custom SoS | UNVERIFIED | Medium |
| AK | elections.alaska.gov | Custom (PDF/txt/xml per media packet) | UNVERIFIED (403/405) | Medium |
| ME, NH | — | No live statewide system found | none | Low |

The custom sites that don't have adapters get one by config (`kind = "static_json"`) once their JSON is captured. CSV/zip sources (NC, NE)
would need a small new adapter; that is not built.

---

## 2. Poll closing times: `config/poll_closing.toml`

- The table covers all 50 states. Per state it lists the closings per time zone (the **last entry is the statewide last close**), local time, UTC time for
  Tue 2026-11-03, the source URL, a `split_time_zone` flag and a `verified` flag.
- DST ends Nov 1, so the offsets are EST −5, CST −6, MST −7 (AZ all year), PST −8, AKST −9, HST −10. `py/tests/replay/test_closing.py`
  recomputes every UTC value from IANA tzdata.
- **Verified from statutes or election offices (17):** AK, AZ, CA, FL, IA, ID, KS, ME, MI, NC, ND, NE, NV, OH, OR, SC, TX.
- **Unverified (33, from 270toWin's 2026 schedule):** includes GA (403 from the SoS; the search snippet says 7 a.m. to 7 p.m.), NH (the statute sets a
  7 p.m. minimum; towns may close at 8) and NY.
- **Split time zones:** FL panhandle, IN, KY, TN, MI (4 UP counties), TX (El Paso and Hudspeth), KS (4 western counties), NE, ND, SD, ID, OR (Malheur) and AK (western Aleutians).
- **Caveats:** some KS counties may close as late as 8 p.m. local and ND precincts as late as 9 p.m. The table stores the statutory 7 p.m.
- **Action:** verify GA and NH from the SoS before Oct 26.

Earliest to latest: 23:00Z (IN/KY Eastern), 00:00Z (GA, SC, VA, VT, …), 00:30Z (NC, OH, WV), 01:00Z, 01:30Z (AR), 02:00Z (IA, NE,
AZ, western TX/KS, MI UP, …), 03:00Z (NV, MT, UT), 04:00Z (CA, WA, OR), 05:00Z (HI, most of AK), 06:00Z (western Aleutians).

---

## 3. Historical county results (2022, 2024)

| Source | Where | License | Granularity | Vote-method split | Download without login |
|---|---|---|---|---|---|
| MEDSL county presidential 2000–2024 | doi:10.7910/DVN/VOQCHQ | CC0 1.0 | County | has `mode` | **No**: Dataverse guestbook form (name, email, institution) |
| MEDSL 2022 Senate county | doi:10.7910/DVN/YB60EJ | CC0 1.0 | County | has `mode` | **No** (guestbook) |
| MEDSL 2022 precinct, by state | GitHub `MEDSL/2022-elections-official` `individual_states/2022-<st>-local-precinct-general.zip`; Dataverse doi:10.7910/DVN/UYQIEP | CC0 1.0 (Dataverse; the GitHub repo has no LICENSE file) | Precinct | yes, 14 states in 2022 Senate | **Yes** |
| MEDSL 2024 precinct, by state | GitHub `MEDSL/2024-elections-official` `individual_states/<st>24.zip`; doi:10.7910/DVN/NYTPDU | CC0 1.0 (Dataverse) | Precinct | yes, 17 states in 2024 president | **Yes** |
| tonmcg county presidential 2008–2024 | github.com/tonmcg/US_County_Level_Election_Results_08-24 | MIT | County | no | Yes |
| Georgia SoS (Enhanced Voting) | results.sos.ga.gov API (§1.3) | Public record | County × method | **yes** (2022 and 2024) | Yes |

- `python -m susq.replay download <year> <states…>` pulls the GitHub precinct zips and aggregates them to county CSV under `data/replay/` (gitignored).
  - It keeps general-stage rows only.
  - It drops non-candidate rows (undervotes, overvotes, total ballots cast) and rows without a FIPS code ("STATE FLOATING").
  - Statewide offices only.
- **Which states split by method:**
  - 2022 Senate: AK AR AZ GA HI IA ID LA MD NC NY OK SC (WA only has write-in rows).
  - 2024 president: AK AL AR AZ CT DE HI IA IN MD NC NJ NY OK RI SC (WV has one label only).
  - **Georgia 2024 is TOTAL-only in MEDSL**; its method split comes from the GA SoS API.
- **Double-count trap:** some states publish a TOTAL row next to the split rows (GA 2022, AZ, ID, SC, NY, CT, DE, IN, NJ, RI). The loader drops TOTAL wherever split rows exist.

**Fixture** (`py/tests/replay/fixtures/county_results_sample.csv`, 2,142 real rows, 209 KB):
- GA 2022 US Senate: 159 counties × 4 methods, from MEDSL. It matches the SoS certified totals exactly (Warnock 1,946,117, Walker 1,908,442, Oliver 81,365), and the test asserts this.
- NH 2022 Senate and 2024 president: towns summed to counties.
- ME 2022 Governor and 2024 president.

**Caveats:**
- Alaska reports by state house district, not county.
- New England reports by town. CT counties are census units only.
- Virginia's independent cities are county equivalents.
- Maine RCV contests are first-round counts.
- MEDSL leaves Maine's statewide UOCAVA (overseas) row out of county totals.
- NH 2024 county sums differ from tonmcg by 1 to 19 votes in 8 counties; which one is right is unverified.

---

## 4. `crates/enr`: the Rust pollers

### Pipeline (single owner, no locks)

1. **Adapters** fetch and parse. Parsers are pure functions over bytes, so every one is tested on recorded payloads.
   - `ca_sos`: bulk or REST.
   - `clarity`: version polling, `detail.xml` (default) or the JSON pair.
   - `static_json`: a jq-like path mapper over any JSON, with `^N` ancestor bindings and a per-candidate vote-type **breakdown pivot**, configured entirely in TOML.
   - County names resolve to FIPS through the bundled Census 2020 table. Names are folded ("DeKalb" and "De Kalb", "St." and "Saint"); VA cities stay distinct; numeric codes are checked against the state prefix.
2. **Validation** (never panics). Each rejection is counted with its reason:
   - an empty race, source or candidate; a bad state code or FIPS;
   - an unknown vote type;
   - duplicate candidates;
   - candidate votes adding up to more than the total;
   - more precincts reporting than exist;
   - counts above 20 M.
3. **Vote-type tagging.** Labels are classified by keyword ("provi" catches the typo; "failsafe" counts as E-day; "one stop" as early). An absent or unknown label becomes `MIXED`.
4. **Dedupe.** `batch_id = sha256(source|race|county|vote_type|total|precincts|sorted candidate votes)[:32]`. Timestamps are excluded on purpose: a re-stamped, unchanged result is not new data.
   - The Python replay builder uses the identical recipe, and a shared test vector (`432a72f2…`) pins it in both languages.
5. **Monotonicity.** Counts are cumulative, so a **decrease is held as a correction** and only applied after the source repeats the same
   numbers on `confirm_corrections_after` (default 2) consecutive polls.
   - A transient glitch that reverts is never applied.
   - Held and applied corrections are logged and archived.
6. **Cross-source consistency.** Per (race, county), each source's latest results are summed over vote types and compared pairwise on DEM/REP/other.
   - A lagging source is fine as long as it is a plausible *earlier snapshot*: no party may have more votes in it than in the leading source (within tolerance).
   - When totals match within 2 % (with a 50-vote floor), the D two-party shares must agree within 1 pt.
   - A violation marks the race **disputed** in `EnrHealth.disputed_races` until it resolves. **The election-night executor must refuse to act on disputed races.**
7. **Emit.** `CountyResultsBatch` has header producer `rust-live/enr`, a strictly increasing `seq` (assigned by the single pipeline task, so it
   is ordered on the channel), and at most 500 results per batch so a bus message stays under NATS' 1 MB default.
   - Output goes on a `tokio::sync::mpsc` channel. Health goes on a `watch` channel.

### HTTP and polling

- There is one client per source, with a per-request total timeout (connect is capped at 5 s) and conditional GETs using `ETag`/`Last-Modified`.
  Validators are stored only after a body is read completely, and dropped when a body fails to parse.
- The cadence is `interval × (1 ± jitter)`, with sources seeded differently so they don't poll in lockstep. Failures back off exponentially up to `max_backoff_s`.
- One failing source never affects another. If the consumer drops the channel, every task stops.
- Per-source health tracks:
  - polls, failures and consecutive failures;
  - 304s, malformed payloads, unresolved counties and version regressions;
  - `last_ok_ns` and `last_new_data_ns`, which the executor can use for staleness.

### Configuration: `config/enr_sources.toml`

- `[defaults]`, `[consistency]`, `[[sources]]` (kind `ca_sos` / `clarity` / `static_json`, IANA `tz`, `enabled`) and `[[races]]` (a contest name as
  the source prints it mapped to our race id; the match ignores case and whitespace; `source = "*"` applies to all sources).
- It ships these sources:
  - **CA SoS**, bulk files, enabled;
  - **CA SoS**, REST API, disabled, as an independent second path for cross-checks;
  - **Clarity SC 2024** and **Clarity IA 2026 primary**, both disabled as references;
  - **Georgia Enhanced Voting 2024**, via the mapper, disabled.
- The race map covers CA governor and all 52 CA House districts under both spellings.
- **Before election night:** swap in 2026 election ids and contest names as each state publishes them, and add race rows for every
  market we trade. Contests that aren't mapped are ignored.

### `enr-capture` during the CA SoS test windows (runbook)

```bash
cargo build --release -p susq-enr --bin enr-capture
./target/release/enr-capture --config config/enr_sources.toml --out data/enr --duration-s 18000   # 8:00-13:00 PT
```

- Raw bodies are written to `data/enr/<source>/<UTC date>/raw/`, but only when they change.
- Batches go to `batches.jsonl` (one compact proto3 JSON line each) and pipeline events to `events.jsonl`.
- `last_health.json` is written on exit.
- Convert a capture to a replay by reading `batches.jsonl`: it is already in the replay line format.

---

## 5. Replays and drills (`py/susq/replay`)

### Replay file format

- `<name>.jsonl` holds one `CountyResultsBatch` per line in compact proto3 JSON. **All timestamps are ns relative to the anchor** (the earliest last-poll-close
  among the included states).
- `<name>.meta.json` holds the anchor UTC, seed, horizon, per-race truth (final and end-of-night D/R) and the inputs.
- The NATS publisher re-anchors timestamps to the wall clock at publish time.

### Synthetic timeline model

For each county and vote type, the final votes are released as 1 to 6 cumulative batches:
- The first batch arrives `first_report_min` after the state's last close. It is scaled by `(county votes / median)^0.25`, clipped to [0.6, 2.5], with log-normal noise (σ 0.35). Later batches are spaced by `spread_min` with σ 0.5.
- Batch fractions are Dirichlet. Each candidate's share of a batch varies around the type's final mix (concentration 150), so early batches mislead realistically, **but cumulative totals land exactly on the certified result**. A test asserts GA sums to 1,946,117 / 1,908,442.
- Only `night_completion` of each type is counted on the night. The rest arrives 20 to 72 h later, beyond the default 17 h horizon (7 p.m. ET to noon ET), which is what makes late-count races hard at the lock.

The per-state conventions in `conventions.py` are **modeling assumptions (low confidence), not measurements**. Calibrate them from captures:

| Convention | States | Order |
|---|---|---|
| mail/early first | GA NC FL AZ OH TX IA SC KS NE NV NH ME MI AK | Mail and early at +12–15 min; E-day from +45 min; provisional days later |
| E-day first, mail late | PA WI | Early and E-day first; mail from about +3 h (no pre-processing) |
| California | CA | Pre-E-day mail at about +8 min; E-day overnight; about 40 % of mail counted after the night |
| default | others | Mixed batches through the night |

### NeedlePrior builder

- **Shift:** `shift = prior_margin_mean − historical statewide two-party margin`. County prior = historical county margin + shift, so the county priors weighted by historical two-party votes reproduce the race prior exactly (tested).
- **Turnout:** `expected_total_votes = historical × turnout_ratio`. `expected_total_sd_frac = hypot(0.05 + 1.5/√expected, turnout_sd_frac)`, capped at 0.6.
- **County margin sd:** `prior_margin_sd = hypot(race sd, county sd)`, with county sd defaulting to 4 pts.
- **Vote types:** shares and mail/E-day margin offsets come from split history. `typical_report_order` comes from the conventions. Unsplit history gives one MIXED entry.

### Needle backends

- **C++** is used when `susq_models.needle.Needle` is importable. The contract for the C++ owner uses serialized protos at the boundary:
  `load_prior(bytes NeedlePrior)` and `ingest(bytes CountyResultsBatch) -> bytes RacePosteriors`. A missing module, submodule or class, or a constructor
  failure, is a **skip** with a reason, never an error.
- **Reference** (`needle_ref.py`) is a small pure-Python implementation of the method, used when the C++ binding is absent and as a cross-check. It:
  - estimates a precision-weighted race shift from county residuals against type-adjusted priors, adding a partial-count noise term that decays as a county completes;
  - projects the remaining vote per county and type with half of each county's residual persisting;
  - propagates shift, county and turnout variance.
  - It does not model the national shift; that is the C++ needle's job.
  - On the sample, it lands within 0.3 pts of the true margin by the end of the night across seeds (tested). On GA 2022 it was D+0.97 against a true D+0.98.

### Runner and chaos

- The runner streams at N× speed (0 = as fast as possible) either in process through a backend, or to NATS `enr.county_results` (`nats-py`), with timestamps
  re-anchored. Payloads are validated: decoding, header and schema version, FIPS format. `seq` gaps and resets are tracked.
- **Feed drop** `start:duration`:
  - Batches are withheld and heartbeats say `feed_ok = False`.
  - On recovery, the first batch carries every change made meanwhile, because the poller's next successful poll does exactly that. The final estimate is unaffected (tested).
- **Malformed injection** `every N`: garbage payloads before every N-th delivery. All are rejected; none are fatal.
- **Publisher kill** `start:restart_after`:
  - No batches and no heartbeats. On NATS, the connection is closed abruptly.
  - On restart, `seq` starts again at 1 and the first batch is a snapshot of every series' current cumulative result.
  - The consumer counts the seq reset and recovers exactly (tested).

### Drill harness: `reports/drills/<date>.md`

- **Latency:** wall time from payload arrival to posterior, and from arrival to the executor's order decision (p50, p95, max).
- **Executor rule** (SPEC Phase 3):
  - Buy side s when `q − ask ≥ max(8 pts, 3 × SE)` and at least 2 counties are reporting. SE is the standard deviation of q over the last 5 posteriors.
  - One position per race and side, held to resolution.
  - Orders are **suppressed while input is unhealthy**: no heartbeat or batch for 90 s, or the feed flagged down. It resumes after 3 healthy inputs, mirroring the needle staleness rule in LANGUAGES_AND_BOUNDARIES.
- **Market model:** the Cup YES(D) mid is our own posterior `market_delay_s` ago (the pre-election prior before that), plus a half-spread.
  This isolates the value of latency; it is not a claim about how the Sponsor quotes. With zero delay there is no edge and no trade (tested).
- **False triggers:** a posterior crossing 0.85 for a side that later drops below 0.5 or whose side loses in the truth.
- **P&L** is measured against the replay truth. The report also records chaos behaviour: rejected payloads, seq resets, kills and restarts, suppressed orders, and time spent unhealthy.

The first sample drill on 2026-09-28 used the reference backend, all three chaos modes and a 10-minute market lag.
- It made 4 trades for **+91.5** SUSQies (100-lot) with **0** false triggers.
- All 44 malformed payloads were rejected.
- There was 1 kill and 1 restart, with 25 minutes of unhealthy input.
- Arrival-to-posterior latency was 5 to 12 ms at p50 in Python on a laptop (it varies with machine load; the C++ needle should be far lower).
- A clean run gives the same trades and P&L, which confirms that the chaos modes cost this sample nothing once the feed recovers.

---

## 6. Open issues and what is not verified

- **CA SoS 2026 general test dates:** unverified (§1.1). The 2026 general endpoint CSVs are not posted yet.
- **2026 election ids** for Clarity (SC, IA) and Enhanced Voting (GA) are not published yet. The configs point at 2024 or primary references and are disabled.
- **Adapters for custom SoS sites** (AZ, NV, TX, OH, MI, KS, AK, NC zip, NE CSV) are not built. Their JSON must be captured first; CSV/zip sources need a small adapter.
- The **Clarity `istestmode` flag** is not read. Captured test data should be labelled by date rather than trusted.
- **Poll-closing table:** 33 states are unverified (GA and NH are the important ones).
- **Reporting conventions** are assumptions to calibrate from captures and from any timestamped archives we can find.
- **Cross-source tolerances** (2 %, 50 votes, 1 pt) are untuned. The CA bulk-versus-REST pair gives a free first calibration on the test days.
- **C++ needle binding:** not built yet (feat/cpp-models). The drill uses the reference backend until `susq_models.needle.Needle` exists with the contract above.

## Sources (all accessed 2026-09-28)

**California:**
- Media guides: https://cms.cdn.sos.ca.gov/media/1-media-guide-v16.pdf, https://cms.cdn.sos.ca.gov/media/1-media-guide-v20.pdf
- ASCII file format: https://cms.cdn.sos.ca.gov/media/2-ascii-file-format-v20.pdf
- API documentation: https://cms.cdn.sos.ca.gov/media/3-api-documentation-v20.pdf
- Testing schedules: https://cms.cdn.sos.ca.gov/media/4-testing-schedule-v20.pdf and https://cms.cdn.sos.ca.gov/media/4-testing-schedule-v18.pdf
- References: https://cms.cdn.sos.ca.gov/media/5-references-v20.pdf
- Media pages and endpoints: https://www.sos.ca.gov/media, https://media.sos.ca.gov/ (`media/json-endpoints.csv`, `media/api-endpoints.csv`, `media/*.json`), https://api.sos.ca.gov/returns/…

**Clarity:**
- https://results.enr.clarityelections.com/WV/122766/356048/…
- https://results.enr.clarityelections.com/GA/Fulton/121874/343653/…
- https://results.enr.clarityelections.com/CO/105975/276916/reports/detailxml.zip
- https://www.enr-scvotes.org/SC/122436/359624/…
- https://electionresults.iowa.gov/IA/elections.json

**Georgia:** https://results.sos.ga.gov/results/public/Georgia/elections/2024NovGen and `/results/public/api/elections/Georgia/2024NovGen/data/ballot-item/…`

**Other state sites:**
- https://er.ncsbe.gov/
- https://electionresults.nebraska.gov/ResultsMedia.aspx
- https://results.texas-election.com/races
- https://sos.ks.gov/media/press-releases/2024/11-05-24-schwab-reminds-kansans-of-election-night-reporting-webpage.html
- https://www.elections.alaska.gov/doc/mp/2024/MediaPacket-2024-Primary.pdf
- https://liveresults.ohiosos.gov/
- https://mielections.us/election/results/
- https://silverstateelection.nv.gov/
- https://azsos.gov/elections/election-information/unofficial-enr

**Poll closing:**
- Per-state statute and office URLs are in `config/poll_closing.toml`.
- https://www.270towin.com/poll-closing-times
- https://www.270towin.com/content/local-poll-closing-times-november-2026-general-election

**Historical results:**
- https://dataverse.harvard.edu/dataset.xhtml?persistentId=doi:10.7910/DVN/VOQCHQ
- doi:10.7910/DVN/YB60EJ, doi:10.7910/DVN/UYQIEP, doi:10.7910/DVN/NYTPDU, doi:10.7910/DVN/IAD3XR, doi:10.7910/DVN/XDJYKC
- https://github.com/MEDSL/2022-elections-official
- https://github.com/MEDSL/2024-elections-official
- https://github.com/tonmcg/US_County_Level_Election_Results_08-24
- https://www.maine.gov/sos/elections-voting/election-results-data/election-results-2024

**County FIPS:** https://www2.census.gov/geo/docs/reference/codes2020/national_county2020.txt (public domain)
