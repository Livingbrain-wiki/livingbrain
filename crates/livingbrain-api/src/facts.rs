//! The `facts` module: temporal facts over HTTP (`docs/temporal-facts.md`).
//!
//! - `POST /v1/facts` — record facts (format v1), in the asker's personal
//!   memory or, with `shared: true`, the workspace's shared one.
//! - `GET /v1/facts?subject=&attribute=&as_of=` — the timelines the asker
//!   may read, with what was valid at `as_of` (default now).
//! - `POST /v1/facts/{id}/retract` — mark one fact as never true.
//!
//! Every route answers to a bearer or the web app's session cookie
//! ([`Service::asker`]), and a cookie write must pass the same-origin check.
//! Every read names only the scopes the credential grants; a write's target
//! scope must be one of them.
//!
//! The rest of this file is what `notes`, `search`, `ask`, `pages` and the
//! MCP tools share: turning a request into [`FactInput`]s, choosing the
//! timelines a question is about, and rendering them for a model.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use cratefield_core::axum::Json;
use cratefield_core::axum::extract::{Path, Query, State};
use cratefield_core::axum::http::{HeaderMap, Method, StatusCode};
use cratefield_core::axum::response::IntoResponse;
use cratefield_core::axum::routing::{get, post};
use cratefield_core::{Config, ConfigError, Migrations, Module, ModuleContext, Port, Problem};
use livingbrain_access::{Scope, UserId};
use livingbrain_mcp::{Asker, page_scope};
use livingbrain_memory::timeline::{self, PartialDate, TimedFact, normalize_attribute};
use livingbrain_pages::{FactInput, FactStore, Recorded, Timeline, fact_json, person_subject};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Wiki;
use crate::problems::{FACT_NOT_FOUND, FACT_REFUSED, SCOPE_NOT_GRANTED, fact_error};
use crate::service::Service;

/// The most facts one request may carry.
pub(crate) const MAX_FACTS_PER_REQUEST: usize = 50;
/// The most timelines a question pulls into a brief.
pub(crate) const MAX_BRIEF_TIMELINES: usize = 20;

/// The `facts` module, constructed with the page access the venture
/// injected ([`Wiki`]): the key custodian facts are sealed with and the
/// credential resolvers.
pub struct Facts {
    wiki: Wiki,
}

impl Facts {
    /// A facts module over the given page access.
    #[must_use]
    pub fn new(wiki: Wiki) -> Self {
        Self { wiki }
    }
}

impl Module for Facts {
    fn name(&self) -> &'static str {
        "facts"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// The same ports as its siblings: a fact needs no blob, but the
    /// composition treats the CLI modules as one family (the blob view is
    /// planted for every one of them), and declaring it costs nothing.
    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Blob, Port::Clock, Port::IdGen]
    }

    /// `Signer`, optionally, for the credential resolver — see `notes`.
    fn optional(&self) -> &'static [Port] {
        &[Port::Signer]
    }

    /// The `facts` table is the pages module's (migration `0006_facts`).
    fn migrations(&self) -> Migrations {
        Migrations::EMPTY
    }

    fn validate_config(&self, _cfg: &dyn Config) -> Result<(), ConfigError> {
        Ok(())
    }

    fn router(&self, ctx: ModuleContext) -> cratefield_core::axum::Router {
        let state = Arc::new(Service::from_wiki(ctx, &self.wiki));
        cratefield_core::axum::Router::new()
            .route("/", get(read).post(write))
            .route("/{id}/retract", post(retract))
            .with_state(state)
    }
}

/// One fact as a request carries it (format v1). Unknown fields are
/// refused: an emitter that sends `status` or `supersedes` is resolving
/// conflicts itself, which the format forbids.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FactRequest {
    subject: String,
    attribute: String,
    /// A string; a number or a boolean is accepted and kept as its text,
    /// because a model will send `41` as often as `"41"`.
    value: Value,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    valid_from: Option<String>,
    #[serde(default)]
    valid_to: Option<String>,
    #[serde(default)]
    observed_at: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    quote: Option<String>,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    cardinality: Option<String>,
}

/// A `POST /v1/facts` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteRequest {
    #[serde(default)]
    format: Option<u32>,
    facts: Vec<FactRequest>,
    #[serde(default)]
    shared: bool,
}

