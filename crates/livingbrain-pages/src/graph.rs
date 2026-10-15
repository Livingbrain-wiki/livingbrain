//! The brain map's inputs (`GET /v1/graph`): the shape of what the asker
//! may read, as rows — never a body.
//!
//! Every query here carries the same predicate the listings do: the page
//! tables by the asker's page scopes (the scope folds the workspace in), the
//! source ledger by the asker's workspace **and** scopes. Nothing is opened
//! from the blob store and nothing is decrypted: a page's title, a fact's
//! value and a source's body are sealed, and a map of thousands of nodes has
//! no business unsealing them. What is drawn is slugs, kinds, projects,
//! dates, links and counts.
//!
//! Each query is capped at `cap` rows (plus one, to tell a full table from a
//! truncated read), so one request's work is bounded however large the
//! workspace grows; [`GraphRows::truncated`] says when a cap was reached.

use cratefield_core::{Database, DbError, Statement};
use sea_query::Value as SeaValue;

use crate::store::text;

/// One page head: where it lives, what it is, and when it was first and
/// last written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphPage {
    pub scope: String,
    pub slug: String,
    pub entity_type: String,
    pub version: u32,
    /// RFC 3339: the head's write.
    pub updated_at: String,
    /// RFC 3339: the first version's write, when the history has one.
    pub created_at: Option<String>,
}

/// One `[[link]]` edge, inside one scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphLink {
    pub scope: String,
    pub from: String,
    pub to: String,
}

/// One source ledger row's columns (no body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphSource {
    pub id: String,
    pub scope: String,
    pub kind: String,
    pub rel_path: String,
    pub project: Option<String>,
    pub created_at: String,
}

/// One fact subject: how many facts (any status but retracted) the asker
/// may read about it, and when the first was observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphSubject {
    pub scope: String,
    pub subject: String,
    pub facts: u64,
    pub first_at: Option<String>,
}

/// Everything the map is built from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphRows {
    pub pages: Vec<GraphPage>,
    pub links: Vec<GraphLink>,
    pub sources: Vec<GraphSource>,
    pub subjects: Vec<GraphSubject>,
    /// Some table held more than `cap` readable rows; the rows above are the
    /// newest `cap` of it.
    pub truncated: bool,
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

fn limit(cap: usize) -> SeaValue {
    SeaValue::BigInt(Some(
        i64::try_from(cap.saturating_add(1)).unwrap_or(i64::MAX),
    ))
}

/// Reads the map's rows for an asker: `workspace` is the asker's own (plain)
/// id, `scopes` the page scopes their credential reaches. An empty scope
/// list or a zero cap reads nothing, without a query.
///
/// # Errors
///
/// [`DbError`] if the database fails.
pub async fn graph_rows(
    db: &dyn Database,
    workspace: &str,
    scopes: &[&str],
    cap: usize,
) -> Result<GraphRows, DbError> {
    let mut out = GraphRows::default();
    if scopes.is_empty() || cap == 0 {
        return Ok(out);
    }
    let marks = placeholders(scopes.len());
    let in_scopes = || scopes.iter().map(|scope| text(scope)).collect::<Vec<_>>();

    // Page heads, newest first, each with its first version's date.
    let mut values = in_scopes();
    values.push(limit(cap));
    let rows = db
        .query(&Statement::with_values(
            format!(
                "SELECT p.scope AS scope, p.slug AS slug, p.entity_type AS entity_type, \
                        p.head_version AS head_version, p.updated_at AS updated_at, \
                        MIN(v.created_at) AS created_at \
                 FROM pages p LEFT JOIN page_versions v \
                   ON v.scope = p.scope AND v.slug = p.slug \
                 WHERE p.scope IN ({marks}) \
                 GROUP BY p.scope, p.slug, p.entity_type, p.head_version, p.updated_at \
                 ORDER BY p.updated_at DESC, p.slug ASC LIMIT ?"
            ),
            values,
        ))
        .await?;
    for row in &rows.rows {
        out.pages.push(GraphPage {
            scope: row.get::<String>("scope").unwrap_or_default(),
            slug: row.get::<String>("slug").unwrap_or_default(),
            entity_type: row.get::<String>("entity_type").unwrap_or_default(),
            version: row.get::<u32>("head_version").unwrap_or_default(),
            updated_at: row.get::<String>("updated_at").unwrap_or_default(),
            created_at: row.get::<String>("created_at"),
        });
    }
    if out.pages.len() > cap {
        out.pages.truncate(cap);
        out.truncated = true;
    }

    // The link table, inside the same scopes.
    let mut values = in_scopes();
    values.push(limit(cap.saturating_mul(4)));
    let rows = db
        .query(&Statement::with_values(
            format!(
                "SELECT scope, from_slug, to_slug FROM page_links \
                 WHERE scope IN ({marks}) ORDER BY scope, from_slug, to_slug LIMIT ?"
            ),
            values,
        ))
        .await?;
    for row in &rows.rows {
        out.links.push(GraphLink {
            scope: row.get::<String>("scope").unwrap_or_default(),
            from: row.get::<String>("from_slug").unwrap_or_default(),
            to: row.get::<String>("to_slug").unwrap_or_default(),
        });
    }
    if out.links.len() > cap.saturating_mul(4) {
        out.links.truncate(cap.saturating_mul(4));
        out.truncated = true;
    }

    // The source ledger: the asker's workspace AND scopes, newest first.
    let mut values = vec![text(workspace)];
    values.extend(in_scopes());
    values.push(limit(cap));
    let rows = db
        .query(&Statement::with_values(
            format!(
                "SELECT id, scope, kind, rel_path, project, created_at FROM sources \
                 WHERE workspace = ? AND scope IN ({marks}) ORDER BY id DESC LIMIT ?"
            ),
            values,
        ))
        .await?;
    for row in &rows.rows {
        out.sources.push(GraphSource {
            id: row.get::<String>("id").unwrap_or_default(),
            scope: row.get::<String>("scope").unwrap_or_default(),
            kind: row.get::<String>("kind").unwrap_or_default(),
            rel_path: row.get::<String>("rel_path").unwrap_or_default(),
            project: row.get::<String>("project"),
            created_at: row.get::<String>("created_at").unwrap_or_default(),
        });
    }
    if out.sources.len() > cap {
        out.sources.truncate(cap);
        out.truncated = true;
    }

    // Fact subjects, counted; the values stay sealed.
    let mut values = in_scopes();
    values.push(text("retracted"));
    values.push(limit(cap));
    let rows = db
        .query(&Statement::with_values(
            format!(
                "SELECT scope, subject, COUNT(*) AS n, MIN(observed_at) AS first_at FROM facts \
                 WHERE scope IN ({marks}) AND status <> ? \
                 GROUP BY scope, subject ORDER BY scope, subject LIMIT ?"
            ),
            values,
        ))
        .await?;
    for row in &rows.rows {
        out.subjects.push(GraphSubject {
            scope: row.get::<String>("scope").unwrap_or_default(),
            subject: row.get::<String>("subject").unwrap_or_default(),
            facts: row
                .get::<i64>("n")
                .and_then(|n| u64::try_from(n).ok())
                .unwrap_or_default(),
            first_at: row.get::<String>("first_at"),
        });
    }
    if out.subjects.len() > cap {
        out.subjects.truncate(cap);
        out.truncated = true;
    }
    Ok(out)
}
