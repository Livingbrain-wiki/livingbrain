//! The fact store: temporal facts (`docs/temporal-facts.md`) in D1, scoped
//! and sealed exactly like pages.
//!
//! A row is one fact. The `subject` and `attribute` are plaintext — like a
//! page slug, they are what a timeline is selected by — and everything a
//! person said (`value`, `unit`, `source`, `quote`) is one JSON payload
//! sealed under the scope's data key, AAD-bound to the scope, the fact id
//! and the key version. Forgetting a scope deletes its rows and destroys the
//! key; a rotation re-seals them ([`PageStore::reencrypt_scope`]).
//!
//! **A write never overwrites.** [`FactStore::record`] inserts the new fact
//! and then *settles* its subject+attribute timeline: every fact is
//! re-resolved by [`livingbrain_memory::timeline::resolve`], which is
//! deterministic over the whole set, so two writers racing on one timeline
//! converge on the same statuses whichever settles last.
//!
//! [`PageStore::reencrypt_scope`]: crate::PageStore::reencrypt_scope

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use base64::Engine as _;
use cratefield_core::{Clock, Database, IdGen, Row, Statement};
use cratefield_kms::{Dek, Kms};
use livingbrain_d1::{BIND_BUDGET, distinct_chunks};
use livingbrain_memory::timeline::{
    self, Cardinality, FactStatus, PartialDate, TimedFact, corroborated, normalize_attribute,
};
use livingbrain_redact::{Policy, redact};
use sea_query::Value as SeaValue;
use serde_json::{Value, json};

use crate::entity::{self, EntityType};
use crate::keys::{self, ScopeKeys};
use crate::store::{PageError, int, text};

/// The AAD domain of a sealed fact payload; the scope, the fact id and the
/// key version follow it, NUL-separated.
const FACT_AAD_DOMAIN: &str = "livingbrain-pages/fact/v1";
/// The longest value, quote or source a fact may carry, in bytes.
pub const MAX_FACT_TEXT: usize = 2000;
/// The longest unit.
pub const MAX_UNIT_LEN: usize = 32;
/// The most rows one read returns.
pub const MAX_FACT_ROWS: usize = 500;
/// The most timelines [`FactStore::index`] lists.
pub const MAX_INDEX_KEYS: usize = 2000;

/// The AAD a fact payload in `scope` with `id`, sealed under `version`, is
/// bound to — so a payload cannot be moved to another fact or scope.
pub(crate) fn fact_aad(scope: &str, id: &str, version: u32) -> String {
    format!("{FACT_AAD_DOMAIN}\0{scope}\0{id}\0{version}")
}

/// One fact as a writer sends it (format v1). Strings are validated and
/// normalised by [`FactStore::record`], so a route can hand the request
/// over as it arrived.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FactInput {
    /// `<entity_type>/<slug>`. `me` is resolved by the caller (see
    /// [`person_subject`]) before it reaches the store.
    pub subject: String,
    pub attribute: String,
    pub value: String,
    pub unit: Option<String>,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    /// RFC 3339; defaults to now, never in the future.
    pub observed_at: Option<String>,
    pub source: Option<String>,
    pub quote: Option<String>,
    pub confidence: Option<f64>,
    pub cardinality: Option<String>,
}

/// What a write did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactOutcome {
    /// A new fact was stored.
    Created,
    /// The fact repeated a live one; nothing was stored.
    Corroborated,
}

impl FactOutcome {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Corroborated => "corroborated",
        }
    }
}

/// A recorded fact, settled, and which scope it is in.
#[derive(Debug, Clone, PartialEq)]
pub struct Recorded {
    pub outcome: FactOutcome,
    pub scope: String,
    pub fact: TimedFact,
}

/// One subject+attribute timeline in one scope, every fact resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct Timeline {
    pub scope: String,
    pub subject: String,
    pub attribute: String,
    pub cardinality: Cardinality,
    /// In `valid_from` order, oldest first.
    pub facts: Vec<TimedFact>,
}

impl Timeline {
    /// Whether any fact in the timeline is still a fact (not retracted). A
    /// timeline that is retracted-only has nothing to say to a reader.
    #[must_use]
    pub fn has_live_facts(&self) -> bool {
        self.facts
            .iter()
            .any(|fact| fact.status != FactStatus::Retracted)
    }
}

/// Everything a fact write or read can be refused for.
#[derive(Debug)]
pub enum FactError {
    /// A field outside the format; the message says which.
    Invalid(String),
    /// The attribute already has facts of the other cardinality in this
    /// scope.
    CardinalityMismatch {
        attribute: String,
        stored: Cardinality,
    },
    /// The page store's own failure (keys, crypto, database).
    Store(PageError),
}

impl std::fmt::Display for FactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "invalid fact: {message}"),
            Self::CardinalityMismatch { attribute, stored } => write!(
                f,
                "`{attribute}` is a `{}` attribute in this scope",
                stored.as_str()
            ),
            Self::Store(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for FactError {}

impl From<PageError> for FactError {
    fn from(error: PageError) -> Self {
        Self::Store(error)
    }
}

/// Whether `subject` is `<entity_type>/<slug>` with a known entity type and
/// a slug under the slug rule.
#[must_use]
pub fn is_subject(subject: &str) -> bool {
    subject
        .split_once('/')
        .is_some_and(|(kind, slug)| EntityType::parse(kind).is_some() && entity::is_slug(slug))
}

/// A member's own person subject: `person/<user id, slugified>` — lowercase,
/// anything outside `[a-z0-9-]` becomes `-`. What `me` resolves to.
#[must_use]
pub fn person_subject(user_id: &str) -> Option<String> {
    let slug: String = user_id
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    entity::is_slug(slug).then(|| format!("person/{slug}"))
}

/// A fact's JSON shape on the wire (format v1), without the scope.
#[must_use]
pub fn fact_json(fact: &TimedFact) -> Value {
    json!({
        "id": fact.id,
        "subject": fact.subject,
        "attribute": fact.attribute,
        "value": fact.value,
        "unit": fact.unit,
        "valid_from": fact.valid_from.text(),
        "valid_from_precision": fact.valid_from.precision().as_str(),
        "valid_to": fact.valid_to.as_ref().map(PartialDate::text),
        "stated_valid_to": fact.stated_valid_to.as_ref().map(PartialDate::text),
        "observed_at": timeline::rfc3339(fact.observed_at),
        "last_confirmed_at": timeline::rfc3339(fact.last_confirmed_at),
        "status": fact.status.as_str(),
        "supersedes": fact.supersedes,
        "superseded_by": fact.superseded_by,
        "source": fact.source,
        "quote": fact.quote,
        "confidence": fact.confidence,
        "cardinality": fact.cardinality.as_str(),
    })
}

/// A fact the format accepted, ready to be placed in its timeline.
struct Prepared {
    subject: String,
    attribute: String,
    value: String,
    unit: Option<String>,
    source: Option<String>,
    quote: Option<String>,
    confidence: f64,
    observed_at: i64,
    valid_from: PartialDate,
    stated_valid_to: Option<PartialDate>,
    asked: Option<Cardinality>,
}

/// One subject that dreaming may tidy: how
/// many current facts it has, how many were stored since its last pass, and
/// when that pass was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamCandidate {
    pub scope: String,
    pub subject: String,
    pub current: usize,
    pub fresh: usize,
    pub dreamt_at: Option<String>,
    pub status: Option<String>,
    /// When the subject's dream row last changed (its last pass or request).
    pub updated_at: Option<String>,
    /// The window cursor: a position, [`SWEEP_END`], or `None` (never swept,
    /// or a pass from before cursors).
    pub cursor: Option<String>,
    /// How many facts the last pass read.
    pub facts_seen: i64,
}

/// The cursor a completed sweep leaves: a finished subject, not a position.
/// A pass that stopped part-way leaves a `created_at|id` position instead,
/// and a `done` row with no cursor is a pass from before cursors existed.
pub const SWEEP_END: &str = "end";

/// What a dream run does to a subject's window cursor when it ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorUpdate<'a> {
    /// Leave it as it was (a run that could not start, or failed: the next
    /// run reviews the same window again).
    Keep,
    /// The sweep reached the end: stored as [`SWEEP_END`], which says the
    /// subject is finished (its recheck interval starts) and which the next
    /// sweep reads as "from the beginning".
    End,
    /// More of the subject is left: the next run starts after this window.
    Set(&'a str),
}

/// One current fact the rule pass reads: the plaintext columns only, since
/// the rules look at the key and the writer and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleFact {
    pub scope: String,
    pub subject: String,
    pub id: String,
    pub attribute: String,
    /// Who wrote the fact (the `written_by` column): the compiler's writer
    /// for a compiled fact, a user id for one a person or the API wrote.
    pub written_by: String,
}

