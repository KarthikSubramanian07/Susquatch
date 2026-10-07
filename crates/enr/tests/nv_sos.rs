//! The `nv_sos` source kind against the Nevada SoS `USandNV.xml`, through the source and race
//! map shipped in `config/enr_sources.toml`. Fixture provenance is in `tests/fixtures/README.md`:
//! * `nv_sos_2024_usandnv_final_trimmed.xml`: the 2024 general's final document (Wayback capture
//!   20250717072406), cut to its USPresident and USSenate pages, bytes otherwise unchanged.

use chrono_tz::Tz;
use susq_contracts::v1::{CountyResult, VoteType};
use susq_enr::adapters::{AdapterContext, build_source, nv_sos};
use susq_enr::config::{EnrConfig, RaceMap, SourceKind};
use susq_enr::pipeline::{Pipeline, PipelineConfig};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture")
}

fn repo_config() -> EnrConfig {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/enr_sources.toml");
    EnrConfig::load(&path).expect("config/enr_sources.toml")
}

fn votes(r: &CountyResult, party: &str) -> u64 {
    r.votes.iter().filter(|c| c.party == party).map(|c| c.votes).sum()
}

fn ctx(races: RaceMap) -> AdapterContext {
    AdapterContext {
        source_id: "nv_sos".into(),
        state: "NV".into(),
        races,
        tz: Tz::America__Los_Angeles,
    }
}

#[test]
fn shipped_source_is_disabled_and_maps_the_governor() {
    let cfg = repo_config();
    let s = cfg.sources.iter().find(|s| s.id == "nv_sos").expect("nv_sos");
    assert!(
        !s.enabled,
        "Imperva challenges every scripted request (2026-10-07)"
    );
    assert!(matches!(s.kind, SourceKind::NvSos { .. }));
    assert!(build_source(&cfg, s).is_ok());
    assert_eq!(cfg.race_map("nv_sos").get("Governor"), Some("2026-GOV-NV"));
}

#[test]
fn final_2024_document_gives_all_17_counties() {
    let races = RaceMap::from_pairs([
        ("United States Senator", "2024-SEN-NV"),
        (
            "President and Vice President of the United States",
            "2024-PRES-NV",
        ),
    ]);
    let parsed =
        nv_sos::parse(&fixture("nv_sos_2024_usandnv_final_trimmed.xml"), &ctx(races), 9).expect("parse");
    assert_eq!(
        parsed.unresolved_counties, 0,
        "Carson City and White Pine resolve"
    );
    assert_eq!(parsed.results.len(), 2 * 17, "RaceTopTwo is not double counted");
    assert!(
        parsed
            .results
            .iter()
            .all(|r| r.vote_type == VoteType::Mixed as i32 && r.precincts_total == 0)
    );
    let of = |race: &str, fips: &str| {
        parsed
            .results
            .iter()
            .find(|r| r.race_id == race && r.county_fips == fips)
            .unwrap_or_else(|| panic!("no {race} {fips}"))
    };
    let clark = of("2024-SEN-NV", "32003");
    assert_eq!((votes(clark, "DEM"), votes(clark, "REP")), (514_662, 441_057));
    assert_eq!(
        clark.votes.len(),
        5,
        "Rosen, Brown, None of these, Hansen, Cunningham"
    );
    let washoe = of("2024-SEN-NV", "32031");
    assert_eq!((votes(washoe, "DEM"), votes(washoe, "REP")), (130_841, 115_713));
    let carson = of("2024-SEN-NV", "32510");
    assert_eq!(votes(carson, "REP"), 15_389);
    let esmeralda = of("2024-PRES-NV", "32009");
    assert_eq!((votes(esmeralda, "REP"), votes(esmeralda, "DEM")), (376, 73));
    // County sums equal the statewide totals the document prints (Rosen 701,105).
    let rosen: u64 = parsed
        .results
        .iter()
        .filter(|r| r.race_id == "2024-SEN-NV")
        .flat_map(|r| r.votes.iter())
        .filter(|v| v.candidate == "ROSEN, JACKY S.")
        .map(|v| v.votes)
        .sum();
    assert_eq!(rosen, 701_105);

    let mut pipe = Pipeline::new(PipelineConfig::default());
    let out = pipe.ingest(parsed.results);
    assert_eq!(out.accepted.len(), 34, "{:?}", out.events);
}

#[test]
fn unmapped_document_yields_nothing() {
    let cfg = repo_config();
    let parsed = nv_sos::parse(
        &fixture("nv_sos_2024_usandnv_final_trimmed.xml"),
        &ctx(cfg.race_map("nv_sos")),
        1,
    )
    .expect("parse");
    assert!(parsed.results.is_empty(), "no Governor race in 2024");
}
