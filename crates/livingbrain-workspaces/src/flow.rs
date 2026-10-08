//! The two cookies this module signs, and the header helpers around them.
//!
//! Both are the same shape as `cratefield-auth-oidc`'s flow cookie: the
//! payload is JSON in the [`Signer`] payload's `subject`, the expiry lives
//! *inside* the JSON and is checked against the [`Clock`] port, and the
//! signer's own `exp` is left `None` — so a test clock drives expiry and
//! the cookie carries no server state.
//!
//! Both names start with `__Host-`, which a browser accepts only with
//! `Secure`, `Path=/` and no `Domain`; the header builders below set
//! exactly that.
//!
//! ADR 0002 made the session's workspace id ours rather than a Slack team
//! id, and gave the flow cookie an optional workspace: a cookie with one is
//! a *link* (the caller is already signed in and is binding a team to their
//! workspace), a cookie without one is a *sign in*.

use cratefield_core::axum::http::{HeaderMap, HeaderValue, header};
use cratefield_core::{Clock, Kid, Payload, Signer};
use serde::{Deserialize, Serialize};

/// The sign-in flow cookie, set by `/slack/start` and cleared by
/// `/slack/callback`.
pub const FLOW_COOKIE: &str = "__Host-lb_flow";
/// The session cookie, set by `/slack/callback`.
pub const SESSION_COOKIE: &str = "__Host-lb_session";
/// The signing purpose of the flow cookie. Public so a test can decode one
/// it just received; a purpose is not a secret, the key is.
pub const FLOW_PURPOSE: &str = "workspaces.flow";
/// The signing purpose of the session cookie.
pub const SESSION_PURPOSE: &str = "workspaces.session";
/// The signing purpose of the Slack install flow's cookie.
///
/// Its own purpose, not [`FLOW_PURPOSE`], because the two round trips
/// protect different things: a sign-in decides *who is signed in*, an install
/// decides *which workspace a bot token is bound to*.
pub const INSTALL_PURPOSE: &str = "workspaces.install";

/// How long a started sign-in stays valid, in seconds.
pub(crate) const FLOW_TTL_SECS: i64 = 600;
/// How long a sign-in link can be spent, in seconds — fifteen minutes, the
/// TTL ADR 0002 names for a magic link. Long enough to read a mail and
/// click, short enough that a link forwarded by accident stops working.
pub(crate) const SIGN_IN_TTL_SECS: i64 = 900;
/// How long a session lasts, in seconds — seven days.
pub(crate) const SESSION_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// The signed payload of the flow cookie: the CSRF state echoed through
/// Slack, the OIDC nonce bound into the `id_token`, and the expiry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Flow {
    /// Matched against the `state` query parameter, in constant time.
    pub state: String,
    /// Matched against the `id_token`'s `nonce` claim.
    pub nonce: String,
    /// Unix seconds; the cookie is refused at or after this instant.
    pub expires_at: i64,
    /// The workspace the caller was signed in to when the flow was started,
    /// which is what tells a *link* apart from a *sign in* (ADR 0002).
    ///
    /// It rides in the cookie rather than in the request because it is
    /// only trustworthy if the server put it there: a caller cannot ask to
    /// link a team to somebody else's workspace by editing a parameter.
    /// `None` on a `/slack/start` cookie, which is a sign in and resolves
    /// its workspace from the `id_token` instead.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// The member the caller was signed in as, the row a linked Slack
    /// user id is bound to. `None` on a sign-in flow, for the same reason.
    #[serde(default)]
    pub user_id: Option<String>,
}

/// The signed payload of the session cookie. It names *who* is signed in
/// and nothing more: every handler resolves the workspace from these two
/// ids, and never from request input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Session {
    /// The workspace — opaque and ours to mint since ADR 0002, not a Slack
    /// team id. The alias reads a cookie sealed before this change, whose
    /// payload named the Slack team id the workspace was created from;
    /// those rows keep that id, so the old name still resolves.
    #[serde(alias = "team_id")]
    pub workspace_id: String,
    /// The member within that workspace — a Slack user id for a member
    /// first seen through Slack, a `usr_` id for one first seen by email.
    pub user_id: String,
    /// Unix seconds; the session is refused at or after this instant.
    pub exp: i64,
}

