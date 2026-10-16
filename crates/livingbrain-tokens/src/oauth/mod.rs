//! The OAuth 2.1 authorization server a browser-based MCP client signs in
//! through (issue #72): dynamic client registration (RFC 7591), the
//! authorization-code grant with PKCE S256 (RFC 7636), rotating refresh tokens
//! (RFC 6749 §6), and the issuer identifier on every authorization response
//! (RFC 9207).
//!
//! The access token it mints is a **connector** personal access token: scoped
//! to the workspace, the member, and a non-admin subset of their scopes, and
//! accepted by the MCP endpoint alone. It appears in Settings → API tokens
//! where it can be revoked, and revoking it ends its renewal too.
//!
//! The consent page sets and reads the session cookie, which is host-only
//! (`__Host-`). Authorization therefore completes on the app's own origin: a
//! request that reaches this module on another host (the MCP host) is sent on
//! there with the same path and query, plus a signed token naming the host it
//! came in on, so the response's `iss` is the issuer the client was given.

mod consent;
mod store;

use std::sync::Arc;

use cratefield_core::axum::body::Bytes;
use cratefield_core::axum::extract::{OriginalUri, Query, State};
use cratefield_core::axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::axum::routing::{get, post};
use cratefield_core::axum::{self, Form, Json};
use cratefield_core::{
    AnyError, BoxFuture, Clock, Config, ConfigError, DataKind, Database, DbError, Disposition, Kid,
    Migrations, Module, ModuleContext, PersonalDataSet, Port, Problem, ProblemDef, RateLimit,
    RateLimitFailure, RateLimiter, Signer, SqlMigration, check_rate_limit, client_ip, origin_of,
    rate_limit_keys,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

use crate::store::Store as TokenStore;
use crate::{MCP_PATH, OAUTH_PATH, origin_of_request, same_origin};
use consent::{
    Choice, Consent, S256, base64url, clean_name, consent_csp, consent_page, html, origin_of_uri,
    redirect_host, valid_challenge, valid_redirect_uri, verified_host, verifier_matches,
    with_params,
};
use store::{CLIENTS, CODES, Client, CodeRow, OAuthStore, REFRESH, RefreshRow, hash};

/// The grant's own tables.
const MIGRATION_INIT: SqlMigration = SqlMigration::new(
    "0001",
    "init",
    include_str!("../../migrations/oauth/sqlite/0001_init.sql"),
);

/// The same schema for Postgres (ADR 0004), applied by the parity job.
const MIGRATION_INIT_POSTGRES: SqlMigration = SqlMigration::new(
    "0001",
    "init",
    include_str!("../../migrations/oauth/postgres/0001_init.sql"),
);

/// The app's public origin: the same config key `livingbrain-pages` reads for
/// its links. Unset, the module answers on whichever host the request reached.
const APP_BASE_KEY: &str = "WORKSPACES_REDIRECT_BASE";

/// A connector's access token lives an hour; its refresh token thirty days.
const ACCESS_TTL: Duration = Duration::hours(1);
const REFRESH_TTL: Duration = Duration::days(30);
/// RFC 6749 §4.1.2: a code is short-lived. Sixty seconds is what the client
/// needs to redeem it straight away.
const CODE_TTL: Duration = Duration::seconds(60);
/// How long an approval form stays valid after the consent page is shown.
const CONSENT_TTL_SECS: i64 = 600;
/// The purpose the consent token is sealed under, so no other signed value in
/// this deployment can be presented as one.
const CONSENT_PURPOSE: &str = "livingbrain-oauth-consent";
/// The purpose of the signed token that carries the host an authorization
/// request arrived on across the sign-in hop (RFC 9207's `iss`).
const ISSUER_PURPOSE: &str = "livingbrain-oauth-issuer";
/// The query parameter that carries that token.
const ISSUER_REF: &str = "issuer_ref";
/// The name an unverified client's token carries when it registered without one.
const DEFAULT_CLIENT_NAME: &str = "An app";
/// Registrations a single request may name, and the longest name kept.
const MAX_REDIRECT_URIS: usize = 8;
const MAX_CLIENT_NAME_CHARS: usize = 128;
/// The most clients registered in one UTC day, across every caller. The per-IP
/// limit stops one machine; this stops a rotating crowd from filling the table.
const MAX_REGISTRATIONS_PER_DAY: usize = 200;
/// How long a registered client with no live code or renewal token is kept.
const CLIENT_RETENTION: Duration = Duration::days(30);

const NOT_SIGNED_IN: ProblemDef = ProblemDef {
    slug: "oauth/consent-expired",
    status: StatusCode::FORBIDDEN,
    title: "This approval has expired",
    description: "The approval was not made from a live sign-in here, or it is older than \
                  ten minutes. Start the connection again from the app that asked for it.",
};

const CROSS_SITE_APPROVAL: ProblemDef = ProblemDef {
    slug: "oauth/cross-site-request",
    status: StatusCode::FORBIDDEN,
    title: "A same-origin approval is required",
    description: "An approval is a browser action on this origin. A request from another \
                  site is refused, so nothing can approve a connection on a person's behalf.",
};

const WORKSPACE_NOT_HELD: ProblemDef = ProblemDef {
    slug: "oauth/workspace-not-held",
    status: StatusCode::FORBIDDEN,
    title: "That workspace is not yours to connect",
    description: "The workspace picked is not one the signed-in person is a member of.",
};

/// The authorization server's module (issue #72). It owns the grant's three
/// tables and mounts its routes at `/v1/oauth`.
#[derive(Debug, Default, Clone, Copy)]
pub struct OAuth;

impl OAuth {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Module for OAuth {
    fn name(&self) -> &'static str {
        "oauth"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// `Db` holds the grant's rows and the member rows a code names, `Signer`
    /// seals the consent form and reads the session cookie, `Clock` stamps
    /// every expiry. The registration limiter is optional at composition and
    /// required at request time: without one, registration refuses (see
    /// [`register`]), so a deployment that forgets it fails closed.
    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Signer, Port::Clock]
    }

    fn optional(&self) -> &'static [Port] {
        &[Port::RateLimiter]
    }

    fn tables(&self) -> &'static [&'static str] {
        &[CLIENTS, CODES, REFRESH]
    }

    fn personal_data(&self) -> &'static [PersonalDataSet] {
        const SETS: &[PersonalDataSet] = &[
            PersonalDataSet {
                table: CODES,
                subject: "user_id",
                kind: DataKind::Identifier,
                disposition: Disposition::Erase,
                description: "For each connection a member approved and its one-time code: \
                              which app, which workspace, and when. The code is held only as \
                              a SHA-256 hash, so a row cannot be replayed.",
                redacted: &["code_hash"],
                subject_via: None,
            },
            PersonalDataSet {
                table: REFRESH,
                subject: "user_id",
                kind: DataKind::Identifier,
                disposition: Disposition::Erase,
                description: "For each connection a member approved: the app, the workspace, \
                              and when its renewal token expires. The token is held only as a \
                              SHA-256 hash.",
                redacted: &["token_hash"],
                subject_via: None,
            },
        ];
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

    fn router(&self, ctx: ModuleContext) -> axum::Router {
        let (Some(db), Some(clock), Some(signer)) = (
            ctx.ports.db.clone(),
            ctx.ports.clock.clone(),
            ctx.ports.signer.clone(),
        ) else {
            // `Harness::build` refuses a composition missing a required port,
            // so this answers nothing rather than panicking if one is absent.
            return axum::Router::new();
        };
        let state = Arc::new(OAuthState {
            store: OAuthStore::new(db.clone()),
            tokens: TokenStore::new(db.clone(), clock.clone()),
            db,
            clock,
            signer,
            limiter: ctx.ports.rate_limiter.clone(),
            config: ctx.config.clone(),
        });
        axum::Router::new()
            .route("/authorize", get(authorize_get).post(authorize_post))
            .route("/register", post(register))
            .route("/token", post(token))
            .with_state(state)
    }

    /// The purge of finished grant rows (see [`OAuthStore::purge`]). Runs on
    /// every scheduled firing, under the same budget the device grant's purge
    /// spends, so a cron pass cannot starve the rest of the Worker.
    fn scheduled<'a>(
        &'a self,
        ctx: &'a ModuleContext,
        _cron: &'a str,
    ) -> BoxFuture<'a, Result<(), AnyError>> {
        Box::pin(async move {
            if !ctx.scheduled.try_spend(1) {
                return Ok(());
            }
            let (Some(db), Some(clock)) = (ctx.ports.db.clone(), ctx.ports.clock.clone()) else {
                return Ok(());
            };
            let now = clock.now();
            OAuthStore::new(db)
                .purge(&stamp(now), &stamp(now - CLIENT_RETENTION))
                .await
                .map(|_| ())
                .map_err(|err| Box::new(err) as AnyError)
        })
    }
}

