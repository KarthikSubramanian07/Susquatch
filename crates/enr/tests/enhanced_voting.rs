//! Enhanced Voting discovery against recorded Georgia payloads (trimmed from the live site on
//! 2026-09-28) and a local mock server. Fixtures:
//! * `ev_ga_jurisdiction_2026-09-28.json`: `GET /jurisdictions/Georgia`, five of the 117 listed
//!   elections, including two on 2026-05-19 (the general primary and a PSC recount);
//! * `ev_ga_primary_2026_data.json`: `GET /elections/Georgia/GeneralPrimary51926/data`, first six
//!   ballot items;
//! * `ev_ga_recount_2026_data.json`: the recount's `/data`;
//! * `ev_ga_primary_2026_us_senate_rep.json`: the "US Senate - Rep" ballot item, four counties
//!   (Evans County publishes candidate totals with every vote method `null`).
//!
//! The Nov 3 general is not published yet, so "the general appears" cases rename the primary's
//! contests to the general-election names ("US Senate", "Governor") inside the test.

use std::time::Duration;

use chrono::NaiveDate;
use chrono_tz::Tz;
use serde_json::{Value, json};
use susq_contracts::v1::{CountyResult, VoteType};
use susq_enr::adapters::enhanced_voting::{
    self, DiscoveryPhase, EnhancedVotingSource, ItemPayload, Settings,
};
use susq_enr::adapters::json_map::CompiledMapping;
use susq_enr::adapters::{AdapterContext, PollOutput, ResultsSource, static_json};
use susq_enr::config::{ContestSpec, EnrConfig, RaceMap, SourceKind, SourceTiming};
use susq_enr::http::HttpClient;
use susq_enr::service::{ServiceConfig, SourceTask, spawn};
use tokio::sync::mpsc;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

const PRIMARY: &str = "GeneralPrimary51926";
const RECOUNT: &str = "RECOUNTPSCDistrict3";
const SENATE_REP_ID: &str = "01000000-f33c-bc21-a444-08dead340297";
const GOV_REP_ID: &str = "01000000-f33c-bc21-51c6-08dead3402a8";
const FULTON: &str = "13121";
const EVANS: &str = "13109";

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture")
}

fn fixture_json(name: &str) -> Value {
    serde_json::from_slice(&fixture(name)).expect("fixture json")
}

fn date(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("date")
}

fn spec(name: &str, race_id: &str) -> ContestSpec {
    ContestSpec {
        name: name.into(),
        race_id: race_id.into(),
    }
}

/// The `[sources.mapping]` shipped for `ga_sos` in `config/enr_sources.toml`.
fn repo_mapping() -> CompiledMapping {
    let cfg_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/enr_sources.toml");
    let cfg = EnrConfig::load(&cfg_path).expect("config");
    let src = cfg.sources.iter().find(|s| s.id == "ga_sos").expect("ga_sos");
    let SourceKind::EnhancedVoting { mapping, .. } = &src.kind else {
        panic!("ga_sos must be enhanced_voting")
    };
    mapping.compile().expect("compile")
}

fn ctx(contests: &[ContestSpec]) -> AdapterContext {
    AdapterContext {
        source_id: "ga_sos".into(),
        state: "GA".into(),
        races: RaceMap::from_pairs(contests.iter().map(|c| (c.name.as_str(), c.race_id.as_str()))),
        tz: Tz::America__New_York,
    }
}

struct Opts {
    date: &'static str,
    contests: Vec<ContestSpec>,
    discovery_interval: Duration,
    min_rediscovery_gap: Duration,
    require_production: bool,
}

impl Opts {
    fn primary() -> Self {
        Self {
            date: "2026-05-19",
            contests: vec![spec("US Senate - Rep", "2026-SEN-GA")],
            discovery_interval: Duration::from_secs(3600),
            min_rediscovery_gap: Duration::ZERO,
            require_production: false,
        }
    }
}

fn source(server: &MockServer, o: Opts) -> EnhancedVotingSource {
    let settings = Settings {
        base_url: server.uri(),
        jurisdiction: "Georgia".into(),
        election_date: date(o.date),
        contests: o.contests.clone(),
        discovery_interval: o.discovery_interval,
        min_rediscovery_gap: o.min_rediscovery_gap,
        require_production: o.require_production,
    };
    let http = HttpClient::new(Duration::from_secs(2), "susq-test").expect("client");
    EnhancedVotingSource::new(ctx(&o.contests), settings, repo_mapping(), http).expect("source")
}

