//! The kit the `models` tests share: a harness with both modules mounted, a
//! signed-in admin, and a fake model endpoint the test scripts per case.
//
// Each test file declares `mod support;` and Cargo compiles each one as its
// own binary, so a helper only one of them uses is dead code in the others.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use cratefield_core::axum::http::{Method, Request, Response, StatusCode, header};
use cratefield_core::axum::{self, body::Body};
use cratefield_core::{HttpClient, HttpError, Kid, MapConfig, Payload, Signer, Statement};
use cratefield_testing::{FixedClock, TestHarness};
use livingbrain_models::Models;
use livingbrain_workspaces::Workspaces;
use serde_json::{Value, json};
use time::OffsetDateTime;

/// The instant every test runs at.
pub const NOW: i64 = 1_800_000_000;
/// The workspace every test signs into.
pub const WORKSPACE: &str = "T0SPACE";
/// The member: the workspace's owner, so an admin everywhere.
pub const OWNER: &str = "U0OWNER";
/// A member who is neither owner nor admin.
pub const PLAIN: &str = "U0PLAIN";
/// The key no test ever expects to see again.
pub const KEY: &str = "sk-live-SUPERSECRET1234";

/// A harness with `workspaces` (which owns the session) and `models`
/// mounted, talking to `http`.
pub fn harness(http: &FakeHttp) -> TestHarness {
    let config = Arc::new(MapConfig::from_pairs([(
        "MODEL_KEYS_SECRET",
        "a-test-encryption-secret-of-32-bytes",
    )]));
    let http = http.clone();
    let clock = FixedClock(OffsetDateTime::from_unix_timestamp(NOW).expect("a valid instant"));
    TestHarness::with_ports(
        vec![Box::new(Workspaces::new()), Box::new(Models::new())],
        move |ports| {
            ports.config = config;
            ports.http = Some(Arc::new(http));
            ports.clock = Some(Arc::new(clock));
        },
    )
}

/// A harness whose seed rows and session cookie already exist.
pub async fn signed_in(http: &FakeHttp) -> (TestHarness, String) {
    let kit = harness(http);
    seed_workspace(&kit, WORKSPACE, OWNER).await;
    seed_member(&kit, WORKSPACE, OWNER, true).await;
    seed_member(&kit, WORKSPACE, PLAIN, false).await;
    let cookie = session(&kit, WORKSPACE, OWNER);
    (kit, cookie)
}

// ---------------------------------------------------------------------------
// Tenancy

/// Writes the workspace and one member, the rows `caller` resolves a
/// session cookie to. Seeded rather than signed in through Slack: what
/// these tests hold the module to is what happens *after* a session
/// verifies, and the sign-in flow has its own tests.
pub async fn seed_workspace(kit: &TestHarness, workspace_id: &str, owner_id: &str) {
    kit.db
        .execute(&Statement::with_values(
            "INSERT INTO workspaces (id, name, owner_id, created_at) VALUES (?, ?, ?, ?)",
            vec![
                text(workspace_id),
                text("A workspace"),
                text(owner_id),
                text("2026-01-01T00:00:00Z"),
            ],
        ))
        .await
        .expect("the workspace row seeds");
}

/// One member of a seeded workspace, admin or not.
pub async fn seed_member(kit: &TestHarness, workspace_id: &str, user_id: &str, is_admin: bool) {
    let now = "2026-01-01T00:00:00Z".to_owned();
    kit.db
        .execute(&Statement::with_values(
            "INSERT INTO workspace_members \
                 (workspace_id, user_id, name, timezone, is_admin, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
            vec![
                text(workspace_id),
                text(user_id),
                text("A member"),
                sea_query::Value::String(None),
                sea_query::Value::BigInt(Some(i64::from(is_admin))),
                text(&now),
            ],
        ))
        .await
        .expect("the member row seeds");
}

/// A session cookie the way the workspaces module seals one. The
/// signature is good; the member row above is what the handler must find.
#[must_use]
pub fn session(kit: &TestHarness, workspace_id: &str, user_id: &str) -> String {
    kit.signer.sign(&Payload {
        purpose: livingbrain_workspaces::SESSION_PURPOSE.to_owned(),
        subject: json!({"team_id": workspace_id, "user_id": user_id, "exp": NOW + 3600})
            .to_string(),
        exp: None,
        kid: Kid::Cur,
    })
}

