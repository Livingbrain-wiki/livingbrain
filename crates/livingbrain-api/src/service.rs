//! Per-request plumbing shared by every route in this crate: the page store
//! over the module's own blob view, the asker behind the bearer token or
//! the session cookie, and the scopes that asker may read. One [`Service`] per module router, built
//! once from the module context and the injected [`Wiki`].
//!
//! The blob the store is built over is `ctx.ports.blob` — **this module's
//! own per-request view**. For the sibling modules the composition has
//! rooted that view on the pages key space
//! ([`RebindBlob`](crate::RebindBlob) planted under the scope the harness
//! applies), so the same expression serves the merged pages surface, whose
//! view is pages-rooted because the module *is* pages. Either way the
//! handle was derived for this request; nothing here outlives it.

use std::sync::Arc;

use cratefield_core::axum::http::{HeaderMap, Method, header};
use cratefield_core::{ModuleContext, Ports, Problem};
use cratefield_kms::Kms;
use livingbrain_mcp::{Asker, AuthError, BearerAuth, page_scopes};
use livingbrain_pages::{PageStore, SourceStore};

use crate::problems::{CROSS_SITE_REQUEST, UNAUTHORIZED};
use crate::{Wiki, port};

/// Resolves the web app's **session cookie** to the asker it speaks for —
/// the second way into these routes, beside [`BearerAuth`].
///
/// The web app never sends an `Authorization` header (`app/assets/app.js`):
/// its session is an `HttpOnly` cookie the browser attaches on its own. So
/// without this seam every call the wiki page makes is a 401. The seam is
/// separate from [`BearerAuth`] on purpose: the MCP endpoint takes that one
/// and stays bearer-only, and a composition that wires no session here
/// gets exactly the bearer-only surface it had before.
///
/// Like [`BearerAuth`] it is implemented by the composition, which can see
/// the module that owns sessions; this crate only decides **when** it is
/// consulted (no `Authorization` header at all) and **what** a cookie may
/// do (a write must pass [`SessionAuth::same_origin`]).
#[async_trait::async_trait]
pub trait SessionAuth: Send + Sync {
    /// The member the request's session cookie names, as an [`Asker`] — a
    /// full session carries `token_scopes: None`.
    ///
    /// # Errors
    ///
    /// [`AuthError`] when there is no cookie, it does not verify, it has
    /// expired, or its member is gone. Every case is the same 401.
    async fn session(&self, ports: &Ports, headers: &HeaderMap) -> Result<Asker, AuthError>;

    /// Whether the browser signals (`sec-fetch-site`, then `Origin`) place
    /// this request on the deployment's own origin. Consulted for every
    /// cookie-authenticated request whose method is not safe.
    ///
    /// # Errors
    ///
    /// The detail a 403 problem carries, saying which signal failed.
    fn same_origin(&self, headers: &HeaderMap) -> Result<(), String>;
}

/// The state behind every route in this crate.
///
/// `Arc`-shared by the module's routes; `store` builds a fresh
/// [`PageStore`] per request, the way the MCP server does — a store holds a
/// clock and an id generator, and both are cheap `Arc`s, but a request
/// ought not share a borrow with the router that served it.
pub(crate) struct Service {
    /// The module's own context. Its port view is the store the page bodies
    /// travel over: rooted on `pages/` — by the composition's
    /// [`RebindBlob`](crate::RebindBlob) for the four sibling modules,
    /// by the module's own name for the merged pages surface. The context is
    /// held behind an `Arc` — the way the pages module hands a surface its
    /// context — and the ports are read per request.
    ctx: Arc<ModuleContext>,
    kms: Arc<dyn Kms>,
    auth: Arc<dyn BearerAuth>,
    /// The cookie resolver, when the composition wired one; `None` is a
    /// bearer-only surface.
    session: Option<Arc<dyn SessionAuth>>,
}

impl Service {
    /// The service for one of the four sibling modules: the ports come from
    /// the module context, the custodian and credential resolver from the
    /// injected [`Wiki`].
    pub(crate) fn from_wiki(ctx: ModuleContext, wiki: &Wiki) -> Self {
        Self::for_pages(Arc::new(ctx), wiki)
    }

    /// The service for the merged pages surface. It runs *inside* the pages
    /// module, so the blob it reads through is the module's own port view —
    /// already scoped to `pages` by the harness, which is the same view the
    /// page store itself is built on.
    pub(crate) fn for_pages(ctx: Arc<ModuleContext>, wiki: &Wiki) -> Self {
        Self {
            ctx,
            kms: wiki.kms.clone(),
            auth: wiki.auth.clone(),
            session: wiki.session.clone(),
        }
    }

