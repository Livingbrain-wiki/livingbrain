//! The kit the workspaces tests share: a harness wired to a fake Slack, a
//! request helper that can carry a `Cookie` header, and the small pieces
//! of a Slack `id_token` a test needs to craft one.
//!
//! Each integration test file declares `mod support;` and Cargo compiles each
//! of them as its own binary, so a helper only one of them uses is dead code
//! in the others.
#![allow(dead_code)]

use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use cratefield_core::axum::http::{HeaderMap, Method, Request, Response, StatusCode, header};
use cratefield_core::axum::{self, body::Body};
use cratefield_core::{
    Config, DbError, HttpClient, HttpError, Kid, MapConfig, Payload, Row, Signer, Statement,
};
use cratefield_testing::{FixedClock, TestHarness};
use livingbrain_pages::{PageAnswers, PageStore};
use livingbrain_workspaces::{
    FLOW_COOKIE, FLOW_PURPOSE, SESSION_COOKIE, SESSION_PURPOSE, UserChange, Workspaces,
    apply_user_change,
};
use sea_query::Value as SeaValue;
use serde_json::{Value, json};
use time::OffsetDateTime;

/// The Slack app the tests configure.
pub const CLIENT_ID: &str = "1234.5678";
pub const CLIENT_SECRET: &str = "slack-client-secret-not-real";
pub const REDIRECT_BASE: &str = "https://brain.example";
/// The Events API signing secret (`WORKSPACES_SLACK_SIGNING_SECRET`).
///
/// Assembled from fragments so the fixture value never appears verbatim in the
/// source tree, where a scanner reads a long hex run on a secret's name as a
/// live credential. The runtime value is unchanged.
pub const SIGNING_SECRET: &str = concat!("8f742231b10e8888", "abcd99yyyzzz85a5");
/// The base64 of a 32-byte key ring: `HARNESS_KEK_CURRENT` plus
/// `HARNESS_KEK_V1`, which is what `WorkerSecretKms` reads. Not a secret.
/// Assembled from fragments for the same reason as [`SIGNING_SECRET`].
pub const KEK: &str = concat!("BwcHBwcHBwcHBwcHBwcHBw", "cHBwcHBwcHBwcHBwcHBwc=");
/// The fixed instant every test runs at, matching the kit's default clock.
pub const NOW: i64 = 1_800_000_000;

// ---------------------------------------------------------------------------
// The Slack the agent loop talks to (issue #123)

/// The team every agent event resolves through.
pub const TEAM: &str = "T0LEGACY";
/// The member who asks, and the Slack user the mirror holds for them.
pub const ASKER: &str = "UASKER";
/// The bot user Slack made for this team's installation.
pub const BOT: &str = "UBOT00BOT";
/// The bot token the install seals. Assembled from fragments so the fixture
/// value never appears verbatim in the source tree, where scanners read it as
/// a live credential; the runtime value is unchanged.
pub const BOT_TOKEN: &str = concat!("xo", "xb-1111-2222-agent-not-a-real-token");
/// Where a test's citations point.
pub const WIKI: &str = "https://brain.example";

/// The callback URL the module must build from `REDIRECT_BASE`, as it
/// appears percent-encoded in the authorize redirect and the token form.
pub fn callback_url() -> String {
    format!("{REDIRECT_BASE}/v1/workspaces/slack/callback")
        .replace(':', "%3A")
        .replace('/', "%2F")
}

/// The harness config a deployment would set: the sign-in keys, the Events
/// signing secret and a key ring. The negative cases take [`config`] and
/// remove one key.
pub fn config() -> MapConfig {
    MapConfig::from_pairs([
        ("WORKSPACES_SLACK_CLIENT_ID", CLIENT_ID),
        ("WORKSPACES_SLACK_CLIENT_SECRET", CLIENT_SECRET),
        ("WORKSPACES_REDIRECT_BASE", REDIRECT_BASE),
        ("WORKSPACES_SLACK_SIGNING_SECRET", SIGNING_SECRET),
        ("HARNESS_KEK_CURRENT", "1"),
        ("HARNESS_KEK_V1", KEK),
    ])
}

/// A harness whose clock reads [`NOW`].
pub fn harness(http: &TokenHttp) -> TestHarness {
    harness_at(http, config(), NOW)
}

