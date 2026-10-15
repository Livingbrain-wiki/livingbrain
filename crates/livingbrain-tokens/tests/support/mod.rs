//! The kit the `tokens` tests share: the three modules a deployment of this
//! feature mounts — `workspaces`, the device grant, `tokens`. The grant and
//! the tokens module get **the same** [`DevicePorts`] handle, which is the
//! point of the crate: this kit wires what the venture wires.

#![allow(unreachable_pub, dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use cratefield_core::axum::body::Body;
use cratefield_core::axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use cratefield_core::{Kid, Payload, Ports, RandomBytes, RandomError, Signer, Statement};
use cratefield_module_device_auth::{DeviceAuth, DeviceClient};
use cratefield_testing::TestHarness;
use livingbrain_tokens::{DevicePorts, Tokens};
use livingbrain_workspaces::Workspaces;
use serde_json::{Value, json};

pub const WORKSPACE: &str = "T0SPACE";
pub const OWNER: &str = "U0OWNER";
/// A second member, so "your own tokens only" has something to be exclusive of.
pub const OTHER: &str = "U0OTHER";
const CLIENT: &str = "livingbrain-cli";
/// A fixed instant for every seeded row, so nothing here depends on the clock.
const SEEDED_AT: &str = "2026-01-01T00:00:00Z";

/// The host the requests pretend to have reached. Deliberately **not** a
/// production name: the discovery documents answer from the request, so these
/// tests assert on this rather than on a hardcoded base.
pub const HOST: &str = "api.staging.livingbrain.wiki";
/// The origin the discovery documents must name, derived from [`HOST`].
pub const API_BASE: &str = "https://api.staging.livingbrain.wiki";

/// Every draw is a fresh counter sequence: two draws never collide, and every
/// run is identical. `cratefield-testing` has no `RandomBytes` fake.
#[derive(Clone, Default)]
struct SeqRandom(Arc<AtomicUsize>);

impl RandomBytes for SeqRandom {
    fn fill(&self, dest: &mut [u8]) -> Result<(), RandomError> {
        let base = self.0.fetch_add(1, Ordering::SeqCst);
        for (i, byte) in dest.iter_mut().enumerate() {
            *byte = u8::try_from((base.wrapping_mul(31) + i * 7 + 13) % 256).expect("mod 256");
        }
        Ok(())
    }
}

/// The harness, with the ports captured so a test can call `authenticate`
/// in-process rather than over the wire.
pub struct Kit {
    pub harness: TestHarness,
    pub ports: Ports,
}

/// The kit, with [`OWNER`] and [`OTHER`] seeded.
pub async fn signed_in() -> Kit {
    let kit = kit();
    let workspace = Statement::with_values(
        "INSERT INTO workspaces (id, name, owner_id, created_at) VALUES (?, ?, ?, ?)",
        vec![
            text(WORKSPACE),
            text("A workspace"),
            text(OWNER),
            text(SEEDED_AT),
        ],
    );
    kit.execute(&workspace, "the workspace row seeds").await;
    for (user, is_admin) in [(OWNER, true), (OTHER, false)] {
        let member = Statement::with_values(
            "INSERT INTO workspace_members (workspace_id, user_id, name, timezone, is_admin, \
             updated_at) VALUES (?, ?, ?, ?, ?, ?)",
            vec![
                text(WORKSPACE),
                text(user),
                text("A member"),
                sea_query::Value::String(None),
                sea_query::Value::BigInt(Some(i64::from(is_admin))),
                text(SEEDED_AT),
            ],
        );
        kit.execute(&member, "the member row seeds").await;
    }
    kit
}

fn kit() -> Kit {
    let device = DevicePorts::new();
    let grant = device.clone();
    let captured: Arc<Mutex<Option<Ports>>> = Arc::new(Mutex::new(None));
    let patch = captured.clone();

    let modules = vec![
        Box::new(Workspaces::new()) as Box<dyn cratefield_core::Module>,
        Box::new(
            DeviceAuth::builder()
                .client(DeviceClient::new(CLIENT))
                .random(SeqRandom::default())
                .approver(grant.approver())
                .issuer(grant.issuer())
                .build(),
        ),
        Box::new(Tokens::new().device_ports(device)),
    ];

    // The ports the router was built with, captured so `authenticate` in a
    // test resolves a caller exactly as the routes do.
    let harness = TestHarness::with_ports(modules, move |ports| {
        let mut copy = Ports::empty();
        copy.config = ports.config.clone();
        copy.db = ports.db.clone();
        copy.clock = ports.clock.clone();
        copy.signer = ports.signer.clone();
        *patch.lock().expect("lock") = Some(copy);
    });
    let ports = captured.lock().expect("lock").take().expect("ports");
    Kit { harness, ports }
}

