//! The four tools, and the citations they answer with.
//!
//! Every tool reads through the scopes the caller was granted and none takes
//! a scope argument. A page the asker was not granted is not refused loudly —
//! it is not found, because answering "you do not have access to this" is
//! itself an answer about somebody else's page.

use livingbrain_access::{Scope, UserId};
use livingbrain_pages::{
    Author, EntityType, Frontmatter, Page, PageError, PageStore, PageWrite, parse_frontmatter,
};
use livingbrain_redact::{Policy, redact};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::auth::{Asker, page_scope};

/// How many hits `brain_search` returns when the caller names no limit.
const DEFAULT_LIMIT: usize = 10;
/// The ceiling on a caller-supplied `limit`.
const MAX_LIMIT: usize = 25;
/// How many pages `brain_context_for` reads, after deduplication.
const CONTEXT_PAGES: usize = 5;
/// How much of a body `brain_search` quotes back.
const SNIPPET_CHARS: usize = 240;
/// The longest title a note's frontmatter may carry.
const MAX_TITLE_CHARS: usize = 120;
/// Hex characters of the note slug's SHA-256.
const SLUG_HEX: usize = 12;
/// The site's wiki root, which [`Found::url`] builds a citation against.
const WIKI_BASE: &str = "https://livingbrain.wiki";

/// The tools as `tools/list` returns them.
pub(crate) fn descriptors() -> Value {
    fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
        json!({
            "name": name,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "required": required,
            },
        })
    }
    json!([
        tool(
            "brain_search",
            "Search the pages you are allowed to read. A page must hold every word of \
             the query: this is a recall of facts, not a fuzzy match.",
            json!({
                "query": { "type": "string", "description": "Words every matching page must contain." },
                "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT,
                           "description": "How many pages to return." },
            }),
            &["query"],
        ),
        tool(
            "brain_page",
            "Read one page's Markdown by slug, from the pages you are allowed to read.",
            json!({ "slug": { "type": "string", "description": "The page's slug." } }),
            &["slug"],
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
        ),
        tool(
            "brain_note",
            "Record something worth remembering in your own private pages. Secrets and \
             personal data are redacted before it is stored.",
            json!({
                "text": { "type": "string", "description": "What to remember." },
                "title": { "type": "string", "description": "A short title for the note." },
            }),
            &["text"],
        ),
    ])
}

/// One parsed tool call. Parsing is where a malformed argument is refused
/// (`-32602`); everything after it is an `isError` result, because the
/// failure is no longer the caller's syntax.
pub(crate) enum Call {
    Search { query: String, limit: usize },
    Page { slug: String },
    ContextFor { repo: String, task: String },
    Note { text: String, title: Option<String> },
}

/// One tool call, parsed. The `Err` is the `-32602` message.
pub(crate) fn parse(name: &str, arguments: &Value) -> Result<Call, String> {
    match name {
        "brain_search" => Ok(Call::Search {
            query: required(arguments, "query")?.to_owned(),
            limit: limit(arguments)?,
        }),
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
        }),
        other => Err(format!("unknown tool: {other}")),
    }
}

/// Run one parsed call. The `Err` is the tool result's `isError` text.
pub(crate) async fn run(
    store: &PageStore,
    asker: &Asker,
    scopes: &[String],
    call: Call,
) -> Result<Value, String> {
    match call {
        Call::Search { query, limit } => search(store, scopes, &query, limit).await,
        Call::Page { slug } => page(store, scopes, &slug).await,
        Call::ContextFor { repo, task } => context_for(store, scopes, &repo, &task).await,
        Call::Note { text, title } => note(store, asker, &text, title.as_deref()).await,
    }
}

// ---------------------------------------------------------------------------
// The tools

/// `brain_search`: every page in the asker's scopes holding **every** word
/// of `query`.
async fn search(
    store: &PageStore,
    scopes: &[String],
    query: &str,
    limit: usize,
) -> Result<Value, String> {
    let found = find(store, scopes, query, limit).await?;
    let mut text = String::new();
    for (index, page) in found.iter().enumerate() {
        text.push_str(&format!(
            "[{}] {} — {}\n    {}\n",
            index + 1,
            page.title,
            page.reference,
            snippet(&page.body),
        ));
    }
    if found.is_empty() {
        text.push_str(
            "No page you can read holds every word of that query. Try fewer, \
             more specific words, or call brain_context_for instead.\n",
        );
    }
    Ok(answer(
        append_citations(&text, &found),
        json!({
            "results": found.iter().map(|page| json!({
                "title": page.title,
                "ref": page.reference,
                "url": page.url,
                "snippet": snippet(&page.body),
            })).collect::<Vec<_>>(),
        }),
    ))
}