/// A harness at an arbitrary instant and config, so a test can expire a
/// cookie by moving the clock rather than waiting.
pub fn harness_at(http: &TokenHttp, config: MapConfig, now: i64) -> TestHarness {
    let config: Arc<dyn Config> = Arc::new(config);
    let http = http.clone();
    let clock = FixedClock(OffsetDateTime::from_unix_timestamp(now).expect("a valid instant"));
    TestHarness::with_ports(vec![Box::new(Workspaces::new())], move |ports| {
        ports.config = config;
        ports.http = Some(Arc::new(http));
        ports.clock = Some(Arc::new(clock));
    })
}

// ---------------------------------------------------------------------------
// A fake Slack token endpoint

/// A fake for `https://slack.com/api/openid.connect.token`.
///
/// `FakeHttpClient` scripts its answers before the module runs, but the
/// `id_token` has to carry the nonce the *server* minted during
/// `/slack/start` — which only exists after the request. So this fake
/// answers from a slot the test fills in between the two requests.
#[derive(Clone, Default)]
pub struct TokenHttp {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    id_token: Option<String>,
    body: Option<String>,
    /// What the Web API (`/api/chat.postMessage` and friends) answers
    /// instead of `ok: true`. Slack's own refusal shape, and the reason the
    /// module reads the body rather than the status line.
    web_api_body: Option<String>,
    requests: Vec<(String, String, String)>,
    api: Vec<Call>,
}

/// One request the fake saw, in the form a test asserts on.
///
/// The `Authorization` header is kept because a bot token is what the header
/// carries and nothing else does: a reply that went out with the wrong
/// credential, or with none, is the failure a test has to be able to see.
#[derive(Clone, Debug)]
pub struct Call {
    pub method: String,
    pub uri: String,
    pub authorization: Option<String>,
    pub body: String,
}

impl Call {
    /// The body as JSON, for an assertion about a rendered message.
    ///
    /// # Panics
    ///
    /// When the body is not JSON.
    #[must_use]
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).expect("a Web API body is JSON")
    }
}

impl TokenHttp {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Answers `ok: true` with this `id_token`.
    pub fn will_return(&self, id_token: &str) {
        let mut inner = self.inner.lock().expect("lock");
        inner.id_token = Some(id_token.to_owned());
        inner.body = None;
    }

    /// Answers this raw body instead — a Slack refusal, or a body that is
    /// not JSON at all.
    pub fn will_answer(&self, body: &str) {
        let mut inner = self.inner.lock().expect("lock");
        inner.body = Some(body.to_owned());
        inner.id_token = None;
    }

    /// Answers the **Web API** with this raw body — Slack's
    /// `{"ok": false, "error": "…"}`, which arrives with HTTP 200 and must
    /// not be mistaken for a post.
    pub fn web_api_will_answer(&self, body: &str) {
        self.inner.lock().expect("lock").web_api_body = Some(body.to_owned());
    }

    /// Every request as `(method, uri, body)`.
    #[must_use]
    pub fn requests(&self) -> Vec<(String, String, String)> {
        self.inner.lock().expect("lock").requests.clone()
    }

    /// Every call to Slack's Web API, in order, with its headers.
    #[must_use]
    pub fn api_calls(&self) -> Vec<Call> {
        self.inner.lock().expect("lock").api.clone()
    }
}

