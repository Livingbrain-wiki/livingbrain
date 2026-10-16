//! The calls this module makes to Discord, over the `HttpClient` port, and
//! the pure rules that read their answers.
//!
//! Every call is a `GET` made with the bot token (`Authorization: Bot …`),
//! plus one URL that the install flow builds for the admin's browser. The
//! wire shapes below keep only what the import reads; everything else in a
//! Discord response is ignored, so a new field never breaks an import.
//!
//! **Rate limits.** Discord answers every call with `X-RateLimit-Remaining`
//! and `X-RateLimit-Reset-After` (fractional seconds) for its route bucket,
//! and a `429` carries `Retry-After` (seconds) and `X-RateLimit-Global`.
//! [`decide`] turns one answer into a [`Verdict`]; the drain stops making
//! calls for the rest of the tick on any [`Verdict::Wait`] and resumes on the
//! next firing. Nothing sleeps: a Worker invocation is wall-clock bounded.

use cratefield_core::HttpClient;
use cratefield_core::axum::body::Bytes;
use cratefield_core::axum::http::{self, Method};
use serde::Deserialize;

use crate::common::Secret;

/// The REST API, version 10.
pub(crate) const API: &str = "https://discord.com/api/v10";
/// The bot-install page. Only the admin's browser visits it.
pub(crate) const AUTHORIZE: &str = "https://discord.com/oauth2/authorize";
/// Sent with every call. Discord asks for an identifying agent.
const USER_AGENT: &str = "LivingBrain (https://livingbrain.wiki, 1.0)";
/// The longest wait a single answer may ask for, in seconds. A bigger value
/// is clamped: the next tick asks again anyway.
const MAX_WAIT_SECS: u64 = 15 * 60;
/// When a `429` names no wait at all.
const DEFAULT_WAIT_SECS: u64 = 5;

/// The URL the admin's browser is sent to in order to add the bot. The
/// `state` is the signed, single-use token; the `redirect_uri` is the one
/// registered on the Discord app.
#[must_use]
pub fn authorize_url(client_id: &str, redirect_uri: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE}?client_id={}&scope=bot&permissions={}&redirect_uri={}&response_type=code&state={}",
        encode(client_id),
        crate::config::PERMISSIONS,
        encode(redirect_uri),
        encode(state),
    )
}

/// Percent-encodes everything outside the RFC 3986 unreserved set.
pub(crate) fn encode(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// The limit headers of one answer.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rate {
    /// Requests left in this route's bucket.
    pub remaining: Option<u64>,
    /// Seconds until that bucket refills.
    pub reset_after: Option<f64>,
    /// Seconds to wait, on a `429`.
    pub retry_after: Option<f64>,
    /// Whether the `429` is for the whole application.
    pub global: bool,
}

impl Rate {
    /// Reads the limit headers, case-insensitively. Unparseable values are
    /// absent.
    #[must_use]
    pub fn from_headers(headers: &[(&str, &str)]) -> Self {
        let find = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.trim())
        };
        Self {
            remaining: find("x-ratelimit-remaining").and_then(|v| v.parse().ok()),
            reset_after: find("x-ratelimit-reset-after").and_then(|v| v.parse().ok()),
            retry_after: find("retry-after").and_then(|v| v.parse().ok()),
            global: find("x-ratelimit-global").is_some_and(|v| v.eq_ignore_ascii_case("true")),
        }
    }
}

/// What the drain does after one answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The answer is usable.
    Ok,
    /// Stop calling Discord for this tick; the next tick may go on.
    Wait { seconds: u64 },
    /// `404`: the channel, thread or server is gone.
    Missing,
    /// `401`/`403`: the bot may not read this. Not retried this tick.
    Denied,
    /// Any other status (a 5xx is treated as [`Verdict::Wait`]).
    Failed,
}

/// Decides from a status and the limit headers. A `429` always waits; a
/// `2xx` whose bucket is empty lets this answer through but says to wait
/// before the next call to that bucket.
#[must_use]
pub fn decide(status: u16, rate: Rate) -> Verdict {
    if status == 429 {
        let seconds = rate
            .retry_after
            .or(rate.reset_after)
            .map_or(DEFAULT_WAIT_SECS, ceil_secs);
        return Verdict::Wait {
            seconds: seconds.min(MAX_WAIT_SECS),
        };
    }
    match status {
        200..=299 => {
            if rate.remaining == Some(0) {
                let seconds = rate.reset_after.map_or(DEFAULT_WAIT_SECS, ceil_secs);
                Verdict::Wait {
                    seconds: seconds.min(MAX_WAIT_SECS),
                }
            } else {
                Verdict::Ok
            }
        }
        404 => Verdict::Missing,
        401 | 403 => Verdict::Denied,
        500..=599 => Verdict::Wait {
            seconds: DEFAULT_WAIT_SECS,
        },
        _ => Verdict::Failed,
    }
}

fn ceil_secs(value: f64) -> u64 {
    if value.is_finite() && value > 0.0 {
        // Clamped above before the cast; a wait is whole seconds.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let whole = value.ceil().min(MAX_WAIT_SECS as f64) as u64;
        whole.max(1)
    } else {
        1
    }
}

/// One answer, with what the drain reads from it. Never logged.
pub(crate) struct Reply {
    pub status: u16,
    pub rate: Rate,
    pub body: Bytes,
}

impl Reply {
    pub(crate) fn json<T: for<'de> Deserialize<'de>>(&self) -> Option<T> {
        serde_json::from_slice(&self.body).ok()
    }
}

