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

use crate::access::ScopeFilter;
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

/// The bound parameters one statement may carry. Cloudflare D1 refuses more
/// than 100 per statement; the budget leaves room for the row limit bind.
const BIND_BUDGET: usize = 90;

/// Packs `items` into consecutive groups whose costs each stay within
/// [`BIND_BUDGET`]. An item over budget on its own gets a group of its own.
fn pack<T>(items: Vec<T>, cost: impl Fn(&T) -> usize) -> Vec<Vec<T>> {
    let mut groups = Vec::new();
    let mut current = Vec::new();
    let mut used = 0;
    for item in items {
        let weight = cost(&item);
        if !current.is_empty() && used + weight > BIND_BUDGET {
            groups.push(std::mem::take(&mut current));
            used = 0;
        }
        used += weight;
        current.push(item);
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

/// One clause of the source ledger's read: a whole scope, or some projects
/// of a scope.
#[derive(Clone)]
enum SourceClause {
    Whole(String),
    Project(String, String),
}

/// Reads the map's rows for an asker: `workspace` is the asker's own (plain)
/// id, `scopes` the page scopes their credential reaches, each read whole.
/// An empty scope list or a zero cap reads nothing, without a query.
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
    let whole: Vec<String> = scopes.iter().map(|scope| (*scope).to_owned()).collect();
    let reach = GraphReach {
        pages: whole
            .iter()
            .map(|scope| ScopeFilter::whole(scope.clone(), ""))
            .collect(),
        sources: whole.clone(),
        source_projects: Vec::new(),
        facts: whole,
        fact_subjects: Vec::new(),
    };
    graph_rows_in(db, workspace, &reach, &|_, _| false, cap).await
}

/// What one asker's map may draw from (ADR 0004), per table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphReach {
    /// The page filters: pages and `[[link]]` rows are read through them,
    /// in the query, before the cap.
    pub pages: Vec<ScopeFilter>,
    /// Scopes whose whole source ledger may be read.
    pub sources: Vec<String>,
    /// Scopes only some projects' sources of which may be read, with the
    /// project names.
    pub source_projects: Vec<(String, Vec<String>)>,
    /// Scopes whose fact subjects may all be read.
    pub facts: Vec<String>,
    /// Scopes only some fact subjects of which may be read; each row is
    /// checked by the caller's predicate before it is kept.
    pub fact_subjects: Vec<String>,
}

