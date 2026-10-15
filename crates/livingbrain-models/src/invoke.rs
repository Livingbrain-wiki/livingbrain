//! Calling a workspace's own connected model: the one path a job uses to
//! spend the key a member connected.
//!
//! Until this existed the module only *probed* a connection; nothing ran a
//! job on it. [`chat`] is the job path, and it keeps every rule the connect
//! path set:
//!
//! - **Which connection** is [`crate::selection::resolve`]'s answer — the
//!   role's own, else Main. Nothing connected is [`InvokeError::NoModel`],
//!   never a silent fall back to some other model: the caller decides what
//!   "no model" means for its job (the coding-agents compiler parks the job
//!   as `skipped-no-model`).
//! - **The key** is decrypted here, used for one request, and dropped. It
//!   never reaches a return value, an error, or a log line. A provider's
//!   error body *is* carried (issue: a compile that failed as a bare
//!   `model-http` could not be diagnosed), but only as [`Upstream::body`]:
//!   the key itself replaced, redacted, scrubbed, on one line, and cut to
//!   [`MAX_DETAIL_CHARS`].
//! - **The SSRF guard** runs again on every call, over the stored base URL:
//!   a host that resolved to a public address at connect time may not any
//!   more, and the guard is cheap next to a completion.
//! - **The cost ledger** (issue #12) records every call that reported
//!   usage, under the workspace, exactly like the connect probe does. The
//!   usage is also handed back, so a caller with its own budget can count
//!   it without a second read.
//! - **Streaming.** A call asks for `"stream": true` unless the provider's
//!   card says it cannot ([`crate::curated::streams`]), so a thinking model
//!   sends bytes the whole time it reasons and no front cuts it as idle
//!   (Z.AI's 524 at 100 s). The call goes out through the port's streaming
//!   send and its events are read as they arrive ([`crate::stream`]), the
//!   whole body under the call's own [`ChatRequest::timeout`]; a provider
//!   that ignored the flag answers JSON, which is read as before. An
//!   unstreamed call is held to the port's buffered ceiling (30 s).
//!
//! The text that goes out is the caller's. This module does not redact it:
//! the caller knows what the text is and is the one that must have redacted
//! it before it got here (the coding-agents compiler does, on the way in
//! and on the way out).

use std::time::Duration;

use bytes::Bytes;
use cratefield_core::axum::http;
use cratefield_core::{Clock, Config, Database, HttpClient, HttpPolicy};
use serde_json::{Value, json};

use crate::catalog::{self, Auth, Wire};
use crate::{crypto, curated, selection, sse, ssrf, store};

/// The `anthropic-version` every Messages request names.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The largest completion body a job call reads back. A JSON edit list for
/// a handful of wiki pages fits well inside it; anything past it is a
/// provider misbehaving, and the call fails rather than buffering it.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// The most characters of a provider's error body an [`InvokeError`]
/// carries: enough for `{"error":{"code":"1302","message":"Rate limit…"}}`,
/// not enough for a provider that quotes the prompt back to matter.
pub const MAX_DETAIL_CHARS: usize = 300;

/// How much of an error body is read at all before it is made safe.
const MAX_DETAIL_READ_BYTES: usize = 4 * 1024;

/// One job's request to the workspace's model.
#[derive(Debug, Clone)]
pub struct ChatRequest<'a> {
    /// The system prompt: the job's own instructions.
    pub system: &'a str,
    /// The user turn: the job's data.
    pub user: &'a str,
    /// The completion ceiling, in tokens.
    pub max_tokens: u32,
    /// Ask the provider for a JSON object where its wire has a switch for
    /// it (`response_format` on the OpenAI wire). The Anthropic wire has
    /// none; the prompt has to ask.
    pub json: bool,
    /// Ask for the answer as one forced tool call whose input is held to a
    /// JSON Schema, where the provider's card says it serves that
    /// ([`crate::curated::structured`]); elsewhere the call goes out as
    /// plain text (with `json` as above) and the caller parses the text.
    /// Either way the reply's `text` is the JSON.
    pub structured: Option<Structured<'a>>,
    /// How long one call may take: a streamed call's whole body, capped at
    /// [`crate::stream::MAX_STREAM_DEADLINE`]; an unstreamed call's
    /// exchange, clamped by the port to 30 s.
    pub timeout: Duration,
}

