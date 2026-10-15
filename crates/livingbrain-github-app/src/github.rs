//! The calls this module makes to GitHub, over the `HttpClient` port.
//!
//! Four kinds, and nothing else:
//!
//! - the manifest conversion (`POST /app-manifests/{code}/conversions`),
//!   which needs no credential — the one-hour `code` is the credential;
//! - app-level calls signed with an RS256 **app JWT** minted from the
//!   workspace's unsealed private key: confirming an installation
//!   (`GET /app/installations/{id}`) and minting an installation token
//!   (`POST /app/installations/{id}/access_tokens`);
//! - installation-level reads with that **installation token**: the
//!   repository list at install time, and the history the cron drain walks;
//! - for the shared app only, one OAuth hop at install time: the `code` is
//!   exchanged for a **user token** (`POST github.com/login/oauth/access_token`),
//!   which reads `GET /user` and `GET /user/installations` once and is then
//!   dropped. It proves which GitHub user is finishing the install; it is
//!   never cached, stored, logged or returned.
//!
//! No credential is ever logged, put in an error, stored in a table or
//! returned to a caller. An installation token lives in the per-isolate
//! [`TokenCache`] (memory only) until shortly before it expires. A
//! transport failure is reported as a fixed text, never the adapter's
//! message, so no header, URL or body can ride along into a stored error.
//!
//! **Timing note.** The JWT is signed with `rsa` 0.9 PKCS#1 v1.5, which
//! signs unblinded (RUSTSEC-2023-0071 concerns the decryption path; the
//! signing path carries no timing countermeasure either). The claims are
//! ours, never an attacker's, and a signature happens at most once per
//! installation token (cached ~50 minutes) — on a cron tick or an install
//! callback, not on an endpoint a stranger can call in a loop and time.

use std::collections::HashMap;
use std::sync::Mutex;

use cratefield_core::HttpClient;
use cratefield_core::axum::body::Bytes;
use cratefield_core::axum::http::{self, Method};
use cratefield_push_auth::Rs256Signer;
use livingbrain_github::rate::{RateLimit, parse_rate_limit};
use serde::Deserialize;
use serde_json::json;

use crate::seal::Secret;

/// The REST API.
pub(crate) const API: &str = "https://api.github.com";
/// The web origin: the manifest form, the install page.
pub(crate) const WEB: &str = "https://github.com";
/// The API version every call pins.
pub(crate) const API_VERSION: &str = "2022-11-28";
/// GitHub refuses a request with no `User-Agent`.
const USER_AGENT: &str = "livingbrain-github-app";
/// An installation token lives an hour; reuse it for at most this long.
const TOKEN_REUSE_SECS: i64 = 50 * 60;

/// Why a call to GitHub did not produce a usable answer. Every variant's
/// text is fixed or a status code: safe to store in `last_error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GhError {
    /// The request never got an answer (network, deadline, size cap).
    Transport,
    /// The private key would not parse as RSA.
    BadKey,
    /// GitHub answered, with this status.
    Status(u16),
    /// The answer was not the JSON expected.
    Shape,
}

impl std::fmt::Display for GhError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport => f.write_str("GitHub could not be reached"),
            Self::BadKey => f.write_str("the app's private key does not parse"),
            Self::Status(status) => write!(f, "GitHub answered {status}"),
            Self::Shape => f.write_str("GitHub answered with an unexpected body"),
        }
    }
}

/// One answer from GitHub: the status, the headers the drain reads, the
/// body. Never logged.
pub(crate) struct GhResponse {
    pub status: u16,
    headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl GhResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn rate(&self) -> RateLimit {
        let pairs: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        parse_rate_limit(&pairs)
    }

    /// The next page the `Link` header names, if any.
    pub fn next_page(&self) -> Option<u32> {
        self.header("link")
            .and_then(livingbrain_github::planner::parse_next_page)
    }

    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn json<T: for<'de> Deserialize<'de>>(&self) -> Result<T, GhError> {
        serde_json::from_slice(&self.body).map_err(|_| GhError::Shape)
    }
}

/// How a request authenticates.
pub(crate) enum Auth<'a> {
    None,
    /// An app JWT or an installation token, sent as `Bearer`.
    Bearer(&'a str),
}

