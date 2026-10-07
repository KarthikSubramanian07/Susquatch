//! The `csv` source kind against real state files, through the mapping shipped in
//! `config/enr_sources.toml`. Fixture provenance is in `tests/fixtures/README.md`:
//! * `ncsbe_2024_results_pct_trimmed.zip`: NC SBE 2024 general `results_pct_20241105.zip`, rows
//!   for Hyde, New Hanover and Watauga (US PRESIDENT, NC GOVERNOR, plus a few other contests);
//! * `ncsbe_2026_results_pct_template_trimmed.zip`: the 2026 general file NC SBE already serves
//!   (all zeros), US SENATE for Hyde and New Hanover.

use chrono_tz::Tz;
use susq_contracts::v1::{CountyResult, VoteType};
use susq_enr::adapters::csv::{self, CsvMapping};
use susq_enr::adapters::{AdapterContext, build_source};
use susq_enr::config::{EnrConfig, RaceMap, SourceKind};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture")
}

fn repo_config() -> EnrConfig {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/enr_sources.toml");
    EnrConfig::load(&path).expect("config/enr_sources.toml")
}

fn repo_mapping(cfg: &EnrConfig, id: &str) -> CsvMapping {
    let src = cfg.sources.iter().find(|s| s.id == id).expect("source in config");
    let SourceKind::Csv { mapping, .. } = &src.kind else {
        panic!("{id} must be kind = \"csv\"")
    };
    (**mapping).clone()
}

fn nc_ctx(races: &[(&str, &str)]) -> AdapterContext {
    AdapterContext {
        source_id: "nc_sbe".into(),
        state: "NC".into(),
        races: RaceMap::from_pairs(races.iter().copied()),
        tz: Tz::America__New_York,
    }
}

fn find<'a>(rs: &'a [CountyResult], race: &str, fips: &str, vt: VoteType) -> &'a CountyResult {
    rs.iter()
        .find(|r| r.race_id == race && r.county_fips == fips && r.vote_type == vt as i32)
        .unwrap_or_else(|| panic!("no result for {race} {fips} {vt:?}"))
}

fn votes(r: &CountyResult, party: &str) -> u64 {
    r.votes.iter().filter(|c| c.party == party).map(|c| c.votes).sum()
}

const HYDE: &str = "37095";
const NEW_HANOVER: &str = "37129";
const WATAUGA: &str = "37189";

#[test]
fn nc_2024_precinct_file_sums_to_county_by_vote_method() {
    let cfg = repo_config();
    let m = repo_mapping(&cfg, "nc_sbe").compile().expect("compile");
    let c = nc_ctx(&[("US PRESIDENT", "2024-PRES-NC"), ("NC GOVERNOR", "2024-GOV-NC")]);
    let parsed = csv::parse(&fixture("ncsbe_2024_results_pct_trimmed.zip"), &m, &c, 5).expect("parse");
    assert_eq!(parsed.unresolved_counties, 0);
    // 3 counties x 2 contests x {E-day, early, mail, provisional}.
    assert_eq!(parsed.results.len(), 3 * 2 * 4);
    assert!(
        parsed
            .results
            .iter()
            .all(|r| r.source == "nc_sbe" && r.state == "NC")
    );

    // Hyde County, US PRESIDENT, summed over its precincts and pseudo-precincts
    // (official NC SBE totals: Trump 1,465, Harris 931).
    let pres = "2024-PRES-NC";
    let eday = find(&parsed.results, pres, HYDE, VoteType::ElectionDay);
    assert_eq!((votes(eday, "REP"), votes(eday, "DEM")), (761, 377));
    let early = find(&parsed.results, pres, HYDE, VoteType::EarlyInPerson);
    assert_eq!((votes(early, "REP"), votes(early, "DEM")), (654, 493));
    let mail = find(&parsed.results, pres, HYDE, VoteType::Mail);
    assert_eq!((votes(mail, "REP"), votes(mail, "DEM")), (39, 55));
    let prov = find(&parsed.results, pres, HYDE, VoteType::Provisional);
    assert_eq!((votes(prov, "REP"), votes(prov, "DEM")), (11, 6));
    assert_eq!(eday.votes.len(), 7, "six tickets plus write-ins");
    assert_eq!(votes(eday, "GRN"), 5, "GRE normalizes to GRN");
    // Seven real precincts, all reporting; pseudo-precincts are not counted.
    assert_eq!((eday.precincts_reporting, eday.precincts_total), (7, 7));
    assert_eq!(eday.observed_at_ns, 5);

    // Two-word county name, NC GOVERNOR: Stein 80,834, Robinson 46,537 across methods.
    let gov = "2024-GOV-NC";
    let total = |fips: &str, party: &str| -> u64 {
        parsed
            .results
            .iter()
            .filter(|r| r.race_id == gov && r.county_fips == fips)
            .map(|r| votes(r, party))
            .sum()
    };
    assert_eq!(
        (total(NEW_HANOVER, "DEM"), total(NEW_HANOVER, "REP")),
        (80_834, 46_537)
    );
    assert_eq!((total(WATAUGA, "DEM"), total(WATAUGA, "REP")), (19_021, 11_697));
    assert_eq!(
        votes(
            find(&parsed.results, gov, NEW_HANOVER, VoteType::EarlyInPerson),
            "DEM"
        ),
        56_297
    );
    let nh = find(&parsed.results, gov, NEW_HANOVER, VoteType::Mail);
    assert_eq!((nh.precincts_reporting, nh.precincts_total), (41, 41));
}

