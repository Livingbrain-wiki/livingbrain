//! Who is asking, and which pages that asker may read.
//!
//! The [`BearerAuth`] seam turns an `Authorization: Bearer …` value into an
//! [`Asker`], and [`page_scopes`] is the only way this crate names a page
//! scope. Every tool reads through it and **no tool accepts a scope
//! argument**, so an agent cannot ask for a scope it was not granted.
//!
//! The naming rule itself lives in `livingbrain-pages` (issue #123), because
//! the Slack agent answers from the same pages and two copies of the rule
//! would be two answer keys. These two functions are the MCP server's own
//! access model — a direct message, no channel memberships — and they say so
//! by delegating.

use async_trait::async_trait;
use cratefield_core::Ports;
use livingbrain_access::{Location, Scope};

/// The person a credential speaks for, and the workspace they are in. The
/// ids are opaque and are only ever combined, never concatenated into
/// something a query takes apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asker {
    pub workspace_id: String,
    pub user_id: String,
}

/// Why a bearer value named no asker. Both variants get the same 401 and the
/// same body: distinguishing them would tell a stranger whether a token
/// exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// No `Authorization: Bearer …` header at all.
    NoCredential,
    /// A credential was presented and it spoke for no live user.
    Rejected,
}

/// Resolves a bearer credential to the asker it speaks for.
///
/// The ports arrive per request rather than being captured at construction:
/// an implementation resolves against the deployment's own database, clock
/// and signer, which a router cannot hold. The interim implementation is the
/// session cookie the web app already has; the OAuth server (issue #72)
/// replaces the implementation, not this trait.
///
/// # Errors
///
/// [`AuthError`] when the credential names no asker. An infrastructure
/// failure is reported as [`AuthError::Rejected`] too: a 401 says nothing
/// about why, and nothing here leaks whether a user exists.
#[async_trait]
pub trait BearerAuth: Send + Sync {
    async fn authenticate(&self, ports: &Ports, token: &str) -> Result<Asker, AuthError>;
}

/// The page scopes an asker may read, most specific first — its own memory
/// before the shared one, so a tool that stops at the first hit prefers a
/// private page to a shared one.
///
/// The grant is `scopes_for(asker, Location::Dm, …)`: the same rule Slack
/// asks with, so a person's private memory is the same set here as there.
pub fn page_scopes(asker: &Asker) -> Vec<String> {
    livingbrain_pages::page_scopes_for(
        &asker.workspace_id,
        &asker.user_id,
        Location::Dm,
        &livingbrain_access::ChannelMemberships::new(),
    )
}

/// The page-store scope one access scope maps to, inside `workspace_id`.
pub fn page_scope(workspace_id: &str, scope: &Scope) -> String {
    livingbrain_pages::page_scope(workspace_id, scope)
}
