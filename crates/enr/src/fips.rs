//! County name → 5-digit FIPS lookup, built from the Census 2020 county list bundled in
//! `data/county_fips.psv` (public domain).
//!
//! Results sites name counties inconsistently ("DeKalb", "De Kalb", "DEKALB COUNTY",
//! "St. Louis City"), so names are folded before lookup: lowercase ASCII, accents stripped,
//! "saint" → "st", the words county/parish/borough/census area removed, and everything that is
//! not a letter or digit dropped. "City" is kept, so Virginia's Richmond County and Richmond city
//! stay distinct.

use std::collections::HashMap;
use std::sync::OnceLock;

const TABLE: &str = include_str!("../data/county_fips.psv");

/// Lookup table from (state, folded county name) to FIPS.
#[derive(Debug)]
pub struct FipsIndex {
    by_name: HashMap<(String, String), String>,
    state_prefix: HashMap<String, String>,
}

impl FipsIndex {
    /// The process-wide index (parsed once from the bundled table).
    pub fn global() -> &'static FipsIndex {
        static INDEX: OnceLock<FipsIndex> = OnceLock::new();
        INDEX.get_or_init(|| FipsIndex::parse(TABLE))
    }

    fn parse(text: &str) -> Self {
        let mut by_name = HashMap::new();
        let mut state_prefix = HashMap::new();
        for line in text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        {
            let mut parts = line.splitn(3, '|');
            let (Some(st), Some(fips), Some(name)) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            state_prefix
                .entry(st.to_owned())
                .or_insert_with(|| fips[..2].to_owned());
            by_name.insert((st.to_owned(), fold_name(name)), fips.to_owned());
        }
        Self {
            by_name,
            state_prefix,
        }
    }

    /// Resolve a county given as a name, a 5-digit FIPS, or a 3-digit county code.
    ///
    /// Numeric input is checked against the state's FIPS prefix, so a Georgia source can never
    /// produce an Ohio county.
    pub fn resolve(&self, state: &str, county: &str) -> Option<String> {
        let county = county.trim();
        let prefix = self.state_prefix.get(state)?;
        if county.len() == 5 && county.bytes().all(|b| b.is_ascii_digit()) {
            return county.starts_with(prefix.as_str()).then(|| county.to_owned());
        }
        if county.len() == 3 && county.bytes().all(|b| b.is_ascii_digit()) {
            return Some(format!("{prefix}{county}"));
        }
        self.by_name.get(&(state.to_owned(), fold_name(county))).cloned()
    }

    /// Number of counties (county equivalents) known for `state`.
    pub fn county_count(&self, state: &str) -> usize {
        self.by_name.keys().filter(|(s, _)| s == state).count()
    }
}

/// Fold a county name to the lookup form described in the module docs.
pub fn fold_name(name: &str) -> String {
    let mut ascii = String::with_capacity(name.len());
    for c in name.chars() {
        let mapped = match c {
            'á' | 'à' | 'â' | 'ä' | 'Á' => 'a',
            'é' | 'è' | 'ê' | 'É' => 'e',
            'í' | 'ì' | 'Í' => 'i',
            'ñ' | 'Ñ' => 'n',
            'ó' | 'ò' | 'ô' | 'ö' | 'Ó' => 'o',
            'ú' | 'ü' | 'Ú' => 'u',
            other => other,
        };
        ascii.push(mapped.to_ascii_lowercase());
    }
    let words: Vec<&str> = ascii
        .split(|c: char| c.is_whitespace() || c == '-' || c == '.')
        .filter(|w| !w.is_empty())
        .collect();
    let mut out = String::new();
    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        let next = words.get(i + 1).copied();
        match (w, next) {
            ("census", Some("area")) => i += 2,
            ("county" | "parish" | "borough" | "municipality", _) if i > 0 => i += 1,
            ("saint", _) => {
                out.push_str("st");
                i += 1;
            }
            _ => {
                out.extend(w.chars().filter(char::is_ascii_alphanumeric));
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_common_spellings() {
        let idx = FipsIndex::global();
        assert_eq!(idx.resolve("GA", "Fulton").as_deref(), Some("13121"));
        assert_eq!(idx.resolve("GA", "FULTON COUNTY").as_deref(), Some("13121"));
        assert_eq!(idx.resolve("GA", "De Kalb").as_deref(), Some("13089"));
        assert_eq!(idx.resolve("GA", "DeKalb").as_deref(), Some("13089"));
        assert_eq!(idx.resolve("CA", "Los Angeles").as_deref(), Some("06037"));
        assert_eq!(idx.resolve("CA", "San Luis Obispo").as_deref(), Some("06079"));
        assert_eq!(idx.resolve("LA", "Orleans Parish").as_deref(), Some("22071"));
        assert_eq!(idx.resolve("MO", "St. Louis County").as_deref(), Some("29189"));
        assert_eq!(idx.resolve("MO", "St. Louis city").as_deref(), Some("29510"));
        assert_eq!(idx.resolve("NM", "Doña Ana").as_deref(), Some("35013"));
    }

    #[test]
    fn keeps_virginia_cities_distinct() {
        let idx = FipsIndex::global();
        assert_eq!(idx.resolve("VA", "Richmond County").as_deref(), Some("51159"));
        assert_eq!(idx.resolve("VA", "Richmond city").as_deref(), Some("51760"));
    }

    #[test]
    fn numeric_codes_are_checked_against_state() {
        let idx = FipsIndex::global();
        assert_eq!(idx.resolve("GA", "13121").as_deref(), Some("13121"));
        assert_eq!(idx.resolve("GA", "39049"), None);
        assert_eq!(idx.resolve("GA", "121").as_deref(), Some("13121"));
        assert_eq!(idx.resolve("ZZ", "Fulton"), None);
        assert_eq!(idx.resolve("GA", "Atlantis"), None);
    }

    #[test]
    fn state_sizes() {
        let idx = FipsIndex::global();
        assert_eq!(idx.county_count("GA"), 159);
        assert_eq!(idx.county_count("CA"), 58);
    }
}
