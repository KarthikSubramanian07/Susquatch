//! Parsers against recorded payloads. Fixtures (see `tests/fixtures/`):
//! * `ca_sos_governor_testdata_2026-09-28.json`: verbatim CA SoS bulk file (2026 general test data);
//! * `ca_sos_us_rep_synthetic_midcount.json`: real CA-13/CA-22 structure with synthetic counts;
//! * `clarity_sc_2024_detailxml.zip`: SC 2024 general `detailxml.zip`, presidential contest only;
//! * `clarity_wv_2024_detail.xml` + `clarity_wv_2024_{summary,details}.json`: WV 2024 general;
//! * `ga_enhanced_voting_2024_president.json`: GA SoS (Enhanced Voting) 2024, three counties.

use chrono_tz::Tz;
use susq_contracts::v1::{CountyResult, VoteType};
use susq_enr::adapters::json_map::JsonMapping;
use susq_enr::adapters::{AdapterContext, ca_sos, clarity, static_json};
use susq_enr::config::{EnrConfig, RaceMap, SourceKind};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture")
}

fn ctx(source: &str, state: &str, races: &[(&str, &str)], tz: Tz) -> AdapterContext {
    AdapterContext {
        source_id: source.into(),
        state: state.into(),
        races: RaceMap::from_pairs(races.iter().copied()),
        tz,
    }
}

fn votes_for(r: &CountyResult, party: &str) -> u64 {
    r.votes.iter().filter(|c| c.party == party).map(|c| c.votes).sum()
}

fn find<'a>(rs: &'a [CountyResult], fips: &str, vt: VoteType) -> &'a CountyResult {
    rs.iter()
        .find(|r| r.county_fips == fips && r.vote_type == vt as i32)
        .unwrap_or_else(|| panic!("no result for {fips} {vt:?}"))
}

#[test]
fn ca_sos_real_test_data_yields_every_county_mixed() {
    let c = ctx(
        "ca_sos",
        "CA",
        &[("Governor", "2026-GOV-CA")],
        Tz::America__Los_Angeles,
    );
    let parsed = ca_sos::parse(&fixture("ca_sos_governor_testdata_2026-09-28.json"), &c, 1).expect("parse");
    assert_eq!(
        parsed.results.len(),
        58,
        "one row per county; statewide row skipped"
    );
    assert_eq!(parsed.unresolved_counties, 0);
    assert!(
        parsed
            .results
            .iter()
            .all(|r| r.vote_type == VoteType::Mixed as i32)
    );
    let alameda = parsed
        .results
        .iter()
        .find(|r| r.county_fips == "06001")
        .expect("Alameda");
    assert_eq!((alameda.precincts_reporting, alameda.precincts_total), (0, 627));
    assert_eq!(alameda.votes[0].party, "DEM");
    // "September 28, 2026, 2:11 p.m." PDT.
    assert_eq!(alameda.source_ts_ns, 1_790_629_860_000_000_000);
}

#[test]
fn ca_sos_house_districts_split_across_counties() {
    let c = ctx(
        "ca_sos",
        "CA",
        &[
            ("United States Representative District 13", "2026-HOUSE-CA-13"),
            ("United States Representative District 22", "2026-HOUSE-CA-22"),
        ],
        Tz::America__Los_Angeles,
    );
    let parsed = ca_sos::parse(&fixture("ca_sos_us_rep_synthetic_midcount.json"), &c, 1).expect("parse");
    let ca13: Vec<_> = parsed
        .results
        .iter()
        .filter(|r| r.race_id == "2026-HOUSE-CA-13")
        .collect();
    let ca22: Vec<_> = parsed
        .results
        .iter()
        .filter(|r| r.race_id == "2026-HOUSE-CA-22")
        .collect();
    assert_eq!(ca13.len(), 5, "Fresno, Madera, Merced, San Joaquin, Stanislaus");
    assert_eq!(ca22.len(), 5, "Fresno, Kern, Kings, Madera, Tulare");
    assert!(ca13.iter().any(|r| r.county_fips == "06047"), "Merced");
    assert!(ca13.iter().all(|r| r.votes.iter().all(|v| v.votes >= 4000)));
    assert_eq!((ca13[0].precincts_reporting, ca13[0].precincts_total), (212, 515));
}

