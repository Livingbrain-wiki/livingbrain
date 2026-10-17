//! The seven tools, and the citations they answer with.
//!
//! Five `brain_*` tools, plus `search` and `fetch`: the same reads in the
//! shape ChatGPT's deep research expects, so a connector there answers from
//! these pages too. Every tool reads through the scopes the caller was
//! granted and none takes a scope argument. A page the asker was not granted
//! is not refused loudly — it is not found, because answering "you do not
//! have access to this" is itself an answer about somebody else's page.

use cratefield_module_orgs::grants::Permission;
use livingbrain_pages::access::{
    AccessPlan, KIND_FACTS, KIND_PAGES, ScopeFilter, personal, space_scope, subject_path,
};
use livingbrain_pages::{
    Author, EntityType, FactError, FactInput, FactStore, Frontmatter, Page, PageError, PageStore,
    PageWrite, Timeline, brief_lines, brief_section, fact_json, is_slug, page_url,
    parse_frontmatter, person_subject, reference_url, relevant_where,
};
use livingbrain_redact::{Policy, redact};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::auth::Asker;

/// How many hits `brain_search` returns when the caller names no limit.
const DEFAULT_LIMIT: usize = 10;
/// The ceiling on a caller-supplied `limit`.
const MAX_LIMIT: usize = 25;
/// The most facts one `brain_note` may state.
const MAX_NOTE_FACTS: usize = 50;
/// How many pages `brain_context_for` reads, after deduplication.
const CONTEXT_PAGES: usize = 5;
/// How much of a body `brain_search` quotes back.
const SNIPPET_CHARS: usize = 240;
/// The longest title a note's frontmatter may carry.
const MAX_TITLE_CHARS: usize = 120;
/// Hex characters of the note slug's SHA-256.
const SLUG_HEX: usize = 12;

/// The tools as `tools/list` returns them.
pub(crate) fn descriptors() -> Value {
    /// `read_only` says whether the tool changes nothing, as the `readOnlyHint`
    /// annotation every read-only tool carries (the one writer, `brain_note`,
    /// carries none).
    fn tool(
        name: &str,
        description: &str,
        properties: Value,
        required: &[&str],
        read_only: bool,
    ) -> Value {
        let mut tool = json!({
            "name": name,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "required": required,
            },
        });
        if read_only {
            tool["annotations"] = json!({ "readOnlyHint": true });
        }
        tool
    }
    json!([
        tool(
            "brain_search",
            "Search the pages you are allowed to read, best match first. A page matches \
             when it holds any of the query's words (stopwords like \"what\" or \"the\" \
             are ignored, plurals fold); pages holding more of the words, rarer ones, or \
             the words in their title rank higher. With no query (or an empty one), it \
             returns the most recently updated pages you can read instead.",
            json!({
                "query": { "type": "string",
                           "description": "The words or question to look for. Leave it out, or \
                                           send it empty, to list the most recently updated \
                                           pages you can read." },
                "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT,
                           "description": "How many pages to return." },
            }),
            &[],
            true,
        ),
        tool(
            "brain_page",
            "Read one page's Markdown by slug, from the pages you are allowed to read.",
            json!({ "slug": { "type": "string", "description": "The page's slug." } }),
            &["slug"],
            true,
        ),
        tool(
            "brain_context_for",
            "A cited brief for a task: the pages that mention it, with their full text. \
             Call this before you plan or write code in a repository.",
            json!({
                "repo": { "type": "string", "description": "The repository or component the work is in." },
                "task": { "type": "string", "description": "What you are about to do, in words." },
            }),
            &["repo", "task"],
            true,
        ),
        tool(
            "brain_note",
            "Record something worth remembering in your own private pages. Secrets and \
             personal data are redacted before it is stored.",
            json!({
                "text": { "type": "string", "description": "What to remember." },
                "title": { "type": "string", "description": "A short title for the note." },
                "facts": {
                    "type": "array",
                    "maxItems": MAX_NOTE_FACTS,
                    "description": "Temporal facts the note states (docs/temporal-facts.md, \
                        format v1): one per claim. The brain keeps history — a new value \
                        supersedes the old one, it never overwrites it. Do not set status \
                        or supersedes; date it only as precisely as the text does.",
                    "items": {
                        "type": "object",
                        "required": ["subject", "attribute", "value"],
                        "properties": {
                            "subject": { "type": "string", "description": "`<entity_type>/<slug>`, e.g. `person/nick`, or `me`." },
                            "attribute": { "type": "string", "description": "snake_case key, e.g. `shoe_size`." },
                            "value": { "type": "string" },
                            "unit": { "type": "string" },
                            "valid_from": { "type": "string", "description": "YYYY, YYYY-MM, YYYY-MM-DD or RFC 3339; when it became true." },
                            "valid_to": { "type": "string", "description": "When it stopped being true (exclusive)." },
                            "observed_at": { "type": "string", "description": "RFC 3339; when it was said." },
                            "quote": { "type": "string" },
                            "confidence": { "type": "number", "minimum": 0, "maximum": 1 },
                            "cardinality": { "enum": ["one", "many"] },
                        },
                    },
                },
            }),
            &["text"],
            false,
        ),
        tool(
            "brain_facts",
            "What is true about an entity, over time: the facts you may read, each with \
             when it was valid and when it was learned. Defaults to now; pass `as_of` \
             (YYYY, YYYY-MM or YYYY-MM-DD) to ask what was true then.",
            json!({
                "subject": { "type": "string", "description": "`<entity_type>/<slug>`, or `me`." },
                "attribute": { "type": "string", "description": "One attribute, e.g. `shoe_size`." },
                "as_of": { "type": "string", "description": "A partial date; the end of that period is used." },
            }),
            &[],
            true,
        ),
        tool(
            "search",
            "For ChatGPT deep research: search the pages the person you act for is \
             allowed to read — the same search `brain_search` runs, best match first — \
             answered as `id`/`title`/`url` rows. Hand an `id` to `fetch` for the page \
             behind a row.",
            json!({
                "query": { "type": "string",
                           "description": "The words or question to look for." },
            }),
            &["query"],
            true,
        ),
        tool(
            "fetch",
            "For ChatGPT deep research: one page's Markdown, by the `id` a `search` \
             result named (`<scope>:<slug>`) — the same read `brain_page` does, over \
             the pages the person you act for is allowed to read.",
            json!({
                "id": { "type": "string",
                        "description": "The page's id, as the `search` result named it." },
            }),
            &["id"],
            true,
        ),
    ])
}

