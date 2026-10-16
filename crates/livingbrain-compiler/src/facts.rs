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
//! **The one dependency this module isolates.** The hand-off is a port,
//! [`FactSink`]: the composition (`livingbrain-api`'s `coding-agents`
//! module) wires a sink over the facts store, which records each fact in
//! the session's own scope. A fact the store refuses (a cardinality
//! conflict, a field redaction will not let through) is a per-fact skip the
//! log line names, never a failed job: the pages are the primary output and
//! carry the same history in prose. [`NoFacts`] is the sink for a
//! composition with no store; it records nothing.

use serde::{Deserialize, Serialize};

/// The entity types a fact subject may name (spec §1).
const SUBJECT_TYPES: [&str; 7] = [
    "person", "project", "decision", "customer", "system", "glossary", "radar",
];

/// The most facts one compile may emit (the spec's per-request cap).
pub const MAX_FACTS: usize = 50;

/// Who the store records as the writer of a compiled fact (`written_by`).
/// Only facts with this writer are the compiler's own: the dreaming rule
/// pass retracts nothing else, so a fact a person or the API wrote is never
/// removed by a rule.
pub const COMPILER_WRITER: &str = "brain:coding-agents";

/// The most current facts of a subject the model is shown, or dreaming
/// reads. The newest are kept: a prompt of every fact an old subject ever
/// had would crowd out the session it is about.
pub const MAX_HELD: usize = 60;

/// Words that make an attribute an event, a one-off investigation or a
/// report, not a state (`session_cost`, `model_router_outage`,
/// `anon_client_rls_bug`, `session_adoption_blocker`). What is true now about
/// a subject is state; what happened, what broke, what was found and what is
/// being looked into are dated prose in the pages, never facts a later one
/// must end. Each word is a whole `_`-separated token, so `outage` matches
/// `model_outage` and `breaker` matches `build_breaker_world360_json`, but
/// nothing durable is caught by a substring.
const EVENT_WORDS: [&str; 38] = [
    "cost",
    "costs",
    "outage",
    "outages",
    "incident",
    "incidents",
    "error",
    "errors",
    "failure",
    "failures",
    "recovery",
    "recoveries",
    "limitation",
    "limitations",
    "crash",
    "crashes",
    "timeout",
    "timeouts",
    "bug",
    "bugs",
    "blocker",
    "blockers",
    "gap",
    "gaps",
    "probe",
    "probes",
    "finding",
    "findings",
    "audit",
    "suspected",
    "breaker",
    "breakers",
    "fix",
    "fixes",
    "resolved",
    "flaky",
    "investigation",
    "investigations",
];

/// Phrases (not single words) that make an attribute an event. `rate_limit`
/// is a rate limit that hit; `root_cause` is an investigation's conclusion.
const EVENT_PHRASES: [&str; 2] = ["rate_limit", "root_cause"];

/// Why an attribute key is not state, or `None` when it may be stored. The
/// one rule list the write-time refusal and the dreaming rule pass share:
/// a fact is refused, or retracted by the rule pass, on exactly these terms.
///
/// `"per-item"`: a number token (`issue_335_status`, `pr_12_state`,
/// `404_root_cause`) makes the key one issue or one incident. A timeline per
/// number would never be ended, and folding them into one shared key would
/// supersede every other issue's status, so the key is refused.
///
/// `"event"`: an event, report or investigation word (see [`EVENT_WORDS`] and
/// [`EVENT_PHRASES`]).
#[must_use]
pub fn attribute_rule(attribute: &str) -> Option<&'static str> {
    let per_item = attribute
        .split('_')
        .any(|token| !token.is_empty() && token.bytes().all(|b| b.is_ascii_digit()));
    if per_item {
        return Some("per-item");
    }
    let event = attribute
        .split('_')
        .any(|token| EVENT_WORDS.contains(&token))
        || EVENT_PHRASES
            .iter()
            .any(|phrase| attribute.contains(phrase));
    event.then_some("event")
}

/// Why a model-written fact is not state, or `None` when it may be stored:
/// [`attribute_rule`] on the fact's key, with the reason the skip log names.
#[must_use]
pub fn refusal(fact: &Fact) -> Option<&'static str> {
    attribute_rule(&fact.attribute).map(|rule| match rule {
        "per-item" => "per-item attribute: a fact is state, not one issue or incident",
        _ => "event, not state: a cost, outage, incident, error or report is not a fact",
    })
}