#[test]
fn ca_sos_single_object_and_garbage() {
    let c = ctx(
        "ca_sos",
        "CA",
        &[("Governor", "2026-GOV-CA")],
        Tz::America__Los_Angeles,
    );
    let one = br#"{"raceTitle":"Governor - Alpine County Results","Reporting":"","ReportingTime":"",
        "candidates":[{"Name":"A","Party":"Dem","Votes":"3"}]}"#;
    assert_eq!(ca_sos::parse(one, &c, 0).expect("parse").results.len(), 1);
    assert!(ca_sos::parse(b"<html>Access denied</html>", &c, 0).is_err());
    assert!(ca_sos::parse(br#"[{"raceTitle":"Governor"}]"#, &c, 0).is_err());
    let bad_votes =
        br#"[{"raceTitle":"Governor - Alpine County Results","candidates":[{"Name":"A","Votes":"-3"}]}]"#;
    assert!(ca_sos::parse(bad_votes, &c, 0).is_err());
}

#[test]
fn clarity_sc_detail_xml_splits_by_vote_type() {
    let c = ctx(
        "clarity:SC",
        "SC",
        &[("President and Vice President", "2024-PRES-SC")],
        Tz::America__New_York,
    );
    let xml = clarity::unzip_detail(&fixture("clarity_sc_2024_detailxml.zip")).expect("unzip");
    let parsed = clarity::parse_detail_xml(&xml, &c, 1).expect("parse");
    assert_eq!(parsed.unresolved_counties, 0);
    // 46 counties x {E-day (incl. Failsafe), early, mail, provisional (incl. Failsafe Provisional)}.
    assert_eq!(parsed.results.len(), 46 * 4);
    let abbeville = "45001";
    let eday = find(&parsed.results, abbeville, VoteType::ElectionDay);
    assert_eq!(votes_for(eday, "REP"), 4153);
    assert_eq!(votes_for(eday, "DEM"), 1321);
    assert_eq!(
        votes_for(find(&parsed.results, abbeville, VoteType::EarlyInPerson), "REP"),
        4197
    );
    assert_eq!(
        votes_for(find(&parsed.results, abbeville, VoteType::Mail), "DEM"),
        165
    );
    assert_eq!(
        votes_for(find(&parsed.results, abbeville, VoteType::Provisional), "REP"),
        7
    );
    assert_eq!((eday.precincts_reporting, eday.precincts_total), (15, 15));
    assert_eq!(eday.votes.len(), 7, "all seven tickets");
    // "7/30/2025 10:46:52 AM EDT"
    assert_eq!(eday.source_ts_ns, 1_753_886_812_000_000_000);
}

#[test]
fn clarity_single_label_is_treated_as_mixed() {
    let c = ctx(
        "clarity:WV",
        "WV",
        &[("U.S. PRESIDENT", "2024-PRES-WV")],
        Tz::America__New_York,
    );
    let xml = String::from_utf8(fixture("clarity_wv_2024_detail.xml")).expect("utf8");
    let parsed = clarity::parse_detail_xml(&xml, &c, 1).expect("parse");
    assert_eq!(parsed.results.len(), 55);
    let barbour = find(&parsed.results, "54001", VoteType::Mixed);
    assert_eq!(votes_for(barbour, "REP"), 5071);
    assert_eq!(votes_for(barbour, "DEM"), 1199);
}

#[test]
fn clarity_json_pair_matches_detail_xml() {
    let c = ctx(
        "clarity:WV",
        "WV",
        &[("U.S. PRESIDENT", "2024-PRES-WV")],
        Tz::America__New_York,
    );
    let json = clarity::parse_json(
        &fixture("clarity_wv_2024_summary.json"),
        &fixture("clarity_wv_2024_details.json"),
        &c,
        1,
    )
    .expect("parse json");
    let xml = String::from_utf8(fixture("clarity_wv_2024_detail.xml")).expect("utf8");
    let xml = clarity::parse_detail_xml(&xml, &c, 1).expect("parse xml");
    assert_eq!(json.results.len(), 55);
    for j in &json.results {
        let x = find(&xml.results, &j.county_fips, VoteType::Mixed);
        for party in ["REP", "DEM"] {
            assert_eq!(
                votes_for(j, party),
                votes_for(x, party),
                "{} {party}",
                j.county_fips
            );
        }
    }
}

#[test]
fn clarity_rejects_broken_snapshots() {
    let c = ctx(
        "clarity:SC",
        "SC",
        &[("President and Vice President", "2024-PRES-SC")],
        Tz::UTC,
    );
    assert!(clarity::unzip_detail(b"PK\x03\x04 not really a zip").is_err());
    let xml = clarity::unzip_detail(&fixture("clarity_sc_2024_detailxml.zip")).expect("unzip");
    let truncated = &xml[..xml.len() / 2];
    assert!(clarity::parse_detail_xml(truncated, &c, 0).is_err());
    let bad_count = xml.replacen("votes=\"4153\"", "votes=\"lots\"", 1);
    assert!(clarity::parse_detail_xml(&bad_count, &c, 0).is_err());
    let summary = fixture("clarity_wv_2024_summary.json");
    let protected = br#"{"Contests":[{"K":"100","P":["Barbour"],"V":[["protected","protected","protected","protected","protected"]]}]}"#;
    let wv = ctx("clarity:WV", "WV", &[("U.S. PRESIDENT", "2024-PRES-WV")], Tz::UTC);
    assert!(
        clarity::parse_json(&summary, protected, &wv, 0).is_err(),
        "privacy-suppressed cells are not zeros"
    );
}

#[test]
fn enhanced_voting_mapping_from_repo_config_pivots_vote_types() {
    let cfg_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/enr_sources.toml");
    let cfg = EnrConfig::load(&cfg_path).expect("config");
    let src = cfg
        .sources
        .iter()
        .find(|s| s.id == "ga_sos")
        .expect("ga_sos source");
    let SourceKind::StaticJson { mapping, .. } = &src.kind else {
        panic!("ga_sos must be static_json")
    };
    let mapping: &JsonMapping = mapping;
    let compiled = mapping.compile().expect("compile");
    let c = AdapterContext {
        source_id: "ga_sos".into(),
        state: "GA".into(),
        races: cfg.race_map("ga_sos"),
        tz: src.tz,
    };
    let parsed = static_json::parse(
        &fixture("ga_enhanced_voting_2024_president.json"),
        &compiled,
        &c,
        1,
    )
    .expect("parse");
    assert_eq!(parsed.results.len(), 3 * 4, "3 counties x 4 vote methods");
    let fulton = "13121";
    assert_eq!(
        votes_for(find(&parsed.results, fulton, VoteType::EarlyInPerson), "DEM"),
        297_293
    );
    assert_eq!(
        votes_for(find(&parsed.results, fulton, VoteType::ElectionDay), "REP"),
        25_789
    );
    assert_eq!(
        votes_for(find(&parsed.results, fulton, VoteType::Mail), "DEM"),
        22_497
    );
    assert_eq!(
        votes_for(find(&parsed.results, fulton, VoteType::Provisional), "DEM"),
        487
    );
    let dem_total: u64 = parsed
        .results
        .iter()
        .filter(|r| r.county_fips == fulton)
        .map(|r| votes_for(r, "DEM"))
        .sum();
    assert_eq!(dem_total, 384_752, "pivot preserves the county total");
    let r = find(&parsed.results, "13001", VoteType::Mail);
    assert_eq!((r.precincts_reporting, r.precincts_total), (9, 9));
    assert!(r.source_ts_ns > 0);
}
