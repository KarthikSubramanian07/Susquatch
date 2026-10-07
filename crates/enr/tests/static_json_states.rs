//! `static_json` sources for Arizona and Texas against real payloads, through the mappings
//! shipped in `config/enr_sources.toml`. Fixture provenance is in `tests/fixtures/README.md`:
//! * `az_sos_2024_all_county_races_trimmed.json` + `az_sos_2024_election_47_0.json`: AZ SoS
//!   2024 general (eid 47, upload 12036), Apache, Pima and Santa Cruz counties;
//! * `tx_sos_2024_county_trimmed.json`: TX SoS legacy site, 2024 general County.json (Anderson,
//!   Harris, El Paso);
//! * `tx_civix_2026_primary_countyinfo_trimmed.json`: TX Civix countyInfo for the 2026 Republican
//!   primary (Mitchell, Harris), re-wrapped as `{"upload": "<base64>"}` after trimming;
//! * `oh_sos_2024_president_by_county_election_night_trimmed.json`: OH SoS API, 2024 President by
//!   county at 11:09 pm ET on election night (Wayback), Adams, Franklin and Van Wert;
//! * `pa_sos_2024_us_senator_county_break_trimmed.json`: PA DoS `GetCountyBreak`, 2024 general US
//!   Senator (final), Adams, McKean and Philadelphia, still double-encoded as the site serves it.

use chrono_tz::Tz;
use susq_contracts::v1::{CountyResult, VoteType};
use susq_enr::adapters::json_map::JsonMapping;
use susq_enr::adapters::{AdapterContext, build_source, static_json};
use susq_enr::config::{EnrConfig, RaceMap, SourceKind};
use susq_enr::pipeline::{Pipeline, PipelineConfig};
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

#[test]
fn tx_legacy_2024_county_file_maps_by_name() {
    let cfg = repo_config();
    // The legacy site names contests `ON` and is not base64-wrapped; otherwise identical.
    let mut mapping = repo_mapping(&cfg, "tx_sos");
    mapping.unwrap_base64 = None;
    mapping.race = "ON".into();
    let m = mapping.compile().expect("compile");
    let c = ctx(
        "tx_sos",
        "TX",
        &[
            ("U. S. SENATOR", "2024-SEN-TX"),
            ("PRESIDENT/VICE-PRESIDENT", "2024-PRES-TX"),
        ],
        Tz::America__Chicago,
    );
    let parsed = static_json::parse(&fixture("tx_sos_2024_county_trimmed.json"), &m, &c, 1).expect("parse");
    assert_eq!(parsed.unresolved_counties, 0);
    assert_eq!(parsed.results.len(), 3 * 2);
    let harris = find(&parsed.results, "2024-SEN-TX", "48201");
    assert_eq!((votes(harris, "DEM"), votes(harris, "REP")), (841_784, 666_027));
    assert_eq!(
        (harris.precincts_reporting, harris.precincts_total),
        (1_173, 1_173)
    );
    let el_paso = find(&parsed.results, "2024-PRES-TX", "48141");
    assert_eq!((votes(el_paso, "DEM"), votes(el_paso, "REP")), (143_156, 105_124));
    let anderson = find(&parsed.results, "2024-SEN-TX", "48001");
    assert_eq!((votes(anderson, "REP"), votes(anderson, "DEM")), (15_022, 3_872));
    assert!(
        parsed
            .results
            .iter()
            .all(|r| r.vote_type == VoteType::Mixed as i32)
    );
}

