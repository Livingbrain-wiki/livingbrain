//! The module's routes at `/v1/tokens`, the two `/.well-known` documents,
//! and the two hooks RFC 8628 leaves to a venture.
//!
//! One rule shapes every route: a token is returned exactly once. `POST`
//! answers the value; the row keeps only its SHA-256. `GET` and `DELETE`
//! work from the public prefix, the only part ever shown again, so no route
//! reads a token back and there is nothing to leak.
//!
//! Both writes are **cookie** routes — `POST` is by construction
//! ([`crate::session`] refuses a bearer) — so both check the same-origin
//! signals a browser sends, as `cratefield-module-device-auth` does for its
//! approval forms: a cookie-authenticated write from another origin is the
//! one request shape here a stranger could make a signed-in person send.

use std::sync::Arc;

use cratefield_core::axum::extract::{OriginalUri, Path, State};
use cratefield_core::axum::http::{HeaderMap, StatusCode, Uri, header};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::axum::routing::get;
use cratefield_core::axum::{self, Json};
use cratefield_core::{ModuleContext, Problem, ProblemDef, origin_of};
use livingbrain_access::{ChannelMemberships, Location, Scope, UserId, scopes_for};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::store::{Store, Summary};
use crate::{DEVICE_AUTH_PATH, DEVICE_CODE_GRANT, MCP_PATH, Member};

/// No credential, a malformed one, an unknown one, a revoked one, and one
/// whose member has left the workspace are all this one problem.
const UNAUTHORIZED: ProblemDef = ProblemDef {
    slug: "tokens/unauthorized",
    status: StatusCode::UNAUTHORIZED,
    title: "Sign in, or present a personal access token",
    description: "This route answers only to a signed-in person or to a bearer token minted \
                  for one. Every refusal is this one answer, so nothing here reveals whether a \
                  given token exists.",
};

/// A creation asked for a scope its creator does not hold: you cannot hand
/// out a key to a door you cannot open.
const SCOPE_NOT_HELD: ProblemDef = ProblemDef {
    slug: "tokens/scope-not-held",
    status: StatusCode::FORBIDDEN,
    title: "A token cannot be granted more than you hold",
    description: "The requested scope is not one of the scopes the caller holds, so minting \
                  this token would hand out access the caller does not have themselves.",
};

/// The prefix named no token of the caller's own.
const NO_SUCH_TOKEN: ProblemDef = ProblemDef {
    slug: "tokens/not-found",
    status: StatusCode::NOT_FOUND,
    title: "No such token",
    description: "No personal access token of the caller's own carries this prefix.",
};

const CROSS_SITE_REQUEST: ProblemDef = ProblemDef {
    slug: "tokens/cross-site-request",
    status: StatusCode::FORBIDDEN,
    title: "A same-origin request is required",
    description: "Creating or revoking a token is a browser action, and accepts only a request \
                  a browser reports as coming from this venture's own origin.",
};

pub(crate) fn unauthorized() -> Problem {
    Problem::new(&UNAUTHORIZED)
}

/// A failed statement. The adapter's message never reaches the body.
fn database(_error: cratefield_core::DbError) -> Problem {
    Problem::internal()
}

struct ModuleState {
    ctx: ModuleContext,
    store: Store,
}

pub(crate) fn router(ctx: ModuleContext, store: Store) -> axum::Router {
    let state = Arc::new(ModuleState { ctx, store });
    axum::Router::new()
        .route("/", get(list).post(create))
        .route("/{prefix}", axum::routing::delete(revoke))
        .with_state(state)
}

#[derive(Debug, Deserialize)]
struct CreateRequest {
    /// Required: a list of prefixes is not something a person can decide to
    /// revoke from.
    name: String,
    /// The scope subset, in the access model's vocabulary. Absent means
    /// "everything I hold".
    #[serde(default)]
    scopes: Vec<String>,
}

/// The one type here whose fields can spell a live bearer credential, so its
/// `Debug` is hand-written: a derive is one `?view` in a log line away from
/// printing it.
#[derive(Serialize)]
struct CreatedView {
    token: String,
    prefix: String,
    name: String,
    /// `None` serializes as `null`, which is what "no subset" means; an
    /// empty array would say the token can read nothing at all.
    scopes: Option<Vec<String>>,
    created_at: String,
}

impl std::fmt::Debug for CreatedView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreatedView")
            .field("token", &"<withheld>")
            .field("prefix", &self.prefix)
            .field("name", &self.name)
            .field(
                "scopes",
                &self
                    .scopes
                    .as_ref()
                    .map_or_else(|| "all".to_owned(), |scopes| scopes.join(" ")),
            )
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// A label, not a document; it is rendered into a settings list.
const MAX_NAME_CHARS: usize = 64;