/// One parsed tool call. Parsing is where a malformed argument is refused
/// (`-32602`); everything after it is an `isError` result, because the
/// failure is no longer the caller's syntax.
pub(crate) enum Call {
    Search {
        query: String,
        limit: usize,
    },
    /// `brain_search` with no query: the most recently updated pages.
    Recent {
        limit: usize,
    },
    Page {
        slug: String,
    },
    ContextFor {
        repo: String,
        task: String,
    },
    Note {
        text: String,
        title: Option<String>,
        facts: Vec<Value>,
    },
    Facts {
        subject: Option<String>,
        attribute: Option<String>,
        as_of: Option<String>,
    },
    /// `search`: [`Call::Search`]'s read in the shape ChatGPT's deep
    /// research reads.
    ChatSearch {
        query: String,
    },
    /// `fetch`: [`Call::Page`]'s read in the same shape, by the `scope:slug`
    /// id a `search` result named. The halves, already split: a malformed id
    /// is refused at parse time, like any other malformed argument.
    ChatFetch {
        scope: String,
        slug: String,
    },
}

/// One tool call, parsed. The `Err` is the `-32602` message.
pub(crate) fn parse(name: &str, arguments: &Value) -> Result<Call, String> {
    match name {
        "brain_search" => {
            // An absent or blank query is not an error: it asks for what was
            // read most recently. A query that is not a string still is.
            let query = match arguments.get("query") {
                None | Some(Value::Null) => None,
                Some(Value::String(value)) if value.trim().is_empty() => None,
                Some(Value::String(value)) => Some(value.to_owned()),
                Some(_) => return Err("`query` must be a string".to_owned()),
            };
            let limit = limit(arguments)?;
            Ok(match query {
                Some(query) => Call::Search { query, limit },
                None => Call::Recent { limit },
            })
        }
        "brain_page" => Ok(Call::Page {
            slug: required(arguments, "slug")?.to_owned(),
        }),
        "brain_context_for" => Ok(Call::ContextFor {
            repo: required(arguments, "repo")?.to_owned(),
            task: required(arguments, "task")?.to_owned(),
        }),
        "brain_note" => Ok(Call::Note {
            text: required(arguments, "text")?.to_owned(),
            title: optional(arguments, "title")?.map(str::to_owned),
            facts: match arguments.get("facts") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(facts)) if facts.len() <= MAX_NOTE_FACTS => facts.clone(),
                Some(Value::Array(_)) => {
                    return Err(format!("at most {MAX_NOTE_FACTS} facts per note"));
                }
                Some(_) => return Err("`facts` must be an array".to_owned()),
            },
        }),
        "brain_facts" => Ok(Call::Facts {
            subject: optional(arguments, "subject")?.map(str::to_owned),
            attribute: optional(arguments, "attribute")?.map(str::to_owned),
            as_of: optional(arguments, "as_of")?.map(str::to_owned),
        }),
        "search" => Ok(Call::ChatSearch {
            query: required(arguments, "query")?.to_owned(),
        }),
        "fetch" => {
            let id = required(arguments, "id")?;
            // The id is `<scope>:<slug>`, and both halves are the slug rule
            // (`[a-z0-9-]`, `is_slug`) — so neither can hold a `:`, and
            // splitting at the first one is unambiguous. Anything else is
            // the caller's syntax, not a page that was not found.
            let (scope, slug) = id.split_once(':').ok_or("`id` must be `<scope>:<slug>`")?;
            if !is_slug(scope) || !is_slug(slug) {
                return Err("`id` must be `<scope>:<slug>`".to_owned());
            }
            Ok(Call::ChatFetch {
                scope: scope.to_owned(),
                slug: slug.to_owned(),
            })
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/// Run one parsed call. The `Err` is the tool result's `isError` text.
pub(crate) async fn run(
    store: &PageStore,
    facts: &FactStore,
    base: &str,
    asker: &Asker,
    plan: &AccessPlan,
    call: Call,
) -> Result<Value, String> {
    // The pages the asker may read, each scope with its slug filter (ADR
    // 0004). Every read below goes through these and nothing else.
    let filters = plan.filters(KIND_PAGES, Permission::Read);
    // What the live feed calls this read (`GET /v1/graph/activity`); facts
    // are not pages, so they record nothing.
    let feed = match &call {
        Call::Search { .. }
        | Call::Recent { .. }
        | Call::ContextFor { .. }
        | Call::ChatSearch { .. } => "search",
        Call::Page { .. } | Call::ChatFetch { .. } => "read",
        Call::Note { .. } => "write",
        Call::Facts { .. } => "",
    };
    let outcome = match call {
        Call::Search { query, limit } => search(store, base, &filters, &query, limit).await,
        Call::Recent { limit } => recent(store, base, &filters, limit).await,
        Call::Page { slug } => page(store, base, &filters, &slug).await,
        Call::ChatSearch { query } => chat_search(store, base, &filters, &query).await,
        Call::ChatFetch { scope, slug } => chat_fetch(store, base, &filters, &scope, &slug).await,
        Call::ContextFor { repo, task } => {
            context_for(store, facts, base, asker, plan, &repo, &task).await
        }
        Call::Note {
            text,
            title,
            facts: stated,
        } => {
            let said = Said {
                text: &text,
                title: title.as_deref(),
                facts: stated,
            };
            note(store, facts, base, asker, plan, said).await
        }
        Call::Facts {
            subject,
            attribute,
            as_of,
        } => {
            read_facts(
                facts,
                asker,
                plan,
                subject.as_deref(),
                attribute.as_deref(),
                as_of.as_deref(),
            )
            .await
        }
    };
    if let Ok(value) = &outcome {
        record_feed(store, &asker.workspace_id, feed, value).await;
    }
    outcome
}

/// Puts the pages an answer names on the live feed, at most five a call.
/// Best effort by construction: `record_activity` never fails.
async fn record_feed(store: &PageStore, workspace: &str, kind: &str, value: &Value) {
    if kind.is_empty() {
        return;
    }
    let structured = value.get("structuredContent").unwrap_or(value);
    // A brain tool names a page by its `ref`; the ChatGPT-shaped tools name
    // it by its `id`. Both are read here, so those tools land on the feed
    // exactly as the ones beside them.
    fn named(row: &Value) -> Option<&str> {
        row.get("ref")
            .or_else(|| row.get("id"))
            .and_then(Value::as_str)
    }
    let mut refs: Vec<&str> = structured
        .get("results")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(named).collect())
        .unwrap_or_default();
    if let Some(one) = named(structured) {
        refs.push(one);
    }
    for reference in refs.into_iter().take(5) {
        // `scope/slug` or `scope:slug`: a scope and a slug are both the slug
        // rule, so neither half can hold either separator.
        if let Some((scope, slug)) = reference
            .split_once('/')
            .or_else(|| reference.split_once(':'))
        {
            store
                .record_activity(workspace, scope, slug, kind, "Claude")
                .await;
        }
    }
}