/// A current fact of one subject, as the compiler reads it back: enough to
/// show the model what is already known, to end or retract it by id, and to
/// copy it under a renamed key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldFact {
    pub id: String,
    pub subject: String,
    pub attribute: String,
    pub value: String,
    pub unit: Option<String>,
    /// As written: `2026`, `2026-10`, `2026-10-15` or an RFC 3339 instant.
    pub valid_from: String,
    /// The effective end as written, `None` while it still holds.
    pub valid_to: Option<String>,
    pub source: Option<String>,
    /// RFC 3339 of the observation.
    pub observed_at: String,
    /// RFC 3339 of the last time a write said it again.
    pub last_confirmed_at: String,
}

/// The newest `MAX_HELD` of `held`, newest `valid_from` first, by the date
/// each starts at (not by its text). A `valid_from` that is not a date sorts
/// last.
#[must_use]
pub fn newest_first(mut held: Vec<HeldFact>) -> Vec<HeldFact> {
    held.sort_by(|a, b| {
        livingbrain_pages::date_start(&b.valid_from)
            .cmp(&livingbrain_pages::date_start(&a.valid_from))
            .then_with(|| a.id.cmp(&b.id))
    });
    held.truncate(MAX_HELD);
    held
}

/// Whether the date `a` starts strictly before the date `b` starts, both as
/// written (`2026`, `2026-10`, `2026-10-15` or RFC 3339). `None` when either
/// is not a date: the caller refuses rather than guess. `2026` does not start
/// before `2026-01-01` (the same instant), and an RFC 3339 instant is compared
/// by the instant it names, whatever its offset.
#[must_use]
pub fn starts_before(a: &str, b: &str) -> Option<bool> {
    Some(livingbrain_pages::date_start(a)? < livingbrain_pages::date_start(b)?)
}

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
pub fn partial_date(value: &str) -> bool {
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

/// What a sink did with one batch of facts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Emitted {
    /// How many the store holds now: stored, or a repeat of a live fact.
    pub recorded: usize,
    /// The ids of the facts this batch actually stored (not repeats of a
    /// live fact, whose id is not this batch's to undo). A sink that does not
    /// report ids leaves it empty.
    pub created: Vec<String>,
    /// One short, log-safe reason per fact the store refused. A refusal is
    /// that fact's alone; the rest of the batch is still recorded.
    pub skipped: Vec<String>,
}

/// Where the compiler hands the facts a session produced.
#[async_trait::async_trait]
pub trait FactSink: Send + Sync {
    /// Stores `facts` in `scope` (the session's own), one at a time, and
    /// answers what became of them.
    ///
    /// # Errors
    ///
    /// A short reason when the sink could not try at all (no store). The
    /// compile still succeeds — the pages are the primary output and carry
    /// the same history in prose.
    async fn emit(&self, workspace: &str, scope: &str, facts: &[Fact]) -> Result<Emitted, String>;

    /// The current facts of `subjects` in `scope`, for the model to reuse
    /// keys from and for dreaming to tidy. A sink with no store knows none.
    ///
    /// # Errors
    ///
    /// A short reason when the store could not be read.
    async fn held(&self, _scope: &str, _subjects: &[String]) -> Result<Vec<HeldFact>, String> {
        Ok(Vec::new())
    }

    /// Ends fact `id` in `scope` at `valid_to` (exclusive). `false` for an id
    /// the scope does not hold.
    ///
    /// # Errors
    ///
    /// A short reason when the store refused the end (not after its start,
    /// retracted) or could not be written.
    async fn end(&self, _scope: &str, _id: &str, _valid_to: &str) -> Result<bool, String> {
        Err("no facts store".to_owned())
    }

    /// Marks fact `id` in `scope` retracted. `false` for an id the scope does
    /// not hold.
    ///
    /// # Errors
    ///
    /// A short reason when the store could not be written.
    async fn retract(&self, _scope: &str, _id: &str) -> Result<bool, String> {
        Err("no facts store".to_owned())
    }
}

/// The sink for a composition with no facts store: records nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoFacts;

