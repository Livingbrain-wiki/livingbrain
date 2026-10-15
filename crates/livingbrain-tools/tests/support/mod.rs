//! The kit the gate tests share: one harness with the module mounted, a
//! fake MCP server that records everything it is sent, and the sealed
//! connection and Slack shapes the gate decides about.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use cratefield_core::axum::http::{Request, Response, header};
use cratefield_core::{HttpClient, HttpError, Statement, UlidIdGen};
use cratefield_kms::{Kms, WorkerSecretKms};
use cratefield_testing::TestHarness;
use livingbrain_access::UserId;
use livingbrain_channel::{Identity, Location, Message, Platform, Visibility, slack::Slack};
use livingbrain_tools::{
    ConnectionRecord, ToolPorts, Tools, put_connection, seal, set_workspace_disabled_tools,
};
use serde_json::{Value, json};

/// The workspace every test runs in.
pub const WORKSPACE: &str = "T0SPACE";
/// The member who connected the server: the owner of the credential.
pub const OWNER: &str = "U0OWNER";
/// A teammate who is not the owner.
pub const OTHER: &str = "U0OTHER";
/// The provider every test connects.
pub const PROVIDER: &str = "github";
/// The token no test ever expects to see again.
pub const TOKEN: &str = concat!("gh", "p_ABCDEFGHIJKLMNOPQRSTUVWXYZ123456");
/// The MCP endpoint, public so the SSRF guard approves the name.
const SERVER_URL: &str = "https://mcp.example.com/rpc";

/// One test's environment: the harness, the fake server and the key
/// custodian it runs against, with the gate's ports and the store helpers
/// reached through it.
pub struct Setup {
    pub kit: TestHarness,
    pub http: FakeMcp,
    custodian: Arc<dyn Kms>,
}

/// A harness with the module mounted (its migrations are applied on
/// build), a fresh fake server, and a custodian over an in-process ring.
#[must_use]
pub fn setup() -> Setup {
    let key = STANDARD.encode([7_u8; 32]);
    let custodian: Arc<dyn Kms> = Arc::new(
        WorkerSecretKms::from_lookup(|name| match name {
            "HARNESS_KEK_CURRENT" => Some("1".to_owned()),
            "HARNESS_KEK_V1" => Some(key.clone()),
            _ => None,
        })
        .expect("the key ring is well formed"),
    );
    Setup {
        kit: TestHarness::with_ports(vec![Box::new(Tools::new())], |_| {}),
        http: FakeMcp::new(),
        custodian,
    }
}

impl Setup {
    /// The gate's port bundle. The `HttpClient` is the fake server, so
    /// every wire claim in a test is a fact about what *it* recorded.
    #[must_use]
    pub fn gate(&self) -> ToolPorts<'_> {
        ToolPorts {
            db: self.kit.db.as_ref(),
            http: &self.http,
            kms: self.custodian.as_ref(),
            clock: &self.kit.clock,
            id_gen: &ULIDS,
        }
    }

    /// Connects `who` to [`PROVIDER`] with the token sealed for real, and
    /// `disabled` switched off on the connection.
    pub async fn connect(&self, who: &str, disabled: &[&str]) {
        let sealed = seal(self.custodian.as_ref(), WORKSPACE, who, PROVIDER, TOKEN)
            .await
            .expect("the token seals");
        put_connection(
            self.kit.db.as_ref(),
            &ConnectionRecord {
                workspace_id: WORKSPACE.to_owned(),
                member_id: who.to_owned(),
                provider: PROVIDER.to_owned(),
                server_url: SERVER_URL.to_owned(),
                sealed,
                disabled_tools: disabled.iter().map(|name| (*name).to_owned()).collect(),
                created_at: "2026-01-01T00:00:00Z".to_owned(),
            },
        )
        .await
        .expect("the connection stores");
    }

    /// Replaces the workspace's disabled tool list.
    pub async fn set_workspace_disabled(&self, names: &[&str]) {
        set_workspace_disabled_tools(
            self.kit.db.as_ref(),
            WORKSPACE,
            &names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>(),
        )
        .await
        .expect("the settings store");
    }

    /// The pending row for `approval_id`, or `None` once it is spent.
    /// Reads only — [`livingbrain_tools::resolve`] is what takes rows out.
    pub async fn pending_row(&self, approval_id: &str) -> Option<Value> {
        let rows = self
            .kit
            .db
            .query(&Statement::with_values(
                "SELECT asker_member_id, tool_name, arguments FROM tool_pending_approvals \
                 WHERE approval_id = ?",
                vec![sea_query::Value::String(Some(Box::new(
                    approval_id.to_owned(),
                )))],
            ))
            .await
            .expect("the pending row reads");
        rows.first().map(|row| {
            json!({
                "asker": row.get::<String>("asker_member_id").unwrap_or_default(),
                "tool": row.get::<String>("tool_name").unwrap_or_default(),
                "arguments": row.get::<Option<String>>("arguments").flatten().unwrap_or_default(),
            })
        })
    }
}

/// The id generator the gate mints approval ids from.
static ULIDS: UlidIdGen = UlidIdGen;

