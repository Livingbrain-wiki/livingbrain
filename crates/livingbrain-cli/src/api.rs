//! The HTTP client for the Living Brain API, and the wire contract it speaks.
//!
//! The CLI holds no state: no cache, no memory, no agent loop; the server does
//! everything else. Every request carries `User-Agent: livingbrain-cli/<ver>`,
//! and every request but the two auth endpoints carries
//! `Authorization: Bearer <access_token>`.
//!
//! | Request | Body | Success response |
//! | :--- | :--- | :--- |
//! | `POST /v1/device-auth/code` | `{"client_id":"livingbrain-cli"}` | `{device_code, user_code, verification_uri, verification_uri_complete?, interval, expires_in}` |
//! | `POST /v1/device-auth/token` | form `grant_type=urn:ietf:params:oauth:grant-type:device_code&device_code=…&client_id=livingbrain-cli` | `{access_token}` |
//! | `POST /v1/ask` | `{question, project?}` | `{answer, citations:[{title, url, quote?}]}` |
//! | `GET /v1/search` | `?q=&project=&limit=` | `{results:[{title, url, snippet}]}` |
//! | `POST /v1/notes` | `{body, project?}` | `{id, url}` |
//! | `GET /v1/pages/{slug}` | — (slug percent-encoded) | `{slug, title, markdown, url}` |
//! | `GET /v1/export` | `?format=obsidian` | a zip response body |
//! | `POST /v1/pages/sources` | `{kind:"import"\|"agent_log", path, body, scope}` | `{id, kind, scope, path, sha256, wikilinks:[], redacted, created, created_at}` (issues #81, #45) |
//!
//! `POST /v1/device-auth/token` answers `400 {error}` with one of
//! `authorization_pending`, `slow_down`, `expired_token`, `access_denied`
//! (RFC 8628 §3.5). Any other non-2xx is read as `{error}` or `{message}`, or as
//! the RFC 9457 problem document the modules answer with (`title`, `detail`).

use std::time::Duration;

use serde_json::{Value, json};

use crate::auth::Token;
use crate::{CliError, CliResult, err};

/// A response body, already read.
type Response = ureq::http::Response<ureq::Body>;

/// What one poll of the token endpoint ended as (RFC 8628 §3.5).
pub enum Poll {
    /// The human approved: here is the access token.
    Issued(String),
    /// `authorization_pending` — keep waiting.
    Pending,
    /// `slow_down` — add five seconds to every later poll.
    SlowDown,
}

impl Poll {
    fn from_body(body: &[u8]) -> CliResult<Self> {
        let value: Value = serde_json::from_slice(body)
            .map_err(|e| err(format!("the token response was not JSON: {e}")))?;
        if let Some(token) = value.get("access_token").and_then(Value::as_str) {
            return Ok(Self::Issued(token.to_owned()));
        }
        match value.get("error").and_then(Value::as_str).unwrap_or("") {
            "authorization_pending" => Ok(Self::Pending),
            "slow_down" => Ok(Self::SlowDown),
            "expired_token" => Err(err("the device code expired before it was approved")),
            "access_denied" => Err(err("the authorization was denied")),
            other => Err(CliError::Api(format!(
                "the server refused the token poll: {other}"
            ))),
        }
    }
}

/// A client for one API base URL: tokenless for `login`, bearer for the rest.
pub struct Client {
    agent: ureq::Agent,
    base: String,
    token: Option<Token>,
}

