//! The OAuth 2.1 authorization server a browser-based MCP client signs in
//! through (issue #72): dynamic client registration (RFC 7591), the
//! authorization-code grant with PKCE S256 (RFC 7636), rotating refresh tokens
//! (RFC 6749 §6), and the issuer identifier on every authorization response
//! (RFC 9207).
//!
//! A client may also identify itself by a Client ID Metadata Document
//! (draft-ietf-oauth-client-id-metadata-document, issue #234): a client_id
//! that is an `https` URL names a document this server fetches, validates
//! and caches — see [`cimd`] — so a platform like ChatGPT needs no
//! registration to connect.
//!
//! Beside those public clients, a deployment may name one **predefined
//! confidential client** for a ChatGPT custom GPT's Actions (issue #234):
//! three `OAUTH_ACTIONS_*` keys describe it out of band, because a GPT's
//! Actions cannot register — the operator pastes a client id and secret into
//! the GPT's OAuth settings. Its secret, compared as a SHA-256 and never
//! configured whole, binds its codes where a public client's PKCE would, and
//! its tokens mint with the `actions` audience instead of `mcp`.
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

mod cimd;
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
    RateLimitFailure, RateLimiter, Signer, SqlMigration, check_rate_limit, client_ip,
    constant_time_eq, origin_of, rate_limit_keys,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