/// [`graph_rows`] over an asker's grants: every table read only where
/// `reach` opens it. Pages, links and sources are filtered in the query,
/// before the cap; a scope opened only for some fact subjects is read
/// separately and each subject checked with `fact_allowed(scope, subject)`
/// before it is kept, so an ungranted subject is never drawn or counted.
///
/// # Errors
///
/// [`DbError`] if the database fails.
pub async fn graph_rows_in(
    db: &dyn Database,
    workspace: &str,
    reach: &GraphReach,
    fact_allowed: &(dyn Fn(&str, &str) -> bool + Sync),
    cap: usize,
) -> Result<GraphRows, DbError> {
    let mut out = GraphRows::default();
    if cap == 0 {
        return Ok(out);
    }

    // Every statement below stays within D1's bound-parameter ceiling: a
    // grant with many projects, scopes or page prefixes is read in several
    // statements, and the answers are merged, de-duplicated and cut to the
    // same order and cap a single statement would have produced.

    // Page heads, newest first, each with its first version's date.
    if !reach.pages.is_empty() {
        let page_binds = |filter: &ScopeFilter| filter.predicate(Some("p")).1.len();
        let pieces: Vec<ScopeFilter> = reach
            .pages
            .iter()
            .flat_map(|filter| filter.split_to_budget(BIND_BUDGET - 1, &page_binds))
            .collect();
        let mut seen = std::collections::BTreeSet::new();
        let mut pages = Vec::new();
        for group in pack(pieces, page_binds) {
            let mut values = Vec::new();
            let predicate = group
                .iter()
                .map(|filter| {
                    let (sql, binds) = filter.predicate(Some("p"));
                    values.extend(binds);
                    sql
                })
                .collect::<Vec<_>>()
                .join(" OR ");
            values.push(limit(cap));
            let rows = db
                .query(&Statement::with_values(
                    format!(
                        "SELECT p.scope AS scope, p.slug AS slug, p.entity_type AS entity_type, \
                                p.head_version AS head_version, p.updated_at AS updated_at, \
                                MIN(v.created_at) AS created_at \
                         FROM pages p LEFT JOIN page_versions v \
                           ON v.scope = p.scope AND v.slug = p.slug \
                         WHERE ({predicate}) \
                         GROUP BY p.scope, p.slug, p.entity_type, p.head_version, p.updated_at \
                         ORDER BY p.updated_at DESC, p.slug ASC LIMIT ?"
                    ),
                    values,
                ))
                .await?;
            for row in &rows.rows {
                let page = GraphPage {
                    scope: row.get::<String>("scope").unwrap_or_default(),
                    slug: row.get::<String>("slug").unwrap_or_default(),
                    entity_type: row.get::<String>("entity_type").unwrap_or_default(),
                    version: row.get::<u32>("head_version").unwrap_or_default(),
                    updated_at: row.get::<String>("updated_at").unwrap_or_default(),
                    created_at: row.get::<String>("created_at"),
                };
                if seen.insert((page.scope.clone(), page.slug.clone())) {
                    pages.push(page);
                }
            }
        }
        pages.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.slug.cmp(&b.slug))
        });
        if pages.len() > cap {
            pages.truncate(cap);
            out.truncated = true;
        }
        out.pages = pages;

        // The link table, inside the same filters: both ends readable.
        let link_binds = |filter: &ScopeFilter| {
            filter.predicate_on("scope", "from_slug").1.len()
                + filter.predicate_on("scope", "to_slug").1.len()
        };
        let pieces: Vec<ScopeFilter> = reach
            .pages
            .iter()
            .flat_map(|filter| filter.split_to_budget(BIND_BUDGET / 2, &link_binds))
            .collect();
        let mut seen = std::collections::BTreeSet::new();
        let mut links = Vec::new();
        for group in pack(pieces, link_binds) {
            let mut values = Vec::new();
            let predicate = group
                .iter()
                .map(|filter| {
                    let (from, from_binds) = filter.predicate_on("scope", "from_slug");
                    let (to, to_binds) = filter.predicate_on("scope", "to_slug");
                    values.extend(from_binds);
                    values.extend(to_binds);
                    format!("({from} AND {to})")
                })
                .collect::<Vec<_>>()
                .join(" OR ");
            values.push(limit(cap.saturating_mul(4)));
            let rows = db
                .query(&Statement::with_values(
                    format!(
                        "SELECT scope, from_slug, to_slug FROM page_links \
                         WHERE ({predicate}) ORDER BY scope, from_slug, to_slug LIMIT ?"
                    ),
                    values,
                ))
                .await?;
            for row in &rows.rows {
                let link = GraphLink {
                    scope: row.get::<String>("scope").unwrap_or_default(),
                    from: row.get::<String>("from_slug").unwrap_or_default(),
                    to: row.get::<String>("to_slug").unwrap_or_default(),
                };
                if seen.insert((link.scope.clone(), link.from.clone(), link.to.clone())) {
                    links.push(link);
                }
            }
        }
        links.sort_by(|a, b| (&a.scope, &a.from, &a.to).cmp(&(&b.scope, &b.from, &b.to)));
        if links.len() > cap.saturating_mul(4) {
            links.truncate(cap.saturating_mul(4));
            out.truncated = true;
        }
        out.links = links;
    }

    // The source ledger: the asker's workspace AND the scopes (or the
    // granted projects in them), newest first.
    if !reach.sources.is_empty() || !reach.source_projects.is_empty() {
        let mut clauses: Vec<SourceClause> = reach
            .sources
            .iter()
            .map(|scope| SourceClause::Whole(scope.clone()))
            .collect();
        for (scope, projects) in &reach.source_projects {
            clauses.extend(
                projects
                    .iter()
                    .map(|project| SourceClause::Project(scope.clone(), project.clone())),
            );
        }
        let cost = |clause: &SourceClause| match clause {
            SourceClause::Whole(_) => 1,
            SourceClause::Project(..) => 2,
        };
        let mut seen = std::collections::BTreeSet::new();
        let mut sources = Vec::new();
        for group in pack(clauses, cost) {
            let mut values = vec![text(workspace)];
            let mut parts = Vec::new();
            let wholes: Vec<&String> = group
                .iter()
                .filter_map(|clause| match clause {
                    SourceClause::Whole(scope) => Some(scope),
                    SourceClause::Project(..) => None,
                })
                .collect();
            if !wholes.is_empty() {
                parts.push(format!("scope IN ({})", placeholders(wholes.len())));
                values.extend(wholes.iter().map(|scope| text(scope)));
            }
            // Projects grouped by scope, so each scope's clause is one IN list.
            let mut by_scope: Vec<(&String, Vec<&String>)> = Vec::new();
            for clause in &group {
                if let SourceClause::Project(scope, project) = clause {
                    match by_scope.iter_mut().find(|(s, _)| *s == scope) {
                        Some((_, projects)) => projects.push(project),
                        None => by_scope.push((scope, vec![project])),
                    }
                }
            }
            for (scope, projects) in &by_scope {
                parts.push(format!(
                    "(scope = ? AND project IN ({}))",
                    placeholders(projects.len())
                ));
                values.push(text(scope));
                values.extend(projects.iter().map(|project| text(project)));
            }
            values.push(limit(cap));
            let rows = db
                .query(&Statement::with_values(
                    format!(
                        "SELECT id, scope, kind, rel_path, project, created_at FROM sources \
                         WHERE workspace = ? AND ({}) ORDER BY id DESC LIMIT ?",
                        parts.join(" OR ")
                    ),
                    values,
                ))
                .await?;
            for row in &rows.rows {
                let source = GraphSource {
                    id: row.get::<String>("id").unwrap_or_default(),
                    scope: row.get::<String>("scope").unwrap_or_default(),
                    kind: row.get::<String>("kind").unwrap_or_default(),
                    rel_path: row.get::<String>("rel_path").unwrap_or_default(),
                    project: row.get::<String>("project"),
                    created_at: row.get::<String>("created_at").unwrap_or_default(),
                };
                if seen.insert(source.id.clone()) {
                    sources.push(source);
                }
            }
        }
        sources.sort_by(|a, b| b.id.cmp(&a.id));
        if sources.len() > cap {
            sources.truncate(cap);
            out.truncated = true;
        }
        out.sources = sources;
    }

    // Fact subjects, counted; the values stay sealed. The whole scopes in
    // one query; each partly opened scope on its own, every subject checked
    // before it is kept.
    let mut fact_reads: Vec<(Vec<String>, bool)> = Vec::new();
    if !reach.facts.is_empty() {
        fact_reads.push((reach.facts.clone(), false));
    }
    for scope in &reach.fact_subjects {
        fact_reads.push((vec![scope.clone()], true));
    }
    for (scopes, checked) in fact_reads {
        // A checked scope reads past the cap: the rows the check drops must
        // not crowd out the ones it keeps.
        let read_cap = if checked { cap.saturating_mul(4) } else { cap };
        let mut found: Vec<(String, String, i64, Option<String>)> = Vec::new();
        for group in pack(scopes, |_| 1) {
            let mut values: Vec<SeaValue> = group.iter().map(|scope| text(scope)).collect();
            values.push(text("retracted"));
            values.push(limit(read_cap));
            let rows = db
                .query(&Statement::with_values(
                    format!(
                        "SELECT scope, subject, COUNT(*) AS n, MIN(observed_at) AS first_at \
                         FROM facts WHERE scope IN ({}) AND status <> ? \
                         GROUP BY scope, subject ORDER BY scope, subject LIMIT ?",
                        placeholders(group.len())
                    ),
                    values,
                ))
                .await?;
            for row in &rows.rows {
                found.push((
                    row.get::<String>("scope").unwrap_or_default(),
                    row.get::<String>("subject").unwrap_or_default(),
                    row.get::<i64>("n").unwrap_or_default(),
                    row.get::<String>("first_at"),
                ));
            }
        }
        found.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        found.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
        if found.len() > read_cap {
            out.truncated = true;
        }
        for (scope, subject, n, first_at) in found.into_iter().take(read_cap) {
            if checked && !fact_allowed(&scope, &subject) {
                continue;
            }
            out.subjects.push(GraphSubject {
                scope,
                subject,
                facts: u64::try_from(n).unwrap_or_default(),
                first_at,
            });
        }
    }
    if out.subjects.len() > cap {
        out.subjects.truncate(cap);
        out.truncated = true;
    }
    Ok(out)
}
