//! The Slack Events API (issue #6): the app manifest, the signature check on
//! a delivery, and the work a verified delivery hands to `Defer`.
//!
//! Slack allows three seconds to acknowledge, so reading the body, claiming
//! its `event_id` in the inbox and deferring the rest all happen inside that
//! window and everything that touches a workspace row happens after the
//! response. `team_id` comes from the signed envelope, never from
//! `event.user` (ADR 0001), and no payload is ever logged: a message body is
//! whatever somebody typed in a channel.

use std::sync::Arc;

use cratefield_core::axum::Json;
use cratefield_core::axum::extract::State;
use cratefield_core::axum::http::{HeaderMap, StatusCode};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::{
    Clock, Database, Inbox, Problem, ProblemDef, SignatureScheme, SignedDelivery, WebhookVerifier,
};
use serde::Serialize;
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;

use crate::handlers::{ModuleState, database, port};
use crate::store;

/// The dedup ledger Slack deliveries are claimed in. The table is created by
/// migration `0003` from [`Inbox::create_table_sql`] verbatim, so the DDL the
/// module ships and the DDL it ships it as cannot drift.
pub(crate) fn inbox() -> Inbox {
    Inbox::new("workspaces_slack_inbox")
}

/// The deployment takes no Slack events: it has no signing secret. A 503
/// rather than a 401 — nothing is wrong with the delivery — under its own
/// slug, so an operator can tell which half of the app is unconfigured.
const EVENTS_NOT_CONFIGURED: ProblemDef = ProblemDef {
    slug: "workspaces/slack-events-not-configured",
    status: StatusCode::SERVICE_UNAVAILABLE,
    title: "The Slack Events API is not configured",
    description: "An operator sets WORKSPACES_SLACK_SIGNING_SECRET.",
};

/// The delivery did not carry a signature this app's signing secret produced,
/// or carried one from too long ago. One problem for every cause, with no
/// detail: telling a caller *which* check failed turns this endpoint into an
/// oracle for guessing the secret one header at a time.
const BAD_SIGNATURE: ProblemDef = ProblemDef {
    slug: "workspaces/slack-bad-signature",
    status: StatusCode::UNAUTHORIZED,
    title: "The Slack signature did not verify",
    description: "Slack signs each delivery with the app's signing secret.",
};

/// Slack's own signature scheme, as the harness's [`SignatureScheme`]:
/// `X-Slack-Signature` (`v0=<hex>`) and `X-Slack-Request-Timestamp` over the
/// bytes `v0:{timestamp}:{raw body}`. Written as a scheme rather than a
/// hand-rolled HMAC so the constant-time comparison and the replay tolerance
/// are the harness's, shared with every other signed webhook this venture
/// takes.
///
/// `None` for anything unreadable — a missing header, a signature that is not
/// `v0=`-prefixed, a hex string that does not decode, a timestamp that is not
/// an integer. It never falls back to "no timestamp", which would drop the
/// replay window silently.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Slack;

impl SignatureScheme for Slack {
    fn extract(&self, headers: &HeaderMap, body: &[u8]) -> Option<SignedDelivery> {
        let timestamp = header(headers, "x-slack-request-timestamp")?
            .trim()
            .to_owned();
        let sent = timestamp.parse::<i64>().ok()?;
        // Only the current version is understood. Slack has one, and a future
        // `v1=` is not something to guess at.
        let candidates: Vec<Vec<u8>> = header(headers, "x-slack-signature")?
            .trim()
            .strip_prefix("v0=")
            .and_then(hex_decode)
            .into_iter()
            .collect();
        Some(SignedDelivery {
            signed_payload: [
                b"v0:".as_slice(),
                timestamp.as_bytes(),
                b":".as_slice(),
                body,
            ]
            .concat(),
            candidates,
            timestamp: Some(sent),
        })
    }
}