// ---------------------------------------------------------------------------
// The tools

/// `brain_search` with no query: the most recently updated pages the asker
/// may read, newest first, in the same shape a search answers.
async fn recent(
    store: &PageStore,
    base: &str,
    filters: &[ScopeFilter],
    limit: usize,
) -> Result<Value, String> {
    let scopes: Vec<&str> = filters.iter().map(|filter| filter.scope.as_str()).collect();
    let listed = if scopes.is_empty() {
        Vec::new()
    } else {
        store.list(&scopes, limit).await.map_err(store_failed)?
    };
    let mut found = Vec::with_capacity(listed.len());
    for item in listed {
        // The grant's slug filter applies to a listing exactly as to a search:
        // a page outside it is not named, not even by its title.
        let readable = filters
            .iter()
            .any(|filter| filter.scope == item.scope && filter.matches(&item.slug));
        if !readable {
            continue;
        }
        match store.read(&item.scope, &item.slug).await {
            Ok(Some(page)) => found.push(Found::new(base, &item.scope, &item.slug, &page)),
            Ok(None) => {}
            Err(error) => return Err(store_failed(error)),
        }
    }
    Ok(pages_answer(&found, "You have no readable pages yet.\n"))
}

/// The answer a page listing gives: one numbered line per page with its
/// snippet, then the citations the model is told to use.
fn pages_answer(found: &[Found], empty: &str) -> Value {
    let mut text = String::new();
    for (index, page) in found.iter().enumerate() {
        text.push_str(&format!(
            "[{}] {} — {}\n    {}\n",
            index + 1,
            page.title,
            page.reference(),
            snippet(&page.body),
        ));
    }
    if found.is_empty() {
        text.push_str(empty);
    }
    answer(
        append_citations(&text, found),
        json!({
            "results": found.iter().map(|page| json!({
                "title": page.title,
                "ref": page.reference(),
                "url": page.url,
                "snippet": snippet(&page.body),
            })).collect::<Vec<_>>(),
        }),
    )
}

