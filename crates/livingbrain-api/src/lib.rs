//! `livingbrain-api`: the HTTP surface for first-party clients (issue #121).
//!
//! The routes the livingbrain-cli's client contract already speaks —
//! `POST /v1/notes`, `GET /v1/search`, `POST /v1/ask`, `GET /v1/export` —
//! plus [`pages_routes`], the page routes the web app reads and writes,
//! mounted by the venture inside the `pages` module at `/v1/pages`. One
//! contract, served from one store: every route reads and writes
//! `livingbrain-pages` and **only with the asker's own access** — the scopes
//! come from the credential ([`livingbrain_mcp::page_scopes`]) and no route
//! accepts one from the client, so a caller can never name a scope it was
//! not granted.
//!
//! ## How a sibling module reaches the pages key space
//!
//! The harness scopes the `Blob` port per module name, so a module mounted
//! at `/v1/notes` sees only `notes/` keys — it could never open a body at
//! `pages/{scope}/{slug}/{version}-….md`, which is where the page store
//! puts them. That is the right default, and it is not worked around per
//! request: instead the **composition** roots each module's own blob view
//! on the pages key space, by planting [`PagesRebind`] at the port layer,
//! over the raw store the runtime resolved for this request —
//!
//! ```text
//! ports.blob = Some(Arc::new(PagesRebind::new(raw_store)));
//! ```
//!
//! — so the module's `ctx.ports.blob` reads and writes `pages/` objects
//! through a handle derived for **this** request. That per-request
//! derivation is the point, not a style choice: workerd ties an
//! env-derived I/O object to the request context that created it, so a
//! pages blob injected at compose time (the `Wiki { blob, … }` this crate
//! once carried) is a handle from some earlier request, and reusing it on
//! a later one fails with `Cannot perform I/O on behalf of a different
//! request context`. The runtime re-derives every binding per request
//! (`ports()` inside `serve`); the blob travels the same road.
//!
//! What a module itself is **constructed** with is a [`Wiki`] — a key
//! custodian and a credential resolver. This is the same shape the shared
//! key custodian already travels by: `cratefield-core` has no `Kms` port
//! either, so the composition hands one to each module beside the context
//! (issue #43).
//!
//! ## Who is asking
//!
//! Authentication is issue #72's seam, not this crate's invention:
//! [`livingbrain_mcp::BearerAuth`] resolves the `Authorization: Bearer …`
//! value to an [`livingbrain_mcp::Asker`], and the OAuth server that issue
//! #72 describes replaces the implementation, not the trait. Every route
//! answers a missing or rejected credential with **one** 401 problem body —
//! the two cases are indistinguishable from the outside, exactly as the
//! MCP server answers.

#![forbid(unsafe_code)]

mod ask;
mod export;
mod notes;
mod pages;
mod problems;
mod rebind;
mod recipes;
mod search;
mod service;
mod zip;

pub use ask::Ask;
pub use export::Export;
pub use notes::Notes;
pub use pages::pages_routes;
pub use rebind::{PagesRebind, RebindBlob};
pub use search::Search;

use cratefield_core::Problem;
use cratefield_kms::Kms;
use livingbrain_mcp::BearerAuth;
use std::sync::Arc;

/// The page access a module in this crate is constructed with.
///
/// Two things a route needs that the ports do not carry, in the order the
/// venture wires them:
///
/// - `kms` — the key custodian the page store seals and opens bodies with.
///   One `Arc` for every module in the composition, exactly as the pages
///   module itself is handed one.
/// - `auth` — the bearer-to-asker resolver, issue #72's seam. The OAuth
///   server replaces the implementation; these routes do not change.
///
/// The pages blob is deliberately **not** one of them: it arrives per
/// request through the module's own `ctx.ports.blob`, rooted on the pages
/// key space by the composition's [`RebindBlob`] (see the crate docs for
/// why a compose-time handle cannot be reused across requests on workerd).
///
/// `Clone`, because one `Wiki` is shared by the four modules of one
/// venture: the fields are `Arc`s, so the clones are views on the same
/// custodian and credential resolver.
#[derive(Clone)]
pub struct Wiki {
    /// The key custodian page bodies are sealed and opened with.
    pub kms: Arc<dyn Kms>,
    /// Resolves a bearer credential to the asker it speaks for.
    pub auth: Arc<dyn BearerAuth>,
}

impl Wiki {
    /// A `Wiki` over one key custodian and one credential resolver — the
    /// `Arc`s the composition already holds for the pages module.
    #[must_use]
    pub fn new(kms: Arc<dyn Kms>, auth: Arc<dyn BearerAuth>) -> Self {
        Self { kms, auth }
    }
}

/// A port a route declared and the runtime did not supply, as a problem.
///
/// The routes declare `Db`, `Clock` and `IdGen`, so a `None` here is a
/// composition bug, not a caller's error: it answers 500 and says nothing
/// about which port was missing, the same way `livingbrain-mcp` and
/// `livingbrain-models` answer it.
pub(crate) fn port<T: ?Sized>(slot: Option<Arc<T>>) -> Result<Arc<T>, Problem> {
    slot.ok_or_else(Problem::internal)
}
