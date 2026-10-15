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
        "status": fact.status.as_str(),
        "supersedes": fact.supersedes,
        "superseded_by": fact.superseded_by,
        "source": fact.source,
        "quote": fact.quote,
        "confidence": fact.confidence,
        "cardinality": fact.cardinality.as_str(),
    })
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
        let subject = input.subject.trim().to_owned();
        if !is_subject(&subject) {
            return Err(FactError::Invalid(
                "`subject` must be `<entity_type>/<slug>`, e.g. `person/nick`".to_owned(),
            ));
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
            return Ok(Recorded {
                outcome: FactOutcome::Corroborated,
                scope: scope.to_owned(),
                fact: found.clone(),
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
        let mut values = vec![text(id)];
        let placeholders = in_list(scopes, &mut values);
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
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let scope = row.get::<String>("scope").unwrap_or_default();
        let subject = row.get::<String>("subject").unwrap_or_default();
        let attribute = row.get::<String>("attribute").unwrap_or_default();
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
        let mut values = Vec::new();
        let placeholders = in_list(scopes, &mut values);
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
        let facts = self.decode(&rows.rows).await?;
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
    /// most [`MAX_INDEX_KEYS`].
    ///
    /// # Errors
    ///
    /// [`FactError::Store`].
    pub async fn index(&self, scopes: &[&str]) -> Result<Vec<(String, String, String)>, FactError> {
        if scopes.is_empty() {
            return Ok(Vec::new());
        }
        let mut values = Vec::new();
        let placeholders = in_list(scopes, &mut values);
        values.push(SeaValue::BigInt(Some(MAX_INDEX_KEYS as i64)));
        let rows = self
            .db
            .query(&Statement::with_values(
                format!(
                    "SELECT DISTINCT scope, subject, attribute FROM facts \
                     WHERE scope IN ({placeholders}) \
                     ORDER BY subject ASC, attribute ASC, scope ASC LIMIT ?"
                ),
                values,
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
                    row.get::<String>("attribute").unwrap_or_default(),
                )
            })
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
                 valid_from_at, stated_valid_to, valid_to, valid_to_at, observed_at, status, \
                 supersedes, superseded_by, confidence, sealed, key_version, written_by, \
                 created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        let (_, subject, attribute) = &key;
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