/// `brain_search`: every page in the asker's scopes holding **every** word
/// of `query`.
async fn search(
    store: &PageStore,
    base: &str,
    filters: &[ScopeFilter],
    query: &str,
    limit: usize,
) -> Result<Value, String> {
    let found = find(store, base, filters, query, limit).await?;
    Ok(pages_answer(
        &found,
        "No page you can read mentions any word of that query. Try other, \
         more specific words, or call brain_context_for instead.\n",
    ))
}

/// The read behind `brain_page` and `fetch`: the first of the asker's scopes
/// that holds the slug — only the scope an id names, when it names one — at
/// the version the read saw. `None` when no readable scope holds it.
///
/// A direct read rather than a search on the slug: search matches a body's
/// words, and a page named `retry-policy` need not contain the word "retry".
async fn read_page(
    store: &PageStore,
    base: &str,
    filters: &[ScopeFilter],
    only: Option<&str>,
    slug: &str,
) -> Option<(Found, u32)> {
    for filter in filters {
        // A slug outside the grant is not looked up at all: no read, no
        // timing, no "it exists". Nor is a scope the asker holds no filter
        // for, however the id names one.
        if only.is_some_and(|scope| scope != filter.scope) || !filter.matches(slug) {
            continue;
        }
        let Ok(Some(hit)) = store.read(&filter.scope, slug).await else {
            continue;
        };
        let found = Found::new(base, &filter.scope, slug, &hit);
        return Some((found, hit.version));
    }
    None
}

/// The not-found answer `brain_page` and `fetch` give: it does not say which
/// of "no such page" and "not yours" it was, because saying which would be an
/// answer about somebody else's page.
fn not_found(named: &str) -> String {
    format!(
        "no page `{named}` in the pages you can read. It may not exist, or it may \
         belong to somebody else — this server does not say which."
    )
}

/// `brain_page`: the asker's own page of that slug, else the shared one.
async fn page(
    store: &PageStore,
    base: &str,
    filters: &[ScopeFilter],
    slug: &str,
) -> Result<Value, String> {
    let Some((found, version)) = read_page(store, base, filters, None, slug).await else {
        return Err(not_found(slug));
    };
    Ok(answer(
        format!("# {}\n\n{}", found.title, found.body),
        json!({
            "title": found.title,
            "ref": found.reference(),
            "url": found.url,
            "version": version,
        }),
    ))
}

/// `search`: `brain_search`'s query over the same filters, in the shape
/// ChatGPT's deep research reads — `id` rows it hands to `fetch`.
async fn chat_search(
    store: &PageStore,
    base: &str,
    filters: &[ScopeFilter],
    query: &str,
) -> Result<Value, String> {
    let found = find(store, base, filters, query, DEFAULT_LIMIT).await?;
    Ok(json_answer(json!({
        "results": found.iter().map(|page| json!({
            "id": page.id(),
            "title": page.title,
            "url": page.url,
        })).collect::<Vec<_>>(),
    })))
}