impl Kit {
    async fn execute(&self, statement: &Statement, why: &str) {
        self.harness
            .db
            .execute(statement)
            .await
            .unwrap_or_else(|err| panic!("{why}: {err}"));
    }

    /// A session cookie the way the workspaces module seals one: the
    /// signature is good and the member row is what resolves it.
    pub fn session(&self, user_id: &str) -> String {
        self.harness.signer.sign(&Payload {
            purpose: livingbrain_workspaces::SESSION_PURPOSE.to_owned(),
            subject: json!({
                "team_id": WORKSPACE,
                "user_id": user_id,
                "exp": self.harness.clock.0.unix_timestamp() + 3600,
            })
            .to_string(),
            exp: None,
            kid: Kid::Cur,
        })
    }

    /// A member's browser on this origin: what a settings page acts as.
    pub fn browser(&self, user_id: &str) -> Caller {
        Caller::browser(&self.session(user_id))
    }

    /// The owner's browser, which is the caller most tests want.
    pub fn owner(&self) -> Caller {
        self.browser(OWNER)
    }

    pub fn other(&self) -> Caller {
        self.browser(OTHER)
    }

    /// The same browser on another site's page: the shape a
    /// cookie-authenticated write arrives in when somebody embeds a link.
    pub fn owner_elsewhere(&self) -> Caller {
        Caller::CrossSite(self.session(OWNER))
    }
}

/// Who is calling, and from where. The variants are the credential and the
/// browser signal together, because here they travel together: the two cookie
/// routes are exactly the requests a browser sends with `sec-fetch-site` and
/// `Origin` attached.
#[derive(Clone)]
pub enum Caller {
    /// A signed-in person's browser, writing from this origin.
    Browser(String),
    /// The same browser, on another site's page.
    CrossSite(String),
    /// A machine holding a personal access token.
    Token(String),
    /// Nobody at all.
    Anonymous,
}

impl Caller {
    pub fn browser(cookie: &str) -> Self {
        Self::Browser(cookie.to_owned())
    }

    pub fn headers(&self) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("host", value(HOST));
        let (cookie, site) = match self {
            Self::Browser(sealed) => {
                h.insert(header::ORIGIN, value(&format!("https://{HOST}")));
                (sealed, "same-origin")
            }
            Self::CrossSite(sealed) => (sealed, "cross-site"),
            Self::Token(token) => {
                h.insert(header::AUTHORIZATION, value(&format!("Bearer {token}")));
                return h;
            }
            Self::Anonymous => return h,
        };
        h.insert(
            header::COOKIE,
            value(&format!("__Host-lb_session={cookie}")),
        );
        h.insert("sec-fetch-site", value(site));
        h
    }
}

pub struct Res {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Res {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|err| {
            panic!("not JSON ({err}): {}", String::from_utf8_lossy(&self.body))
        })
    }

    /// The body as text, for the tests reading it looking for a token.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn location(&self) -> Option<String> {
        self.headers
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }
}

pub async fn get(kit: &Kit, path: &str, who: &Caller) -> Res {
    send(kit, Method::GET, path, None, &who.headers()).await
}

/// The same `GET`, addressed to another hostname and optionally over another
/// scheme — how these tests reach a second deployment's documents out of one
/// Worker.
pub async fn get_at(kit: &Kit, path: &str, host: &str, proto: Option<&str>) -> Res {
    let mut headers = Caller::Anonymous.headers();
    headers.insert("host", value(host));
    if let Some(proto) = proto {
        headers.insert("x-forwarded-proto", value(proto));
    }
    send(kit, Method::GET, path, None, &headers).await
}

pub async fn post_json(kit: &Kit, path: &str, body: &Value, who: &Caller) -> Res {
    let body = ("application/json", body.to_string());
    send(kit, Method::POST, path, Some(body), &who.headers()).await
}

pub async fn delete(kit: &Kit, path: &str, who: &Caller) -> Res {
    send(kit, Method::DELETE, path, None, &who.headers()).await
}

