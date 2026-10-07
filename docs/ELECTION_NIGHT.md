# Election night: data layer, replays and drills

*Written 2026-09-28; the battleground feeds (§1.4), poll closing (§2) and open issues (§6) were updated
2026-10-06. Everything marked "verified" was fetched or tested on the date given. Anything not verified
says so.*

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

# The 2026 prior: history for every Cup state, the spec from the fair file, then the NeedlePrior (§5)
uv run python -m susq.replay download 2022 --market-map state/market_map.json --out data/replay/county_results_2022.csv
uv run python -m susq.replay download 2024 --market-map state/market_map.json --out data/replay/county_results_2024.csv
uv run python -m susq.replay spec2026 --market-map state/market_map.json --fair state/fair_live.json \
    --history data/replay/county_results_2022.csv --history data/replay/county_results_2024.csv \
    --out py/susq/replay/specs/general_2026.toml
uv run python -m susq.replay prior py/susq/replay/specs/general_2026.toml --out state/needle/prior.json
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

### 1.3 Georgia: Enhanced Voting (verified live on the 2026 primary; 2026 general ids auto-discovered)

These are the endpoints the results site itself calls, under `https://results.sos.ga.gov/results/public/api`. They are
unauthenticated but **not a published API**, so paths and ids can change. All were checked on 2026-09-28 with plain GETs.

| Endpoint | Returns |
|---|---|
| `/jurisdictions/Georgia` | `elections[]` (117 on 2026-09-28), each `{publicElectionId, electionDate, name[], dateText[]}` |
| `/elections/Georgia/{publicElectionId}` | election metadata (`isProduction`, `isOfficialResults`, `lastUpdated`, …); 404 for an unknown id |
| `/elections/Georgia/{publicElectionId}/data` | `{election, ballotItems[], localityElections[], …}`, about 1.7 MB for the primary; **204 with no body** for an unknown id |
| `/elections/Georgia/{publicElectionId}/data/ballot-item/{ballotItemId}` | the same document plus `ballotItemWithBreakdown.breakdownResults[]`, one per county, each with `ballotOptions[].groupResults[]` per vote method (Advance(d) Voting, Election Day, Absentee by Mail, Provisional) and `precinctReportingStatus`; about 2.5 MB; **200 with `ballotItemWithBreakdown: null`** for an unknown ballot item |

What we learned from the live data:
- Election slugs are ad hoc (`2024NovGen`, `GeneralPrimary51926`, `06162026GeneralPrimaryRunoff`), and ballot-item ids are
  GUIDs minted per election. Neither can be configured ahead of time. **The Nov 3 2026 general is not listed yet**, and
  guessing `2026NovGen` returns 404.
- Two elections can share a date: on 2026-05-19 the list has both the general primary and a PSC District 3 recount.
- Contest names differ by election type. The primary has "US Senate - Rep" and "Governor - Dem"; the general should
  print "US Senate" and "Governor" (the 2024 general printed "President of the US").
- 7 of 159 counties in the primary Senate item publish candidate totals with **every vote method `null`**. The mapper
  emits the unsplit remainder as `MIXED` rather than failing the payload (it used to fail).
- Responses carry `cache-control: public, max-age=60` and no `ETag`, so conditional GETs rarely help here.

**The `enhanced_voting` source kind** (`crates/enr/src/adapters/enhanced_voting.rs`) is configured by *what* to follow and
finds *where* at runtime:

1. It polls the jurisdiction list until an election with exactly `election_date` appears. Until then its health says
   `discovery.phase = "awaiting_election"` and it makes one small request per `discovery_interval_s` (600 s).
2. It fetches `/data` for every election on that date and follows the one whose `ballotItems[]` match the most configured
   contest names. Matching is exact after folding case and whitespace; there is no fuzzy matching. Unmatched contests
   are logged and skipped. A `/data` that is still empty gives `awaiting_data`, and no matches at all gives
   `no_matching_contests`.
3. It polls each resolved ballot item through the shared HTTP layer, which does conditional GETs, and through the
   service's jittered backoff. Each item is mapped with the `[sources.mapping]` block, the same `candidate_breakdown`
   pivot as before.
4. It re-discovers every `discovery_interval_s`. A 404, a 204 or a payload that is not the expected ballot item requests
   an early re-discovery, rate-limited to once a minute. A changed ballot-item id is counted in
   `discovery.ballot_item_id_changes` and followed. If one of several same-day elections fails, the others still resolve.
5. Every ballot-item payload's `election.isProduction` / `isOfficialResults` / `lastUpdated` is copied into the source's
   health entry. `discovery.production_verified` is true only when the site explicitly says `isProduction: true`, so
   test data is labelled and never passes as real returns. With `require_production = true`, payloads not flagged as
   production are withheld and counted in `withheld_test_payloads`.