/// The page scope a write lands in: the asker's personal memory, or the
/// workspace's shared one — and only if the credential grants it, so a
/// token narrowed to one scope cannot write to another.
pub(crate) fn write_scope(state: &Service, asker: &Asker, shared: bool) -> Result<String, Problem> {
    let scope = if shared {
        page_scope(&asker.workspace_id, &Scope::Shared)
    } else {
        page_scope(
            &asker.workspace_id,
            &Scope::User(UserId::new(asker.user_id.clone())),
        )
    };
    if state.scopes(asker).contains(&scope) {
        Ok(scope)
    } else {
        Err(Problem::new(&SCOPE_NOT_GRANTED))
    }
}

/// `me` → the asker's own person subject; anything else as sent.
pub(crate) fn resolve_subject(asker: &Asker, subject: &str) -> Result<String, Problem> {
    if subject.trim() == "me" {
        person_subject(&asker.user_id).ok_or_else(|| {
            Problem::new(&FACT_REFUSED).with_detail("`me` does not name a person here")
        })
    } else {
        Ok(subject.trim().to_owned())
    }
}

/// A request's facts as store inputs, `me` resolved and `source` defaulted.
pub(crate) fn inputs(
    asker: &Asker,
    facts: Vec<FactRequest>,
    default_source: Option<&str>,
) -> Result<Vec<FactInput>, Problem> {
    if facts.len() > MAX_FACTS_PER_REQUEST {
        return Err(Problem::new(&FACT_REFUSED)
            .with_detail(format!("at most {MAX_FACTS_PER_REQUEST} facts per request")));
    }
    facts
        .into_iter()
        .map(|fact| {
            let value = match fact.value {
                Value::String(text) => text,
                Value::Number(number) => number.to_string(),
                Value::Bool(flag) => flag.to_string(),
                _ => {
                    return Err(Problem::new(&FACT_REFUSED)
                        .with_detail("`value` is a string, a number or a boolean"));
                }
            };
            Ok(FactInput {
                subject: resolve_subject(asker, &fact.subject)?,
                attribute: fact.attribute,
                value,
                unit: fact.unit,
                valid_from: fact.valid_from,
                valid_to: fact.valid_to,
                observed_at: fact.observed_at,
                source: fact.source.or_else(|| default_source.map(str::to_owned)),
                quote: fact.quote,
                confidence: fact.confidence,
                cardinality: fact.cardinality,
            })
        })
        .collect()
}

/// Records every input in `scope`, in order, and answers each as
/// `{id, outcome, scope, fact}`. The first refusal stops the batch: what
/// was recorded before it stays (each fact is its own write).
pub(crate) async fn record_all(
    store: &FactStore,
    scope: &str,
    inputs: Vec<FactInput>,
    written_by: &str,
) -> Result<Vec<Value>, Problem> {
    let mut out = Vec::with_capacity(inputs.len());
    for input in inputs {
        let Recorded {
            outcome,
            scope,
            fact,
        } = store
            .record(scope, input, written_by)
            .await
            .map_err(fact_error)?;
        out.push(json!({
            "id": fact.id,
            "outcome": outcome.as_str(),
            "scope": scope_label(&scope),
            "fact": fact_json(&fact),
        }));
    }
    Ok(out)
}

/// `personal`, `channel` or `shared`, by the prefix a page scope keeps.
pub(crate) fn scope_label(scope: &str) -> &'static str {
    if scope.starts_with("user-") {
        "personal"
    } else if scope.starts_with("channel-") {
        "channel"
    } else {
        "shared"
    }
}

/// `POST /` — record facts.
///
/// # Errors
///
/// 401 without a credential, 403 for a cross-site cookie write or a scope
/// the credential does not grant, 422 for a fact outside the format.
async fn write(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
    Json(request): Json<WriteRequest>,
) -> Result<impl IntoResponse, Problem> {
    let asker = state.asker(&method, &headers).await?;
    if request.format.is_some_and(|format| format != 1) {
        return Err(Problem::new(&FACT_REFUSED).with_detail("this server speaks `format: 1`"));
    }
    if request.facts.is_empty() {
        return Err(Problem::new(&FACT_REFUSED).with_detail("`facts` is empty"));
    }
    let scope = write_scope(&state, &asker, request.shared)?;
    let inputs = inputs(&asker, request.facts, None)?;
    let store = state.facts_store()?;
    let recorded = record_all(&store, &scope, inputs, &asker.user_id).await?;
    Ok((StatusCode::CREATED, Json(json!({ "facts": recorded }))))
}

