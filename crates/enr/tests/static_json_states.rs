//! `static_json` sources for Arizona against real payloads, through the mappings
//! shipped in `config/enr_sources.toml`. Fixture provenance is in `tests/fixtures/README.md`:
//! * `az_sos_2024_all_county_races_trimmed.json` + `az_sos_2024_election_47_0.json`: AZ SoS
//!   2024 general (eid 47, upload 12036), Apache, Pima and Santa Cruz counties.

use chrono_tz::Tz;
use susq_contracts::v1::{CountyResult, VoteType};
use susq_enr::adapters::json_map::JsonMapping;
use susq_enr::adapters::{AdapterContext, build_source, static_json};
use susq_enr::config::{EnrConfig, RaceMap, SourceKind};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture")
}

fn repo_config() -> EnrConfig {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/enr_sources.toml");
    EnrConfig::load(&path).expect("config/enr_sources.toml")
}

fn repo_mapping(cfg: &EnrConfig, id: &str) -> JsonMapping {
    let src = cfg.sources.iter().find(|s| s.id == id).expect("source in config");
    let SourceKind::StaticJson { mapping, .. } = &src.kind else {
        panic!("{id} must be kind = \"static_json\"")
    };
    (**mapping).clone()
}

fn ctx(source: &str, state: &str, races: &[(&str, &str)], tz: Tz) -> AdapterContext {
    AdapterContext {
        source_id: source.into(),
        state: state.into(),
        races: RaceMap::from_pairs(races.iter().copied()),
        tz,
    }
}

fn find<'a>(rs: &'a [CountyResult], race: &str, fips: &str) -> &'a CountyResult {
    rs.iter()
        .find(|r| r.race_id == race && r.county_fips == fips)
        .unwrap_or_else(|| panic!("no result for {race} {fips}"))
}

fn votes(r: &CountyResult, party: &str) -> u64 {
    r.votes.iter().filter(|c| c.party == party).map(|c| c.votes).sum()
}

#[test]
fn az_flat_rows_merge_per_county_with_party_from_name() {
    let cfg = repo_config();
    let m = repo_mapping(&cfg, "az_sos").compile().expect("compile");
    let c = ctx(
        "az_sos",
        "AZ",
        &[
            ("U.S. Senator", "2024-SEN-AZ"),
            ("President of the United States", "2024-PRES-AZ"),
        ],
        Tz::America__Phoenix,
    );
    let parsed =
        static_json::parse(&fixture("az_sos_2024_all_county_races_trimmed.json"), &m, &c, 3).expect("parse");
    assert_eq!(parsed.unresolved_counties, 0);
    assert_eq!(parsed.results.len(), 3 * 2, "one result per county and contest");
    assert!(
        parsed
            .results
            .iter()
            .all(|r| r.vote_type == VoteType::Mixed as i32)
    );

    let apache = find(&parsed.results, "2024-SEN-AZ", "04001");
    assert_eq!(
        (votes(apache, "DEM"), votes(apache, "REP"), votes(apache, "GRN")),
        (19_901, 11_283, 675)
    );
    assert_eq!(apache.votes.len(), 3);
    assert_eq!((apache.precincts_reporting, apache.precincts_total), (44, 44));
    let pima = find(&parsed.results, "2024-PRES-AZ", "04019");
    assert_eq!((votes(pima, "DEM"), votes(pima, "REP")), (292_450, 214_669));
    assert_eq!((pima.precincts_reporting, pima.precincts_total), (266, 266));
    let santa_cruz = find(&parsed.results, "2024-SEN-AZ", "04023");
    assert_eq!(
        (votes(santa_cruz, "DEM"), votes(santa_cruz, "REP")),
        (11_986, 6_370)
    );
    assert_eq!(
        santa_cruz.total_votes, 0,
        "merged totals are recomputed by validation"
    );
}