/// One window of a subject's current facts, for a dream pass: the facts in
/// order, whether more follow, and the cursor after the last of them.
#[derive(Debug, Clone, PartialEq)]
pub struct DreamWindow {
    pub facts: Vec<TimedFact>,
    pub more: bool,
    pub cursor: Option<String>,
}

/// How one dream run ended, for [`FactStore::dream_finish`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DreamFinish<'a> {
    /// `done`, `queued` (carries on later), `failed` or `skipped`.
    pub status: &'a str,
    /// Whether the pass read the subject: sets `dreamt_at`.
    pub dreamt: bool,
    /// Where the next run of this subject starts.
    pub cursor: CursorUpdate<'a>,
    pub facts_seen: usize,
    pub decisions: usize,
    pub applied: usize,
    /// A fixed-vocabulary reason, never model text.
    pub error: Option<&'a str>,
}

/// One decision of a dream pass, as the decision log keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DreamDecisionRow<'a> {
    pub scope: &'a str,
    /// RFC 3339 of the pass.
    pub dream_at: &'a str,
    pub seq: usize,
    pub subject: &'a str,
    pub op: &'a str,
    pub fact_id: &'a str,
    pub by_fact_id: Option<&'a str>,
    pub new_valid_to: Option<&'a str>,
    pub new_attribute: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub applied: bool,
    /// JSON array of the fact ids the model was shown.
    pub input_ids: &'a str,
    /// The call's role (the model itself is not recorded per call).
    pub model: &'a str,
    pub decided_at: &'a str,
}

/// One completed dream pass, as a timeline reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamPass {
    pub scope: String,
    pub subject: String,
    pub at: String,
    pub facts_seen: i64,
    pub decisions: i64,
    pub applied: i64,
}

/// One row of the decision log, as a read returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamDecisionRecord {
    pub scope: String,
    pub dream_at: String,
    pub seq: i64,
    pub subject: String,
    pub op: String,
    pub fact_id: String,
    pub by_fact_id: Option<String>,
    pub new_valid_to: Option<String>,
    pub new_attribute: Option<String>,
    pub reason: Option<String>,
    pub applied: bool,
    pub model: String,
    pub decided_at: String,
}

/// What dreaming did in a set of scopes: the passes, and every applied
/// decision as `(scope, subject, fact id, op)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DreamOutcomes {
    pub passes: Vec<DreamPass>,
    pub edits: Vec<(String, String, String, String)>,
}

/// The fact store.
pub struct FactStore {
    db: Arc<dyn Database>,
    keys: ScopeKeys,
    clock: Arc<dyn Clock>,
    id_gen: Arc<dyn IdGen>,
}

/// One cleaned text field: redacted, trimmed, `None` when empty.
fn clean(field: &str, raw: Option<&str>, max: usize) -> Result<Option<String>, FactError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let (cleaned, _) = redact(raw, Policy::Redact)
        .map_err(|_| FactError::Invalid(format!("`{field}` could not be redacted")))?;
    let cleaned = cleaned.trim().to_owned();
    if cleaned.len() > max {
        return Err(FactError::Invalid(format!(
            "`{field}` is longer than {max} bytes"
        )));
    }
    Ok((!cleaned.is_empty()).then_some(cleaned))
}

fn date(field: &str, raw: Option<&str>) -> Result<Option<PartialDate>, FactError> {
    raw.filter(|raw| !raw.trim().is_empty())
        .map(|raw| {
            PartialDate::parse(raw).map_err(|e| FactError::Invalid(format!("`{field}`: {e}")))
        })
        .transpose()
}

/// The first instant of a stored date as written (`2026`, `2026-10`,
/// `2026-10-15` or RFC 3339, any offset), as seconds since the epoch in UTC.
/// `None` for anything that is not a date. Dates are compared by this, never
/// as text: `2026` and `2026-01-01` start at the same instant, and two
/// RFC 3339 instants with different offsets sort by their instant.
#[must_use]
pub fn date_start(value: &str) -> Option<i64> {
    PartialDate::parse(value).ok().map(|date| date.start())
}

fn opt_text(value: Option<&str>) -> SeaValue {
    SeaValue::String(value.map(|v| Box::new(v.to_owned())))
}

impl FactStore {
    /// A store over the database, the key custodian, the clock and the id
    /// generator — the ports a [`PageStore`](crate::PageStore) is built from,
    /// minus the blob store (a fact lives in its row).
    #[must_use]
    pub fn new(
        db: Arc<dyn Database>,
        kms: Arc<dyn Kms>,
        clock: Arc<dyn Clock>,
        id_gen: Arc<dyn IdGen>,
    ) -> Self {
        Self {
            keys: ScopeKeys::new(db.clone(), kms, clock.clone()),
            db,
            clock,
            id_gen,
        }
    }

    /// The store's "now", unix seconds.
    #[must_use]
    pub fn now(&self) -> i64 {
        self.clock.now().unix_timestamp()
    }

    /// Checks a fact against the format without touching the store: what
    /// [`FactStore::record`] would refuse before reading a timeline. A
    /// caller that writes something else first (a note) checks every fact
    /// up front, so a malformed one refuses the whole request rather than
    /// leaving half of it stored.
    ///
    /// # Errors
    ///
    /// [`FactError::Invalid`] for a field outside the format.
    pub fn check(&self, input: &FactInput) -> Result<(), FactError> {
        self.prepare(input).map(|_| ())
    }

    /// The stateless half of a write: every field validated, normalised and
    /// redacted.
    fn prepare(&self, input: &FactInput) -> Result<Prepared, FactError> {
        let subject = input.subject.trim().to_owned();
        if !is_subject(&subject) {
            return Err(FactError::Invalid(
                "`subject` must be `<entity_type>/<slug>`, e.g. `person/nick`".to_owned(),
            ));
        }
        // The subject and attribute are stored in plaintext (like a page
        // slug), so neither may carry anything redaction would have taken
        // out of a body: refused, not silently rewritten into another key.
        for (field, raw) in [("subject", &input.subject), ("attribute", &input.attribute)] {
            match redact(raw, Policy::Redact) {
                Ok((_, findings)) if findings.is_empty() => {}
                _ => {
                    return Err(FactError::Invalid(format!(
                        "`{field}` looks like it holds a secret or personal data; \
                         it is stored unencrypted, so name the thing instead"
                    )));
                }
            }
        }
        let attribute = normalize_attribute(&input.attribute).ok_or_else(|| {
            FactError::Invalid("`attribute` must be 1-64 letters, digits or `_`".to_owned())
        })?;
        let value = clean("value", Some(&input.value), MAX_FACT_TEXT)?
            .ok_or_else(|| FactError::Invalid("`value` is required".to_owned()))?;
        let unit = clean("unit", input.unit.as_deref(), MAX_UNIT_LEN)?;
        let source = clean("source", input.source.as_deref(), 512)?;
        let quote = clean("quote", input.quote.as_deref(), MAX_FACT_TEXT)?;
        let confidence = input.confidence.unwrap_or(1.0);
        if !(0.0..=1.0).contains(&confidence) {
            return Err(FactError::Invalid(
                "`confidence` must be between 0 and 1".to_owned(),
            ));
        }
        let now = self.now();
        let observed_at = match date("observed_at", input.observed_at.as_deref())? {
            Some(observed) if observed.start() > now => {
                return Err(FactError::Invalid(
                    "`observed_at` is in the future".to_owned(),
                ));
            }
            Some(observed) => observed.start(),
            None => now,
        };
        let valid_from = date("valid_from", input.valid_from.as_deref())?
            .unwrap_or_else(|| PartialDate::instant(observed_at));
        let stated_valid_to = date("valid_to", input.valid_to.as_deref())?;
        if let Some(to) = &stated_valid_to
            && to.start() <= valid_from.start()
        {
            return Err(FactError::Invalid(
                "`valid_to` must start after `valid_from`".to_owned(),
            ));
        }
        let asked = match input.cardinality.as_deref() {
            None | Some("") => None,
            Some(raw) => Some(Cardinality::parse(raw).ok_or_else(|| {
                FactError::Invalid("`cardinality` is `one` or `many`".to_owned())
            })?),
        };
        Ok(Prepared {
            subject,
            attribute,
            value,
            unit,
            source,
            quote,
            confidence,
            observed_at,
            valid_from,
            stated_valid_to,
            asked,
        })
    }

