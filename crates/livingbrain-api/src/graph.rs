//! The `graph` module: `GET /v1/graph`, the brain map's data — the asker's
//! pages, sources, projects and fact subjects as nodes, and the links
//! between them as edges.
//!
//! **Who sees what.** Exactly what the listings show this asker, through the
//! grants plan (ADR 0004): pages and links through its page filters, the
//! source ledger and the compile queue through its sources reach (workspace
//! *and* scope, or the granted projects), fact subjects through its facts
//! filters — all in the query, before the cap. The credential decides them,
//! never the client; a narrowed token narrows the map. Nothing is unsealed:
//! the map is built from slugs, kinds, projects, dates, links and counts
//! ([`livingbrain_pages::graph_rows_in`]).
//!
//! **Filters before the limit.** `project` and `kind` narrow the node set
//! first; then the nodes are ranked (most connected, then newest) and the
//! first `limit` kept, and only edges between kept nodes are returned. So a
//! filtered map is the best `limit` of *that* project or kind, however many
//! other nodes exist. `limit` defaults to [`DEFAULT_LIMIT`] and is clamped to
//! [`MAX_LIMIT`]; `counts` says how many there were before it.
//!
//! **Node kinds.** A page's entity type (`person`, `project`, `decision`,
//! `customer`, `system`, `glossary`, `radar`, `topic`), or `coding-agents`
//! for the compiled `coding-agents--…` pages; `session` for an agent session,
//! `github` for a GitHub source, `source` for any other source (an import, a
//! note, a chat); `project` for a project the sources or compiled pages are
//! filed under; `fact` for a fact subject with no page of its own.
//!
//! **Edge kinds.** `link` (a page's `[[link]]`), `cites` (a compiled page →
//! the session it was compiled from), `member` (a project → its pages and
//! sessions) and `about` (a fact subject's facts → the page or project they
//! describe, folded into that node's `facts` count rather than drawn).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use cratefield_core::axum::Json;
use cratefield_core::axum::extract::{Query, State};
use cratefield_core::axum::http::{HeaderMap, Method};
use cratefield_core::axum::routing::get;
use cratefield_core::{Config, ConfigError, Migrations, Module, ModuleContext, Port, Problem};
use cratefield_module_orgs::grants::Permission;
use livingbrain_compiler::layout::segment;
use livingbrain_compiler::{JobStatus, Queue};
use livingbrain_pages::access::{KIND_FACTS, KIND_PAGES, ScopeFilter, subject_path};
use livingbrain_pages::{
    ActivityRow, GraphReach, GraphRows, activity_rows, graph_rows_in, is_project,
};
use serde_json::{Value, json};

use crate::Wiki;
use crate::facts::scope_label;
use crate::port;
use crate::service::Service;
use crate::sources::SourceReach;

/// How many nodes the map answers with when the caller names no limit.
pub const DEFAULT_LIMIT: usize = 800;
/// The ceiling on a caller-supplied node count: above it is clamped.
pub const MAX_LIMIT: usize = 2000;
/// How many rows of each table one request reads at most. Above
/// [`MAX_LIMIT`], so a filter still has room to find its nodes.
pub const SCAN_CAP: usize = 10_000;

/// The node kinds a `kind` filter may name.
pub const KINDS: [&str; 13] = [
    "person",
    "project",
    "decision",
    "customer",
    "system",
    "glossary",
    "radar",
    "topic",
    "coding-agents",
    "session",
    "github",
    "source",
    "fact",
];

/// The compiled coding-agents pages' slug prefix (`livingbrain-compiler`'s
/// layout): `coding-agents--<agent>--<project>--<page>`.
const COMPILED: &str = "coding-agents--";

/// The `graph` module, constructed with the page access the venture
/// injected.
pub struct Graph {
    wiki: Wiki,
}

impl Graph {
    /// A graph module over the given page access. See [`Wiki`] for where
    /// each field comes from.
    #[must_use]
    pub fn new(wiki: Wiki) -> Self {
        Self { wiki }
    }
}

impl Module for Graph {
    fn name(&self) -> &'static str {
        "graph"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// The database is the whole of what the map reads; the blob is
    /// declared like the siblings' so the composition's port layer is the
    /// same for every CLI module, though no body is ever opened here.
    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Blob, Port::Clock, Port::IdGen]
    }

    /// `Signer` for the credential resolver, as every sibling declares it.
    fn optional(&self) -> &'static [Port] {
        &[Port::Signer]
    }

    fn migrations(&self) -> Migrations {
        Migrations::EMPTY
    }

    fn validate_config(&self, _cfg: &dyn Config) -> Result<(), ConfigError> {
        Ok(())
    }

    fn router(&self, ctx: ModuleContext) -> cratefield_core::axum::Router {
        let state = Arc::new(Service::from_wiki(ctx, &self.wiki));
        cratefield_core::axum::Router::new()
            .route("/", get(graph))
            .route("/activity", get(activity))
            .with_state(state)
    }
}

