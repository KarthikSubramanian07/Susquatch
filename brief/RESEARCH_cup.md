# RESEARCH — SIG Predictions Cup (Midterm Elections), as of Sept 27, 2026

Confidence: **H** = primary, **M** = secondary/inferred, **L** = weak or model-based. ⚠️ marks a conflicted source (Sponsor PR, affiliate sites, vendors, or competitors).

## Bottom line

Final rank is decided entirely by the **resolved 0/1 payoff of what you hold at the Nov 4, 12:00pm ET lock**, and first prize is 80% of the $37.5k pool. October bankroll-building can't win this on its own. What drives P(1st):

- **(a)** a high-multiple, correlated position in the world that actually happens; and
- **(b)** an election-night engine that buys outcomes live data already favors, at prices the Cup hasn't caught up to, and rolls gains through the night.

Pre-open Cup prices mirror Kalshi almost exactly, so treat the Sponsor as a sharp, Kalshi-anchored counterparty. The edge is in stale student orders and election-night latency.

## 1. Verified rules (Official Rules, predictionscup.com/rules; Guide at sig.thesuper.market/docs)

1. **Competition period:** Oct 1, 2026, 12:00pm to Nov 4, 2026, 12:00pm Eastern. "All trades must be received prior to 12:00pm Eastern Standard Time on November 4, 2026." (DST ends Nov 1.) **H**
2. **Starting balance:** 100,000 SUSQies at registration. Late registrants still get the full allotment. SUSQies cannot be bought, transferred, or cashed out. **H**
3. **Prizes and ranking:** $30,000 / $5,000 / $2,500 (total $37,500), one per person. Rank is by "verified total SUSQies balance as of the resolution of all Markets." There are **no tiebreakers**: tied players share the pooled prizes for the places they occupy. **H**
4. **Ranking requirement:** you must complete ≥1 trade to be ranked. **H**
5. **Bots and account rules:** unlimited Bots via API, but only one Account. API use is subject to authentication, **rate limits, position limits**, and technical controls. The platform may suspend, throttle, disqualify, or remove any Bot for any reason. **H** (exact numbers unknown)
6. **Sponsor as market maker:** the Sponsor "may… act as a market maker in any or all Markets," with no obligation to provide liquidity, and may withdraw at any time. **H**
7. **Lock and settlement:** after the period ends, positions are locked and can't be modified. Rankings become final once every market has settled. Winners are verified within 2 weeks after all markets resolve. **H**
8. **Sponsor discretion:** the Sponsor may update rules, outcomes, or resolution at any time (including after the resolution date), and may void or reverse orders, trades, or positions. Force majeure covers recounts, challenges, and certification delays. **H**
9. **Competition Data:** the Sponsor owns all Competition Data (orders, trades, positions, API/Bot logs, and statistics, analytics, models, and derivative works built from them). **H**
10. **Market structure:** YES/NO markets with separate markets per party per race. Example: "Will the Democratic Party win the New Hampshire Senate?" #381 at 86%, and the Republican market #382 at 14%.
    - The tournament parameter is `tournament=midterm-elections`.
    - Market IDs observed range from about 151 to 388.
    - Categories are "Election Outcome" and "Freeform."
    - There are no mention markets.
    - Each market lists its resolution source in an "Info" tab.
    - The Platform Operator (The Super Market, not affiliated with SIG) resolves markets. **H**
11. **Pre-open prices track real money (inference M):** House D 92% (Kalshi/Poly average 92), Senate R 39% (market D ≈62), TX D 61 (Kalshi 61), ME D 61 (Kalshi 60), NH D 86 (86), NC D 94 (93), GA R 6 (Kalshi D 94). **H** (observation)
12. **Eligibility:** enrolled undergrad, grad, or PhD students of majority age in 36 jurisdictions. Current and former SIG interns and co-ops may participate. Winners submit an eligibility form within 72 hours and documents within 5 days. US winners get a W-9 and a 1099-MISC for anything over $2,000. Non-US winners have 30% withheld. **H**
13. **MNPI:** no trading on material non-public information, including if you work, intern, volunteer, or advise for a candidate or campaign (or are immediate family of one) whose result decides a market. **H**
14. **Recruiting (⚠️ Sponsor PR):** Bloomberg (Sept 23): SIG's prediction team grew to 80 from about 60 in January (Jeremy Maletz, head of macro trading and prediction markets). Lauren Laver, head of campus recruiting, runs the contest push. Business Wire says trading competitions are part of how SIG identifies students with an aptitude for probabilistic thinking. The platform has a "Job Opportunities" link and an opt-in for internship contact.
15. **Inaugural event:** this is the first Predictions Cup, so there are no prior results or post-mortems. **H**