    /// Records one fact in `scope`, written by `written_by`, and settles its
    /// timeline. A repeat of a live fact is answered with that fact
    /// ([`FactOutcome::Corroborated`]) and stores nothing.
    ///
    /// Every text field is redacted before it is sealed.
    ///
    /// # Errors
    ///
    /// [`FactError::Invalid`] for a field outside the format,
    /// [`FactError::CardinalityMismatch`], and [`FactError::Store`].
    pub async fn record(
        &self,
        scope: &str,
        input: FactInput,
        written_by: &str,
    ) -> Result<Recorded, FactError> {
        if !entity::is_slug(scope) {
            return Err(PageError::InvalidScope(scope.to_owned()).into());
        }
        let Prepared {
            subject,
            attribute,
            value,
            unit,
            source,
            quote,
            confidence,
            observed_at,
            valid_from,
            stated_valid_to,
            asked,
        } = self.prepare(&input)?;

        let existing = self.load(scope, &subject, &attribute).await?;
        let cardinality = match (existing.first().map(|f| f.cardinality), asked) {
            (Some(stored), Some(asked)) if stored != asked => {
                return Err(FactError::CardinalityMismatch { attribute, stored });
            }
            (Some(stored), _) => stored,
            (None, Some(asked)) => asked,
            (None, None) => Cardinality::default_for(&attribute),
        };
        if let Some(found) = corroborated(&existing, &value, unit.as_deref(), &valid_from) {
            // A repeat is a confirmation, not a second row: the existing fact's
            // freshness moves forward to this write's observation, never back.
            let mut fact = found.clone();
            self.confirm(scope, &fact.id, observed_at).await?;
            fact.last_confirmed_at = fact.last_confirmed_at.max(observed_at);
            return Ok(Recorded {
                outcome: FactOutcome::Corroborated,
                scope: scope.to_owned(),
                fact,
            });
        }

        let id = format!("fact-{}", self.id_gen.ulid());
        let fact = TimedFact {
            id: id.clone(),
            subject,
            attribute,
            value,
            unit,
            valid_from,
            valid_to: stated_valid_to.clone(),
            stated_valid_to,
            observed_at,
            last_confirmed_at: observed_at,
            status: FactStatus::Current,
            supersedes: None,
            superseded_by: None,
            source,
            quote,
            confidence,
            cardinality,
        };
        self.insert(scope, &fact, written_by).await?;
        let settled = self.settle(scope, &fact.subject, &fact.attribute).await?;
        let fact = settled.into_iter().find(|f| f.id == id).unwrap_or(fact);
        Ok(Recorded {
            outcome: FactOutcome::Created,
            scope: scope.to_owned(),
            fact,
        })
    }

    /// Marks the fact `id` retracted, if it is in one of `scopes`, and
    /// settles its timeline. `None` for an id no scope here holds — the same
    /// answer for one that does not exist and one the caller may not read.
    ///
    /// # Errors
    ///
    /// [`FactError::Store`].
    pub async fn retract(
        &self,
        scopes: &[&str],
        id: &str,
    ) -> Result<Option<(String, TimedFact)>, FactError> {
        if scopes.is_empty() {
            return Ok(None);
        }
        let mut found = None;
        for group in scope_groups(scopes, 1) {
            let mut values = vec![text(id)];
            let placeholders = in_list(&group, &mut values);
            let rows = self
                .db
                .query(&Statement::with_values(
                    format!(
                        "SELECT scope, subject, attribute FROM facts \
                         WHERE id = ? AND scope IN ({placeholders})"
                    ),
                    values,
                ))
                .await
                .map_err(PageError::Store)?;
            if let Some(row) = rows.first() {
                found = Some((
                    row.get::<String>("scope").unwrap_or_default(),
                    row.get::<String>("subject").unwrap_or_default(),
                    row.get::<String>("attribute").unwrap_or_default(),
                ));
                break;
            }
        }
        let Some((scope, subject, attribute)) = found else {
            return Ok(None);
        };
        self.db
            .execute(&Statement::with_values(
                "UPDATE facts SET status = ?, updated_at = ? WHERE scope = ? AND id = ?",
                vec![
                    text(FactStatus::Retracted.as_str()),
                    text(&timeline::rfc3339(self.now())),
                    text(&scope),
                    text(id),
                ],
            ))
            .await
            .map_err(PageError::Store)?;
        let settled = self.settle(&scope, &subject, &attribute).await?;
        Ok(settled
            .into_iter()
            .find(|fact| fact.id == id)
            .map(|fact| (scope, fact)))
    }

    /// The scope and subject of fact `id`, if one of `scopes` holds it —
    /// what a grant on a subject prefix is checked against before a retract
    /// (ADR 0004). `None` for an id no scope here holds.
    ///
    /// # Errors
    ///
    /// [`FactError::Store`].
    pub async fn subject_of(
        &self,
        scopes: &[&str],
        id: &str,
    ) -> Result<Option<(String, String)>, FactError> {
        if scopes.is_empty() {
            return Ok(None);
        }
        for group in scope_groups(scopes, 1) {
            let mut values = vec![text(id)];
            let placeholders = in_list(&group, &mut values);
            let rows = self
                .db
                .query(&Statement::with_values(
                    format!(
                        "SELECT scope, subject FROM facts WHERE id = ? AND scope IN ({placeholders})"
                    ),
                    values,
                ))
                .await
                .map_err(PageError::Store)?;
            if let Some(row) = rows.first() {
                return Ok(Some((
                    row.get::<String>("scope").unwrap_or_default(),
                    row.get::<String>("subject").unwrap_or_default(),
                )));
            }
        }
        Ok(None)
    }

    /// The timelines in `scopes`, optionally narrowed to one subject and/or
    /// one attribute, ordered by subject then attribute then scope, each
    /// timeline oldest first. At most [`MAX_FACT_ROWS`] facts.
    ///
    /// The asker supplies the scopes; this never widens them and never runs
    /// a query without a scope predicate.
    ///
    /// # Errors
    ///
    /// [`FactError::Store`].
    pub async fn timelines(
        &self,
        scopes: &[&str],
        subject: Option<&str>,
        attribute: Option<&str>,
    ) -> Result<Vec<Timeline>, FactError> {
        if scopes.is_empty() {
            return Ok(Vec::new());
        }
        for scope in scopes {
            if !entity::is_slug(scope) {
                return Err(PageError::InvalidScope((*scope).to_owned()).into());
            }
        }
        let fixed = 1 + usize::from(subject.is_some()) + usize::from(attribute.is_some());
        let mut fetched: Vec<Row> = Vec::new();
        for group in scope_groups(scopes, fixed) {
            let mut values = Vec::new();
            let placeholders = in_list(&group, &mut values);
            let mut sql = format!("SELECT * FROM facts WHERE scope IN ({placeholders})");
            if let Some(subject) = subject {
                sql.push_str(" AND subject = ?");
                values.push(text(subject));
            }
            if let Some(attribute) = attribute {
                sql.push_str(" AND attribute = ?");
                values.push(text(attribute));
            }
            sql.push_str(
                " ORDER BY subject ASC, attribute ASC, scope ASC, valid_from_at ASC, id ASC LIMIT ?",
            );
            values.push(SeaValue::BigInt(Some(MAX_FACT_ROWS as i64)));
            let rows = self
                .db
                .query(&Statement::with_values(sql, values))
                .await
                .map_err(PageError::Store)?;
            fetched.extend(rows.rows);
        }
        // The order one statement would have used, cut to the cap.
        fetched.sort_by(|a, b| {
            let key = |row: &Row| {
                (
                    row.get::<String>("subject"),
                    row.get::<String>("attribute"),
                    row.get::<String>("scope"),
                    row.get::<String>("valid_from_at"),
                    row.get::<String>("id"),
                )
            };
            key(a).cmp(&key(b))
        });
        fetched.truncate(MAX_FACT_ROWS);
        let facts = self.decode(&fetched).await?;
        let mut grouped: BTreeMap<(String, String, String), Vec<TimedFact>> = BTreeMap::new();
        for (scope, fact) in facts {
            grouped
                .entry((fact.subject.clone(), fact.attribute.clone(), scope))
                .or_default()
                .push(fact);
        }
        Ok(grouped
            .into_iter()
            .map(|((subject, attribute, scope), facts)| Timeline {
                cardinality: facts.first().map_or(Cardinality::One, |f| f.cardinality),
                scope,
                subject,
                attribute,
                facts,
            })
            .collect())
    }