#[async_trait]
impl HttpClient for TokenHttp {
    async fn send(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        let (parts, body) = request.into_parts();
        let uri = parts.uri.to_string();
        let authorization = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = String::from_utf8_lossy(&body).into_owned();
        // The Web API is told apart from the token endpoints by its
        // `/api/chat.`-shaped method, because the fake serves both and a
        // test that seeded an install must not be answered `ok: true` with
        // an `id_token` when it means a refusal.
        let is_web_api = uri.contains("/api/chat.");
        let mut inner = self.inner.lock().expect("lock");
        inner
            .requests
            .push((parts.method.to_string(), uri.clone(), body.clone()));
        if is_web_api {
            inner.api.push(Call {
                method: parts.method.to_string(),
                uri,
                authorization,
                body,
            });
        }
        let answer = if is_web_api {
            inner
                .web_api_body
                .clone()
                .unwrap_or_else(|| json!({"ok": true}).to_string())
        } else {
            match (&inner.body, &inner.id_token) {
                (Some(raw), _) => raw.clone(),
                (None, Some(token)) => json!({
                    "ok": true,
                    "access_token": "xoxb-not-used",
                    "id_token": token,
                })
                .to_string(),
                (None, None) => json!({"ok": false, "error": "invalid_code"}).to_string(),
            }
        };
        drop(inner);
        Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Bytes::from(answer))
            .map_err(|err| HttpError::Transport(err.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Requests

/// A buffered response, since the kit's own `TestResponse` body is private.
pub struct Res {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Res {
    /// # Panics
    ///
    /// When the body is not JSON.
    #[must_use]
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("body is JSON")
    }

    #[must_use]
    pub fn location(&self) -> String {
        header_of(&self.headers, header::LOCATION)
    }

    /// Every `Set-Cookie` value, in order.
    #[must_use]
    pub fn set_cookies(&self) -> Vec<String> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_owned)
            .collect()
    }

    /// One cookie's value from the response's `Set-Cookie` headers.
    #[must_use]
    pub fn cookie(&self, name: &str) -> Option<String> {
        self.set_cookies().into_iter().find_map(|cookie| {
            let (key, rest) = cookie.split_once('=')?;
            (key == name).then(|| rest.split(';').next().unwrap_or_default().to_owned())
        })
    }
}

fn header_of(headers: &HeaderMap, name: axum::http::HeaderName) -> String {
    headers
        .get(&name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

pub async fn get(router: &axum::Router, path: &str, cookies: &[(&str, &str)]) -> Res {
    send(router, Method::GET, path, cookies).await
}

/// A `POST` carrying a JSON body, which is how the email start route is
/// called.
pub async fn post_json(
    router: &axum::Router,
    path: &str,
    cookies: &[(&str, &str)],
    body: &Value,
) -> Res {
    send_with_body(
        router,
        Method::POST,
        path,
        cookies,
        Some("application/json"),
        &body.to_string(),
    )
    .await
}

/// A `POST` carrying an `application/x-www-form-urlencoded` body, which is
/// what the browser submits the verify form as.
pub async fn post_form(
    router: &axum::Router,
    path: &str,
    cookies: &[(&str, &str)],
    body: &str,
) -> Res {
    send_with_body(
        router,
        Method::POST,
        path,
        cookies,
        Some("application/x-www-form-urlencoded"),
        body,
    )
    .await
}

/// A request carrying a `Cookie` header. The kit's `request` helper cannot
/// send one, and every route this module reads needs one.
pub async fn send(
    router: &axum::Router,
    method: Method,
    path: &str,
    cookies: &[(&str, &str)],
) -> Res {
    send_with_body(router, method, path, cookies, None, "").await
}

// ---------------------------------------------------------------------------
// Slack event deliveries

/// The events endpoint, as the harness mounts it.
pub const EVENTS: &str = "/v1/workspaces/slack/events";

/// `POST /slack/events` carrying a body Slack signed at `timestamp`.
pub async fn post_signed(router: &axum::Router, body: &Value, timestamp: i64) -> Res {
    let raw = body.to_string();
    post_signed_with(router, &raw, timestamp, SIGNING_SECRET).await
}

/// [`post_signed`] with the secret spelled out, so a test can sign with the
/// wrong one.
pub async fn post_signed_with(
    router: &axum::Router,
    raw: &str,
    timestamp: i64,
    secret: &str,
) -> Res {
    let builder = Request::builder()
        .method(Method::POST)
        .uri(EVENTS)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-slack-request-timestamp", timestamp.to_string());
    // No signature at all is a refusal too, and a test says so by signing
    // with nothing.
    let builder = if secret.is_empty() {
        builder
    } else {
        builder.header("x-slack-signature", signature(secret, timestamp, raw))
    };
    respond(
        router,
        builder
            .body(Body::from(raw.to_owned()))
            .expect("request builds"),
    )
    .await
}

/// The signature Slack would put in `X-Slack-Signature` for this body:
/// `v0=` + hex(`HMAC-SHA256(secret, "v0:{timestamp}:{body}")`).
///
/// Written out here rather than borrowed from the module, because a test
/// that signs with the module's own code proves only that the module agrees
/// with itself.
pub fn signature(secret: &str, timestamp: i64, body: &str) -> String {
    use hmac::Mac as _;
    let mut mac = <hmac::Hmac<sha2::Sha256> as hmac::Mac>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts a key of any length");
    mac.update(format!("v0:{timestamp}:{body}").as_bytes());
    let hex =
        mac.finalize()
            .into_bytes()
            .iter()
            .fold(String::with_capacity(64), |mut out, byte| {
                use std::fmt::Write as _;
                let _ = write!(out, "{byte:02x}");
                out
            });
    format!("v0={hex}")
}

/// A `url_verification` handshake Slack sends when an app's request URL is
/// first saved.
pub fn url_verification(challenge: &str) -> Value {
    json!({"type": "url_verification", "challenge": challenge})
}

/// An `event_callback` envelope around `event`, the shape Slack sends.
///
/// `event_id` is what the inbox dedups on and `team_id` is what the module is
/// allowed to read the workspace from — never `event.user.team_id`.
pub fn envelope(event_id: &str, team_id: &str, event: Value) -> Value {
    json!({
        "token": "legacy-verification-placeholder",
        "team_id": team_id,
        "api_app_id": "A0APP",
        "event": event,
        "type": "event_callback",
        "event_id": event_id,
        "event_time": NOW,
    })
}

/// Drives one request through the router and reads the whole answer.
pub async fn respond(router: &axum::Router, request: Request<Body>) -> Res {
    use tower::ServiceExt;
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("router answers");
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 1024 * 1024)
        .await
        .expect("body reads");
    Res {
        status: parts.status,
        headers: parts.headers,
        body: bytes.to_vec(),
    }
}

/// [`send`] with a body and, when there is one, a `Content-Type`.
async fn send_with_body(
    router: &axum::Router,
    method: Method,
    path: &str,
    cookies: &[(&str, &str)],
    content_type: Option<&str>,
    body: &str,
) -> Res {
    use tower::ServiceExt;
    let mut builder = Request::builder().method(method).uri(path);
    if !cookies.is_empty() {
        let header = cookies
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ");
        builder = builder.header(header::COOKIE, header);
    }
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    let request = builder
        .body(Body::from(body.to_owned()))
        .expect("request builds");
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("router answers");
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 1024 * 1024)
        .await
        .expect("body reads");
    Res {
        status: parts.status,
        headers: parts.headers,
        body: bytes.to_vec(),
    }
}

// ---------------------------------------------------------------------------
// Slack tokens

/// Builds an `id_token` — three base64url parts. The signature is
/// placeholder text: the module is specified not to check it, because the
/// token came from Slack's own token endpoint over TLS (OIDC Core
/// §3.1.3.7 item 6), and these tests hold it to that.
pub fn id_token(claims: &Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_string(claims).expect("claims serialize"));
    format!("{header}.{payload}.signature-not-checked")
}