/// What every handler needs: the grant's tables, the personal access token
/// store it mints through, and the ports the consent reads.
pub(crate) struct OAuthState {
    store: OAuthStore,
    tokens: TokenStore,
    db: Arc<dyn Database>,
    clock: Arc<dyn Clock>,
    signer: Arc<dyn Signer>,
    limiter: Option<Arc<dyn RateLimiter>>,
    config: Arc<dyn Config>,
}

// ---------------------------------------------------------------------------
// Small helpers

/// A UTC RFC 3339 stamp with whole seconds. Every stamp this module writes or
/// compares is UTC, so the strings order chronologically.
fn stamp(at: OffsetDateTime) -> String {
    at.to_offset(time::UtcOffset::UTC)
        .replace_nanosecond(0)
        .expect("truncation to whole seconds stays in range")
        .format(&Rfc3339)
        .unwrap_or_default()
}

/// Whether an expiry stamp is still ahead of `now`. An unreadable stamp is
/// not: fail closed.
fn live_at(expiry: &str, now: OffsetDateTime) -> bool {
    OffsetDateTime::parse(expiry, &Rfc3339).is_ok_and(|at| at > now)
}

/// `n` random bytes from the isolate's CSPRNG, as unpadded base64url.
fn random_token(n: usize) -> Result<String, DbError> {
    let mut bytes = vec![0_u8; n];
    getrandom::fill(&mut bytes).map_err(|err| DbError::Execute(format!("entropy: {err}")))?;
    Ok(base64url(&bytes))
}

