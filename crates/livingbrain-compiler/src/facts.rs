//! Temporal facts the compiler emits — the adapter onto the bi-temporal
//! facts layer (`docs/temporal-facts.md`, format v1).
//!
//! The brain is chronological: a session that changes something
//! established earlier ("the project moved from Supabase to D1", "that
//! command no longer works") must not overwrite what was true before. The
//! pages say so in prose with dates; the facts layer says so in structure —
//! subject + attribute + value with a validity range, and the server orders,
//! supersedes and disputes. The compiler's model emits facts in exactly the
//! v1 emitter shape, [`Fact`], which is validated here against the spec's
//! JSON Schema rules before anything is handed on.
//!
//! **The one dependency this module isolates.** The facts store is being
//! built beside this compiler (branch `feat/temporal-facts`); until it is on
//! main there is no store API to call. So the hand-off is a port,
//! [`FactSink`], and the only sink wired today is [`NoFacts`], which accepts
//! nothing and says so (`emitted: 0`, the facts are counted as pending in
//! the log line). When the store lands, the composition wires a sink over
//! it — one adapter, no change to the compiler or its model schema.

use serde::{Deserialize, Serialize};

/// The entity types a fact subject may name (spec §1).
const SUBJECT_TYPES: [&str; 7] = [
    "person", "project", "decision", "customer", "system", "glossary", "radar",
];

/// The most facts one compile may emit (the spec's per-request cap).
pub const MAX_FACTS: usize = 50;

/// One fact, in the v1 emitter shape. `status`, `supersedes` and
/// `superseded_by` are the server's and are not fields here at all — the
/// spec forbids an emitter from resolving conflicts itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fact {
    pub subject: String,
    pub attribute: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cardinality: Option<String>,
}

/// Whether `fact` passes the v1 schema's rules (spec §4).
#[must_use]
pub fn valid(fact: &Fact) -> bool {
    subject_ok(&fact.subject)
        && !fact.attribute.is_empty()
        && fact.attribute.len() <= 64
        && fact
            .attribute
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && !fact.value.trim().is_empty()
        && fact.value.chars().count() <= 2000
        && fact.unit.as_deref().is_none_or(|unit| unit.len() <= 32)
        && fact.valid_from.as_deref().is_none_or(partial_date)
        && fact.valid_to.as_deref().is_none_or(partial_date)
        && fact.source.as_deref().is_none_or(|s| s.len() <= 512)
        && fact
            .quote
            .as_deref()
            .is_none_or(|q| q.chars().count() <= 2000)
        && fact
            .confidence
            .is_none_or(|c| c.is_finite() && (0.0..=1.0).contains(&c))
        && fact
            .cardinality
            .as_deref()
            .is_none_or(|c| c == "one" || c == "many")
}

/// `me`, or `<entity type>/<slug>`.
fn subject_ok(subject: &str) -> bool {
    if subject == "me" {
        return true;
    }
    let Some((kind, slug)) = subject.split_once('/') else {
        return false;
    };
    SUBJECT_TYPES.contains(&kind) && livingbrain_pages::is_slug(slug)
}

/// `YYYY`, `YYYY-MM`, `YYYY-MM-DD` or a full RFC 3339 instant.
fn partial_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    let digits = |range: std::ops::Range<usize>| {
        bytes
            .get(range)
            .is_some_and(|part| part.iter().all(u8::is_ascii_digit))
    };
    match bytes.len() {
        4 => digits(0..4),
        7 => digits(0..4) && bytes[4] == b'-' && digits(5..7),
        10 => digits(0..4) && bytes[4] == b'-' && digits(5..7) && bytes[7] == b'-' && digits(8..10),
        _ => time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
            .is_ok(),
    }
}

/// Where the compiler hands the facts a session produced.
#[async_trait::async_trait]
pub trait FactSink: Send + Sync {
    /// Stores `facts` in `scope` (the session's own), answering how many
    /// were accepted.
    ///
    /// # Errors
    ///
    /// A short reason; the compile still succeeds — the pages are the
    /// primary output and carry the same history in prose.
    async fn emit(&self, workspace: &str, scope: &str, facts: &[Fact]) -> Result<usize, String>;
}

/// The sink while the facts layer is not on main: accepts nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoFacts;

#[async_trait::async_trait]
impl FactSink for NoFacts {
    async fn emit(&self, _workspace: &str, _scope: &str, _facts: &[Fact]) -> Result<usize, String> {
        Ok(0)
    }
}

/// The subject for an agent: `system/<agent>`. The spec's subject types
/// have no `agent`, and a coding agent is a system the person uses.
#[must_use]
pub fn agent_subject(agent: &str) -> String {
    format!("system/{agent}")
}

/// The subject for a project: `project/<project>`.
#[must_use]
pub fn project_subject(project: &str) -> String {
    format!("project/{project}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact() -> Fact {
        Fact {
            subject: "project/demo".to_owned(),
            attribute: "database".to_owned(),
            value: "D1".to_owned(),
            unit: None,
            valid_from: Some("2026-10".to_owned()),
            valid_to: None,
            observed_at: Some("2026-10-15T01:00:00Z".to_owned()),
            source: Some("source:01J".to_owned()),
            quote: None,
            confidence: Some(0.8),
            cardinality: None,
        }
    }

    #[test]
    fn a_v1_fact_validates() {
        assert!(valid(&fact()));
        assert!(valid(&Fact {
            subject: "me".to_owned(),
            ..fact()
        }));
    }

    #[test]
    fn the_schema_rules_refuse() {
        for bad in [
            Fact {
                subject: "agent/claude".to_owned(),
                ..fact()
            },
            Fact {
                attribute: "Uses Tool".to_owned(),
                ..fact()
            },
            Fact {
                value: " ".to_owned(),
                ..fact()
            },
            Fact {
                valid_from: Some("last week".to_owned()),
                ..fact()
            },
            Fact {
                confidence: Some(1.5),
                ..fact()
            },
            Fact {
                cardinality: Some("several".to_owned()),
                ..fact()
            },
        ] {
            assert!(!valid(&bad), "{bad:?}");
        }
    }

    #[test]
    fn a_status_field_is_not_the_emitter_s() {
        let raw = r#"{"subject":"project/demo","attribute":"db","value":"D1","status":"current"}"#;
        assert!(serde_json::from_str::<Fact>(raw).is_err());
    }
}