    /// Which timelines exist in `scopes`: `(scope, subject, attribute)`,
    /// sorted, without opening a single payload — the plaintext columns a
    /// reader matches a question against before it decrypts anything. At
    /// most [`MAX_INDEX_KEYS`]. A timeline whose every fact is retracted is
    /// not listed: nothing in it can be read as a fact, so a question never
    /// spends a slot on it.
    ///
    /// # Errors
    ///
    /// [`FactError::Store`].
    pub async fn index(&self, scopes: &[&str]) -> Result<Vec<(String, String, String)>, FactError> {
        if scopes.is_empty() {
            return Ok(Vec::new());
        }
        let mut keys: BTreeSet<(String, String, String)> = BTreeSet::new();
        for group in scope_groups(scopes, 1) {
            let mut values = Vec::new();
            let placeholders = in_list(&group, &mut values);
            values.push(SeaValue::BigInt(Some(MAX_INDEX_KEYS as i64)));
            let rows = self
                .db
                .query(&Statement::with_values(
                    format!(
                        "SELECT DISTINCT scope, subject, attribute FROM facts \
                         WHERE scope IN ({placeholders}) AND status <> 'retracted' \
                         ORDER BY subject ASC, attribute ASC, scope ASC LIMIT ?"
                    ),
                    values,
                ))
                .await
                .map_err(PageError::Store)?;
            for row in &rows.rows {
                keys.insert((
                    row.get::<String>("subject").unwrap_or_default(),
                    row.get::<String>("attribute").unwrap_or_default(),
                    row.get::<String>("scope").unwrap_or_default(),
                ));
            }
        }
        // Sorted by subject, attribute, scope, as the statement would order
        // them, and cut to the cap.
        Ok(keys
            .into_iter()
            .take(MAX_INDEX_KEYS)
            .map(|(subject, attribute, scope)| (scope, subject, attribute))
            .collect())
    }

    /// One timeline in one scope, every fact resolved, oldest first — for a
    /// caller that already chose it from [`FactStore::index`].
    ///
    /// # Errors
    ///
    /// [`FactError::Store`].
    pub async fn timeline(
        &self,
        scope: &str,
        subject: &str,
        attribute: &str,
    ) -> Result<Timeline, FactError> {
        let facts = self.load(scope, subject, attribute).await?;
        Ok(Timeline {
            scope: scope.to_owned(),
            subject: subject.to_owned(),
            attribute: attribute.to_owned(),
            cardinality: facts.first().map_or(Cardinality::One, |f| f.cardinality),
            facts,
        })
    }