fn data_path(election: &str) -> String {
    format!("/elections/Georgia/{election}/data")
}

fn item_path(election: &str, item: &str) -> String {
    format!("/elections/Georgia/{election}/data/ballot-item/{item}")
}

fn ok_json(v: &Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(v)
}

async fn serve(server: &MockServer, p: &str, resp: ResponseTemplate) {
    Mock::given(path(p)).respond_with(resp).mount(server).await;
}

async fn serve_n(server: &MockServer, p: &str, resp: ResponseTemplate, n: u64) {
    Mock::given(path(p))
        .respond_with(resp)
        .up_to_n_times(n)
        .mount(server)
        .await;
}

/// Jurisdiction list with an extra election appended (how the general will show up).
fn jurisdiction_with(public_id: &str, election_date: &str, name: &str) -> Value {
    let mut j = fixture_json("ev_ga_jurisdiction_2026-09-28.json");
    j["elections"].as_array_mut().expect("elections").push(json!({
        "publicElectionId": public_id,
        "electionDate": election_date,
        "name": [{"languageId": "en", "text": name}],
    }));
    j
}

/// Primary `/data` with ballot items renamed and/or re-keyed.
fn data_with(renames: &[(&str, &str)], new_ids: &[(&str, &str)]) -> Value {
    let mut d = fixture_json("ev_ga_primary_2026_data.json");
    for item in d["ballotItems"].as_array_mut().expect("ballotItems") {
        let name = item["name"][0]["text"].as_str().unwrap_or_default().to_owned();
        if let Some((_, to)) = renames.iter().find(|(from, _)| *from == name) {
            item["name"][0]["text"] = json!(to);
        }
        let id = item["id"].as_str().unwrap_or_default().to_owned();
        if let Some((_, to)) = new_ids.iter().find(|(from, _)| *from == id) {
            item["id"] = json!(to);
        }
    }
    d
}

/// The recorded "US Senate - Rep" ballot item, optionally renamed, re-keyed or re-flagged.
fn item_with(name: Option<&str>, id: Option<&str>, is_production: Option<bool>) -> Value {
    let mut v = fixture_json("ev_ga_primary_2026_us_senate_rep.json");
    if let Some(n) = name {
        v["ballotItemWithBreakdown"]["name"][0]["text"] = json!(n);
    }
    if let Some(i) = id {
        v["ballotItemWithBreakdown"]["id"] = json!(i);
    }
    if let Some(p) = is_production {
        v["election"]["isProduction"] = json!(p);
    }
    v
}

fn votes(r: &CountyResult, candidate: &str) -> u64 {
    r.votes
        .iter()
        .filter(|c| c.candidate == candidate)
        .map(|c| c.votes)
        .sum()
}

fn find(rs: &[CountyResult], fips: &str, vt: VoteType) -> CountyResult {
    rs.iter()
        .find(|r| r.county_fips == fips && r.vote_type == vt as i32)
        .unwrap_or_else(|| panic!("no result for {fips} {vt:?}"))
        .clone()
}

fn discovery(out: &PollOutput) -> &enhanced_voting::DiscoveryStatus {
    out.discovery.as_ref().expect("discovery status")
}

// ── Pure parsers on recorded payloads ─────────────────────────────────────────────────────

#[test]
fn recorded_jurisdiction_lists_both_may_19_elections_and_no_general_yet() {
    let body = fixture("ev_ga_jurisdiction_2026-09-28.json");
    let may = enhanced_voting::elections_on(&body, date("2026-05-19")).expect("parse");
    let ids: Vec<_> = may.iter().map(|e| e.public_id.as_str()).collect();
    assert_eq!(ids, vec![RECOUNT, PRIMARY], "listing order preserved");
    assert_eq!(may[1].name, "May 19, 2026 - General Primary");
    assert!(
        enhanced_voting::elections_on(&body, date("2026-11-03"))
            .expect("parse")
            .is_empty(),
        "the Nov 3 general was not published on 2026-09-28"
    );
    let runoff = enhanced_voting::elections_on(&body, date("2026-06-16")).expect("parse");
    assert_eq!(runoff[0].public_id, "06162026GeneralPrimaryRunoff");
}