#[test]
fn az_version_document_names_the_upload() {
    let v = static_json::VersionedUrls {
        url: String::new(),
        path: susq_enr::adapters::json_map::Path::parse("uploadId").expect("path"),
    };
    assert_eq!(
        static_json::parse_version(&fixture("az_sos_2024_election_47_0.json"), &v.path).expect("version"),
        "12036"
    );
    assert!(static_json::parse_version(br#"{"uploadId":null}"#, &v.path).is_err());
    assert!(static_json::parse_version(br#"{"uploadId":"../x"}"#, &v.path).is_err());
    assert!(static_json::parse_version(b"<html>", &v.path).is_err());
}

fn versioned_config(base: &str) -> EnrConfig {
    EnrConfig::parse(&format!(
        r#"
[[sources]]
id = "az_sos"
state = "AZ"
kind = "static_json"
tz = "America/Phoenix"
version_url = "{base}/data/47/0/election_47_0.json"
version_path = "uploadId"
urls = ["{base}/data/47/0/all_county_races_47_0_en_{{version}}.json"]

[sources.mapping]
rows = "[*]"
race = "ContestName"
county = "CountyName"
candidates = ""
candidate_name = "ChoiceName"
candidate_votes = "ChoiceTotalVotes"
party_from_name = true
merge_rows = true
precincts_reporting = "PrecinctsReported"
precincts_total = "PrecinctsParticipating"

[[races]]
source = "az_sos"
contest = "U.S. Senator"
race_id = "2024-SEN-AZ"
"#
    ))
    .expect("config")
}

#[tokio::test]
async fn versioned_source_follows_upload_id_and_refuses_regressions() {
    let server = MockServer::start().await;
    let version = |id: u64| format!(r#"{{"uploadId":{id},"electionId":47}}"#);
    Mock::given(method("GET"))
        .and(path("/data/47/0/election_47_0.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(version(12036)))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/data/47/0/all_county_races_47_0_en_12036.json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(fixture("az_sos_2024_all_county_races_trimmed.json")),
        )
        .mount(&server)
        .await;
    let cfg = versioned_config(&server.uri());
    let mut src = build_source(&cfg, &cfg.sources[0]).expect("build");
    let out = src.poll().await.expect("first poll");
    assert_eq!(out.results.len(), 3, "Senate in three counties");
    assert_eq!(out.raw.len(), 2, "version document and upload archived");
    assert!(out.raw[1].url.ends_with("_12036.json"));

    // The site rolls back to an older upload: refused, not ingested.
    Mock::given(method("GET"))
        .and(path("/data/47/0/election_47_0.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(version(12035)))
        .mount(&server)
        .await;
    assert!(matches!(
        src.poll().await,
        Err(susq_enr::adapters::SourceError::VersionRegression {
            prev: 12036,
            got: 12035
        })
    ));
}

#[test]
fn version_settings_are_validated() {
    let ok = versioned_config("https://example.invalid");
    assert_eq!(ok.sources.len(), 1);
    let text = |a: &str, b: &str| {
        let base = r#"
[[sources]]
id = "x"
state = "AZ"
kind = "static_json"
VERSION
urls = ["URL"]
[sources.mapping]
rows = "[*]"
race = "r"
county = "c"
candidates = ""
candidate_name = "n"
candidate_votes = "v"
"#;
        base.replace("VERSION", a).replace("URL", b)
    };
    assert!(EnrConfig::parse(&text("", "https://e.invalid/a.json")).is_ok());
    assert!(EnrConfig::parse(&text("", "https://e.invalid/{version}.json")).is_err());
    assert!(
        EnrConfig::parse(&text(
            "version_url = \"https://e.invalid/v\"",
            "https://e.invalid/{version}"
        ))
        .is_err()
    );
    assert!(
        EnrConfig::parse(&text(
            "version_url = \"https://e.invalid/v\"\nversion_path = \"id\"",
            "https://e.invalid/fixed.json"
        ))
        .is_err()
    );
    assert!(
        EnrConfig::parse(&text(
            "version_url = \"https://e.invalid/v\"\nversion_path = \"id\"",
            "https://e.invalid/{version}.json"
        ))
        .is_ok()
    );
}

#[test]
fn repo_sources_build() {
    let cfg = repo_config();
    for id in ["az_sos"] {
        let s = cfg.sources.iter().find(|s| s.id == id).expect(id);
        assert!(build_source(&cfg, s).is_ok(), "{id}");
    }
}