    /// One timeline in one scope, decrypted, oldest first.
    async fn load(
        &self,
        scope: &str,
        subject: &str,
        attribute: &str,
    ) -> Result<Vec<TimedFact>, FactError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT * FROM facts WHERE scope = ? AND subject = ? AND attribute = ? \
                 ORDER BY valid_from_at ASC, id ASC",
                vec![text(scope), text(subject), text(attribute)],
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(self
            .decode(&rows.rows)
            .await?
            .into_iter()
            .map(|(_, fact)| fact)
            .collect())
    }

    /// Re-resolves one timeline and writes back every fact whose resolution
    /// changed. Returns the settled timeline.
    async fn settle(
        &self,
        scope: &str,
        subject: &str,
        attribute: &str,
    ) -> Result<Vec<TimedFact>, FactError> {
        let before = self.load(scope, subject, attribute).await?;
        let mut after = before.clone();
        timeline::resolve(&mut after);
        let now = timeline::rfc3339(self.now());
        let statements: Vec<Statement> = before
            .iter()
            .zip(&after)
            .filter(|(old, new)| old != new)
            .map(|(_, fact)| {
                Statement::with_values(
                    "UPDATE facts SET status = ?, valid_to = ?, valid_to_at = ?, supersedes = ?, \
                     superseded_by = ?, updated_at = ? WHERE scope = ? AND id = ?",
                    vec![
                        text(fact.status.as_str()),
                        opt_text(fact.valid_to.as_ref().map(PartialDate::text)),
                        opt_text(
                            fact.valid_to
                                .as_ref()
                                .map(|to| timeline::rfc3339(to.start()))
                                .as_deref(),
                        ),
                        opt_text(fact.supersedes.as_deref()),
                        opt_text(fact.superseded_by.as_deref()),
                        text(&now),
                        text(scope),
                        text(&fact.id),
                    ],
                )
            })
            .collect();
        if !statements.is_empty() {
            self.db
                .batch_atomic(&statements)
                .await
                .map_err(PageError::Store)?;
        }
        Ok(after)
    }

    /// Moves a fact's `last_confirmed_at` forward to `at`, never back.
    async fn confirm(&self, scope: &str, id: &str, at: i64) -> Result<(), FactError> {
        let at = timeline::rfc3339(at);
        self.db
            .execute(&Statement::with_values(
                "UPDATE facts SET last_confirmed_at = ?, updated_at = ? \
                 WHERE scope = ? AND id = ? AND (last_confirmed_at IS NULL OR last_confirmed_at < ?)",
                vec![
                    text(&at),
                    text(&timeline::rfc3339(self.now())),
                    text(scope),
                    text(id),
                    text(&at),
                ],
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(())
    }

    /// Ends the fact `id` in `scope` at `valid_to` (exclusive), keeping it:
    /// the stated end is written and the timeline settled, so the fact reads
    /// as superseded by a successor that begins there, or as ended. `None`
    /// for an id the scope does not hold.
    ///
    /// Dreaming uses this for "it stopped being true on …"; the same checks
    /// as a write apply, so an end that is not after the fact's start is
    /// refused, and a retracted fact has nothing to end.
    ///
    /// # Errors
    ///
    /// [`FactError::Invalid`] for a date outside the format or an end that
    /// does not follow the start, and [`FactError::Store`].
    pub async fn end(
        &self,
        scope: &str,
        id: &str,
        valid_to: &str,
    ) -> Result<Option<TimedFact>, FactError> {
        if !entity::is_slug(scope) {
            return Err(PageError::InvalidScope(scope.to_owned()).into());
        }
        let to = date("valid_to", Some(valid_to))?
            .ok_or_else(|| FactError::Invalid("`valid_to` is required".to_owned()))?;
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT * FROM facts WHERE scope = ? AND id = ?",
                vec![text(scope), text(id)],
            ))
            .await
            .map_err(PageError::Store)?;
        let Some((_, fact)) = self.decode(&rows.rows).await?.into_iter().next() else {
            return Ok(None);
        };
        if fact.status == FactStatus::Retracted {
            return Err(FactError::Invalid(
                "a retracted fact has nothing to end".to_owned(),
            ));
        }
        if to.start() <= fact.valid_from.start() {
            return Err(FactError::Invalid(
                "`valid_to` must start after `valid_from`".to_owned(),
            ));
        }
        self.db
            .execute(&Statement::with_values(
                "UPDATE facts SET stated_valid_to = ?, updated_at = ? WHERE scope = ? AND id = ?",
                vec![
                    text(to.text()),
                    text(&timeline::rfc3339(self.now())),
                    text(scope),
                    text(id),
                ],
            ))
            .await
            .map_err(PageError::Store)?;
        let settled = self.settle(scope, &fact.subject, &fact.attribute).await?;
        Ok(settled.into_iter().find(|f| f.id == id))
    }

    /// Subject+attribute keys that have at least `min_facts` current facts,
    /// with how many of those were stored since the subject's last dream and
    /// when it was last dreamt about. A plaintext read: nothing is opened.
    ///
    /// # Errors
    ///
    /// [`FactError::Store`].
    pub async fn dream_candidates(
        &self,
        min_facts: usize,
        limit: usize,
    ) -> Result<Vec<DreamCandidate>, FactError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT f.scope AS scope, f.subject AS subject, COUNT(*) AS current_count, \
                 SUM(CASE WHEN f.created_at > COALESCE(d.dreamt_at, '') THEN 1 ELSE 0 END) AS fresh, \
                 d.dreamt_at AS dreamt_at, d.status AS status, d.updated_at AS updated_at, \
                 d.cursor AS cursor, d.facts_seen AS facts_seen \
                 FROM facts f LEFT JOIN fact_dreams d ON d.scope = f.scope AND d.subject = f.subject \
                 WHERE f.status = 'current' \
                 GROUP BY f.scope, f.subject, d.dreamt_at, d.status, d.updated_at, d.cursor, d.facts_seen \
                 HAVING COUNT(*) >= ? ORDER BY f.scope ASC, f.subject ASC LIMIT ?",
                vec![
                    SeaValue::BigInt(Some(min_facts as i64)),
                    SeaValue::BigInt(Some(limit as i64)),
                ],
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(rows
            .rows
            .iter()
            .map(|row| DreamCandidate {
                scope: row.get::<String>("scope").unwrap_or_default(),
                subject: row.get::<String>("subject").unwrap_or_default(),
                current: row.get::<i64>("current_count").unwrap_or_default().max(0) as usize,
                fresh: row.get::<i64>("fresh").unwrap_or_default().max(0) as usize,
                dreamt_at: row.get::<String>("dreamt_at"),
                status: row.get::<String>("status"),
                updated_at: row.get::<String>("updated_at"),
                cursor: row.get::<String>("cursor"),
                facts_seen: row.get::<i64>("facts_seen").unwrap_or_default(),
            })
            .collect())
    }

    /// Asks for a dream of one subject: queued, unless one is running now.
    /// A queued request that is asked again keeps its place.
    ///
    /// # Errors
    ///
    /// [`FactError::Store`].
    pub async fn dream_enqueue(
        &self,
        scope: &str,
        subject: &str,
        now: i64,
    ) -> Result<(), FactError> {
        let at = timeline::rfc3339(now);
        self.db
            .execute(&Statement::with_values(
                "INSERT INTO fact_dreams (scope, subject, status, queued_at, attempts, facts_seen, \
                 decisions, applied, updated_at) VALUES (?, ?, 'queued', ?, 0, 0, 0, 0, ?) \
                 ON CONFLICT (scope, subject) DO UPDATE SET status = 'queued', queued_at = excluded.queued_at, \
                 updated_at = excluded.updated_at WHERE fact_dreams.status <> 'running'",
                vec![text(scope), text(subject), text(&at), text(&at)],
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(())
    }

    /// The oldest queued dreams, at most `limit`: `(scope, subject)`.
    ///
    /// # Errors
    ///
    /// [`FactError::Store`].
    pub async fn dream_queued(&self, limit: usize) -> Result<Vec<(String, String)>, FactError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT scope, subject FROM fact_dreams WHERE status = 'queued' \
                 ORDER BY queued_at ASC, scope ASC, subject ASC LIMIT ?",
                vec![SeaValue::BigInt(Some(limit as i64))],
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(rows
            .rows
            .iter()
            .map(|row| {
                (
                    row.get::<String>("scope").unwrap_or_default(),
                    row.get::<String>("subject").unwrap_or_default(),
                )
            })
            .collect())
    }

    /// Claims a queued dream for this run: `true` only for the caller whose
    /// conditional update found it still queued (a second tick finds it
    /// running and leaves it alone).
    ///
    /// # Errors
    ///
    /// [`FactError::Store`].
    pub async fn dream_claim(
        &self,
        scope: &str,
        subject: &str,
        now: i64,
        lease_until: i64,
    ) -> Result<bool, FactError> {
        let affected = self
            .db
            .execute(&Statement::with_values(
                "UPDATE fact_dreams SET status = 'running', attempts = attempts + 1, \
                 lease_until = ?, updated_at = ? WHERE scope = ? AND subject = ? AND status = 'queued'",
                vec![
                    text(&timeline::rfc3339(lease_until)),
                    text(&timeline::rfc3339(now)),
                    text(scope),
                    text(subject),
                ],
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(affected > 0)
    }

    /// Records how a dream ended. `dreamt` is set when a pass actually read
    /// the subject (`done`); a queued-again or failed one keeps its last
    /// completed pass time.
    ///
    /// # Errors
    ///
    /// [`FactError::Store`].
    pub async fn dream_finish(
        &self,
        scope: &str,
        subject: &str,
        outcome: &DreamFinish<'_>,
        now: i64,
    ) -> Result<(), FactError> {
        let at = timeline::rfc3339(now);
        // The cursor is set, cleared or left alone; only the first two write it.
        let (cursor_sql, cursor_value) = match outcome.cursor {
            CursorUpdate::Keep => ("", None),
            CursorUpdate::End => (", cursor = ?", Some(SWEEP_END)),
            CursorUpdate::Set(cursor) => (", cursor = ?", Some(cursor)),
        };
        let mut values = vec![
            text(outcome.status),
            opt_text(outcome.dreamt.then_some(at.as_str())),
            SeaValue::BigInt(Some(outcome.facts_seen as i64)),
            SeaValue::BigInt(Some(outcome.decisions as i64)),
            SeaValue::BigInt(Some(outcome.applied as i64)),
            opt_text(outcome.error),
            text(&at),
        ];
        if !cursor_sql.is_empty() {
            values.push(opt_text(cursor_value));
        }
        values.push(text(scope));
        values.push(text(subject));
        self.db
            .execute(&Statement::with_values(
                format!(
                    "UPDATE fact_dreams SET status = ?, dreamt_at = COALESCE(?, dreamt_at), \
                     facts_seen = ?, decisions = ?, applied = ?, last_error = ?, lease_until = NULL, \
                     updated_at = ?{cursor_sql} WHERE scope = ? AND subject = ?"
                ),
                values,
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(())
    }

    /// Where a subject's next dream window starts (`created_at|id` of the
    /// last fact reviewed), `None` from the beginning.
    ///
    /// # Errors
    ///
    /// [`FactError::Store`].
    pub async fn dream_cursor(
        &self,
        scope: &str,
        subject: &str,
    ) -> Result<Option<String>, FactError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT cursor FROM fact_dreams WHERE scope = ? AND subject = ?",
                vec![text(scope), text(subject)],
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(rows
            .rows
            .first()
            .and_then(|row| row.get::<String>("cursor")))
    }

    /// The next window of a subject's **current** facts after `cursor`, in
    /// the order a dream reads them (`created_at`, then `id`), at most
    /// `limit`. `more` says whether any current fact follows the window; the
    /// returned cursor is the last fact's position. Superseded, ended and
    /// retracted facts are never in a window.
    ///
    /// # Errors
    ///
    /// [`FactError::Store`].
    pub async fn dream_window(
        &self,
        scope: &str,
        subject: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<DreamWindow, FactError> {
        let mut values = vec![text(scope), text(subject)];
        let mut sql = String::from(
            "SELECT * FROM facts WHERE scope = ? AND subject = ? AND status = 'current'",
        );
        // A finished sweep's marker is not a position: the next sweep starts
        // from the beginning.
        let cursor = cursor.filter(|c| *c != SWEEP_END);
        if let Some((created, id)) = cursor.and_then(|c| c.split_once('|')) {
            sql.push_str(" AND (created_at > ? OR (created_at = ? AND id > ?))");
            values.push(text(created));
            values.push(text(created));
            values.push(text(id));
        }
        sql.push_str(" ORDER BY created_at ASC, id ASC LIMIT ?");
        values.push(SeaValue::BigInt(Some(limit as i64 + 1)));
        let rows = self
            .db
            .query(&Statement::with_values(sql, values))
            .await
            .map_err(PageError::Store)?;
        let more = rows.rows.len() > limit;
        let kept = &rows.rows[..rows.rows.len().min(limit)];
        let cursor = kept.last().map(|row| {
            format!(
                "{}|{}",
                row.get::<String>("created_at").unwrap_or_default(),
                row.get::<String>("id").unwrap_or_default()
            )
        });
        let facts = self
            .decode(kept)
            .await?
            .into_iter()
            .map(|(_, fact)| fact)
            .collect();
        Ok(DreamWindow {
            facts,
            more,
            cursor,
        })
    }

    /// Each subject of `scope` with its current attribute families and how
    /// many current facts it has: plaintext columns only. Dreaming compares
    /// subjects by these to propose an alias.
    ///
    /// # Errors
    ///
    /// [`FactError::Store`].
    pub async fn dream_subject_families(
        &self,
        scope: &str,
    ) -> Result<Vec<(String, Vec<String>, i64)>, FactError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT subject, attribute, COUNT(*) AS n FROM facts \
                 WHERE scope = ? AND status = 'current' \
                 GROUP BY subject, attribute ORDER BY subject ASC, attribute ASC LIMIT 5000",
                vec![text(scope)],
            ))
            .await
            .map_err(PageError::Store)?;
        let mut by_subject: BTreeMap<String, (BTreeSet<String>, i64)> = BTreeMap::new();
        for row in &rows.rows {
            let subject = row.get::<String>("subject").unwrap_or_default();
            let attribute = row.get::<String>("attribute").unwrap_or_default();
            let n = row.get::<i64>("n").unwrap_or_default();
            let entry = by_subject.entry(subject).or_default();
            if let Some(family) = family_of(&attribute) {
                entry.0.insert(family.to_owned());
            }
            entry.1 += n;
        }
        Ok(by_subject
            .into_iter()
            .map(|(subject, (families, n))| (subject, families.into_iter().collect(), n))
            .collect())
    }

    /// The current facts of `subject` in the attribute families `families`
    /// (see [`family_of`]) that are not in `skip`, oldest first, at most
    /// `limit`. Dreaming reads a window's families whole, so a property
    /// spread over several keys (`auth_provider_*`) is decided in one
    /// place rather than split across windows.
    ///
    /// # Errors
    ///
    /// [`FactError::Store`].
    pub async fn dream_family(
        &self,
        scope: &str,
        subject: &str,
        families: &[String],
        skip: &[String],
        limit: usize,
    ) -> Result<Vec<TimedFact>, FactError> {
        if families.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        // The plaintext columns pick the rows; only the kept ones are opened.
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT * FROM facts WHERE scope = ? AND subject = ? AND status = 'current' \
                 ORDER BY created_at ASC, id ASC LIMIT ?",
                vec![
                    text(scope),
                    text(subject),
                    SeaValue::BigInt(Some(MAX_FACT_ROWS as i64)),
                ],
            ))
            .await
            .map_err(PageError::Store)?;
        let kept: Vec<Row> = rows
            .rows
            .into_iter()
            .filter(|row| {
                let attribute = row.get::<String>("attribute").unwrap_or_default();
                let id = row.get::<String>("id").unwrap_or_default();
                !skip.contains(&id)
                    && family_of(&attribute)
                        .is_some_and(|family| families.iter().any(|f| f == family))
            })
            .take(limit)
            .collect();
        Ok(self
            .decode(&kept)
            .await?
            .into_iter()
            .map(|(_, fact)| fact)
            .collect())
    }

    /// The next batch of current facts for the rule pass, oldest first, after
    /// the stored position, and advances that position. The batch that reaches
    /// the end clears it, so the next tick starts over. Bounded by `limit`.
    ///
    /// # Errors
    ///
    /// [`FactError::Store`].
    pub async fn rule_batch(&self, limit: usize, now: i64) -> Result<Vec<RuleFact>, FactError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT cursor FROM fact_rule_scan WHERE name = 'facts'",
                Vec::new(),
            ))
            .await
            .map_err(PageError::Store)?;
        let stored = rows
            .rows
            .first()
            .and_then(|row| row.get::<String>("cursor"));
        let mut values = Vec::new();
        let mut sql = String::from(
            "SELECT scope, subject, id, attribute, written_by, created_at FROM facts WHERE status = 'current'",
        );
        if let Some((created, id)) = stored.as_deref().and_then(|c| c.split_once('|')) {
            sql.push_str(" AND (created_at > ? OR (created_at = ? AND id > ?))");
            values.push(text(created));
            values.push(text(created));
            values.push(text(id));
        }
        sql.push_str(" ORDER BY created_at ASC, id ASC LIMIT ?");
        values.push(SeaValue::BigInt(Some(limit as i64)));
        let rows = self
            .db
            .query(&Statement::with_values(sql, values))
            .await
            .map_err(PageError::Store)?;
        let next = if rows.rows.len() < limit {
            None
        } else {
            rows.rows.last().map(|row| {
                format!(
                    "{}|{}",
                    row.get::<String>("created_at").unwrap_or_default(),
                    row.get::<String>("id").unwrap_or_default()
                )
            })
        };
        self.db
            .execute(&Statement::with_values(
                "INSERT INTO fact_rule_scan (name, cursor, updated_at) VALUES ('facts', ?, ?) \
                 ON CONFLICT (name) DO UPDATE SET cursor = excluded.cursor, \
                 updated_at = excluded.updated_at",
                vec![opt_text(next.as_deref()), text(&timeline::rfc3339(now))],
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(rows
            .rows
            .iter()
            .map(|row| RuleFact {
                scope: row.get::<String>("scope").unwrap_or_default(),
                subject: row.get::<String>("subject").unwrap_or_default(),
                id: row.get::<String>("id").unwrap_or_default(),
                attribute: row.get::<String>("attribute").unwrap_or_default(),
                written_by: row.get::<String>("written_by").unwrap_or_default(),
            })
            .collect())
    }

    /// The attribute vocabulary of a subject's current facts: each distinct
    /// attribute with how many current facts carry it and the id of its
    /// latest fact (ids are time-ordered, so the largest is the newest), by
    /// name. Plaintext columns only; at most 300 keys.
    ///
    /// # Errors
    ///
    /// [`FactError::Store`].
    pub async fn dream_vocabulary(
        &self,
        scope: &str,
        subject: &str,
    ) -> Result<Vec<(String, i64, String)>, FactError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT attribute, COUNT(*) AS n, MAX(id) AS latest FROM facts \
                 WHERE scope = ? AND subject = ? AND status = 'current' \
                 GROUP BY attribute ORDER BY attribute ASC LIMIT 300",
                vec![text(scope), text(subject)],
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(rows
            .rows
            .iter()
            .map(|row| {
                (
                    row.get::<String>("attribute").unwrap_or_default(),
                    row.get::<i64>("n").unwrap_or_default(),
                    row.get::<String>("latest").unwrap_or_default(),
                )
            })
            .collect())
    }

    /// Writes one decision of a pass to the decision log, applied or not.
    ///
    /// # Errors
    ///
    /// [`FactError::Store`].
    pub async fn dream_log(&self, row: &DreamDecisionRow<'_>) -> Result<(), FactError> {
        self.db
            .execute(&Statement::with_values(
                "INSERT INTO fact_dream_decisions (scope, dream_at, seq, subject, op, fact_id, \
                 by_fact_id, new_valid_to, new_attribute, reason, applied, input_ids, model, \
                 decided_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                vec![
                    text(row.scope),
                    text(row.dream_at),
                    SeaValue::BigInt(Some(row.seq as i64)),
                    text(row.subject),
                    text(row.op),
                    text(row.fact_id),
                    opt_text(row.by_fact_id),
                    opt_text(row.new_valid_to),
                    opt_text(row.new_attribute),
                    opt_text(row.reason),
                    SeaValue::BigInt(Some(i64::from(row.applied))),
                    text(row.input_ids),
                    text(row.model),
                    text(row.decided_at),
                ],
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(())
    }

    /// The decision log in `scopes`, newest pass first, then by scope and
    /// position in the pass. `before` is the `(dream_at, scope, seq)` of the
    /// last row of the previous page; `limit` rows are read (the caller asks
    /// for one more to know whether a page follows). Scope-filtered like every
    /// read here.
    ///
    /// # Errors
    ///
    /// [`FactError::Store`].
    pub async fn dream_decisions(
        &self,
        scopes: &[&str],
        before: Option<(&str, &str, i64)>,
        limit: usize,
    ) -> Result<Vec<DreamDecisionRecord>, FactError> {
        if scopes.is_empty() {
            return Ok(Vec::new());
        }
        // The cursor and the limit are the fixed binds: 5 and 1.
        let fixed = 1 + if before.is_some() { 5 } else { 0 };
        let mut fetched: Vec<Row> = Vec::new();
        for group in scope_groups(scopes, fixed) {
            let mut values = Vec::new();
            let placeholders = in_list(&group, &mut values);
            let mut sql = format!(
                "SELECT scope, dream_at, seq, subject, op, fact_id, by_fact_id, new_valid_to, \
                 new_attribute, reason, applied, model, decided_at FROM fact_dream_decisions \
                 WHERE scope IN ({placeholders})"
            );
            if let Some((dream_at, scope, seq)) = before {
                sql.push_str(
                    " AND (dream_at < ? OR (dream_at = ? AND (scope > ? OR (scope = ? AND seq > ?))))",
                );
                values.push(text(dream_at));
                values.push(text(dream_at));
                values.push(text(scope));
                values.push(text(scope));
                values.push(SeaValue::BigInt(Some(seq)));
            }
            sql.push_str(" ORDER BY dream_at DESC, scope ASC, seq ASC LIMIT ?");
            values.push(SeaValue::BigInt(Some(limit as i64)));
            let rows = self
                .db
                .query(&Statement::with_values(sql, values))
                .await
                .map_err(PageError::Store)?;
            fetched.extend(rows.rows);
        }
        // The order one statement would have used, cut to the limit.
        fetched.sort_by(|a, b| {
            let key = |row: &Row| {
                (
                    std::cmp::Reverse(row.get::<String>("dream_at")),
                    row.get::<String>("scope"),
                    row.get::<i64>("seq"),
                )
            };
            key(a).cmp(&key(b))
        });
        fetched.truncate(limit);
        Ok(fetched
            .iter()
            .map(|row| DreamDecisionRecord {
                scope: row.get::<String>("scope").unwrap_or_default(),
                dream_at: row.get::<String>("dream_at").unwrap_or_default(),
                seq: row.get::<i64>("seq").unwrap_or_default(),
                subject: row.get::<String>("subject").unwrap_or_default(),
                op: row.get::<String>("op").unwrap_or_default(),
                fact_id: row.get::<String>("fact_id").unwrap_or_default(),
                by_fact_id: row.get::<String>("by_fact_id"),
                new_valid_to: row.get::<String>("new_valid_to"),
                new_attribute: row.get::<String>("new_attribute"),
                reason: row.get::<String>("reason"),
                applied: row.get::<i64>("applied").unwrap_or_default() != 0,
                model: row.get::<String>("model").unwrap_or_default(),
                decided_at: row.get::<String>("decided_at").unwrap_or_default(),
            })
            .collect())
    }

    /// What dreaming did in the asker's scopes: each completed pass per
    /// subject, and every applied decision (`fact id`, `op`), so a timeline
    /// can say a fact was ended while dreaming. Scope-filtered like every
    /// read here.
    ///
    /// # Errors
    ///
    /// [`FactError::Store`].
    pub async fn dream_outcomes(
        &self,
        scopes: &[&str],
        subject: Option<&str>,
    ) -> Result<DreamOutcomes, FactError> {
        if scopes.is_empty() {
            return Ok(DreamOutcomes::default());
        }
        let fixed = usize::from(subject.is_some());
        let mut pass_rows: Vec<Row> = Vec::new();
        let mut edit_rows: Vec<Row> = Vec::new();
        for group in scope_groups(scopes, fixed) {
            let mut values = Vec::new();
            let placeholders = in_list(&group, &mut values);
            let mut passes_sql = format!(
                "SELECT scope, subject, dreamt_at, facts_seen, decisions, applied FROM fact_dreams \
                 WHERE scope IN ({placeholders}) AND dreamt_at IS NOT NULL"
            );
            let mut edits_sql = format!(
                "SELECT scope, subject, fact_id, op, dream_at, seq FROM fact_dream_decisions \
                 WHERE scope IN ({placeholders}) AND applied = 1"
            );
            if let Some(subject) = subject {
                passes_sql.push_str(" AND subject = ?");
                edits_sql.push_str(" AND subject = ?");
                values.push(text(subject));
            }
            passes_sql.push_str(" ORDER BY dreamt_at DESC LIMIT 500");
            edits_sql.push_str(" ORDER BY dream_at DESC, seq ASC LIMIT 2000");
            let passes = self
                .db
                .query(&Statement::with_values(passes_sql, values.clone()))
                .await
                .map_err(PageError::Store)?;
            let edits = self
                .db
                .query(&Statement::with_values(edits_sql, values))
                .await
                .map_err(PageError::Store)?;
            pass_rows.extend(passes.rows);
            edit_rows.extend(edits.rows);
        }
        // Each list in the order its statement used, cut to the same cap.
        pass_rows.sort_by(|a, b| {
            std::cmp::Reverse(a.get::<String>("dreamt_at"))
                .cmp(&std::cmp::Reverse(b.get::<String>("dreamt_at")))
        });
        pass_rows.truncate(500);
        edit_rows.sort_by(|a, b| {
            let key = |row: &Row| {
                (
                    std::cmp::Reverse(row.get::<String>("dream_at")),
                    row.get::<i64>("seq"),
                )
            };
            key(a).cmp(&key(b))
        });
        edit_rows.truncate(2000);
        Ok(DreamOutcomes {
            passes: pass_rows
                .iter()
                .map(|row| DreamPass {
                    scope: row.get::<String>("scope").unwrap_or_default(),
                    subject: row.get::<String>("subject").unwrap_or_default(),
                    at: row.get::<String>("dreamt_at").unwrap_or_default(),
                    facts_seen: row.get::<i64>("facts_seen").unwrap_or_default(),
                    decisions: row.get::<i64>("decisions").unwrap_or_default(),
                    applied: row.get::<i64>("applied").unwrap_or_default(),
                })
                .collect(),
            edits: edit_rows
                .iter()
                .map(|row| {
                    (
                        row.get::<String>("scope").unwrap_or_default(),
                        row.get::<String>("subject").unwrap_or_default(),
                        row.get::<String>("fact_id").unwrap_or_default(),
                        row.get::<String>("op").unwrap_or_default(),
                    )
                })
                .collect(),
        })
    }

    /// Seals a fact's payload under the scope's active key and inserts it.
    async fn insert(
        &self,
        scope: &str,
        fact: &TimedFact,
        written_by: &str,
    ) -> Result<(), FactError> {
        let active = self.keys.active(scope).await?;
        let sealed = seal_payload(&active.dek, active.version, scope, fact)?;
        let now = timeline::rfc3339(self.now());
        self.db
            .execute(&Statement::with_values(
                "INSERT INTO facts (scope, id, subject, attribute, cardinality, valid_from, \
                 valid_from_at, stated_valid_to, valid_to, valid_to_at, observed_at, \
                 last_confirmed_at, status, supersedes, superseded_by, confidence, sealed, \
                 key_version, written_by, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                vec![
                    text(scope),
                    text(&fact.id),
                    text(&fact.subject),
                    text(&fact.attribute),
                    text(fact.cardinality.as_str()),
                    text(fact.valid_from.text()),
                    text(&timeline::rfc3339(fact.valid_from.start())),
                    opt_text(fact.stated_valid_to.as_ref().map(PartialDate::text)),
                    opt_text(fact.valid_to.as_ref().map(PartialDate::text)),
                    opt_text(
                        fact.valid_to
                            .as_ref()
                            .map(|to| timeline::rfc3339(to.start()))
                            .as_deref(),
                    ),
                    text(&timeline::rfc3339(fact.observed_at)),
                    text(&timeline::rfc3339(fact.last_confirmed_at)),
                    text(fact.status.as_str()),
                    opt_text(fact.supersedes.as_deref()),
                    opt_text(fact.superseded_by.as_deref()),
                    SeaValue::BigInt(Some(per_mille(fact.confidence))),
                    text(&sealed),
                    int(active.version),
                    text(written_by),
                    text(&now),
                    text(&now),
                ],
            ))
            .await
            .map_err(PageError::Store)?;
        Ok(())
    }

    /// Rows to facts, each payload opened with the key version its row
    /// names. One unwrap per (scope, version).
    async fn decode(&self, rows: &[Row]) -> Result<Vec<(String, TimedFact)>, FactError> {
        let mut deks: HashMap<(String, u32), Dek> = HashMap::new();
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let scope = row.get::<String>("scope").unwrap_or_default();
            let version = row.get::<u32>("key_version").unwrap_or_default();
            let key = (scope.clone(), version);
            if !deks.contains_key(&key) {
                let dek = self.keys.dek(&scope, version).await?;
                deks.insert(key.clone(), dek);
            }
            let dek = deks.get(&key).expect("inserted above");
            out.push((scope.clone(), row_fact(row, dek, &scope, version)?));
        }
        Ok(out)
    }
}

