//! The capability probe: on connect, send a few small requests to the
//! model endpoint and report what works — over whichever wire the provider
//! speaks.
//!
//! The probe sends at most three requests:
//! 1. Tool calling — a trivial tool the model is required to call
//!    (`tool_choice: "required"` on the OpenAI wire, `{"type":"any"}` on the
//!    Anthropic one).
//! 2. JSON output — `response_format: {"type":"json_object"}` on the OpenAI
//!    wire; the Anthropic wire has no such switch, so it asks for a bare JSON
//!    object and checks that it got one.
//! 3. Context size — `GET {base}/v1/models/{model}`, best effort.
//!
//! The key travels as the provider's catalog says: `Authorization: Bearer`
//! or Anthropic's `x-api-key`. The Anthropic wire also sends
//! `anthropic-version`. Paths are joined the way Colonizer's gateway joins
//! them ([`catalog::join`]): a base URL that already ends in a version
//! segment does not get a second `/v1`.
//!
//! `base` is the *checked* [`Url`], not the string a member sent: the host
//! the guard approved is the host the probe connects to, and the path
//! segments are appended to it rather than concatenated onto a raw string.
//! The model name is pushed as one percent-encoded path segment.
//!
//! If the very first call fails (non-2xx or transport error), the endpoint
//! is unreachable: the connection is refused with a 422 that names the
//! HTTP status only — never the provider's response body, which can echo
//! the key. The probe sets a tight `HttpPolicy` so a slow or chatty
//! endpoint cannot cost much, and a `max_tokens` just large enough for a
//! thinking model: GLM-5.3 and its kind reason before they answer (a
//! `thinking` block first on the Anthropic wire, `reasoning_content` on the
//! OpenAI one) and cannot turn it off, so a ceiling of a few tokens is
//! spent on the reasoning and the answer never arrives. The answer is read
//! from every block of the reply, whatever came before it.
//!
//! When the endpoint reports no context size, the size the provider
//! documents for the model (`app/assets/model-cards.json`, through
//! [`crate::curated`]) is the answer.

use std::time::Duration;

use bytes::Bytes;
use cratefield_core::axum::http;
use cratefield_core::{HttpClient, HttpPolicy};
use serde_json::{Value, json};
use url::Url;

use crate::catalog::{self, Auth, Wire};
use crate::curated::{self, Listing};
use crate::ssrf;

/// The `anthropic-version` every Messages request names.
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// The most model ids [`list_models`] hands back, and the longest one.
const MAX_LISTED: usize = 500;
const MAX_ID_LEN: usize = 200;
/// The response ceiling for a model list.
const LIST_MAX_RESPONSE: usize = 1024 * 1024;

/// The maximum response body for a probe reply (well under the port ceiling).
const PROBE_MAX_RESPONSE: usize = 64 * 1024;
/// The timeout for a probe request.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// The `max_tokens` for a probe completion. Room for a thinking model's
/// reasoning before its tool call or its JSON (see the module docs): the
/// reply is still small, and the usage the provider reports — not this
/// ceiling — is what the ledger records.
const PROBE_MAX_TOKENS: u32 = 1024;

/// The result of a capability probe.
#[derive(Debug, Clone)]
pub(crate) struct ProbeResult {
    /// `"works"` when tools and JSON pass, `"answers only"` when the model
    /// answers but tool calling fails.
    pub(crate) status: &'static str,
    /// Which capabilities are missing (e.g. `["tool calling", "JSON output"]`).
    pub(crate) missing: Vec<&'static str>,
    /// The model's context size, if the endpoint reported one.
    pub(crate) context_size: Option<String>,
    /// The token usage the two chat calls reported, summed. `None` is a
    /// provider that named no numbers — unknown, never guessed as zero.
    pub(crate) usage: Option<TokenUsage>,
}

/// The token usage one chat call reported, as both wires name it
/// (`prompt_tokens`/`completion_tokens` on the OpenAI wire,
/// `input_tokens`/`output_tokens` on Anthropic's).
#[derive(Debug, Clone, Copy)]
pub(crate) struct TokenUsage {
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
}

impl TokenUsage {
    /// The sum of two calls' usage: the probe sends two completions.
    fn plus(self, other: Self) -> Self {
        Self {
            prompt_tokens: self.prompt_tokens.saturating_add(other.prompt_tokens),
            completion_tokens: self
                .completion_tokens
                .saturating_add(other.completion_tokens),
        }
    }
}

