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
use cratefield_core::{ModuleContext, Problem};
use cratefield_kms::Kms;
use livingbrain_mcp::{Asker, BearerAuth, SessionAuth};
use livingbrain_pages::access::AccessPlan;
use livingbrain_pages::{FactStore, PageStore, SourceStore};

use crate::problems::{CROSS_SITE_REQUEST, UNAUTHORIZED};
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
    /// The cookie resolver, when the composition wired one; `None` is a
    /// bearer-only surface.
    session: Option<Arc<dyn SessionAuth>>,
    /// How the compiler's triage classifier is built, when the composition
    /// wired one.
    jev: Option<Arc<dyn livingbrain_models::jev::JevConnector>>,
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
            jev: wiki.jev.clone(),
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

    /// The fact store for one request, over the same database and key
    /// custodian as the pages: a fact is sealed under the same scope key a
    /// page body is (`docs/temporal-facts.md` §6).
    ///
    /// # Errors
    ///
    /// [`Problem::internal`] when the composition left a declared port
    /// unwired, exactly as [`Service::store`] does.
    pub(crate) fn facts_store(&self) -> Result<FactStore, Problem> {
        Ok(FactStore::new(
            port(self.ctx.ports.db.clone())?,
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

    /// The deployment's public web-app origin every page link is built
    /// under ([`livingbrain_pages::app_base`]): one setting, read from this
    /// module's config, so search, ask, notes and the page routes all cite
    /// the same host the MCP server and the Slack agent do.
    pub(crate) fn base(&self) -> String {
        livingbrain_pages::app_base(self.ctx.config.as_ref())
    }

    /// This module's own context: its ports and its config.
    pub(crate) fn ctx(&self) -> &ModuleContext {
        &self.ctx
    }

    /// The page access this service was built with.
    pub(crate) fn wiki(&self) -> Wiki {
        Wiki {
            kms: self.kms.clone(),
            auth: self.auth.clone(),
            session: self.session.clone(),
            jev: self.jev.clone(),
        }
    }

    /// Records what an asker did in the audit log (ADR 0004): every write an
    /// agent account makes, and every export, are attributed — the agent,
    /// the member it acts for, the credential's prefix. Best effort: the
    /// action has already happened, and a lost line must not undo it.
    pub(crate) async fn audit(&self, asker: &Asker, action: &str, resource: &str) {
        let ports = &self.ctx.ports;
        let (Some(db), Some(clock), Some(id_gen)) =
            (ports.db.clone(), ports.clock.clone(), ports.id_gen.clone())
        else {
            return;
        };
        let entry = cratefield_module_orgs::audit::AuditEntry {
            id: id_gen.ulid(),
            org_id: asker.workspace_id.clone(),
            actor_kind: if asker.is_agent() { "agent" } else { "member" }.to_owned(),
            actor_id: asker.user_id.clone(),
            on_behalf_of: asker.agent_owner.clone(),
            credential: asker
                .credential
                .clone()
                .unwrap_or_else(|| "session".to_owned()),
            action: action.to_owned(),
            resource: resource.to_owned(),
            detail: String::new(),
            at: String::new(),
        };
        if cratefield_module_orgs::audit::record(&*db, &entry, clock.now())
            .await
            .is_err()
        {
            eprintln!("an audit entry could not be written: {action}");
        }
    }

    /// Refuses an agent account (ADR 0004) on a route that acts for a
    /// member: their own compile queue and jobs, the workspace's settings.
    /// Such a route has nothing a grant could open to an agent.
    ///
    /// # Errors
    ///
    /// [`AGENT_NOT_ALLOWED`](crate::problems::AGENT_NOT_ALLOWED) for an agent.
    pub(crate) fn member_only(asker: &Asker) -> Result<(), Problem> {
        if asker.is_agent() {
            return Err(Problem::new(&crate::problems::AGENT_NOT_ALLOWED));
        }
        Ok(())
    }

    /// What the asker may read and write (ADR 0004): their implicit reach
    /// and every live grant stored for them, read once per request. The only
    /// way a route names scopes, and the reason no route takes one from the
    /// client.
    ///
    /// # Errors
    ///
    /// [`Problem::internal`] when the grants cannot be read: no plan is a
    /// refusal, never an empty grant list.
    pub(crate) async fn access(&self, asker: &Asker) -> Result<AccessPlan, Problem> {
        let db = port(self.ctx.ports.db.clone())?;
        let clock = port(self.ctx.ports.clock.clone())?;
        asker
            .access(&*db, clock.now())
            .await
            .map_err(|_| Problem::internal())
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