#[async_trait::async_trait]
impl FactSink for NoFacts {
    async fn emit(
        &self,
        _workspace: &str,
        _scope: &str,
        _facts: &[Fact],
    ) -> Result<Emitted, String> {
        Ok(Emitted::default())
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

    #[test]
    fn durable_state_keys_are_kept() {
        for state in [
            "database",
            "deploy_target",
            "package_manager",
            "node_major",
            "auth_provider_production",
            "db_migration_runner",
            "vault_of_record",
            "cf_worker_minify_enabled",
        ] {
            assert_eq!(attribute_rule(state), None, "{state} is state");
            assert_eq!(
                refusal(&Fact {
                    attribute: state.to_owned(),
                    ..fact()
                }),
                None,
                "{state}"
            );
        }
    }

    #[test]
    fn events_investigations_and_per_item_keys_are_dropped() {
        for dropped in [
            "issue_320_status",
            "issue_364_status",
            "issue_701_status",
            "issue_136_resolved",
            "issue_505_rls_fix",
            "issue_784_scope",
            "404_root_cause",
            "flaky_test_root_cause",
            "anonymous_401_probes",
            "anon_client_rls_bug",
            "suspected_earthcoin_mint_bug",
            "session_adoption_blocker",
            "upstream_gap",
            "build_breaker_world360_json",
            "migration_0008_messaging",
            "chi_audit_findings",
            "session_cost",
            "model_router_outage",
            "sandbox_limitation",
            "last_error",
            "rate_limit_status",
        ] {
            assert!(attribute_rule(dropped).is_some(), "{dropped} is dropped");
            assert!(
                refusal(&Fact {
                    attribute: dropped.to_owned(),
                    ..fact()
                })
                .is_some(),
                "{dropped} is refused at write time"
            );
        }
        assert_eq!(attribute_rule("issue_335_status"), Some("per-item"));
        assert_eq!(attribute_rule("session_cost"), Some("event"));
    }

    #[test]
    fn partial_dates_are_compared_as_the_instant_they_start_at() {
        // A year and the first day of it start at the same instant.
        assert_eq!(starts_before("2026", "2026-01-01"), Some(false));
        assert_eq!(starts_before("2026-01-01", "2026"), Some(false));
        assert_eq!(starts_before("2025", "2026-01"), Some(true));
        assert_eq!(starts_before("2026-01", "2026-01-15"), Some(true));
        assert_eq!(starts_before("2026-01-15", "2026-01"), Some(false));
        // Text order is wrong across offsets: 00:30 at +02:00 is 22:30 the
        // day before, so it is the earlier instant.
        assert_eq!(
            starts_before("2026-01-01T00:30:00+02:00", "2025-12-31T23:00:00Z"),
            Some(true)
        );
        assert_eq!(
            starts_before("2025-12-31T23:00:00Z", "2026-01-01T00:30:00+02:00"),
            Some(false)
        );
        // The same instant in two offsets is not before itself.
        assert_eq!(
            starts_before("2026-01-01T00:00:00+01:00", "2025-12-31T23:00:00Z"),
            Some(false)
        );
        // Anything that is not a date is refused, not compared.
        for bad in [
            "",
            "last week",
            "2026-13",
            "2026-02-30",
            "2026-01-01T25:00:00Z",
        ] {
            assert_eq!(starts_before(bad, "2026"), None, "{bad:?}");
            assert_eq!(starts_before("2026", bad), None, "{bad:?}");
        }
    }

    #[test]
    fn newest_first_orders_by_the_instant_and_puts_a_non_date_last() {
        let held = |id: &str, valid_from: &str| HeldFact {
            id: id.to_owned(),
            subject: "project/demo".to_owned(),
            attribute: "database".to_owned(),
            value: "D1".to_owned(),
            unit: None,
            valid_from: valid_from.to_owned(),
            valid_to: None,
            source: None,
            observed_at: "2026-10-15T00:00:00Z".to_owned(),
            last_confirmed_at: "2026-10-15T00:00:00Z".to_owned(),
        };
        let kept = newest_first(vec![
            held("fact-a", "2025-12-31T23:00:00Z"),
            held("fact-b", "2026-01-01T00:30:00+02:00"),
            held("fact-c", "not a date"),
            held("fact-d", "2026-01-01"),
        ]);
        let order: Vec<&str> = kept.iter().map(|fact| fact.id.as_str()).collect();
        // fact-b is 2025-12-31T22:30Z, an hour before fact-a's 23:00Z, so it
        // sorts after it although its text reads as a later day.
        assert_eq!(order, ["fact-d", "fact-a", "fact-b", "fact-c"]);
    }

    #[test]
    fn a_held_fact_list_keeps_the_newest_first_and_caps_it() {
        let held = |n: usize| HeldFact {
            id: format!("fact-{n:03}"),
            subject: "project/demo".to_owned(),
            attribute: "database".to_owned(),
            value: "D1".to_owned(),
            unit: None,
            valid_from: format!("2026-{:02}", (n % 12) + 1),
            valid_to: None,
            source: None,
            observed_at: "2026-10-15T00:00:00Z".to_owned(),
            last_confirmed_at: "2026-10-15T00:00:00Z".to_owned(),
        };
        let all: Vec<HeldFact> = (0..MAX_HELD + 10).map(held).collect();
        let kept = newest_first(all);
        assert_eq!(kept.len(), MAX_HELD);
        assert!(
            kept.windows(2)
                .all(|pair| pair[0].valid_from >= pair[1].valid_from)
        );
    }
}