/// The one tool a structured call is forced to call: its input is the
/// answer. Anthropic wire: `tools` + `tool_choice: {type: tool}`; OpenAI
/// wire: a `function` tool + `tool_choice: {type: function}`.
#[derive(Debug, Clone, Copy)]
pub struct Structured<'a> {
    pub name: &'a str,
    pub description: &'a str,
    /// The input's JSON Schema.
    pub schema: &'a Value,
}

/// What came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatReply {
    /// The completion's text, concatenated across text blocks.
    pub text: String,
    /// The tokens the provider said the call used; zero when it named none
    /// (the call is still recorded as one request).
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Whether the provider reported usage at all.
    pub usage_reported: bool,
    /// The connection that served the call: its role, provider and model.
    pub role: String,
    pub provider: String,
    pub model: String,
    /// Whether a member's own runner answered (see [`crate::jobs`]): its
    /// tokens were spent on their subscription and cost the brain nothing.
    pub runner: bool,
    /// The provider stopped at `max_tokens` (`stop_reason: max_tokens`,
    /// `finish_reason: length`): the text is likely cut off.
    pub truncated: bool,
}

/// The connection a job for `role` would run on, without its key: the
/// role the resolver picked, the provider, the model, and the model's context
/// window in tokens when the connection probe read one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Serving {
    pub role: String,
    pub provider: String,
    pub model: String,
    /// The context window the provider reported at connect time
    /// ([`parse_context_size`] of the stored value); `None` when it named
    /// none.
    pub context_size: Option<u64>,
}

/// A stored `context_size` as a token count. The probe stores whatever JSON
/// value the provider answered with — a number (`1000000`) or a string
/// (`"1000000"`, `"128k"`, `"1M"`) — so this reads all of those, and answers
/// `None` for anything else rather than guessing.
#[must_use]
pub fn parse_context_size(raw: &str) -> Option<u64> {
    let cleaned: String = raw
        .trim()
        .trim_matches('"')
        .chars()
        .filter(|c| !matches!(c, ',' | '_' | ' '))
        .collect();
    let lower = cleaned.to_ascii_lowercase();
    let (digits, scale) = if let Some(number) = lower.strip_suffix('k') {
        (number, 1_000)
    } else if let Some(number) = lower.strip_suffix('m') {
        (number, 1_000_000)
    } else {
        (lower.as_str(), 1)
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|value| value.checked_mul(scale))
        .filter(|value| *value > 0)
}

/// A provider's non-2xx answer, made safe to store and show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    /// The HTTP status.
    pub status: u16,
    /// The provider's error body, made safe by [`safe_detail`]: the key
    /// replaced, redacted, scrubbed, on one line, at most
    /// [`MAX_DETAIL_CHARS`] characters. Empty when it sent none.
    pub body: String,
    /// The provider's `Retry-After`, in seconds, when it sent one.
    pub retry_after_secs: Option<u64>,
}

/// Why a call did not happen, or did not answer.
///
/// No variant carries the key. A provider's error body is carried only as
/// [`Upstream::body`] and a transport failure only as
/// [`InvokeError::Transport`], both made safe by [`safe_detail`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvokeError {
    /// Nothing serves the role in this workspace.
    NoModel,
    /// `MODEL_KEYS_SECRET` is not set, so no stored key can be opened.
    NoSecret,
    /// The stored key would not decrypt under this deployment's secret.
    Key,
    /// The stored base URL is refused by the SSRF guard now.
    Refused,
    /// The call could not be made at all (`None`), or answered this status.
    /// [`chat`] itself answers [`InvokeError::Status`] or
    /// [`InvokeError::Transport`]; this is for callers with no ports.
    Http(Option<u16>),
    /// The provider answered a non-2xx status.
    Status(Upstream),
    /// The request never got an answer: the transport failed or the
    /// deadline passed. The message is made safe by [`safe_detail`].
    Transport(String),
    /// The provider answered 2xx with something that is not a completion.
    BadResponse,
    /// The provider stopped at the completion ceiling before writing any
    /// answer: a thinking model spent the whole `max_tokens` thinking.
    Exhausted,
    /// The connections table could not be read.
    Store,
    /// A member's runner reported a failure; the string is one of
    /// [`crate::jobs::FAIL_REASONS`].
    Runner(String),
}

