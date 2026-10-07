//! A small jq-like path language for mapping arbitrary results JSON to county results, so a new
//! state site can be added from config alone.
//!
//! Paths are dot-separated steps. A step is a key (`contests`), an index (`[0]`), or a wildcard
//! (`*` or `[*]`) that iterates an array's items or an object's values. Keys may carry index or
//! wildcard suffixes: `contests[*]`, `rows[2]`. Keys containing dots are not supported.
//!
//! Field paths are resolved against a scope chosen by prefix:
//! * `$.a.b` from the document root;
//! * `^N.a` from the element bound at the N-th wildcard of `rows` (0-based), e.g. the contest
//!   enclosing a county row;
//! * anything else from the current element (the row, or the candidate for candidate fields).
//!
//! ```toml
//! [sources.mapping]
//! rows = "contests[*].counties[*]"
//! race = "^0.name"
//! county = "name"
//! candidates = "results[*]"
//! candidate_name = "name"
//! candidate_party = "party"
//! candidate_votes = "votes"
//! ```

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

/// Mapping from a JSON document to county rows. See the module docs for path syntax.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsonMapping {
    /// Iterates one row per (contest, county[, vote type]).
    pub rows: String,
    /// Contest name, looked up in the race map. Leave empty when `contest` is set.
    #[serde(default)]
    pub race: String,
    /// Fixed contest name for documents that hold one contest without printing its name
    /// (Pennsylvania's `GetCountyBreak` is fetched per office id). Exclusive with `race`.
    #[serde(default)]
    pub contest: Option<String>,
    /// County name, 5-digit FIPS or 3-digit county code.
    pub county: String,
    /// Iterates candidates within a row.
    pub candidates: String,
    pub candidate_name: String,
    #[serde(default)]
    pub candidate_party: Option<String>,
    pub candidate_votes: String,
    /// Vote-type label of the row (optional; absent means MIXED).
    #[serde(default)]
    pub vote_type: Option<String>,
    /// Iterates a per-candidate vote-type breakdown (e.g. Enhanced Voting's
    /// `ballotOptions[*].groupResults[*]`). When set, each row is pivoted into one result per
    /// vote type, labels that classify to the same type are summed, and `total_votes` is ignored
    /// (it covers all types).
    #[serde(default)]
    pub candidate_breakdown: Option<String>,
    #[serde(default)]
    pub breakdown_label: Option<String>,
    #[serde(default)]
    pub breakdown_votes: Option<String>,
    /// Per-candidate scalar fields that each hold one vote method, keyed by a vote-type label,
    /// e.g. `{ "Election Day" = "ElectionDayVotes", Mail = "MailInVotes" }` (Pennsylvania).
    /// Each candidate is pivoted into one result per type, like `candidate_breakdown`; whatever
    /// part of `candidate_votes` the fields do not cover (a missing or null field, or a county
    /// that publishes only totals) is MIXED. Exclusive with `candidate_breakdown`.
    #[serde(default)]
    pub candidate_vote_fields: BTreeMap<String, String>,
    #[serde(default)]
    pub total_votes: Option<String>,
    #[serde(default)]
    pub precincts_reporting: Option<String>,
    #[serde(default)]
    pub precincts_total: Option<String>,
    /// Source timestamp (RFC 3339 or Clarity-style local time).
    #[serde(default)]
    pub source_ts: Option<String>,
    /// Root path to a string holding the real document, base64-encoded JSON (Texas' Civix
    /// results API wraps every payload as `{"upload": "<base64>"}`). Paths then apply to the
    /// decoded document.
    #[serde(default)]
    pub unwrap_base64: Option<String>,
    /// Path (`$` for the root) to a string holding the real document as JSON text: Pennsylvania's
    /// API serialises every response twice, so the body is one JSON string. An empty string is a
    /// document with no rows (what Pennsylvania serves before an election is loaded). Exclusive
    /// with `unwrap_base64`.
    #[serde(default)]
    pub unwrap_json: Option<String>,
    /// Sum rows that land on the same (race, county, vote type) into one result. Needed for flat
    /// feeds with one row per (county, contest, candidate), such as Arizona's.
    #[serde(default)]
    pub merge_rows: bool,
    /// Without a party path (or when it is empty), take the party from a trailing
    /// parenthesised code in the candidate name: `"Gallego, Ruben (DEM)"` gives `DEM`.
    #[serde(default)]
    pub party_from_name: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Key(String),
    Index(usize),
    All,
}