/// The app's origin from config, or `None` when unset or malformed (the same
/// rule `livingbrain-pages` applies to its own links).
fn app_origin(config: &dyn Config) -> Option<String> {
    config
        .get(APP_BASE_KEY)
        .map(|raw| raw.trim().trim_end_matches('/').to_owned())
        .filter(|base| base.starts_with("https://") || base.starts_with("http://"))
        .filter(|base| !base.contains(['?', '#']))
        .and_then(|base| origin_of(&base).ok())
}

/// The request's path and query, which always begins with `/`, so it can be
/// appended to an origin without naming a different host.
fn path_and_query(uri: &Uri) -> String {
    uri.path_and_query().map_or_else(
        || format!("{OAUTH_PATH}/authorize"),
        |pq| pq.as_str().to_owned(),
    )
}

/// Appends one query parameter to a path-and-query.
fn append_query(path: &str, name: &str, value: &str) -> String {
    let separator = if path.contains('?') { '&' } else { '?' };
    format!("{path}{separator}{name}={}", consent::encode(value))
}

/// A redirect that never leaks the page it came from: `Referrer-Policy` says
/// so for every hop this module sends.
fn redirect(status: StatusCode, location: &str) -> Response {
    let mut response = status.into_response();
    if let Ok(value) = HeaderValue::from_str(location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response
}

/// An RFC 6749 §5.2 error body, never cached.
fn token_error(status: StatusCode, error: &str, description: &str) -> Response {
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        Json(json!({ "error": error, "error_description": description })),
    )
        .into_response()
}

fn server_error() -> Response {
    token_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "server_error",
        "the request could not be completed",
    )
}

/// A plain page for a request that cannot be sent back to its client: an
/// unknown client or an unregistered redirect URI.
fn error_page(status: u16, title: &str, detail: &str) -> Response {
    let body = format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title}</title><link rel=\"stylesheet\" href=\"/assets/app.css\">\
         <link rel=\"stylesheet\" href=\"/assets/device.css\"></head>\
         <body><main class=\"device\"><p class=\"eyebrow\">Connect an app</p>\
         <h1>{title}</h1><p class=\"lede\">{detail}</p></main></body></html>\n",
        title = consent::escape(title),
        detail = consent::escape(detail),
    );
    html(status, body, &consent_csp(""))
}

/// The error an authorization request is sent back with, once its client and
/// redirect URI are known to be genuine. Carries `iss` (RFC 9207) so the
/// client can tell which authorization server answered. A GET answers `302`;
/// the approval form, a POST, answers `303` so the browser follows with a GET.
fn authorize_error(
    status: StatusCode,
    redirect_uri: &str,
    state: Option<&str>,
    issuer: &str,
    error: &str,
    detail: &str,
) -> Response {
    let mut params = vec![
        ("error", error),
        ("error_description", detail),
        ("iss", issuer),
    ];
    if let Some(state) = state {
        params.push(("state", state));
    }
    redirect(status, &with_params(redirect_uri, &params))
}

fn problem(def: &'static ProblemDef) -> Response {
    Problem::new(def).into_response()
}

/// The scopes a connector token is granted: everything a person can read as
/// shared, and their own private memory to add notes to. A subset, so the
/// token is never an admin, whatever the member is (see `member_for`).
fn granted_scopes(user_id: &str) -> Vec<String> {
    vec!["shared".to_owned(), format!("user:{user_id}")]
}

/// The name a connector's token carries in Settings → API tokens. A client
/// whose redirect host Living Brain recognises is labelled as a Claude
/// connector; any other client keeps only the name it registered (cleaned),
/// so nothing invents a provenance it does not have.
fn connector_name(client: &Client) -> String {
    let registered = clean_name(&client.client_name);
    let name = if registered.is_empty() {
        DEFAULT_CLIENT_NAME.to_owned()
    } else {
        registered
    };
    let verified = client
        .redirect_uris
        .iter()
        .filter_map(|uri| redirect_host(uri))
        .any(|host| verified_host(&host));
    let label = if verified {
        format!("Claude connector · {name}")
    } else {
        name
    };
    label.chars().take(64).collect()
}

fn token_response(access: &str, refresh: &str, scope: &str) -> Response {
    let mut body = json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": ACCESS_TTL.whole_seconds(),
        "refresh_token": refresh,
        "scope": scope,
    });
    if scope.is_empty() {
        body.as_object_mut().map(|object| object.remove("scope"));
    }
    (
        StatusCode::OK,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        Json(body),
    )
        .into_response()
}

