//! The MCP client: JSON-RPC 2.0 over Streamable HTTP, through the framework
//! `HttpClient` port. One [`Mcp`] handle runs one connection's calls: the
//! first request is the `initialize` handshake, the `Mcp-Session-Id` the
//! server answers with travels on every later request, and a reply is read
//! whether it came back as JSON or as an event stream.

use std::time::Duration;

use bytes::Bytes;
use cratefield_core::axum::http::{self, header};
use cratefield_core::{HttpClient, HttpPolicy};
use serde_json::{Value, json};

use crate::tools::{ToolDescriptor, descriptor_from_value};

/// The response ceiling for an MCP reply.
const MAX_RESPONSE: usize = 1024 * 1024;
/// The timeout for one MCP request.
const TIMEOUT: Duration = Duration::from_secs(15);
/// The protocol version this client announces.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Why an MCP call did not happen. `Unreachable` is the transport, the
/// status or the SSRF guard; `Protocol` is a server that answered with
/// something that is not the JSON-RPC message that was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    Unreachable(String),
    Protocol(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(why) => write!(f, "the MCP server was unreachable: {why}"),
            Self::Protocol(why) => write!(f, "the MCP server answered off protocol: {why}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// What a `tools/call` came back with: the tool's own content and whether
/// the *tool* reported an error — a tool error is a result in MCP, not a
/// transport one.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub content: Value,
    pub is_error: bool,
}

/// One connection's client: the endpoint it posts to, the session the
/// server handed back, and the JSON-RPC id counter.
pub struct Mcp<'a> {
    http: &'a dyn HttpClient,
    server_url: &'a str,
    token: &'a str,
    session_id: Option<String>,
    next_id: i64,
    initialized: bool,
}

impl<'a> Mcp<'a> {
    /// A client for `server_url` signed with `token`. The URL must already
    /// be the SSRF-checked one; the client connects where it is pointed.
    #[must_use]
    pub fn new(http: &'a dyn HttpClient, server_url: &'a str, token: &'a str) -> Self {
        Self {
            http,
            server_url,
            token,
            session_id: None,
            next_id: 1,
            initialized: false,
        }
    }

    /// The server's tool list, handshaking first if it has not happened yet.
    pub async fn list_tools(&mut self) -> Result<Vec<ToolDescriptor>, ClientError> {
        let result = self.rpc("tools/list", json!({})).await?;
        let Some(entries) = result.get("tools").and_then(Value::as_array) else {
            return Ok(Vec::new());
        };
        Ok(entries.iter().filter_map(descriptor_from_value).collect())
    }