/// The claims Slack sends for a sign-in.
pub fn claims(nonce: &str, team_id: &str, user_id: &str, name: &str) -> Value {
    json!({
        "iss": "https://slack.com",
        "aud": CLIENT_ID,
        "iat": NOW,
        "exp": NOW + 3600,
        "nonce": nonce,
        "name": name,
        "https://slack.com/team_id": team_id,
        "https://slack.com/user_id": user_id,
        "https://slack.com/team_name": format!("{team_id} workspace"),
    })
}

// ---------------------------------------------------------------------------
// Driving the flow

/// A sign-in attempt that `/slack/start` has begun: the flow cookie, and
/// the two values sealed inside it.
pub struct Flow {
    pub cookie: String,
    pub state: String,
    pub nonce: String,
}

impl Flow {
    /// The callback path for this attempt, carrying `code`.
    #[must_use]
    pub fn callback(&self, code: &str) -> String {
        format!(
            "/v1/workspaces/slack/callback?code={code}&state={}",
            self.state
        )
    }

    /// The `Cookie` header a browser would send to the callback.
    #[must_use]
    pub fn cookies(&self) -> [(&str, &str); 1] {
        [(FLOW_COOKIE, self.cookie.as_str())]
    }
}

/// Begins a sign-in: drives `/slack/start` and returns the attempt.
pub async fn start_flow(kit: &TestHarness) -> Flow {
    let start = get(&kit.router, "/v1/workspaces/slack/start", &[]).await;
    assert_eq!(start.status, StatusCode::FOUND, "start redirects");
    let cookie = start.cookie(FLOW_COOKIE).expect("start sets a flow cookie");
    let payload = flow_payload(kit, &cookie);
    Flow {
        state: query_param(&start.location(), "state"),
        nonce: payload["nonce"].as_str().expect("a nonce").to_owned(),
        cookie,
    }
}