/// `scopes` as `?, ?, …`, their values pushed onto `values`.
fn in_list(scopes: &[&str], values: &mut Vec<SeaValue>) -> String {
    scopes
        .iter()
        .map(|scope| {
            values.push(text(scope));
            "?"
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The distinct scopes of `scopes`, in groups that leave `fixed` binds of
/// D1's budget for the statement's other clauses. One statement per group;
/// the answers merge back into what one statement over all of `scopes`
/// would have given.
fn scope_groups<'a>(scopes: &[&'a str], fixed: usize) -> Vec<Vec<&'a str>> {
    distinct_chunks(scopes, BIND_BUDGET.saturating_sub(fixed).max(1))
}

fn per_mille(confidence: f64) -> i64 {
    // In 0..=1 by validation, so the product is in 0..=1000.
    #[allow(clippy::cast_possible_truncation)]
    let scaled = (confidence * 1000.0).round() as i64;
    scaled.clamp(0, 1000)
}

/// The sealed payload: `{value, unit, source, quote}` as JSON, sealed, base64.
pub(crate) fn seal_payload(
    dek: &Dek,
    version: u32,
    scope: &str,
    fact: &TimedFact,
) -> Result<String, PageError> {
    let payload = json!({
        "value": fact.value,
        "unit": fact.unit,
        "source": fact.source,
        "quote": fact.quote,
    })
    .to_string();
    let envelope = keys::seal(
        dek,
        version,
        &fact_aad(scope, &fact.id, version),
        payload.as_bytes(),
    )?;
    Ok(base64::engine::general_purpose::STANDARD.encode(envelope))
}