use crate::store::Store as TokenStore;
use crate::{MCP_PATH, OAUTH_PATH, origin_of_request, same_origin};
use consent::{
    Choice, Consent, S256, base64url, clean_name, connector_vendor, consent_csp, consent_page,
    html, origin_of_uri, redirect_host, valid_challenge, valid_redirect_uri, verifier_matches,
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

/// The Client ID Metadata Document cache (issue #234): when a cached
/// document must be fetched again.
const MIGRATION_METADATA_EXPIRY: SqlMigration = SqlMigration::new(
    "0002",
    "metadata_expiry",
    include_str!("../../migrations/oauth/sqlite/0002_metadata_expiry.sql"),
);

/// The same column for Postgres (ADR 0004), applied by the parity job.
const MIGRATION_METADATA_EXPIRY_POSTGRES: SqlMigration = SqlMigration::new(
    "0002",
    "metadata_expiry",
    include_str!("../../migrations/oauth/postgres/0002_metadata_expiry.sql"),
);

/// The app's public origin: the same config key `livingbrain-pages` reads for
/// its links. Unset, the module answers on whichever host the request reached.
const APP_BASE_KEY: &str = "WORKSPACES_REDIRECT_BASE";

/// The predefined Actions client's id (issue #234): the operator pastes it
/// into the GPT's OAuth settings, so it comes from config, not registration.
const ACTIONS_ID_KEY: &str = "OAUTH_ACTIONS_CLIENT_ID";
/// The lowercase-hex SHA-256 of that client's secret. The secret itself is
/// never configured — only its hash, so a deployment's config holds nothing
/// a copy of would let its holder back in.
const ACTIONS_SECRET_KEY: &str = "OAUTH_ACTIONS_CLIENT_SECRET_SHA256";
/// The client's redirect URIs, space-separated: the GPT callback URLs the
/// operator copied out of ChatGPT.
const ACTIONS_REDIRECTS_KEY: &str = "OAUTH_ACTIONS_REDIRECT_URIS";
/// The client's display name, optional.
const ACTIONS_NAME_KEY: &str = "OAUTH_ACTIONS_CLIENT_NAME";
/// [`ACTIONS_NAME_KEY`]'s value when the operator sets none.
const DEFAULT_ACTIONS_NAME: &str = "Living Brain GPT";

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
/// The most apps with an unverified redirect host registered in one UTC day,
/// across every caller. Verified (Claude) registrations do not count toward it.
const MAX_UNVERIFIED_REGISTRATIONS_PER_DAY: usize = 200;
/// The most apps one registering address may create in an hour.
const MAX_REGISTRATIONS_PER_IP_HOUR: usize = 10;
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

    /// The `HttpClient` a Client ID Metadata Document client is fetched
    /// through ([`cimd`]): without one a client_id that is a URL is refused
    /// with a clear error and every other route — registration included —
    /// is unaffected.
    fn optional(&self) -> &'static [Port] {
        &[Port::RateLimiter, Port::HttpClient]
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

    /// The predefined Actions client's keys (issue #234): unset is off, all
    /// three set is on, and anything between or malformed is refused here —
    /// the Cloudflare runtime runs this once per isolate at cold start and
    /// logs the failure loudly, and each route that meets the configured id
    /// refuses on it too (see [`actions_config`]), so existing deployments
    /// and every other client are unaffected either way.
    fn validate_config(&self, cfg: &dyn Config) -> Result<(), ConfigError> {
        let mut errors = ConfigError::new();
        if let ActionsConfig::Broken { reason, .. } = actions_config(cfg) {
            errors.push(reason);
        }
        errors.into_result()
    }

    fn migrations(&self) -> Migrations {
        const MIGRATIONS: [SqlMigration; 2] = [MIGRATION_INIT, MIGRATION_METADATA_EXPIRY];
        const _: () = cratefield_core::assert_migration_set(&MIGRATIONS);
        const MIGRATIONS_POSTGRES: [SqlMigration; 2] =
            [MIGRATION_INIT_POSTGRES, MIGRATION_METADATA_EXPIRY_POSTGRES];
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
            http: ctx.ports.http.clone(),
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
/// store it mints through, and the ports the consent reads. `http` is the
/// optional port a Client ID Metadata Document client is fetched through.
pub(crate) struct OAuthState {
    store: OAuthStore,
    tokens: TokenStore,
    db: Arc<dyn Database>,
    clock: Arc<dyn Clock>,
    signer: Arc<dyn Signer>,
    limiter: Option<Arc<dyn RateLimiter>>,
    http: Option<Arc<dyn cratefield_core::HttpClient>>,
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

// ---------------------------------------------------------------------------
// The predefined Actions client (issue #234)

/// The configured Actions client: the client synthesized from config — never
/// a stored row — plus the SHA-256 its secret is compared against.
struct ActionsClient {
    client: Client,
    secret_sha256: String,
}

/// What the `OAUTH_ACTIONS_*` keys say. All three required keys set makes a
/// client; none set leaves the feature off; anything between — or a value
/// that breaks a rule — is [`ActionsConfig::Broken`], which
/// [`Module::validate_config`] reports at cold start and every route that
/// meets the configured id refuses on, so a half-configured deployment
/// approves nothing rather than something half-checked.
enum ActionsConfig {
    Off,
    Broken {
        /// The configured id, when one was set: the id whose requests the
        /// `reason` refuses.
        id: Option<String>,
        reason: String,
    },
    On(ActionsClient),
}

/// Reads the predefined Actions client out of config. Cheap enough to call
/// per request, and per request is the point: nothing is cached across a
/// config change.
fn actions_config(config: &dyn Config) -> ActionsConfig {
    let key = |name: &str| {
        config
            .get(name)
            .map(|raw| raw.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let (id, secret_hash, redirects) = (
        key(ACTIONS_ID_KEY),
        key(ACTIONS_SECRET_KEY),
        key(ACTIONS_REDIRECTS_KEY),
    );
    if id.is_none() && secret_hash.is_none() && redirects.is_none() {
        return ActionsConfig::Off;
    }
    let broken = |id: &Option<String>, reason: String| ActionsConfig::Broken {
        id: id.clone(),
        reason: format!(
            "oauth: the predefined Actions client is misconfigured ({reason}); set \
             {ACTIONS_ID_KEY}, {ACTIONS_SECRET_KEY} and {ACTIONS_REDIRECTS_KEY} together, \
             or none of them"
        ),
    };
    let Some(id) = id else {
        return broken(
            &None,
            format!("{ACTIONS_ID_KEY} is unset while the other keys are set"),
        );
    };
    if secret_hash.is_none() || redirects.is_none() {
        return broken(
            &Some(id),
            "every key of the three is required once any is set".to_owned(),
        );
    }
    let (secret_hash, redirects) = (secret_hash.expect("checked"), redirects.expect("checked"));
    if id.starts_with("lbc_") || id.starts_with("https://") {
        let reason = format!(
            "the client_id must not look like a dynamically registered (`lbc_…`) or a \
             document (`https://…`) one: {id:?}"
        );
        return broken(&Some(id), reason);
    }
    if id.len() > 128 || !id.bytes().all(|byte| byte.is_ascii_graphic()) {
        return broken(
            &Some(id),
            "the client_id must be 1 to 128 printable ASCII characters".to_owned(),
        );
    }
    if secret_hash.len() != 64
        || !secret_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return broken(
            &Some(id),
            format!(
                "{ACTIONS_SECRET_KEY} must be the 64-character lowercase-hex SHA-256 of \
                 the client secret"
            ),
        );
    }
    let uris: Vec<String> = redirects.split_whitespace().map(str::to_owned).collect();
    if uris.is_empty() {
        return broken(
            &Some(id),
            format!("{ACTIONS_REDIRECTS_KEY} names no redirect URI"),
        );
    }
    if uris.len() > MAX_REDIRECT_URIS || uris.iter().any(|uri| !valid_redirect_uri(uri)) {
        return broken(
            &Some(id),
            format!(
                "each of at most {MAX_REDIRECT_URIS} {ACTIONS_REDIRECTS_KEY} must be https \
                 (or http to localhost), ASCII, plain-host and fragment-free"
            ),
        );
    }
    let mut name = clean_name(config.get(ACTIONS_NAME_KEY).as_deref().unwrap_or_default())
        .chars()
        .take(MAX_CLIENT_NAME_CHARS)
        .collect::<String>();
    if name.is_empty() {
        name = DEFAULT_ACTIONS_NAME.to_owned();
    }
    ActionsConfig::On(ActionsClient {
        client: Client {
            client_id: id,
            client_name: name,
            redirect_uris: uris,
            metadata_expires_at: None,
        },
        secret_sha256: secret_hash,
    })
}

/// Whether `client_id` is this deployment's predefined Actions client: the
/// one client PKCE is optional for at `/authorize` and a secret is required
/// of at `/token`.
fn is_actions_client(st: &OAuthState, client_id: &str) -> bool {
    matches!(actions_config(&*st.config), ActionsConfig::On(actions) if actions.client.client_id == client_id)
}

/// The page every route answers on when the configured id is met but the
/// configuration is broken (see [`actions_config`]): fail closed, exactly as
/// a deployment with no limiter refuses CIMD clients, and say who can fix it.
fn actions_misconfigured_page() -> Response {
    error_page(
        500,
        "The Custom GPT Actions client is misconfigured",
        "This deployment's OAUTH_ACTIONS_* keys are set but do not make a usable \
         client, so nothing was approved or sent. The operator will find the \
         reason in the Worker's logs at startup.",
    )
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

/// The name a connector's token carries in Settings → API tokens, decided by
/// the redirect URI the code is being issued for: a host Living Brain
/// recognises as a connector platform gets the platform's label ("Claude
/// connector", "ChatGPT connector" — see [`connector_vendor`]), any other
/// host keeps only the name the app registered. Stored with the code, so the
/// token and any renewal carry the label of the redirect that was actually
/// approved.
fn connector_label(client_name: &str, redirect_uri: &str) -> String {
    let registered = clean_name(client_name);
    let name = if registered.is_empty() {
        DEFAULT_CLIENT_NAME.to_owned()
    } else {
        registered
    };
    let label = match redirect_host(redirect_uri)
        .as_deref()
        .and_then(connector_vendor)
    {
        Some(vendor) => format!("{} connector · {name}", vendor.label()),
        None => name,
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

/// The signed token naming the origin an authorization request arrived on. It
/// expires with the consent seal's window, so an old hop cannot be replayed.
fn seal_issuer(st: &OAuthState, origin: &str) -> String {
    let exp = st.clock.now().unix_timestamp() + CONSENT_TTL_SECS;
    let subject = json!({ "origin": origin, "exp": exp }).to_string();
    st.signer.sign(&cratefield_core::Payload {
        purpose: ISSUER_PURPOSE.to_owned(),
        subject,
        exp: None,
        kid: Kid::Cur,
    })
}

/// The origin a signed, unexpired issuer token names, or `None` for a forged,
/// stale or unrecognised one (the caller then keeps its own origin).
fn open_issuer(st: &OAuthState, token: &str) -> Option<String> {
    let payload = st.signer.verify(token, ISSUER_PURPOSE)?;
    let value: Value = serde_json::from_str(&payload.subject).ok()?;
    let origin = value.get("origin")?.as_str()?.to_owned();
    let exp = value.get("exp")?.as_i64()?;
    let fresh = exp > st.clock.now().unix_timestamp();
    (fresh && (origin.starts_with("https://") || origin.starts_with("http://"))).then_some(origin)
}

// ---------------------------------------------------------------------------
// The client an authorization names

/// The 400 page every unknown or unfetchable client gets. A Client ID
/// Metadata Document that cannot be fetched or validated is indistinguishable
/// from an unknown client from the outside, and neither ever redirects:
/// there is no validated redirect URI to send anything to.
fn unknown_client_page() -> Response {
    error_page(
        400,
        "Unknown app",
        "This app is not registered with Living Brain. Add it again from the app.",
    )
}

/// The page a client_id that is a document URL gets when this deployment
/// cannot check one at all: no rate limiter bound (the same fail-closed
/// posture registration has) or no `HttpClient` port. Everything else —
/// registration included — keeps working.
fn cimd_unavailable_page() -> Response {
    error_page(
        503,
        "This app cannot be checked",
        "This deployment cannot check an app that identifies itself by its own \
         document, so nothing was approved or sent. Try again later.",
    )
}

/// The client an authorization request names: the predefined Actions client
/// synthesized from config (issue #234) — never a stored row, so nothing can
/// shadow it and no purge can drop it — the registered row, which, for a
/// client_id that is a URL, is the cached Client ID Metadata Document, or a
/// freshly fetched document. A cached document past its expiry is fetched
/// again before the consent page is shown and before an approval is acted
/// on, so neither ever vouches for stale metadata; the token endpoint reads
/// the row alone and never fetches.
async fn client_for_authorize(
    st: &OAuthState,
    client_id: &str,
    headers: &HeaderMap,
) -> Result<Client, Box<Response>> {
    match actions_config(&*st.config) {
        ActionsConfig::On(actions) if actions.client.client_id == client_id => {
            return Ok(actions.client);
        }
        ActionsConfig::Broken { id: Some(id), .. } if id == client_id => {
            return Err(Box::new(actions_misconfigured_page()));
        }
        _ => {}
    }
    match st.store.client(client_id).await {
        Ok(Some(client)) => {
            let expired = client
                .metadata_expires_at
                .as_deref()
                .is_some_and(|expiry| !live_at(expiry, st.clock.now()));
            if expired {
                cimd_client(st, client_id, headers).await
            } else {
                Ok(client)
            }
        }
        Ok(None) => {
            // Only a client_id that is the shape of a document URL starts a
            // fetch; anything else is simply unknown.
            if cimd::validate_client_id(client_id).is_ok() {
                cimd_client(st, client_id, headers).await
            } else {
                Err(Box::new(unknown_client_page()))
            }
        }
        Err(_) => Err(Box::new(server_error_page())),
    }
}

/// Fetches, validates and caches the document a client_id URL names. Every
/// fetch — first or refresh — is rate-limited per calling address exactly as
/// registration is, and refuses outright with no limiter or no `HttpClient`
/// port, because it is an unauthenticated `/authorize` that triggers it; a
/// cache hit never reaches here, so browsing apps costs no allowance.
/// Failures are never cached: the caller gets the unknown-client page and
/// the next request starts the fetch over.
async fn cimd_client(
    st: &OAuthState,
    client_id: &str,
    headers: &HeaderMap,
) -> Result<Client, Box<Response>> {
    let Some(limiter) = st.limiter.as_ref() else {
        return Err(Box::new(cimd_unavailable_page()));
    };
    let keys: Vec<String> = rate_limit_keys(client_ip(headers).as_deref(), None)
        .into_iter()
        .map(|key| format!("oauth-cimd:{key}"))
        .collect();
    if let RateLimit::Denied { decision } =
        check_rate_limit(Some(limiter), &keys, RateLimitFailure::FailClosed).await
    {
        return Err(Box::new(cratefield_core::rate_limited(&decision)));
    }
    let Some(http) = st.http.as_ref() else {
        return Err(Box::new(cimd_unavailable_page()));
    };
    let metadata = match cimd::fetch(http.as_ref(), client_id).await {
        Ok(metadata) => metadata,
        Err(_) => return Err(Box::new(unknown_client_page())),
    };
    let now = st.clock.now();
    let ttl = i64::try_from(metadata.cache_secs).unwrap_or(3600);
    let client = Client {
        client_id: client_id.to_owned(),
        client_name: metadata.client_name,
        redirect_uris: metadata.redirect_uris,
        metadata_expires_at: Some(stamp(now + Duration::seconds(ttl))),
    };
    // The row's `unverified` flag follows the same host mapping a
    // registration's does; the daily cap it feeds is a registration limit,
    // so a document client never spends it — the fetch limit above is what
    // bounds this path.
    let unverified = client
        .redirect_uris
        .iter()
        .filter_map(|uri| redirect_host(uri))
        .any(|host| connector_vendor(&host).is_none());
    let address = hash(client_ip(headers).as_deref().unwrap_or("unknown"));
    if st
        .store
        .upsert_metadata_client(&client, &stamp(now), &address, unverified)
        .await
        .is_err()
    {
        return Err(Box::new(server_error_page()));
    }
    Ok(client)
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
    let client = match client_for_authorize(&st, client_id, &headers).await {
        Ok(client) => client,
        Err(page) => return *page,
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
    // PKCE S256 binds every public client's code (OAuth 2.1 has no `plain`).
    // The one confidential client — the predefined Actions client, whose
    // Custom GPT Actions cannot send a challenge — may ask without one: its
    // secret binds the code instead, checked at the token endpoint. A
    // challenge it *does* send must be S256 and is enforced there against
    // the stored value; the empty marker a challenge-less request seals is
    // redeemable by that client alone (see `exchange_code`), so no public
    // client can ever take the no-PKCE path.
    let challenge = match query.code_challenge.as_deref() {
        Some(challenge) if valid_challenge(challenge) => {
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
            challenge.to_owned()
        }
        Some(_) => {
            return authorize_error(
                StatusCode::FOUND,
                redirect_uri,
                state,
                &issuer,
                "invalid_request",
                "a valid code_challenge is required",
            );
        }
        None if is_actions_client(&st, client_id) => String::new(),
        None => {
            return authorize_error(
                StatusCode::FOUND,
                redirect_uri,
                state,
                &issuer,
                "invalid_request",
                "a code_challenge is required",
            );
        }
    };
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
        code_challenge: challenge,
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
        verified: connector_vendor(&host).is_some(),
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
    // The client is resolved again, not trusted from the seal: a cached
    // document past its expiry is refetched here, so an approval acts on
    // current metadata or none. A refusal is the page the GET would have
    // given, never a redirect to an unvalidated URI.
    let client = match client_for_authorize(&st, &sealed.client_id, &headers).await {
        Ok(client) => client,
        Err(page) => return *page,
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
            issue_code(&st, &sealed, &client.client_name, &choice).await
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
    client_name: &str,
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
        name: connector_label(client_name, &sealed.redirect_uri),
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
/// Three limits keep the table from being filled, and each has a reason:
/// re-registering an app with the same name and redirect set returns its
/// existing identity (no row, no allowance spent); one registering address may
/// create [`MAX_REGISTRATIONS_PER_IP_HOUR`] clients an hour, so one machine
/// cannot exhaust the rest; and the day's cap of
/// [`MAX_UNVERIFIED_REGISTRATIONS_PER_DAY`] counts only apps whose redirect
/// host is not one Living Brain recognises as a connector platform (see
/// [`connector_vendor`]), so a crowd of unverified apps cannot close
/// registration for everyone. Refuses outright when no rate limiter is bound.
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
    let mut uris: Vec<String> = match metadata.get("redirect_uris") {
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
    // The redirect set is compared as a set: sorted, so order does not matter.
    uris.sort();
    uris.dedup();
    let uris_json = serde_json::to_string(&uris).unwrap_or_default();
    let now = st.clock.now();

    // The same app again: its existing identity, no new row.
    match st
        .store
        .client_with_registration(&client_name, &uris_json)
        .await
    {
        Ok(Some(existing)) => return registered(&existing, now),
        Ok(None) => {}
        Err(_) => return server_error(),
    }

    let unverified = uris
        .iter()
        .filter_map(|uri| redirect_host(uri))
        .any(|host| connector_vendor(&host).is_none());
    let address = hash(client_ip(&headers).as_deref().unwrap_or("unknown"));
    let hour_ago = now - Duration::hours(1);
    match st
        .store
        .registrations_from(&address, &stamp(hour_ago))
        .await
    {
        Ok(count) if count < MAX_REGISTRATIONS_PER_IP_HOUR => {}
        Ok(_) => {
            return token_error(
                StatusCode::TOO_MANY_REQUESTS,
                "temporarily_unavailable",
                "this address has registered too many apps this hour; try again later",
            );
        }
        Err(_) => return server_error(),
    }
    if unverified {
        let day_start = now.replace_time(time::Time::MIDNIGHT);
        match st.store.clients_since(&stamp(day_start), true).await {
            Ok(count) if count < MAX_UNVERIFIED_REGISTRATIONS_PER_DAY => {}
            Ok(_) => {
                return token_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "temporarily_unavailable",
                    "too many apps have registered today; try again tomorrow",
                );
            }
            Err(_) => return server_error(),
        }
    }

    let Ok(id) = random_token(16) else {
        return server_error();
    };
    let client = Client {
        client_id: format!("lbc_{id}"),
        client_name,
        redirect_uris: uris,
        metadata_expires_at: None,
    };
    if st
        .store
        .insert_client(&client, &stamp(now), &address, unverified)
        .await
        .is_err()
    {
        return server_error();
    }
    registered(&client, now)
}

/// The RFC 7591 answer for a client, new or re-registered.
fn registered(client: &Client, now: OffsetDateTime) -> Response {
    (
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "client_id": client.client_id,
            "client_id_issued_at": now.unix_timestamp(),
            "client_name": client.client_name,
            "redirect_uris": client.redirect_uris,
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
    /// `client_secret_post`: the confidential client's secret in the form
    /// body. Refused for every public client (see [`token_client`]).
    client_secret: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
}

/// The one token endpoint. The device grant's poll is answered by the device
/// handler at `/v1/device-auth/token`; the venture routes a device poll there
/// before it reaches this module, so one advertised `token_endpoint` serves
/// every grant a client may use. A device poll that does arrive here is named
/// as unsupported rather than mis-answered.
async fn token(
    State(st): State<Arc<OAuthState>>,
    headers: HeaderMap,
    Form(form): Form<TokenForm>,
) -> Response {
    match form.grant_type.as_deref() {
        Some("authorization_code") => exchange_code(&st, &form, &headers).await,
        Some("refresh_token") => rotate_refresh(&st, &form, &headers).await,
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

/// The `Authorization: Basic` credentials a token request may carry (RFC
/// 6749 §2.3.1: standard base64 over the form-urlencoded id, a colon and the
/// form-urlencoded secret — see [`consent::basic_credentials`]). `Absent` for
/// a header that is not Basic auth; `Malformed` for a Basic payload that does
/// not decode, which every client that presents it is refused on.
enum BasicAuth {
    Absent,
    Malformed,
    Credentials { id: String, secret: String },
}

impl BasicAuth {
    fn id(&self) -> Option<&str> {
        match self {
            Self::Credentials { id, .. } => Some(id),
            _ => None,
        }
    }

    /// The secret half, when Basic carried a non-empty one.
    fn secret(&self) -> Option<&str> {
        match self {
            Self::Credentials { secret, .. } if !secret.is_empty() => Some(secret),
            _ => None,
        }
    }

    fn is_present(&self) -> bool {
        !matches!(self, Self::Absent)
    }
}

fn basic_auth(headers: &HeaderMap) -> BasicAuth {
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return BasicAuth::Absent;
    };
    let mut parts = value.splitn(2, ' ');
    let (scheme, payload) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    if !scheme.eq_ignore_ascii_case("basic") {
        return BasicAuth::Absent;
    }
    consent::basic_credentials(payload.as_bytes()).map_or(BasicAuth::Malformed, |(id, secret)| {
        BasicAuth::Credentials { id, secret }
    })
}

/// RFC 6749 §5.2's `invalid_client` — and, when the attempt came by the
/// `Authorization` header, the `WWW-Authenticate: Basic` a 401 must carry.
fn invalid_client(basic_used: bool, description: &str) -> Response {
    let mut response = token_error(StatusCode::UNAUTHORIZED, "invalid_client", description);
    if basic_used {
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"livingbrain\", charset=\"UTF-8\""),
        );
    }
    response
}

/// The client a token request names, with its authentication settled. Every
/// client this server registers — dynamically or by its document — is
/// public: PKCE and the client_id bind its codes, and a secret presented for
/// one is refused, so the Actions secret is never a way to pass as an app
/// someone else registered. The one confidential client is the predefined
/// Actions client (issue #234), which must present its secret, as
/// `client_secret_post` or `client_secret_basic`, before anything of its own
/// is read: the SHA-256 of what it presented is compared with the configured
/// hash in constant time.
struct TokenClient {
    client: Client,
    /// The predefined Actions client: confidential, so its tokens mint with
    /// the `actions` audience and a no-PKCE code of its own is redeemable.
    actions: bool,
}

async fn token_client(
    st: &OAuthState,
    form: &TokenForm,
    headers: &HeaderMap,
) -> Result<TokenClient, Box<Response>> {
    let basic = basic_auth(headers);
    let client_id = form
        .client_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .or_else(|| basic.id().filter(|id| !id.is_empty()).map(str::to_owned));
    let Some(client_id) = client_id else {
        return Err(Box::new(token_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "client_id is required",
        )));
    };
    // A Basic credential's id must agree with the client the form names.
    if let Some(basic_id) = basic.id()
        && basic_id != client_id
    {
        return Err(Box::new(invalid_client(
            true,
            "the Basic credentials name a different client",
        )));
    }
    let form_secret = form
        .client_secret
        .as_deref()
        .filter(|secret| !secret.is_empty());
    match actions_config(&*st.config) {
        ActionsConfig::On(actions) if actions.client.client_id == client_id => {
            // The form parameter first, the Basic header as the other way
            // round; either alone is what each method's client sends.
            let Some(presented) = form_secret.or(basic.secret()) else {
                return Err(Box::new(invalid_client(
                    basic.is_present(),
                    "this client requires its client secret",
                )));
            };
            if !constant_time_eq(hash(presented).as_bytes(), actions.secret_sha256.as_bytes()) {
                return Err(Box::new(invalid_client(
                    basic.is_present(),
                    "the client secret is not recognised",
                )));
            }
            Ok(TokenClient {
                client: actions.client,
                actions: true,
            })
        }
        ActionsConfig::Broken { id: Some(id), .. } if id == client_id => {
            Err(Box::new(token_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "the predefined Actions client is misconfigured on this deployment; \
                 nothing was issued",
            )))
        }
        _ => {
            if form_secret.is_some() || basic.secret().is_some() || basic.is_present() {
                return Err(Box::new(invalid_client(
                    basic.is_present(),
                    "this client authenticates with no secret",
                )));
            }
            match st.store.client(&client_id).await {
                Ok(Some(client)) => Ok(TokenClient {
                    client,
                    actions: false,
                }),
                Ok(None) => Err(Box::new(invalid_client(
                    false,
                    "the client is not registered",
                ))),
                Err(_) => Err(Box::new(server_error())),
            }
        }
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

async fn exchange_code(st: &OAuthState, form: &TokenForm, headers: &HeaderMap) -> Response {
    let client = match token_client(st, form, headers).await {
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
    // A challenge the code carries is enforced, whichever client redeems it.
    // The empty marker is the confidential Actions client's challenge-less
    // code (Custom GPT Actions send no PKCE), and only that client — which
    // authenticated by its secret above — may redeem one.
    let pkce_bound = if row.code_challenge.is_empty() {
        client.actions
    } else {
        verifier_matches(verifier, &row.code_challenge)
    };
    let bound = row.client_id == client.client.client_id
        && form.redirect_uri.as_deref() == Some(row.redirect_uri.as_str())
        && live_at(&row.expires_at, now)
        && pkce_bound;
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
    let access =
        match mint_access(st, &client, &row.workspace_id, &row.user_id, &row.name, now).await {
            Ok(access) => access,
            Err(_) => return server_error(),
        };
    let Ok(refresh) = issue_refresh(
        st,
        &Issued {
            name: &row.name,
            family: &row.family,
            prefix: &access.minted.prefix,
            client_id: &client.client.client_id,
            workspace_id: &row.workspace_id,
            user_id: &row.user_id,
        },
        now,
    )
    .await
    else {
        return server_error();
    };
    token_response(&access.minted.token, &refresh, &access.scope)
}

/// A connection's freshly minted access token, and the scope list it
/// actually carries — what the response echoes, so an actions token's
/// `read:`-only subset is named as it is, not as the grant asked.
struct MintedAccess {
    minted: crate::store::Minted,
    scope: String,
}

/// Mints the connection's access token: an actions token for the predefined
/// Actions client (audience `actions`, scopes forced `read:`-only), a
/// connector token for every public one. The same mint serves the code
/// exchange and every rotation of its renewal, so a connection's tokens
/// never change kind partway through.
async fn mint_access(
    st: &OAuthState,
    client: &TokenClient,
    workspace_id: &str,
    user_id: &str,
    name: &str,
    now: OffsetDateTime,
) -> Result<MintedAccess, DbError> {
    let granted = granted_scopes(user_id);
    if client.actions {
        let scope = TokenStore::read_only(&granted)?.join(" ");
        let minted = st
            .tokens
            .mint_actions(workspace_id, user_id, name, &granted, now + ACCESS_TTL)
            .await?;
        Ok(MintedAccess { minted, scope })
    } else {
        let scope = granted.join(" ");
        let minted = st
            .tokens
            .mint_connector(workspace_id, user_id, name, &granted, now + ACCESS_TTL)
            .await?;
        Ok(MintedAccess { minted, scope })
    }
}

/// What a renewal token is issued for: the connection it opens and the access
/// token it sits beside.
struct Issued<'a> {
    name: &'a str,
    family: &'a str,
    prefix: &'a str,
    client_id: &'a str,
    workspace_id: &'a str,
    user_id: &'a str,
}

async fn issue_refresh(
    st: &OAuthState,
    issued: &Issued<'_>,
    now: OffsetDateTime,
) -> Result<String, DbError> {
    let token = format!("lbr_{}", random_token(32)?);
    let row = RefreshRow {
        name: issued.name.to_owned(),
        family: issued.family.to_owned(),
        prefix: issued.prefix.to_owned(),
        client_id: issued.client_id.to_owned(),
        workspace_id: issued.workspace_id.to_owned(),
        user_id: issued.user_id.to_owned(),
        expires_at: stamp(now + REFRESH_TTL),
        used: false,
    };
    st.store
        .insert_refresh(&hash(&token), &row, &stamp(now))
        .await?;
    Ok(token)
}

async fn rotate_refresh(st: &OAuthState, form: &TokenForm, headers: &HeaderMap) -> Response {
    let client = match token_client(st, form, headers).await {
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
    if row.client_id != client.client.client_id || !live_at(&row.expires_at, now) {
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
    let access =
        match mint_access(st, &client, &row.workspace_id, &row.user_id, &row.name, now).await {
            Ok(access) => access,
            Err(_) => return server_error(),
        };
    let Ok(refresh) = issue_refresh(
        st,
        &Issued {
            name: &row.name,
            family: &row.family,
            prefix: &access.minted.prefix,
            client_id: &client.client.client_id,
            workspace_id: &row.workspace_id,
            user_id: &row.user_id,
        },
        now,
    )
    .await
    else {
        return server_error();
    };
    token_response(&access.minted.token, &refresh, &access.scope)
}