/// One header's value as `&str`, or `None` if it is absent or not text.
fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// Even-length hex to bytes. `None` for anything that is not hex — a
/// signature header is not a place to be forgiving, because a forgiving
/// decoder is how a truncated signature comes to match a prefix of one.
fn hex_decode(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            let digits = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(digits, 16).ok()
        })
        .collect()
}

/// The harness's HMAC core with Slack's five-minute tolerance either way.
fn verifier() -> WebhookVerifier {
    WebhookVerifier::new(Slack)
}

/// `POST /slack/events` — verify, acknowledge, and defer.
///
/// The order is the whole contract: the signature runs over the raw bytes
/// before anything parses them (a parser that normalises whitespace signs
/// different bytes from the ones Slack signed), the `url_verification`
/// handshake is answered, and an `event_callback` has its `event_id` claimed
/// and its work handed to `Defer` before `200` goes out. Nothing here awaits
/// a workspace row.
pub(crate) async fn events(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
    body: cratefield_core::axum::body::Bytes,
) -> Result<Response, Problem> {
    // Read apart from `Settings`: a deployment that signs people in with
    // Slack OpenID and takes no events is correctly configured, and refusing
    // to build the router for it would take sign-in down too.
    let Some(secret) = state.signing_secret() else {
        return Err(Problem::new(&EVENTS_NOT_CONFIGURED));
    };
    let clock = port(state.ctx.ports.clock.clone())?;
    let db = port(state.ctx.ports.db.clone())?;
    let defer = port(state.ctx.ports.defer.clone())?;

    if !verifier().verify(&secret, &headers, &body, clock.now().unix_timestamp()) {
        return Err(Problem::new(&BAD_SIGNATURE));
    }
    let envelope: Value =
        serde_json::from_slice(&body).map_err(|_| Problem::new(&BAD_SIGNATURE))?;

    match envelope.get("type").and_then(Value::as_str) {
        // Only after the signature has verified: an unauthenticated caller
        // must not be able to make this endpoint echo a challenge and so
        // confirm it is wired to a live deployment.
        Some("url_verification") => {
            let challenge = envelope
                .get("challenge")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Ok(Json(Challenge { challenge }).into_response());
        }
        Some("event_callback") => {}
        // Slack sends other envelope types over time; acknowledging one keeps
        // it from retrying a delivery nobody asked for.
        _ => return Ok(StatusCode::OK.into_response()),
    }

    let Some(event_id) = envelope
        .get("event_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        // No id means no way to be idempotent on, so there is nothing safe to
        // do with it — but the answer is still `200`: an event we cannot
        // deduplicate is not a reason to make Slack retry it forever.
        return Ok(StatusCode::OK.into_response());
    };
    // The envelope's team, never the event's (ADR 0001). An org-wide app
    // sends it only under `authorizations[0]`, so both are read.
    let team_id = envelope_team_id(&envelope);
    let event = envelope.get("event").cloned().unwrap_or(Value::Null);

    // Slack retries anything it does not see acknowledged within three
    // seconds, and the same `event_id` arrives again on every retry. The claim
    // is a single `INSERT … ON CONFLICT DO NOTHING`, so under two concurrent
    // deliveries of one event exactly one caller is told `true` and only that
    // caller defers any work.
    let seen_at = clock.now().format(&Rfc3339).unwrap_or_default();
    if !inbox()
        .claim(&*db, event_id, &seen_at)
        .await
        .map_err(database)?
    {
        return Ok(StatusCode::OK.into_response());
    }

    defer.wait_until(Box::pin(dispatch(
        Arc::clone(&db),
        Arc::clone(&clock),
        team_id,
        event,
    )));
    Ok(StatusCode::OK.into_response())
}

/// The team the *signed envelope* names, or the empty string. A delivery with
/// no team is still deferred — `apply_user_change` refuses an empty team —
/// because deciding that here would mean reading the event, and the envelope
/// is the only half of the delivery the signature check vouched for.
fn envelope_team_id(envelope: &Value) -> String {
    envelope
        .get("team_id")
        .or_else(|| {
            envelope
                .get("authorizations")
                .and_then(Value::as_array)
                .and_then(|entries| entries.first())
                .and_then(|entry| entry.get("team_id"))
        })
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The body of a `url_verification` handshake: Slack compares this string to
/// the one it sent, and that is the whole response.
#[derive(Debug, Serialize)]
struct Challenge<'a> {
    challenge: &'a str,
}