/// A payload sealed under `(old, from)` re-sealed under `(new, to)`, for a
/// key rotation. The plaintext never leaves this call.
pub(crate) fn reseal(
    old: &Dek,
    from: u32,
    new: &Dek,
    to: u32,
    scope: &str,
    id: &str,
    sealed: &str,
) -> Result<String, PageError> {
    let payload = open_payload(old, from, scope, id, sealed)?.to_string();
    let envelope = keys::seal(new, to, &fact_aad(scope, id, to), payload.as_bytes())?;
    Ok(base64::engine::general_purpose::STANDARD.encode(envelope))
}

/// Opens a sealed payload, as JSON.
pub(crate) fn open_payload(
    dek: &Dek,
    version: u32,
    scope: &str,
    id: &str,
    sealed: &str,
) -> Result<Value, PageError> {
    let envelope = base64::engine::general_purpose::STANDARD
        .decode(sealed)
        .map_err(|_| PageError::Crypto(format!("fact `{id}` is not base64")))?;
    let plaintext = keys::open(dek, version, &fact_aad(scope, id, version), &envelope)?;
    serde_json::from_slice(&plaintext)
        .map_err(|_| PageError::Corrupt(format!("fact `{id}` payload is not JSON")))
}

fn row_fact(row: &Row, dek: &Dek, scope: &str, version: u32) -> Result<TimedFact, PageError> {
    let id = row.get::<String>("id").unwrap_or_default();
    let corrupt = |what: &str| PageError::Corrupt(format!("fact `{id}`: {what}"));
    let payload = open_payload(
        dek,
        version,
        scope,
        &id,
        &row.get::<String>("sealed").unwrap_or_default(),
    )?;
    let opt = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_owned);
    let parse = |key: &str| -> Result<Option<PartialDate>, PageError> {
        row.get::<String>(key)
            .map(|raw| PartialDate::parse(&raw).map_err(|_| corrupt(key)))
            .transpose()
    };
    let status = row.get::<String>("status").unwrap_or_default();
    let cardinality = row.get::<String>("cardinality").unwrap_or_default();
    Ok(TimedFact {
        subject: row.get::<String>("subject").unwrap_or_default(),
        attribute: row.get::<String>("attribute").unwrap_or_default(),
        value: opt("value").unwrap_or_default(),
        unit: opt("unit"),
        valid_from: parse("valid_from")?.ok_or_else(|| corrupt("no valid_from"))?,
        stated_valid_to: parse("stated_valid_to")?,
        valid_to: parse("valid_to")?,
        observed_at: parse("observed_at")?.map_or(0, |at| at.start()),
        // A row from before the column has none: its observation stands in.
        last_confirmed_at: parse("last_confirmed_at")?.map_or_else(
            || {
                parse("observed_at")
                    .ok()
                    .flatten()
                    .map_or(0, |at| at.start())
            },
            |at| at.start(),
        ),
        status: FactStatus::parse(&status).ok_or_else(|| corrupt("status"))?,
        supersedes: row.get::<String>("supersedes"),
        superseded_by: row.get::<String>("superseded_by"),
        source: opt("source"),
        quote: opt("quote"),
        confidence: f64::from(
            u16::try_from(row.get::<i64>("confidence").unwrap_or(1000)).unwrap_or(1000),
        ) / 1000.0,
        cardinality: Cardinality::parse(&cardinality).ok_or_else(|| corrupt("cardinality"))?,
        id,
    })
}

