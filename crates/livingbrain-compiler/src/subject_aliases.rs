//! Subject aliases and the subject a compiled fact is filed under.
//!
//! One entity is often written under several names (`project/world369`,
//! `project/earthos-world`, `project/aura`). Each workspace keeps an alias
//! table (`fact_subject_aliases`), one row per alias:
//!
//! - `set`: in force. The owner set it, or accepted a proposal; `set_by` is
//!   them. The compiler files new writes under the canonical subject, and the
//!   dreaming rule pass moves existing facts there.
//! - `proposed`: dreaming noticed two subjects sharing most of their
//!   attribute families. Nothing is applied until an owner accepts it.
//! - `dismissed`: an owner removed an alias or turned a proposal down. Kept
//!   so the same pair is not proposed again.
//!
//! The table holds one hop only: a `set` canonical subject is never itself a
//! `set` alias, which [`set`] refuses.

use cratefield_core::{Database, DbError, Statement};
use sea_query::Value as SeaValue;
use time::OffsetDateTime;

use crate::facts::{Fact, subject_ok};
use crate::queue::rfc3339;

/// The alias pairs in force in one workspace: `(alias, canonical)`.
pub type Aliases = Vec<(String, String)>;

/// One row of the alias table, as an owner reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasRow {
    pub alias: String,
    pub canonical: String,
    /// `set`, `proposed` or `dismissed`.
    pub state: String,
    /// How many attribute families the two subjects shared (a proposal's
    /// evidence; `0` for a row set by hand).
    pub shared: i64,
    /// The owner who set or accepted it; blank for a proposal.
    pub set_by: String,
    pub updated_at: String,
}

/// Every row of the workspace's table, any state, by alias.
///
/// # Errors
///
/// [`DbError`] from the database.
pub async fn rows(db: &dyn Database, workspace: &str) -> Result<Vec<AliasRow>, DbError> {
    let rows = db
        .query(&Statement::with_values(
            "SELECT alias, canonical, state, shared, set_by, updated_at \
             FROM fact_subject_aliases WHERE workspace_id = ? ORDER BY alias ASC",
            vec![text(workspace)],
        ))
        .await?;
    Ok(rows
        .rows
        .iter()
        .filter_map(|row| {
            Some(AliasRow {
                alias: row.get::<String>("alias")?,
                canonical: row.get::<String>("canonical")?,
                state: row.get::<String>("state").unwrap_or_default(),
                shared: row.get::<i64>("shared").unwrap_or_default(),
                set_by: row.get::<String>("set_by").unwrap_or_default(),
                updated_at: row.get::<String>("updated_at").unwrap_or_default(),
            })
        })
        .collect())
}

/// The aliases in force (`set`) in the workspace, by alias.
///
/// # Errors
///
/// [`DbError`] from the database.
pub async fn load(db: &dyn Database, workspace: &str) -> Result<Aliases, DbError> {
    Ok(rows(db, workspace)
        .await?
        .into_iter()
        .filter(|row| row.state == "set")
        .map(|row| (row.alias, row.canonical))
        .collect())
}

/// The canonical subject for `subject`: its alias target, or itself.
#[must_use]
pub fn canonical<'a>(aliases: &'a [(String, String)], subject: &'a str) -> &'a str {
    aliases
        .iter()
        .find(|(alias, _)| alias == subject)
        .map_or(subject, |(_, canonical)| canonical.as_str())
}

/// Points each fact at its canonical subject, in place.
pub fn apply(aliases: &[(String, String)], facts: &mut [Fact]) {
    if aliases.is_empty() {
        return;
    }
    for fact in facts {
        let target = canonical(aliases, &fact.subject).to_owned();
        fact.subject = target;
    }
}

/// [`load`] then [`apply`]: the facts as the workspace files them. A read
/// failure leaves the facts as they are: an alias is never a reason to lose
/// a fact.
pub async fn canonicalize(db: &dyn Database, workspace: &str, facts: &mut [Fact]) {
    if let Ok(aliases) = load(db, workspace).await {
        apply(&aliases, facts);
    }
}

/// Why an alias pair is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// A name is not a subject the compiler may write.
    BadSubject,
    /// Alias and canonical subject are the same.
    SameSubject,
    /// The canonical subject is itself an alias in force.
    CanonicalIsAlias,
    /// The alias is already the canonical subject of other aliases in force.
    AliasIsCanonical,
    /// No proposal for this alias to accept.
    NotProposed,
    /// The write failed.
    Store,
}

impl Refused {
    /// The fixed word a caller can show.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BadSubject => "bad-subject",
            Self::SameSubject => "same-subject",
            Self::CanonicalIsAlias => "canonical-is-alias",
            Self::AliasIsCanonical => "alias-is-canonical",
            Self::NotProposed => "not-proposed",
            Self::Store => "store",
        }
    }
}

