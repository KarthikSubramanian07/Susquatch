//! Reporting units: the key a [`susq_contracts::v1::CountyResult`] is filed under.
//!
//! Almost every result is keyed by its county's 5-digit FIPS code. A US House district that a
//! source reports as one total (California's SoS feed prints a "Districtwide Results" row per
//! district) is instead **one reporting unit** with a pseudo id `<ST>-HD<NN>`: `CA-HD22`,
//! `CA-HD01`, `WY-HDAL` for an at-large seat. The id carries a letter and a hyphen, so it can
//! never collide with a FIPS code, and the needle treats it as an opaque county key: the district's
//! prior is a single `CountyPrior` under the same id (`py/susq/replay`, docs/ELECTION_NIGHT.md §5).
//!
//! Why one unit and not the district's county pieces: most competitive districts split counties
//! (CA-22 and CA-48 both do), so county history cannot be attributed to a district, while the
//! district total needs no geography at all.

/// Prefix between the state code and the district number of a House reporting unit.
const HOUSE_UNIT_TAG: &str = "-HD";

/// The House district of a race id, `2026-HOUSE-CA-22` → `("CA", "22")`; `None` for anything else.
///
/// Strict, like the race-id grammar (`py/susq/mapping/races.py`): two digits or `AL`, so a
/// mistyped `2026-HOUSE-CA-1` is not a House race (and `EnrConfig` refuses it).
pub fn house_district(race_id: &str) -> Option<(&str, &str)> {
    let mut parts = race_id.split('-');
    let (_cycle, office, state, district) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if office != "HOUSE" || parts.next().is_some() || !is_state(state) {
        return None;
    }
    let canonical = district == "AL" || (district.len() == 2 && normalize_district(district).is_some());
    canonical.then_some((state, district))
}

/// True for a race id that names a House seat at all (`<cycle>-HOUSE-<anything>-<anything>`),
/// well-formed or not: config validation uses it to refuse malformed House ids.
pub fn looks_like_house_race(race_id: &str) -> bool {
    let parts: Vec<&str> = race_id.split('-').collect();
    parts.len() >= 3 && parts[1] == "HOUSE"
}

/// Reporting-unit id of a House district: `("CA", "22")` → `CA-HD22`, `("CA", "1")` → `CA-HD01`,
/// `("WY", "AL")` → `WY-HDAL`. `None` when the state or district is malformed.
pub fn house_unit(state: &str, district: &str) -> Option<String> {
    if !is_state(state) {
        return None;
    }
    Some(format!(
        "{state}{HOUSE_UNIT_TAG}{}",
        normalize_district(district)?
    ))
}

/// The reporting unit for a House race id (`2026-HOUSE-CA-22` → `CA-HD22`).
pub fn house_unit_for_race(race_id: &str) -> Option<String> {
    let (state, district) = house_district(race_id)?;
    house_unit(state, district)
}

/// True for a well-formed House reporting-unit id of `state` (`CA-HD22` for `CA`).
pub fn is_house_unit(key: &str, state: &str) -> bool {
    key.strip_prefix(state)
        .and_then(|rest| rest.strip_prefix(HOUSE_UNIT_TAG))
        .is_some_and(|d| d == "AL" || (d.len() == 2 && d.bytes().all(|b| b.is_ascii_digit()) && d != "00"))
}

/// True for a 5-digit county FIPS code.
pub fn is_county_fips(key: &str) -> bool {
    key.len() == 5 && key.bytes().all(|b| b.is_ascii_digit())
}

/// A key the pipeline accepts for a result of `state`: a county FIPS or one of its House units.
pub fn is_unit_key(key: &str, state: &str) -> bool {
    is_county_fips(key) || is_house_unit(key, state)
}

fn is_state(s: &str) -> bool {
    s.len() == 2 && s.bytes().all(|b| b.is_ascii_uppercase())
}

/// `"22"` → `"22"`, `"1"` / `"01"` → `"01"`, `"AL"` → `"AL"`; `None` otherwise (0 is not a district).
fn normalize_district(d: &str) -> Option<String> {
    if d == "AL" {
        return Some("AL".to_owned());
    }
    if d.is_empty() || d.len() > 2 || !d.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u8 = d.parse().ok()?;
    (n > 0).then(|| format!("{n:02}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn house_race_ids_map_to_units() {
        assert_eq!(
            house_unit_for_race("2026-HOUSE-CA-22").as_deref(),
            Some("CA-HD22")
        );
        assert_eq!(
            house_unit_for_race("2026-HOUSE-CA-01").as_deref(),
            Some("CA-HD01")
        );
        assert_eq!(
            house_unit_for_race("2026-HOUSE-WY-AL").as_deref(),
            Some("WY-HDAL")
        );
        assert_eq!(house_district("2026-HOUSE-CA-48"), Some(("CA", "48")));
        for not_house in [
            "2026-GOV-CA",
            "2026-SEN-FL-S",
            "2026-HOUSE-CA",
            "2026-HOUSE-CA-00",
            "2026-HOUSE-CA-1",
            "2026-HOUSE-CA-123",
            "2026-HOUSE-ca-22",
            "2026-HOUSE-CA-22-X",
            "2026-CONTROL-HOUSE",
        ] {
            assert_eq!(house_unit_for_race(not_house), None, "{not_house}");
        }
        assert_eq!(house_unit("CA", "1").as_deref(), Some("CA-HD01"));
        assert!(looks_like_house_race("2026-HOUSE-CA-1"));
        assert!(!looks_like_house_race("2026-GOV-CA"));
    }

    #[test]
    fn unit_keys_are_fips_or_house_units_of_the_state() {
        assert!(is_unit_key("06037", "CA"));
        assert!(is_unit_key("CA-HD22", "CA"));
        assert!(is_unit_key("CA-HDAL", "CA"));
        assert!(!is_unit_key("CA-HD22", "NV"), "another state's district");
        assert!(!is_unit_key("CA-HD2", "CA"));
        assert!(!is_unit_key("CA-HD00", "CA"));
        assert!(!is_unit_key("CA-22", "CA"));
        assert!(!is_unit_key("0603", "CA"));
        assert!(!is_unit_key("", "CA"));
    }
}