/// A parsed path with its scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path {
    scope: Scope,
    steps: Vec<Step>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Root,
    Binding(usize),
    Current,
}

/// Error in a path expression (a config error, reported at start-up).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("bad path {path:?}: {reason}")]
pub struct PathError {
    pub path: String,
    pub reason: String,
}

impl Path {
    /// Parse a path expression.
    pub fn parse(expr: &str) -> Result<Self, PathError> {
        let err = |reason: &str| PathError {
            path: expr.to_owned(),
            reason: reason.to_owned(),
        };
        let expr_trim = expr.trim();
        let (scope, rest) = if let Some(rest) = expr_trim.strip_prefix('$') {
            (Scope::Root, rest.strip_prefix('.').unwrap_or(rest))
        } else if let Some(rest) = expr_trim.strip_prefix('^') {
            let (num, tail) = rest.split_once('.').unwrap_or((rest, ""));
            let n = num
                .parse::<usize>()
                .map_err(|_| err("^ must be followed by a binding number"))?;
            (Scope::Binding(n), tail)
        } else {
            (Scope::Current, expr_trim)
        };
        let mut steps = Vec::new();
        for piece in rest.split('.').filter(|p| !p.is_empty()) {
            let (key, mut brackets) = match piece.find('[') {
                Some(i) => (&piece[..i], &piece[i..]),
                None => (piece, ""),
            };
            match key {
                "" => {}
                "*" => steps.push(Step::All),
                k => steps.push(Step::Key(k.to_owned())),
            }
            while !brackets.is_empty() {
                let close = brackets.find(']').ok_or_else(|| err("unclosed ["))?;
                let inner = &brackets[1..close];
                if inner == "*" {
                    steps.push(Step::All);
                } else {
                    steps.push(Step::Index(
                        inner.parse().map_err(|_| err("index must be * or a number"))?,
                    ));
                }
                brackets = &brackets[close + 1..];
                if !brackets.is_empty() && !brackets.starts_with('[') {
                    return Err(err("unexpected text after ]"));
                }
            }
        }
        Ok(Self { scope, steps })
    }

    fn wildcards(&self) -> usize {
        self.steps.iter().filter(|s| **s == Step::All).count()
    }
}

/// Expand a row path from the root, returning each row with the elements bound at each wildcard.
pub fn expand_rows<'a>(root: &'a Value, path: &Path) -> Vec<(&'a Value, Vec<&'a Value>)> {
    let mut frontier: Vec<(&Value, Vec<&Value>)> = vec![(root, Vec::new())];
    for step in &path.steps {
        let mut next = Vec::new();
        for (v, binds) in frontier {
            match step {
                Step::Key(k) => {
                    if let Some(child) = v.get(k.as_str()) {
                        next.push((child, binds));
                    }
                }
                Step::Index(i) => {
                    if let Some(child) = v.get(*i) {
                        next.push((child, binds));
                    }
                }
                Step::All => {
                    let children: Vec<&Value> = match v {
                        Value::Array(a) => a.iter().collect(),
                        Value::Object(o) => o.values().collect(),
                        _ => Vec::new(),
                    };
                    for c in children {
                        let mut b = binds.clone();
                        b.push(c);
                        next.push((c, b));
                    }
                }
            }
        }
        frontier = next;
    }
    frontier
}

/// Evaluate a path to all matching values, given the root, the wildcard bindings and the
/// current element.
pub fn select<'a>(path: &Path, root: &'a Value, binds: &[&'a Value], current: &'a Value) -> Vec<&'a Value> {
    let start = match path.scope {
        Scope::Root => root,
        Scope::Current => current,
        Scope::Binding(n) => match binds.get(n) {
            Some(v) => v,
            None => return Vec::new(),
        },
    };
    expand_rows(
        start,
        &Path {
            scope: Scope::Current,
            steps: path.steps.clone(),
        },
    )
    .into_iter()
    .map(|(v, _)| v)
    .collect()
}

/// Evaluate a path that must yield exactly one value.
pub fn select_one<'a>(
    path: &Path,
    root: &'a Value,
    binds: &[&'a Value],
    current: &'a Value,
) -> Option<&'a Value> {
    let found = select(path, root, binds, current);
    (found.len() == 1).then(|| found[0])
}

