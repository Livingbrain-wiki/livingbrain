//! What the three modules share: who is calling, the problems they answer
//! with, and the ports.

use std::sync::Arc;

use cratefield_core::axum::http::{HeaderMap, Method, StatusCode};
use cratefield_core::{Clock, Database, IdGen, ModuleContext, Problem, ProblemDef};
use livingbrain_tokens::Member;
use time::OffsetDateTime;

/// No session, or one that names nobody.
pub(crate) const UNAUTHORIZED: ProblemDef = ProblemDef {
    slug: "accounts/unauthorized",
    status: StatusCode::UNAUTHORIZED,
    title: "Sign in to manage access",
    description: "Agent accounts, grants and the audit log are managed from a signed-in \
                  browser session.",
};

/// A cookie write from another site.
pub(crate) const CROSS_SITE: ProblemDef = ProblemDef {
    slug: "accounts/cross-site-request",
    status: StatusCode::FORBIDDEN,
    title: "This request came from another site",
    description: "Access is managed only from this site's own pages.",
};

/// The caller's role, or the grants they hold, do not allow this.
pub(crate) const FORBIDDEN: ProblemDef = ProblemDef {
    slug: "accounts/forbidden",
    status: StatusCode::FORBIDDEN,
    title: "You can't do that here",
    description: "Ask an owner or admin of the workspace, or the member whose space it is.",
};

/// Nothing by that id that the caller may see.
pub(crate) const NOT_FOUND: ProblemDef = ProblemDef {
    slug: "accounts/not-found",
    status: StatusCode::NOT_FOUND,
    title: "Not found",
    description: "There is no such agent account, key or grant that you can see.",
};

/// A field outside its rules.
pub(crate) const INVALID: ProblemDef = ProblemDef {
    slug: "accounts/invalid",
    status: StatusCode::UNPROCESSABLE_ENTITY,
    title: "That request is not valid",
    description: "The detail says which field.",
};

pub(crate) fn invalid(detail: impl Into<String>) -> Problem {
    Problem::new(&INVALID).with_detail(detail.into())
}

pub(crate) fn forbidden() -> Problem {
    Problem::new(&FORBIDDEN)
}

pub(crate) fn not_found() -> Problem {
    Problem::new(&NOT_FOUND)
}

/// A failed statement: a 500 that says nothing about the database.
pub(crate) fn database<E>(_error: E) -> Problem {
    Problem::internal()
}

/// A port the module declared and the runtime did not supply.
pub(crate) fn port<T: ?Sized>(slot: Option<Arc<T>>) -> Result<Arc<T>, Problem> {
    slot.ok_or_else(Problem::internal)
}

/// The per-router state: the module's context.
pub(crate) struct State {
    pub(crate) ctx: ModuleContext,
}

impl State {
    pub(crate) fn db(&self) -> Result<Arc<dyn Database>, Problem> {
        port(self.ctx.ports.db.clone())
    }

    pub(crate) fn now(&self) -> Result<OffsetDateTime, Problem> {
        let clock: Arc<dyn Clock> = port(self.ctx.ports.clock.clone())?;
        Ok(clock.now())
    }

    pub(crate) fn id(&self, prefix: &str) -> Result<String, Problem> {
        let id_gen: Arc<dyn IdGen> = port(self.ctx.ports.id_gen.clone())?;
        Ok(format!("{prefix}{}", id_gen.ulid().to_ascii_lowercase()))
    }

    /// The signed-in member, from the session cookie **only**. A write must
    /// also come from this site's own pages.
    pub(crate) async fn caller(
        &self,
        method: &Method,
        headers: &HeaderMap,
    ) -> Result<Member, Problem> {
        if !method.is_safe() {
            livingbrain_tokens::same_origin(headers)
                .map_err(|refused| Problem::new(&CROSS_SITE).with_detail(refused.detail))?;
        }
        livingbrain_tokens::session(&self.ctx.ports, headers)
            .await
            .map_err(|_| Problem::new(&UNAUTHORIZED))
    }
}

/// The member as the grants plan sees them: a session, so no narrowing.
pub(crate) fn principal_of(member: &Member) -> livingbrain_pages::access::Principal {
    livingbrain_pages::access::Principal {
        workspace_id: member.workspace_id.clone(),
        id: member.user_id.clone(),
        kind: livingbrain_pages::access::PrincipalKind::Member {
            role: member.role.as_str().to_owned(),
        },
        credential_scopes: None,
    }
}