/// The parsed query: what to keep, and how much of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphQuery {
    /// Only this project's nodes (and the project itself).
    pub project: Option<String>,
    /// Only these node kinds.
    pub kinds: Option<BTreeSet<String>>,
    /// How many nodes at most.
    pub limit: usize,
}

/// `GET /?project=&kind=&limit=` — the map, as `{nodes, edges, counts,
/// truncated}`.
///
/// # Errors
///
/// 401 without a credential, 400 (`validation-failed`) for a parameter
/// outside its rule, and [`Problem::internal`] when the database fails.
async fn graph(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&method, &headers).await?;
    let query = parse_query(&query)?;
    // The grants decide every table the map reads (ADR 0004): pages and
    // links through the page filters, sources through the sources reach,
    // fact subjects through the facts filters — each in the query, before
    // the cap, so nothing ungranted is drawn, linked or counted.
    let plan = state.access(&asker).await?;
    let sources = SourceReach::of(&plan);
    let (facts, fact_subjects): (Vec<ScopeFilter>, Vec<ScopeFilter>) = plan
        .filters(KIND_FACTS, Permission::Read)
        .into_iter()
        .partition(ScopeFilter::is_whole);
    let reach = GraphReach {
        pages: plan.filters(KIND_PAGES, Permission::Read),
        sources: sources.whole.clone(),
        source_projects: sources.by_project,
        facts: facts.into_iter().map(|filter| filter.scope).collect(),
        fact_subjects: fact_subjects
            .into_iter()
            .map(|filter| filter.scope)
            .collect(),
    };
    let fact_allowed = |scope: &str, subject: &str| {
        plan.allows(KIND_FACTS, scope, &subject_path(subject), Permission::Read)
    };
    let db = port(state.ctx().ports.db.clone())?;
    let rows = graph_rows_in(
        db.as_ref(),
        &asker.workspace_id,
        &reach,
        &fact_allowed,
        SCAN_CAP,
    )
    .await
    .map_err(|_| Problem::internal())?;
    // The compile queue says which session each compiled page came from. A
    // composition without the compiler has no queue: the map just has no
    // `cites` edges. Only jobs of a source scope read whole, and an edge is
    // drawn only between a page and a session already on the map.
    let compiled: Vec<Compiled> = Queue::new(db)
        .list(&asker.workspace_id, &sources.whole, None, SCAN_CAP)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|job| job.status == JobStatus::Done)
        .map(|job| Compiled {
            scope: job.scope,
            source_id: job.source_id,
            pages: job.pages,
        })
        .collect();
    Ok(Json(build(&rows, &compiled, &query, &state.base())))
}

/// The most events one live-feed answer carries; the newest are kept.
pub const ACTIVITY_CAP: usize = 100;

/// `GET /activity?since=` — the live feed: the pages the asker may read, that
/// were read, searched or written, as `{cursor, events}`. Pass the answer's
/// `cursor` back as `since` to get only what came after it.
///
/// **Who sees what.** The same page grants as the map: a row is answered only
/// when the asker's page filters read its scope and slug. A row the asker may
/// not read is dropped here and its existence is not said; the cursor still
/// moves past it, so a filtered feed never re-reads what it skipped.
///
/// # Errors
///
/// 401 without a credential, 400 (`validation-failed`) for a `since` that is
/// not a cursor, and [`Problem::internal`] when the database fails.
async fn activity(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&method, &headers).await?;
    let since = parse_since(&query)?;
    let plan = state.access(&asker).await?;
    let filters = plan.filters(KIND_PAGES, Permission::Read);
    let db = port(state.ctx().ports.db.clone())?;
    let rows = activity_rows(db.as_ref(), &asker.workspace_id, since, SCAN_CAP)
        .await
        .map_err(|_| Problem::internal())?;
    Ok(Json(activity_view(&rows, &filters, since)))
}