The fixture tests (`crates/enr/tests/enhanced_voting.rs`) use payloads trimmed from the live site. They cover discovery
found, not yet published, an election that appears later, a changed id, 404 and stale-payload re-discovery, name
normalization, unmatched contests, test-data labelling and an end-to-end service run. They assert official Fulton
County numbers per method: Dooley advance 10,797, E-day 10,711, mail 226, provisional 15. The 2024 fixture still checks
Harris advance 297,293 and E-day Trump 25,789.

Live verification (2026-09-28):
- The shipped config (`2026-11-03`, "US Senate", "Governor") reports `awaiting_election` gracefully.
- A temporary config for the primary (`2026-05-19`, "US Senate - Rep" and "Governor - Dem", plus a deliberately unmatched
  "US Senate") chose `GeneralPrimary51926` over the same-day recount, logged "US Senate" as not on the ballot, and emitted
  1,236 county results. That is 159 counties per contest: 152 Senate and 154 governor counties × 4 methods, and 7 and 5
  unsplit counties as `MIXED`. Health showed `production_verified: true`.

### 1.4 Battleground results sites (re-checked 2026-10-06)

Every endpoint below was fetched on 2026-10-06 with a browser User-Agent from a laptop, unless the row says otherwise.
"Wayback" means the payload came from a web.archive.org capture because the live site refused scripted requests.
The source ids are the `[[sources]]` entries in `config/enr_sources.toml`, where each one carries a dated comment listing what was tried.

| State | Source id | Endpoint | Kind | Fixture-tested | Enabled | 2026 races mapped |
|---|---|---|---|---|---|---|
| CA | `ca_sos` | media.sos.ca.gov bulk JSON (§1.1) | `ca_sos` | yes | yes | GOV, 52 House |
| GA | `ga_sos` | results.sos.ga.gov API, discovered by date (§1.3) | `enhanced_voting` | yes | yes | SEN, GOV |
| NC | `nc_sbe` | `s3.amazonaws.com/dl.ncsbe.gov/ENRS/2026_11_03/results_pct_20261103.zip` (live, already posted with zeros) | `csv` | yes (2024 general, 2026 file) | yes | SEN |
| TX | `tx_sos` | Civix `goelect.txelections.civixapps.com/api-ivis-system/api/s3/enr/election/countyInfo/53815` (live) | `static_json` + `unwrap_base64` | yes (2024 legacy site, 2026 primary on Civix) | yes | SEN, GOV |
| AZ | `az_sos` | `cdn1.arizona.vote/data/69/0/election_69_0.json` gives the upload id for `all_county_races_69_0_en_{version}.json` (live, no challenge) | `static_json` + `version_url` | yes (2024 general) | no: `election_69_0.json` is 404 until the first upload | GOV |
| MI | `mi_sos` | MVIC `mvic.sos.state.mi.us/VoteHistory/GetElectionResultFile?electionId=<id>` (Cloudflare challenge; Wayback) | `csv` | yes (Aug 2024 primary) | no: challenge, and no 2026 general id | SEN, GOV |
| OH | `oh_sos` | `liveresults.ohiosos.gov/Api/v1/Contest/GetContestDetailsObject?...&resultsLevel=County` (Cloudflare error 1006; Wayback) | `static_json` | yes (2024 President, captured mid-count) | no: refused, and no 2026 contest ids | SEN, GOV (no markets) |
| SC | `clarity:SC` | `enr-scvotes.org/SC/<id>` | `clarity` | yes (2024) | no: the 2026 general id is not published | none yet |
| IA | `clarity:IA` | `electionresults.iowa.gov/IA/<id>` | `clarity` | no (2026 primary id only) | no: the 2026 general id is not published | none yet |
| NE | none | `electionresults.nebraska.gov/resultsCSV.aspx` is statewide and per district only | none | no | no | none |
| NV | none | `silverstateelection.nv.gov/_xml/USandNV.xml` (Incapsula challenge; Wayback) | none | no | no | none |
| KS | none | `ent.sos.ks.gov/kssos_ent.html`, HTML only (Cloudflare challenge; Wayback) | none | no | no | none |
| AK | none | `elections.alaska.gov/enr26/results/GA_ENR_Precinct_State_of_Alaska.csv` (live), but by house district | none | no | no | none |
| ME, NH, PA, WI | none | no statewide live feed found | none | no | no | none |

**What each state's feed looks like:**