/// Whether an MCP resource URL names the MCP endpoint: an absolute URL under
/// the same rule the redirect URIs follow, whose path is the endpoint's.
fn valid_resource(resource: &str) -> bool {
    valid_redirect_uri(resource)
        && resource
            .split_once("://")
            .and_then(|(_, rest)| rest.find('/').map(|at| &rest[at..]))
            == Some(MCP_PATH)
}

// ---------------------------------------------------------------------------
// The sealed consent and the issuer token

/// What the consent page binds: the request as it was validated, the issuer
/// it came in on, and the member it was shown to. The form posts this back
/// sealed, so nothing the approval acts on is read from the post itself but
/// the chosen workspace.
#[derive(Debug, Serialize, Deserialize)]
struct Sealed {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    state: Option<String>,
    issuer: String,
    workspace_id: String,
    user_id: String,
    exp: i64,
}

fn seal(st: &OAuthState, consent: &Sealed) -> Option<String> {
    let subject = serde_json::to_string(consent).ok()?;
    Some(st.signer.sign(&cratefield_core::Payload {
        purpose: CONSENT_PURPOSE.to_owned(),
        subject,
        exp: None,
        kid: Kid::Cur,
    }))
}

fn open(st: &OAuthState, token: &str) -> Option<Sealed> {
    let payload = st.signer.verify(token, CONSENT_PURPOSE)?;
    let sealed: Sealed = serde_json::from_str(&payload.subject).ok()?;
    (sealed.exp > st.clock.now().unix_timestamp()).then_some(sealed)
}

/// The signed token naming the origin an authorization request arrived on.
fn seal_issuer(st: &OAuthState, origin: &str) -> String {
    st.signer.sign(&cratefield_core::Payload {
        purpose: ISSUER_PURPOSE.to_owned(),
        subject: origin.to_owned(),
        exp: None,
        kid: Kid::Cur,
    })
}

/// The origin a signed issuer token names, or `None` for a forged or
/// unrecognised one (the caller then keeps its own origin).
fn open_issuer(st: &OAuthState, token: &str) -> Option<String> {
    st.signer
        .verify(token, ISSUER_PURPOSE)
        .map(|payload| payload.subject)
        .filter(|origin| origin.starts_with("https://") || origin.starts_with("http://"))
}

// ---------------------------------------------------------------------------
// GET /v1/oauth/authorize

#[derive(Debug, Deserialize)]
struct AuthorizeQuery {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    state: Option<String>,
    /// Accepted and ignored: the grant issues the scopes it defines
    /// ([`granted_scopes`]), and a requested scope is never echoed or stored.
    #[allow(dead_code)]
    scope: Option<String>,
    resource: Option<String>,
    /// Set by this module when it sends a request on to the app origin.
    issuer_ref: Option<String>,
}