/// `OWNER` or `OTHER`, as the access model spells a member.
#[must_use]
pub fn member(id: &str) -> UserId {
    UserId::new(id.to_owned())
}

/// A mention from `OTHER` in a private channel — where an approval card
/// would land.
#[must_use]
pub fn message() -> Message {
    Message {
        author: Identity {
            platform: Platform::Slack,
            team: WORKSPACE.to_owned(),
            user: OTHER.to_owned(),
        },
        location: Location {
            platform: Platform::Slack,
            channel: "C0TOOLS".to_owned(),
            visibility: Visibility::Private,
        },
        thread: None,
        id: "1.1".to_owned(),
        text: "run it".to_owned(),
        mentions_bot: true,
    }
}

/// The Slack adapter the cards render through.
#[must_use]
pub fn slack() -> Slack {
    Slack {
        bot_user_id: "U0BOT".to_owned(),
    }
}

/// A fake remote MCP server: answers the handshake, lists whatever tools
/// the test scripted, and answers `tools/call` — recording everything.
#[derive(Clone)]
pub struct FakeMcp {
    inner: Arc<Mutex<Script>>,
}

struct Script {
    /// What every DoH `A` answer resolves to.
    address: String,
    /// The `tools/list` answer.
    tools: Value,
    /// The `tools/call` result.
    call_result: Value,
    /// The `Mcp-Session-Id` the handshake answers with, when any.
    session: Option<String>,
    /// Reply in `text/event-stream` bodies rather than JSON.
    stream: bool,
    /// Every request: the JSON-RPC method (or notification name), the
    /// `Authorization` header, and the body.
    sent: Vec<Sent>,
}

/// How one request arrived.
#[derive(Debug, Clone)]
pub struct Sent {
    pub method: String,
    pub authorization: Option<String>,
    pub body: String,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            address: "93.184.216.34".to_owned(),
            tools: json!([
                {"name": "search", "description": "Search", "inputSchema": {"type": "object"},
                 "annotations": {"readOnlyHint": true}},
                {"name": "create_issue", "description": "Create an issue",
                 "inputSchema": {"type": "object"}},
            ]),
            call_result: json!({"content": [{"type": "text", "text": "done"}], "isError": false}),
            session: Some("S0SESSION".to_owned()),
            stream: false,
            sent: Vec::new(),
        }
    }
}

impl FakeMcp {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Script::default())),
        }
    }

    /// The tools the server lists.
    pub fn listing(&self, tools: Value) -> &Self {
        self.inner.lock().expect("lock").tools = tools;
        self
    }

    /// Reply in event-stream bodies.
    pub fn streaming(&self) -> &Self {
        self.inner.lock().expect("lock").stream = true;
        self
    }

    /// Every request the server received, in order.
    #[must_use]
    pub fn sent(&self) -> Vec<Sent> {
        self.inner.lock().expect("lock").sent.clone()
    }

    /// Every recorded `tools/call`.
    #[must_use]
    pub fn tool_calls(&self) -> Vec<Sent> {
        self.sent()
            .into_iter()
            .filter(|sent| sent.method == "tools/call")
            .collect()
    }

    fn answer(&self, request: Request<Bytes>) -> Response<Bytes> {
        let (parts, body) = request.into_parts();
        let text = String::from_utf8_lossy(&body).to_string();
        let message: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let method = message["method"].as_str().unwrap_or_default().to_owned();
        let id = message["id"].as_i64().unwrap_or_default();
        let mut inner = self.inner.lock().expect("lock");
        inner.sent.push(Sent {
            method,
            authorization: parts
                .headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
            body: text,
        });
        let result = match inner.sent.last().expect("just pushed").method.as_str() {
            "initialize" => json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "serverInfo": {"name": "fake", "version": "0"},
            }),
            "tools/list" => json!({"tools": inner.tools}),
            "tools/call" => inner.call_result.clone(),
            // The `notifications/initialized` courtesy gets an empty 202.
            _ => return Response::builder().status(202).body(Bytes::new()).unwrap(),
        };
        let payload = json!({"jsonrpc": "2.0", "id": id, "result": result});
        let (content_type, payload) = if inner.stream {
            (
                "text/event-stream",
                format!("event: message\ndata: {payload}\n\n"),
            )
        } else {
            ("application/json", payload.to_string())
        };
        let mut builder = Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, content_type);
        if let Some(session) = &inner.session {
            builder = builder.header("Mcp-Session-Id", session.clone());
        }
        builder.body(Bytes::from(payload)).unwrap()
    }
}

#[async_trait]
impl HttpClient for FakeMcp {
    async fn send(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        // The DoH resolver the SSRF guard asks before anything is sent.
        if request
            .uri()
            .host()
            .is_some_and(|host| host.contains("cloudflare-dns.com"))
        {
            let address = self.inner.lock().expect("lock").address.clone();
            let answer =
                json!({"Answer": [{"name": "mcp.example.com", "type": 1, "data": address}]});
            return Ok(Response::builder()
                .status(200)
                .header(header::CONTENT_TYPE, "application/dns-json")
                .body(Bytes::from(answer.to_string()))
                .unwrap());
        }
        Ok(self.answer(request))
    }
}
