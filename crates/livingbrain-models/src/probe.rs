//! The capability probe: on connect, send a few small requests to the
//! model endpoint and report what works.
//!
//! The probe sends at most three requests:
//! 1. Tool calling — a trivial tool with `tool_choice: "required"`.
//! 2. JSON output — `response_format: {"type":"json_object"}`.
//! 3. Context size — `GET {base_url}/models/{model}`.
//!
//! `base_url` is the *checked* [`Url`], not the string a member sent: the
//! host the guard approved is the host the probe connects to, and the path
//! segments below are appended to it rather than concatenated onto a raw
//! string, so neither can disagree about where the request is going. The
//! model name is pushed as one percent-encoded path segment for the same
//! reason.
//!
//! If the very first call fails (non-2xx or transport error), the endpoint
//! is unreachable: the connection is refused with a 422 that names the
//! HTTP status only — never the provider's response body, which can echo
//! the key. The probe keeps `max_tokens` small and sets a tight
//! `HttpPolicy` so a slow or chatty endpoint cannot cost much.

use std::time::Duration;

use bytes::Bytes;
use cratefield_core::axum::http;
use cratefield_core::{HttpClient, HttpPolicy};
use serde_json::Value;
use url::Url;

/// The maximum response body for a probe reply (well under the port ceiling).
const PROBE_MAX_RESPONSE: usize = 64 * 1024;
/// The timeout for a probe request.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// The `max_tokens` for a probe completion: small, because we only need to
/// see whether the model calls a tool or emits JSON.
const PROBE_MAX_TOKENS: u32 = 16;

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
}

/// The endpoint was unreachable: the first call returned a non-2xx status
/// or a transport error. The HTTP status is named; the body is never echoed.
#[derive(Debug)]
pub(crate) struct Unreachable(pub(crate) Option<u16>);

/// `base`, with `segments` appended as percent-encoded path segments.
fn endpoint(base: &Url, segments: &[&str]) -> Option<Url> {
    let mut url = base.clone();
    {
        let mut path = url.path_segments_mut().ok()?;
        // `https://host` normalises to a path of "/", whose only segment is
        // empty; dropping it is what makes `https://host` and
        // `https://host/v1` both append cleanly.
        path.pop_if_empty();
        for segment in segments {
            path.push(segment);
        }
    }
    Some(url)
}

/// Probes the model endpoint at `base` with `model` and `api_key`, where
/// `base` is the URL the SSRF guard approved. Returns a [`ProbeResult`] on
/// success, or [`Unreachable`] if the first call fails.
///
/// # Errors
///
/// [`Unreachable`] when the first basic call fails (non-2xx or transport).
pub(crate) async fn probe(
    http: &dyn HttpClient,
    base: &Url,
    model: &str,
    api_key: &str,
) -> Result<ProbeResult, Unreachable> {
    let completions = endpoint(base, &["chat", "completions"]).ok_or(Unreachable(None))?;

    // (a) Tool calling.
    let tool_ok = tool_calling_probe(http, &completions, model, api_key).await?;
    // (b) JSON output.
    let json_ok = json_output_probe(http, &completions, model, api_key).await?;
    // (c) Context size.
    let context_size = context_size_probe(http, base, model, api_key).await?;

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
    })
}

/// Sends a trivial tool with `tool_choice: "required"`. Pass if the model
/// returns at least one tool call.
async fn tool_calling_probe(
    http: &dyn HttpClient,
    url: &Url,
    model: &str,
    api_key: &str,
) -> Result<bool, Unreachable> {
    let body = serde_json::json!({
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
    });
    let response = send_chat(http, url, api_key, &body).await?;
    let parsed: Value = serde_json::from_slice(response.body()).map_err(|_| Unreachable(None))?;
    let tool_calls = parsed
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array);
    Ok(tool_calls.is_some_and(|calls| !calls.is_empty()))
}

/// Sends a request with `response_format: {"type":"json_object"}`. Pass if
/// the content parses as a JSON object.
async fn json_output_probe(
    http: &dyn HttpClient,
    url: &Url,
    model: &str,
    api_key: &str,
) -> Result<bool, Unreachable> {
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "Return {\"ok\":true}"}],
        "response_format": {"type": "json_object"},
        "max_tokens": PROBE_MAX_TOKENS,
    });
    let response = send_chat(http, url, api_key, &body).await?;
    let parsed: Value = serde_json::from_slice(response.body()).map_err(|_| Unreachable(None))?;
    let content = parsed
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(serde_json::from_str::<Value>(content).is_ok_and(|v| v.is_object()))
}

/// GETs `{base}/models/{model}` and reads the context length. Returns
/// `None` if the endpoint does not report one (unknown, not a failure).
async fn context_size_probe(
    http: &dyn HttpClient,
    base: &Url,
    model: &str,
    api_key: &str,
) -> Result<Option<String>, Unreachable> {
    let url = endpoint(base, &["models", model]).ok_or(Unreachable(None))?;
    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(url.as_str())
        .header(http::header::AUTHORIZATION, format!("Bearer {api_key}"))
        .extension(HttpPolicy {
            max_response_bytes: PROBE_MAX_RESPONSE,
            timeout: PROBE_TIMEOUT,
        })
        .body(Bytes::new())
        .map_err(|_| Unreachable(None))?;
    let response = http.send(request).await.map_err(|_| Unreachable(None))?;
    if !response.status().is_success() {
        // Context size is best-effort: a failure here is "unknown", not
        // unreachable (the chat probes already proved the endpoint works).
        return Ok(None);
    }
    let body: Value = serde_json::from_slice(response.body()).map_err(|_| Unreachable(None))?;
    // Some endpoints nest the model data under "data"; others return it
    // directly. Check both.
    let data = body.get("data").unwrap_or(&body);
    for key in &[
        "context_length",
        "context_window",
        "max_model_len",
        "max_context_length",
    ] {
        // A number or a string, both of which name a size; the four keys
        // are the spellings providers actually use for it.
        if let Some(value) = data.get(*key)
            && (value.is_i64() || value.is_u64() || value.is_string())
        {
            return Ok(Some(value.to_string()));
        }
    }
    Ok(None)
}

/// Sends a chat-completion request. The first call that fails (non-2xx or
/// transport) makes the endpoint unreachable. The transport error's own
/// message is dropped rather than reported: it can carry request details,
/// and the key is in a header of this request.
async fn send_chat(
    http: &dyn HttpClient,
    url: &Url,
    api_key: &str,
    body: &Value,
) -> Result<http::Response<Bytes>, Unreachable> {
    let request = http::Request::builder()
        .method(http::Method::POST)
        .uri(url.as_str())
        .header(http::header::AUTHORIZATION, format!("Bearer {api_key}"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .extension(HttpPolicy {
            max_response_bytes: PROBE_MAX_RESPONSE,
            timeout: PROBE_TIMEOUT,
        })
        .body(Bytes::from(body.to_string()))
        .map_err(|_| Unreachable(None))?;
    let response = http.send(request).await.map_err(|_| Unreachable(None))?;
    if !response.status().is_success() {
        return Err(Unreachable(Some(response.status().as_u16())));
    }
    Ok(response)
}