/// `brain_page`: the asker's own page of that slug, else the shared one.
///
/// A direct read rather than a search on the slug: search matches a body's
/// words, and a page named `retry-policy` need not contain the word "retry".
async fn page(store: &PageStore, scopes: &[String], slug: &str) -> Result<Value, String> {
    for scope in scopes {
        let Ok(Some(hit)) = store.read(scope, slug).await else {
            continue;
        };
        let found = Found::new(scope, slug, &hit);
        return Ok(answer(
            format!("# {}\n\n{}", found.title, found.body),
            json!({
                "title": found.title,
                "ref": found.reference,
                "url": found.url,
                "version": hit.version,
            }),
        ));
    }
    Err(format!(
        "no page `{slug}` in the pages you can read. It may not exist, or it may \
         belong to somebody else — this server does not say which."
    ))
}

/// `brain_context_for`: a brief for a task, out of the asker's own pages.
///
/// Two queries — the task's own words, then the repository's — and the union
/// of their hits, because the store's index intersects a query's tokens: a
/// page mentioning every word of "api retry refund policy" is a rarer thing
/// than one mentioning the task.
async fn context_for(
    store: &PageStore,
    scopes: &[String],
    repo: &str,
    task: &str,
) -> Result<Value, String> {
    let mut found = find(store, scopes, task, CONTEXT_PAGES).await?;
    for page in find(store, scopes, repo, CONTEXT_PAGES).await? {
        if !found.contains(&page) {
            found.push(page);
        }
    }
    found.truncate(CONTEXT_PAGES);

    let mut text = String::new();
    for (index, page) in found.iter().enumerate() {
        text.push_str(&format!(
            "\n[{}] {} — {}\n\n{}\n",
            index + 1,
            page.title,
            page.reference,
            page.body.trim(),
        ));
    }
    if found.is_empty() {
        text.push_str(
            "Nothing in the pages you can read mentions this. Say so rather than \
             guessing, and ask for the repository name you are working in if it \
             was a guess.\n",
        );
    }
    let pages = found
        .iter()
        .map(|page| json!({ "title": page.title, "ref": page.reference, "url": page.url }))
        .collect::<Vec<_>>();
    Ok(answer(
        append_citations(&text, &found),
        json!({ "repo": repo, "task": task, "pages": pages }),
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
    asker: &Asker,
    text: &str,
    title: Option<&str>,
) -> Result<Value, String> {
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

    let scope = page_scope(
        &asker.workspace_id,
        &Scope::User(UserId::new(asker.user_id.clone())),
    );
    let slug = note_slug(&clean);
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
    let mut said = format!("Noted as `{reference}` (version {}).", meta.version);
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
            "url": format!("{WIKI_BASE}/brain/{reference}"),
            "version": meta.version,
            "redacted": findings.len(),
        }),
    ))
}

// ---------------------------------------------------------------------------
// Reading

/// One page a tool quoted, prepared once: the tools print a title, a
/// reference and a body and never re-parse the frontmatter.
#[derive(PartialEq, Eq)]
struct Found {
    title: String,
    reference: String,
    url: String,
    body: String,
}

impl Found {
    fn new(scope: &str, slug: &str, page: &Page) -> Self {
        let reference = format!("{scope}/{slug}");
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
            reference: reference.clone(),
            url: format!("{WIKI_BASE}/brain/{reference}"),
            body,
        }
    }
}

/// Search the asker's scopes and read back each hit. The scopes are the
/// `&[&str]` the store binds — the only place a query meets a scope list.
async fn find(
    store: &PageStore,
    scopes: &[String],
    query: &str,
    limit: usize,
) -> Result<Vec<Found>, String> {
    let names: Vec<&str> = scopes.iter().map(String::as_str).collect();
    let hits = store
        .search(&names, query, limit)
        .await
        .map_err(store_failed)?;
    let mut found = Vec::with_capacity(hits.len());
    for hit in hits {
        match store.read(&hit.scope, &hit.slug).await {
            // A hit whose page is gone — a scope shredded between the search
            // and the read — is skipped rather than failing the whole call.
            Ok(None) => {}
            Ok(Some(page)) => found.push(Found::new(&hit.scope, &hit.slug, &page)),
            Err(error) => return Err(store_failed(error)),
        }
    }
    Ok(found)
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