#[test]
fn recorded_primary_resolves_contests_by_normalized_name() {
    let body = fixture("ev_ga_primary_2026_data.json");
    let res = enhanced_voting::resolve_contests(
        &body,
        &[
            spec("  us senate -   REP ", "2026-SEN-GA"),
            spec("Governor - Rep", "2026-GOV-GA"),
            spec("US Senate", "x"),
        ],
    )
    .expect("parse");
    assert_eq!(res.matched.len(), 2);
    assert_eq!(res.matched[0].1, SENATE_REP_ID);
    assert_eq!(res.matched[1].1, GOV_REP_ID);
    assert_eq!(
        res.unmatched,
        vec!["US Senate".to_owned()],
        "no fuzzy matching: the primary has only party-suffixed contests"
    );
    assert_eq!(res.flags.is_production, Some(true));
    assert_eq!(res.flags.is_official_results, Some(true));
}

#[test]
fn recorded_ballot_item_maps_to_county_vote_method_results() {
    let body = fixture("ev_ga_primary_2026_us_senate_rep.json");
    let Ok(ItemPayload::Current { root, flags }) = enhanced_voting::inspect_item(&body, SENATE_REP_ID) else {
        panic!("recorded ballot item must be current");
    };
    assert_eq!(flags.is_production, Some(true));
    let contests = [spec("US Senate - Rep", "2026-SEN-GA")];
    let parsed = static_json::parse_value(&root, &repo_mapping(), &ctx(&contests), 1).expect("parse");
    assert_eq!(parsed.unresolved_counties, 0);
    assert_eq!(
        parsed.results.len(),
        3 * 4 + 1,
        "Appling, DeKalb, Fulton x 4 methods; Evans publishes no method split"
    );
    assert!(parsed.results.iter().all(|r| r.race_id == "2026-SEN-GA"));
    // Evans County: every method is `null` on the live site, only candidate totals exist.
    let evans = find(&parsed.results, EVANS, VoteType::Mixed);
    assert_eq!(votes(&evans, "Earl L. \"Buddy\" Carter"), 666);
    assert_eq!(votes(&evans, "Derek Dooley"), 147);
    assert_eq!(
        parsed.results.iter().filter(|r| r.county_fips == EVANS).count(),
        1
    );
    // Official Fulton County numbers for Derek Dooley.
    assert_eq!(
        votes(
            &find(&parsed.results, FULTON, VoteType::EarlyInPerson),
            "Derek Dooley"
        ),
        10_797
    );
    assert_eq!(
        votes(
            &find(&parsed.results, FULTON, VoteType::ElectionDay),
            "Derek Dooley"
        ),
        10_711
    );
    assert_eq!(
        votes(&find(&parsed.results, FULTON, VoteType::Mail), "Derek Dooley"),
        226
    );
    let prov = find(&parsed.results, FULTON, VoteType::Provisional);
    assert_eq!(votes(&prov, "Derek Dooley"), 15);
    assert_eq!((prov.precincts_reporting, prov.precincts_total), (437, 437));
    assert_eq!(prov.votes[0].party, "REP");
    assert!(prov.source_ts_ns > 0);
}

// ── Discovery state machine against a mock server ─────────────────────────────────────────