/// Signs `value` as a cookie value under `purpose`.
///
/// `None` only when the payload cannot be serialized, which for these two
/// structs cannot happen; the callers answer it with a 500 rather than a
/// panic.
pub(crate) fn seal<T: Serialize>(signer: &dyn Signer, purpose: &str, value: &T) -> Option<String> {
    let subject = serde_json::to_string(value).ok()?;
    Some(signer.sign(&Payload {
        purpose: purpose.to_owned(),
        subject,
        // Deliberately none: expiry rides inside the payload so it is
        // checked against the Clock port, not the signer's wall clock.
        exp: None,
        kid: Kid::Cur,
    }))
}

/// Verifies and decodes a flow cookie.
///
/// `None` for a missing signature, a tampered or truncated value, the
/// wrong purpose, JSON that is not a [`Flow`], or one already expired.
pub(crate) fn open_flow(signer: &dyn Signer, clock: &dyn Clock, cookie: &str) -> Option<Flow> {
    let payload = signer.verify(cookie, FLOW_PURPOSE)?;
    let flow: Flow = serde_json::from_str(&payload.subject).ok()?;
    if flow.expires_at <= clock.now().unix_timestamp() {
        return None;
    }
    Some(flow)
}

/// Verifies and decodes an install flow cookie: the same payload as a sign-in
/// flow, sealed under [`INSTALL_PURPOSE`].
pub(crate) fn open_install(signer: &dyn Signer, clock: &dyn Clock, cookie: &str) -> Option<Flow> {
    let payload = signer.verify(cookie, INSTALL_PURPOSE)?;
    let flow: Flow = serde_json::from_str(&payload.subject).ok()?;
    if flow.expires_at <= clock.now().unix_timestamp() {
        return None;
    }
    Some(flow)
}

/// Verifies and decodes a session cookie. The same refusals as
/// [`open_flow`], over a [`Session`].
pub(crate) fn open_session(
    signer: &dyn Signer,
    clock: &dyn Clock,
    cookie: &str,
) -> Option<Session> {
    let payload = signer.verify(cookie, SESSION_PURPOSE)?;
    let session: Session = serde_json::from_str(&payload.subject).ok()?;
    if session.exp <= clock.now().unix_timestamp() {
        return None;
    }
    Some(session)
}

/// The `Set-Cookie` header that sets one of the module's cookies.
///
/// Every cookie this module writes goes through here, so `__Host-`'s
/// required attributes (`Secure`, `Path=/`, no `Domain`) are stated once.
/// The value is a freshly signed token — base64url and a dot — so
/// [`HeaderValue::from_str`] cannot refuse it; `None` is defensive.
pub(crate) fn set_cookie(name: &str, value: &str, max_age_secs: i64) -> Option<HeaderValue> {
    HeaderValue::from_str(&format!(
        "{name}={value}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}"
    ))
    .ok()
}

/// The `Set-Cookie` header that drops one of the module's cookies.
pub(crate) fn clear_cookie(name: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(&format!(
        "{name}=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0; \
         Expires=Thu, 01 Jan 1970 00:00:00 GMT"
    ))
    .ok()
}

/// Reads one of the module's cookies out of a request's `Cookie` header.
///
/// Every `Cookie` header is searched, and the name must be followed by
/// `=` — so a cookie named `__Host-lb_session_extra` cannot stand in for
/// `__Host-lb_session`.
pub(crate) fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    for header in headers.get_all(header::COOKIE) {
        let Ok(raw) = header.to_str() else { continue };
        for pair in raw.split(';') {
            let Some(rest) = pair.trim().strip_prefix(name) else {
                continue;
            };
            let Some(value) = rest.strip_prefix('=') else {
                continue;
            };
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}