## 2. Platform/API: known vs unknown

- **Known:**
  - peer-to-peer YES/NO order books with resting bids and offers;
  - the API supports market data, order submit and cancel, and position monitoring;
  - a trade counts as received only on explicit confirmation, and the platform's server records control;
  - the portfolio shows "SUSQies available to trade" and "account value";
  - the live leaderboard ranks by account value (provisional).
- **Unofficial** (⚠️ competitor repo github.com/ZVogel1/SIG-PM-Challenge-F2026):
  - API keys under Settings → API Keys with read+trade scopes;
  - `TOURNAMENT_SLUG` isolates the cup's order book and balance;
  - 120 s cycles, idempotency keys, and per-race and per-basket caps;
  - a "slightly more aggressive than Kelly" low-variance approach, which rarely wins a top-heavy tournament.
- **API surface (M, from a competitor's working client; see `API_NOTES.md`):**
  - Bearer-token auth. Base URL `https://www.thesuper.market/api/v1` in their config, even though the docs live on `sig.thesuper.market`.
  - Endpoints for tournaments, paginated markets, bulk prices (≤100 exchange IDs per call), orderbook, **platform-computed cross-market constraint violations with suggested corrective trades**, positions, leaderboard (`myRank`), and `POST /orders` with `exchangeId`, `side` (yes/no), `action` (buy/sell), `quantity`, a decimal `price` in 0–1, `tournamentId`, and `idempotencyKey`.
  - Because the platform publishes constraint violations to everyone, **constraint arbitrage is a crowded speed race**, not a hidden edge.
- **Unknown until Oct 1:**
  - tick size, payout units, and price bounds (whole-percent prices suggest 1¢ ticks on 0–100);
  - order types, matching priority, and fees;
  - rate-limit and position-limit numbers;
  - whether streaming exists, and pagination;
  - how open positions are marked for account value;
  - Sponsor spreads, depth, and speed;
  - registrant count and field lean;
  - per-market resolution rules (Info tabs);
  - whether new markets get added;
  - Terms of Service details.
- **Structural implication:** where an independent can win (Nebraska Senate, Osborn (I)), both party markets can resolve NO, so YES(D) + YES(R) < 100 is **not** arbitrage there. Check each Info tab.

## 3. Rules nuances

- **Individual play only.** No team prizes. Reading public code is not prohibited. Coordinating trades with friends is risky. Transferring SUSQies through deliberately losing trades violates the multi-account clause and means disqualification.
- **Voided trades.** Hitting obviously broken Sponsor quotes can be reversed (§16), so size any glitch trade as if it may be voided.
- **Publishing.** Your independently written code isn't Competition Data. Published order logs, fills, P&L curves, and models trained on platform data arguably are. Publish code, methodology, the simulator, external backtests, and aggregate results. **Email predictionscup@sig.com** before publishing anything derived from platform data. (M; not legal advice.)
- **Prize timing.** Expect payout between late November and Q1 2027, depending on the slowest market's resolution. Polymarket's House market resolves around Jan 4, 2027. Kalshi's control markets resolve in Feb 2027.
- **Taxes.** The $30k prize is taxable income. Budget roughly 25–35% combined federal plus CA.

## 4. Tournament theory (why variance and correlation dominate)

- Lichtendahl & Winkler (Management Science 2007): a forecaster who wants to beat rivals should report more extreme probabilities.
- Witkowski et al.: top-score-wins can fail to select the most accurate forecaster, and a perfect forecaster can have 0 chance of winning.
- Forecasting competitions with correlated events (arXiv 2303.13793): competitors extremize to maximize their chance of the top score.
- **Implications:**
  - In any realized world, several longshots hit. With hundreds of YOLO players spread across cheap contracts, the winner's multiple T is probably ≥10–20×.
  - **Prior thresholds (L):** N≈500 → 8–15×; N≈2,000 → 15–30×; N≈10,000 → 25–100×.
  - Consensus or Kelly strategies have P(1st) ≈ 0 when YOLO players are present.
  - October gains multiply the final bet's multiple but rarely decide the winner.
  - **Ties are real** (everyone starts at exactly 100,000), so build in an idiosyncratic leg.
  - **What wins:** (i) a coherent correlated scenario book whose multiple in its world is ≥T; or (ii) election-night rolling, where 3 rolls at ~2.5× ≈ 15× and 4 rolls ≈ 40×, if stale depth exists.

## 5. Midterm landscape (Sept 26–27, 2026; will move)

- **Chamber control (real money):**
  - House D ≈ 92% (DeFi Rate Kalshi/Poly average; Kalshi ask 91.7¢, Polymarket 93¢).
  - Senate D ≈ 62% (62.5–63¢).
  - Republican sweep ≈ 7.5–7.8¢.
  - ⚠️ An aggregator showing House D 85.4% conflicts with direct venue quotes; likely stale.
  - Race to the WH model: Senate D 68.9% (above the market). Silver Bulletin FLIPR toplines are paywalled. A public note says an NYT poll has Collins (R-ME) +3.
- **National environment:**
  - Generic ballot: D+8.1 (Silver Bulletin 9/23), D+7.6 (FiftyPlusOne), D+7.4 (US Polling Data).
  - Trump approval: 34.6/62.2 (FiftyPlusOne); cross-aggregator about 36.1/61.3; RCP 38.8/56.9; Strength In Numbers/Verasight 33/63 (⚠️ outlet run by G. Elliott Morris).
  - Net approval of about −25 to −28 is worse than in 2018, when Republicans lost 41 House seats.
- **Senate battlefield (Kalshi, Sept 27; D% = chance the Democrat wins):**

  | Race | Matchup | D% | 7-day | Notes |
  |---|---|---|---|---|
  | Ohio (special) | Brown vs Husted | 58 | +2 | stale quote |
  | Iowa | Turek vs Hinson | 43 | +2 | Cook Lean R → Toss-up (9/22) |
  | Maine | Jackson vs Collins | 60 | −8 | 51st seat; NYT poll Collins +3 conflicts with market |
  | Texas | Talarico vs Paxton | 61 | +1 | |
  | Alaska | Peltola vs Sullivan | 71 | −1 | stale; RCV → slow |
  | Kansas | — | 30 | +3 | Likely → Lean R |
  | Michigan | El-Sayed vs Rogers | 72 | +6 | Cook Toss-up → Lean D |
  | New Hampshire | Pappas | 86 | +3 | |
  | South Carolina | — | ~12–13 | — | Sabato Safe → Likely R |
  | Nebraska | Osborn (I) vs Ricketts | n/a | — | independent; both party markets can resolve NO |
  | Georgia / North Carolina | — | 94 / 93 | — | Sabato GA Lean → Likely D |

  - Democrats need +4 net. The Senate is currently 53R–47D.
  - A naive independent combination of the race prices implies 64% D Senate vs 62% on the control market. That gap is correlation, not arbitrage.
- **Genuinely uncertain races:** OH, IA, ME, TX, KS (~30), AK and MI (~70). House control is ~8% R; individual House seats hold the tail.
- **Clusters:**
  - national;
  - red-state reach (TX, OH, IA, KS, AK, SC): Silver Bulletin notes "the blue wave is mostly in red states";
  - New England incumbency (ME);
  - Midwest industrial (MI, OH);
  - House suburban seats (e.g., NY-17 D 65%, PA-07 D 84% on the Cup).
- **Field-exposure hypothesis:** a student field may lean D. If Cup prices drift more D than Kalshi, the contrarian scenario is "R beats polls" (R holds ME, OH, TX, IA, AK; narrow R House at 8¢). G. Elliott Morris notes Republicans beat August Senate polls in 2018, 2020, 2022, and 2024, but correcting for predicted bias helped in only 9 of 14 cycles. **Direction is not an edge; field under-exposure is.**

## 6. Simulator inputs

- Shirani-Mehr, Rothschild, Goel & Gelman (JASA 2018): 4,221 polls across 608 state races (1998–2014, final 3 weeks). RMSE ≈ **3.5 pts on vote share**, about 2× the stated MOE. Average absolute bias 1.8 (Senate) and 2.1 (Governor); excess SE ≈ 0.8. Double these for margin.
- FiveThirtyEight weighted-average absolute error on margin since 1998: 6.0 overall; 4.8 in 2021–22 (a record low); generic ballot 3.9; district House polls 6.7.
- In 2020, Senate misses exceeded presidential misses; 7 Senate races polled within ±3 points.
- Recommended parameters are in SPEC §5. Read the Silver Bulletin FLIPR methodology (it models House, Senate, and governor races jointly, ~40k sims) and the Economist model docs before finalizing correlations.

## 7. Election-night data

- **California SoS (H for existence):**
  - The Election Night Data-Feed Guide (v14/v16) documents ASCII "V message" files and a **JSON API**; XML was dropped after Nov 2018.
  - The SoS runs several **test iterations** in the weeks before the election, generating simulated test data.
  - The June 2, 2026 primary media page lists JSON and API endpoint CSVs.
  - Test dates of Oct 22, Oct 29, and Nov 1 are **unverified**; confirm them in the v16 guide or on media.sos.ca.gov.
  - Guide URL: https://cms.cdn.sos.ca.gov/media/1-media-guide-v16.pdf
- **DDHQ (⚠️ vendor):**
  - Sources results from official state sites ("never modeled or imputed"). County archive back to 2000. APIs "optimized for financial services." 14B+ requests on election night 2024.
  - DDHQ Votes shows batch type (absentee/early/Election Day) plus Kalshi/Polymarket odds. Its method was published in the Harvard Data Science Review (fall 2024).
  - Pricing and student access aren't public; worth emailing. It hires students for election night.
- **AP Elections API:** cost and academic access unverified; assume expensive and gated.
- **Replay data:** DDHQ's archive is commercial. Otherwise, capture state ENR/Clarity JSON yourself during test windows. Free timestamped archives are unverified. Build a synthetic replay from final county results, with realistic reporting order by vote type.
- **Needle method:** remaining vote = expected turnout − reported. Estimate the remaining margin by vote type from reported batches plus 2022/2024 priors. Propagate within-state correlated errors. Use the pre-election sim as the prior on national shift N, and update N from early East Coast results to move western races.
- **Poll closing times (M; confirm from each SoS before Oct 15):**
  - ~7pm ET: GA, SC, VA, VT, most of IN/KY.
  - 7:30pm: NC, OH, WV.
  - 8pm: ME, NH, PA, most of MI/FL/TX/KS.
  - 9pm: AZ, IA, NE, and the western parts of MI/TX/KS.
  - 10–11pm: NV and the West Coast.
  - Alaska: ~midnight–1am ET.
- **Real-money market speed on election night:** not measured. Record Kalshi/Polymarket ticks as your reference clock.

## 8. Late-counting races and settlement timing

- **2024:** House control was settled Nov 13 when Ciscomani (AZ) won the 218th R seat (PBS/AP). The last seat, CA-13, was called for Adam Gray on Dec 3; the final House was 220R–215D (APM Research Lab). The Senate's 51st seat was called just after midnight on election night.
- **2022:** Republicans were at 217 on Nov 15, with CA and CO calls pending.
- **Likely uncalled at noon Nov 4, 2026 (M):**
  - California House seats;
  - Alaska Senate (RCV rounds begin after the final count on the 15th day after the election, around Nov 18; first-choice counts updated on days 7, 10, and 15);
  - Maine if no one gets a majority (RCV);
  - AZ/NV close races;
  - WA/OR mail-heavy seats;
  - any Georgia race under 50% → runoff (GA Senate currently ~94% D, so low risk).
  - Louisiana's 2026 system is unverified.
- **Rule:** in slow races, estimate the late ballots by county and batch type. The direction of California's late shift changed between the 2010s and 2022–24, so don't rely on folklore.

## 9. Sponsor behavior (M, inference)

The Sponsor's prices match Kalshi to within about 1 point, SIG is Kalshi's flagship market maker, and it has an 80-person desk. Base case: it quotes around its own real-money marks with limited size, updates fast, and widens or pulls during fast election-night moves (the rules allow withdrawing). Your API logs are a behavioral record that SIG owns, and are plausibly a stronger recruiting signal than rank.

## 10. Honest expectations

P(1st) ≈ 1–5% at N≈2,000, lower at 10,000. Expected prize value is about $300–$1,500. Recruiting value is larger.

## Key URLs

- Rules: https://predictionscup.com/rules/
- Guide: https://sig.thesuper.market/docs and https://sig.thesuper.market/docs/tournaments-and-organizations
- API reference: https://sig.thesuper.market/api/v1/docs (may block automated fetch)
- Platform: https://sig.thesuper.market/
- Bloomberg (Sept 23, 2026): https://www.bloomberg.com/news/articles/2026-09-23/susquehanna-s-80-person-prediction-team-launches-student-contest
- Business Wire: https://www.businesswire.com/news/home/20260922468550/en/
- DeFi Rate midterm odds ⚠️: https://defirate.com/prediction-markets/2026-midterms/
- CA SoS media: https://media.sos.ca.gov/
- DDHQ docs ⚠️: https://votes-docs.decisiondeskhq.com/
- Lichtendahl & Winkler: https://papers.ssrn.com/sol3/papers.cfm?abstract_id=961001
- Correlated forecasting competitions: https://arxiv.org/html/2303.13793v1
- Shirani-Mehr et al.: https://sites.stat.columbia.edu/gelman/research/unpublished/pollposition_v3.pdf
- APM House 2024 calls: https://www.apmresearchlab.org/us-house-senate-control-2025
- G. Elliott Morris on poll bias: https://www.gelliottmorris.com/p/2026-08-21-predicting-poll-bias-fools-errand