/// `POST /{id}/retract` — mark one fact retracted.
///
/// # Errors
///
/// 401, 403 as for a write; 404 for an id no readable scope holds.
async fn retract(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&method, &headers).await?;
    let scopes = state.scopes(&asker);
    let names: Vec<&str> = scopes.iter().map(String::as_str).collect();
    let store = state.facts_store()?;
    let (scope, fact) = store
        .retract(&names, &id)
        .await
        .map_err(fact_error)?
        .ok_or_else(|| Problem::new(&FACT_NOT_FOUND))?;
    Ok(Json(json!({
        "id": fact.id,
        "scope": scope_label(&scope),
        "fact": fact_json(&fact),
    })))
}

/// `GET /?subject=&attribute=&as_of=` — the asker's timelines.
///
/// # Errors
///
/// 401 without a credential, 422 for an unparseable `as_of`, `subject` or
/// `attribute`.
async fn read(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&method, &headers).await?;
    let subject = match query
        .get("subject")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        Some(raw) => Some(resolve_subject(&asker, raw)?),
        None => None,
    };
    let attribute = match query
        .get("attribute")
        .map(|a| a.trim())
        .filter(|a| !a.is_empty())
    {
        Some(raw) => Some(normalize_attribute(raw).ok_or_else(|| {
            Problem::new(&FACT_REFUSED).with_detail("`attribute` is not an attribute key")
        })?),
        None => None,
    };
    let store = state.facts_store()?;
    let (as_of_text, at) = as_of(query.get("as_of").map(String::as_str), store.now())?;
    let scopes = state.scopes(&asker);
    let names: Vec<&str> = scopes.iter().map(String::as_str).collect();
    let timelines = store
        .timelines(&names, subject.as_deref(), attribute.as_deref())
        .await
        .map_err(fact_error)?;
    let keys: Vec<Value> = timelines
        .iter()
        .map(|timeline| timeline_json(timeline, at, &|fact| fact.source.clone()))
        .collect();
    Ok(Json(json!({
        "as_of": as_of_text,
        "as_of_instant": timeline::rfc3339(at),
        "keys": keys,
    })))
}

/// An `as_of` parameter as `(as written, instant)`: the last second of the
/// period it names, never after now. Absent is now.
///
/// # Errors
///
/// 422 for a date that does not parse.
pub(crate) fn as_of(raw: Option<&str>, now: i64) -> Result<(String, i64), Problem> {
    match raw
        .map(str::trim)
        .filter(|raw| !raw.is_empty() && *raw != "now")
    {
        None => Ok(("now".to_owned(), now)),
        Some(raw) => {
            let date = PartialDate::parse(raw)
                .map_err(|error| Problem::new(&FACT_REFUSED).with_detail(error.to_string()))?;
            Ok((date.text().to_owned(), date.as_of_instant().min(now)))
        }
    }
}

/// One timeline as the read routes answer it: what was valid at `at`
/// (`current`), every fact newest first (`history`), and the one line a
/// model reads.
pub(crate) fn timeline_json(
    timeline: &Timeline,
    at: i64,
    cite: &dyn Fn(&TimedFact) -> Option<String>,
) -> Value {
    let current: Vec<Value> = timeline::valid_at(&timeline.facts, at)
        .into_iter()
        .map(fact_json)
        .collect();
    let history: Vec<Value> = timeline::history(&timeline.facts)
        .into_iter()
        .map(fact_json)
        .collect();
    json!({
        "subject": timeline.subject,
        "attribute": timeline.attribute,
        "scope": scope_label(&timeline.scope),
        "cardinality": timeline.cardinality.as_str(),
        "current": current,
        "history": history,
        "line": timeline::render_line(&timeline.facts, at, cite),
    })
}

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
/// The store's mapped problem.
pub(crate) async fn relevant(
    store: &FactStore,
    asker: &Asker,
    scopes: &[String],
    question: &str,
) -> Result<Vec<Timeline>, Problem> {
    let asked = words(question);
    if asked.is_empty() && !about_self(question) {
        return Ok(Vec::new());
    }
    let me = person_subject(&asker.user_id);
    let me_too = about_self(question);
    let names: Vec<&str> = scopes.iter().map(String::as_str).collect();
    let index = store.index(&names).await.map_err(fact_error)?;
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
        out.push(
            store
                .timeline(&scope, &subject, &attribute)
                .await
                .map_err(fact_error)?,
        );
    }
    Ok(out)
}

/// Fact lines for a brief, each with the index of the timeline it renders
/// (a timeline with nothing but retractions renders none), each fact's
/// source cited as `F1`, `F2`, … in first-seen order, and the citations
/// those labels name.
pub(crate) fn brief_lines(timelines: &[Timeline], at: i64) -> (Vec<(usize, String)>, Vec<Value>) {
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
pub(crate) fn brief_section(lines: &[(usize, String)], at: i64) -> String {
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
