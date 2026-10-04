//! A minimal stdio MCP server: newline-delimited JSON-RPC 2.0 on stdin and
//! stdout, forwarding each tool call to the same API client the commands use.
//! Its output is already JSON, so `--json` is a no-op here.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

use crate::api::Client;
use crate::{CliResult, err};

/// Serve until stdin closes.
///
/// # Errors
///
/// When stdin cannot be read or stdout cannot be written.
pub fn serve(client: &Client) -> CliResult<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| err(format!("could not read stdin: {e}")))?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle(client, &line) {
            writeln!(stdout, "{response}")
                .map_err(|e| err(format!("could not write stdout: {e}")))?;
            stdout
                .flush()
                .map_err(|e| err(format!("could not flush stdout: {e}")))?;
        }
    }
    Ok(())
}

/// Handle one JSON-RPC line; `None` means "a notification, answer nothing".
fn handle(client: &Client, line: &str) -> Option<Value> {
    let request: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            return Some(error_response(
                Value::Null,
                -32700,
                &format!("parse error: {error}"),
            ));
        }
    };
    // A message with no id is a notification (JSON-RPC 2.0 §4.1): never a reply,
    // whatever its method.
    let id = request.get("id").cloned()?;
    match request.get("method").and_then(Value::as_str).unwrap_or("") {
        "initialize" => Some(ok(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "livingbrain", "version": env!("CARGO_PKG_VERSION") },
            }),
        )),
        // A message that carries an id must be answered even though clients send
        // this one as a notification.
        "notifications/initialized" => Some(ok(id, json!({}))),
        "tools/list" => Some(ok(id, json!({ "tools": tools() }))),
        "tools/call" => Some(call(client, id, request.get("params"))),
        other => Some(error_response(
            id,
            -32601,
            &format!("method not found: {other}"),
        )),
    }
}

/// The three tools, each a thin wrapper over one API call.
fn tools() -> Value {
    fn tool(name: &str, description: &str, properties: Value, required: &str) -> Value {
        json!({
            "name": name,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "required": [required],
            },
        })
    }
    json!([
        tool(
            "brain_search",
            "Search the team brain and get matching pages.",
            json!({ "query": { "type": "string" }, "project": { "type": "string" }, "limit": { "type": "integer" } }),
            "query",
        ),
        tool(
            "brain_page",
            "Read a wiki page's Markdown by slug.",
            json!({ "slug": { "type": "string" } }),
            "slug",
        ),
        tool(
            "brain_note",
            "Add a note to the brain.",
            json!({ "body": { "type": "string" }, "project": { "type": "string" } }),
            "body",
        ),
    ])
}

/// `tools/call` for one of the three tools.
fn call(client: &Client, id: Value, params: Option<&Value>) -> Value {
    let Some(params) = params else {
        return error_response(id, -32602, "tools/call needs params");
    };
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return error_response(id, -32602, "tools/call needs a tool name");
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let project = arguments.get("project").and_then(Value::as_str);
    let result = match name {
        "brain_search" => match arguments.get("query").and_then(Value::as_str) {
            Some(query) => {
                let limit = arguments
                    .get("limit")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok());
                client.search(query, project, limit)
            }
            None => return error_response(id, -32602, "brain_search needs a query"),
        },
        "brain_page" => match arguments.get("slug").and_then(Value::as_str) {
            Some(slug) => client.page(slug),
            None => return error_response(id, -32602, "brain_page needs a slug"),
        },
        "brain_note" => match arguments.get("body").and_then(Value::as_str) {
            Some(body) => client.note(body, project),
            None => return error_response(id, -32602, "brain_note needs a body"),
        },
        other => return error_response(id, -32601, &format!("unknown tool: {other}")),
    };
    match result {
        Ok(value) => ok(id, tool_content(&value)),
        Err(error) => ok(
            id,
            json!({ "content": [{ "type": "text", "text": error.to_string() }], "isError": true }),
        ),
    }
}

/// Wrap the API's answer as MCP text content (Markdown where there is any).
fn tool_content(value: &Value) -> Value {
    let text = value
        .get("markdown")
        .and_then(Value::as_str)
        .map_or_else(|| value.to_string(), str::to_owned);
    json!({ "content": [{ "type": "text", "text": text }] })
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}