impl InvokeError {
    /// A short, stable name for a status column or a log line.
    #[must_use]
    pub fn variant(&self) -> &'static str {
        match self {
            Self::NoModel => "no-model",
            Self::NoSecret => "no-secret",
            Self::Key => "key",
            Self::Refused => "ssrf-refused",
            Self::Http(_) | Self::Status(_) | Self::Transport(_) => "http",
            Self::BadResponse => "bad-response",
            Self::Exhausted => "max-tokens",
            Self::Store => "store",
            Self::Runner(_) => "runner",
        }
    }

    /// The failure as one line for a status column, a log line or an API
    /// answer: the [`InvokeError::variant`], then what the provider said —
    /// `http 429: {"error":{…}}`, `http: deadline exceeded after 300s`,
    /// `runner-timeout`. Never the key (see [`safe_detail`]).
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::Status(upstream) if upstream.body.is_empty() => {
                format!("http {}", upstream.status)
            }
            Self::Status(upstream) => format!("http {}: {}", upstream.status, upstream.body),
            Self::Transport(message) => format!("http: {message}"),
            Self::Http(Some(status)) => format!("http {status}"),
            Self::Runner(reason) => format!("runner-{reason}"),
            other => other.variant().to_owned(),
        }
    }

    /// Whether the provider said it is rate-limiting this key (HTTP 429):
    /// nothing was spent, and the same call later is expected to work.
    #[must_use]
    pub fn rate_limited(&self) -> bool {
        matches!(self, Self::Status(upstream) if upstream.status == 429)
    }

    /// The provider's `Retry-After`, in seconds, when it sent one.
    #[must_use]
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            Self::Status(upstream) => upstream.retry_after_secs,
            _ => None,
        }
    }
}

impl std::fmt::Display for InvokeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoModel => write!(f, "no model is connected for this job"),
            Self::NoSecret => write!(f, "MODEL_KEYS_SECRET is not configured"),
            Self::Key => write!(f, "the stored model key could not be opened"),
            Self::Refused => write!(f, "the model endpoint is refused by the SSRF guard"),
            Self::Http(Some(status)) => write!(f, "the model endpoint answered HTTP {status}"),
            Self::Http(None) => write!(f, "the model endpoint could not be reached"),
            Self::Status(upstream) if upstream.body.is_empty() => {
                write!(f, "the model endpoint answered HTTP {}", upstream.status)
            }
            Self::Status(upstream) => write!(
                f,
                "the model endpoint answered HTTP {}: {}",
                upstream.status, upstream.body
            ),
            Self::Transport(message) => {
                write!(f, "the model endpoint could not be reached ({message})")
            }
            Self::BadResponse => write!(f, "the model endpoint answered with no completion"),
            Self::Exhausted => write!(
                f,
                "the model spent its whole completion budget before answering"
            ),
            Self::Store => write!(f, "the model connections could not be read"),
            Self::Runner(reason) => write!(f, "your runner could not answer ({reason})"),
        }
    }
}

impl std::error::Error for InvokeError {}