/// What this module does with a verified event. `user_change` is the one
/// already ours: it refreshes the member mirror, the seam ADR 0001 left.
#[derive(Debug, Clone, Copy)]
enum Event {
    UserChange,
    AppMention,
    Message,
    MemberJoinedChannel,
}

/// The event this delivery carries, or `None` for one this app does not act
/// on. The names are exactly the manifest's `bot_events`, minus the three
/// `message.*` channels a `Message` covers: Slack also sends one `message`
/// per subtype, and the manifest subscribes to the channel kinds.
fn recognised(event: &Value) -> Option<Event> {
    match event.get("type").and_then(Value::as_str)? {
        "user_change" => Some(Event::UserChange),
        "app_mention" => Some(Event::AppMention),
        "member_joined_channel" => Some(Event::MemberJoinedChannel),
        "message" => matches!(
            event.get("channel_type").and_then(Value::as_str),
            Some("channel" | "group" | "im")
        )
        .then_some(Event::Message),
        _ => None,
    }
}

/// The work a verified delivery earns — run after the response is returned.
/// Errors are swallowed deliberately: nothing awaits this future, so a `?`
/// here would drop the rest of the batch, and Slack has already been told
/// `200`. The inbox claim is what keeps the next delivery from doing it twice.
async fn dispatch(db: Arc<dyn Database>, clock: Arc<dyn Clock>, team_id: String, event: Value) {
    // Everything else is the attachment point for issue #7's agent loop, and
    // deliberately nothing yet: the conversation events are recognised,
    // claimed, deduplicated and deferred, and there is no agent to answer a
    // mention and no queue to enqueue on until it lands.
    if let Some(Event::UserChange) = recognised(&event) {
        let _ = store::apply_user_change(&*db, &*clock, &team_id, &event).await;
    }
}

// ---------------------------------------------------------------------------
// The app manifest

/// The Slack app manifest for this deployment. Slack's "create an app from a
/// manifest" form takes this as pasted JSON, so generating it here rather
/// than asking an operator to keep it in step with the deployment is the only
/// way the URLs in it stay true — and a manifest whose `request_url` names
/// an origin the deployment does not serve is the commonest way an Events API
/// app silently stops working.
#[must_use]
pub(crate) fn manifest(public_base: &str) -> Value {
    let base = public_base.trim_end_matches('/');
    json!({
        "display_information": {
            "name": "Livingbrain",
        },
        "features": {
            "bot_user": {
                "display_name": "livingbrain",
                "always_online": true,
            },
        },
        "oauth_config": {
            "scopes": {
                "bot": bot_scopes(),
                // The OpenID Connect scopes the sign-in flow already
                // requests, so one manifest installs both halves of the app.
                "user": ["openid", "profile", "email"],
            },
            "redirect_urls": [
                install_callback_url(base),
                format!("{base}/v1/workspaces/slack/callback"),
            ],
        },
        "settings": {
            "event_subscriptions": {
                "request_url": format!("{base}/v1/workspaces/slack/events"),
                "bot_events": [
                    "app_mention",
                    "message.channels",
                    "message.groups",
                    "message.im",
                    "member_joined_channel",
                    "user_change",
                ],
            },
            // One workspace per install, so an org-wide app that would
            // deliver every workspace's events to one deployment is off;
            // socket mode would need a long-lived connection this Worker
            // has nowhere to keep; and token rotation is a per-workspace
            // refetch this module does not do, so claiming it would promise
            // a refresh nobody performs.
            "org_deploy_enabled": false,
            "socket_mode_enabled": false,
            "token_rotation_enabled": false,
        },
    })
}