#[test]
fn nc_2026_template_maps_us_senate_with_nothing_reporting() {
    let cfg = repo_config();
    let m = repo_mapping(&cfg, "nc_sbe").compile().expect("compile");
    // The shipped race map, so the 2026 contest name is checked against the real file.
    let c = nc_ctx(&[]);
    let c = AdapterContext {
        races: cfg.race_map("nc_sbe"),
        ..c
    };
    let parsed =
        csv::parse(&fixture("ncsbe_2026_results_pct_template_trimmed.zip"), &m, &c, 1).expect("parse");
    assert_eq!(parsed.results.len(), 2 * 4, "2 counties x 4 methods");
    for r in &parsed.results {
        assert_eq!(r.race_id, "2026-SEN-NC");
        assert!(r.votes.iter().all(|v| v.votes == 0));
        assert_eq!(r.precincts_reporting, 0);
    }
    let hyde = find(&parsed.results, "2026-SEN-NC", HYDE, VoteType::ElectionDay);
    let mut names: Vec<_> = hyde
        .votes
        .iter()
        .map(|v| (v.candidate.as_str(), v.party.as_str()))
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            ("Michael Dublin", "GRN"),
            ("Michael Whatley", "REP"),
            ("Roy Cooper", "DEM"),
            ("Shannon W. Bray", "LIB"),
        ]
    );
    assert_eq!(hyde.precincts_total, 7);
    let nh = find(&parsed.results, "2026-SEN-NC", NEW_HANOVER, VoteType::Mail);
    assert_eq!(nh.precincts_total, 41);
}

#[test]
fn nc_unzipped_body_and_corrupt_zip() {
    let cfg = repo_config();
    let m = repo_mapping(&cfg, "nc_sbe").compile().expect("compile");
    let c = nc_ctx(&[("US PRESIDENT", "2024-PRES-NC")]);
    let zipped = fixture("ncsbe_2024_results_pct_trimmed.zip");
    let text = csv::unzip_entry(&zipped, Some(".txt")).expect("unzip");
    let plain = csv::parse(&text, &m, &c, 1).expect("plain text parses too");
    let zipped = csv::parse(&zipped, &m, &c, 1).expect("zip");
    assert_eq!(plain.results, zipped.results);
    assert!(csv::unzip_entry(&fixture("ncsbe_2024_results_pct_trimmed.zip"), Some(".csv")).is_err());
    let truncated = &fixture("ncsbe_2024_results_pct_trimmed.zip")[..4000];
    assert!(csv::parse(truncated, &m, &c, 1).is_err());
    // A renamed column fails loudly rather than reading zeros.
    let renamed = String::from_utf8(text)
        .expect("ascii")
        .replacen("Early Voting", "One Stop", 1);
    assert!(csv::parse(renamed.as_bytes(), &m, &c, 1).is_err());
}

#[test]
fn nc_source_builds_from_repo_config() {
    let cfg = repo_config();
    let src = cfg.sources.iter().find(|s| s.id == "nc_sbe").expect("nc_sbe");
    assert!(build_source(&cfg, src).is_ok());
    assert!(
        cfg.race_sources()
            .get("2026-SEN-NC")
            .is_some_and(|s| s.contains("nc_sbe"))
            == src.enabled
    );
}