/// `fetch`: [`page`]'s read by the `scope:slug` id a `search` result named,
/// in the shape deep research reads. A scope the asker was not granted is the
/// same not-found answer, never a page that "exists".
async fn chat_fetch(
    store: &PageStore,
    base: &str,
    filters: &[ScopeFilter],
    scope: &str,
    slug: &str,
) -> Result<Value, String> {
    let Some((found, version)) = read_page(store, base, filters, Some(scope), slug).await else {
        return Err(not_found(&format!("{scope}:{slug}")));
    };
    Ok(json_answer(json!({
        "id": found.id(),
        "title": found.title,
        "text": format!("# {}\n\n{}", found.title, found.body),
        "url": found.url,
        "metadata": {
            "scope": found.scope,
            "slug": found.slug,
            "version": version,
        },
    })))
}

/// `brain_context_for`: a brief for a task, out of the asker's own pages.
///
/// Two queries — the task's own words, then the repository's — and the union
/// of their hits, the task's first: the store ranks pages by how many of a
/// query's words they hold, so a page about the repository but not the
/// task's words would otherwise sort below every task match.
async fn context_for(
    store: &PageStore,
    facts: &FactStore,
    base: &str,
    asker: &Asker,
    plan: &AccessPlan,
    repo: &str,
    task: &str,
) -> Result<Value, String> {
    let filters = plan.filters(KIND_PAGES, Permission::Read);
    let mut found = find(store, base, &filters, task, CONTEXT_PAGES).await?;
    for page in find(store, base, &filters, repo, CONTEXT_PAGES).await? {
        if !found.contains(&page) {
            found.push(page);
        }
    }
    found.truncate(CONTEXT_PAGES);

    // The facts the task is about, as of now, ahead of the pages
    // (docs/temporal-facts.md §7): the convention, then one line each.
    let now = facts.now();
    let fact_scopes = plan.scopes(KIND_FACTS, Permission::Read);
    let timelines = relevant_where(
        facts,
        &asker.user_id,
        &fact_scopes,
        task,
        |scope, subject| plan.allows(KIND_FACTS, scope, &subject_path(subject), Permission::Read),
    )
    .await
    .map_err(facts_failed)?;
    let (lines, fact_citations) = brief_lines(&timelines, now);
    let mut text = brief_section(&lines, now);
    for (index, page) in found.iter().enumerate() {
        text.push_str(&format!(
            "\n[{}] {} — {}\n\n{}\n",
            index + 1,
            page.title,
            page.reference(),
            page.body.trim(),
        ));
    }
    if found.is_empty() && lines.is_empty() {
        text.push_str(
            "Nothing in the pages you can read mentions this. Say so rather than \
             guessing, and ask for the repository name you are working in if it \
             was a guess.\n",
        );
    }
    let pages = found
        .iter()
        .map(|page| json!({ "title": page.title, "ref": page.reference(), "url": page.url }))
        .collect::<Vec<_>>();
    Ok(answer(
        append_citations(&text, &found),
        json!({ "repo": repo, "task": task, "pages": pages, "facts": fact_citations }),
    ))
}