// ---------------------------------------------------------------------------
// Reading facts for a question: shared by the HTTP routes and the MCP tools.

/// The most timelines a question pulls into a brief.
pub const MAX_BRIEF_TIMELINES: usize = 20;

/// Words that say nothing about which fact a question is after.
const STOP: &[&str] = &[
    "the",
    "and",
    "for",
    "what",
    "whats",
    "what's",
    "was",
    "were",
    "is",
    "are",
    "who",
    "which",
    "when",
    "how",
    "does",
    "did",
    "do",
    "has",
    "have",
    "had",
    "now",
    "currently",
    "current",
    "this",
    "that",
    "there",
    "their",
    "about",
    "with",
    "from",
    "into",
    "you",
    "your",
    "our",
];

/// The words of a question, lowercased, at least three letters, no stop
/// words.
fn words(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| w.len() >= 3 && !STOP.contains(&w.as_str()))
        .collect()
}

/// Whether a question speaks about the person asking it.
fn about_self(text: &str) -> bool {
    text.split(|c: char| !c.is_alphanumeric() && c != '\'')
        .map(str::to_lowercase)
        .any(|w| matches!(w.as_str(), "my" | "me" | "i" | "mine" | "i'm" | "myself"))
}

/// The attribute family of `attribute`: the key without its last `_`
/// segment (`auth_provider_github` → `auth_provider`, `auth_provider_oidc`
/// → `auth_provider`). A key with no `_` is its own family and `None`
/// here: nothing to cluster it with.
#[must_use]
pub fn family_of(attribute: &str) -> Option<&str> {
    attribute
        .rsplit_once('_')
        .map(|(family, _)| family)
        .filter(|f| !f.is_empty())
}

/// The timelines a question is about, read in full: those whose attribute
/// shares a word with the question (`shoe size` → `shoe_size`), on a
/// subject the question names or the asker themselves for `my`/`me`; or
/// every attribute of a subject the question names outright. Chosen from
/// the plaintext index, so only the chosen ones are decrypted. At most
/// [`MAX_BRIEF_TIMELINES`].
///
/// # Errors
///
/// [`FactError::Store`].
pub async fn relevant(
    store: &FactStore,
    user_id: &str,
    scopes: &[String],
    question: &str,
) -> Result<Vec<Timeline>, FactError> {
    relevant_where(store, user_id, scopes, question, |_, _| true).await
}

/// [`relevant`], choosing only among the `(scope, subject)` pairs `allowed`
/// says yes to (ADR 0004: a subject-prefix grant). The filter runs on the
/// plaintext index **before** anything is chosen, truncated or decrypted, so
/// a timeline the asker may not read never crowds out one they may, and is
/// never opened.
///
/// # Errors
///
/// [`FactError::Store`].
pub async fn relevant_where(
    store: &FactStore,
    user_id: &str,
    scopes: &[String],
    question: &str,
    allowed: impl Fn(&str, &str) -> bool,
) -> Result<Vec<Timeline>, FactError> {
    let asked = words(question);
    if asked.is_empty() && !about_self(question) {
        return Ok(Vec::new());
    }
    let me = person_subject(user_id);
    let me_too = about_self(question);
    let names: Vec<&str> = scopes.iter().map(String::as_str).collect();
    let index = store.index(&names).await?;
    let mut chosen: Vec<(usize, (String, String, String))> = Vec::new();
    for key in index {
        let (scope, subject, attribute) = &key;
        if !allowed(scope, subject) {
            continue;
        }
        let slug = subject.split_once('/').map_or("", |(_, slug)| slug);
        let subject_named = slug
            .split('-')
            .filter(|part| part.len() >= 3)
            .any(|part| asked.contains(part))
            || (me_too && me.as_deref() == Some(subject.as_str()));
        let attribute_hits = attribute
            .split('_')
            .filter(|part| asked.contains(*part))
            .count();
        let named_outright = slug.len() >= 3 && asked.contains(slug);
        let score = match (subject_named, attribute_hits) {
            (true, hits) if hits > 0 => 10 + hits,
            (_, hits) if hits > 0 && !me_too => hits,
            (true, _) if named_outright => 1,
            _ => 0,
        };
        if score > 0 {
            chosen.push((score, key));
        }
    }
    chosen.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    chosen.truncate(MAX_BRIEF_TIMELINES);
    let mut out = Vec::with_capacity(chosen.len());
    for (_, (scope, subject, attribute)) in chosen {
        out.push(store.timeline(&scope, &subject, &attribute).await?);
    }
    Ok(out)
}

/// Fact lines for a brief, each with the index of the timeline it renders
/// (a timeline with nothing but retractions renders none), each fact's
/// source cited as `F1`, `F2`, … in first-seen order, and the citations
/// those labels name.
pub fn brief_lines(timelines: &[Timeline], at: i64) -> (Vec<(usize, String)>, Vec<Value>) {
    let mut labels: Vec<(String, Option<String>)> = Vec::new();
    let mut lines = Vec::new();
    for (index, timeline) in timelines.iter().enumerate() {
        // Labels are assigned before the line is rendered, in the order the
        // line cites them (the facts valid at `at`), so two facts on one
        // line never share a label and the numbering is stable.
        for fact in timeline::valid_at(&timeline.facts, at) {
            if let Some(source) = &fact.source
                && !labels.iter().any(|(seen, _)| seen == source)
            {
                labels.push((source.clone(), fact.quote.clone()));
            }
        }
        let cite = |fact: &TimedFact| -> Option<String> {
            let source = fact.source.as_ref()?;
            labels
                .iter()
                .position(|(seen, _)| seen == source)
                .map(|p| format!("F{}", p + 1))
        };
        if let Some(line) = timeline::render_line(&timeline.facts, at, &cite) {
            lines.push((index, line));
        }
    }
    let citations = labels
        .into_iter()
        .enumerate()
        .map(|(index, (source, quote))| {
            json!({
                "title": format!("F{}", index + 1),
                "source": source,
                "quote": quote,
            })
        })
        .collect();
    (lines, citations)
}

/// The facts section of a brief: the convention, then one line per
/// timeline. Empty when there are no lines.
pub fn brief_section(lines: &[(usize, String)], at: i64) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "Facts (as of {}):\n{}\n\n",
        timeline::rfc3339(at),
        timeline::PROMPT_CONVENTION
    );
    for (_, line) in lines {
        out.push_str("- ");
        out.push_str(line);
        out.push('\n');
    }
    out
}
