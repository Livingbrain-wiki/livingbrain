//! The write gate: the one entry point a turn uses to offer and to run a
//! tool over a member's connection.
//!
//! 1. **A write never runs without the asker's own connection or an
//!    approved borrow.** A tool the server does not claim to be read-only
//!    is a write; a write by the connection's owner runs, and a write by
//!    anyone else files the call whole and returns the owner an approval
//!    card — without a `tools/call` leaving the building. [`resolve`] runs
//!    the recorded call once, on the owner's decision, and only after
//!    re-checking the disabled lists.
//! 2. **A disabled tool is never offered to the model** ([`offered_for`]),
//!    and never runs either: the gate re-checks both disabled lists before
//!    anything is sent.
//!
//! Reads run through the connection's owner: `livingbrain-access` has no
//! read rule for connections — its scope table is about memory — so a read
//! borrows freely, which is what makes a workspace's shared servers useful.

use cratefield_core::{Clock, Database, DbError, HttpClient, IdGen};
use cratefield_kms::Kms;
use livingbrain_access::{Approval, ConnectionGrant, ConnectionUse, UserId, connection_for_write};
use livingbrain_channel::{ApprovalCard, Channel, Message, Outbound};
use serde_json::Value;
use time::format_description::well_known::Rfc3339;

use crate::client::{self, ClientError, ToolResult};
use crate::envelope;
use crate::store;
use crate::tools::{self, ToolDescriptor};

/// The ports one gated call needs. A bundle rather than eight parameters.
pub struct ToolPorts<'a> {
    pub db: &'a dyn Database,
    pub http: &'a dyn HttpClient,
    pub kms: &'a dyn Kms,
    pub clock: &'a dyn Clock,
    pub id_gen: &'a dyn IdGen,
}

/// One ask: who is asking, whose connection it runs on, and what to run.
pub struct ToolRequest<'a> {
    pub workspace: &'a str,
    pub asker: &'a UserId,
    pub owner: &'a UserId,
    pub provider: &'a str,
    pub tool: &'a str,
    pub arguments: &'a Value,
}

/// What a gated call ended as. `Refused`, `Failed` and `NotFound` are
/// outcomes, not errors: nothing was sent, and the reason is what the turn
/// tells the asker.
#[derive(Debug)]
pub enum ToolOutcome {
    /// The call ran; `is_error` on the result is the *tool's* own verdict.
    Ran(ToolResult),
    /// The ask is filed and the card is rendered, ready to post. No
    /// `tools/call` left the building.
    Pending { approval_id: String, card: Outbound },
    /// Policy refused the ask; nothing was sent.
    Refused(String),
    /// The call was allowed but did not complete (transport, server,
    /// storage). Nothing partial was sent.
    Failed(String),
    /// The approval id is unknown, already spent, or not this approver's
    /// in this workspace; nothing was sent and nothing was consumed.
    NotFound,
}

/// Why offering could not even be attempted.
#[derive(Debug)]
pub enum ToolError {
    Store(DbError),
    Kms(cratefield_kms::KmsError),
    Client(ClientError),
    /// The SSRF guard refused the connection's server URL.
    ServerUrl(String),
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "the connection row could not be read: {error}"),
            Self::Kms(error) => write!(f, "{error}"),
            Self::Client(error) => write!(f, "{error}"),
            Self::ServerUrl(why) => write!(f, "the server URL is refused: {why}"),
        }
    }
}

impl std::error::Error for ToolError {}

impl From<DbError> for ToolError {
    fn from(error: DbError) -> Self {
        Self::Store(error)
    }
}

impl From<cratefield_kms::KmsError> for ToolError {
    fn from(error: cratefield_kms::KmsError) -> Self {
        Self::Kms(error)
    }
}

impl From<ClientError> for ToolError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