/// Sets `alias` to `canonical` in `workspace` in force, by `by` at `at`
/// (an owner's choice, or an accepted proposal).
///
/// # Errors
///
/// The [`Refused`] reason for a pair that is not allowed, or a store failure.
pub async fn set(
    db: &dyn Database,
    workspace: &str,
    alias: &str,
    canonical_subject: &str,
    by: &str,
    at: OffsetDateTime,
) -> Result<(), Refused> {
    if !subject_ok(alias) || !subject_ok(canonical_subject) || alias == "me" {
        return Err(Refused::BadSubject);
    }
    if alias == canonical_subject {
        return Err(Refused::SameSubject);
    }
    let current = load(db, workspace).await.map_err(|_| Refused::Store)?;
    if current.iter().any(|(a, _)| a == canonical_subject) {
        return Err(Refused::CanonicalIsAlias);
    }
    if current.iter().any(|(_, c)| c == alias) {
        return Err(Refused::AliasIsCanonical);
    }
    db.execute(&Statement::with_values(
        "INSERT INTO fact_subject_aliases \
         (workspace_id, alias, canonical, state, shared, set_by, updated_at) \
         VALUES (?, ?, ?, 'set', 0, ?, ?) \
         ON CONFLICT (workspace_id, alias) DO UPDATE SET \
         canonical = excluded.canonical, state = 'set', shared = 0, \
         set_by = excluded.set_by, updated_at = excluded.updated_at",
        vec![
            text(workspace),
            text(alias),
            text(canonical_subject),
            text(by),
            text(&rfc3339(at)),
        ],
    ))
    .await
    .map(|_| ())
    .map_err(|_| Refused::Store)
}

/// Removes `alias` from the workspace's aliases: an alias in force, or a
/// proposal, is dismissed so it is not proposed again. Answers whether a row
/// was there to dismiss.
///
/// # Errors
///
/// [`Refused::Store`] when the write fails.
pub async fn remove(
    db: &dyn Database,
    workspace: &str,
    alias: &str,
    by: &str,
    at: OffsetDateTime,
) -> Result<bool, Refused> {
    let changed = db
        .execute(&Statement::with_values(
            "UPDATE fact_subject_aliases SET state = 'dismissed', set_by = ?, updated_at = ? \
             WHERE workspace_id = ? AND alias = ? AND state <> 'dismissed'",
            vec![text(by), text(&rfc3339(at)), text(workspace), text(alias)],
        ))
        .await
        .map_err(|_| Refused::Store)?;
    Ok(changed > 0)
}

/// Records a proposal that `alias` be filed under `canonical_subject`,
/// because the two subjects share `shared` attribute families. Nothing is
/// proposed for an alias that already has a row in any state, so a dismissed
/// pair stays dismissed. Answers whether a proposal was stored.
///
/// # Errors
///
/// [`Refused::BadSubject`], [`Refused::SameSubject`], or [`Refused::Store`].
pub async fn propose(
    db: &dyn Database,
    workspace: &str,
    alias: &str,
    canonical_subject: &str,
    shared: i64,
    at: OffsetDateTime,
) -> Result<bool, Refused> {
    if !subject_ok(alias) || !subject_ok(canonical_subject) || alias == "me" {
        return Err(Refused::BadSubject);
    }
    if alias == canonical_subject {
        return Err(Refused::SameSubject);
    }
    let stored = db
        .execute(&Statement::with_values(
            "INSERT INTO fact_subject_aliases \
             (workspace_id, alias, canonical, state, shared, set_by, updated_at) \
             VALUES (?, ?, ?, 'proposed', ?, '', ?) \
             ON CONFLICT (workspace_id, alias) DO NOTHING",
            vec![
                text(workspace),
                text(alias),
                text(canonical_subject),
                SeaValue::BigInt(Some(shared)),
                text(&rfc3339(at)),
            ],
        ))
        .await
        .map_err(|_| Refused::Store)?;
    Ok(stored > 0)
}

/// Accepts the proposal for `alias`: it is set, by `by`, under the canonical
/// subject it was proposed with. Refused as the [`set`] pair is.
///
/// # Errors
///
/// [`Refused::NotProposed`] when there is no proposal for `alias`, or the
/// reason [`set`] gives.
pub async fn accept(
    db: &dyn Database,
    workspace: &str,
    alias: &str,
    by: &str,
    at: OffsetDateTime,
) -> Result<(), Refused> {
    let proposal = rows(db, workspace)
        .await
        .map_err(|_| Refused::Store)?
        .into_iter()
        .find(|row| row.alias == alias && row.state == "proposed")
        .ok_or(Refused::NotProposed)?;
    set(db, workspace, alias, &proposal.canonical, by, at).await
}

fn text(value: &str) -> SeaValue {
    SeaValue::String(Some(Box::new(value.to_owned())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs() -> Aliases {
        vec![
            ("project/aura".to_owned(), "project/earthos".to_owned()),
            ("project/world369".to_owned(), "project/earthos".to_owned()),
        ]
    }

    #[test]
    fn every_alias_reads_as_its_canonical_subject() {
        let aliases = pairs();
        assert_eq!(canonical(&aliases, "project/world369"), "project/earthos");
        assert_eq!(canonical(&aliases, "project/aura"), "project/earthos");
        assert_eq!(canonical(&aliases, "project/earthos"), "project/earthos");
        assert_eq!(canonical(&aliases, "person/nick"), "person/nick");
    }

    #[test]
    fn facts_are_refiled_under_the_canonical_subject() {
        let mut facts = vec![Fact {
            subject: "project/world369".to_owned(),
            attribute: "hosting".to_owned(),
            value: "Workers".to_owned(),
            unit: None,
            valid_from: None,
            valid_to: None,
            observed_at: None,
            source: None,
            quote: None,
            confidence: None,
            cardinality: None,
        }];
        apply(&pairs(), &mut facts);
        assert_eq!(facts[0].subject, "project/earthos");
    }
}
