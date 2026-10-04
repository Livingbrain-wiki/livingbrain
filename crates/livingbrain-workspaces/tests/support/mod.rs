//! The kit the workspaces tests share: a harness wired to a fake Slack, a
//! request helper that can carry a `Cookie` header, and the small pieces
//! of a Slack `id_token` a test needs to craft one.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use cratefield_core::axum::http::{HeaderMap, Method, Request, Response, StatusCode, header};
use cratefield_core::axum::{self, body::Body};
use cratefield_core::{Config, HttpClient, HttpError, Kid, MapConfig, Payload, Signer};
use cratefield_testing::{FixedClock, TestHarness};
use livingbrain_workspaces::{
    FLOW_COOKIE, FLOW_PURPOSE, SESSION_COOKIE, UserChange, Workspaces, apply_user_change,
};
use serde_json::{Value, json};
use time::OffsetDateTime;

/// The Slack app the tests configure.
pub const CLIENT_ID: &str = "1234.5678";
pub const CLIENT_SECRET: &str = "slack-client-secret-not-real";
pub const REDIRECT_BASE: &str = "https://brain.example";
/// The fixed instant every test runs at, matching the kit's default clock.
pub const NOW: i64 = 1_800_000_000;

/// The callback URL the module must build from `REDIRECT_BASE`, as it
/// appears percent-encoded in the authorize redirect and the token form.
pub fn callback_url() -> String {
    format!("{REDIRECT_BASE}/v1/workspaces/slack/callback")
        .replace(':', "%3A")
        .replace('/', "%2F")
}

/// The harness config a deployment would set.
pub fn config() -> MapConfig {
    MapConfig::from_pairs([
        ("WORKSPACES_SLACK_CLIENT_ID", CLIENT_ID),
        ("WORKSPACES_SLACK_CLIENT_SECRET", CLIENT_SECRET),
        ("WORKSPACES_REDIRECT_BASE", REDIRECT_BASE),
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
    requests: Vec<(String, String, String)>,
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

    /// Every request as `(method, uri, body)`.
    #[must_use]
    pub fn requests(&self) -> Vec<(String, String, String)> {
        self.inner.lock().expect("lock").requests.clone()
    }
}

#[async_trait]
impl HttpClient for TokenHttp {
    async fn send(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        let (parts, body) = request.into_parts();
        let mut inner = self.inner.lock().expect("lock");
        inner.requests.push((
            parts.method.to_string(),
            parts.uri.to_string(),
            String::from_utf8_lossy(&body).into_owned(),
        ));
        let answer = match (&inner.body, &inner.id_token) {
            (Some(raw), _) => raw.clone(),
            (None, Some(token)) => json!({
                "ok": true,
                "access_token": "xoxb-not-used",
                "id_token": token,
            })
            .to_string(),
            (None, None) => json!({"ok": false, "error": "invalid_code"}).to_string(),
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

/// A request carrying a `Cookie` header. The kit's `request` helper cannot
/// send one, and every route this module reads needs one.
pub async fn send(
    router: &axum::Router,
    method: Method,
    path: &str,
    cookies: &[(&str, &str)],
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
    let request = builder.body(Body::empty()).expect("request builds");
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

/// Signs `user_id` of `team_id` in, end to end, and returns the session
/// cookie. Panics if any step of the sign-in does not succeed.
pub async fn sign_in(
    kit: &TestHarness,
    http: &TokenHttp,
    team_id: &str,
    user_id: &str,
    name: &str,
) -> String {
    let flow = start_flow(kit).await;
    http.will_return(&id_token(&claims(&flow.nonce, team_id, user_id, name)));
    let callback = get(&kit.router, &flow.callback("code-1"), &flow.cookies()).await;
    assert_eq!(
        callback.status,
        StatusCode::FOUND,
        "callback redirects: {}",
        String::from_utf8_lossy(&callback.body)
    );
    callback
        .cookie(SESSION_COOKIE)
        .expect("callback sets a session cookie")
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
pub fn signed_session(kit: &TestHarness, team_id: &str, user_id: &str, exp: i64) -> String {
    kit.signer.sign(&Payload {
        purpose: livingbrain_workspaces::SESSION_PURPOSE.to_owned(),
        subject: json!({"team_id": team_id, "user_id": user_id, "exp": exp}).to_string(),
        exp: None,
        kid: Kid::Cur,
    })
}

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

/// Applies a Slack event the way issue #6's webhook will: after verifying
/// the signature, with the `team_id` read out of the signed envelope.
pub async fn apply_event(kit: &TestHarness, team_id: &str, event: &Value) -> UserChange {
    apply_user_change(&*kit.db, &clock(), team_id, event)
        .await
        .expect("the event applies")
}