#[tokio::test]
async fn not_yet_published_waits_and_rechecks_only_on_schedule() {
    let server = MockServer::start().await;
    Mock::given(path("/jurisdictions/Georgia"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(fixture("ev_ga_jurisdiction_2026-09-28.json")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut src = source(
        &server,
        Opts {
            date: "2026-11-03",
            contests: vec![spec("US Senate", "2026-SEN-GA"), spec("Governor", "2026-GOV-GA")],
            ..Opts::primary()
        },
    );
    for _ in 0..3 {
        let out = src.poll().await.expect("waiting is not an error");
        assert!(out.results.is_empty() && out.partial_errors.is_empty());
        let d = discovery(&out);
        assert_eq!(d.phase, DiscoveryPhase::AwaitingElection);
        assert_eq!(d.election_date, "2026-11-03");
        assert_eq!(d.election_id, None);
        assert!(!d.production_verified);
        assert_eq!(d.contests.get("US Senate"), Some(&None));
    }
    assert_eq!(
        src.status().discoveries,
        1,
        "no rediscovery before discovery_interval"
    );
    server.verify().await;
}

#[tokio::test]
async fn general_election_appearing_later_is_picked_up() {
    let server = MockServer::start().await;
    serve_n(
        &server,
        "/jurisdictions/Georgia",
        ResponseTemplate::new(200).set_body_bytes(fixture("ev_ga_jurisdiction_2026-09-28.json")),
        1,
    )
    .await;
    serve(
        &server,
        "/jurisdictions/Georgia",
        ok_json(&jurisdiction_with(
            "2026NovGen",
            "2026-11-03",
            "November 3, 2026 - General Election",
        )),
    )
    .await;
    serve(
        &server,
        &data_path("2026NovGen"),
        ok_json(&data_with(
            &[("US Senate - Rep", "US Senate"), ("Governor - Rep", "Governor")],
            &[],
        )),
    )
    .await;
    serve(
        &server,
        &item_path("2026NovGen", SENATE_REP_ID),
        ok_json(&item_with(Some("US Senate"), None, None)),
    )
    .await;
    // The recorded Senate breakdown doubles as the governor payload (renamed and re-keyed).
    serve(
        &server,
        &item_path("2026NovGen", GOV_REP_ID),
        ok_json(&item_with(Some("Governor"), Some(GOV_REP_ID), None)),
    )
    .await;

    let mut src = source(
        &server,
        Opts {
            date: "2026-11-03",
            contests: vec![spec("US Senate", "2026-SEN-GA"), spec("Governor", "2026-GOV-GA")],
            discovery_interval: Duration::from_millis(20),
            ..Opts::primary()
        },
    );
    let first = src.poll().await.expect("poll");
    assert_eq!(discovery(&first).phase, DiscoveryPhase::AwaitingElection);
    tokio::time::sleep(Duration::from_millis(30)).await;
    let out = src.poll().await.expect("poll");
    let d = discovery(&out);
    assert_eq!(d.phase, DiscoveryPhase::Polling);
    assert_eq!(d.election_id.as_deref(), Some("2026NovGen"));
    assert_eq!(
        d.election_name.as_deref(),
        Some("November 3, 2026 - General Election")
    );
    assert_eq!(d.contests["US Senate"].as_deref(), Some(SENATE_REP_ID));
    assert_eq!(d.contests["Governor"].as_deref(), Some(GOV_REP_ID));
    assert!(d.production_verified);
    assert_eq!(
        out.results.len(),
        2 * 13,
        "two contests x (3 counties x 4 methods + Evans unsplit)"
    );
    let races: std::collections::BTreeSet<_> = out.results.iter().map(|r| r.race_id.as_str()).collect();
    assert_eq!(
        races.into_iter().collect::<Vec<_>>(),
        vec!["2026-GOV-GA", "2026-SEN-GA"]
    );
    assert_eq!(out.raw.len(), 2, "ballot-item bodies are archived");
}

#[tokio::test]
async fn ballot_item_id_change_is_followed_on_scheduled_rediscovery() {
    let server = MockServer::start().await;
    const NEW_ID: &str = "01000000-aaaa-bbbb-cccc-000000000001";
    serve(
        &server,
        "/jurisdictions/Georgia",
        ResponseTemplate::new(200).set_body_bytes(fixture("ev_ga_jurisdiction_2026-09-28.json")),
    )
    .await;
    serve(
        &server,
        &data_path(RECOUNT),
        ok_json(&fixture_json("ev_ga_recount_2026_data.json")),
    )
    .await;
    serve_n(&server, &data_path(PRIMARY), ok_json(&data_with(&[], &[])), 1).await;
    serve(
        &server,
        &data_path(PRIMARY),
        ok_json(&data_with(&[], &[(SENATE_REP_ID, NEW_ID)])),
    )
    .await;
    Mock::given(path(item_path(PRIMARY, SENATE_REP_ID)))
        .respond_with(ok_json(&item_with(None, None, None)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path(item_path(PRIMARY, NEW_ID)))
        .respond_with(ok_json(&item_with(None, Some(NEW_ID), None)))
        .expect(1)
        .mount(&server)
        .await;

    let mut src = source(
        &server,
        Opts {
            discovery_interval: Duration::from_millis(20),
            ..Opts::primary()
        },
    );
    let out = src.poll().await.expect("poll");
    assert_eq!(
        discovery(&out).election_id.as_deref(),
        Some(PRIMARY),
        "recount has no Senate contest"
    );
    assert_eq!(out.results.len(), 13);
    tokio::time::sleep(Duration::from_millis(30)).await;
    let out = src.poll().await.expect("poll");
    let d = discovery(&out);
    assert_eq!(d.contests["US Senate - Rep"].as_deref(), Some(NEW_ID));
    assert_eq!(d.ballot_item_id_changes, 1);
    assert_eq!(out.results.len(), 13);
    server.verify().await;
}

#[tokio::test]
async fn ballot_item_404_forces_rediscovery() {
    let server = MockServer::start().await;
    const NEW_ID: &str = "01000000-aaaa-bbbb-cccc-000000000002";
    serve(
        &server,
        "/jurisdictions/Georgia",
        ResponseTemplate::new(200).set_body_bytes(fixture("ev_ga_jurisdiction_2026-09-28.json")),
    )
    .await;
    serve(
        &server,
        &data_path(RECOUNT),
        ok_json(&fixture_json("ev_ga_recount_2026_data.json")),
    )
    .await;
    serve_n(&server, &data_path(PRIMARY), ok_json(&data_with(&[], &[])), 1).await;
    serve(
        &server,
        &data_path(PRIMARY),
        ok_json(&data_with(&[], &[(SENATE_REP_ID, NEW_ID)])),
    )
    .await;
    serve(
        &server,
        &item_path(PRIMARY, SENATE_REP_ID),
        ResponseTemplate::new(404),
    )
    .await;
    serve(
        &server,
        &item_path(PRIMARY, NEW_ID),
        ok_json(&item_with(None, Some(NEW_ID), None)),
    )
    .await;

    // Scheduled rediscovery is an hour away: only the 404 can trigger it.
    let mut src = source(&server, Opts::primary());
    let err = src.poll().await.expect_err("the only ballot item 404'd");
    assert!(err.to_string().contains("404"), "{err}");
    assert_eq!(src.status().forced_rediscoveries, 1);
    let out = src.poll().await.expect("poll after rediscovery");
    let d = discovery(&out);
    assert_eq!(d.discoveries, 2);
    assert_eq!(d.contests["US Senate - Rep"].as_deref(), Some(NEW_ID));
    assert_eq!(out.results.len(), 13);
}

#[tokio::test]
async fn stale_payloads_force_rediscovery_but_respect_the_gap() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/jurisdictions/Georgia",
        ResponseTemplate::new(200).set_body_bytes(fixture("ev_ga_jurisdiction_2026-09-28.json")),
    )
    .await;
    serve(
        &server,
        &data_path(RECOUNT),
        ok_json(&fixture_json("ev_ga_recount_2026_data.json")),
    )
    .await;
    serve(&server, &data_path(PRIMARY), ok_json(&data_with(&[], &[]))).await;
    // What the live site returns for an unknown ballot item: 200 with a null breakdown.
    serve(
        &server,
        &item_path(PRIMARY, SENATE_REP_ID),
        ok_json(&json!({"election": {"isProduction": true}, "ballotItemWithBreakdown": null})),
    )
    .await;

    let mut src = source(
        &server,
        Opts {
            min_rediscovery_gap: Duration::from_secs(3600),
            ..Opts::primary()
        },
    );
    for _ in 0..3 {
        let out = src.poll().await.expect("a stale body is not a transport failure");
        assert!(out.results.is_empty());
        assert!(out.partial_errors.iter().any(|e| e.contains("stale ballot item")));
    }
    assert_eq!(
        src.status().forced_rediscoveries,
        1,
        "requested once, not per poll"
    );
    assert_eq!(src.status().discoveries, 1, "min_rediscovery_gap holds it back");
}

#[tokio::test]
async fn empty_204_data_means_awaiting_data() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/jurisdictions/Georgia",
        ok_json(&jurisdiction_with("2026NovGen", "2026-11-03", "General")),
    )
    .await;
    serve(&server, &data_path("2026NovGen"), ResponseTemplate::new(204)).await;
    let mut src = source(
        &server,
        Opts {
            date: "2026-11-03",
            contests: vec![spec("US Senate", "2026-SEN-GA")],
            ..Opts::primary()
        },
    );
    let out = src.poll().await.expect("poll");
    let d = discovery(&out);
    assert_eq!(d.phase, DiscoveryPhase::AwaitingData);
    assert_eq!(d.election_id.as_deref(), Some("2026NovGen"));
}