- **North Carolina.**
  - The file is a zipped, tab-separated, CRLF text file with one row per (precinct, contest, choice) and one column per vote method: `Election Day`, `Early Voting`, `Absentee by Mail`, `Provisional`.
  - The `csv` kind sums the rows to counties.
  - Precincts reporting is a proxy: it counts the distinct precincts with `Real Precinct = Y` that have at least one vote in the contest. This leaves out the ABSENTEE, EARLY VOTING, PROVISIONAL and TRANSFER pseudo-precincts.
  - The 2026 general file already exists with all counts at zero. Its `US SENATE` contest lists Cooper, Whatley, Bray and Dublin.
  - The fixture asserts official 2024 totals for Hyde, New Hanover and Watauga.
  - A live `enr-capture` run emitted 400 results (100 counties × 4 methods) on its first poll and got a 304 on the second.
- **Texas.**
  - Since 2025 results run on Civix. Its API answers plain GETs but wraps every document as `{"upload": "<base64 JSON>"}`, has no ETag, and takes 9 to 11 s to download the 1.8 MB document. That is why `tx_sos` has a 30 s timeout and polls every 45 s.
  - The 2026 general is election 53815. Today its county document holds 2025 propositions at zero, so nothing maps yet.
  - The schema is the same as the 2024 legacy site's `County.json`. The only difference is that the contest name is in `N` rather than `ON`.
  - `EV` (early in-person plus mail) is published per candidate, but results are tagged `MIXED` because the mapper cannot yet split one total into two scalars.
  - Precincts come from `Summary.PRR` and `Summary.PRP`. Their meaning is inferred, not documented.
  - A live capture made 2 polls, both successful, with 0 results.
- **Arizona.**
  - The Cloudflare-protected results.arizona.vote reads from Azure blob storage on `cdn1.arizona.vote`, which has no challenge and sends an ETag.
  - The county file is a flat array with one row per (county, contest, candidate). The party is inside the name (`"Gallego, Ruben (DEM)"`), so the mapping uses `merge_rows` and `party_from_name`.
  - There is no vote-method split per county, so results are `MIXED`.
  - Per the site, precincts reporting counts Election Day locations only.
  - A live capture against the 2024 general (eid 47) emitted all 15 counties and then got 304s.
- **Michigan.** MVIC's result file is county-level and tab-separated, with a trailer line (`RECORDS: n<TAB>RESULTS: ...`). It has no precinct counts and no method split.
- **Ohio.** The contest API returns county rows with precinct counts but no method split. A candidate's `totalVotes` field is the county's contest total; the candidate's own votes are in `vCount`.
- **Nebraska.** County numbers exist only as HTML county pages (`resultsSW.aspx?type=CTYALL&map=CTY&cty=NN`) and as an xlsx export behind an ASP.NET postback. The 2024 general's final CSV (Wayback, 2025-01-19) has no county rows either.
- **Nevada.** The XML puts one `<County>Votes` attribute per county on each candidate. It has no method split, and the 2026 general is not posted.
- **Kansas.** County numbers are inside `<area title=...>` HTML. There is no machine-readable feed.
- **Alaska.** The precinct CSV splits by method but has no borough, so it cannot produce FIPS-keyed results until Alaska is modelled by district.

---

## 2. Poll closing times: `config/poll_closing.toml`

- The table covers all 50 states. Per state it lists the closings per time zone (the **last entry is the statewide last close**), local time, UTC time for
  Tue 2026-11-03, the source URL, a `split_time_zone` flag and a `verified` flag.
- DST ends Nov 1, so the offsets are EST −5, CST −6, MST −7 (AZ all year), PST −8, AKST −9, HST −10. `py/tests/replay/test_closing.py`
  recomputes every UTC value from IANA tzdata.
- **Verified from statutes or election offices (20):** AK, AZ, CA, FL, GA, IA, ID, KS, ME, MI, NC, ND, NE, NV, OH, OR, PA, SC, TX, WI.
  GA, PA and WI were verified on 2026-10-06, and AK, AZ, IA, KS, MI, NC, NE, NV, OH, SC and TX were re-checked that day. Each checked state has a
  `# Verified 2026-10-06 from <url>` comment. TX, OH, NC, KS and AK now cite pages that answer (statute or votetexas.gov), because
  sos.state.tx.us and ohiosos.gov return 403.