/// Reads the `usage` object off a parsed chat response, per wire. `None`
/// when the provider named no numbers.
fn parse_usage(parsed: &Value, wire: Wire) -> Option<TokenUsage> {
    let usage = parsed.get("usage")?;
    let (prompt, completion) = match wire {
        Wire::Openai => ("prompt_tokens", "completion_tokens"),
        Wire::Anthropic => ("input_tokens", "output_tokens"),
    };
    Some(TokenUsage {
        prompt_tokens: usage.get(prompt)?.as_u64()?,
        completion_tokens: usage.get(completion)?.as_u64()?,
    })
}

/// The endpoint was unreachable: the first call returned a non-2xx status
/// or a transport error. The HTTP status is named; the body is never echoed.
#[derive(Debug)]
pub(crate) struct Unreachable(pub(crate) Option<u16>);

/// Where a request goes and how it is signed: the checked base URL, the
/// wire, and the auth style, all decided by the catalog (or by the member
/// for `custom`), never by the probe.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Target<'a> {
    pub(crate) base: &'a Url,
    pub(crate) wire: Wire,
    pub(crate) auth: Auth,
    pub(crate) api_key: &'a str,
}

impl Target<'_> {
    /// A request to `segments` under the base, with the key in the header
    /// the provider expects and the probe's tight policy.
    fn request(
        &self,
        method: http::Method,
        segments: &[&str],
        body: Option<&Value>,
    ) -> Result<http::Request<Bytes>, Unreachable> {
        let url = catalog::join(self.base, segments).ok_or(Unreachable(None))?;
        let mut builder = http::Request::builder()
            .method(method)
            .uri(url.as_str())
            .extension(HttpPolicy {
                max_response_bytes: PROBE_MAX_RESPONSE,
                timeout: PROBE_TIMEOUT,
            });
        builder = match self.auth {
            Auth::Bearer => builder.header(
                http::header::AUTHORIZATION,
                format!("Bearer {}", self.api_key),
            ),
            Auth::XApiKey => builder.header("x-api-key", self.api_key),
        };
        if self.wire == Wire::Anthropic {
            builder = builder.header("anthropic-version", ANTHROPIC_VERSION);
        }
        let bytes = match body {
            Some(body) => {
                builder = builder.header(http::header::CONTENT_TYPE, "application/json");
                Bytes::from(body.to_string())
            }
            None => Bytes::new(),
        };
        builder.body(bytes).map_err(|_| Unreachable(None))
    }

    /// A `GET` to `url` itself — a provider's own list URL — signed the way
    /// [`Target::request`] signs a request under the base.
    fn get(&self, url: &Url) -> Result<http::Request<Bytes>, Unreachable> {
        let builder = http::Request::builder()
            .method(http::Method::GET)
            .uri(url.as_str())
            .extension(HttpPolicy {
                max_response_bytes: LIST_MAX_RESPONSE,
                timeout: PROBE_TIMEOUT,
            });
        let builder = match self.auth {
            Auth::Bearer => builder.header(
                http::header::AUTHORIZATION,
                format!("Bearer {}", self.api_key),
            ),
            Auth::XApiKey => builder.header("x-api-key", self.api_key),
        };
        builder.body(Bytes::new()).map_err(|_| Unreachable(None))
    }

    /// The completion endpoint's path for this wire.
    fn chat_path(&self) -> &'static [&'static str] {
        match self.wire {
            Wire::Anthropic => &["v1", "messages"],
            Wire::Openai => &["v1", "chat", "completions"],
        }
    }
}

/// Probes the model endpoint `target` with `model`. Returns a
/// [`ProbeResult`] on success, or [`Unreachable`] if the first call fails.
///
/// # Errors
///
/// [`Unreachable`] when the first basic call fails (non-2xx or transport).
pub(crate) async fn probe(
    http: &dyn HttpClient,
    target: Target<'_>,
    model: &str,
    documented_context: Option<u64>,
) -> Result<ProbeResult, Unreachable> {
    // (a) Tool calling.
    let (tool_ok, tool_usage) = tool_calling_probe(http, target, model).await?;
    // (b) JSON output.
    let (json_ok, json_usage) = json_output_probe(http, target, model).await?;
    // (c) Context size.
    let context_size = context_size_probe(http, target, model)
        .await?
        .or_else(|| documented_context.map(|size| size.to_string()));

    let mut missing = Vec::new();
    if !tool_ok {
        missing.push("tool calling");
    }
    if !json_ok {
        missing.push("JSON output");
    }
    if context_size.is_none() {
        missing.push("context size");
    }

    let status = if tool_ok && json_ok {
        "works"
    } else {
        "answers only"
    };

    Ok(ProbeResult {
        status,
        missing,
        context_size,
        usage: [tool_usage, json_usage]
            .into_iter()
            .flatten()
            .reduce(TokenUsage::plus),
    })
}

