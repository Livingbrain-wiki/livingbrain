//! Per-request plumbing shared by every route in this crate: the page store
//! over the module's own blob view, the asker behind the bearer token, and
//! the scopes that asker may read. One [`Service`] per module router, built
//! once from the module context and the injected [`Wiki`].
//!
//! The blob the store is built over is `ctx.ports.blob` — **this module's
//! own per-request view**. For the four sibling modules the composition has
//! rooted that view on the pages key space
//! ([`RebindBlob`](crate::RebindBlob) planted under the scope the harness
//! applies), so the same expression serves the merged pages surface, whose
//! view is pages-rooted because the module *is* pages. Either way the
//! handle was derived for this request; nothing here outlives it.

use std::sync::Arc;

use cratefield_core::axum::http::{HeaderMap, header};
use cratefield_core::{ModuleContext, Problem};
use cratefield_kms::Kms;
use livingbrain_mcp::{Asker, BearerAuth, page_scopes};
use livingbrain_pages::PageStore;

use crate::problems::UNAUTHORIZED;
use crate::{Wiki, port};

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
}

impl Service {
    /// The service for one of the four sibling modules: the ports come from
    /// the module context, the custodian and credential resolver from the
    /// injected [`Wiki`].
    pub(crate) fn from_wiki(ctx: ModuleContext, wiki: &Wiki) -> Self {
        Self::new(Arc::new(ctx), wiki.kms.clone(), wiki.auth.clone())
    }

    /// The service for the merged pages surface. It runs *inside* the pages
    /// module, so the blob it reads through is the module's own port view —
    /// already scoped to `pages` by the harness, which is the same view the
    /// page store itself is built on.
    pub(crate) fn for_pages(
        ctx: Arc<ModuleContext>,
        kms: Arc<dyn Kms>,
        auth: Arc<dyn BearerAuth>,
    ) -> Self {
        Self::new(ctx, kms, auth)
    }

    fn new(ctx: Arc<ModuleContext>, kms: Arc<dyn Kms>, auth: Arc<dyn BearerAuth>) -> Self {
        Self { ctx, kms, auth }
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

    /// The asker the request's bearer credential speaks for.
    ///
    /// The token is read the way the MCP server reads it (a mirror of
    /// `bearer` in `livingbrain-mcp/src/protocol.rs`), and a missing token
    /// and a rejected one answer with the **same** 401 problem — which of
    /// the two it was is not the caller's business.
    ///
    /// # Errors
    ///
    /// [`UNAUTHORIZED`] when no credential was presented, it was malformed,
    /// or it named no live asker.
    pub(crate) async fn asker(&self, headers: &HeaderMap) -> Result<Asker, Problem> {
        let Some(token) = bearer(headers) else {
            return Err(Problem::new(&UNAUTHORIZED));
        };
        match self.auth.authenticate(&self.ctx.ports, token).await {
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