#[tokio::test]
async fn unmatched_contests_are_skipped_and_the_rest_polled() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/jurisdictions/Georgia",
        ResponseTemplate::new(200).set_body_bytes(fixture("ev_ga_jurisdiction_2026-09-28.json")),
    )
    .await;
    serve(
        &server,
        &data_path(RECOUNT),
        ok_json(&fixture_json("ev_ga_recount_2026_data.json")),
    )
    .await;
    serve(&server, &data_path(PRIMARY), ok_json(&data_with(&[], &[]))).await;
    serve(
        &server,
        &item_path(PRIMARY, SENATE_REP_ID),
        ok_json(&item_with(None, None, None)),
    )
    .await;
    let mut src = source(
        &server,
        Opts {
            contests: vec![
                spec("US Senate - Rep", "2026-SEN-GA"),
                spec("US Senate - Libertarian", "nope"),
            ],
            ..Opts::primary()
        },
    );
    let out = src.poll().await.expect("poll");
    let d = discovery(&out);
    assert_eq!(d.phase, DiscoveryPhase::Polling);
    assert_eq!(d.contests["US Senate - Libertarian"], None);
    assert_eq!(out.results.len(), 13);

    // Nothing on the ballot at all.
    let mut none = source(
        &server,
        Opts {
            contests: vec![spec("Dog Catcher", "nope")],
            ..Opts::primary()
        },
    );
    let out = none.poll().await.expect("poll");
    assert_eq!(discovery(&out).phase, DiscoveryPhase::NoMatchingContests);
    assert!(out.results.is_empty());
}