/// Makes one call. `path` starts with `/` and is relative to [`API`].
pub(crate) async fn call(
    http: &dyn HttpClient,
    method: Method,
    path: &str,
    auth: Auth<'_>,
    if_none_match: Option<&str>,
) -> Result<GhResponse, GhError> {
    let mut builder = http::Request::builder()
        .method(method)
        .uri(format!("{API}{path}"))
        .header("accept", "application/vnd.github+json")
        .header("x-github-api-version", API_VERSION)
        .header("user-agent", USER_AGENT);
    if let Auth::Bearer(token) = auth {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(etag) = if_none_match {
        builder = builder.header("if-none-match", etag);
    }
    let request = builder.body(Bytes::new()).map_err(|_| GhError::Transport)?;
    let response = http.send(request).await.map_err(|_| GhError::Transport)?;
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            let name = name.as_str();
            let keep = matches!(
                name,
                "link" | "etag" | "x-ratelimit-remaining" | "x-ratelimit-reset" | "retry-after"
            );
            keep.then(|| Some((name.to_owned(), value.to_str().ok()?.to_owned())))
                .flatten()
        })
        .collect();
    Ok(GhResponse {
        status: response.status().as_u16(),
        headers,
        body: response.into_body(),
    })
}

/// An RS256 app JWT, valid from a minute ago (clock drift) for nine
/// minutes (GitHub allows ten).
pub(crate) fn app_jwt(private_key_pem: &Secret, app_id: i64, now: i64) -> Result<String, GhError> {
    let signer = Rs256Signer::from_pem(private_key_pem.expose()).map_err(|_| GhError::BadKey)?;
    Ok(signer.sign_jwt(
        &json!({"alg": "RS256", "typ": "JWT"}),
        &json!({"iat": now - 60, "exp": now + 9 * 60, "iss": app_id}),
    ))
}

/// What the manifest conversion hands back: the app and its three
/// credentials, as [`Secret`]s from the moment they are parsed.
pub(crate) struct Conversion {
    pub app_id: i64,
    pub slug: String,
    pub client_id: String,
    pub pem: Secret,
    pub webhook_secret: Secret,
    pub client_secret: Secret,
}

#[derive(Deserialize)]
struct ConversionWire {
    id: i64,
    slug: String,
    client_id: String,
    pem: String,
    webhook_secret: Option<String>,
    client_secret: String,
}

/// `POST /app-manifests/{code}/conversions`. `code` has been checked to be
/// a plain token before it reaches a URL.
pub(crate) async fn convert_manifest(
    http: &dyn HttpClient,
    code: &str,
) -> Result<Conversion, GhError> {
    let response = call(
        http,
        Method::POST,
        &format!("/app-manifests/{code}/conversions"),
        Auth::None,
        None,
    )
    .await?;
    if !response.ok() {
        return Err(GhError::Status(response.status));
    }
    let wire: ConversionWire = response.json()?;
    Ok(Conversion {
        app_id: wire.id,
        slug: wire.slug,
        client_id: wire.client_id,
        pem: Secret::new(wire.pem),
        webhook_secret: Secret::new(wire.webhook_secret.unwrap_or_default()),
        client_secret: Secret::new(wire.client_secret),
    })
}

/// An installation as `GET /app/installations/{id}` describes it.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InstallationWire {
    pub id: i64,
    pub app_id: i64,
    pub account: Option<AccountWire>,
    pub suspended_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AccountWire {
    pub login: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub id: Option<i64>,
}

/// Confirms with GitHub that `installation_id` is an installation of the
/// app whose key signs `jwt`. `Ok(None)` when GitHub says it is not (404),
/// or answers for a different app or installation.
pub(crate) async fn confirm_installation(
    http: &dyn HttpClient,
    jwt: &str,
    app_id: i64,
    installation_id: i64,
) -> Result<Option<InstallationWire>, GhError> {
    let response = call(
        http,
        Method::GET,
        &format!("/app/installations/{installation_id}"),
        Auth::Bearer(jwt),
        None,
    )
    .await?;
    if response.status == 404 {
        return Ok(None);
    }
    if !response.ok() {
        return Err(GhError::Status(response.status));
    }
    let wire: InstallationWire = response.json()?;
    Ok((wire.id == installation_id && wire.app_id == app_id).then_some(wire))
}