/// The authorization request. The client and redirect URI are checked first
/// and nothing redirects until both are genuine: a bad client gets a page,
/// never a redirect that could carry a code to somewhere it should not go.
async fn authorize_get(
    State(st): State<Arc<OAuthState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<AuthorizeQuery>,
) -> Response {
    let Some(client_id) = query.client_id.as_deref() else {
        return error_page(
            400,
            "This link is incomplete",
            "The app did not say which client is asking.",
        );
    };
    let client = match st.store.client(client_id).await {
        Ok(Some(client)) => client,
        Ok(None) => {
            return error_page(
                400,
                "Unknown app",
                "This app is not registered with Living Brain. Add it again from the app.",
            );
        }
        Err(_) => return error_page(500, "Something went wrong", "Try again in a moment."),
    };
    let Some(redirect_uri) = query
        .redirect_uri
        .as_deref()
        .filter(|uri| client.redirect_uris.iter().any(|known| known == uri))
    else {
        return error_page(
            400,
            "The return address does not match",
            "The app asked to return somewhere it did not register, so nothing was sent.",
        );
    };
    let state = query.state.as_deref();

    // The host this request arrived on is the issuer the client was told
    // about (its protected-resource document names it). The consent lives on
    // the app's origin, where the session cookie is, so a request that came in
    // elsewhere is sent on there, carrying that host in a signed token.
    let here = origin_of_request(&headers, &uri);
    let app = app_origin(&*st.config);
    if let (Some(app), Some(here)) = (app.as_deref(), here.as_deref())
        && here != app
    {
        let carried = append_query(&path_and_query(&uri), ISSUER_REF, &seal_issuer(&st, here));
        return redirect(StatusCode::FOUND, &format!("{app}{carried}"));
    }
    let issuer = query
        .issuer_ref
        .as_deref()
        .and_then(|token| open_issuer(&st, token))
        .or(here)
        .unwrap_or_default();

    if query.response_type.as_deref() != Some("code") {
        return authorize_error(
            StatusCode::FOUND,
            redirect_uri,
            state,
            &issuer,
            "unsupported_response_type",
            "only the code response is supported",
        );
    }
    let Some(challenge) = query
        .code_challenge
        .as_deref()
        .filter(|c| valid_challenge(c))
    else {
        return authorize_error(
            StatusCode::FOUND,
            redirect_uri,
            state,
            &issuer,
            "invalid_request",
            "a code_challenge is required",
        );
    };
    if query.code_challenge_method.as_deref() != Some(S256) {
        return authorize_error(
            StatusCode::FOUND,
            redirect_uri,
            state,
            &issuer,
            "invalid_request",
            "code_challenge_method must be S256",
        );
    }
    if query
        .resource
        .as_deref()
        .is_some_and(|resource| !valid_resource(resource))
    {
        return authorize_error(
            StatusCode::FOUND,
            redirect_uri,
            state,
            &issuer,
            "invalid_target",
            "resource must name the MCP endpoint",
        );
    }

    let caller =
        match livingbrain_workspaces::caller(&*st.signer, &*st.clock, &*st.db, &headers).await {
            Ok(caller) => caller,
            Err(_) => {
                // Nobody is signed in here: the app's sign-in brings the person
                // back to this exact request afterwards.
                let location = sign_in_to(&path_and_query(&uri));
                return redirect(StatusCode::FOUND, &location);
            }
        };
    let choices = match livingbrain_workspaces::workspace_choices(
        &*st.db,
        &caller.workspace_id,
        &caller.user_id,
    )
    .await
    {
        Ok(choices) => choices,
        Err(_) => return error_page(500, "Something went wrong", "Try again in a moment."),
    };
    let choice_views: Vec<Choice<'_>> = choices
        .iter()
        .map(|choice| Choice {
            workspace_id: &choice.workspace_id,
            name: &choice.name,
        })
        .collect();
    let sealed = Sealed {
        client_id: client.client_id.clone(),
        redirect_uri: redirect_uri.to_owned(),
        code_challenge: challenge.to_owned(),
        state: query.state.clone(),
        issuer: issuer.clone(),
        workspace_id: caller.workspace_id.clone(),
        user_id: caller.user_id.clone(),
        exp: st.clock.now().unix_timestamp() + CONSENT_TTL_SECS,
    };
    let Some(request) = seal(&st, &sealed) else {
        return server_error_page();
    };
    let Some(host) = redirect_host(redirect_uri) else {
        return server_error_page();
    };
    let origin = origin_of_uri(redirect_uri).unwrap_or_default();
    let name = connector_display_name(&client);
    let page = consent_page(&Consent {
        client_name: &name,
        host: &host,
        verified: verified_host(&host),
        request: &request,
        choices: &choice_views,
    });
    html(200, page, &consent_csp(&origin))
}

/// The client's name as the consent page shows it.
fn connector_display_name(client: &Client) -> String {
    let name = clean_name(&client.client_name);
    if name.is_empty() {
        DEFAULT_CLIENT_NAME.to_owned()
    } else {
        name
    }
}

fn server_error_page() -> Response {
    error_page(500, "Something went wrong", "Try again in a moment.")
}

/// The app's sign-in page with `return_to` naming `path`. Relative, because
/// the consent is only ever reached on the app's own origin.
fn sign_in_to(path: &str) -> String {
    crate::device::sign_in_location("/index.html", path)
}

// ---------------------------------------------------------------------------
// POST /v1/oauth/authorize

#[derive(Debug, Deserialize)]
struct ConsentForm {
    request: Option<String>,
    decision: Option<String>,
    workspace_id: Option<String>,
}

/// The approval. Four things must hold before a code is minted: the browser
/// reports the same origin, the sealed form is genuine and unexpired, the
/// signed-in person is the one it was shown to, and the workspace picked is
/// one they hold. Only then is a single-use code issued.
async fn authorize_post(
    State(st): State<Arc<OAuthState>>,
    headers: HeaderMap,
    Form(form): Form<ConsentForm>,
) -> Response {
    if let Err(refused) = same_origin(&headers) {
        return Problem::new(&CROSS_SITE_APPROVAL)
            .with_detail(refused.detail)
            .into_response();
    }
    let Some(sealed) = form.request.as_deref().and_then(|token| open(&st, token)) else {
        return problem(&NOT_SIGNED_IN);
    };
    let caller = match livingbrain_workspaces::caller(&*st.signer, &*st.clock, &*st.db, &headers)
        .await
    {
        Ok(caller)
            if caller.workspace_id == sealed.workspace_id && caller.user_id == sealed.user_id =>
        {
            caller
        }
        _ => return problem(&NOT_SIGNED_IN),
    };
    let Ok(Some(client)) = st.store.client(&sealed.client_id).await else {
        return error_page(
            400,
            "Unknown app",
            "This app is not registered with Living Brain.",
        );
    };
    if !client.redirect_uris.contains(&sealed.redirect_uri) {
        return error_page(
            400,
            "The return address does not match",
            "Nothing was sent.",
        );
    }
    let state = sealed.state.as_deref();
    match form.decision.as_deref() {
        Some("deny") => authorize_error(
            StatusCode::SEE_OTHER,
            &sealed.redirect_uri,
            state,
            &sealed.issuer,
            "access_denied",
            "the person did not approve the connection",
        ),
        Some("approve") => {
            let choices = match livingbrain_workspaces::workspace_choices(
                &*st.db,
                &caller.workspace_id,
                &caller.user_id,
            )
            .await
            {
                Ok(choices) => choices,
                Err(_) => return server_error_page(),
            };
            let Some(choice) = form
                .workspace_id
                .as_deref()
                .and_then(|id| choices.into_iter().find(|choice| choice.workspace_id == id))
            else {
                return problem(&WORKSPACE_NOT_HELD);
            };
            issue_code(&st, &sealed, &choice).await
        }
        _ => authorize_error(
            StatusCode::SEE_OTHER,
            &sealed.redirect_uri,
            state,
            &sealed.issuer,
            "invalid_request",
            "the decision is not recognised",
        ),
    }
}

