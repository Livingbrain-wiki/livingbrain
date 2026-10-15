//! Personal access tokens, and the OAuth discovery documents that point at
//! them (issue #72).
//!
//! A token is returned exactly once, at creation. The row holds only the
//! SHA-256 of the whole token, compared in constant time on every request;
//! only the public prefix is ever shown again, which is what a settings list
//! needs so a person can revoke one.
//!
//! [`authenticate`] is the one entry point: `Authorization: Bearer <token>`,
//! falling back to the session cookie through the same
//! [`livingbrain_workspaces::caller`] the workspaces routes use, so there is
//! one implementation of "who is calling". Every refusal — no credential,
//! malformed, unknown, revoked, member gone — is the same 401, because
//! telling a caller *why* would tell a stranger whether a token exists.

#![forbid(unsafe_code)]

mod avatars;
mod device;
mod handlers;
mod me;
mod store;

use std::sync::Arc;

use cratefield_core::axum::http::uri::Authority;
use cratefield_core::axum::http::{HeaderMap, Uri, header};
use cratefield_core::{
    Config, ConfigError, DataKind, Database, DbError, Disposition, Migrations, Module,
    ModuleContext, PersonalDataSet, Port, Ports, SqlMigration, origin_of,
};
use livingbrain_access::{Location, Scope, scopes_for};

pub use avatars::Avatars;
pub use device::{DevicePorts, PatIssuer, SessionApprover};
pub use me::Me;

const MIGRATION_INIT: SqlMigration = SqlMigration::new(
    "0001",
    "init",
    include_str!("../migrations/sqlite/0001_init.sql"),
);

/// The same schema for Postgres, so the parity job applies a migration rather
/// than falling back to the sqlite one. Nothing in it is dialect-specific.
const MIGRATION_INIT_POSTGRES: SqlMigration = SqlMigration::new(
    "0001",
    "init",
    include_str!("../migrations/postgres/0001_init.sql"),
);

/// RFC 8628 §3.4. The one grant this venture's authorization server
/// implements — hence no authorization endpoint and no redirect flow.
pub const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Where the device grant is mounted, named by the discovery document.
pub const DEVICE_AUTH_PATH: &str = "/v1/device-auth";

/// The MCP endpoint, the protected resource the discovery document names.
pub const MCP_PATH: &str = "/v1/pages/mcp";

// ---------------------------------------------------------------------------
// The caller

/// Who is calling, and what that credential may reach.
///
/// `Debug` is hand-written: handlers log this, and there is no token *field*
/// to print in the first place — the value is gone by the time a `Member`
/// exists. A derive would one day print whatever is added.
#[derive(Clone, PartialEq, Eq)]
pub struct Member {
    pub workspace_id: String,
    pub user_id: String,
    pub is_admin: bool,
    /// The member's role (ADR 0004).
    pub role: livingbrain_workspaces::Role,
    /// The scope subset this credential was cut down to, or `None` for one
    /// that carries everything its member holds.
    pub scopes: Option<Vec<String>>,
    /// The token's public prefix, or `None` for the session cookie.
    pub credential: Option<String>,
}

impl std::fmt::Debug for Member {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Member")
            .field("workspace_id", &self.workspace_id)
            .field("user_id", &self.user_id)
            .field("is_admin", &self.is_admin)
            .field("role", &self.role)
            .field("credential", &self.credential)
            .field(
                "scopes",
                &self
                    .scopes
                    .as_ref()
                    .map_or_else(|| "all".to_owned(), |scopes| scopes.join(" ")),
            )
            .finish()
    }
}

impl Member {
    /// The scopes this caller may read in `location`: the grant
    /// [`scopes_for`] decides, cut down to the credential's subset. A subset
    /// reads only the **intersection**, so a token can never widen what the
    /// access model already refused.
    #[must_use]
    pub fn read_scopes<M: livingbrain_access::MembershipView + ?Sized>(
        &self,
        location: Location,
        memberships: &M,
    ) -> Vec<Scope> {
        let held = scopes_for(
            &livingbrain_access::UserId::new(self.user_id.clone()),
            location,
            memberships,
        );
        match self.scopes.as_deref() {
            None => held.scopes().cloned().collect(),
            Some(subset) => held
                .scopes()
                .filter(|scope| subset.contains(&scope.to_string()))
                .cloned()
                .collect(),
        }
    }
}