// ---------------------------------------------------------------------------
// A fake model endpoint and a fake resolver

/// A fake for the two upstreams `connect` talks to: the DoH resolver and
/// the model endpoint.
///
/// What it answers for a hostname is [`FakeHttp::resolving_to`]; whether
/// the model calls tools is [`FakeHttp::calling_tools`]. Both default to
/// the refusal-safe shape: a public address and a model that answers.
#[derive(Clone)]
pub struct FakeHttp {
    inner: Arc<Mutex<Script>>,
}

struct Script {
    address: String,
    /// A reply whose `A` answers carry this instead of an address, so a
    /// test can make the name resolve to nothing.
    cname_only: bool,
    tools: bool,
    context: Option<String>,
    requests: Vec<(String, String)>,
    /// What each request to the model host carried: `(uri, authorization,
    /// x-api-key, anthropic-version)`, so a test can see how the key went.
    sent: Vec<Sent>,
    /// The status `GET …/models` (the list) answers with.
    list_status: u16,
}

/// How one request to the model host was signed.
#[derive(Debug, Clone)]
pub struct Sent {
    pub uri: String,
    pub authorization: Option<String>,
    pub x_api_key: Option<String>,
    pub anthropic_version: Option<String>,
    pub body: String,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            address: "93.184.216.34".to_owned(),
            cname_only: false,
            tools: true,
            context: Some("200000".to_owned()),
            requests: Vec::new(),
            sent: Vec::new(),
            list_status: 200,
        }
    }
}

impl FakeHttp {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Script::default())),
        }
    }

    /// What every `A` answer resolves to.
    pub fn resolving_to(&self, address: &str) -> &Self {
        self.inner.lock().expect("lock").address = address.to_owned();
        self
    }

    /// The `A` answer carries a CNAME and no address, so the name
    /// resolves to nothing at all.
    pub fn resolving_to_nothing(&self) -> &Self {
        self.inner.lock().expect("lock").cname_only = true;
        self
    }

    /// Whether the model answers the tool probe with a tool call.
    pub fn calling_tools(&self, tools: bool) -> &Self {
        self.inner.lock().expect("lock").tools = tools;
        self
    }

    /// The context size the model endpoint reports, `None` for an
    /// endpoint that reports none.
    pub fn with_context(&self, context: Option<&str>) -> &Self {
        self.inner.lock().expect("lock").context = context.map(str::to_owned);
        self
    }

    /// The status the model list answers with.
    pub fn listing_with(&self, status: u16) -> &Self {
        self.inner.lock().expect("lock").list_status = status;
        self
    }

    /// Every request to the model host, with the headers that carry the key.
    #[must_use]
    pub fn sent(&self) -> Vec<Sent> {
        self.inner.lock().expect("lock").sent.clone()
    }

    /// Every request as `(method, uri)`.
    #[must_use]
    pub fn requests(&self) -> Vec<(String, String)> {
        self.inner.lock().expect("lock").requests.clone()
    }
}