/// Runs one ask through the gate. `channel` and `message` say where the
/// approval card goes if the ask needs one; nothing is posted from here —
/// the returned [`Outbound`] is what the turn's route sends.
pub async fn run_tool<C: Channel>(
    ports: &ToolPorts<'_>,
    request: &ToolRequest<'_>,
    channel: &C,
    message: &Message,
) -> ToolOutcome {
    // (i) A connection that is not there is a refusal before any request
    // is made.
    let connection = match store::get_connection(
        ports.db,
        request.workspace,
        &request.owner.to_string(),
        request.provider,
    )
    .await
    {
        Ok(Some(connection)) => connection,
        Ok(None) => {
            return ToolOutcome::Refused(format!(
                "no {} connection for {}",
                request.provider, request.owner
            ));
        }
        Err(error) => return ToolOutcome::Failed(error.to_string()),
    };
    // (ii) The disabled lists, before anything is sent — not even a
    // `tools/list` reaches a server about a tool that is switched off.
    if let Some(outcome) =
        disabled_outcome(ports, request.workspace, request.tool, &connection).await
    {
        return outcome;
    }
    // (iii) The server's own list decides that the tool exists and what it
    // does. What the asker named is never trusted past this point.
    let (url, token) = match opened(ports, request.workspace, request.owner, &connection).await {
        Opened::Ready(url, token) => (url, token),
        Opened::Stopped(outcome) => return outcome,
    };
    let mut mcp = client::Mcp::new(ports.http, &url, token.as_str());
    let server_tools = match mcp.list_tools().await {
        Ok(tools) => tools,
        Err(error) => return ToolOutcome::Failed(error.to_string()),
    };
    let Some(descriptor) = server_tools.iter().find(|t| t.name == request.tool) else {
        return ToolOutcome::Refused(format!(
            "{} does not offer {}",
            request.provider, request.tool
        ));
    };

    // (iv) A read runs through the owner's connection; a write runs only on
    // the asker's own, or on an approval the owner granted.
    match tools::classify(descriptor) {
        tools::Effect::Read => call(&mut mcp, request.tool, request.arguments).await,
        tools::Effect::Write => match connection_for_write(request.asker, request.owner) {
            ConnectionUse::Own => call(&mut mcp, request.tool, request.arguments).await,
            ConnectionUse::NeedsApproval { .. } => {
                file_for_approval(ports, request, channel, message).await
            }
        },
    }
}

/// Resolves an approval: the owner of the connection, in the workspace the
/// ask was filed in, and nobody else, can decide it. The recorded call is
/// what runs — the tool name and arguments are the ones that were filed,
/// not whatever the resolver was handed — and the disabled lists are
/// re-read at resolve time, so a tool switched off between the ask and the
/// decision still never runs.
pub async fn resolve(
    ports: &ToolPorts<'_>,
    workspace: &str,
    approver: &UserId,
    approval_id: &str,
    decision: Approval,
) -> ToolOutcome {
    // Take, not read: the delete matches the id, the workspace *and* the
    // owner, so a stranger's click — or a click about another workspace's
    // row — consumes nothing, and the owner can still decide later.
    let pending =
        match store::take_pending(ports.db, workspace, &approver.to_string(), approval_id).await {
            Ok(Some(pending)) => pending,
            Ok(None) => return ToolOutcome::NotFound,
            Err(error) => return ToolOutcome::Failed(error.to_string()),
        };
    // The ask was filed because the asker did not own the connection, so
    // the decision applies through `ConnectionUse::resolve` — the same
    // move an owner's own call skips by never filing one.
    let owner = UserId::new(pending.owner_member_id.clone());
    let borrowed = ConnectionUse::NeedsApproval {
        owner: owner.clone(),
    };
    if let ConnectionGrant::Refused { owner } = borrowed.resolve(decision) {
        return ToolOutcome::Refused(format!("{owner} denied the borrow; nothing was sent"));
    }
    let connection = match store::get_connection(
        ports.db,
        &pending.workspace_id,
        &pending.owner_member_id,
        &pending.provider,
    )
    .await
    {
        Ok(Some(connection)) => connection,
        Ok(None) => {
            return ToolOutcome::Refused(
                "the connection the ask was filed against is gone; nothing was sent".to_owned(),
            );
        }
        Err(error) => return ToolOutcome::Failed(error.to_string()),
    };
    let arguments = pending.arguments;
    run_recorded(
        ports,
        &pending.workspace_id,
        &connection,
        &owner,
        &pending.tool_name,
        &arguments,
    )
    .await
}

/// The tools a connection offers the model: the server's list minus both
/// disabled lists. This is what a turn puts into the model's tool list, so
/// a disabled tool is never even named to the model. A workspace with no
/// such connection offers nothing, which is not an error.
pub async fn offered_for(
    ports: &ToolPorts<'_>,
    workspace: &str,
    owner: &UserId,
    provider: &str,
) -> Result<Vec<ToolDescriptor>, ToolError> {
    let Some(connection) =
        store::get_connection(ports.db, workspace, &owner.to_string(), provider).await?
    else {
        return Ok(Vec::new());
    };
    let url = checked_url(ports.http, &connection.server_url).await?;
    let token = envelope::open(
        ports.kms,
        workspace,
        &owner.to_string(),
        provider,
        &connection.sealed,
    )
    .await?;
    let mut mcp = client::Mcp::new(ports.http, &url, token.as_str());
    let server_tools = mcp.list_tools().await?;
    let workspace_disabled = store::workspace_disabled_tools(ports.db, workspace).await?;
    Ok(tools::offered_tools(
        &server_tools,
        &connection.disabled_tools,
        &workspace_disabled,
    ))
}