/// Sends a trivial tool the model must call. Pass if it calls one; the
/// call's reported token usage, if any, comes back with the verdict.
async fn tool_calling_probe(
    http: &dyn HttpClient,
    target: Target<'_>,
    model: &str,
) -> Result<(bool, Option<TokenUsage>), Unreachable> {
    let body = match target.wire {
        Wire::Openai => json!({
            "model": model,
            "messages": [{"role": "user", "content": "What is 2+2?"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "calculate",
                    "description": "Calculate a math expression",
                    "parameters": {"type": "object", "properties": {}}
                }
            }],
            "tool_choice": "required",
            "max_tokens": PROBE_MAX_TOKENS,
        }),
        Wire::Anthropic => json!({
            "model": model,
            "messages": [{"role": "user", "content": "What is 2+2?"}],
            "tools": [{
                "name": "calculate",
                "description": "Calculate a math expression",
                "input_schema": {"type": "object", "properties": {}}
            }],
            "tool_choice": {"type": "any"},
            "max_tokens": PROBE_MAX_TOKENS,
        }),
    };
    let parsed = send_chat(http, target, &body).await?;
    let usage = parse_usage(&parsed, target.wire);
    let called = match target.wire {
        Wire::Openai => parsed
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("tool_calls"))
            .and_then(Value::as_array)
            .is_some_and(|calls| !calls.is_empty()),
        Wire::Anthropic => parsed
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
            }),
    };
    Ok((called, usage))
}

/// Asks for a JSON object. Pass if the answer's text parses as one; the
/// call's reported token usage, if any, comes back with the verdict.
async fn json_output_probe(
    http: &dyn HttpClient,
    target: Target<'_>,
    model: &str,
) -> Result<(bool, Option<TokenUsage>), Unreachable> {
    let body = match target.wire {
        Wire::Openai => json!({
            "model": model,
            "messages": [{"role": "user", "content": "Return {\"ok\":true}"}],
            "response_format": {"type": "json_object"},
            "max_tokens": PROBE_MAX_TOKENS,
        }),
        Wire::Anthropic => json!({
            "model": model,
            "messages": [{
                "role": "user",
                "content": "Reply with exactly this JSON object and nothing else: {\"ok\":true}"
            }],
            "max_tokens": PROBE_MAX_TOKENS,
        }),
    };
    let parsed = send_chat(http, target, &body).await?;
    let usage = parse_usage(&parsed, target.wire);
    let content = match target.wire {
        Wire::Openai => parsed
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        Wire::Anthropic => parsed
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<String>()
            })
            .unwrap_or_default(),
    };
    let emitted = serde_json::from_str::<Value>(unfenced(&content)).is_ok_and(|v| v.is_object());
    Ok((emitted, usage))
}

/// GETs `{base}/v1/models/{model}` and reads the context length. Returns
/// `None` if the endpoint does not report one (unknown, not a failure).
async fn context_size_probe(
    http: &dyn HttpClient,
    target: Target<'_>,
    model: &str,
) -> Result<Option<String>, Unreachable> {
    let request = target.request(http::Method::GET, &["v1", "models", model], None)?;
    let response = http.send(request).await.map_err(|_| Unreachable(None))?;
    if !response.status().is_success() {
        // Context size is best-effort: a failure here is "unknown", not
        // unreachable (the chat probes already proved the endpoint works).
        return Ok(None);
    }
    let Ok(body) = serde_json::from_slice::<Value>(response.body()) else {
        return Ok(None);
    };
    // Some endpoints nest the model data under "data"; others return it
    // directly. Check both.
    let data = body.get("data").unwrap_or(&body);
    for key in &[
        "context_length",
        "context_window",
        "max_model_len",
        "max_context_length",
        "max_input_tokens",
    ] {
        // A number or a string, both of which name a size; the keys are the
        // spellings providers actually use for it.
        if let Some(value) = data.get(*key)
            && (value.is_i64() || value.is_u64() || value.is_string())
        {
            return Ok(Some(value.to_string()));
        }
    }
    Ok(None)
}