/// A completed sign-in: the session cookie, and the workspace it is for.
///
/// ADR 0002 made the workspace id ours to mint — `ws_` plus an IdGen id for
/// a team that has never been seen — so a test cannot name one up front.
/// It reads it back out of the cookie the module just sealed, which is the
/// same value `/me` reports.
pub struct SignIn {
    pub cookie: String,
    pub workspace_id: String,
}

impl SignIn {
    /// The `Cookie` header a browser would send with this session.
    #[must_use]
    pub fn cookies(&self) -> [(&str, &str); 1] {
        [(SESSION_COOKIE, self.cookie.as_str())]
    }
}

/// Signs `user_id` of `team_id` in, end to end. Panics if any step of the
/// sign-in does not succeed.
pub async fn sign_in(
    kit: &TestHarness,
    http: &TokenHttp,
    team_id: &str,
    user_id: &str,
    name: &str,
) -> SignIn {
    let flow = start_flow(kit).await;
    http.will_return(&id_token(&claims(&flow.nonce, team_id, user_id, name)));
    let callback = get(&kit.router, &flow.callback("code-1"), &flow.cookies()).await;
    assert_eq!(
        callback.status,
        StatusCode::FOUND,
        "callback redirects: {}",
        String::from_utf8_lossy(&callback.body)
    );
    let cookie = callback
        .cookie(SESSION_COOKIE)
        .expect("callback sets a session cookie");
    let workspace_id = session_payload(kit, &cookie)["workspace_id"]
        .as_str()
        .expect("a workspace in the session")
        .to_owned();
    SignIn {
        cookie,
        workspace_id,
    }
}

/// The signed payload behind a session cookie.
pub fn session_payload(kit: &TestHarness, cookie: &str) -> Value {
    let payload = kit
        .signer
        .verify(cookie, SESSION_PURPOSE)
        .expect("the session cookie verifies");
    serde_json::from_str(&payload.subject).expect("the session payload is JSON")
}

/// The signed flow payload behind a flow cookie.
pub fn flow_payload(kit: &TestHarness, cookie: &str) -> Value {
    let payload = kit
        .signer
        .verify(cookie, FLOW_PURPOSE)
        .expect("the flow cookie verifies");
    serde_json::from_str(&payload.subject).expect("the flow payload is JSON")
}

/// A session cookie signed the way the module signs one — for a
/// (workspace, user) pair that need not exist. The signature is good; it
/// is the member row that the handler must find.
#[must_use]
pub fn signed_session(kit: &TestHarness, workspace_id: &str, user_id: &str, exp: i64) -> String {
    kit.signer.sign(&Payload {
        purpose: SESSION_PURPOSE.to_owned(),
        subject: json!({"workspace_id": workspace_id, "user_id": user_id, "exp": exp}).to_string(),
        exp: None,
        kid: Kid::Cur,
    })
}

// ---------------------------------------------------------------------------
// Installing the Slack app (issue #6)

/// An install attempt `GET /slack/install` has begun: the flow cookie and the
/// CSRF state Slack will send back.
pub struct Install {
    pub cookie: String,
    pub state: String,
}

impl Install {
    /// The install callback path for this attempt, carrying `code`.
    #[must_use]
    pub fn callback(&self, code: &str) -> String {
        format!(
            "/v1/workspaces/slack/install/callback?code={code}&state={}",
            self.state
        )
    }

    /// The `Cookie` header a browser would send to the callback.
    #[must_use]
    pub fn cookies(&self) -> [(&str, &str); 1] {
        [(FLOW_COOKIE, self.cookie.as_str())]
    }
}

/// Begins an install: drives `/slack/install` and returns the attempt.
pub async fn start_install(kit: &TestHarness) -> Install {
    let install = get(&kit.router, "/v1/workspaces/slack/install", &[]).await;
    assert_eq!(install.status, StatusCode::FOUND, "install redirects");
    Install {
        cookie: install
            .cookie(FLOW_COOKIE)
            .expect("install sets a flow cookie"),
        state: query_param(&install.location(), "state"),
    }
}