/// `brain_note`: a page in the asker's own private scope.
///
/// **Title and text are both redacted first**: nothing a model writes reaches
/// a body with a secret or a piece of personal data still in it (issue
/// #108), and a title is body text the model chose. A `Decision` page
/// because that is the entity type whose only required key is a `title`.
/// The slug is the hash of the *redacted* text, so the same note lands on
/// the same page rather than accumulating near-duplicates.
async fn note(
    store: &PageStore,
    facts: &FactStore,
    base: &str,
    asker: &Asker,
    plan: &AccessPlan,
    said: Said<'_>,
) -> Result<Value, String> {
    let Said {
        text,
        title,
        facts: stated,
    } = said;
    // The facts are read before anything is written, so a malformed one
    // refuses the note rather than leaving half of it stored.
    let inputs = stated
        .iter()
        .map(|fact| fact_input(asker, fact))
        .collect::<Result<Vec<_>, _>>()?;
    for input in &inputs {
        facts.check(input).map_err(facts_failed)?;
    }
    let (clean, findings) = redact(text, Policy::Redact).map_err(|_| {
        "nothing was written: this note held something redaction refused to handle".to_owned()
    })?;
    let title = match title {
        Some(title) => match redact(title, Policy::Redact) {
            Ok((clean, _)) => one_line(&clean),
            Err(_) => String::new(),
        },
        None => String::new(),
    };
    let title = if title.is_empty() {
        "Note".to_owned()
    } else {
        title
    };

    let slug = note_slug(&clean);
    let scope = note_scope(asker, plan, &slug, !inputs.is_empty())?;
    let head = store
        .read(&scope, &slug)
        .await
        .map_err(store_failed)?
        .map(|page| page.version);
    let meta = store
        .write(
            &scope,
            &slug,
            PageWrite {
                entity_type: EntityType::Decision,
                markdown: format!("---\ntitle: {title}\n---\n\n{clean}\n"),
                author: Author::Brain { reconciles: None },
                base_version: head,
            },
        )
        .await
        .map_err(store_failed)?;

    let reference = format!("{scope}/{slug}");
    let mut recorded = Vec::with_capacity(inputs.len());
    for mut input in inputs {
        if input.source.is_none() {
            input.source = Some(format!("note:{reference}"));
        }
        let done = facts
            .record(&scope, input, &asker.user_id)
            .await
            .map_err(facts_failed)?;
        recorded.push(json!({
            "id": done.fact.id,
            "outcome": done.outcome.as_str(),
            "fact": fact_json(&done.fact),
        }));
    }
    let mut said = format!("Noted as `{reference}` (version {}).", meta.version);
    if !recorded.is_empty() {
        said.push_str(&format!(
            " {} fact(s) recorded; earlier values are kept as history.",
            recorded.len()
        ));
    }
    if !findings.is_empty() {
        said.push_str(&format!(
            " {} sensitive span(s) were redacted before it was stored; the note is \
             not what you wrote, verbatim.",
            findings.len()
        ));
    }
    Ok(answer(
        said,
        json!({
            "title": title,
            "ref": reference,
            "url": reference_url(base, &reference),
            "version": meta.version,
            "redacted": findings.len(),
            "facts": recorded,
        }),
    ))
}

/// `brain_facts`: the asker's timelines, as of a date (default now), one
/// line each in the format a model reads (docs/temporal-facts.md §7).
async fn read_facts(
    facts: &FactStore,
    asker: &Asker,
    plan: &AccessPlan,
    subject: Option<&str>,
    attribute: Option<&str>,
    as_of: Option<&str>,
) -> Result<Value, String> {
    let subject = match subject.map(str::trim).filter(|s| !s.is_empty()) {
        Some("me") => Some(
            person_subject(&asker.user_id).ok_or_else(|| "`me` names no person here".to_owned())?,
        ),
        Some(other) => Some(other.to_owned()),
        None => None,
    };
    let attribute = match attribute.map(str::trim).filter(|a| !a.is_empty()) {
        Some(raw) => Some(
            livingbrain_memory::timeline::normalize_attribute(raw)
                .ok_or_else(|| "`attribute` is not an attribute key".to_owned())?,
        ),
        None => None,
    };
    let now = facts.now();
    let at = match as_of
        .map(str::trim)
        .filter(|a| !a.is_empty() && *a != "now")
    {
        Some(raw) => livingbrain_memory::timeline::PartialDate::parse(raw)
            .map_err(|error| error.to_string())?
            .as_of_instant()
            .min(now),
        None => now,
    };
    let scopes = plan.scopes(KIND_FACTS, Permission::Read);
    let names: Vec<&str> = scopes.iter().map(String::as_str).collect();
    let timelines: Vec<Timeline> = facts
        .timelines(&names, subject.as_deref(), attribute.as_deref())
        .await
        .map_err(facts_failed)?
        .into_iter()
        // A subject-prefix grant: timelines outside it are dropped before
        // anything is rendered or cited.
        .filter(|timeline| {
            plan.allows(
                KIND_FACTS,
                &timeline.scope,
                &subject_path(&timeline.subject),
                Permission::Read,
            )
        })
        .collect();
    let (lines, citations) = brief_lines(&timelines, at);
    let text = if lines.is_empty() {
        "No fact you can read matches that. Facts are recorded with brain_note's \
         `facts` argument."
            .to_owned()
    } else {
        brief_section(&lines, at)
    };
    Ok(answer(
        text,
        json!({
            "as_of": livingbrain_memory::timeline::rfc3339(at),
            "lines": lines.iter().map(|(_, line)| line).collect::<Vec<_>>(),
            "citations": citations,
        }),
    ))
}