/// The request never got an answer. The text is fixed: no adapter message
/// rides along into a stored error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Unreachable;

/// Makes one bot-authenticated `GET`. `path` starts with `/` and is relative
/// to [`API`].
pub(crate) async fn bot_get(
    http: &dyn HttpClient,
    token: &Secret,
    path: &str,
) -> Result<Reply, Unreachable> {
    let request = http::Request::builder()
        .method(Method::GET)
        .uri(format!("{API}{path}"))
        .header("authorization", format!("Bot {}", token.expose()))
        .header("user-agent", USER_AGENT)
        .body(Bytes::new())
        .map_err(|_| Unreachable)?;
    let response = http.send(request).await.map_err(|_| Unreachable)?;
    let status = response.status().as_u16();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            let name = name.as_str();
            let keep = matches!(
                name,
                "x-ratelimit-remaining"
                    | "x-ratelimit-reset-after"
                    | "retry-after"
                    | "x-ratelimit-global"
            );
            keep.then(|| Some((name.to_owned(), value.to_str().ok()?.to_owned())))
                .flatten()
        })
        .collect();
    let pairs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    Ok(Reply {
        status,
        rate: Rate::from_headers(&pairs),
        body: response.into_body(),
    })
}

/// `GET /guilds/{id}`: the server's name, which proves the bot is in it.
#[derive(Debug, Deserialize)]
pub(crate) struct Guild {
    pub id: String,
    pub name: String,
}

/// A channel or thread as `GET /guilds/{id}/channels` and the thread
/// listings report it. Only the fields the import reads.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Channel {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: u8,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub nsfw: Option<bool>,
    #[serde(default)]
    pub thread_metadata: Option<ThreadMeta>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ThreadMeta {
    #[serde(default)]
    pub archive_timestamp: Option<String>,
}

/// `GET /guilds/{id}/threads/active`.
#[derive(Debug, Deserialize)]
pub(crate) struct ActiveThreads {
    pub threads: Vec<Channel>,
}

/// `GET /channels/{id}/threads/archived/public`.
#[derive(Debug, Deserialize)]
pub(crate) struct ArchivedThreads {
    pub threads: Vec<Channel>,
    #[serde(default)]
    pub has_more: bool,
}

/// A message, as `GET /channels/{id}/messages` returns it.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Message {
    pub id: String,
    #[serde(rename = "type", default)]
    pub kind: u8,
    /// RFC 3339, UTC: `2026-10-03T14:02:11.123000+00:00`.
    pub timestamp: String,
    #[serde(default)]
    pub content: String,
    pub author: User,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub message_reference: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    pub referenced_message: Option<Referenced>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct User {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub global_name: Option<String>,
    #[serde(default)]
    pub bot: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Attachment {
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub url: String,
}

/// The author of the message a reply answers, when Discord includes it.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Referenced {
    pub author: User,
}

/// Message types the transcript keeps: a plain message, a reply and a slash
/// command. Joins, pins, boosts and thread notices are system messages and
/// are skipped.
pub(crate) const KEPT_TYPES: [u8; 3] = [0, 19, 20];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_install_url_asks_for_the_bot_and_exactly_two_permissions() {
        let url = authorize_url(
            "1234",
            "https://app.livingbrain.wiki/v1/discord/callback",
            "tok_en-1",
        );
        assert!(url.starts_with("https://discord.com/oauth2/authorize?"));
        assert!(url.contains("&scope=bot&"));
        assert!(url.contains("&permissions=66560&"), "{url}");
        assert!(
            url.contains(
                "redirect_uri=https%3A%2F%2Fapp.livingbrain.wiki%2Fv1%2Fdiscord%2Fcallback"
            )
        );
        assert!(url.ends_with("&state=tok_en-1"));
    }

    #[test]
    fn a_429_waits_for_retry_after_and_is_clamped() {
        let rate = Rate::from_headers(&[("Retry-After", "2.5")]);
        assert_eq!(decide(429, rate), Verdict::Wait { seconds: 3 });
        let huge = Rate::from_headers(&[("retry-after", "99999")]);
        assert_eq!(
            decide(429, huge),
            Verdict::Wait {
                seconds: MAX_WAIT_SECS
            }
        );
        assert_eq!(
            decide(429, Rate::default()),
            Verdict::Wait {
                seconds: DEFAULT_WAIT_SECS
            }
        );
    }

    #[test]
    fn an_empty_bucket_waits_for_its_reset_but_keeps_the_answer() {
        let empty = Rate::from_headers(&[
            ("x-ratelimit-remaining", "0"),
            ("x-ratelimit-reset-after", "1.2"),
        ]);
        assert_eq!(decide(200, empty), Verdict::Wait { seconds: 2 });
        let spare = Rate::from_headers(&[("x-ratelimit-remaining", "4")]);
        assert_eq!(decide(200, spare), Verdict::Ok);
    }

    #[test]
    fn statuses_map_to_verdicts() {
        assert_eq!(decide(404, Rate::default()), Verdict::Missing);
        assert_eq!(decide(403, Rate::default()), Verdict::Denied);
        assert_eq!(decide(401, Rate::default()), Verdict::Denied);
        assert_eq!(
            decide(502, Rate::default()),
            Verdict::Wait {
                seconds: DEFAULT_WAIT_SECS
            }
        );
        assert_eq!(decide(400, Rate::default()), Verdict::Failed);
    }
}
