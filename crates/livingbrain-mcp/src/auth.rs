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
    /// The scope subset this credential was cut down to, in the access
    /// model's own vocabulary. `None` carries everything the member holds
    /// (issue #72). Filtered here, before [`page_scope`] hashes anything: a
    /// narrowed credential must not even be able to name the pages it lost.
    pub token_scopes: Option<Vec<String>>,
    /// `Some(owner)` when the credential is an agent account's key (ADR
    /// 0004): `user_id` is then the agent account's id, and `owner` the
    /// member it acts for. `None` is a member.
    pub agent_owner: Option<String>,
    /// The member's role, when the resolver knows it. `None` reads as an
    /// ordinary member, which is what every asker was before roles existed.
    pub role: Option<String>,
    /// The credential's public prefix (a token or key), or `None` for a
    /// session — for the audit trail, never for a decision.
    pub credential: Option<String>,
}

impl Asker {
    /// A member asking, with the credential's scope subset.
    #[must_use]
    pub fn member(
        workspace_id: impl Into<String>,
        user_id: impl Into<String>,
        token_scopes: Option<Vec<String>>,
    ) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            user_id: user_id.into(),
            token_scopes,
            agent_owner: None,
            role: None,
            credential: None,
        }
    }

    /// Whether an agent account is asking.
    #[must_use]
    pub fn is_agent(&self) -> bool {
        self.agent_owner.is_some()
    }

    /// The asker as the grants plan sees them (ADR 0004).
    #[must_use]
    pub fn principal(&self) -> livingbrain_pages::access::Principal {
        use livingbrain_pages::access::{Principal, PrincipalKind};
        Principal {
            workspace_id: self.workspace_id.clone(),
            id: self.user_id.clone(),
            kind: match &self.agent_owner {
                Some(owner) => PrincipalKind::Agent {
                    owner: owner.clone(),
                },
                None => PrincipalKind::Member {
                    role: self.role.clone().unwrap_or_else(|| "member".to_owned()),
                },
            },
            credential_scopes: self.token_scopes.clone(),
        }
    }

    /// The grants plan for this asker: their implicit reach and every live
    /// grant stored for them, read once (ADR 0004).
    ///
    /// # Errors
    ///
    /// The database's failure to read the grants; a caller answers it as a
    /// refusal, never as "no grants".
    pub async fn access(
        &self,
        db: &dyn cratefield_core::Database,
        now: time::OffsetDateTime,
    ) -> Result<livingbrain_pages::access::AccessPlan, cratefield_core::DbError> {
        livingbrain_pages::access::AccessPlan::load(db, &self.principal(), now).await
    }
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
    // An agent account has no memory of its own and no implicit reach: what
    // it may read is decided by the grants plan, never by this rule.
    if asker.is_agent() {
        return Vec::new();
    }
    let memberships = livingbrain_access::ChannelMemberships::new();
    let all = livingbrain_pages::page_scopes_for(
        &asker.workspace_id,
        &asker.user_id,
        Location::Dm,
        &memberships,
    );
    // Issue #72: a narrowed credential keeps only the scopes in its subset,
    // decided on the access model's own names before anything is hashed, and
    // the ordering stays the one `page_scopes_for` owns.
    let Some(subset) = asker.token_scopes.as_deref() else {
        return all;
    };
    let allowed: std::collections::HashSet<String> = livingbrain_access::scopes_for(
        &livingbrain_access::UserId::new(asker.user_id.clone()),
        Location::Dm,
        &memberships,
    )
    .scopes()
    .filter(|scope| subset.contains(&scope.to_string()))
    .map(|scope| page_scope(&asker.workspace_id, scope))
    .collect();
    all.into_iter()
        .filter(|scope| allowed.contains(scope))
        .collect()
}

/// The page-store scope one access scope maps to, inside `workspace_id`.
pub fn page_scope(workspace_id: &str, scope: &Scope) -> String {
    livingbrain_pages::page_scope(workspace_id, scope)
}

/// Records what an agent account did (ADR 0004): best effort, after the
/// fact, never a secret.
pub(crate) async fn audit_agent(ports: &Ports, asker: &Asker, action: &str, resource: &str) {
    let Some(owner) = asker.agent_owner.clone() else {
        return;
    };
    let (Some(db), Some(clock), Some(id_gen)) =
        (ports.db.clone(), ports.clock.clone(), ports.id_gen.clone())
    else {
        return;
    };
    let entry = cratefield_module_orgs::audit::AuditEntry {
        id: id_gen.ulid(),
        org_id: asker.workspace_id.clone(),
        actor_kind: "agent".to_owned(),
        actor_id: asker.user_id.clone(),
        on_behalf_of: Some(owner),
        credential: asker.credential.clone().unwrap_or_default(),
        action: action.to_owned(),
        resource: resource.to_owned(),
        detail: String::new(),
        at: String::new(),
    };
    let _ = cratefield_module_orgs::audit::record(&*db, &entry, clock.now()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asker(token_scopes: Option<Vec<String>>) -> Asker {
        Asker::member("T0SPACE", "U0OWNER", token_scopes)
    }

    /// The filter this crate owns — that a subset reaches no page outside
    /// itself. The intersection with the access model itself is held by
    /// `livingbrain-tokens`.
    #[test]
    fn a_narrowed_credential_reads_only_what_is_in_both_grants() {
        let all = page_scopes(&asker(None));
        assert!(all.len() > 1, "the member holds more than one: {all:?}");

        let narrowed = page_scopes(&asker(Some(vec!["shared".to_owned()])));
        assert_eq!(
            narrowed,
            vec![page_scope("T0SPACE", &Scope::Shared)],
            "one scope, its own hash"
        );
        assert!(all.len() > narrowed.len(), "and something was cut: {all:?}");
    }
}