/// One `facts` item of `brain_note`, as a store input; `me` resolved.
fn fact_input(asker: &Asker, fact: &Value) -> Result<FactInput, String> {
    const KNOWN: [&str; 11] = [
        "subject",
        "attribute",
        "value",
        "unit",
        "valid_from",
        "valid_to",
        "observed_at",
        "source",
        "quote",
        "confidence",
        "cardinality",
    ];
    let Some(object) = fact.as_object() else {
        return Err("each fact is an object".to_owned());
    };
    if let Some(unknown) = object.keys().find(|key| !KNOWN.contains(&key.as_str())) {
        return Err(format!(
            "a fact has no `{unknown}` field (the brain sets status and supersession itself)"
        ));
    }
    let text = |key: &str| -> Result<Option<String>, String> {
        match object.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.clone())),
            Some(Value::Number(number)) if key == "value" => Ok(Some(number.to_string())),
            Some(Value::Bool(flag)) if key == "value" => Ok(Some(flag.to_string())),
            Some(_) => Err(format!("a fact's `{key}` must be a string")),
        }
    };
    let subject = text("subject")?.ok_or("a fact needs a `subject`")?;
    let subject = if subject.trim() == "me" {
        person_subject(&asker.user_id).ok_or("`me` names no person here")?
    } else {
        subject
    };
    Ok(FactInput {
        subject,
        attribute: text("attribute")?.ok_or("a fact needs an `attribute`")?,
        value: text("value")?.ok_or("a fact needs a `value`")?,
        unit: text("unit")?,
        valid_from: text("valid_from")?,
        valid_to: text("valid_to")?,
        observed_at: text("observed_at")?,
        source: text("source")?,
        quote: text("quote")?,
        confidence: match object.get("confidence") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_f64()
                    .ok_or("a fact's `confidence` must be a number")?,
            ),
        },
        cardinality: text("cardinality")?,
    })
}

/// A fact store failure, as a model should see it: its own mistakes say
/// what they were, the store's internals do not.
fn facts_failed(error: FactError) -> String {
    match error {
        FactError::Invalid(message) => format!("the fact was refused: {message}"),
        error @ FactError::CardinalityMismatch { .. } => {
            format!("the fact was refused: {error}")
        }
        FactError::Store(error) => store_failed(error),
    }
}

// ---------------------------------------------------------------------------
// Reading

/// One page a tool quoted, prepared once: the tools print a title, a
/// reference and a body and never re-parse the frontmatter.
#[derive(PartialEq, Eq)]
struct Found {
    title: String,
    scope: String,
    slug: String,
    url: String,
    body: String,
}

impl Found {
    fn new(base: &str, scope: &str, slug: &str, page: &Page) -> Self {
        let (_, body) = parse_frontmatter(&page.markdown, page.entity_type)
            .unwrap_or_else(|_| (Frontmatter::default(), page.markdown.clone()));
        // The page's title is whichever key its entity type requires; the
        // store refuses a page without one, so the slug is a last resort.
        let title = page
            .frontmatter
            .get("name")
            .or_else(|| page.frontmatter.get("title"))
            .or_else(|| page.frontmatter.get("term"))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(slug)
            .to_owned();
        Self {
            title,
            scope: scope.to_owned(),
            slug: slug.to_owned(),
            url: page_url(base, scope, slug),
            body,
        }
    }

    /// `scope/slug`: the ref a citation names.
    fn reference(&self) -> String {
        format!("{}/{}", self.scope, self.slug)
    }

    /// `scope:slug`: the id a `search` result names and a `fetch` takes. A
    /// scope and a slug are both the slug rule, so neither holds the `:` —
    /// the one separator there can be.
    fn id(&self) -> String {
        format!("{}:{}", self.scope, self.slug)
    }
}

/// Search the asker's scopes and read back each hit. The scopes are the
/// `&[&str]` the store binds — the only place a query meets a scope list.
async fn find(
    store: &PageStore,
    base: &str,
    filters: &[ScopeFilter],
    query: &str,
    limit: usize,
) -> Result<Vec<Found>, String> {
    let hits = store
        .search_in(filters, query, limit)
        .await
        .map_err(store_failed)?;
    let mut found = Vec::with_capacity(hits.len());
    for hit in hits {
        match store.read(&hit.scope, &hit.slug).await {
            // A hit whose page is gone — a scope shredded between the search
            // and the read — is skipped rather than failing the whole call.
            Ok(None) => {}
            Ok(Some(page)) => found.push(Found::new(base, &hit.scope, &hit.slug, &page)),
            Err(error) => return Err(store_failed(error)),
        }
    }
    Ok(found)
}

/// What a `brain_note` call says: the text, its title, and the facts it
/// states.
struct Said<'a> {
    text: &'a str,
    title: Option<&'a str>,
    facts: Vec<Value>,
}