/// Why a credential named no caller. One type, one 401, so an unknown token,
/// a revoked one, a departed member and no credential at all are one answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthError;

impl AuthError {
    #[must_use]
    pub fn problem() -> cratefield_core::Problem {
        handlers::unauthorized()
    }
}

impl From<AuthError> for cratefield_core::Problem {
    fn from(_error: AuthError) -> Self {
        AuthError::problem()
    }
}

fn port<T: ?Sized>(slot: Option<Arc<T>>) -> Result<Arc<T>, AuthError> {
    slot.ok_or(AuthError)
}

fn database(_error: DbError) -> AuthError {
    AuthError
}

/// Resolves the caller from `Authorization: Bearer <token>`, falling back to
/// the session cookie. The bearer is checked first: a request that presents
/// one has said what it wants to be.
pub async fn authenticate(ports: &Ports, headers: &HeaderMap) -> Result<Member, AuthError> {
    let db = port(ports.db.clone())?;
    if let Some(token) = cratefield_core::bearer_token(headers) {
        let clock = port(ports.clock.clone())?;
        let Some(principal) = store::Store::new(db.clone(), clock)
            .verify(token)
            .await
            .map_err(database)?
        else {
            return Err(AuthError);
        };
        return member_for(db, principal).await;
    }
    session(ports, headers).await
}

/// The caller from the session cookie **only** — never from a bearer. A
/// token that could mint a token is a credential factory, so `POST
/// /v1/tokens` uses this instead of [`authenticate`].
pub async fn session(ports: &Ports, headers: &HeaderMap) -> Result<Member, AuthError> {
    let (signer, clock, db) = (
        port(ports.signer.clone())?,
        port(ports.clock.clone())?,
        port(ports.db.clone())?,
    );
    let caller = livingbrain_workspaces::caller(&*signer, &*clock, &*db, headers)
        .await
        .map_err(|_| AuthError)?;
    Ok(Member {
        workspace_id: caller.workspace_id,
        user_id: caller.user_id,
        is_admin: caller.is_admin,
        role: caller.role,
        scopes: None,
        credential: None,
    })
}

/// A verified token's subject, resolved to a member — and refused when the
/// member row is gone, so a token stops working the moment the membership it
/// was minted for does.
async fn member_for(
    db: Arc<dyn Database>,
    principal: store::Principal,
) -> Result<Member, AuthError> {
    let caller =
        livingbrain_workspaces::caller_for(&*db, &principal.workspace_id, &principal.user_id)
            .await
            .map_err(database)?
            .ok_or(AuthError)?;
    Ok(Member {
        workspace_id: caller.workspace_id,
        user_id: caller.user_id,
        // A narrowed token is not admin: a scope subset must not carry
        // workspace-wide rights the subset does not describe.
        is_admin: principal.scopes.is_none() && caller.is_admin,
        role: caller.role,
        scopes: principal.scopes,
        credential: Some(principal.prefix),
    })
}

/// Revokes every live token `user_id` holds in `workspace_id` — what
/// removing a member does (ADR 0004), so a person who is later re-admitted
/// with the same member id does not find their old tokens working again.
/// Answers how many were revoked.
///
/// # Errors
///
/// [`DbError`] when the update fails.
pub async fn revoke_member_tokens(
    db: &dyn Database,
    workspace_id: &str,
    user_id: &str,
    now: &str,
) -> Result<u64, DbError> {
    db.execute(&cratefield_core::Statement::with_values(
        "UPDATE personal_access_tokens SET revoked_at = ? \
          WHERE workspace_id = ? AND user_id = ? AND revoked_at IS NULL",
        vec![now.into(), workspace_id.into(), user_id.into()],
    ))
    .await
}

// ---------------------------------------------------------------------------
// The module

/// Personal access tokens, and the discovery documents that point at the
/// device grant that mints them.
#[derive(Debug, Default)]
pub struct Tokens {
    /// The origin the discovery documents fall back to for a request that
    /// names no host, or `None` to serve no document in that case.
    api_base: Option<String>,
    device: Option<DevicePorts>,
}