#[tokio::test]
async fn test_data_is_labelled_and_withheld_when_production_is_required() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/jurisdictions/Georgia",
        ResponseTemplate::new(200).set_body_bytes(fixture("ev_ga_jurisdiction_2026-09-28.json")),
    )
    .await;
    serve(
        &server,
        &data_path(RECOUNT),
        ok_json(&fixture_json("ev_ga_recount_2026_data.json")),
    )
    .await;
    serve(&server, &data_path(PRIMARY), ok_json(&data_with(&[], &[]))).await;
    serve(
        &server,
        &item_path(PRIMARY, SENATE_REP_ID),
        ok_json(&item_with(None, None, Some(false))),
    )
    .await;

    let mut lenient = source(&server, Opts::primary());
    let out = lenient.poll().await.expect("poll");
    let d = discovery(&out);
    assert_eq!(d.is_production, Some(false));
    assert!(!d.production_verified, "test data is labelled");
    assert_eq!(out.results.len(), 13, "require_production = false still maps it");

    let mut strict = source(
        &server,
        Opts {
            require_production: true,
            ..Opts::primary()
        },
    );
    let out = strict.poll().await.expect("poll");
    let d = discovery(&out);
    assert!(out.results.is_empty(), "test data withheld");
    assert_eq!(d.withheld_test_payloads, 1);
    assert!(!d.production_verified);
}

#[tokio::test]
async fn one_broken_same_day_election_does_not_block_the_other() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/jurisdictions/Georgia",
        ResponseTemplate::new(200).set_body_bytes(fixture("ev_ga_jurisdiction_2026-09-28.json")),
    )
    .await;
    serve(&server, &data_path(RECOUNT), ResponseTemplate::new(500)).await;
    serve(&server, &data_path(PRIMARY), ok_json(&data_with(&[], &[]))).await;
    serve(
        &server,
        &item_path(PRIMARY, SENATE_REP_ID),
        ok_json(&item_with(None, None, None)),
    )
    .await;
    let mut src = source(&server, Opts::primary());
    let out = src.poll().await.expect("poll");
    assert_eq!(discovery(&out).election_id.as_deref(), Some(PRIMARY));
    assert_eq!(out.results.len(), 13);
}