/// Reads `since`, the cursor of an earlier live-feed answer.
///
/// # Errors
///
/// A `validation-failed` problem for a value that is not a cursor.
pub fn parse_since(query: &HashMap<String, String>) -> Result<Option<i64>, Problem> {
    match query.get("since").map(|raw| raw.trim()) {
        None | Some("") => Ok(None),
        Some(raw) => raw
            .parse::<i64>()
            .ok()
            .filter(|cursor| *cursor >= 0)
            .map(Some)
            .ok_or_else(|| {
                Problem::validation_failed(format!(
                    "`since` must be a cursor from an earlier answer, got {raw:?}"
                ))
            }),
    }
}

/// The feed's answer from the rows (oldest first): the rows the filters read,
/// the newest [`ACTIVITY_CAP`] of them, and the cursor past every row seen.
#[must_use]
pub fn activity_view(rows: &[ActivityRow], filters: &[ScopeFilter], since: Option<i64>) -> Value {
    let cursor = rows.iter().map(|row| row.id).max().or(since).unwrap_or(0);
    let visible: Vec<Value> = rows
        .iter()
        .filter(|row| {
            filters
                .iter()
                .any(|filter| filter.scope == row.scope && filter.matches(&row.slug))
        })
        .filter_map(|row| {
            let kind = match row.kind.as_str() {
                "read" | "search" | "write" => row.kind.as_str(),
                _ => return None,
            };
            Some(json!({
                "at": row.at,
                "kind": kind,
                "node": format!("page:{}:{}", row.scope, row.slug),
                "actor": row.actor,
                "label": activity_label(kind, &row.actor),
            }))
        })
        .collect();
    let newest = visible.len().saturating_sub(ACTIVITY_CAP);
    json!({
        "cursor": cursor.to_string(),
        "events": visible[newest..],
    })
}

/// A feed line in plain English.
fn activity_label(kind: &str, actor: &str) -> String {
    match kind {
        "read" => format!("read by {actor}"),
        "search" => format!("found by a search from {actor}"),
        _ => format!("written by {actor}"),
    }
}

/// Reads `project`, `kind` (comma-separated) and `limit`.
///
/// # Errors
///
/// A `validation-failed` problem for a value outside its rule.
pub fn parse_query(query: &HashMap<String, String>) -> Result<GraphQuery, Problem> {
    let project = match query.get("project").map(|raw| raw.trim()) {
        None | Some("") => None,
        Some(raw) if is_project(raw) => Some(raw.to_owned()),
        Some(raw) => {
            return Err(Problem::validation_failed(format!(
                "`project` must be 1..=64 bytes of ascii letters, digits, `.`, `_` and `-`, got {raw:?}"
            )));
        }
    };
    let kinds = match query.get("kind").map(|raw| raw.trim()) {
        None | Some("") => None,
        Some(raw) => {
            let mut set = BTreeSet::new();
            for kind in raw.split(',').map(str::trim).filter(|k| !k.is_empty()) {
                if !KINDS.contains(&kind) {
                    return Err(Problem::validation_failed(format!(
                        "`kind` must be one of {}, got {kind:?}",
                        KINDS.join(", ")
                    )));
                }
                set.insert(kind.to_owned());
            }
            (!set.is_empty()).then_some(set)
        }
    };
    let limit = match query.get("limit") {
        None => DEFAULT_LIMIT,
        Some(raw) => (raw.trim().parse::<u32>().map_err(|_| {
            Problem::validation_failed(format!("`limit` must be a positive integer, got {raw:?}"))
        })? as usize)
            .clamp(1, MAX_LIMIT),
    };
    Ok(GraphQuery {
        project,
        kinds,
        limit,
    })
}

/// What one finished compile wrote: the pages (in `scope`) compiled from
/// the session `source_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compiled {
    pub scope: String,
    pub source_id: String,
    pub pages: Vec<String>,
}

/// One node while the map is assembled.
#[derive(Debug, Clone, Default)]
struct Node {
    id: String,
    kind: String,
    label: String,
    slug: Option<String>,
    scope: Option<&'static str>,
    project: Option<String>,
    agent: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
    backlinks: u64,
    facts: u64,
    degree: u64,
    url: Option<String>,
}

/// The compiled page's folders: `(agent, project segment)` of a
/// `coding-agents--<agent>--<project>--<page>` slug, the rollup
/// `coding-agents--projects--<project>` as `(None, project)`.
fn compiled_place(slug: &str) -> Option<(Option<String>, Option<String>)> {
    let rest = slug.strip_prefix(COMPILED)?;
    let parts: Vec<&str> = rest.split("--").collect();
    match parts.as_slice() {
        ["projects", project] => Some((None, Some((*project).to_owned()))),
        [agent, project, _page] => Some((Some((*agent).to_owned()), Some((*project).to_owned()))),
        [agent, _page] => Some((Some((*agent).to_owned()), None)),
        _ => Some((None, None)),
    }
}

