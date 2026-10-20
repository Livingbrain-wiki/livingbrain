//! The `public` module: `/v1/public`, the one way a page becomes
//! customer-visible (issue #51; ADR 0005).
//!
//! | Route | What |
//! |---|---|
//! | `GET /` | the published pages |
//! | `POST /pages` `{slug, url?}` | publish a page of the shared space |
//! | `DELETE /pages/{slug}` | withdraw it |
//!
//! Publishing is a workspace **owner's** act, with their whole credential,
//! and is audited. The source is always a page of the workspace's `shared`
//! space: personal and channel pages cannot be named, and a session log is
//! refused. The page is passed through the same inbound screen every note
//! passes (hidden text, injection payloads, secrets, personal data), and the
//! *cleaned* text is copied into the workspace's `public` scope as a snapshot
//! — editing the internal page afterwards publishes nothing until the owner
//! publishes again.

use std::sync::Arc;

use cratefield_core::axum::Json;
use cratefield_core::axum::extract::{Path, State};
use cratefield_core::axum::http::{HeaderMap, Method, StatusCode};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::axum::routing::{delete, get, post};
use cratefield_core::{Config, ConfigError, Migrations, Module, ModuleContext, Port, Problem};
use livingbrain_access::Scope;
use livingbrain_pages::{Author, EntityType, PageWrite, Publication, is_slug, page_scope};
use livingbrain_radar::screen::{is_fetchable_url, screen_text};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Wiki;
use crate::problems::{
    NOT_FOUND, PAGE_NOT_FOUND, SCREENED, TOOL_INPUT, page_read_error, page_write_error,
};
use crate::recipes::{title_of, wiki_url};
use crate::service::Service;

/// The `public` module.
pub struct PublicPages {
    wiki: Wiki,
}

impl PublicPages {
    /// A public-pages module over the given page access.
    #[must_use]
    pub fn new(wiki: Wiki) -> Self {
        Self { wiki }
    }
}

impl Module for PublicPages {
    fn name(&self) -> &'static str {
        "public"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Blob, Port::Clock, Port::IdGen]
    }

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
            .route("/", get(list))
            .route("/pages", post(publish))
            .route("/pages/{slug}", delete(unpublish))
            .with_state(state)
    }
}

fn publication_json(publication: &Publication) -> Value {
    json!({
        "slug": publication.slug,
        "title": publication.title,
        "url": publication.url,
        "scope": "public",
        "source_version": publication.source_version,
        "published_by": publication.published_by,
        "published_at": publication.published_at,
    })
}

/// `GET /` — what is published, for owners.
async fn list(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&method, &headers).await?;
    Service::owner_only(&asker)?;
    let scope = page_scope(&asker.workspace_id, &Scope::Public);
    let pages = state
        .public_store()?
        .list(&scope)
        .await
        .map_err(page_read_error)?;
    Ok(Json(json!({
        "pages": pages.iter().map(publication_json).collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Deserialize)]
struct PublishRequest {
    slug: String,
    /// Where a citation links to; the web app's page address when omitted.
    #[serde(default)]
    url: Option<String>,
}

/// `POST /pages` — publishes a page of the shared space.
async fn publish(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
    Json(request): Json<PublishRequest>,
) -> Result<Response, Problem> {
    let asker = state.asker(&method, &headers).await?;
    Service::owner_only(&asker)?;
    if !is_slug(&request.slug) {
        return Err(Problem::new(&TOOL_INPUT).with_detail("`slug` is outside the slug rule"));
    }
    if let Some(url) = &request.url
        && !is_fetchable_url(url)
    {
        return Err(Problem::new(&TOOL_INPUT)
            .with_detail("`url` must be an http or https address of visible ASCII"));
    }
    let store = state.store()?;
    let source_scope = page_scope(&asker.workspace_id, &Scope::Shared);
    let public_scope = page_scope(&asker.workspace_id, &Scope::Public);
    // Only the shared space is ever a source, and a session log is a
    // coding agent's working record, not something to tell customers.
    let page = store
        .read(&source_scope, &request.slug)
        .await
        .map_err(page_read_error)?
        .filter(|page| page.entity_type != EntityType::Session)
        .ok_or_else(|| Problem::new(&PAGE_NOT_FOUND))?;
    // The same screen every inbound note passes, and the cleaned text is
    // what is published: a secret or a payload in the internal page never
    // reaches the public copy.
    let clean = screen_text(&page.markdown).map_err(|_| Problem::new(&SCREENED))?;
    let head = store
        .read(&public_scope, &request.slug)
        .await
        .map_err(page_read_error)?;
    store
        .write(
            &public_scope,
            &request.slug,
            PageWrite {
                entity_type: page.entity_type,
                markdown: clean,
                author: Author::Human {
                    id: asker.user_id.clone(),
                },
                base_version: head.map(|existing| existing.version),
            },
        )
        .await
        .map_err(page_write_error)?;
    let title = screen_text(&title_of(&page)).unwrap_or_else(|_| request.slug.clone());
    let publication = Publication {
        scope: public_scope.clone(),
        slug: request.slug.clone(),
        title,
        url: request
            .url
            .clone()
            .unwrap_or_else(|| wiki_url(&state.base(), &public_scope, &request.slug)),
        source_scope,
        source_version: page.version,
        published_by: asker.user_id.clone(),
        published_at: state
            .core()?
            .1
            .now()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
    };
    state
        .public_store()?
        .publish(&asker.workspace_id, &publication)
        .await
        .map_err(page_write_error)?;
    state
        .audit(&asker, "page.publish", &format!("public/{}", request.slug))
        .await;
    Ok((StatusCode::CREATED, Json(publication_json(&publication))).into_response())
}

/// `DELETE /pages/{slug}` — withdraws a page from customers at once.
async fn unpublish(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&method, &headers).await?;
    Service::owner_only(&asker)?;
    let scope = page_scope(&asker.workspace_id, &Scope::Public);
    let removed = state
        .public_store()?
        .unpublish(&scope, &slug)
        .await
        .map_err(page_read_error)?;
    if !removed {
        return Err(Problem::new(&NOT_FOUND));
    }
    state
        .audit(&asker, "page.unpublish", &format!("public/{slug}"))
        .await;
    Ok(Json(json!({ "unpublished": slug })))
}
