//! The `export` module: `GET /v1/export`, the route the CLI's
//! `livingbrain export` speaks.
//!
//! One zip of every page the asker can read, one Markdown file per page at
//! `{scope}/{slug}.md` — the bodies are UTF-8 Markdown that already carry
//! their frontmatter, so the archive *is* the wiki. The zip writer is
//! hand-rolled ([`zip`]) because a module's wasm graph must stay free of
//! archive crates, and a stored-entry zip is a hundred lines, not a
//! dependency.

use std::collections::HashMap;
use std::sync::Arc;

use cratefield_core::axum::extract::{Query, State};
use cratefield_core::axum::http::{HeaderMap, Method, StatusCode, header};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::axum::routing::get;
use cratefield_core::{Config, ConfigError, Migrations, Module, ModuleContext, Port, Problem};
use cratefield_module_orgs::grants::Permission;
use livingbrain_pages::PageError;
use livingbrain_pages::access::KIND_PAGES;

use crate::Wiki;
use crate::problems::{EXPORT_FORMAT, page_read_error};
use crate::service::Service;
use crate::zip;

/// How many page heads one export reads at most — the same ceiling the
/// zip writer puts on entries, comfortably above any real wiki.
const EXPORT_PAGE_LIMIT: usize = 1000;

/// The one export format this surface speaks.
const SUPPORTED_FORMAT: &str = "obsidian";

/// The `export` module, constructed with the pages access the venture
/// injected.
pub struct Export {
    wiki: Wiki,
}

impl Export {
    /// An export module over the given pages access. See [`Wiki`] for
    /// where each field comes from.
    #[must_use]
    pub fn new(wiki: Wiki) -> Self {
        Self { wiki }
    }
}

impl Module for Export {
    fn name(&self) -> &'static str {
        "export"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// The store is built per request from these; the blob is this module's
    /// own per-request view, which the composition has rooted on the pages
    /// key space by planting [`RebindBlob`](crate::RebindBlob) under the
    /// scope the harness applies. Undeclared, the view would never arrive.
    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Blob, Port::Clock, Port::IdGen]
    }

    /// `Signer` is optional for the same reason the pages module's is: the
    /// credential resolver runs on this module's port view, and the views
    /// only carry what a module declared. Without the declaration the view
    /// would arrive with no signer, and `BearerAuth` could never verify a
    /// caller.
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
            .route("/", get(export))
            .with_state(state)
    }
}

/// `GET /?format=obsidian` — every page the asker can read, zipped.
///
/// A page that vanished or whose scope was shredded between the list and
/// the read is skipped — an export of what *is* readable, not a failure
/// over one stale head. Any other read failure stops the export: a zip
/// that quietly misses pages because a database hiccuped would be worse
/// than no zip.
///
/// # Errors
///
/// 401 without a credential, 422 for a format other than
/// [`SUPPORTED_FORMAT`], and the store's mapped problem otherwise.
async fn export(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, Problem> {
    let asker = state.asker(&method, &headers).await?;
    let format = query
        .get("format")
        .map(String::as_str)
        .unwrap_or(SUPPORTED_FORMAT);
    if format != SUPPORTED_FORMAT {
        return Err(Problem::new(&EXPORT_FORMAT).with_detail(format!(
            "the export speaks one format: `{SUPPORTED_FORMAT}` (got {format:?})"
        )));
    }

    let store = state.store()?;
    let plan = state.access(&asker).await?;
    let filters = plan.filters(KIND_PAGES, Permission::Read);
    let listed = store
        .list_in(&filters, EXPORT_PAGE_LIMIT)
        .await
        .map_err(page_read_error)?;

    let mut entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(listed.len());
    for summary in listed {
        match store.read(&summary.scope, &summary.slug).await {
            // Vanished between the list and the read, or shredded on
            // purpose: the export carries what is readable and keeps going.
            Ok(Some(page)) => entries.push((
                format!("{}/{}.md", summary.scope, summary.slug),
                page.markdown.into_bytes(),
            )),
            Ok(None) | Err(PageError::Shredded(_)) => {}
            Err(error) => return Err(page_read_error(error)),
        }
    }

    // An export is the whole readable wiki leaving the service: always
    // recorded, whoever asks (ADR 0004).
    state
        .audit(&asker, "export", &format!("pages:{}", entries.len()))
        .await;
    // The page cap above keeps the entry count far below the zip format's
    // own; `None` would mean that bound was raised past the format's.
    let archive = zip::archive(&entries).ok_or_else(Problem::internal)?;
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/zip"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"livingbrain-export.zip\"",
            ),
        ],
        archive,
    )
        .into_response())
}