async fn issue_code(
    st: &OAuthState,
    sealed: &Sealed,
    choice: &livingbrain_workspaces::WorkspaceChoice,
) -> Response {
    let code = match random_token(32) {
        Ok(code) => code,
        Err(_) => return server_error_page(),
    };
    let family = match random_token(16) {
        Ok(family) => family,
        Err(_) => return server_error_page(),
    };
    let now = st.clock.now();
    let granted = granted_scopes(&choice.user_id).join(" ");
    let row = CodeRow {
        client_id: sealed.client_id.clone(),
        redirect_uri: sealed.redirect_uri.clone(),
        code_challenge: sealed.code_challenge.clone(),
        workspace_id: choice.workspace_id.clone(),
        user_id: choice.user_id.clone(),
        scope: granted,
        family,
        expires_at: stamp(now + CODE_TTL),
        used: false,
    };
    if st
        .store
        .insert_code(&hash(&code), &row, &stamp(now))
        .await
        .is_err()
    {
        return server_error_page();
    }
    let mut params = vec![("code", code.as_str()), ("iss", sealed.issuer.as_str())];
    if let Some(state) = sealed.state.as_deref() {
        params.push(("state", state));
    }
    redirect(
        StatusCode::SEE_OTHER,
        &with_params(&sealed.redirect_uri, &params),
    )
}

// ---------------------------------------------------------------------------
// POST /v1/oauth/register (RFC 7591)