async fn create(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
    Json(body): Json<CreateRequest>,
) -> Result<Response, Problem> {
    require_same_origin(&headers)?;
    let member = crate::session(&state.ctx.ports, &headers).await?;
    let name = body.name.trim().to_owned();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(Problem::validation_failed(format!(
            "name must be 1 to {MAX_NAME_CHARS} characters"
        )));
    }
    let scopes = checked_scopes(&member, &body.scopes)?;
    let minted = state
        .store
        .mint(&member.workspace_id, &member.user_id, &name, &scopes)
        .await
        .map_err(database)?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedView {
            // The only time this value exists outside the browser about to
            // receive it. Nothing logs it, nothing stores it.
            token: minted.token,
            prefix: minted.prefix,
            name,
            scopes: (!scopes.is_empty()).then_some(scopes),
            created_at: minted.created_at,
        }),
    )
        .into_response())
}

/// Every scope asked for must be one the caller holds, and must parse as one
/// of the access model's scopes — a token cannot be minted with a scope
/// nothing would ever grant it.
fn checked_scopes(member: &Member, requested: &[String]) -> Result<Vec<String>, Problem> {
    if requested.is_empty() {
        return Ok(Vec::new());
    }
    let held: Vec<String> = scopes_for(
        &UserId::new(member.user_id.clone()),
        Location::Dm,
        &ChannelMemberships::new(),
    )
    .scope_strings()
    .collect();
    let mut out: Vec<String> = Vec::with_capacity(requested.len());
    for scope in requested {
        let parsed: Scope = scope.parse().map_err(|_| {
            Problem::new(&SCOPE_NOT_HELD)
                .with_detail("a scope must be `shared`, `channel:<id>` or `user:<id>`")
        })?;
        let canonical = parsed.to_string();
        if !held.contains(&canonical) {
            return Err(Problem::new(&SCOPE_NOT_HELD)
                .with_detail(format!("the caller does not hold {canonical:?}")));
        }
        if !out.contains(&canonical) {
            out.push(canonical);
        }
    }
    Ok(out)
}

#[derive(Serialize)]
struct ListView {
    tokens: Vec<Summary>,
}

async fn list(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let member = crate::authenticate(&state.ctx.ports, &headers).await?;
    let tokens = state
        .store
        .list(&member.workspace_id, &member.user_id)
        .await
        .map_err(database)?;
    Ok(Json(ListView { tokens }).into_response())
}