    /// The page store for one request, over this module's own blob view —
    /// pages-rooted, per the composition that mounted the module.
    ///
    /// # Errors
    ///
    /// [`Problem::internal`] when the composition left a declared port
    /// unwired — a deployment bug, never a caller's. A sibling module
    /// whose composition did not plant the [`RebindBlob`](crate::RebindBlob)
    /// (or a pages module whose binding did not resolve) lands here, with
    /// a 500 that says nothing about which port was missing.
    pub(crate) fn store(&self) -> Result<PageStore, Problem> {
        Ok(PageStore::new(
            port(self.ctx.ports.db.clone())?,
            port(self.ctx.ports.blob.clone())?,
            self.kms.clone(),
            port(self.ctx.ports.clock.clone())?,
            port(self.ctx.ports.id_gen.clone())?,
        ))
    }

    /// The source ledger for one request, over the same ports and the same
    /// pages-rooted blob view — a source body is sealed exactly as a page
    /// body is, so a route this composition has rooted on the pages key
    /// space is a route that can open one.
    ///
    /// # Errors
    ///
    /// [`Problem::internal`] when the composition left a declared port
    /// unwired, exactly as [`Service::store`] does.
    pub(crate) fn sources_store(&self) -> Result<SourceStore, Problem> {
        Ok(SourceStore::new(
            port(self.ctx.ports.db.clone())?,
            port(self.ctx.ports.blob.clone())?,
            self.kms.clone(),
            port(self.ctx.ports.clock.clone())?,
            port(self.ctx.ports.id_gen.clone())?,
        ))
    }

    /// The asker the request's credential speaks for: the bearer token when
    /// the request carries an `Authorization` header, else the session
    /// cookie when the composition wired a [`SessionAuth`].
    ///
    /// **A request with an `Authorization` header is decided by it alone.**
    /// A rejected or malformed one is a 401 even when a cookie rides along —
    /// a caller that presented a header has said what it wants to be, the
    /// rule `livingbrain_tokens::authenticate` follows — so the CLI and MCP
    /// path is exactly what it was. The token is read the way the MCP server
    /// reads it (a mirror of `bearer` in `livingbrain-mcp/src/protocol.rs`).
    ///
    /// **The cookie is an ambient credential**, so a cookie-authenticated
    /// request whose method is not safe (`PUT`, `POST`, `DELETE`, …) must
    /// pass the same-origin check first; a bearer request never does, as it
    /// carries nothing a browser attaches on its own. Safe methods need no
    /// check: the harness's CORS layer never allows credentials, so another
    /// origin cannot read a cookie-authenticated answer, and every `/v1/*`
    /// answer is `Cache-Control: no-store`, so no cache can serve one to
    /// somebody else.
    ///
    /// A missing credential and a rejected one answer with the **same** 401
    /// problem — which of the two it was is not the caller's business.
    ///
    /// # Errors
    ///
    /// [`UNAUTHORIZED`] when no credential was presented, it was malformed,
    /// or it named no live asker; [`CROSS_SITE_REQUEST`] (403) for a
    /// cookie-authenticated write from another origin.
    pub(crate) async fn asker(
        &self,
        method: &Method,
        headers: &HeaderMap,
    ) -> Result<Asker, Problem> {
        let resolved = if headers.contains_key(header::AUTHORIZATION) {
            let Some(token) = bearer(headers) else {
                return Err(Problem::new(&UNAUTHORIZED));
            };
            self.auth.authenticate(&self.ctx.ports, token).await
        } else {
            let Some(session) = &self.session else {
                return Err(Problem::new(&UNAUTHORIZED));
            };
            if !method.is_safe() {
                session
                    .same_origin(headers)
                    .map_err(|detail| Problem::new(&CROSS_SITE_REQUEST).with_detail(detail))?;
            }
            session.session(&self.ctx.ports, headers).await
        };
        match resolved {
            // An asker with no workspace or no user id names nobody: the
            // credential is wrong, and the 401 says so without saying how.
            Ok(asker) if !asker.workspace_id.is_empty() && !asker.user_id.is_empty() => Ok(asker),
            _ => Err(Problem::new(&UNAUTHORIZED)),
        }
    }

    /// The page scopes the asker may read, most specific first. The only
    /// way a route names scopes, and the reason no route takes one from the
    /// client.
    pub(crate) fn scopes(&self, asker: &Asker) -> Vec<String> {
        page_scopes(asker)
    }
}

/// The `Authorization: Bearer …` value of a request, if it carries one.
///
/// Mirrored from `bearer` in `livingbrain-mcp/src/protocol.rs`, so both
/// surfaces read a credential the same way: scheme compared
/// case-insensitively, the token trimmed, an empty token read as absent.
pub(crate) fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}