#[async_trait]
impl HttpClient for FakeHttp {
    async fn send(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        let (parts, body) = request.into_parts();
        let uri = parts.uri.to_string();
        let method = parts.method.clone();
        let wants_tools = String::from_utf8_lossy(&body).contains("\"tools\"");
        let mut inner = self.inner.lock().expect("lock");
        inner.requests.push((method.to_string(), uri.clone()));
        let header = |name: &str| {
            parts
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        if !uri.starts_with("https://cloudflare-dns.com/") {
            inner.sent.push(Sent {
                uri: uri.clone(),
                authorization: header("authorization"),
                x_api_key: header("x-api-key"),
                anthropic_version: header("anthropic-version"),
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        let mut status = 200;

        let answer = if uri.starts_with("https://cloudflare-dns.com/dns-query") {
            // Only the A query is answered; an AAAA with no answers leaves
            // the resolver with the A record, as a real one would.
            if uri.contains("type=AAAA") {
                json!({"Status": 0}).to_string()
            } else if inner.cname_only {
                json!({
                    "Status": 0,
                    "Answer": [{"name": query(&uri, "name"), "type": 5,
                                "data": "elsewhere.invalid"}]
                })
                .to_string()
            } else {
                json!({
                    "Status": 0,
                    "Answer": [{"name": query(&uri, "name"), "type": 1, "data": inner.address}]
                })
                .to_string()
            }
        } else if uri.ends_with("/chat/completions") {
            chat_completion(inner.tools && wants_tools)
        } else if uri.ends_with("/messages") {
            messages_reply(inner.tools && wants_tools)
        } else if uri.ends_with("/models") {
            status = inner.list_status;
            json!({"data": [{"id": "model-b"}, {"id": "model-a"}, {"id": "model-a"}]}).to_string()
        } else if uri.contains("/models/") {
            match &inner.context {
                Some(size) => {
                    json!({"id": "m", "context_length": size.parse::<u32>().unwrap_or_default()})
                        .to_string()
                }
                None => json!({"id": "m"}).to_string(),
            }
        } else {
            return Err(HttpError::Transport(format!("unexpected request to {uri}")));
        };
        drop(inner);

        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Bytes::from(answer))
            .map_err(|err| HttpError::Transport(err.to_string()))
    }
}

/// A chat completion that either calls the tool or answers in JSON. The
/// JSON leg is always a JSON object, so a model that does not call tools
/// is "answers only" for tool calling and nothing else.
fn chat_completion(calls_tool: bool) -> String {
    let message = if calls_tool {
        json!({"content": null, "tool_calls": [{"id": "1", "type": "function",
            "function": {"name": "calculate", "arguments": "{}"}}]})
    } else {
        json!({"content": "{\"ok\":true}"})
    };
    json!({"choices": [{"message": message}]}).to_string()
}

/// An Anthropic Messages reply: a `tool_use` block, or a text block holding
/// a JSON object.
fn messages_reply(calls_tool: bool) -> String {
    let block = if calls_tool {
        json!({"type": "tool_use", "id": "t1", "name": "calculate", "input": {}})
    } else {
        json!({"type": "text", "text": "{\"ok\":true}"})
    };
    json!({"type": "message", "role": "assistant", "content": [block]}).to_string()
}

/// One query parameter out of a URL, or the empty string.
fn query(url: &str, name: &str) -> String {
    url.split_once('?')
        .and_then(|(_, query)| query.split('&').find(|pair| pair.starts_with(name)))
        .and_then(|pair| pair.split_once('='))
        .map_or_else(String::new, |(_, value)| value.to_owned())
}

// ---------------------------------------------------------------------------
// Requests

/// A buffered response, since the kit's own `TestResponse` body is private.
pub struct Res {
    pub status: StatusCode,
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

    /// The body as text, for the tests that read it for a leaked key.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub async fn get(kit: &TestHarness, path: &str, cookie: &str) -> Res {
    send(kit, Method::GET, path, cookie, None).await
}

pub async fn put(kit: &TestHarness, path: &str, cookie: &str, body: Value) -> Res {
    send(kit, Method::PUT, path, cookie, Some(body)).await
}

pub async fn delete(kit: &TestHarness, path: &str, cookie: &str) -> Res {
    send(kit, Method::DELETE, path, cookie, None).await
}

/// A request carrying the session cookie the way a browser would.
pub async fn send(
    kit: &TestHarness,
    method: Method,
    path: &str,
    cookie: &str,
    body: Option<Value>,
) -> Res {
    use tower::ServiceExt;
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::COOKIE, format!("__Host-lb_session={cookie}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(match body {
            Some(body) => Body::from(body.to_string()),
            None => Body::empty(),
        })
        .expect("request builds");
    let response = kit
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("router answers");
    let (parts, body) = response.into_parts();
    let body = axum::body::to_bytes(body, 1024 * 1024)
        .await
        .expect("body reads");
    Res {
        status: parts.status,
        body: body.to_vec(),
    }
}

/// The stored ciphertext of one role, as text. `None` when there is no
/// row.
pub async fn ciphertext(kit: &TestHarness, workspace_id: &str, role: &str) -> Option<String> {
    let rows = kit
        .db
        .query(&Statement::with_values(
            "SELECT key_ciphertext FROM model_connections WHERE workspace_id = ? AND role = ?",
            vec![text(workspace_id), text(role)],
        ))
        .await
        .expect("the row reads");
    let row = rows.first()?;
    let blob: Vec<u8> = row.get("key_ciphertext").unwrap_or_default();
    Some(String::from_utf8_lossy(&blob).into_owned())
}

fn text(value: &str) -> sea_query::Value {
    sea_query::Value::String(Some(Box::new(value.to_owned())))
}