/// Registers a public client. Only `none` is accepted as the client
/// authentication method: a connector cannot keep a secret, so PKCE binds its
/// codes instead.
///
/// Fails closed when no rate limiter is bound, and refuses once the day's cap
/// is reached: an open registration endpoint with neither would let anyone fill
/// the table, and the per-IP limit alone does not stop a crowd of addresses.
async fn register(State(st): State<Arc<OAuthState>>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(limiter) = st.limiter.as_ref() else {
        return token_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "temporarily_unavailable",
            "client registration is not available on this deployment",
        );
    };
    let keys: Vec<String> = rate_limit_keys(client_ip(&headers).as_deref(), None)
        .into_iter()
        .map(|key| format!("oauth-register:{key}"))
        .collect();
    if let RateLimit::Denied { decision } =
        check_rate_limit(Some(limiter), &keys, RateLimitFailure::FailClosed).await
    {
        return cratefield_core::rate_limited(&decision);
    }
    let now = st.clock.now();
    let day_start = now.replace_time(time::Time::MIDNIGHT);
    match st.store.clients_since(&stamp(day_start)).await {
        Ok(count) if count < MAX_REGISTRATIONS_PER_DAY => {}
        Ok(_) => {
            return token_error(
                StatusCode::TOO_MANY_REQUESTS,
                "temporarily_unavailable",
                "too many apps have registered today; try again tomorrow",
            );
        }
        Err(_) => return server_error(),
    }
    let invalid = |error: &str, detail: &str| {
        (
            StatusCode::BAD_REQUEST,
            [(header::CACHE_CONTROL, "no-store")],
            Json(json!({ "error": error, "error_description": detail })),
        )
            .into_response()
    };
    let Ok(Value::Object(metadata)) = serde_json::from_slice::<Value>(&body) else {
        return invalid("invalid_client_metadata", "the body must be a JSON object");
    };
    let uris: Vec<String> = match metadata.get("redirect_uris") {
        Some(Value::Array(items)) if !items.is_empty() && items.len() <= MAX_REDIRECT_URIS => {
            let Some(uris) = items
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect::<Option<Vec<String>>>()
            else {
                return invalid("invalid_redirect_uri", "each redirect URI must be a string");
            };
            uris
        }
        _ => {
            return invalid(
                "invalid_redirect_uri",
                "name between one and eight redirect URIs",
            );
        }
    };
    if uris.iter().any(|uri| !valid_redirect_uri(uri)) {
        return invalid(
            "invalid_redirect_uri",
            "a redirect URI must be https, or http to localhost, 127.0.0.1 or [::1], with a plain host, no fragment and ASCII only",
        );
    }
    if metadata
        .get("token_endpoint_auth_method")
        .is_some_and(|method| method.as_str() != Some("none"))
    {
        return invalid(
            "invalid_client_metadata",
            "only token_endpoint_auth_method none is supported",
        );
    }
    let grants_ok = metadata.get("grant_types").is_none_or(|grants| {
        grants.as_array().is_some_and(|items| {
            items
                .iter()
                .all(|item| matches!(item.as_str(), Some("authorization_code" | "refresh_token")))
        })
    });
    if !grants_ok {
        return invalid(
            "invalid_client_metadata",
            "grant_types may be authorization_code and refresh_token",
        );
    }
    let responses_ok = metadata.get("response_types").is_none_or(|types| {
        types
            .as_array()
            .is_some_and(|items| items.iter().all(|item| item.as_str() == Some("code")))
    });
    if !responses_ok {
        return invalid("invalid_client_metadata", "response_types may be code");
    }
    // Control and format characters are stripped before the name is kept, so
    // a right-to-left override cannot make it read as something else.
    let client_name: String = clean_name(
        metadata
            .get("client_name")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    )
    .chars()
    .take(MAX_CLIENT_NAME_CHARS)
    .collect();

    let Ok(id) = random_token(16) else {
        return server_error();
    };
    let client = Client {
        client_id: format!("lbc_{id}"),
        client_name: client_name.clone(),
        redirect_uris: uris.clone(),
    };
    if st.store.insert_client(&client, &stamp(now)).await.is_err() {
        return server_error();
    }
    (
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "client_id": client.client_id,
            "client_id_issued_at": now.unix_timestamp(),
            "client_name": client_name,
            "redirect_uris": uris,
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// POST /v1/oauth/token

#[derive(Debug, Deserialize)]
struct TokenForm {
    grant_type: Option<String>,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
}

/// The one token endpoint. The device grant's poll is answered by the device
/// handler at `/v1/device-auth/token`; the venture routes a device poll there
/// before it reaches this module, so one advertised `token_endpoint` serves
/// every grant a client may use. A device poll that does arrive here is named
/// as unsupported rather than mis-answered.
async fn token(State(st): State<Arc<OAuthState>>, Form(form): Form<TokenForm>) -> Response {
    match form.grant_type.as_deref() {
        Some("authorization_code") => exchange_code(&st, &form).await,
        Some("refresh_token") => rotate_refresh(&st, &form).await,
        Some("urn:ietf:params:oauth:grant-type:device_code") => token_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "the device grant polls /v1/device-auth/token",
        ),
        _ => token_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "the grant is not supported",
        ),
    }
}

/// The client a token request names, which must be registered: a public
/// client is identified by its id alone.
async fn named_client(st: &OAuthState, client_id: Option<&str>) -> Result<Client, Box<Response>> {
    let Some(client_id) = client_id.filter(|id| !id.is_empty()) else {
        return Err(Box::new(token_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "client_id is required",
        )));
    };
    match st.store.client(client_id).await {
        Ok(Some(client)) => Ok(client),
        Ok(None) => Err(Box::new(token_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "the client is not registered",
        ))),
        Err(_) => Err(Box::new(server_error())),
    }
}

/// Ends a connection: every access token its family was given is revoked.
/// Run when a spent code or a spent refresh token is presented again, the
/// signal that a copy of the credential is loose. Revoking a token also spends
/// its renewal token (see `TokenStore::revoke`), so the chain ends with it.
async fn revoke_family(st: &OAuthState, family: &str) {
    let Ok(rows) = st.store.family(family).await else {
        return;
    };
    for row in rows {
        let _ = st
            .tokens
            .revoke(&row.workspace_id, &row.user_id, &row.prefix)
            .await;
    }
}