/// The agent a session's path names (`claude_code session …`).
fn session_agent(path: &str) -> Option<String> {
    let (agent, rest) = path.split_once(' ')?;
    (rest.starts_with("session")
        && !agent.is_empty()
        && agent.len() <= 32
        && agent
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'))
    .then(|| agent.to_owned())
}

fn source_kind(kind: &str) -> &'static str {
    match kind {
        "agent_log" => "session",
        "github_commit" | "github_issue" => "github",
        _ => "source",
    }
}

/// Builds the answer from the rows: nodes, edges, then the filters, the
/// ranking and the limit. Pure, so the shaping is pinned without a database.
#[must_use]
pub fn build(rows: &GraphRows, compiled: &[Compiled], query: &GraphQuery, base: &str) -> Value {
    let mut nodes: BTreeMap<String, Node> = BTreeMap::new();
    let mut edges: BTreeSet<(String, String, &'static str)> = BTreeSet::new();
    // Which node a `(scope, slug)` is, and the first scope a slug is in.
    let mut page_at: HashMap<(String, String), String> = HashMap::new();
    let mut slug_first: HashMap<String, String> = HashMap::new();

    let project_id = |segment: &str| format!("project:{segment}");
    let ensure_project =
        |nodes: &mut BTreeMap<String, Node>, raw: &str, at: Option<&str>| -> Option<String> {
            let seg = segment(raw)?;
            let id = project_id(&seg);
            let node = nodes.entry(id.clone()).or_insert_with(|| Node {
                id: id.clone(),
                kind: "project".to_owned(),
                label: raw.to_owned(),
                project: Some(seg.clone()),
                ..Node::default()
            });
            // One node per project key, named by the spelling a person
            // knows: a compiled page's folder (`earthos-world`) gives way to
            // the repository's own name (`EarthOS.world`) from a source.
            node.label = livingbrain_pages::prefer_display(&node.label, raw).to_owned();
            // The earliest thing filed under a project is when it started.
            if let Some(at) = at
                && node.created_at.as_deref().is_none_or(|have| at < have)
            {
                node.created_at = Some(at.to_owned());
            }
            if let Some(at) = at
                && node.updated_at.as_deref().is_none_or(|have| at > have)
            {
                node.updated_at = Some(at.to_owned());
            }
            Some(id)
        };

    for page in &rows.pages {
        let id = format!("page:{}:{}", page.scope, page.slug);
        let compiled = compiled_place(&page.slug);
        let kind = if compiled.is_some() {
            "coding-agents".to_owned()
        } else {
            page.entity_type.clone()
        };
        let (agent, project) = compiled.clone().unwrap_or((None, None));
        page_at.insert((page.scope.clone(), page.slug.clone()), id.clone());
        slug_first
            .entry(page.slug.clone())
            .or_insert_with(|| id.clone());
        nodes.insert(
            id.clone(),
            Node {
                id: id.clone(),
                kind,
                label: page.slug.clone(),
                slug: Some(page.slug.clone()),
                scope: Some(scope_label(&page.scope)),
                project: project.clone(),
                agent,
                created_at: page
                    .created_at
                    .clone()
                    .or_else(|| Some(page.updated_at.clone())),
                updated_at: Some(page.updated_at.clone()),
                url: Some(crate::recipes::wiki_url(base, &page.scope, &page.slug)),
                ..Node::default()
            },
        );
        if let Some(project) = project {
            let at = page
                .created_at
                .clone()
                .unwrap_or_else(|| page.updated_at.clone());
            if let Some(pid) = ensure_project(&mut nodes, &project, Some(&at)) {
                edges.insert((pid, id.clone(), "member"));
            }
        }
    }

    // A `project` page joins the project node of the same name.
    let project_pages: Vec<(String, String)> = nodes
        .values()
        .filter(|node| node.kind == "project")
        .filter_map(|node| Some((node.id.clone(), node.slug.clone()?)))
        .collect();
    for (id, slug) in project_pages {
        let pid = project_id(&slug);
        if nodes.contains_key(&pid) {
            edges.insert((pid, id, "member"));
        }
    }

    // Links, inside a scope first, else to the first readable page of that
    // slug; a link to no readable page is not drawn.
    for link in &rows.links {
        let Some(from) = page_at.get(&(link.scope.clone(), link.from.clone())) else {
            continue;
        };
        let to = page_at
            .get(&(link.scope.clone(), link.to.clone()))
            .or_else(|| slug_first.get(&link.to));
        if let Some(to) = to
            && from != to
        {
            if let Some(node) = nodes.get_mut(to) {
                node.backlinks += 1;
            }
            edges.insert((from.clone(), to.clone(), "link"));
        }
    }

    let mut source_ids: HashMap<String, String> = HashMap::new();
    for source in &rows.sources {
        let id = format!("source:{}", source.id);
        let kind = source_kind(&source.kind);
        let agent = (kind == "session")
            .then(|| session_agent(&source.rel_path))
            .flatten();
        let project_seg = source.project.as_deref().and_then(segment);
        source_ids.insert(source.id.clone(), id.clone());
        nodes.insert(
            id.clone(),
            Node {
                id: id.clone(),
                kind: kind.to_owned(),
                label: if source.rel_path.is_empty() {
                    source.kind.clone()
                } else {
                    source.rel_path.clone()
                },
                scope: Some(scope_label(&source.scope)),
                project: project_seg,
                agent,
                created_at: Some(source.created_at.clone()),
                updated_at: Some(source.created_at.clone()),
                url: (kind == "session")
                    .then(|| format!("wiki.html?view=sessions&source={}", source.id)),
                ..Node::default()
            },
        );
        if let Some(project) = source.project.as_deref()
            && let Some(pid) = ensure_project(&mut nodes, project, Some(&source.created_at))
        {
            edges.insert((pid, id, "member"));
        }
    }

    // A compiled page cites the session it was compiled from.
    for job in compiled {
        let Some(session) = source_ids.get(&job.source_id) else {
            continue;
        };
        for slug in &job.pages {
            if let Some(page) = page_at.get(&(job.scope.clone(), slug.clone())) {
                edges.insert((page.clone(), session.clone(), "cites"));
            }
        }
    }

    // Fact subjects: counted onto the page or project they are about, or a
    // node of their own.
    for subject in &rows.subjects {
        let (kind, slug) = subject
            .subject
            .split_once('/')
            .unwrap_or(("", subject.subject.as_str()));
        let target = page_at
            .get(&(subject.scope.clone(), slug.to_owned()))
            .or_else(|| slug_first.get(slug))
            .cloned()
            .or_else(|| {
                (kind == "project")
                    .then(|| project_id(slug))
                    .filter(|pid| nodes.contains_key(pid))
            });
        if let Some(target) = target {
            if let Some(node) = nodes.get_mut(&target) {
                node.facts += subject.facts;
            }
            continue;
        }
        let id = format!("fact:{}", subject.subject);
        let node = nodes.entry(id.clone()).or_insert_with(|| Node {
            id: id.clone(),
            kind: "fact".to_owned(),
            label: subject.subject.clone(),
            scope: Some(scope_label(&subject.scope)),
            created_at: subject.first_at.clone(),
            updated_at: subject.first_at.clone(),
            ..Node::default()
        });
        node.facts += subject.facts;
    }

    for (from, to, _) in &edges {
        if let Some(node) = nodes.get_mut(from) {
            node.degree += 1;
        }
        if let Some(node) = nodes.get_mut(to) {
            node.degree += 1;
        }
    }

    let nodes_total = nodes.len();
    let edges_total = edges.len();

    // Filters first…
    let wanted_project = query.project.as_deref().and_then(segment);
    let mut kept: Vec<&Node> = nodes
        .values()
        .filter(|node| {
            wanted_project
                .as_deref()
                .is_none_or(|want| node.project.as_deref() == Some(want))
        })
        .filter(|node| {
            query
                .kinds
                .as_ref()
                .is_none_or(|kinds| kinds.contains(&node.kind))
        })
        .collect();
    let matched = kept.len();
    // …then the ranking and the limit.
    kept.sort_by(|a, b| {
        b.degree
            .cmp(&a.degree)
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| a.id.cmp(&b.id))
    });
    kept.truncate(query.limit);
    let ids: BTreeSet<&str> = kept.iter().map(|node| node.id.as_str()).collect();
    let kept_edges: Vec<Value> = edges
        .iter()
        .filter(|(from, to, _)| ids.contains(from.as_str()) && ids.contains(to.as_str()))
        .map(|(from, to, kind)| json!({ "source": from, "target": to, "kind": kind }))
        .collect();

    let mut by_kind: BTreeMap<&str, u64> = BTreeMap::new();
    for node in &kept {
        *by_kind.entry(node.kind.as_str()).or_default() += 1;
    }
    let mut projects: BTreeMap<String, String> = BTreeMap::new();
    for node in nodes.values().filter(|node| node.kind == "project") {
        if let Some(seg) = &node.project {
            projects.insert(seg.clone(), node.label.clone());
        }
    }
    let agents: BTreeSet<&str> = nodes
        .values()
        .filter_map(|node| node.agent.as_deref())
        .collect();

    let out_nodes: Vec<Value> = kept
        .iter()
        .map(|node| {
            json!({
                "id": node.id,
                "kind": node.kind,
                "label": node.label,
                "slug": node.slug,
                "scope": node.scope,
                "project": node.project,
                "agent": node.agent,
                "created_at": node.created_at,
                "updated_at": node.updated_at,
                "backlinks": node.backlinks,
                "facts": node.facts,
                "degree": node.degree,
                "url": node.url,
            })
        })
        .collect();
    let truncated = matched > kept.len() || rows.truncated;
    json!({
        "nodes": out_nodes,
        "edges": kept_edges,
        "counts": {
            "nodes": kept.len(),
            "edges": kept_edges.len(),
            "matched": matched,
            "nodes_total": nodes_total,
            "edges_total": edges_total,
            "by_kind": by_kind,
        },
        "projects": projects.into_iter().map(|(id, name)| json!({ "id": id, "name": name })).collect::<Vec<_>>(),
        "agents": agents.into_iter().collect::<Vec<_>>(),
        "limit": query.limit,
        "truncated": truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use livingbrain_pages::{GraphLink, GraphPage, GraphSource, GraphSubject};

    fn page(scope: &str, slug: &str, kind: &str, at: &str) -> GraphPage {
        GraphPage {
            scope: scope.to_owned(),
            slug: slug.to_owned(),
            entity_type: kind.to_owned(),
            version: 1,
            updated_at: at.to_owned(),
            created_at: Some(at.to_owned()),
        }
    }

    fn rows() -> GraphRows {
        GraphRows {
            pages: vec![
                page("shared-x", "kestrel", "system", "2026-10-02T00:00:00Z"),
                page("shared-x", "ada", "person", "2026-10-01T00:00:00Z"),
                page(
                    "shared-x",
                    "coding-agents--claude--demo--decisions",
                    "topic",
                    "2026-10-03T00:00:00Z",
                ),
                page("shared-x", "demo", "project", "2026-09-01T00:00:00Z"),
            ],
            links: vec![
                GraphLink {
                    scope: "shared-x".into(),
                    from: "kestrel".into(),
                    to: "ada".into(),
                },
                GraphLink {
                    scope: "shared-x".into(),
                    from: "kestrel".into(),
                    to: "nowhere".into(),
                },
            ],
            sources: vec![GraphSource {
                id: "01S".into(),
                scope: "shared-x".into(),
                kind: "agent_log".into(),
                rel_path: "claude_code session abc".into(),
                project: Some("Demo".into()),
                created_at: "2026-10-03T00:00:00Z".into(),
            }],
            subjects: vec![
                GraphSubject {
                    scope: "shared-x".into(),
                    subject: "person/ada".into(),
                    facts: 2,
                    first_at: None,
                },
                GraphSubject {
                    scope: "shared-x".into(),
                    subject: "system/ghost".into(),
                    facts: 1,
                    first_at: Some("2026-08-01T00:00:00Z".into()),
                },
            ],
            truncated: false,
        }
    }

    fn q(limit: usize) -> GraphQuery {
        GraphQuery {
            project: None,
            kinds: None,
            limit,
        }
    }

    /// One project node per key: the compiler's `earthos-world` folder, a
    /// session filed under `EarthOS.world` and a clone's `earthos-world` are
    /// one project, named the way the repository spells it, and filtering by
    /// any spelling finds all of it.
    #[test]
    fn a_project_is_one_node_named_by_its_original_spelling() {
        let mut rows = rows();
        rows.pages.push(page(
            "shared-x",
            "coding-agents--codex--earthos-world--learnings",
            "topic",
            "2026-10-04T00:00:00Z",
        ));
        for (id, project) in [("01E", "EarthOS.world"), ("01F", "earthos-world")] {
            rows.sources.push(GraphSource {
                id: id.into(),
                scope: "shared-x".into(),
                kind: "agent_log".into(),
                rel_path: format!("codex session {id}"),
                project: Some(project.into()),
                created_at: "2026-10-04T00:00:00Z".into(),
            });
        }
        let out = build(&rows, &[], &q(100), "https://app.test");
        let projects: Vec<&Value> = out["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["kind"] == "project" && n["project"] == "earthos-world")
            .collect();
        assert_eq!(projects.len(), 1, "{projects:?}");
        assert_eq!(projects[0]["label"], "EarthOS.world");
        let filtered = build(
            &rows,
            &[],
            &GraphQuery {
                project: Some("EarthOS.world".into()),
                kinds: None,
                limit: 100,
            },
            "https://app.test",
        );
        let ids: Vec<&str> = filtered["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_str().unwrap())
            .collect();
        for want in [
            "project:earthos-world",
            "source:01E",
            "source:01F",
            "page:shared-x:coding-agents--codex--earthos-world--learnings",
        ] {
            assert!(ids.contains(&want), "{want} in {ids:?}");
        }
    }

    #[test]
    fn nodes_and_edges_are_shaped_from_the_rows() {
        let out = build(&rows(), &[], &q(100), "https://app.test");
        let ids: Vec<&str> = out["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_str().unwrap())
            .collect();
        for want in [
            "page:shared-x:kestrel",
            "page:shared-x:ada",
            "page:shared-x:coding-agents--claude--demo--decisions",
            "project:demo",
            "source:01S",
            "fact:system/ghost",
        ] {
            assert!(ids.contains(&want), "{want} in {ids:?}");
        }
        let node = |id: &str| {
            out["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|n| n["id"] == id)
                .unwrap()
                .clone()
        };
        assert_eq!(
            node("page:shared-x:ada")["facts"],
            2,
            "facts fold onto their page"
        );
        assert_eq!(node("page:shared-x:ada")["backlinks"], 1);
        assert_eq!(
            node("page:shared-x:coding-agents--claude--demo--decisions")["kind"],
            "coding-agents"
        );
        assert_eq!(
            node("page:shared-x:coding-agents--claude--demo--decisions")["agent"],
            "claude"
        );
        assert_eq!(node("source:01S")["kind"], "session");
        assert_eq!(node("source:01S")["agent"], "claude_code");
        assert_eq!(node("source:01S")["project"], "demo");
        assert_eq!(
            node("project:demo")["label"],
            "Demo",
            "the source's own spelling wins over the compiler's folded folder name"
        );
        let edges: Vec<(String, String, String)> = out["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["source"].as_str().unwrap().into(),
                    e["target"].as_str().unwrap().into(),
                    e["kind"].as_str().unwrap().into(),
                )
            })
            .collect();
        assert!(edges.contains(&(
            "page:shared-x:kestrel".into(),
            "page:shared-x:ada".into(),
            "link".into()
        )));
        assert!(edges.contains(&("project:demo".into(), "source:01S".into(), "member".into())));
        assert!(edges.contains(&(
            "project:demo".into(),
            "page:shared-x:demo".into(),
            "member".into()
        )));
        assert!(
            !edges.iter().any(|(_, to, _)| to.contains("nowhere")),
            "dangling links are not drawn"
        );
    }

    #[test]
    fn a_compile_draws_cites_from_its_pages_to_its_session() {
        let compiled = Compiled {
            scope: "shared-x".into(),
            source_id: "01S".into(),
            pages: vec![
                "coding-agents--claude--demo--decisions".into(),
                "not-a-page".into(),
            ],
        };
        let out = build(&rows(), &[compiled], &q(100), "");
        let cites: Vec<&Value> = out["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["kind"] == "cites")
            .collect();
        assert_eq!(cites.len(), 1);
        assert_eq!(
            cites[0]["source"],
            "page:shared-x:coding-agents--claude--demo--decisions"
        );
        assert_eq!(cites[0]["target"], "source:01S");
        let out = build(&rows(), &[], &q(100), "");
        assert!(
            !out["edges"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["kind"] == "cites")
        );
    }

    #[test]
    fn filters_apply_before_the_limit() {
        // Only one node per kind filter, even with a limit of one: the
        // person is found although it is not the best-ranked node overall.
        let mut query = q(1);
        query.kinds = Some(BTreeSet::from(["person".to_owned()]));
        let out = build(&rows(), &[], &query, "");
        assert_eq!(out["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(out["nodes"][0]["id"], "page:shared-x:ada");
        assert_eq!(out["counts"]["matched"], 1);
        assert_eq!(out["truncated"], false);

        let mut query = q(10);
        query.project = Some("Demo".into());
        let out = build(&rows(), &[], &query, "");
        let ids: BTreeSet<&str> = out["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            BTreeSet::from([
                "project:demo",
                "source:01S",
                "page:shared-x:coding-agents--claude--demo--decisions"
            ])
        );

        let out = build(&rows(), &[], &q(2), "");
        assert_eq!(out["nodes"].as_array().unwrap().len(), 2);
        assert_eq!(out["truncated"], true);
        assert_eq!(out["counts"]["nodes_total"], 7);
        for edge in out["edges"].as_array().unwrap() {
            let ids: Vec<&str> = out["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n["id"].as_str().unwrap())
                .collect();
            assert!(
                ids.contains(&edge["source"].as_str().unwrap())
                    && ids.contains(&edge["target"].as_str().unwrap())
            );
        }
    }

    #[test]
    fn the_query_is_checked_and_the_limit_clamped() {
        let parse = |pairs: &[(&str, &str)]| {
            parse_query(
                &pairs
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            )
        };
        assert_eq!(parse(&[]).unwrap().limit, DEFAULT_LIMIT);
        assert_eq!(parse(&[("limit", "999999")]).unwrap().limit, MAX_LIMIT);
        assert_eq!(parse(&[("limit", "0")]).unwrap().limit, 1);
        assert!(parse(&[("limit", "lots")]).is_err());
        assert!(parse(&[("kind", "person,bogus")]).is_err());
        assert!(parse(&[("project", "../etc")]).is_err());
        let kinds = parse(&[("kind", "person, session")])
            .unwrap()
            .kinds
            .unwrap();
        assert_eq!(
            kinds,
            BTreeSet::from(["person".to_owned(), "session".to_owned()])
        );
    }
}

#[cfg(test)]
mod activity_tests {
    use super::*;

    fn row(id: i64, scope: &str, slug: &str, kind: &str) -> ActivityRow {
        ActivityRow {
            id,
            scope: scope.to_owned(),
            slug: slug.to_owned(),
            kind: kind.to_owned(),
            actor: "Claude".to_owned(),
            at: "2026-10-17T10:00:00Z".to_owned(),
        }
    }

    #[test]
    fn only_the_pages_the_filters_read_are_answered() {
        let rows = vec![
            row(1, "mine", "plan", "read"),
            row(2, "theirs", "private", "read"),
            row(3, "mine", "notes", "search"),
        ];
        let filters = [ScopeFilter::whole("mine", "personal:me")];
        let view = activity_view(&rows, &filters, None);
        let nodes: Vec<&str> = view["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["node"].as_str().unwrap())
            .collect();
        assert_eq!(nodes, ["page:mine:plan", "page:mine:notes"]);
        assert!(
            !view.to_string().contains("private"),
            "an unreadable slug is not said"
        );
        assert_eq!(
            view["cursor"], "3",
            "the cursor moves past the skipped row too"
        );
    }

    #[test]
    fn a_grant_on_another_scope_shows_its_events() {
        let rows = vec![row(7, "theirs", "private", "write")];
        assert_eq!(activity_view(&rows, &[], None)["events"], json!([]));
        let granted = [ScopeFilter::whole("theirs", "shared")];
        let view = activity_view(&rows, &granted, None);
        assert_eq!(view["events"][0]["label"], "written by Claude");
    }

    #[test]
    fn the_cursor_is_monotonic_and_empty_answers_keep_it() {
        let rows = vec![row(4, "mine", "a", "read"), row(9, "mine", "b", "read")];
        let filters = [ScopeFilter::whole("mine", "personal:me")];
        assert_eq!(activity_view(&rows, &filters, None)["cursor"], "9");
        assert_eq!(activity_view(&[], &filters, Some(12))["cursor"], "12");
        assert_eq!(activity_view(&[], &filters, None)["cursor"], "0");
    }

    #[test]
    fn the_feed_keeps_the_newest_hundred_in_order() {
        let rows: Vec<ActivityRow> = (1..=150)
            .map(|id| row(id, "mine", &format!("p{id}"), "read"))
            .collect();
        let filters = [ScopeFilter::whole("mine", "personal:me")];
        let events = activity_view(&rows, &filters, None)["events"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(events.len(), ACTIVITY_CAP);
        assert_eq!(events[0]["node"], "page:mine:p51");
        assert_eq!(events[99]["node"], "page:mine:p150");
    }

    #[test]
    fn since_must_be_a_cursor() {
        let mut query = HashMap::new();
        assert_eq!(parse_since(&query).unwrap(), None);
        query.insert("since".to_owned(), "42".to_owned());
        assert_eq!(parse_since(&query).unwrap(), Some(42));
        query.insert("since".to_owned(), "-1".to_owned());
        assert!(parse_since(&query).is_err());
        query.insert("since".to_owned(), "yesterday".to_owned());
        assert!(parse_since(&query).is_err());
    }
}