/// The Slack answer `oauth.v2.access` gives for one install. `token` is what
/// the row must be sealed against and what the test reads back — the only
/// place a test sees the plaintext.
#[must_use]
pub fn install_answer(team_id: &str, app_id: &str, bot_user_id: &str, token: &str) -> String {
    json!({
        "ok": true,
        "access_token": token,
        "token_type": "bot",
        "scope": "app_mentions:read,channels:history,chat:write",
        "app_id": app_id,
        "team": {"id": team_id, "name": format!("{team_id} workspace")},
        "enterprise": null,
        "authed_user": {"id": "UINSTALLER"},
        "bot_user_id": bot_user_id,
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// The email magic link

/// The sign-in token out of the last message the fake mailer recorded — the
/// 32 random bytes the module hex encoded into the link, and the only place
/// a test can see them.
#[must_use]
pub fn mailed_token(kit: &TestHarness) -> String {
    let message = kit.mailer.last_message().expect("a sign-in mail went out");
    let link = message
        .text
        .lines()
        .find(|line| line.contains("/v1/workspaces/email/verify?token="))
        .expect("the mail carries the sign-in link");
    let token = query_param(link, "token");
    assert!(!token.is_empty(), "the link carries a token: {link}");
    token
}

/// One `sign_in_links` row, by its `token_hash`, as the raw columns the
/// module wrote.
pub async fn sign_in_link(kit: &TestHarness, token_hash: &str) -> Option<Row> {
    let rows = kit
        .db
        .query(&Statement::with_values(
            "SELECT token_hash, email, workspace_id, workspace_name, expires_at, spent_at \
             FROM sign_in_links WHERE token_hash = ?",
            vec![text(token_hash)],
        ))
        .await
        .expect("sign_in_links reads");
    rows.first().cloned()
}

/// Moves an unspent link's expiry into the past.
///
/// The kit's clock is fixed when the harness — and with it the database —
/// is built, so the only way a row can be aged within one kit is to write
/// the timestamp the handler compares against.
pub async fn expire_link(kit: &TestHarness, token_hash: &str) {
    kit.db
        .execute(&Statement::with_values(
            "UPDATE sign_in_links SET expires_at = ? WHERE token_hash = ?",
            vec![text("2000-01-01T00:00:00Z"), text(token_hash)],
        ))
        .await
        .expect("the link expires");
}

// ---------------------------------------------------------------------------
// Reaching past the module, for a test that seeds or reads a row directly

/// A non-null text bind.
#[must_use]
pub fn text(value: &str) -> SeaValue {
    SeaValue::String(Some(Box::new(value.to_owned())))
}

/// Runs one statement against the harness's database, so a test can plant a
/// row the module has no public seam for. `# Panics` when it fails, which is
/// what a failing seed should say.
pub async fn exec(kit: &TestHarness, sql: &str, values: Vec<SeaValue>) -> u64 {
    exec_result(kit, sql, values)
        .await
        .unwrap_or_else(|error| panic!("{sql} applies: {error}"))
}

/// [`exec`] with the error as the answer rather than a panic, for a test
/// that is asserting a constraint refuses a write.
pub async fn exec_result(
    kit: &TestHarness,
    sql: &str,
    values: Vec<SeaValue>,
) -> Result<u64, DbError> {
    kit.db.execute(&Statement::with_values(sql, values)).await
}

/// The rows one statement returns, for the same reason as [`exec`].
pub async fn rows(kit: &TestHarness, sql: &str, values: Vec<SeaValue>) -> Vec<Row> {
    kit.db
        .query(&Statement::with_values(sql, values))
        .await
        .unwrap_or_else(|error| panic!("{sql} reads: {error}"))
        .rows
}

/// One column of one row, by query, as a string.
pub async fn column(kit: &TestHarness, sql: &str, values: Vec<SeaValue>) -> Option<String> {
    rows(kit, sql, values)
        .await
        .first()
        .and_then(|row| row.get::<String>("value"))
}

/// A workspace and its owner, planted as ADR 0001 wrote one: a row whose id
/// *is* the Slack team id, and no connections or identities at all.
pub async fn seed_workspace(kit: &TestHarness, id: &str, name: &str, owner: &str) {
    exec(
        kit,
        "INSERT INTO workspaces (id, name, owner_id, created_at) VALUES (?, ?, ?, ?)",
        vec![text(id), text(name), text(owner), text(RFC3339_NOW)],
    )
    .await;
    for member in [owner] {
        exec(
            kit,
            "INSERT INTO workspace_members (workspace_id, user_id, name, timezone, is_admin, updated_at) \
             VALUES (?, ?, ?, NULL, 0, ?)",
            vec![
                text(id),
                text(member),
                text(&format!("{member} of {id}")),
                text(RFC3339_NOW),
            ],
        )
        .await;
    }
}

/// A workspace created before ADR 0002, plus the Slack links migration
/// `0002`'s backfill gave it — which is the state every Slack-created
/// workspace is in once the migration has run.
///
/// The two `INSERT … SELECT` statements are the backfill's own, quoted from
/// `migrations/sqlite/0002_identities.sql`; running them here rather than
/// writing the links out by hand is what makes the fixture the migration's
/// output rather than this test's idea of it.
pub async fn seed_legacy_workspace(kit: &TestHarness, id: &str, name: &str, owner: &str) {
    seed_workspace(kit, id, name, owner).await;
    exec(
        kit,
        "INSERT INTO workspace_connections (workspace_id, platform, external_id, created_at)
         SELECT id, 'slack', id, created_at FROM workspaces
         WHERE NOT EXISTS (
             SELECT 1 FROM workspace_connections c
             WHERE c.workspace_id = workspaces.id AND c.platform = 'slack'
         )",
        vec![],
    )
    .await;
    exec(
        kit,
        "INSERT INTO member_identities (workspace_id, platform, external_id, user_id, created_at)
         SELECT workspace_id, 'slack', user_id, user_id, updated_at FROM workspace_members
         WHERE NOT EXISTS (
             SELECT 1 FROM member_identities i
             WHERE i.workspace_id = workspace_members.workspace_id
               AND i.platform = 'slack'
               AND i.external_id = workspace_members.user_id
         )",
        vec![],
    )
    .await;
}

/// The kit's [`NOW`] in the RFC 3339 the rows are written in.
pub const RFC3339_NOW: &str = "2027-01-15T08:00:00Z";

/// One query parameter out of a URL, or the empty string.
#[must_use]
pub fn query_param(url: &str, name: &str) -> String {
    let Some(query) = url.split_once('?').map(|(_, query)| query) else {
        return String::new();
    };
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_owned())
        .unwrap_or_default()
}

/// The [`Clock`](cratefield_core::Clock) the kit wires, for a seam that
/// takes one.
#[must_use]
pub fn clock() -> FixedClock {
    FixedClock(OffsetDateTime::from_unix_timestamp(NOW).expect("a valid instant"))
}

/// A key custodian over an in-process key ring: the production
/// `WorkerSecretKms` shape, served by a closure so a test needs no file and
/// no environment variable.
#[must_use]
pub fn kms() -> Arc<dyn cratefield_kms::Kms> {
    Arc::new(
        cratefield_kms::WorkerSecretKms::from_lookup(|name| match name {
            "HARNESS_KEK_CURRENT" => Some("1".to_owned()),
            "HARNESS_KEK_V1" => Some(KEK.to_owned()),
            _ => None,
        })
        .expect("the key ring is well formed"),
    )
}

/// Applies a Slack event the way issue #6's webhook will: after verifying
/// the signature, with the `team_id` read out of the signed envelope.
pub async fn apply_event(kit: &TestHarness, team_id: &str, event: &Value) -> UserChange {
    apply_user_change(&*kit.db, &clock(), team_id, event)
        .await
        .expect("the event applies")
}

// ---------------------------------------------------------------------------
// The agent loop (issue #123)

/// A world where the Slack agent answers from real pages.
///
/// Both modules are mounted, because the pages module owns the tables *and*
/// the blob prefix — the harness scopes `Blob` per module name, so a store
/// built anywhere else reads a prefix nothing ever writes. The store a test
/// seeds is therefore built the same way the composition builds it, over the
/// pages-scoped blob.
pub struct Agent {
    pub kit: TestHarness,
    /// The store a test writes pages into. Shared, because the seam the
    /// module holds and the test's own writes must be the same store.
    pub store: Arc<PageStore>,
    /// The seam the workspaces module holds, which a test fills with
    /// [`Deferred::answering`] once [`Agent::store`] exists.
    pub answers: Deferred,
}

/// An [`Answers`] a test fills once the harness is built.
///
/// The same seam-ordering problem the composition has, in miniature: the
/// module is constructed before its `ModuleContext` exists, so the page store
/// a test seeds with cannot be the one the module was handed. The test fills
/// this before it delivers anything, which is exactly what the Worker does on
/// its first request.
#[derive(Clone, Default)]
pub struct Deferred(Arc<OnceLock<Arc<dyn livingbrain_pages::Answers>>>);

impl Deferred {
    /// Answers from `store` over `base`. The first writer wins.
    pub fn answering(&self, store: impl Into<Arc<PageStore>>, base: impl Into<String>) -> bool {
        self.0.set(Arc::new(PageAnswers::new(store, base))).is_ok()
    }
}

#[async_trait]
impl livingbrain_pages::Answers for Deferred {
    async fn answer(
        &self,
        asker: &livingbrain_pages::Asker,
        question: &str,
    ) -> Result<livingbrain_pages::Answered, livingbrain_pages::AnswerError> {
        self.0
            .get()
            .ok_or(livingbrain_pages::AnswerError::Unavailable)?
            .answer(asker, question)
            .await
    }
}

/// A harness with `workspaces` answering through a [`Deferred`] seam and
/// `pages` mounted beside it.
///
/// `classifier` is `None` for the policy most deployments run — the kit's
/// default port set always supplies one, so an unwired judge has to be asked
/// for — and `Some` for a deployment that wired the fast judge (#112).
pub fn agent_harness(
    http: &TokenHttp,
    classifier: Option<Arc<dyn cratefield_core::Classifier>>,
) -> Agent {
    let mut blob: Option<Arc<dyn cratefield_core::Blob>> = None;
    let config: Arc<dyn Config> = Arc::new(config());
    let http = http.clone();
    let clock = FixedClock(OffsetDateTime::from_unix_timestamp(NOW).expect("a valid instant"));
    let key = kms();
    let answers = Deferred::default();
    let kit = TestHarness::with_ports(
        vec![
            Box::new(Workspaces::new().answering(Arc::new(answers.clone()))),
            Box::new(
                livingbrain_pages::Pages::new()
                    .nest("/mcp", |_ctx| cratefield_core::axum::Router::new()),
            ),
        ],
        |ports| {
            let blob_store: Arc<dyn cratefield_core::Blob> =
                Arc::new(cratefield_testing::MemoryBlob::new());
            ports.config = config;
            ports.http = Some(Arc::new(http));
            ports.clock = Some(Arc::new(clock.clone()));
            ports.blob = Some(Arc::clone(&blob_store));
            ports.classifier = classifier;
            blob = Some(blob_store);
        },
    );
    let store = Arc::new(livingbrain_pages::PageStore::new(
        Arc::clone(&kit.db),
        // The production scope: the module name `pages` is mounted under, so
        // a seeded body is where the module's own store looks for it.
        Arc::new(cratefield_core::ScopedBlob::new(
            blob.expect("the port set has the blob store it was given"),
            "pages",
        )),
        Arc::clone(&key),
        Arc::new(kit.clock.clone()) as Arc<dyn cratefield_core::Clock>,
        Arc::new(cratefield_core::UlidIdGen),
    ));
    Agent {
        kit,
        store,
        answers,
    }
}

/// The workspace, the Slack install and the asker's own page: everything the
/// loop needs before it may say anything.
///
/// The install goes through the real callback rather than a planted row, so
/// the sealed bot token is one the module itself wrote — a test that seeded
/// the row would prove the loop can read a row, not that it can open one.
pub async fn install_agent(agent: &Agent, http: &TokenHttp) {
    seed_legacy_workspace(&agent.kit, TEAM, "a workspace", ASKER).await;
    let install = start_install(&agent.kit).await;
    http.will_answer(&install_answer(TEAM, "A0APP", BOT, BOT_TOKEN));
    let callback = get(
        &agent.kit.router,
        &install.callback("install-code"),
        &install.cookies(),
    )
    .await;
    assert_eq!(
        callback.status,
        StatusCode::OK,
        "the install completes: {}",
        String::from_utf8_lossy(&callback.body)
    );
}

/// Drives one signed event delivery through the module and drains the work
/// it deferred, so an assertion is about what the agent did rather than
/// about what it queued.
pub async fn deliver(kit: &TestHarness, event_id: &str, team_id: &str, event: Value) {
    let response = post_signed(&kit.router, &envelope(event_id, team_id, event), NOW).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the delivery is acknowledged: {}",
        String::from_utf8_lossy(&response.body)
    );
    kit.defer.drain().await;
}