/// Text form of a scalar (strings as-is, numbers formatted).
pub fn as_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Parsed and checked form of a [`JsonMapping`].
#[derive(Debug, Clone)]
pub struct CompiledMapping {
    pub rows: Path,
    /// Where the contest name comes from: a path, or the fixed `contest` name.
    pub race: Option<Path>,
    pub contest: Option<String>,
    pub county: Path,
    pub candidates: Path,
    pub candidate_name: Path,
    pub candidate_party: Option<Path>,
    pub candidate_votes: Path,
    pub vote_type: Option<Path>,
    /// (breakdown iterator, label, votes), all relative to the candidate / breakdown element.
    pub breakdown: Option<(Path, Path, Path)>,
    /// (vote-type label, path relative to the candidate), from `candidate_vote_fields`.
    pub vote_fields: Vec<(String, Path)>,
    pub total_votes: Option<Path>,
    pub precincts_reporting: Option<Path>,
    pub precincts_total: Option<Path>,
    pub source_ts: Option<Path>,
    pub unwrap_base64: Option<Path>,
    pub unwrap_json: Option<Path>,
    pub merge_rows: bool,
    pub party_from_name: bool,
}

impl JsonMapping {
    /// Parse every path and check `^N` bindings exist in `rows`.
    pub fn compile(&self) -> Result<CompiledMapping, PathError> {
        let opt = |p: &Option<String>| p.as_deref().map(Path::parse).transpose();
        let rows = Path::parse(&self.rows)?;
        let breakdown = match (
            &self.candidate_breakdown,
            &self.breakdown_label,
            &self.breakdown_votes,
        ) {
            (None, None, None) => None,
            (Some(it), Some(label), Some(votes)) => {
                Some((Path::parse(it)?, Path::parse(label)?, Path::parse(votes)?))
            }
            _ => {
                return Err(PathError {
                    path: "candidate_breakdown".into(),
                    reason: "candidate_breakdown, breakdown_label and breakdown_votes go together".into(),
                });
            }
        };
        let bad = |path: &str, reason: &str| PathError {
            path: path.into(),
            reason: reason.into(),
        };
        let contest = self
            .contest
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_owned);
        let race = match (self.race.trim().is_empty(), &contest) {
            (false, None) => Some(Path::parse(&self.race)?),
            (true, Some(_)) => None,
            (true, None) => return Err(bad("race", "set race (a path) or contest (a fixed name)")),
            (false, Some(_)) => return Err(bad("contest", "race and contest are exclusive")),
        };
        if breakdown.is_some() && !self.candidate_vote_fields.is_empty() {
            return Err(bad(
                "candidate_vote_fields",
                "candidate_vote_fields and candidate_breakdown are exclusive",
            ));
        }
        if self.unwrap_base64.is_some() && self.unwrap_json.is_some() {
            return Err(bad("unwrap_json", "unwrap_json and unwrap_base64 are exclusive"));
        }
        let vote_fields = self
            .candidate_vote_fields
            .iter()
            .map(|(label, p)| Ok((label.clone(), Path::parse(p)?)))
            .collect::<Result<Vec<_>, PathError>>()?;
        let m = CompiledMapping {
            breakdown,
            vote_fields,
            race,
            contest,
            county: Path::parse(&self.county)?,
            candidates: Path::parse(&self.candidates)?,
            candidate_name: Path::parse(&self.candidate_name)?,
            candidate_party: opt(&self.candidate_party)?,
            candidate_votes: Path::parse(&self.candidate_votes)?,
            vote_type: opt(&self.vote_type)?,
            total_votes: opt(&self.total_votes)?,
            precincts_reporting: opt(&self.precincts_reporting)?,
            precincts_total: opt(&self.precincts_total)?,
            source_ts: opt(&self.source_ts)?,
            unwrap_base64: opt(&self.unwrap_base64)?,
            unwrap_json: opt(&self.unwrap_json)?,
            merge_rows: self.merge_rows,
            party_from_name: self.party_from_name,
            rows,
        };
        let n = m.rows.wildcards();
        let all = [
            m.race.as_ref(),
            Some(&m.county),
            Some(&m.candidates),
            m.vote_type.as_ref(),
            m.total_votes.as_ref(),
        ];
        for p in all.into_iter().flatten() {
            if let Scope::Binding(b) = p.scope
                && b >= n
            {
                return Err(PathError {
                    path: format!("^{b}"),
                    reason: format!("rows has only {n} wildcard(s)"),
                });
            }
        }
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_paths() {
        let p = Path::parse("contests[*].counties[*]").expect("parse");
        assert_eq!(
            p.steps,
            vec![
                Step::Key("contests".into()),
                Step::All,
                Step::Key("counties".into()),
                Step::All
            ]
        );
        assert_eq!(Path::parse("$.meta.ts").expect("parse").scope, Scope::Root);
        assert_eq!(Path::parse("^1.name").expect("parse").scope, Scope::Binding(1));
        assert_eq!(
            Path::parse("a[2][*]").expect("parse").steps,
            vec![Step::Key("a".into()), Step::Index(2), Step::All]
        );
        assert!(Path::parse("a[x]").is_err());
        assert!(Path::parse("a[1").is_err());
        assert!(Path::parse("^x.name").is_err());
    }