async fn revoke(
    State(state): State<Arc<ModuleState>>,
    Path(prefix): Path<String>,
    headers: HeaderMap,
    OriginalUri(_uri): OriginalUri,
) -> Result<Response, Problem> {
    require_same_origin(&headers)?;
    let member = crate::authenticate(&state.ctx.ports, &headers).await?;
    // Scoped to the caller's own rows, so another member's prefix is "not
    // found" rather than "forbidden".
    let revoked = state
        .store
        .revoke(&member.workspace_id, &member.user_id, &prefix)
        .await
        .map_err(database)?;
    if !revoked {
        return Err(Problem::new(&NO_SUCH_TOKEN));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Refuses a write a browser reports as coming from another site.
///
/// `sec-fetch-site` is checked first — `same-origin` and `none` pass and
/// everything else, including `same-site`, is refused, because a sibling
/// subdomain can post here — then `Origin`, when present, must be this
/// request's own origin, compared with `origin_of` as RFC 6454 origins
/// rather than as strings. A request carrying neither header is not a
/// browser and is accepted.
fn require_same_origin(headers: &HeaderMap) -> Result<(), Problem> {
    let refused = |detail: &str| Problem::new(&CROSS_SITE_REQUEST).with_detail(detail.to_owned());
    if let Some(site) = headers.get("sec-fetch-site") {
        let site = site.to_str().unwrap_or_default().to_ascii_lowercase();
        if !matches!(site.as_str(), "same-origin" | "none") {
            return Err(refused(&format!(
                "sec-fetch-site is {site}; only same-origin or none is accepted"
            )));
        }
    }
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Ok(());
    };
    let presented = origin
        .to_str()
        .map_err(|_| refused("origin was not valid UTF-8"))?;
    let presented = origin_of(presented)
        .map_err(|err| refused(&format!("origin {presented:?} is not an origin: {err}")))?;
    let own = crate::own_origin(headers)
        .ok_or_else(|| refused("origin was present but the request named no host"))?;
    if presented != own {
        return Err(refused(&format!(
            "origin {presented} does not match the host this request reached ({own})"
        )));
    }
    Ok(())
}

/// An authorization server for exactly one grant and one client: the device
/// code grant, for a machine with no browser. There is no authorization
/// endpoint to describe, which is why `response_types_supported` is empty
/// rather than absent, and why the client authenticates with `none` — the
/// device code *is* the proof. `scopes_supported` is deliberately absent:
/// two of the three scopes are per-person, so there is no finite list, and
/// publishing a prefix a client must then guess the rest of is worse.
fn authorization_server(base: &str) -> serde_json::Value {
    json!({
        "issuer": base,
        "device_authorization_endpoint": format!("{base}{DEVICE_AUTH_PATH}/code"),
        "token_endpoint": format!("{base}{DEVICE_AUTH_PATH}/token"),
        "grant_types_supported": [DEVICE_CODE_GRANT],
        "response_types_supported": [],
        "token_endpoint_auth_methods_supported": ["none"],
    })
}

/// The resource is the MCP endpoint and the authorization server is this
/// deployment, so a client that follows the RFC 9728 challenge on a 401
/// arrives here with a token this module accepts. `bearer_methods_supported`
/// is `["header"]` and nothing else: a token in a query parameter would land
/// in access logs and browser history.
fn protected_resource(base: &str) -> serde_json::Value {
    json!({
        "resource": format!("{base}{MCP_PATH}"),
        "authorization_servers": [base],
        "bearer_methods_supported": ["header"],
    })
}

/// The two `/.well-known` routers, as paths relative to the prefix.
///
/// The origin is resolved **per request** from the `Host` the request
/// reached, so one Worker fronts `api.`, `staging-api.`, `mcp.` and a
/// `wrangler dev` on `localhost:8787` and each is told about itself — which
/// RFC 8414 (the `issuer` must equal the prefix the document was fetched
/// from) and RFC 9728 (a client fetches this document from the resource's
/// own host) both require. `fallback` covers a request that names no host at
/// all, which a proxied deployment can produce; a document that advertised
/// the wrong origin is worse than one that admits it does not know.
pub(crate) fn discovery(fallback: Option<&str>) -> axum::Router {
    let fallback = fallback.map(str::to_owned);
    let server = {
        let fallback = fallback.clone();
        move |headers: HeaderMap, uri: Uri| {
            let fallback = fallback.clone();
            async move {
                base(&headers, &uri, fallback.as_deref())
                    .map(|base| Json(authorization_server(&base)).into_response())
                    .ok_or_else(no_origin)
            }
        }
    };
    let resource = move |headers: HeaderMap, uri: Uri| {
        let fallback = fallback.clone();
        async move {
            base(&headers, &uri, fallback.as_deref())
                .map(|base| Json(protected_resource(&base)).into_response())
                .ok_or_else(no_origin)
        }
    };
    axum::Router::new()
        .route("/oauth-authorization-server", get(server))
        .route("/oauth-protected-resource", get(resource))
}

/// The origin these documents are written under, or `None` when the request
/// named no host and no fallback was configured — in which case no
/// `issuer`-bearing document can be written honestly, so none is.
fn base(headers: &HeaderMap, uri: &Uri, fallback: Option<&str>) -> Option<String> {
    crate::origin_of_request(headers, uri).or_else(|| fallback.map(str::to_owned))
}

/// A request that named no host, and a deployment that configured no
/// fallback, leaves no origin to write an `issuer` under.
fn no_origin() -> Problem {
    Problem::internal()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A browser cookie jar sends `sec-fetch-site` and `Origin`; a
    /// non-browser client sends neither. Both are accepted; a cross-site
    /// browser, and a sibling subdomain, are not.
    #[test]
    fn the_same_origin_check_reads_the_two_browser_signals() {
        let own = |pairs: &[(&str, &str)]| {
            let mut headers = HeaderMap::new();
            for (name, value) in pairs {
                headers.insert(
                    axum::http::HeaderName::from_bytes(name.as_bytes()).expect("a header name"),
                    axum::http::HeaderValue::from_str(value).expect("a value"),
                );
            }
            headers
        };
        let ok = |pairs: &[(&str, &str)]| require_same_origin(&own(pairs)).is_ok();
        assert!(ok(&[]), "a non-browser client");
        assert!(ok(&[("sec-fetch-site", "same-origin")]), "the same origin");
        assert!(!ok(&[("sec-fetch-site", "cross-site")]), "another site");
        assert!(
            !ok(&[("sec-fetch-site", "same-site")]),
            "a sibling subdomain can post here"
        );
        assert!(
            ok(&[
                ("host", "api.example.test"),
                ("origin", "https://api.example.test")
            ]),
            "our own origin"
        );
        assert!(
            !ok(&[
                ("host", "api.example.test"),
                ("origin", "https://evil.example")
            ]),
            "another origin"
        );
    }
}