#[test]
fn tx_civix_base64_payload_with_shipped_mapping_and_race_map() {
    let cfg = repo_config();
    let m = repo_mapping(&cfg, "tx_sos").compile().expect("compile");
    let c = AdapterContext {
        races: cfg.race_map("tx_sos"),
        ..ctx("tx_sos", "TX", &[], Tz::America__Chicago)
    };
    let parsed = static_json::parse(
        &fixture("tx_civix_2026_primary_countyinfo_trimmed.json"),
        &m,
        &c,
        1,
    )
    .expect("parse");
    assert_eq!(parsed.unresolved_counties, 0);
    assert_eq!(
        parsed.results.len(),
        2 * 2,
        "Senate and governor, Mitchell and Harris"
    );
    let harris_sen = find(&parsed.results, "2026-SEN-TX", "48201");
    let cornyn = harris_sen
        .votes
        .iter()
        .find(|v| v.candidate == "JOHN CORNYN (I)")
        .expect("Cornyn");
    assert_eq!((cornyn.votes, cornyn.party.as_str()), (76_361, "REP"));
    assert_eq!(
        (harris_sen.precincts_reporting, harris_sen.precincts_total),
        (1_225, 1_225)
    );
    let mitchell_gov = find(&parsed.results, "2026-GOV-TX", "48335");
    assert_eq!(
        votes(mitchell_gov, "REP"),
        mitchell_gov.votes.iter().map(|v| v.votes).sum::<u64>()
    );
    assert!(
        mitchell_gov
            .votes
            .iter()
            .any(|v| v.candidate == "GREG ABBOTT (I)" && v.votes == 669)
    );
    // Garbage inside the wrapper fails the payload instead of yielding nothing.
    assert!(static_json::parse(br#"{"upload":"!!!"}"#, &m, &c, 1).is_err());
    assert!(static_json::parse(br#"{"other":"e30="}"#, &m, &c, 1).is_err());
}

#[test]
fn base64_decoder_matches_reference_vectors() {
    for (enc, dec) in [
        ("", ""),
        ("Zg==", "f"),
        ("Zm8=", "fo"),
        ("Zm9v", "foo"),
        ("Zm9vYg", "foob"),
        ("Zm9vYmE=", "fooba"),
        ("Zm9v\nYmFy", "foobar"),
    ] {
        assert_eq!(
            static_json::decode_base64(enc).expect(enc),
            dec.as_bytes(),
            "{enc}"
        );
    }
    assert!(static_json::decode_base64("Zm9v*").is_err());
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
    for id in ["az_sos", "tx_sos", "oh_sos", "pa_sos"] {
        let s = cfg.sources.iter().find(|s| s.id == id).expect(id);
        assert!(build_source(&cfg, s).is_ok(), "{id}");
    }
}

#[test]
fn oh_election_night_capture_maps_partial_counts() {
    let cfg = repo_config();
    let m = repo_mapping(&cfg, "oh_sos").compile().expect("compile");
    let c = ctx(
        "oh_sos",
        "OH",
        &[("President and Vice President", "2024-PRES-OH")],
        Tz::America__New_York,
    );
    let parsed = static_json::parse(
        &fixture("oh_sos_2024_president_by_county_election_night_trimmed.json"),
        &m,
        &c,
        1,
    )
    .expect("parse");
    assert_eq!(parsed.unresolved_counties, 0);
    assert_eq!(parsed.results.len(), 3);
    let franklin = find(&parsed.results, "2024-PRES-OH", "39049");
    assert_eq!(
        (votes(franklin, "DEM"), votes(franklin, "REP")),
        (364_702, 200_818)
    );
    assert_eq!(
        (franklin.precincts_reporting, franklin.precincts_total),
        (868, 888),
        "mid-count"
    );
    let adams = find(&parsed.results, "2024-PRES-OH", "39001");
    assert_eq!((votes(adams, "REP"), votes(adams, "DEM")), (10_056, 2_073));
    let van_wert = find(&parsed.results, "2024-PRES-OH", "39161");
    assert_eq!(votes(van_wert, "REP"), 11_535);
    assert_eq!(van_wert.votes.len(), 14, "every ticket and write-in");
}

#[test]
fn pa_double_encoded_county_break_splits_methods_per_candidate() {
    let cfg = repo_config();
    let mut mapping = repo_mapping(&cfg, "pa_sos");
    assert_eq!(
        mapping.contest.as_deref(),
        Some("Governor"),
        "shipped for officeId=3"
    );
    // The fixture is the 2024 US Senator break (officeId=2); same shape, another fixed name.
    mapping.contest = Some("United States Senator".into());
    let m = mapping.compile().expect("compile");
    let c = ctx(
        "pa_sos",
        "PA",
        &[("United States Senator", "2024-SEN-PA")],
        Tz::America__New_York,
    );
    let parsed = static_json::parse(
        &fixture("pa_sos_2024_us_senator_county_break_trimmed.json"),
        &m,
        &c,
        1,
    )
    .expect("parse");
    assert_eq!(
        parsed.unresolved_counties, 0,
        "McKean resolves despite the casing"
    );
    assert_eq!(
        parsed.results.len(),
        3 * 3,
        "three counties x election day, mail, provisional"
    );
    let of = |fips: &str, vt: VoteType| {
        parsed
            .results
            .iter()
            .find(|r| r.county_fips == fips && r.vote_type == vt as i32)
            .unwrap_or_else(|| panic!("no {fips} {vt:?}"))
    };
    let philly_eday = of("42101", VoteType::ElectionDay);
    assert_eq!(
        (votes(philly_eday, "DEM"), votes(philly_eday, "REP")),
        (366_221, 104_849)
    );
    let philly_mail = of("42101", VoteType::Mail);
    assert_eq!(
        (votes(philly_mail, "DEM"), votes(philly_mail, "REP")),
        (166_272, 22_601)
    );
    let philly_prov = of("42101", VoteType::Provisional);
    assert_eq!(
        (votes(philly_prov, "DEM"), votes(philly_prov, "REP")),
        (9_074, 1_648)
    );
    // Methods add up to the published candidate totals (Casey 541,567 in Philadelphia).
    let casey: u64 = parsed
        .results
        .iter()
        .filter(|r| r.county_fips == "42101")
        .flat_map(|r| r.votes.iter())
        .filter(|v| v.candidate == "ROBERT P CASEY JR")
        .map(|v| v.votes)
        .sum();
    assert_eq!(casey, 541_567);
    let adams_eday = of("42001", VoteType::ElectionDay);
    assert_eq!(
        adams_eday.votes.len(),
        5,
        "DEM, REP, LIB, GRN and the Constitution party"
    );
    assert_eq!(votes(adams_eday, "OTH"), 223, "CST is not a major code");
    let mckean_mail = of("42083", VoteType::Mail);
    assert_eq!(votes(mckean_mail, "REP"), 1_960);
    assert!(parsed.results.iter().all(|r| r.race_id == "2024-SEN-PA"));

    // The pipeline accepts every result (validation recomputes the zero totals).
    let mut pipe = Pipeline::new(PipelineConfig::default());
    let out = pipe.ingest(parsed.results.clone());
    assert_eq!(out.accepted.len(), 9, "{:?}", out.events);
}

#[test]
fn pa_empty_string_is_no_results_and_garbage_fails() {
    let cfg = repo_config();
    let m = repo_mapping(&cfg, "pa_sos").compile().expect("compile");
    let c = AdapterContext {
        races: cfg.race_map("pa_sos"),
        ..ctx("pa_sos", "PA", &[], Tz::America__New_York)
    };
    assert_eq!(c.races.get("Governor"), Some("2026-GOV-PA"));
    // What the 2026 general id (117/G) serves before the election is loaded (2026-10-07).
    let empty = static_json::parse(br#""""#, &m, &c, 1).expect("empty string");
    assert!(empty.results.is_empty());
    // The same document shape as the fixture, mapped through the shipped Governor mapping.
    let gov = br#""{\"Election\":{\"Statewide\":[{\"YORK\":[{\"CountyName\":\"YORK\",\"PartyName\":\"DEM\",\"CandidateName\":\"JOSH SHAPIRO\",\"Votes\":\"12\",\"ElectionDayVotes\":\"10\",\"MailInVotes\":\"2\",\"ProvisionalVotes\":\"0\"}]}]}}""#;
    let parsed = static_json::parse(gov, &m, &c, 1).expect("parse");
    assert_eq!(parsed.results.len(), 3);
    assert!(
        parsed
            .results
            .iter()
            .all(|r| r.race_id == "2026-GOV-PA" && r.county_fips == "42133")
    );
    // Not a string, a string that is not JSON, or methods above the total: payload fails.
    assert!(static_json::parse(br#"{"Election":{}}"#, &m, &c, 1).is_err());
    assert!(static_json::parse(br#""not json""#, &m, &c, 1).is_err());
    let over = br#""{\"Election\":{\"Statewide\":[{\"YORK\":[{\"CountyName\":\"YORK\",\"PartyName\":\"DEM\",\"CandidateName\":\"X\",\"Votes\":\"1\",\"ElectionDayVotes\":\"10\"}]}]}}""#;
    assert!(static_json::parse(over, &m, &c, 1).is_err());
}