    #[test]
    fn expands_with_bindings() {
        let doc = json!({"contests": [
            {"name": "Gov", "counties": [{"n": "A"}, {"n": "B"}]},
            {"name": "Sen", "counties": [{"n": "C"}]}
        ]});
        let rows = expand_rows(&doc, &Path::parse("contests[*].counties[*]").expect("parse"));
        assert_eq!(rows.len(), 3);
        let race = Path::parse("^0.name").expect("parse");
        let names: Vec<_> = rows
            .iter()
            .map(|(row, b)| select_one(&race, &doc, b, row).and_then(as_text))
            .collect();
        assert_eq!(
            names,
            vec![Some("Gov".into()), Some("Gov".into()), Some("Sen".into())]
        );
    }

    #[test]
    fn object_wildcards_iterate_values() {
        let doc = json!({"byCounty": {"001": {"v": 1}, "003": {"v": 2}}});
        let rows = expand_rows(&doc, &Path::parse("byCounty.*").expect("parse"));
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn compile_checks_bindings() {
        let mut m = JsonMapping {
            rows: "a[*]".into(),
            race: "^1.name".into(),
            contest: None,
            county: "c".into(),
            candidates: "x[*]".into(),
            candidate_name: "n".into(),
            candidate_party: None,
            candidate_votes: "v".into(),
            vote_type: None,
            candidate_breakdown: None,
            breakdown_label: None,
            breakdown_votes: None,
            candidate_vote_fields: Default::default(),
            total_votes: None,
            precincts_reporting: None,
            precincts_total: None,
            source_ts: None,
            unwrap_base64: None,
            unwrap_json: None,
            merge_rows: false,
            party_from_name: false,
        };
        assert!(m.compile().is_err());
        m.race = "^0.name".into();
        assert!(m.compile().is_ok());
        m.candidate_breakdown = Some("groups[*]".into());
        assert!(m.compile().is_err(), "breakdown needs label and votes too");
        m.candidate_breakdown = None;

        // A fixed contest name replaces the race path; exactly one of the two.
        m.contest = Some("Governor".into());
        assert!(m.compile().is_err(), "race and contest are exclusive");
        m.race = String::new();
        let c = m.compile().expect("contest alone");
        assert!(c.race.is_none());
        assert_eq!(c.contest.as_deref(), Some("Governor"));
        m.contest = Some("  ".into());
        assert!(m.compile().is_err(), "neither race nor contest");
        m.contest = Some("Governor".into());

        // Scalar vote-method fields exclude a breakdown iterator; the two unwraps exclude each other.
        m.candidate_vote_fields = [("Mail".to_owned(), "MailInVotes".to_owned())].into();
        assert_eq!(m.compile().expect("vote fields").vote_fields.len(), 1);
        m.candidate_breakdown = Some("groups[*]".into());
        m.breakdown_label = Some("l".into());
        m.breakdown_votes = Some("v".into());
        assert!(m.compile().is_err(), "vote fields and breakdown are exclusive");
        m.candidate_breakdown = None;
        m.breakdown_label = None;
        m.breakdown_votes = None;
        m.unwrap_json = Some("$".into());
        assert!(m.compile().is_ok());
        m.unwrap_base64 = Some("upload".into());
        assert!(m.compile().is_err(), "one unwrap at a time");
    }
}