    /// Calls one tool. A JSON-RPC error is a refusal at the server, not a
    /// tool-level `isError` result.
    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: &Value,
    ) -> Result<ToolResult, ClientError> {
        let result = self
            .rpc("tools/call", json!({"name": name, "arguments": arguments}))
            .await?;
        Ok(ToolResult {
            content: result.get("content").cloned().unwrap_or(Value::Null),
            is_error: result
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    /// One JSON-RPC request, handshaking first. The `id` is this handle's
    /// own counter; the SSE branch matches on it because a stream can carry
    /// messages for other requests besides.
    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        if !self.initialized {
            self.request(
                "initialize",
                Some(json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "livingbrain",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                })),
            )
            .await?;
            self.initialized = true;
            // Spec courtesy: the server is told the handshake completed.
            // Best effort — a server that drops the notification has not
            // failed the call that follows.
            let _ = self.notify("notifications/initialized").await;
        }
        self.request(method, Some(params)).await
    }

    /// Sends one request and returns its `result`. The session id, when the
    /// server names one on `initialize`, rides along from then on.
    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value, ClientError> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": method,
            "params": params.unwrap_or(json!({})),
        });
        let id = body["id"].as_i64().unwrap_or_default();
        self.next_id += 1;
        let response = self.send(method, body, id).await?;
        if let Some(error) = response.get("error") {
            return Err(ClientError::Protocol(format!("{method}: {error}")));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| ClientError::Protocol(format!("{method} answered without a result")))
    }

    /// A notification: no id, and no answer is read. A failure here is
    /// absorbed by the caller.
    async fn notify(&mut self, method: &str) -> Result<(), ClientError> {
        let body = json!({"jsonrpc": "2.0", "method": method});
        self.send(method, body, i64::MIN).await.map(|_| ())
    }

    /// The POST itself: bearer-signed, JSON and event-stream both accepted,
    /// the session header carried once the server has named one.
    async fn send(&mut self, method: &str, body: Value, id: i64) -> Result<Value, ClientError> {
        let mut builder = http::Request::builder()
            .method(http::Method::POST)
            .uri(self.server_url)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::AUTHORIZATION, format!("Bearer {}", self.token))
            .extension(HttpPolicy {
                max_response_bytes: MAX_RESPONSE,
                timeout: TIMEOUT,
            });
        if let Some(session) = &self.session_id {
            builder = builder.header("Mcp-Session-Id", session.clone());
        }
        let request = builder
            .body(Bytes::from(body.to_string()))
            .map_err(|_| ClientError::Unreachable("the request did not build".to_owned()))?;
        let response = self
            .http
            .send(request)
            .await
            .map_err(|error| ClientError::Unreachable(error.to_string()))?;
        if !response.status().is_success() {
            // The status is named; the body is never echoed, because a
            // server can quote the Authorization header back in it.
            return Err(ClientError::Unreachable(format!(
                "{method} answered {}",
                response.status().as_u16()
            )));
        }
        if self.session_id.is_none() {
            self.session_id = response
                .headers()
                .get("Mcp-Session-Id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
        }
        parse_message(response.headers(), response.body(), id)
    }
}

/// Reads one JSON-RPC message out of a reply: the body when it is JSON, or
/// the `data:` line whose id matches when the server streamed the answer.
fn parse_message(headers: &http::HeaderMap, body: &[u8], id: i64) -> Result<Value, ClientError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let text = std::str::from_utf8(body)
        .map_err(|_| ClientError::Protocol("the reply is not UTF-8".to_owned()))?;
    if content_type.contains("text/event-stream") {
        return sse_message(text, id).ok_or_else(|| {
            ClientError::Protocol("the event stream carried no message for the request".to_owned())
        });
    }
    serde_json::from_str(text)
        .map_err(|error| ClientError::Protocol(format!("the reply is not JSON: {error}")))
}

/// The message a `text/event-stream` body carried for `id`, and nothing
/// else: a notification or another request's answer on the same stream is
/// never accepted in its place.
fn sse_message(body: &str, id: i64) -> Option<Value> {
    data_events(body)
        .iter()
        .filter_map(|event| serde_json::from_str::<Value>(event).ok())
        .find(|message| message.get("id").and_then(Value::as_i64) == Some(id))
}

/// The `data:` payloads of an SSE body, one per event: consecutive `data:`
/// lines join, a blank line ends the event.
fn data_events(body: &str) -> Vec<String> {
    let mut events = Vec::new();
    let mut data: Vec<&str> = Vec::new();
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.strip_prefix(' ').unwrap_or(rest));
        } else if line.trim().is_empty() && !data.is_empty() {
            events.push(data.join("\n"));
            data.clear();
        }
    }
    if !data.is_empty() {
        events.push(data.join("\n"));
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_sse_body_yields_the_message_with_the_matching_id() {
        let body = concat!(
            ": ping\n",
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"x\"}\n",
            "\n",
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ok\":true}}\n",
            "\n"
        );
        let message = sse_message(body, 2).expect("the id is in there");
        assert_eq!(message["result"]["ok"], true);
    }

    #[test]
    fn an_event_without_the_requests_id_is_ignored() {
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":9,\"result\":{}}\n",
            "\n"
        );
        assert!(sse_message(body, 7).is_none(), "only the matching id");
    }
}