/// The redirect URI the install flow registers and the token call carries,
/// and the one the manifest lists. One function, so the three cannot drift.
#[must_use]
pub(crate) fn install_callback_url(public_base: &str) -> String {
    format!(
        "{}/v1/workspaces/slack/install/callback",
        public_base.trim_end_matches('/')
    )
}

/// The bot scopes the install flow requests and the manifest declares. One
/// list, so an app installed from one and serving the other is not refused at
/// install time, hours later, with a Slack-side error.
#[must_use]
pub fn bot_scopes() -> Vec<&'static str> {
    vec![
        "app_mentions:read",
        "channels:history",
        "groups:history",
        "im:history",
        "channels:read",
        "groups:read",
        "im:read",
        "users:read",
        "chat:write",
        "reactions:write",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use cratefield_core::axum::http::HeaderValue;

    /// The signing secret every test signs with.
    const SECRET: &str = "8f742231b10e8888abcd99yyyzzz85a5";
    /// The instant the verifier is asked about.
    const NOW: i64 = 1_800_000_000;
    /// The delivery both tests verify.
    const BODY: &str = r#"{"type":"event_callback"}"#;

    /// The signature Slack would send for [`BODY`] at this timestamp.
    fn signature(timestamp: i64) -> String {
        use hmac::Mac as _;
        let mut mac = <hmac::Hmac<sha2::Sha256> as hmac::Mac>::new_from_slice(SECRET.as_bytes())
            .expect("HMAC accepts a key of any length");
        mac.update(format!("v0:{timestamp}:{BODY}").as_bytes());
        let digest = mac.finalize().into_bytes();
        format!(
            "v0={}",
            digest
                .iter()
                .fold(String::with_capacity(64), |mut out, byte| {
                    use std::fmt::Write as _;
                    let _ = write!(out, "{byte:02x}");
                    out
                })
        )
    }

    /// The two headers of a delivery, spelled out rather than computed.
    fn headers(timestamp: &str, signature: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-slack-request-timestamp",
            HeaderValue::from_str(timestamp).expect("a timestamp header"),
        );
        headers.insert(
            "x-slack-signature",
            HeaderValue::from_str(signature).expect("a signature header"),
        );
        headers
    }

    /// The two headers of a delivery Slack signed at `timestamp`.
    fn signed(timestamp: i64) -> HeaderMap {
        headers(&timestamp.to_string(), &signature(timestamp))
    }

    fn verifies(headers: &HeaderMap, now: i64) -> bool {
        verifier().verify(SECRET, headers, BODY.as_bytes(), now)
    }

    #[test]
    fn the_replay_window_is_five_minutes_in_either_direction() {
        for sent in [NOW - 300, NOW + 300] {
            assert!(
                verifies(&signed(sent), NOW),
                "a delivery sent at {sent} is at the edge of the window"
            );
        }
        for sent in [NOW - 301, NOW + 301] {
            assert!(
                !verifies(&signed(sent), NOW),
                "a delivery sent at {sent} is outside the window"
            );
        }
    }

    #[test]
    fn anything_that_is_not_a_readable_signature_refuses() {
        // A well-formed delivery verifies, so what the rest refuse is their
        // shape rather than the body.
        assert!(verifies(&signed(NOW), NOW));

        let mut no_signature = signed(NOW);
        no_signature.remove("x-slack-signature");
        let mut no_timestamp = signed(NOW);
        no_timestamp.remove("x-slack-request-timestamp");
        for headers in [
            HeaderMap::new(),
            no_signature,
            no_timestamp,
            // Not hex at all, half a signature, an empty one, a version this
            // app does not speak, and a timestamp that is not an integer.
            headers(&NOW.to_string(), "not-hex-at-all"),
            headers(&NOW.to_string(), "v0=abc"),
            headers(&NOW.to_string(), "v0="),
            headers(&NOW.to_string(), "v1=00ff"),
            headers("not-a-number", &signature(NOW)),
        ] {
            assert!(!verifies(&headers, NOW), "{headers:?} must refuse");
        }
    }
}