/// Where `brain_note` writes (ADR 0004). A member writes into their own
/// memory, as always. An agent account has no memory of its own: its note
/// lands in its owner's personal space, else the shared one — whichever it
/// holds `write` on for this slug (and on facts, when it states any).
fn note_scope(
    asker: &Asker,
    plan: &AccessPlan,
    slug: &str,
    with_facts: bool,
) -> Result<String, String> {
    if let Some(own) = plan.own_scope() {
        return Ok(own);
    }
    let owner = asker.agent_owner.as_deref().unwrap_or_default();
    for space in [
        personal(owner),
        livingbrain_pages::access::SHARED.to_owned(),
    ] {
        let Some(scope) = space_scope(&asker.workspace_id, &space) else {
            continue;
        };
        if plan.allows_page(&scope, slug, Permission::Write)
            && (!with_facts || plan.allows(KIND_FACTS, &scope, &[], Permission::Write))
        {
            return Ok(scope);
        }
    }
    Err(
        "nothing was written: this agent account has not been granted write access to \
         its owner's notes or to the shared space"
            .to_owned(),
    )
}

/// The note slug for a body: SHA-256, hex-truncated to [`SLUG_HEX`]. The same
/// redaction, the same note, the same page.
fn note_slug(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let hex: String = digest
        .iter()
        .take(SLUG_HEX / 2)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("note-{hex}")
}

/// A tool result: the text the model reads, and the same thing as data.
fn answer(text: String, structured: Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
    })
}

/// A tool result for a caller that reads JSON, not prose (ChatGPT's deep
/// research): the one text item is the structured content itself as a JSON
/// string, which is the shape that caller's spec asks for.
fn json_answer(structured: Value) -> Value {
    let text = structured.to_string();
    answer(text, structured)
}

/// `text` followed by the numbered citations its `[n]` markers resolve to.
fn append_citations(text: &str, found: &[Found]) -> String {
    let mut out = text.to_owned();
    out.push_str("\nCitations:\n");
    for (index, page) in found.iter().enumerate() {
        out.push_str(&format!("[{}] {} — {}\n", index + 1, page.title, page.url));
    }
    out
}

/// A body with its whitespace collapsed, cut to [`SNIPPET_CHARS`] on a
/// character boundary.
fn snippet(body: &str) -> String {
    let joined = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = joined.chars().take(SNIPPET_CHARS).collect();
    if joined.chars().nth(SNIPPET_CHARS).is_some() {
        out.push('…');
    }
    out
}

/// Whitespace collapsed to single spaces, so a note's frontmatter and the
/// tool's own line wrapping cannot break the `key: value` rule.
fn one_line(value: &str) -> String {
    let mut out: String = value.split_whitespace().collect::<Vec<_>>().join(" ");
    out.truncate(
        out.char_indices()
            .nth(MAX_TITLE_CHARS)
            .map_or(out.len(), |(index, _)| index),
    );
    out
}

// ---------------------------------------------------------------------------
// Arguments

/// A required string argument.
fn required<'a>(arguments: &'a Value, key: &str) -> Result<&'a str, String> {
    match arguments.get(key) {
        Some(Value::String(value)) if value.trim().is_empty() => Err(format!("`{key}` is empty")),
        Some(Value::String(value)) => Ok(value),
        Some(_) => Err(format!("`{key}` must be a string")),
        None => Err(format!("`{key}` is required")),
    }
}

/// An optional string argument: absent is fine, wrong-typed is not.
fn optional<'a>(arguments: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

/// A caller-supplied `limit`, capped rather than refused: an agent asking
/// for a thousand hits wants hits, not an error.
fn limit(arguments: &Value) -> Result<usize, String> {
    match arguments.get("limit") {
        None | Some(Value::Null) => Ok(DEFAULT_LIMIT),
        Some(value) => match value.as_u64() {
            Some(asked) => Ok(asked.clamp(1, MAX_LIMIT as u64) as usize),
            None => Err("`limit` must be a positive integer".to_owned()),
        },
    }
}

/// A store failure, as a model should see it.
///
/// The store's own messages name key versions, scope names and custodian
/// errors, which is the wrong audience for them: a model can do nothing with
/// a key version and should not be shown one. Two cases stay distinct
/// because the model can act on them — a write refused over a human edit
/// asks for a reconciliation, and a shredded scope asks it to stop.
fn store_failed(error: PageError) -> String {
    match error {
        PageError::HumanEditPending { version } => format!(
            "nothing was written: that page has a human edit at version {version} waiting. \
             Re-read it, reconcile, and try again."
        ),
        PageError::Conflict { base, head } => format!(
            "nothing was written: the page moved to version {head} since you read it \
             (you wrote against {base}). Re-read it and try again."
        ),
        PageError::Shredded(_) => {
            "that memory has been deleted; nothing is there to read.".to_owned()
        }
        _ => "the page store refused this call. Nothing was written.".to_owned(),
    }
}