impl Tokens {
    /// A module whose discovery documents answer from the request, and with
    /// no device grant hooks.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The origin the discovery documents fall back to when a request names
    /// no host. Normally they answer from the request instead, which is what
    /// keeps one Worker correct across every hostname it fronts; this is for
    /// a proxy that strips `Host` and the request URI, where a fixed origin
    /// is the only one that can be right.
    #[must_use]
    pub fn api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = Some(base.into().trim_end_matches('/').to_owned());
        self
    }

    /// The handle the device grant's `SessionApprover` and `PatIssuer` are
    /// built from, and the one this module publishes its ports to.
    #[must_use]
    pub fn device_ports(mut self, ports: DevicePorts) -> Self {
        self.device = Some(ports);
        self
    }
}

impl Module for Tokens {
    fn name(&self) -> &'static str {
        "tokens"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// `Db` holds the tokens and the member rows they name, `Signer`
    /// verifies the session cookie, `Clock` stamps creation and revocation.
    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Signer, Port::Clock]
    }

    fn tables(&self) -> &'static [&'static str] {
        &[store::TABLE]
    }

    /// A token is a credential and its row is somebody's: erasing a member's
    /// rows deletes every machine access they had.
    fn personal_data(&self) -> &'static [PersonalDataSet] {
        const SETS: &[PersonalDataSet] = &[PersonalDataSet {
            table: store::TABLE,
            subject: "user_id",
            kind: DataKind::Identifier,
            disposition: Disposition::Erase,
            description: "For each personal access token a member minted: the workspace it \
                          belongs to, the label they gave it, the scope subset it was cut down \
                          to, when it was created and when it was revoked. The token itself is \
                          held only as a SHA-256 hash, so a row cannot be replayed as a \
                          credential; `secret_hash` is redacted for the same reason.",
            redacted: &["secret_hash"],
            subject_via: None,
        }];
        SETS
    }

    fn validate_config(&self, _cfg: &dyn Config) -> Result<(), ConfigError> {
        Ok(())
    }

    fn migrations(&self) -> Migrations {
        const MIGRATIONS: [SqlMigration; 1] = [MIGRATION_INIT];
        const _: () = cratefield_core::assert_migration_set(&MIGRATIONS);
        const MIGRATIONS_POSTGRES: [SqlMigration; 1] = [MIGRATION_INIT_POSTGRES];
        const _: () = cratefield_core::assert_migration_set(&MIGRATIONS_POSTGRES);
        Migrations {
            sqlite: &MIGRATIONS,
            postgres: &MIGRATIONS_POSTGRES,
        }
    }

    /// The two discovery documents, and nothing else: discovery URLs are a
    /// singleton namespace across the harness, so this module's `Some` is why
    /// no sibling may also return one.
    ///
    /// Both documents are written under the origin the *request* reached, so
    /// a Worker serving `api.`, `staging-api.` and `mcp.` answers each with
    /// its own name — which RFC 8414 requires of `issuer` and RFC 9728 gets
    /// a client to expect of `resource`. [`Tokens::api_base`] is only the
    /// fallback for a request that names no host at all.
    fn well_known(&self) -> Option<cratefield_core::axum::Router> {
        Some(handlers::discovery(self.api_base.as_deref()))
    }

    fn router(&self, ctx: ModuleContext) -> cratefield_core::axum::Router {
        // The grant's hooks were built at composition time, before any port
        // existed. This is the first moment they can: the harness builds
        // every module's router from one `Ports` before it serves anything.
        if let Some(device) = &self.device {
            device.publish(&ctx.ports);
        }
        let (Some(db), Some(clock)) = (ctx.ports.db.clone(), ctx.ports.clock.clone()) else {
            // `Harness::build` refuses a composition missing a required port,
            // so this is unreachable in a serving harness; the module still
            // answers rather than panicking if one is absent.
            return cratefield_core::axum::Router::new();
        };
        handlers::router(ctx, store::Store::new(db, clock))
    }
}

// ---------------------------------------------------------------------------
// Helpers shared with the handlers

/// The origin this request reached: the `Host` header when it carries one,
/// else the URI's own authority, under `x-forwarded-proto` (the scheme a
/// proxy terminated with) and otherwise `https`. This is the same
/// resolution `cratefield-module-device-auth` does for its `own_host`, so
/// one Worker answers every hostname it fronts with that hostname's origin.
/// The MCP 401 challenge resolves its `resource_metadata` the same way, so a
/// client that follows it stays on the host it was talking to.
pub fn origin_of_request(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .filter(|host| !host.is_empty())
        .or_else(|| uri.authority().map(Authority::as_str))?;
    let scheme = uri
        .scheme_str()
        .filter(|scheme| matches!(*scheme, "http" | "https"))
        .or_else(|| {
            headers
                .get("x-forwarded-proto")
                .and_then(|value| value.to_str().ok())
                .map(|scheme| scheme.split(',').next().unwrap_or(scheme).trim())
                .filter(|scheme| matches!(*scheme, "http" | "https"))
        })
        .unwrap_or("https");
    origin_of(&format!("{scheme}://{host}")).ok()
}