impl Client {
    /// A tokenless client (the two auth endpoints only need this).
    #[must_use]
    pub fn new(base_url: &str) -> Self {
        let config = ureq::Agent::config_builder()
            .user_agent(concat!("livingbrain-cli/", env!("CARGO_PKG_VERSION")))
            .timeout_global(Some(Duration::from_secs(30)))
            // Read a 4xx/5xx body so the server's own message can be surfaced.
            .http_status_as_error(false)
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
            base: base_url.trim_end_matches('/').to_owned(),
            token: None,
        }
    }

    /// Attach the bearer token every non-auth request uses.
    #[must_use]
    pub fn with_token(mut self, token: Token) -> Self {
        self.token = Some(token);
        self
    }

    fn get(&self, path: &str) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
        let request = self.agent.get(format!("{}{path}", self.base));
        match &self.token {
            Some(token) => request.header("Authorization", format!("Bearer {}", token.expose())),
            None => request,
        }
    }

    fn post(&self, path: &str) -> ureq::RequestBuilder<ureq::typestate::WithBody> {
        let request = self.agent.post(format!("{}{path}", self.base));
        match &self.token {
            Some(token) => request.header("Authorization", format!("Bearer {}", token.expose())),
            None => request,
        }
    }

    /// `POST /v1/device-auth/code` — the device authorization request (RFC 8628 §3.1).
    pub fn device_authorize(&self) -> CliResult<Value> {
        let response = self
            .post("/v1/device-auth/code")
            .send_json(json!({ "client_id": "livingbrain-cli" }))
            .map_err(transport)?;
        read_json(response)
    }

    /// `POST /v1/device-auth/token` — one poll of the device token endpoint.
    pub fn device_token(&self, device_code: &str) -> CliResult<Poll> {
        let form = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device_code),
            // A device code is issued for one client, so the poll names it.
            ("client_id", "livingbrain-cli"),
        ];
        let response = self
            .post("/v1/device-auth/token")
            .send_form(form)
            .map_err(transport)?;
        let status = response.status().as_u16();
        let body = response.into_body().read_to_vec().map_err(transport)?;
        // A granted token (2xx) and the RFC 8628 refusals (400) both carry the
        // JSON `from_body` reads; anything else is a plain API error.
        if (200..300).contains(&status) || status == 400 {
            Poll::from_body(&body)
        } else {
            Err(api_error(status, &body))
        }
    }

    /// `POST /v1/ask`.
    pub fn ask(&self, question: &str, project: Option<&str>) -> CliResult<Value> {
        self.post_json("/v1/ask", json!({ "question": question }), project)
    }

    /// `GET /v1/search`.
    pub fn search(
        &self,
        query: &str,
        project: Option<&str>,
        limit: Option<u32>,
    ) -> CliResult<Value> {
        let mut request = self.get("/v1/search").query("q", query);
        if let Some(project) = project {
            request = request.query("project", project);
        }
        if let Some(limit) = limit {
            request = request.query("limit", limit.to_string());
        }
        read_json(request.call().map_err(transport)?)
    }

    /// `POST /v1/notes`.
    pub fn note(&self, body_text: &str, project: Option<&str>) -> CliResult<Value> {
        self.post_json("/v1/notes", json!({ "body": body_text }), project)
    }

    /// `GET /v1/pages/{slug}`.
    pub fn page(&self, slug: &str) -> CliResult<Value> {
        let path = format!("/v1/pages/{}", encode_segment(slug));
        read_json(self.get(&path).call().map_err(transport)?)
    }

    /// `GET /v1/export` — the zip bytes.
    pub fn export(&self, format: &str) -> CliResult<Vec<u8>> {
        let response = self
            .get("/v1/export")
            .query("format", format)
            .call()
            .map_err(transport)?;
        read_bytes(response)
    }

    /// `POST /v1/pages/sources` — add one source: an imported file (`kind`
    /// `import`) or an agent session's normalised log (`kind` `agent_log`).
    ///
    /// For `import`, `path` is the vault-relative path with `/` separators;
    /// for `agent_log`, it is the session's filing name, `<agent> session
    /// <id>`. `body` is the already-redacted text; the server re-redacts and
    /// derives the wikilinks itself, and answers `201` for a new source or
    /// `200` for one it already holds (the idempotency key is the scope plus
    /// the body's SHA-256).
    pub fn post_source(&self, kind: &str, path: &str, body: &str, scope: &str) -> CliResult<Value> {
        self.post_json(
            "/v1/pages/sources",
            json!({ "kind": kind, "path": path, "body": body, "scope": scope }),
            None,
        )
    }

    /// POST one JSON body (plus `project`) and read the JSON answer.
    fn post_json(&self, path: &str, mut body: Value, project: Option<&str>) -> CliResult<Value> {
        if let Some(project) = project {
            body["project"] = Value::String(project.to_owned());
        }
        read_json(self.post(path).send_json(&body).map_err(transport)?)
    }
}

/// Percent-encode one path segment (RFC 3986 unreserved characters kept).
fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Map a transport failure to a CLI error.
fn transport(error: ureq::Error) -> CliError {
    CliError::Api(format!("could not reach the API: {error}"))
}

/// Read a response, turning a non-2xx into the server's message.
fn read_bytes(response: Response) -> CliResult<Vec<u8>> {
    let status = response.status().as_u16();
    let body = response.into_body().read_to_vec().map_err(transport)?;
    if (200..300).contains(&status) {
        Ok(body)
    } else {
        Err(api_error(status, &body))
    }
}

/// Read a response as JSON.
fn read_json(response: Response) -> CliResult<Value> {
    serde_json::from_slice(&read_bytes(response)?)
        .map_err(|e| err(format!("the server's response was not JSON: {e}")))
}

/// The error for a non-2xx response, from its `{error}` / `{message}`, or from
/// the problem document the modules answer with (RFC 9457: `title`, `detail`).
/// The raw body is the last resort, and only because an unrecognised shape
/// should still say something.
fn api_error(status: u16, body: &[u8]) -> CliError {
    let parsed: Value = serde_json::from_slice(body).unwrap_or_default();
    let field = |name: &str| {
        parsed
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    // A problem document's `detail` is the sentence written for the reader;
    // `title` only classifies it, so it is a prefix rather than a substitute.
    let problem = match (field("title"), field("detail")) {
        (Some(title), Some(detail)) => Some(format!("{title}: {detail}")),
        (_, Some(detail)) => Some(detail),
        (Some(title), None) => Some(title),
        _ => None,
    };
    let message = field("error")
        .or_else(|| field("message"))
        .or(problem)
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_owned());
    let message = if message.is_empty() {
        format!("the API answered HTTP {status}")
    } else if status == 401 {
        format!("{message} — run `livingbrain login`")
    } else {
        message
    };
    CliError::Api(message)
}

#[cfg(test)]
mod tests {
    use super::Poll;

    #[test]
    fn a_token_poll_is_read_by_the_rfc_8628_error_code() {
        assert!(matches!(
            Poll::from_body(br#"{"access_token":"t"}"#).expect("issued"),
            Poll::Issued(t) if t == "t"
        ));
        assert!(matches!(
            Poll::from_body(br#"{"error":"authorization_pending"}"#).expect("pending"),
            Poll::Pending
        ));
        assert!(matches!(
            Poll::from_body(br#"{"error":"slow_down"}"#).expect("slow down"),
            Poll::SlowDown
        ));
        assert!(Poll::from_body(br#"{"error":"expired_token"}"#).is_err());
        assert!(Poll::from_body(br#"{"error":"access_denied"}"#).is_err());
    }
}