/// Installation tokens, per isolate, in memory only: `(app id,
/// installation id)` → the token and the unix second it stops being
/// reused. Never written anywhere.
#[derive(Default)]
pub(crate) struct TokenCache(Mutex<HashMap<(i64, i64), (Secret, i64)>>);

impl std::fmt::Debug for TokenCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenCache([redacted])")
    }
}

impl TokenCache {
    /// The cached token for one installation, if one is still fresh.
    pub fn cached(&self, app_id: i64, installation_id: i64, now: i64) -> Option<Secret> {
        let guard = self.0.lock().ok()?;
        guard
            .get(&(app_id, installation_id))
            .filter(|(_, until)| now < *until)
            .map(|(token, _)| Secret::new(token.expose().to_owned()))
    }

    fn put(&self, key: (i64, i64), token: &Secret, until: i64) {
        if let Ok(mut guard) = self.0.lock() {
            guard.retain(|_, (_, at)| *at > until - TOKEN_REUSE_SECS);
            guard.insert(key, (Secret::new(token.expose().to_owned()), until));
        }
    }

    /// Forgets every token of `app_id` (the app was removed).
    pub fn forget_app(&self, app_id: i64) {
        if let Ok(mut guard) = self.0.lock() {
            guard.retain(|(app, _), _| *app != app_id);
        }
    }

    /// Forgets one installation's token (GitHub refused it).
    pub fn forget(&self, app_id: i64, installation_id: i64) {
        if let Ok(mut guard) = self.0.lock() {
            guard.remove(&(app_id, installation_id));
        }
    }
}

#[derive(Deserialize)]
struct TokenWire {
    token: String,
}

/// An installation token: the cached one, or a fresh one minted with an
/// app JWT (`POST /app/installations/{id}/access_tokens`).
pub(crate) async fn installation_token(
    http: &dyn HttpClient,
    cache: &TokenCache,
    private_key_pem: &Secret,
    app_id: i64,
    installation_id: i64,
    now: i64,
) -> Result<Secret, GhError> {
    if let Some(token) = cache.cached(app_id, installation_id, now) {
        return Ok(token);
    }
    let jwt = app_jwt(private_key_pem, app_id, now)?;
    let response = call(
        http,
        Method::POST,
        &format!("/app/installations/{installation_id}/access_tokens"),
        Auth::Bearer(&jwt),
        None,
    )
    .await?;
    if !response.ok() {
        return Err(GhError::Status(response.status));
    }
    let wire: TokenWire = response.json()?;
    let token = Secret::new(wire.token);
    if token.expose().is_empty() {
        return Err(GhError::Shape);
    }
    cache.put((app_id, installation_id), &token, now + TOKEN_REUSE_SECS);
    Ok(token)
}

#[derive(Deserialize)]
struct RepoListWire {
    repositories: Vec<RepoWire>,
}

#[derive(Deserialize)]
pub(crate) struct RepoWire {
    pub id: i64,
    pub full_name: String,
}