/// Why a request was refused as cross-site by [`same_origin`]: the detail a
/// problem body carries, saying which browser signal failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossSite {
    pub detail: String,
}

/// The same-origin rule for a **cookie-authenticated write** — one
/// implementation, shared by this module's own token routes and by every
/// other surface whose writes accept the session cookie (the wiki routes in
/// `livingbrain-api`, through the venture's session seam), so the rule
/// cannot drift between them.
///
/// `sec-fetch-site` is checked first — `same-origin` and `none` pass and
/// everything else, including `same-site`, is refused, because a sibling
/// subdomain can post here — then `Origin`, when present, must be this
/// request's own origin, compared with `origin_of` as RFC 6454 origins
/// rather than as strings (so `Origin: null` is refused). A request carrying
/// neither header is not a browser, so it carries no ambient credential a
/// stranger could borrow, and is accepted.
///
/// # Errors
///
/// [`CrossSite`] when either browser signal names another site.
pub fn same_origin(headers: &HeaderMap) -> Result<(), CrossSite> {
    let refused = |detail: String| CrossSite { detail };
    if let Some(site) = headers.get("sec-fetch-site") {
        let site = site.to_str().unwrap_or_default().to_ascii_lowercase();
        if !matches!(site.as_str(), "same-origin" | "none") {
            return Err(refused(format!(
                "sec-fetch-site is {site}; only same-origin or none is accepted"
            )));
        }
    }
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Ok(());
    };
    let presented = origin
        .to_str()
        .map_err(|_| refused("origin was not valid UTF-8".to_owned()))?;
    let presented = origin_of(presented)
        .map_err(|err| refused(format!("origin {presented:?} is not an origin: {err}")))?;
    let own = own_origin(headers)
        .ok_or_else(|| refused("origin was present but the request named no host".to_owned()))?;
    if presented != own {
        return Err(refused(format!(
            "origin {presented} does not match the host this request reached ({own})"
        )));
    }
    Ok(())
}

/// The host a same-origin check compares an `Origin` against.
fn own_origin(headers: &HeaderMap) -> Option<String> {
    origin_of_request(headers, &Uri::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                cratefield_core::axum::http::HeaderName::from_bytes(name.as_bytes())
                    .expect("a header name"),
                cratefield_core::axum::http::HeaderValue::from_str(value).expect("a value"),
            );
        }
        headers
    }

    /// The documents name the host the request reached, so staging, `wrangler
    /// dev` and the MCP hostname each answer for themselves rather than for
    /// whichever deployment compiled the binary.
    #[test]
    fn a_request_names_its_own_origin() {
        let staging = headers(&[("host", "staging-api.livingbrain.wiki")]);
        assert_eq!(
            origin_of_request(&staging, &Uri::default()).as_deref(),
            Some("https://staging-api.livingbrain.wiki")
        );
        let local = headers(&[("host", "localhost:8787"), ("x-forwarded-proto", "http")]);
        assert_eq!(
            origin_of_request(&local, &Uri::default()).as_deref(),
            Some("http://localhost:8787"),
            "a non-default port stays, and `wrangler dev` is not https"
        );
    }

    /// An absolute request URI stands in for a `Host` header the proxy
    /// dropped, which is what a Worker behind one sees.
    #[test]
    fn a_uri_authority_stands_in_for_a_missing_host() {
        let uri: Uri = "https://mcp.livingbrain.wiki/v1/pages/mcp"
            .parse()
            .expect("an absolute URI");
        assert_eq!(
            origin_of_request(&HeaderMap::new(), &uri).as_deref(),
            Some("https://mcp.livingbrain.wiki")
        );
        assert_eq!(origin_of_request(&HeaderMap::new(), &Uri::default()), None);
    }

    #[test]
    fn a_module_always_serves_a_discovery_document() {
        assert!(Tokens::new().well_known().is_some());
        assert!(
            Tokens::new()
                .api_base("https://api.example.test")
                .well_known()
                .is_some()
        );
    }
}