async fn send(
    kit: &Kit,
    method: Method,
    path: &str,
    form: Option<(&str, String)>,
    headers: &HeaderMap,
) -> Res {
    use tower::ServiceExt;
    let mut builder = Request::builder().method(method).uri(path);
    for (name, header) in headers {
        builder = builder.header(name, header.clone());
    }
    let body = match form {
        Some((content_type, body)) => {
            builder = builder.header(header::CONTENT_TYPE, content_type);
            Body::from(body)
        }
        None => Body::empty(),
    };
    let response = kit
        .harness
        .router
        .clone()
        .oneshot(builder.body(body).expect("request builds"))
        .await
        .expect("router answers");
    let (parts, body) = response.into_parts();
    let bytes = cratefield_core::axum::body::to_bytes(body, 1024 * 1024)
        .await
        .expect("body reads");
    Res {
        status: parts.status,
        headers: parts.headers,
        body: bytes.to_vec(),
    }
}

/// A device authorization request's two codes.
pub struct Codes {
    pub device_code: String,
    pub user_code: String,
}

pub async fn device_code(kit: &Kit) -> Codes {
    let response = post_json(
        kit,
        "/v1/device-auth/code",
        &json!({"client_id": CLIENT, "name": "Alice's laptop"}),
        &Caller::Anonymous,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.text());
    let body = response.json();
    let field = |name: &str| {
        body[name]
            .as_str()
            .unwrap_or_else(|| panic!("{name}"))
            .to_owned()
    };
    Codes {
        device_code: field("device_code"),
        user_code: field("user_code"),
    }
}

/// The approval form a person's browser submits.
pub async fn approve(kit: &Kit, codes: &Codes, who: &Caller) -> Res {
    let form = (
        "application/x-www-form-urlencoded",
        format!("user_code={}", codes.user_code),
    );
    send(
        kit,
        Method::POST,
        "/v1/device-auth/approve",
        Some(form),
        &who.headers(),
    )
    .await
}

/// One poll of the device access-token request: what the CLI repeats until it
/// is answered.
pub async fn poll(kit: &Kit, device_code: &str) -> Res {
    let grant = json!({
        "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
        "device_code": device_code,
        "client_id": CLIENT,
    });
    post_json(kit, "/v1/device-auth/token", &grant, &Caller::Anonymous).await
}

/// The whole flow in one call: a code, an approval from `who`, and the poll
/// that answers it — which is where the credential is.
pub async fn device_flow(kit: &Kit, who: &Caller) -> Res {
    let codes = device_code(kit).await;
    approve(kit, &codes, who).await;
    poll(kit, &codes.device_code).await
}

/// Every column of the `personal_access_tokens` table as text, so a test can
/// assert nothing secret is in it.
pub async fn rows(kit: &Kit) -> Vec<HashMap<String, String>> {
    const COLUMNS: [&str; 8] = [
        "prefix",
        "secret_hash",
        "workspace_id",
        "user_id",
        "name",
        "scopes",
        "created_at",
        "revoked_at",
    ];
    let select = format!("SELECT {} FROM personal_access_tokens", COLUMNS.join(", "));
    let result = kit
        .harness
        .db
        .query(&Statement::with_values(&select, Vec::new()))
        .await
        .expect("the rows read");
    result
        .rows
        .iter()
        .map(|row| {
            COLUMNS
                .into_iter()
                .map(|c| {
                    let stored: Option<String> = row.get::<Option<String>>(c).flatten();
                    (c.to_owned(), stored.unwrap_or_default())
                })
                .collect()
        })
        .collect()
}

/// Removes the member row a token was minted for, so the token stops working.
pub async fn drop_member(kit: &Kit, user_id: &str) {
    let statement = Statement::with_values(
        "DELETE FROM workspace_members WHERE workspace_id = ? AND user_id = ?",
        vec![text(WORKSPACE), text(user_id)],
    );
    kit.harness
        .db
        .execute(&statement)
        .await
        .expect("the row goes");
}

/// A token's two halves: the prefix in the clear, the secret that must never
/// be stored or logged.
pub fn halves(token: &str) -> (&str, &str) {
    token.rsplit_once('_').unwrap_or((token, ""))
}

pub fn prefix_of(token: &str) -> &str {
    halves(token).0
}

pub fn secret_of(token: &str) -> &str {
    halves(token).1
}

fn value(text: &str) -> HeaderValue {
    HeaderValue::from_str(text).expect("a header value")
}

/// A bound text value for a statement, the shape the rows are seeded with.
pub fn text(value: &str) -> sea_query::Value {
    sea_query::Value::String(Some(Box::new(value.to_owned())))
}