/// The repositories an installation reaches (`GET /installation/repositories`),
/// at most `max_pages` pages of 100.
pub(crate) async fn installation_repositories(
    http: &dyn HttpClient,
    token: &Secret,
    max_pages: u32,
) -> Result<Vec<RepoWire>, GhError> {
    let mut out = Vec::new();
    let mut page = 1;
    while page <= max_pages {
        let response = call(
            http,
            Method::GET,
            &format!("/installation/repositories?per_page=100&page={page}"),
            Auth::Bearer(token.expose()),
            None,
        )
        .await?;
        if !response.ok() {
            return Err(GhError::Status(response.status));
        }
        let wire: RepoListWire = response.json()?;
        out.extend(wire.repositories);
        match response.next_page() {
            Some(next) if next > page => page = next,
            _ => break,
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// The shared app's OAuth hop

/// `application/x-www-form-urlencoded` / query-component encoding: every
/// byte but the unreserved set is percent-encoded.
pub(crate) fn url_encode(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// GitHub's authorize page for the shared app, sending the browser back to
/// `redirect_uri` with `code` and `state`.
pub(crate) fn authorize_url(client_id: &str, redirect_uri: &str, state: &str) -> String {
    format!(
        "{WEB}/login/oauth/authorize?client_id={}&redirect_uri={}&state={}",
        url_encode(client_id),
        url_encode(redirect_uri),
        url_encode(state),
    )
}

#[derive(Deserialize)]
struct OAuthWire {
    access_token: Option<String>,
}

/// Exchanges an OAuth `code` for a user token. The request carries the
/// client secret, so neither it nor the answer is ever logged; a refusal
/// (`{"error": …}`) is [`GhError::Shape`], a fixed text.
pub(crate) async fn exchange_oauth_code(
    http: &dyn HttpClient,
    client_id: &str,
    client_secret: &Secret,
    code: &str,
    redirect_uri: &str,
) -> Result<Secret, GhError> {
    // The standard OAuth form body; the answer is asked for as JSON.
    let body = Secret::new(format!(
        "client_id={}&client_secret={}&code={}&redirect_uri={}",
        url_encode(client_id),
        url_encode(client_secret.expose()),
        url_encode(code),
        url_encode(redirect_uri),
    ));
    let request = http::Request::builder()
        .method(Method::POST)
        .uri(format!("{WEB}/login/oauth/access_token"))
        .header("accept", "application/json")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("user-agent", USER_AGENT)
        .body(Bytes::from(body.expose().to_owned()))
        .map_err(|_| GhError::Transport)?;
    drop(body);
    let response = http.send(request).await.map_err(|_| GhError::Transport)?;
    if !response.status().is_success() {
        return Err(GhError::Status(response.status().as_u16()));
    }
    let wire: OAuthWire = serde_json::from_slice(response.body()).map_err(|_| GhError::Shape)?;
    match wire.access_token {
        Some(token) if !token.is_empty() => Ok(Secret::new(token)),
        _ => Err(GhError::Shape),
    }
}

#[derive(Deserialize)]
struct UserWire {
    id: i64,
}

/// The GitHub user a user token belongs to (`GET /user`): their numeric id.
pub(crate) async fn user_id(http: &dyn HttpClient, token: &Secret) -> Result<i64, GhError> {
    let response = call(
        http,
        Method::GET,
        "/user",
        Auth::Bearer(token.expose()),
        None,
    )
    .await?;
    if !response.ok() {
        return Err(GhError::Status(response.status));
    }
    let wire: UserWire = response.json()?;
    if wire.id > 0 {
        Ok(wire.id)
    } else {
        Err(GhError::Shape)
    }
}

#[derive(Deserialize)]
struct UserInstallationsWire {
    installations: Vec<InstallationWire>,
}

/// Whether `GET /user/installations` (the installations of this app the
/// user token can reach) lists `installation_id` under `app_id` — GitHub's
/// own check that the person is associated with the installation. At most
/// `max_pages` pages of 100.
pub(crate) async fn user_sees_installation(
    http: &dyn HttpClient,
    token: &Secret,
    app_id: i64,
    installation_id: i64,
    max_pages: u32,
) -> Result<bool, GhError> {
    let mut page = 1;
    while page <= max_pages {
        let response = call(
            http,
            Method::GET,
            &format!("/user/installations?per_page=100&page={page}"),
            Auth::Bearer(token.expose()),
            None,
        )
        .await?;
        if !response.ok() {
            return Err(GhError::Status(response.status));
        }
        let wire: UserInstallationsWire = response.json()?;
        if wire
            .installations
            .iter()
            .any(|i| i.id == installation_id && i.app_id == app_id)
        {
            return Ok(true);
        }
        match response.next_page() {
            Some(next) if next > page => page = next,
            _ => break,
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_authorize_url_encodes_every_part() {
        let url = authorize_url(
            "Iv23li.x",
            "https://app.livingbrain.wiki/v1/github/oauth/callback",
            "a&b=c",
        );
        assert_eq!(
            url,
            "https://github.com/login/oauth/authorize?client_id=Iv23li.x&redirect_uri=\
             https%3A%2F%2Fapp.livingbrain.wiki%2Fv1%2Fgithub%2Foauth%2Fcallback&state=a%26b%3Dc"
        );
    }
}