/// Files a borrowed write: the call is recorded whole, and the card the
/// owner decides on is rendered for the asker's conversation. No
/// `tools/call` here — that is the whole point of the gate.
async fn file_for_approval<C: Channel>(
    ports: &ToolPorts<'_>,
    request: &ToolRequest<'_>,
    channel: &C,
    message: &Message,
) -> ToolOutcome {
    let approval_id = ports.id_gen.ulid();
    let pending = store::PendingCall {
        approval_id: approval_id.clone(),
        workspace_id: request.workspace.to_owned(),
        owner_member_id: request.owner.to_string(),
        asker_member_id: request.asker.to_string(),
        provider: request.provider.to_owned(),
        tool_name: request.tool.to_owned(),
        arguments: request.arguments.clone(),
        created_at: now(ports.clock),
    };
    if let Err(error) = store::put_pending(ports.db, &pending).await {
        return ToolOutcome::Failed(error.to_string());
    }
    let card = ApprovalCard {
        id: approval_id.clone(),
        prompt: format!(
            "{} asks to run {} on your {} connection — approve to let it run once.",
            request.asker, request.tool, request.provider
        ),
    };
    ToolOutcome::Pending {
        approval_id,
        card: channel.approval(message, &card),
    }
}

/// Runs one call over an open client.
async fn call(mcp: &mut client::Mcp<'_>, tool: &str, arguments: &Value) -> ToolOutcome {
    match mcp.call_tool(tool, arguments).await {
        Ok(result) => ToolOutcome::Ran(result),
        Err(error) => ToolOutcome::Failed(error.to_string()),
    }
}

/// Runs one recorded call on an already-loaded connection, behind the
/// disabled lists — the read/write path and the resolve path both come
/// through here.
async fn run_recorded(
    ports: &ToolPorts<'_>,
    workspace: &str,
    connection: &store::ConnectionRecord,
    owner: &UserId,
    tool: &str,
    arguments: &Value,
) -> ToolOutcome {
    if let Some(outcome) = disabled_outcome(ports, workspace, tool, connection).await {
        return outcome;
    }
    let (url, token) = match opened(ports, workspace, owner, connection).await {
        Opened::Ready(url, token) => (url, token),
        Opened::Stopped(outcome) => return outcome,
    };
    let mut mcp = client::Mcp::new(ports.http, &url, token.as_str());
    call(&mut mcp, tool, arguments).await
}

/// The disabled lists as a [`ToolOutcome::Refused`], checked at connection
/// and then workspace level; `None` when the tool may proceed. A read that
/// fails fails the call — it never reads as "nothing disabled".
async fn disabled_outcome(
    ports: &ToolPorts<'_>,
    workspace: &str,
    tool: &str,
    connection: &store::ConnectionRecord,
) -> Option<ToolOutcome> {
    if tools::is_disabled(&connection.disabled_tools, tool) {
        return Some(ToolOutcome::Refused(format!(
            "{tool} is disabled on this connection; nothing was sent"
        )));
    }
    match store::workspace_disabled_tools(ports.db, workspace).await {
        Ok(disabled) if tools::is_disabled(&disabled, tool) => Some(ToolOutcome::Refused(format!(
            "{tool} is disabled for this workspace; nothing was sent"
        ))),
        Ok(_) => None,
        Err(error) => Some(ToolOutcome::Failed(error.to_string())),
    }
}

/// What a call needs open before it can be made: the checked server URL
/// and the connection's token. The client that borrows them lives in the
/// caller's scope.
async fn opened(
    ports: &ToolPorts<'_>,
    workspace: &str,
    owner: &UserId,
    connection: &store::ConnectionRecord,
) -> Opened {
    let url = match checked_url(ports.http, &connection.server_url).await {
        Ok(url) => url,
        Err(error) => return Opened::Stopped(ToolOutcome::Failed(error.to_string())),
    };
    let token = match envelope::open(
        ports.kms,
        workspace,
        &owner.to_string(),
        &connection.provider,
        &connection.sealed,
    )
    .await
    {
        Ok(token) => token,
        Err(error) => return Opened::Stopped(ToolOutcome::Failed(error.to_string())),
    };
    Opened::Ready(url, token)
}

/// [`opened`]'s answer: the pair a call needs, or the outcome that stops
/// the call instead. (A `Result` would make the large
/// [`ToolOutcome::Pending`] card an `Err` payload, which clippy flags.)
enum Opened {
    Ready(String, envelope::Token),
    Stopped(ToolOutcome),
}

/// The SSRF guard on a connection's server URL: structure first, then the
/// addresses the name resolves to. The checked URL's text is what the
/// client connects to, not the string the row stores.
async fn checked_url(http: &dyn HttpClient, server_url: &str) -> Result<String, ToolError> {
    let url = livingbrain_models::ssrf::validate_url(server_url)
        .map_err(|error| ToolError::ServerUrl(error.to_string()))?;
    livingbrain_models::ssrf::check_destination(http, &url)
        .await
        .map_err(|error| ToolError::ServerUrl(error.to_string()))?;
    Ok(url.as_str().to_owned())
}

/// The row's timestamp form.
fn now(clock: &dyn Clock) -> String {
    clock.now().format(&Rfc3339).unwrap_or_default()
}
