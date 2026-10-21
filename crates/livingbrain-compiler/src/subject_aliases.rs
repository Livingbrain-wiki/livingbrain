//! Subject aliases and the subject a compiled fact is filed under.
//!
//! One entity is often written under several names (`project/world369`,
//! `project/earthos-world`, `project/aura`). Each workspace keeps a small
//! alias table (`fact_subject_aliases`): an alias maps to one canonical
//! subject, and a fact the compiler writes under an alias is stored under
//! the canonical subject. The table holds one hop only: a canonical subject
//! is never itself an alias, which [`set`] refuses.
//!
//! Aliases apply to what the compiler writes from now on; facts already
//! stored under an alias are merged by the dreaming pass, which is the only
//! writer that changes stored facts.

use cratefield_core::{Database, DbError, Statement};
use sea_query::Value as SeaValue;
use time::OffsetDateTime;

use crate::facts::{Fact, subject_ok};
use crate::queue::rfc3339;

/// The alias pairs of one workspace: `(alias, canonical)`.
pub type Aliases = Vec<(String, String)>;

/// The workspace's aliases, by alias. Empty when none are set.
///
/// # Errors
///
/// [`DbError`] from the database.
pub async fn load(db: &dyn Database, workspace: &str) -> Result<Aliases, DbError> {
    let rows = db
        .query(&Statement::with_values(
            "SELECT alias, canonical FROM fact_subject_aliases WHERE workspace_id = ? \
             ORDER BY alias ASC",
            vec![text(workspace)],
        ))
        .await?;
    Ok(rows
        .rows
        .iter()
        .filter_map(|row| Some((row.get::<String>("alias")?, row.get::<String>("canonical")?)))
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

/// Sets `alias` to `canonical` in `workspace`, at `at`.
///
/// Refused, with the reason, when either name is not a subject the compiler
/// may write, when they are the same, or when `canonical` is itself an
/// alias (the table holds one hop; a chain would be ambiguous to read).
/// Also refused when `alias` is already the canonical subject of other
/// aliases, which would leave those pointing at an alias.
///
/// # Errors
///
/// A fixed reason for a refused pair, or `"store"` when the write fails.
pub async fn set(
    db: &dyn Database,
    workspace: &str,
    alias: &str,
    canonical_subject: &str,
    at: OffsetDateTime,
) -> Result<(), &'static str> {
    if !subject_ok(alias) || !subject_ok(canonical_subject) {
        return Err("bad-subject");
    }
    if alias == canonical_subject {
        return Err("same-subject");
    }
    let current = load(db, workspace).await.map_err(|_| "store")?;
    if current.iter().any(|(a, _)| a == canonical_subject) {
        return Err("canonical-is-alias");
    }
    if current.iter().any(|(_, c)| c == alias) {
        return Err("alias-is-canonical");
    }
    db.execute(&Statement::with_values(
        "INSERT INTO fact_subject_aliases (workspace_id, alias, canonical, updated_at) \
         VALUES (?, ?, ?, ?) \
         ON CONFLICT (workspace_id, alias) DO UPDATE SET \
         canonical = excluded.canonical, updated_at = excluded.updated_at",
        vec![
            text(workspace),
            text(alias),
            text(canonical_subject),
            text(&rfc3339(at)),
        ],
    ))
    .await
    .map(|_| ())
    .map_err(|_| "store")
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