/// What a provider's model list came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Listed {
    /// The ids the provider listed, with the context size where it named one.
    Models(Vec<curated::Model>),
    /// The provider publishes no list we can read: it has no listing route
    /// (`404`, `405`, `501` and the other refusals that are not about the
    /// key), it answered with something that is not a list, it could not be
    /// reached, or its card says it has none. Not an error: the curated
    /// models are the answer.
    Unsupported,
}

/// The model ids the provider says the key can use, from wherever `listing`
/// says it lists them: `GET {base}/v1/models` (Anthropic itself included,
/// with `x-api-key` and `anthropic-version`), an OpenAI-compatible list URL
/// the provider's card names (SSRF-checked like every other destination), or
/// nowhere. Both wires answer `{"data": [{"id": …}, …]}`.
///
/// # Errors
///
/// [`Unreachable`] with the status only when the provider refused the key
/// (`401` or `403`): that is worth saying before a connect, and it is the
/// one refusal a curated list cannot paper over. Every other failure is
/// [`Listed::Unsupported`].
pub(crate) async fn list_models(
    http: &dyn HttpClient,
    target: Target<'_>,
    listing: &Listing,
) -> Result<Listed, Unreachable> {
    let request = match listing {
        Listing::None => return Ok(Listed::Unsupported),
        Listing::Native => {
            let mut request = target.request(http::Method::GET, &["v1", "models"], None)?;
            // A router's list is long (OpenRouter's runs to hundreds of KiB),
            // so this one call gets a larger ceiling than the probe's.
            request.extensions_mut().insert(HttpPolicy {
                max_response_bytes: LIST_MAX_RESPONSE,
                timeout: PROBE_TIMEOUT,
            });
            request
        }
        Listing::Url(url) => {
            let Ok(url) = ssrf::validate_url(url) else {
                return Ok(Listed::Unsupported);
            };
            if ssrf::check_destination(http, &url).await.is_err() {
                return Ok(Listed::Unsupported);
            }
            target.get(&url)?
        }
    };
    let Ok(response) = http.send(request).await else {
        return Ok(Listed::Unsupported);
    };
    let status = response.status().as_u16();
    if status == 401 || status == 403 {
        return Err(Unreachable(Some(status)));
    }
    if !response.status().is_success() {
        return Ok(Listed::Unsupported);
    }
    let Ok(body) = serde_json::from_slice::<Value>(response.body()) else {
        return Ok(Listed::Unsupported);
    };
    // A provider that answers `200` with an error envelope (Z.AI's
    // `{"code":401,…}`) has no `data`: no list, not an empty one.
    let Some(rows) = body.get("data").and_then(Value::as_array) else {
        return Ok(Listed::Unsupported);
    };
    let mut models: Vec<curated::Model> = rows
        .iter()
        .filter_map(|row| {
            let id = row.get("id").and_then(Value::as_str)?;
            if id.is_empty() || id.len() > MAX_ID_LEN || id.chars().any(char::is_control) {
                return None;
            }
            let mut model = curated::Model::bare(id.to_owned());
            model.context = ["context_length", "context_window", "max_input_tokens"]
                .iter()
                .find_map(|key| row.get(*key).and_then(Value::as_u64));
            Some(model)
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    models.truncate(MAX_LISTED);
    Ok(Listed::Models(models))
}

/// A reply's text without the Markdown code fence a model may wrap JSON in.
fn unfenced(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    rest.strip_suffix("```").unwrap_or(rest).trim()
}

/// Sends a completion request and parses the answer. The first call that
/// fails (non-2xx or transport) makes the endpoint unreachable. The
/// transport error's own message is dropped rather than reported: it can
/// carry request details, and the key is in a header of this request.
async fn send_chat(
    http: &dyn HttpClient,
    target: Target<'_>,
    body: &Value,
) -> Result<Value, Unreachable> {
    let request = target.request(http::Method::POST, target.chat_path(), Some(body))?;
    let response = http.send(request).await.map_err(|_| Unreachable(None))?;
    if !response.status().is_success() {
        return Err(Unreachable(Some(response.status().as_u16())));
    }
    serde_json::from_slice(response.body()).map_err(|_| Unreachable(None))
}