/// Text from a provider — an error body, a transport message — made safe
/// to store and show: `key` replaced wherever it appears whole, then the
/// redactor's findings removed, then the logging scrubber (bearer
/// credentials, addresses, query strings), then flattened onto one line and
/// cut to [`MAX_DETAIL_CHARS`]. Text the redactor refuses becomes nothing.
#[must_use]
pub fn safe_detail(raw: &str, key: &str) -> String {
    let mut end = raw.len().min(MAX_DETAIL_READ_BYTES);
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    let mut text = raw[..end].to_owned();
    let key = key.trim();
    if key.len() >= 8 {
        text = text.replace(key, "[redacted]");
    }
    let Ok((redacted, _)) = livingbrain_redact::redact(&text, livingbrain_redact::Policy::Redact)
    else {
        return String::new();
    };
    let scrubbed = cratefield_core::scrub_text(&redacted);
    let flat = scrubbed
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX_DETAIL_CHARS {
        return flat;
    }
    let mut cut: String = flat.chars().take(MAX_DETAIL_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// A `Retry-After` header in seconds. The HTTP-date form is not read: no
/// provider this module talks to sends it, and a guess is worse than none.
fn retry_after(headers: &http::HeaderMap) -> Option<u64> {
    headers
        .get(http::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// A transport failure, made safe ([`safe_detail`]).
fn transport(error: &cratefield_core::HttpError, key: &str) -> InvokeError {
    use cratefield_core::HttpError;
    let message = match error {
        HttpError::DeadlineExceeded { after } => {
            format!("deadline exceeded after {}s", after.as_secs())
        }
        HttpError::ResponseTooLarge { limit } => format!("response over {limit} bytes"),
        HttpError::BlockedDestination(_) => return InvokeError::Refused,
        HttpError::Transport(message) => format!("transport: {message}"),
        // The runtime's outbound budget refused the call before it was sent.
        HttpError::BudgetExhausted { retry_after, .. } => format!(
            "outbound budget exhausted, retry after {}s",
            retry_after.as_secs()
        ),
    };
    InvokeError::Transport(safe_detail(&message, key))
}

/// The connection that would serve a job for `role` in `workspace_id`, or
/// `None` when nothing does. Reads no key.
///
/// # Errors
///
/// [`InvokeError::Store`] when the table cannot be read.
pub async fn serving(
    db: &dyn Database,
    workspace_id: &str,
    role: &str,
) -> Result<Option<Serving>, InvokeError> {
    let rows = store::list(db, workspace_id)
        .await
        .map_err(|_| InvokeError::Store)?;
    Ok(selection::resolve(&rows, role).map(|row| Serving {
        role: row.role.clone(),
        provider: row.provider.clone(),
        model: row.model.clone(),
        context_size: row.context_size.as_deref().and_then(parse_context_size),
    }))
}

/// Runs one chat completion on the connection that serves `role` in
/// `workspace_id`, records its usage in the cost ledger, and answers the
/// text.
///
/// # Errors
///
/// See [`InvokeError`]. Nothing is recorded for a call that failed before
/// the provider answered.
pub async fn chat(
    db: &dyn Database,
    http: &dyn HttpClient,
    config: &dyn Config,
    clock: &dyn Clock,
    workspace_id: &str,
    role: &str,
    request: &ChatRequest<'_>,
) -> Result<ChatReply, InvokeError> {
    let rows = store::list(db, workspace_id)
        .await
        .map_err(|_| InvokeError::Store)?;
    let connection = selection::resolve(&rows, role)
        .ok_or(InvokeError::NoModel)?
        .clone();
    let secret = crypto::derive_key(config).ok_or(InvokeError::NoSecret)?;
    let ciphertext = store::key_ciphertext(db, workspace_id, &connection.role)
        .await
        .map_err(|_| InvokeError::Store)?
        .ok_or(InvokeError::NoModel)?;
    let key = crypto::decrypt(&secret, &ciphertext, workspace_id, &connection.role)
        .map_err(|_| InvokeError::Key)?;
    let key = String::from_utf8(key).map_err(|_| InvokeError::Key)?;

    let wire = Wire::parse(&connection.wire).ok_or(InvokeError::BadResponse)?;
    let auth = Auth::parse(&connection.auth).ok_or(InvokeError::BadResponse)?;
    let url = ssrf::validate_url(&connection.base_url).map_err(|_| InvokeError::Refused)?;
    ssrf::check_destination(http, &url)
        .await
        .map_err(|_| InvokeError::Refused)?;

    let stream = curated::streams(&connection.provider);
    let structured = request
        .structured
        .filter(|_| curated::structured(&connection.provider));
    let mut body = request_body(wire, &connection.model, request, structured);
    if stream {
        stream_body(&mut body, wire);
    }
    if wire == Wire::Anthropic
        && thinking_switch(&connection.provider, &url)
        && thinking_optional(&connection.model)
    {
        // A thinking model spends its budget on a `thinking` block before
        // the answer; a job wants the answer. Z.AI's GLM takes the switch on
        // the series that allow it; GLM-5.3 thinks always, and its thinking
        // blocks are skipped when the answer is read.
        body["thinking"] = json!({"type": "disabled"});
    }
    let segments: &[&str] = match wire {
        Wire::Anthropic => &["v1", "messages"],
        Wire::Openai => &["v1", "chat", "completions"],
    };
    let target = catalog::join(&url, segments).ok_or(InvokeError::Refused)?;
    // The port clamps the head (and a buffered call's whole exchange) to
    // its 30 s ceiling; a streamed call's body runs under its own
    // [`StreamPolicy`](cratefield_core::StreamPolicy): the idle gap between
    // chunks and the call's deadline for the whole (see [`crate::stream`]).
    let mut builder = http::Request::builder()
        .method(http::Method::POST)
        .uri(target.as_str())
        .header(http::header::CONTENT_TYPE, "application/json")
        .extension(HttpPolicy {
            max_response_bytes: MAX_RESPONSE_BYTES,
            timeout: request.timeout,
        });
    if stream {
        builder = builder.extension(crate::stream::policy(request.timeout));
    }
    builder = match auth {
        Auth::Bearer => builder.header(http::header::AUTHORIZATION, format!("Bearer {key}")),
        Auth::XApiKey => builder.header("x-api-key", key.as_str()),
    };
    if wire == Wire::Anthropic {
        builder = builder.header("anthropic-version", ANTHROPIC_VERSION);
    }
    let outgoing = builder
        .body(Bytes::from(body.to_string()))
        .map_err(|_| InvokeError::Http(None))?;
    // The key is kept only until the answer is read, so that a provider (or
    // a transport layer) quoting it back can have it taken out of what the
    // failure carries; see [`safe_detail`].
    let sent = if stream {
        crate::stream::receive(http, outgoing, wire, MAX_RESPONSE_BYTES).await
    } else {
        http.send(outgoing).await
    };
    let response = match sent {
        Ok(response) => response,
        Err(error) => return Err(transport(&error, &key)),
    };
    if !response.status().is_success() {
        let upstream = Upstream {
            status: response.status().as_u16(),
            body: safe_detail(&String::from_utf8_lossy(response.body()), &key),
            retry_after_secs: retry_after(response.headers()),
        };
        return Err(InvokeError::Status(upstream));
    }
    let parsed = if event_stream(response.headers()) {
        // An unstreamed call whose provider streamed anyway: read the
        // buffered events.
        match sse::read_body(response.body(), wire, MAX_RESPONSE_BYTES) {
            Ok(completion) => completion.native_json(wire),
            Err(failure) => {
                return Err(InvokeError::Status(Upstream {
                    status: failure.status,
                    body: safe_detail(&failure.body, &key),
                    retry_after_secs: None,
                }));
            }
        }
    } else {
        serde_json::from_slice(response.body()).map_err(|_| InvokeError::BadResponse)?
    };
    drop(key);
    let text = completion_text(&parsed, wire);
    // Nothing but thinking, cut at the ceiling: say so, rather than "no
    // completion", so the caller knows a bigger budget is the fix. Those
    // tokens were spent, so they are recorded like an answer's.
    let exhausted = text.is_none() && stopped_at_ceiling(&parsed, wire);
    if text.is_none() && !exhausted {
        return Err(InvokeError::BadResponse);
    }
    let truncated = stopped_at_ceiling(&parsed, wire);
    let usage = usage(&parsed, wire);
    let (prompt_tokens, completion_tokens) = usage.unwrap_or((0, 0));
    // The ledger (issue #12). A lost count must never fail the call that
    // spent the tokens, so the error is dropped, as the probe drops it.
    let _ = livingbrain_usage::record_model(
        db,
        clock.now(),
        workspace_id,
        prompt_tokens,
        completion_tokens,
    )
    .await;
    let Some(text) = text else {
        return Err(InvokeError::Exhausted);
    };
    Ok(ChatReply {
        text,
        prompt_tokens,
        completion_tokens,
        usage_reported: usage.is_some(),
        role: connection.role,
        provider: connection.provider,
        model: connection.model,
        runner: false,
        truncated,
    })
}

/// Whether the provider takes Anthropic's `thinking` switch to turn a
/// thinking model's reasoning off: Z.AI's GLM endpoints (`zhipu-glm*`, or a
/// base URL on `z.ai` / `bigmodel.cn`). Never Anthropic itself, whose
/// models only think when asked to.
fn thinking_switch(provider: &str, base: &url::Url) -> bool {
    provider.starts_with("zhipu-glm")
        || base.host_str().is_some_and(|host| {
            host == "api.z.ai"
                || host.ends_with(".z.ai")
                || host == "open.bigmodel.cn"
                || host.ends_with(".bigmodel.cn")
        })
}

/// Whether a GLM model's thinking may be turned off. Z.AI's thinking-mode
/// guide (docs.z.ai/guides/capabilities/thinking-mode, 2026-10-15): the
/// GLM-5.2, GLM-5.1, GLM-5 and GLM-4.7 series take `thinking: disabled`;
/// GLM-5.3 and GLM-5.3-Flash "use forced thinking and cannot be disabled",
/// so they are never sent the switch. An unknown model is left alone.
fn thinking_optional(model: &str) -> bool {
    let model = model.trim().to_ascii_lowercase();
    let model = model.split('[').next().unwrap_or_default();
    ["glm-5.2", "glm-5.1", "glm-4.7"]
        .iter()
        .any(|series| model == *series || model.starts_with(&format!("{series}-")))
        || model == "glm-5"
        || model.starts_with("glm-5-")
}

/// The request body for one wire: a forced tool call when `structured`,
/// else plain text (a JSON object where the wire can ask for one).
fn request_body(
    wire: Wire,
    model: &str,
    request: &ChatRequest<'_>,
    structured: Option<Structured<'_>>,
) -> Value {
    match wire {
        Wire::Openai => {
            let mut body = json!({
                "model": model,
                "messages": [
                    {"role": "system", "content": request.system},
                    {"role": "user", "content": request.user},
                ],
                "max_tokens": request.max_tokens,
            });
            if let Some(tool) = structured {
                body["tools"] = json!([{
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.schema,
                    },
                }]);
                body["tool_choice"] = json!({"type": "function", "function": {"name": tool.name}});
            } else if request.json {
                body["response_format"] = json!({"type": "json_object"});
            }
            body
        }
        Wire::Anthropic => {
            let mut body = json!({
                "model": model,
                "system": request.system,
                "messages": [{"role": "user", "content": request.user}],
                "max_tokens": request.max_tokens,
            });
            if let Some(tool) = structured {
                body["tools"] = json!([{
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.schema,
                }]);
                body["tool_choice"] = json!({"type": "tool", "name": tool.name});
            }
            body
        }
    }
}

/// Asks for the completion as a stream: `"stream": true`, and on the
/// OpenAI wire the usage chunk at the end (`stream_options.include_usage`),
/// without which a streamed call reports no tokens.
fn stream_body(body: &mut Value, wire: Wire) {
    body["stream"] = json!(true);
    if wire == Wire::Openai {
        body["stream_options"] = json!({"include_usage": true});
    }
}

/// Whether a response is a Server-Sent Events body.
fn event_stream(headers: &http::HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim_start().starts_with("text/event-stream"))
}

/// The completion's text, per wire; `None` when the answer carries none.
/// A forced tool call's input is the answer: its JSON text.
fn completion_text(parsed: &Value, wire: Wire) -> Option<String> {
    match wire {
        // `content`, else the first tool call's arguments (a JSON string).
        // Never `reasoning_content`: a reasoning model's thinking is not its
        // answer, and an empty `content` (the budget went on thinking) is no
        // completion at all.
        Wire::Openai => {
            let message = parsed.get("choices")?.get(0)?.get("message")?;
            message
                .get("content")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .or_else(|| {
                    message
                        .pointer("/tool_calls/0/function/arguments")
                        .and_then(Value::as_str)
                        .filter(|text| !text.trim().is_empty())
                })
                .map(str::to_owned)
        }
        // A `tool_use` block's input, when there is one; else every `text`
        // block, in order: `thinking` and `redacted_thinking` blocks (a GLM
        // answer starts with one) are skipped.
        Wire::Anthropic => {
            let blocks = parsed.get("content")?.as_array()?;
            if let Some(input) = blocks
                .iter()
                .find(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                .and_then(|b| b.get("input"))
            {
                // A streamed input that did not parse (cut off) is carried
                // as its raw text, so the caller can say why.
                return match input {
                    Value::String(raw) => Some(raw.clone()),
                    other => Some(other.to_string()),
                }
                .filter(|text| !text.trim().is_empty());
            }
            let text: String = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            Some(text).filter(|text| !text.trim().is_empty())
        }
    }
}

/// Whether the answer says it stopped at `max_tokens`.
fn stopped_at_ceiling(parsed: &Value, wire: Wire) -> bool {
    match wire {
        Wire::Anthropic => parsed.get("stop_reason").and_then(Value::as_str) == Some("max_tokens"),
        Wire::Openai => {
            parsed
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
                == Some("length")
        }
    }
}

/// The `(prompt, completion)` token counts a response reported.
fn usage(parsed: &Value, wire: Wire) -> Option<(u64, u64)> {
    let usage = parsed.get("usage")?;
    let (prompt, completion) = match wire {
        Wire::Openai => ("prompt_tokens", "completion_tokens"),
        Wire::Anthropic => ("input_tokens", "output_tokens"),
    };
    Some((
        usage.get(prompt)?.as_u64()?,
        usage.get(completion)?.as_u64()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_context_size_reads_as_tokens() {
        assert_eq!(parse_context_size("1000000"), Some(1_000_000));
        assert_eq!(parse_context_size("\"1000000\""), Some(1_000_000));
        assert_eq!(parse_context_size("\"128k\""), Some(128_000));
        assert_eq!(parse_context_size("1M"), Some(1_000_000));
        assert_eq!(parse_context_size("1,048,576"), Some(1_048_576));
        assert_eq!(parse_context_size("0"), None);
        assert_eq!(parse_context_size("unknown"), None);
        assert_eq!(parse_context_size(""), None);
    }

    #[test]
    fn the_openai_body_asks_for_json_only_when_told() {
        let request = ChatRequest {
            system: "s",
            user: "u",
            max_tokens: 10,
            json: true,
            structured: None,
            timeout: Duration::from_secs(1),
        };
        let body = request_body(Wire::Openai, "m", &request, None);
        assert_eq!(body["response_format"]["type"], "json_object");
        assert_eq!(body["messages"][0]["role"], "system");
        let plain = ChatRequest {
            json: false,
            ..request
        };
        assert!(
            request_body(Wire::Openai, "m", &plain, None)
                .get("response_format")
                .is_none()
        );
    }

    #[test]
    fn a_structured_call_forces_its_one_tool_on_both_wires() {
        let schema = json!({"type": "object", "properties": {"pages": {"type": "array"}}});
        let tool = Structured {
            name: "apply_edits",
            description: "the edit list",
            schema: &schema,
        };
        let request = ChatRequest {
            system: "s",
            user: "u",
            max_tokens: 10,
            json: true,
            structured: Some(tool),
            timeout: Duration::from_secs(1),
        };
        let anthropic = request_body(Wire::Anthropic, "m", &request, Some(tool));
        assert_eq!(anthropic["tools"][0]["name"], "apply_edits");
        assert_eq!(anthropic["tools"][0]["input_schema"], schema);
        assert_eq!(
            anthropic["tool_choice"],
            json!({"type": "tool", "name": "apply_edits"})
        );
        let openai = request_body(Wire::Openai, "m", &request, Some(tool));
        assert_eq!(openai["tools"][0]["type"], "function");
        assert_eq!(openai["tools"][0]["function"]["parameters"], schema);
        assert_eq!(openai["tool_choice"]["function"]["name"], "apply_edits");
        // The schema replaces the looser JSON switch.
        assert!(openai.get("response_format").is_none());
    }

    #[test]
    fn a_forced_tool_call_is_read_as_the_answer_on_both_wires() {
        let anthropic = json!({
            "content": [
                {"type": "thinking", "thinking": "…"},
                {"type": "tool_use", "id": "t", "name": "apply_edits",
                 "input": {"summary": "s", "pages": []}}
            ],
            "stop_reason": "tool_use"
        });
        let text = completion_text(&anthropic, Wire::Anthropic).expect("an answer");
        assert_eq!(
            serde_json::from_str::<Value>(&text).expect("json"),
            json!({"summary": "s", "pages": []})
        );
        assert!(!stopped_at_ceiling(&anthropic, Wire::Anthropic));
        // A streamed input cut off at the limit arrives as its raw text.
        let cut = json!({
            "content": [{"type": "tool_use", "name": "apply_edits", "input": "{\"summary\": \"s"}],
            "stop_reason": "max_tokens"
        });
        assert_eq!(
            completion_text(&cut, Wire::Anthropic).as_deref(),
            Some("{\"summary\": \"s")
        );
        assert!(stopped_at_ceiling(&cut, Wire::Anthropic));
        let openai = json!({"choices": [{
            "message": {"role": "assistant", "content": null, "tool_calls": [{
                "type": "function",
                "function": {"name": "apply_edits", "arguments": "{\"summary\":\"s\",\"pages\":[]}"}
            }]},
            "finish_reason": "length"
        }]});
        assert_eq!(
            completion_text(&openai, Wire::Openai).as_deref(),
            Some("{\"summary\":\"s\",\"pages\":[]}")
        );
        assert!(stopped_at_ceiling(&openai, Wire::Openai));
    }

    #[test]
    fn the_anthropic_body_carries_the_system_prompt_apart() {
        let request = ChatRequest {
            system: "rules",
            user: "data",
            max_tokens: 10,
            json: true,
            structured: None,
            timeout: Duration::from_secs(1),
        };
        let body = request_body(Wire::Anthropic, "m", &request, None);
        assert_eq!(body["system"], "rules");
        assert_eq!(body["messages"][0]["content"], "data");
    }

    #[test]
    fn a_streamed_body_asks_for_usage_on_the_openai_wire() {
        let request = ChatRequest {
            system: "s",
            user: "u",
            max_tokens: 10,
            json: false,
            structured: None,
            timeout: Duration::from_secs(1),
        };
        let mut openai = request_body(Wire::Openai, "m", &request, None);
        stream_body(&mut openai, Wire::Openai);
        assert_eq!(openai["stream"], true);
        assert_eq!(openai["stream_options"]["include_usage"], true);
        let mut anthropic = request_body(Wire::Anthropic, "m", &request, None);
        stream_body(&mut anthropic, Wire::Anthropic);
        assert_eq!(anthropic["stream"], true);
        assert!(anthropic.get("stream_options").is_none());
    }

    #[test]
    fn text_and_usage_read_per_wire() {
        let openai = json!({
            "choices": [{"message": {"content": "{\"ok\":true}"}}],
            "usage": {"prompt_tokens": 7, "completion_tokens": 3}
        });
        assert_eq!(
            completion_text(&openai, Wire::Openai).as_deref(),
            Some("{\"ok\":true}")
        );
        assert_eq!(usage(&openai, Wire::Openai), Some((7, 3)));
        let anthropic = json!({
            "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}],
            "usage": {"input_tokens": 5, "output_tokens": 2}
        });
        assert_eq!(
            completion_text(&anthropic, Wire::Anthropic).as_deref(),
            Some("ab")
        );
        assert_eq!(usage(&anthropic, Wire::Anthropic), Some((5, 2)));
        assert_eq!(usage(&json!({}), Wire::Openai), None);
    }

    /// A GLM-style thinking model: the Anthropic-wire answer starts with a
    /// `thinking` block, and the OpenAI-wire one carries the reasoning in
    /// `reasoning_content`. Only the answer is read.
    #[test]
    fn a_thinking_block_is_not_the_answer() {
        let anthropic = json!({
            "content": [
                {"type": "thinking", "thinking": "Let me think…", "signature": "s"},
                {"type": "redacted_thinking", "data": "x"},
                {"type": "text", "text": "{\"pages\":[]}"}
            ]
        });
        assert_eq!(
            completion_text(&anthropic, Wire::Anthropic).as_deref(),
            Some("{\"pages\":[]}")
        );
        let only_thinking = json!({"content": [{"type": "thinking", "thinking": "…"}]});
        assert_eq!(completion_text(&only_thinking, Wire::Anthropic), None);
        let openai = json!({"choices": [{"message": {
            "content": "", "reasoning_content": "thinking out loud"
        }, "finish_reason": "length"}]});
        assert_eq!(completion_text(&openai, Wire::Openai), None);
        let answered = json!({"choices": [{"message": {
            "content": "ok", "reasoning_content": "…"
        }}]});
        assert_eq!(
            completion_text(&answered, Wire::Openai).as_deref(),
            Some("ok")
        );
    }

    #[test]
    fn forced_thinking_models_are_never_sent_the_switch() {
        for model in [
            "glm-5.2",
            "GLM-5.1",
            "glm-5",
            "glm-5-turbo",
            "glm-4.7-flash",
            "glm-5.2[1m]",
        ] {
            assert!(thinking_optional(model), "{model}");
        }
        for model in [
            "glm-5.3",
            "glm-5.3-flash",
            "glm-5.3[1m]",
            "glm-5.30",
            "claude-x",
            "",
        ] {
            assert!(!thinking_optional(model), "{model}");
        }
    }

    #[test]
    fn only_glm_hosts_get_the_thinking_switch() {
        let url = |s: &str| url::Url::parse(s).expect("url");
        assert!(thinking_switch(
            "zhipu-glm-en",
            &url("https://api.z.ai/api/anthropic")
        ));
        assert!(thinking_switch(
            "custom",
            &url("https://api.z.ai/api/anthropic")
        ));
        assert!(thinking_switch(
            "custom",
            &url("https://open.bigmodel.cn/api/anthropic")
        ));
        assert!(!thinking_switch(
            "anthropic",
            &url("https://api.anthropic.com")
        ));
        assert!(!thinking_switch(
            "custom",
            &url("https://notz.ai.example.com")
        ));
    }
}
