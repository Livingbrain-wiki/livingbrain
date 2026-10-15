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
//!   never reaches a return value, an error, or a log line — an error names
//!   an HTTP status at most, never a provider's body, which can echo it.
//! - **The SSRF guard** runs again on every call, over the stored base URL:
//!   a host that resolved to a public address at connect time may not any
//!   more, and the guard is cheap next to a completion.
//! - **The cost ledger** (issue #12) records every call that reported
//!   usage, under the workspace, exactly like the connect probe does. The
//!   usage is also handed back, so a caller with its own budget can count
//!   it without a second read.
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
use crate::{crypto, selection, ssrf, store};

/// The `anthropic-version` every Messages request names.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The largest completion body a job call reads back. A JSON edit list for
/// a handful of wiki pages fits well inside it; anything past it is a
/// provider misbehaving, and the call fails rather than buffering it.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

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
    /// How long one call may take.
    pub timeout: Duration,
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

/// Why a call did not happen, or did not answer.
///
/// No variant carries a provider's body or the key: an HTTP status is the
/// most a caller learns.
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
    /// The provider answered with a non-2xx status, or the transport
    /// failed (`None`).
    Http(Option<u16>),
    /// The provider answered 2xx with something that is not a completion.
    BadResponse,
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
            Self::Http(_) => "http",
            Self::BadResponse => "bad-response",
            Self::Store => "store",
            Self::Runner(_) => "runner",
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
            Self::BadResponse => write!(f, "the model endpoint answered with no completion"),
            Self::Store => write!(f, "the model connections could not be read"),
            Self::Runner(reason) => write!(f, "your runner could not answer ({reason})"),
        }
    }
}

impl std::error::Error for InvokeError {}

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

    let mut body = request_body(wire, &connection.model, request);
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
    let mut builder = http::Request::builder()
        .method(http::Method::POST)
        .uri(target.as_str())
        .header(http::header::CONTENT_TYPE, "application/json")
        .extension(HttpPolicy {
            max_response_bytes: MAX_RESPONSE_BYTES,
            timeout: request.timeout,
        });
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
    drop(key);
    // The transport error's own message is dropped: it can carry request
    // details, and the key is in a header of this request.
    let response = http
        .send(outgoing)
        .await
        .map_err(|_| InvokeError::Http(None))?;
    if !response.status().is_success() {
        return Err(InvokeError::Http(Some(response.status().as_u16())));
    }
    let parsed: Value =
        serde_json::from_slice(response.body()).map_err(|_| InvokeError::BadResponse)?;
    let text = completion_text(&parsed, wire).ok_or(InvokeError::BadResponse)?;
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
    Ok(ChatReply {
        text,
        prompt_tokens,
        completion_tokens,
        usage_reported: usage.is_some(),
        role: connection.role,
        provider: connection.provider,
        model: connection.model,
        runner: false,
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

/// The request body for one wire.
fn request_body(wire: Wire, model: &str, request: &ChatRequest<'_>) -> Value {
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
            if request.json {
                body["response_format"] = json!({"type": "json_object"});
            }
            body
        }
        Wire::Anthropic => json!({
            "model": model,
            "system": request.system,
            "messages": [{"role": "user", "content": request.user}],
            "max_tokens": request.max_tokens,
        }),
    }
}

/// The completion's text, per wire; `None` when the answer carries none.
fn completion_text(parsed: &Value, wire: Wire) -> Option<String> {
    match wire {
        // `content` only: a reasoning model's `reasoning_content` is its
        // thinking, not its answer, and an empty `content` (the budget went
        // on thinking) is no completion at all.
        Wire::Openai => parsed
            .get("choices")?
            .get(0)?
            .get("message")?
            .get("content")?
            .as_str()
            .filter(|text| !text.trim().is_empty())
            .map(str::to_owned),
        // Every `text` block, in order: `thinking` and `redacted_thinking`
        // blocks (a GLM answer starts with one) are skipped.
        Wire::Anthropic => {
            let blocks = parsed.get("content")?.as_array()?;
            let text: String = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            Some(text).filter(|text| !text.trim().is_empty())
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
            timeout: Duration::from_secs(1),
        };
        let body = request_body(Wire::Openai, "m", &request);
        assert_eq!(body["response_format"]["type"], "json_object");
        assert_eq!(body["messages"][0]["role"], "system");
        let plain = ChatRequest {
            json: false,
            ..request
        };
        assert!(
            request_body(Wire::Openai, "m", &plain)
                .get("response_format")
                .is_none()
        );
    }

    #[test]
    fn the_anthropic_body_carries_the_system_prompt_apart() {
        let request = ChatRequest {
            system: "rules",
            user: "data",
            max_tokens: 10,
            json: true,
            timeout: Duration::from_secs(1),
        };
        let body = request_body(Wire::Anthropic, "m", &request);
        assert_eq!(body["system"], "rules");
        assert_eq!(body["messages"][0]["content"], "data");
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
