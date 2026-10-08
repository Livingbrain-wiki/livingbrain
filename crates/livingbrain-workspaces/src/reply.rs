//! Sending one [`Outbound`] to a chat platform's Web API.
//!
//! Slack answers `{"ok": false, "error": "…"}` with **HTTP 200**, so the
//! status line alone says nothing about whether a message was posted, and the
//! body has to be read. The error string is never returned verbatim: Slack's
//! own codes are plain snake_case, and [`slack::SlackError::refused`] is what
//! decides whether one may travel — the same rule the sign-in half of this
//! module already follows, for the same reason.
//!
//! The bot token reaches the header and nothing else. It is a
//! `BotToken`, whose `Debug` prints `[redacted]`, so no error path here can
//! leak it through a log.

use bytes::Bytes;
use cratefield_core::HttpClient;
use cratefield_core::axum::http;
use livingbrain_channel::Outbound;

use crate::slack::SlackError;

/// Slack's Web API root. Every `Outbound::method` hangs off it.
const API_BASE: &str = "https://slack.com/api";

/// Posts `out` with `token`, and fails unless Slack says it did.
///
/// # Errors
///
/// [`SlackError`] when Slack refused, or when the transport, the URI or the
/// body could not be produced. Never with a token in it.
pub(crate) async fn post(
    http: &dyn HttpClient,
    token: &str,
    out: &Outbound,
) -> Result<(), SlackError> {
    let uri = format!("{API_BASE}/{}", out.method);
    let request = http::Request::builder()
        .method(http::Method::POST)
        .uri(&uri)
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Bytes::from(out.body.to_string()))
        .map_err(|_| SlackError::opaque())?;
    let response = http.send(request).await.map_err(|_| SlackError::opaque())?;

    // Slack's `ok` is the real answer, whatever the status line says.
    let body: serde_json::Value =
        serde_json::from_slice(response.body()).map_err(|_| SlackError::opaque())?;
    if body.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(SlackError::refused(
            body.get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default(),
        ));
    }
    Ok(())
}