async fn exchange_code(st: &OAuthState, form: &TokenForm) -> Response {
    let client = match named_client(st, form.client_id.as_deref()).await {
        Ok(client) => client,
        Err(response) => return *response,
    };
    let Some(code) = form.code.as_deref().filter(|code| !code.is_empty()) else {
        return token_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "code is required",
        );
    };
    let code_hash = hash(code);
    let row = match st.store.code(&code_hash).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            return token_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "the code is not recognised",
            );
        }
        Err(_) => return server_error(),
    };
    // A code that was already spent is a replay: the connection it opened is
    // ended, and the caller is refused like any other bad code.
    if row.used {
        revoke_family(st, &row.family).await;
        return token_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "the code has already been used",
        );
    }
    let now = st.clock.now();
    match st.store.spend_code(&code_hash, &stamp(now)).await {
        Ok(true) => {}
        Ok(false) => {
            // Lost a race to a concurrent redemption of the same code.
            revoke_family(st, &row.family).await;
            return token_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "the code has already been used",
            );
        }
        Err(_) => return server_error(),
    }
    // The code is spent whatever the checks below say: a wrong verifier or a
    // different client is one guess, not a retry.
    let verifier = form.code_verifier.as_deref().unwrap_or_default();
    let bound = row.client_id == client.client_id
        && form.redirect_uri.as_deref() == Some(row.redirect_uri.as_str())
        && live_at(&row.expires_at, now)
        && verifier_matches(verifier, &row.code_challenge);
    if !bound {
        return token_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "the code does not match this request",
        );
    }
    match livingbrain_workspaces::caller_for(&*st.db, &row.workspace_id, &row.user_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return token_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "the member is no longer in this workspace",
            );
        }
        Err(_) => return server_error(),
    }
    let granted = granted_scopes(&row.user_id);
    let access = match st
        .tokens
        .mint_connector(
            &row.workspace_id,
            &row.user_id,
            &connector_name(&client),
            &granted,
            now + ACCESS_TTL,
        )
        .await
    {
        Ok(access) => access,
        Err(_) => return server_error(),
    };
    let Ok(refresh) = issue_refresh(
        st,
        &row.family,
        &access.prefix,
        &client.client_id,
        &row.workspace_id,
        &row.user_id,
        now,
    )
    .await
    else {
        return server_error();
    };
    token_response(&access.token, &refresh, &row.scope)
}

async fn issue_refresh(
    st: &OAuthState,
    family: &str,
    prefix: &str,
    client_id: &str,
    workspace_id: &str,
    user_id: &str,
    now: OffsetDateTime,
) -> Result<String, DbError> {
    let token = format!("lbr_{}", random_token(32)?);
    let row = RefreshRow {
        family: family.to_owned(),
        prefix: prefix.to_owned(),
        client_id: client_id.to_owned(),
        workspace_id: workspace_id.to_owned(),
        user_id: user_id.to_owned(),
        expires_at: stamp(now + REFRESH_TTL),
        used: false,
    };
    st.store
        .insert_refresh(&hash(&token), &row, &stamp(now))
        .await?;
    Ok(token)
}

async fn rotate_refresh(st: &OAuthState, form: &TokenForm) -> Response {
    let client = match named_client(st, form.client_id.as_deref()).await {
        Ok(client) => client,
        Err(response) => return *response,
    };
    let Some(presented) = form
        .refresh_token
        .as_deref()
        .filter(|token| !token.is_empty())
    else {
        return token_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "refresh_token is required",
        );
    };
    let presented_hash = hash(presented);
    let row = match st.store.refresh(&presented_hash).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            return token_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "the refresh token is not recognised",
            );
        }
        Err(_) => return server_error(),
    };
    let now = st.clock.now();
    if row.client_id != client.client_id || !live_at(&row.expires_at, now) {
        return token_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "the refresh token is no longer valid",
        );
    }
    if row.used {
        // A spent refresh token presented again: somebody holds a copy. Every
        // access token of the connection is revoked, the live one included, so
        // the copy and whatever it was rotated into end together.
        revoke_family(st, &row.family).await;
        return token_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "the refresh token has already been used",
        );
    }
    match st.store.spend_refresh(&presented_hash, &stamp(now)).await {
        Ok(true) => {}
        Ok(false) => {
            revoke_family(st, &row.family).await;
            return token_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "the refresh token has already been used",
            );
        }
        Err(_) => return server_error(),
    }
    // Revoking the access token ends the connection: a refresh cannot renew it.
    match st.tokens.is_unrevoked(&row.prefix).await {
        Ok(true) => {}
        Ok(false) => {
            return token_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "the connection was revoked",
            );
        }
        Err(_) => return server_error(),
    }
    match livingbrain_workspaces::caller_for(&*st.db, &row.workspace_id, &row.user_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return token_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "the member is no longer in this workspace",
            );
        }
        Err(_) => return server_error(),
    }
    // Rotate: retire the access token this refresh kept alive, and mint its
    // successor, so the settings list shows one connection, not one per hour.
    let _ = st
        .tokens
        .revoke(&row.workspace_id, &row.user_id, &row.prefix)
        .await;
    let granted = granted_scopes(&row.user_id);
    let access = match st
        .tokens
        .mint_connector(
            &row.workspace_id,
            &row.user_id,
            &connector_name(&client),
            &granted,
            now + ACCESS_TTL,
        )
        .await
    {
        Ok(access) => access,
        Err(_) => return server_error(),
    };
    let Ok(refresh) = issue_refresh(
        st,
        &row.family,
        &access.prefix,
        &client.client_id,
        &row.workspace_id,
        &row.user_id,
        now,
    )
    .await
    else {
        return server_error();
    };
    token_response(&access.token, &refresh, &granted.join(" "))
}