#[tokio::test]
async fn discovery_failures_are_errors_until_something_resolves() {
    let server = MockServer::start().await;
    serve(&server, "/jurisdictions/Georgia", ResponseTemplate::new(503)).await;
    let mut src = source(&server, Opts::primary());
    assert!(src.poll().await.is_err());
    assert_eq!(src.status().discovery_failures, 1);
    serve(
        &server,
        "/jurisdictions/Nowhere",
        ResponseTemplate::new(200).set_body_string("<html>"),
    )
    .await;
    let settings = Settings {
        base_url: server.uri(),
        jurisdiction: "Nowhere".into(),
        election_date: date("2026-11-03"),
        contests: vec![spec("US Senate", "2026-SEN-GA")],
        discovery_interval: Duration::from_secs(60),
        min_rediscovery_gap: Duration::ZERO,
        require_production: false,
    };
    let http = HttpClient::new(Duration::from_secs(2), "susq-test").expect("client");
    let mut garbage =
        EnhancedVotingSource::new(ctx(&settings.contests), settings, repo_mapping(), http).expect("source");
    let err = garbage.poll().await.expect_err("html is malformed");
    assert!(err.to_string().contains("malformed"), "{err}");
}

// ── End to end: jurisdiction JSON → CountyResultsBatch through the service ─────────────────

#[tokio::test]
async fn end_to_end_from_jurisdiction_to_county_results() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/jurisdictions/Georgia",
        ResponseTemplate::new(200).set_body_bytes(fixture("ev_ga_jurisdiction_2026-09-28.json")),
    )
    .await;
    serve(
        &server,
        &data_path(RECOUNT),
        ok_json(&fixture_json("ev_ga_recount_2026_data.json")),
    )
    .await;
    serve(
        &server,
        &data_path(PRIMARY),
        ResponseTemplate::new(200).set_body_bytes(fixture("ev_ga_primary_2026_data.json")),
    )
    .await;
    Mock::given(path(item_path(PRIMARY, SENATE_REP_ID)))
        .and(wiremock::matchers::header("if-none-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(path(item_path(PRIMARY, SENATE_REP_ID)))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"v1\"")
                .set_body_bytes(fixture("ev_ga_primary_2026_us_senate_rep.json")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let (tx, mut rx) = mpsc::channel(16);
    let task = SourceTask {
        source: Box::new(source(&server, Opts::primary())),
        timing: SourceTiming {
            interval: Duration::from_millis(40),
            jitter_frac: 0.1,
            timeout: Duration::from_millis(500),
            max_backoff: Duration::from_millis(200),
        },
    };
    let mut handle = spawn(ServiceConfig::default(), vec![task], tx, None);
    let batch = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("batch in time")
        .expect("channel open");
    assert_eq!(batch.header.as_ref().expect("header").seq, 1);
    assert_eq!(batch.results.len(), 13);
    let fulton = find(&batch.results, FULTON, VoteType::EarlyInPerson);
    assert_eq!(fulton.source, "ga_sos");
    assert_eq!(fulton.state, "GA");
    assert_eq!(votes(&fulton, "Derek Dooley"), 10_797);

    let health = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            {
                let h = handle.health.borrow_and_update();
                if h.sources["ga_sos"].not_modified >= 2 {
                    return h.clone();
                }
            }
            handle.health.changed().await.expect("health");
        }
    })
    .await
    .expect("conditional GETs in time");
    let d = health.sources["ga_sos"]
        .discovery
        .as_ref()
        .expect("discovery in health");
    assert_eq!(d.phase, DiscoveryPhase::Polling);
    assert_eq!(d.election_id.as_deref(), Some(PRIMARY));
    assert!(d.production_verified);
    assert_eq!(d.is_official_results, Some(true));
    let json = serde_json::to_value(&health).expect("health serializes");
    assert_eq!(json["sources"]["ga_sos"]["discovery"]["phase"], "polling");
    assert_eq!(health.last_seq, 1, "304s produce no batches");
    handle.shutdown().await;
    server.verify().await;
}