- **Unverified (30, from 270toWin's 2026 schedule):** includes NH and NY. For NH, RSA 659:4 sets only a floor (polls close "not earlier than
  7 o'clock"; city councils set city hours), so the 7 p.m. row is official but the 8 p.m. row for later-closing towns is not, and the flag stays false.
- **Split time zones:** FL panhandle, IN, KY, TN, MI (4 UP counties), TX (El Paso and Hudspeth), KS (4 western counties), NE, ND, SD, ID, OR (Malheur) and AK (western Aleutians).
- **Caveats:** under K.S.A. 25-106 Kansas Central-zone counties default to 7 p.m. and may run to 8 p.m.; the four Mountain-zone counties default
  to 6 p.m. MT and may run to 7 p.m. MT. The table stores 7 p.m. for both (the Central default and the Mountain maximum). ND precincts may close
  as late as 9 p.m.
- **Action:** find an official list of NH town closing hours before Oct 26 (sos.nh.gov returned 403 on 2026-09-28 and 2026-10-06).

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

- `python -m susq.replay download <year> [<states…>] [--market-map state/market_map.json] [--house]` pulls the GitHub precinct zips and aggregates them to county CSV under `data/replay/` (gitignored).
  - `--market-map` adds every state with a Cup Senate or Governor race (44 states on Oct 6).
  - Each state is cached as `data/replay/medsl/<year>/<ST>.csv` and reused unless `--refresh`. A state that fails is reported and the command exits non-zero, but the merged CSV still holds every state that worked. A state with no Senate, Governor or President race that year (DE, MS, MT, NJ, TN, VA, WV in 2022) is listed, not failed.
  - It keeps general-stage rows only.
  - It drops non-candidate rows: undervotes, overvotes, blanks, voids, and the ballot counts some states file as candidates (NY "PUBLIC COUNTER", 3.3M "votes" in 2022; MI "BALLOTS CAST"; NJ "CONTEST TOTAL"). The loader drops the same labels again, so older CSVs read correctly.
  - It drops rows without a FIPS code ("STATE FLOATING"). Alaska has no counties: its rows are keyed by state house district (`jurisdiction_fips`, 02001-02040).
  - Suppressed vote cells (`*`, in 2024 CA, KS, NM, NV) count as 0.
  - Special elections are kept apart (`US SENATE SPECIAL`: OK 2022 ran two Senate races).
  - Party: `party_simplified`, falling back to `party_detailed` where MEDSL simplified a major party to OTHER (MN DFL, NE 2024 President). The loader also counts fusion lines (NY Working Families, Conservative) for the candidate's major party.
  - Statewide offices by default. `--house` also keeps US House rows by county × district (`district = "07"`); the needle cannot use them yet (§6).
- **Which states split by method:**
  - 2022 Senate: AK AR AZ GA HI IA ID LA MD NC NY OK SC (WA only has write-in rows).
  - 2024 president: AK AL AR AZ CT DE HI IA IN MD NC NJ NY OK RI SC (WV has one label only).
  - **Georgia 2024 is TOTAL-only in MEDSL**; its method split comes from the GA SoS API.
- **Double-count trap:** some states publish a TOTAL row next to the split rows (GA 2022, AZ, ID, SC, NY, CT, DE, IN, NJ, RI). The loader drops TOTAL wherever split rows exist.

**Fixture** (`py/tests/replay/fixtures/county_results_sample.csv`, 2,142 real rows, 209 KB):
- GA 2022 US Senate: 159 counties × 4 methods, from MEDSL. It matches the SoS certified totals exactly (Warnock 1,946,117, Walker 1,908,442, Oliver 81,365), and the test asserts this.
- NH 2022 Senate and 2024 president: towns summed to counties.
- ME 2022 Governor and 2024 president.

**Coverage on Oct 6** (all 44 Cup states downloaded for both years; no failures):

| | 2022 | 2024 |
|---|---|---|
| States with a Senate, Governor or President race in the file | 37 | 44 |
| No statewide race in the MEDSL file | DE MS MT NJ TN VA WV (TN held a Governor race, but the MEDSL file lacks it) | none |
| Known source mislabels | CA has no Senate rows; NM has no party labels; VT Governor's Democrat is OTHER; WY Governor's Democrat is LIBERTARIAN | NE Senate's Osborn (independent) is DEMOCRAT; NE's special Senate race is missing |

The spec builder skips a contest where either side has under 15% of the two-party vote, so the mislabels fall through to the
next baseline instead of producing a −100 prior.

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
   - `static_json`: a jq-like path mapper over any JSON, with `^N` ancestor bindings and a per-candidate vote-type **breakdown pivot**, configured entirely in TOML. A breakdown whose methods are all `null` falls back to the candidate total as `MIXED`.
     Options added on 2026-10-06 for the battleground feeds:
     - `version_url` and `version_path` read a version document first and substitute `{version}` into the urls. A lower numeric version is refused (AZ).
     - `unwrap_base64` maps a base64 JSON document nested in the payload (TX Civix).
     - `merge_rows` sums flat one-candidate-per-row feeds per county (AZ).
     - `party_from_name` reads a trailing `(DEM)` from the candidate name (AZ).
   - `csv`: CSV or TSV, plain or zipped (zip is detected by magic bytes), mapped by header name.
     - Rows are summed to (contest, county). Vote methods come from one column per method (`vote_type_columns`) or from a label column.
     - Precincts come from explicit columns or a distinct-precinct proxy (`precinct`, `precinct_flag`).
     - `skip_rows_starting_with` drops trailer lines.
     - A missing header or a bad count fails the payload. Used for NC and MI.
   - `enhanced_voting`: discovers the election by date and the ballot items by contest name, then maps each item with the same mapper (§1.3).
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
   - A violation marks the race **disputed** in `EnrHealth.disputed_races` until it resolves. **The election-night executor refuses disputed races**: rust-live reads the health channel every tick (`crates/live/src/enr_health.rs`), the `EnExecutor` places nothing on a disputed race and cancels its own resting orders there, and the dispute appears in *events* and the `enr` health tile.
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
  - `last_ok_ns` and `last_new_data_ns`. rust-live turns these into one `enr:<source>` health tile per source and the executor's
    stale-feed gate: a source is stale once `last_ok_ns` is older than `[strategies.en] feed_stale_s` (180 s), it has failed three polls
    in a row, or it has never polled; a race whose mapped sources (`[[races]]` rows and enhanced-voting `contests`) are all stale gets
    no new buys, while held legs are still rolled;
  - for `enhanced_voting` sources, a `discovery` block: phase, election id and name, contest to ballot-item ids, the site's
    `isProduction` / `isOfficialResults` flags, `production_verified`, and discovery counters.

### Configuration: `config/enr_sources.toml`

- `[defaults]`, `[consistency]`, `[[sources]]` (kind `ca_sos` / `clarity` / `static_json` / `csv` / `enhanced_voting`, IANA `tz`, `enabled`) and `[[races]]` (a contest name as
  the source prints it mapped to our race id; the match ignores case and whitespace; `source = "*"` applies to all sources).
- It ships these sources:
  - **CA SoS**, bulk files, enabled;
  - **CA SoS**, REST API, disabled, as an independent second path for cross-checks;
  - **Clarity SC 2024** and **Clarity IA 2026 primary**, both disabled as references;
  - **Georgia Enhanced Voting** (`ga_sos`, kind `enhanced_voting`), enabled. It targets 2026-11-03 for "US Senate"
    (`2026-SEN-GA`) and "Governor" (`2026-GOV-GA`) and discovers the election and its ballot items itself (§1.3).
    A commented `ga_sos_runoff` block is ready for the Dec 1 general runoff.
  - **North Carolina** (`nc_sbe`, `csv`) and **Texas** (`tx_sos`, `static_json` on Civix), both enabled, for 2026-SEN-NC, 2026-SEN-TX and 2026-GOV-TX.
  - **Arizona** (`az_sos`), **Michigan** (`mi_sos`) and **Ohio** (`oh_sos`), configured and fixture-tested but disabled. Each has a dated
    comment saying what has to change before it can be enabled.
  - Nebraska, Nevada, Kansas and Alaska have no source yet. Their comment blocks record what was tried (§1.4).
- The race map covers CA governor and all 52 CA House districts under both spellings.
- **Before election night:** swap in 2026 election ids and contest names as each state publishes them, and add race rows for every
  market we trade. Contests that aren't mapped are ignored. Georgia is the exception: its ids are discovered, so only its
  contest names need checking.

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

### The 2026 spec (`spec2026.py`, `specs/general_2026.toml`)

`python -m susq.replay spec2026` writes a spec that `load_spec` reads, one race per Cup Senate or Governor race id in the market
map (69 on Oct 6). It never copies anything from the map except race ids. The generated file is committed with a header naming
the command, the run date and the fair file's timestamp. The pre-flight regenerates it on the VM.

- **Prior margin from the fair file.** The left side is the Democrat, or the independent where the independent out-prices the
  Democrat (`calibrate.consensus.left_party`: NE and MT Senate on Oct 6). `p_left = p_left / (p_left + p_rep)` removes the two-way
  overround. The sim maps a margin to a win probability through the race's unconditional margin SD, and the needle reads
  `prior_margin_sd` as the race's total prior SD, so:

  ```
  prior_margin_sd   = analytic_margin_sd(race spec, SimParams())   # 5.13 pts; 5.34 with a named cluster; 5.77 for an independent
  prior_margin_mean = prior_margin_sd × Φ⁻¹(p_left)                 # p clipped to [0.001, 0.999]
  national_shift_prior_sd = sqrt(national_sd² + miss_p × miss_shift²) = 2.96
  ```

  With no returns the needle then says `Φ(mean / sd)`, which is the fair value.
- **Saturated prices.** Beyond 95/5 a price says "safe" but not how safe: 0.99 is 2.3 SD, about 12 points, while Wyoming is 40+.
  A race whose prior is 30 points too close would pull the needle's shared national shift as soon as it reports. So when
  `|Φ⁻¹(p)| ≥ 1.645` and the historical margin points the same way and is larger, the historical margin is used (30 of 69 races on
  Oct 6). The race SD is unchanged, so those races sit further from 50/50 than the fair value says, which only matters for races
  nobody trades.
- **Baseline contest.** The same-state statewide contest closest to a 2026 midterm: 2022 same office, 2022 other office, 2024
  same office, 2024 President. A contest is skipped if either side has under 15% of the two-party vote. Independent-led races pool
  the leading independent with the Democrat (`history.pool_independent`, the needle's "side A"). They prefer a contest where an
  independent ran strongly, then any same-office contest.
- **Turnout ratio.** 1.0 for a 2022 baseline (midterm to midterm). For a 2024 baseline it is the state's 2022 total over its 2024
  total, or the median of that ratio (0.752 over 37 states) when the state has no 2022 race. The reason is written next to each race.
- **Poll close** is the state's last close from `config/poll_closing.toml`, pinned per race (`poll_close_utc`).
- **Check.** `weighted_prior_margin` (county priors weighted by expected total votes) matches each race's prior margin within 0.3
  points for 66 of 69 races. The worst are AK (1.5 points; Walker's 20% independent vote in 2022 is uneven across districts) and
  ID (1.2; counties clipped at −100). The needle re-centres the county priors on the race prior when it loads, so this is a
  sanity check, not an error in what the needle uses.

**Races and baselines on Oct 6:**

| State | 2022 history | 2024 history | 2026 race: baseline |
|---|---|---|---|
| AK | GOV SEN | PRES | GOV: 2022 GOV; SEN: 2022 GOV (2022 Senate was R vs R) |
| AL | GOV SEN | PRES | GOV: 2022 GOV; SEN: 2022 SEN |
| AR | GOV SEN | PRES | GOV: 2022 GOV; SEN: 2022 SEN |
| AZ | GOV SEN | PRES SEN | GOV: 2022 GOV |
| CA | GOV | PRES SEN | GOV: 2022 GOV |
| CO | GOV SEN | PRES | GOV: 2022 GOV; SEN: 2022 SEN |
| CT | GOV SEN | PRES SEN | GOV: 2022 GOV |
| DE | - | GOV PRES SEN | SEN: 2024 SEN |
| FL | GOV SEN | PRES SEN | GOV: 2022 GOV; SEN (special): 2022 SEN |
| GA | GOV SEN | PRES | GOV: 2022 GOV; SEN: 2022 SEN |
| HI | GOV SEN | PRES SEN | GOV: 2022 GOV |
| IA | GOV SEN | PRES | GOV: 2022 GOV; SEN: 2022 SEN |
| ID | GOV SEN | PRES | GOV: 2022 GOV; SEN: 2022 SEN |
| IL | GOV SEN | PRES | GOV: 2022 GOV; SEN: 2022 SEN |
| KS | GOV SEN | PRES | GOV: 2022 GOV; SEN: 2022 SEN |
| KY | SEN | PRES | SEN: 2022 SEN |
| LA | SEN | PRES | SEN: 2022 SEN |
| MA | GOV | PRES SEN | GOV: 2022 GOV; SEN: 2022 GOV |
| MD | GOV SEN | PRES SEN | GOV: 2022 GOV |
| ME | GOV | PRES SEN | GOV: 2022 GOV; SEN: 2022 GOV |
| MI | GOV | PRES SEN | GOV: 2022 GOV; SEN: 2022 GOV |
| MN | GOV | PRES SEN | GOV: 2022 GOV; SEN: 2022 GOV |
| MS | - | PRES SEN | SEN: 2024 SEN |
| MT | - | GOV PRES SEN | SEN: 2024 SEN, independent pooled |
| NC | SEN | GOV PRES | SEN: 2022 SEN |
| NE | GOV | PRES SEN | GOV: 2022 GOV; SEN: 2024 SEN (Osborn), independent pooled |
| NH | GOV SEN | GOV PRES | GOV: 2022 GOV; SEN: 2022 SEN |
| NJ | - | PRES SEN | SEN: 2024 SEN |
| NM | GOV (no parties) | PRES SEN | GOV: 2024 PRES; SEN: 2024 SEN |
| NV | GOV SEN | PRES SEN | GOV: 2022 GOV |
| NY | GOV SEN | PRES SEN | GOV: 2022 GOV |
| OK | GOV SEN SEN-special | PRES | GOV: 2022 GOV; SEN: 2022 SEN |
| OR | GOV SEN | PRES | GOV: 2022 GOV; SEN: 2022 SEN |
| PA | GOV SEN | PRES SEN | GOV: 2022 GOV |
| RI | GOV | PRES SEN | GOV: 2022 GOV; SEN: 2022 GOV |
| SC | GOV SEN | PRES | GOV: 2022 GOV; SEN: 2022 SEN |
| SD | GOV SEN | PRES | GOV: 2022 GOV; SEN: 2022 SEN |
| TN | - | PRES SEN | GOV: 2024 PRES; SEN: 2024 SEN |
| TX | GOV | PRES SEN | GOV: 2022 GOV; SEN: 2022 GOV |
| VA | - | PRES SEN | SEN: 2024 SEN |
| VT | GOV SEN | GOV PRES SEN | GOV: 2022 SEN (the 2022 Governor file has no Democrat) |
| WI | GOV SEN | PRES SEN | GOV: 2022 GOV |
| WV | - | GOV PRES SEN | SEN: 2024 SEN |
| WY | GOV | PRES SEN | GOV: 2024 PRES; SEN: 2024 SEN |

**Not covered:** every House race (46 on Oct 6), for the reason in §6. Every Senate and Governor race in the map has a prior.

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

**Pre-election pacing.** Until `[defaults] idle_until_utc` (Nov 3 21:00Z = 16:00 ET) every source polls at most every
`idle_interval_s` (30 min), so weeks of paper running stay polite to state sites. The rust-live enr tiles therefore read
stale until then; that is expected. `enr-capture` ignores the schedule.

## 6. Open issues and what is not verified

- **House races have no needle prior.** The needle keys county priors by (race, county FIPS), and most competitive districts
  split counties (CA-22 and CA-48 both do), so a district cannot be modelled from county history: the feed's county totals
  would mix two districts. `download --house` already keeps MEDSL House rows by county × district. What is missing is a
  county × district key in the ENR feed (`CountyResult`), in `CountyPrior` and in the needle. Until then `spec2026` lists the
  46 House races as uncovered and the executor gets no House posteriors.
- **Baselines are a different candidate pair.** A 2026 Senate race often uses the 2022 Governor or the 2024 Senate race as its
  county pattern (see the table in §5). The race-level shift absorbs the statewide gap but not a home-county or incumbency
  pattern (VT Governor uses Welch's 2022 Senate map for a Scott race).
- **Prior margins go stale.** The committed spec uses the Oct 1 fair file. The pre-flight must regenerate it from the current
  `state/fair_live.json` on Oct 31 and Nov 3 (RUNBOOK).
- **Alaska** history is keyed by state house district (02001-02040); the AK priors only work with a feed that reports by the
  same code, and no AK source is built.

- **CA SoS 2026 general test dates:** unverified (§1.1). The 2026 general endpoint CSVs are not posted yet.
- **2026 election ids** for Clarity (SC, IA) are not published yet.
  - On 2026-10-06 SC's `elections.json` was `[]`, and IA's listed only the 2026 primary and the 2025 specials.
  - Ids up to 127800 all return 404 on both hosts.
  - Both configs stay disabled and point at 2024 or primary references. Re-check from Oct 20, then set the id and add `[[races]]` rows.
  Georgia no longer needs manual ids: `ga_sos` discovers the Nov 3 general by date (§1.3). What is not verified is the general's
  exact contest names ("US Senate" and "Governor" are assumed from the 2024 and primary naming). Once the SoS lists the election,
  check `discovery.contests` in `last_health.json` for `null` entries.
- **Georgia test data:** `ga_sos` ships with `require_production = true`. SoS test payloads (not flagged `isProduction`) are
  held off the bus and counted in health. If Georgia ever mislabels real returns, the held counter climbs and the health
  panel shows it; flip the flag only after checking `last_health.json` (`discovery.production_verified`).
- **Custom SoS sites (status on 2026-10-06; details in §1.4):**
  - **Live and enabled:**
    - NC (`csv`).
    - TX (`static_json` on Civix). Contest names come from the 2026 primary, and the precinct fields are inferred.
  - **Mapped and fixture-tested but disabled:**
    - AZ: enable once `cdn1.arizona.vote/data/69/0/election_69_0.json` returns 200. The contest name "Governor" is assumed.
    - MI: needs a Cloudflare clearance and a 2026 general election id.
    - OH: Cloudflare 1006 refuses scripted clients, and the 2026 contest ids are not published.
  - **Not buildable as things stand:**
    - NE: no county-level machine-readable feed.
    - NV: Incapsula challenge plus an XML attribute layout.
    - KS: HTML only, behind a challenge.
    - AK: house districts, not counties.
  - Feeds without a vote-method split (AZ, TX, MI, OH) are tagged `MIXED`. TX's `EV` early-vote figure could be split out with a small mapper
    extension.
  - A host that is refused by Cloudflare or Incapsula would need a browser-issued cookie, which this crate does not do.
- The **Clarity `istestmode` flag** is not read. Captured test data should be labelled by date rather than trusted.
- **Poll-closing table:** 30 states are unverified. GA, PA and WI were verified on 2026-10-06. NH stays unverified because only its 7 p.m. floor is official.
- **Reporting conventions** are assumptions to calibrate from captures and from any timestamped archives we can find.
- **Cross-source tolerances** (2 %, 50 votes, 1 pt) are untuned. The CA bulk-versus-REST pair gives a free first calibration on the test days.
- **C++ needle binding:** built (`cpp/bindings/needle_bindings.cpp`, in the python image through the `models` dependency group) and
  used by the drill whenever `susq_models.needle.Needle` imports, with the reference backend as the fallback and cross-check. What a
  binding drill does **not** exercise is the `susq-needle` service itself (NATS, headers, seq, heartbeats, the rust-live watchdog); the
  service image has not yet been built or run on the VM, which the Oct 8 smoke and both rehearsals must cover.
- **Posterior cadence and the executor's gates:** the needle publishes posteriors only after an accepted batch; rust-live keeps acting on
  the last one for `[needle] posterior_max_age_s` (600 s) while heartbeats keep the needle healthy. With `--election` the executor also
  refuses disputed races and places no new buys where every mapped source is stale (`[strategies.en] feed_stale_s`); without the pollers
  (a replay published straight onto the bus) neither gate applies. Both gates are unit-tested and have not yet run against the real stack.
- **Buy threshold and the night budget:** a buy needs `q − ask ≥ max(min_edge_pts, se_mult × SE)`. SE now comes from the posterior's own
  margin by the delta method, `phi(margin_mean / margin_sd) × se_margin_err_pts / margin_sd` (the move in `q` for a one-point model error),
  so a narrow lead at a tight sd is treated as fragile however many batches are in, and a race called by several sd trades on the 8-point
  floor; a posterior without `margin_sd` falls back to `sqrt(q(1−q)/(1+n_batches))`. Risk caps what needle-derived buys may have deployed at
  once at `[strategies.en] night_budget_frac` of equity (bought or resting at their limit, less roll proceeds). After a restart, closed
  election-night orders are counted as fully bought, so the cap can only bind harder. `crates/live/tests/session.rs` drives a real
  `NeedleState` into the engine: a posterior with edge produces a filled buy on the mock exchange, heartbeat silence cancels the working
  order and blocks new ones despite fresh size on the book, and trading resumes only after the resume streak and a fresh posterior.

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

**Georgia:** https://results.sos.ga.gov/results/public/Georgia/elections/2024NovGen and `/results/public/api/elections/Georgia/2024NovGen/data/ballot-item/…`;
`/results/public/api/jurisdictions/Georgia`, `/results/public/api/elections/Georgia/GeneralPrimary51926{,/data,/data/ballot-item/…}`
and `/results/public/api/elections/Georgia/RECOUNTPSCDistrict3/data`

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

**Battleground feeds (accessed 2026-10-06):**
- https://s3.amazonaws.com/dl.ncsbe.gov/ENRS/2024_11_05/results_pct_20241105.zip and https://s3.amazonaws.com/dl.ncsbe.gov/ENRS/2026_11_03/results_pct_20261103.zip
- https://electionresults.nebraska.gov/resultsCSV.aspx?text=All&type=SW&map=CTY and http://web.archive.org/web/20250119073412id_/https://electionresults.nebraska.gov/resultsCSV.aspx?text=All
- https://cdn1.arizona.vote/data/47/0/election_47_0.json, https://cdn1.arizona.vote/data/47/0/all_county_races_47_0_en_12036.json, https://cdn1.arizona.vote/data/69/0/site_header_69_0_en.json
- https://goelect.txelections.civixapps.com/api-ivis-system/api/s3/enr/electionConstants, `.../election/countyInfo/53813`, `.../countyInfo/53815`; https://results.texas-election.com/static/data/election/49664/1012/County.json
- https://mvic.sos.state.mi.us/votehistory/Index?type=C&electionDate=11-5-2024 and `/VoteHistory/GetElectionResultFile?electionId=698` (Wayback 20240829225126)
- https://liveresults.ohiosos.gov/Api/v1/Contest/GetContestDetailsObject (Wayback 20241106040935)
- https://silverstateelection.nv.gov/_xml/USandNV.xml (Wayback 20250717072406)
- https://ent.sos.ks.gov/kssos_ent.html (Wayback 20241106112118)
- https://www.elections.alaska.gov/enr26/results/GA_ENR_Precinct_State_of_Alaska.csv and https://www.elections.alaska.gov/results/24GENR/ENRbyPrecinct.csv
- https://www.enr-scvotes.org/SC/elections.json, https://electionresults.iowa.gov/IA/elections.json

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
