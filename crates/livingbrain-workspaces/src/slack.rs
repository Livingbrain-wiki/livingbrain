//! The Slack half of sign-in: the authorize URL, the token exchange, and
//! what the `id_token` is allowed to say.
//!
//! `cratefield-auth-oidc` cannot do this yet (see the ADR): its provider
//! list is static, its callback keeps only subject/email/name, and
//! `auth-core`'s `identities.provider` CHECK refuses `slack`. So the code
//! flow is owned here, over ports only.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use cratefield_core::HttpClient;
use cratefield_core::axum::http;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

/// Where Slack sends the browser to approve the sign-in.
const AUTHORIZE_URL: &str = "https://slack.com/openid/connect/authorize";
/// Where the code is exchanged for an `id_token`.
const TOKEN_URL: &str = "https://slack.com/api/openid.connect.token";
/// The only issuer this module accepts.
const ISSUER: &str = "https://slack.com";
/// The scopes requested. `openid` is what makes it OIDC; `profile` is what
/// carries the name; `email` is asked for because a Slack workspace's
/// people are usually identified by address elsewhere in the product.
///
/// These three are all Slack's OpenID Connect has: its discovery document
/// (`https://slack.com/.well-known/openid-configuration`) advertises
/// `"scopes_supported": ["openid","profile","email"]`, and the `id_token`
/// claims documented for `openid.connect.token` are identity and team/user
/// identifiers — there is no admin flag and no `entitlements` claim. So an
/// admin of a team cannot be told apart from an ordinary member of it out
/// of a token from this endpoint, which is why the linking route has a
/// hole: see `handlers::link_slack_workspace`. Widening this constant will
/// not close it; a second Slack app that can ask Slack who the admins are
/// can.
const SCOPE: &str = "openid profile email";

/// The claim naming the workspace.
const CLAIM_TEAM_ID: &str = "https://slack.com/team_id";
/// The claim naming the person within the workspace.
const CLAIM_USER_ID: &str = "https://slack.com/user_id";
/// The claim naming the workspace in human words.
const CLAIM_TEAM_NAME: &str = "https://slack.com/team_name";

/// Everything outside RFC 3986's unreserved set, so a value cannot break
/// out of the slot it is encoded into.
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// A sign-in step that did not produce a usable identity.
///
/// The detail is `Some` only for a Slack `error` code that survived
/// [`SlackError::refused`]'s whitelist. Upstream transport text, a parse
/// failure and a rejected token all carry `None`: none of it is the
/// client's business and some of it is not ours to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlackError(pub Option<String>);

impl SlackError {
    /// Nothing worth telling the caller.
    pub(crate) fn opaque() -> Self {
        Self(None)
    }

    /// A failure of this deployment rather than of Slack: the key ring could
    /// not wrap the token, say. The detail is the KMS error's own text, which
    /// names the secret and the fix and never the token.
    pub(crate) fn internal(detail: String) -> Self {
        Self(Some(detail))
    }

    /// A Slack refusal, keeping its `error` code only when it is a plain
    /// snake_case token — which is what Slack's own codes are, and is not
    /// true of anything else that might reach this field.
    pub(crate) fn refused(code: &str) -> Self {
        let plain = !code.is_empty()
            && code.len() <= 64
            && code
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_');
        Self(plain.then(|| format!("Slack error: {code}")))
    }
}

/// What a verified `id_token` says, narrowed to what this module stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlackIdentity {
    /// The workspace (Slack team) id.
    pub team_id: String,
    /// The person's Slack user id.
    pub user_id: String,
    /// The workspace's name, empty if Slack omitted it.
    pub team_name: String,
    /// The person's display name, empty if Slack omitted it.
    pub name: String,
}

/// The URL to redirect a browser to, to begin sign-in.
///
/// Every parameter is percent-encoded, so a `redirect_uri` carrying its
/// own query or a state value carrying `&` cannot break out of its slot.
pub(crate) fn authorize_url(
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    nonce: &str,
) -> String {
    format!(
        "{AUTHORIZE_URL}?response_type=code&scope={}&client_id={}&redirect_uri={}&state={}&nonce={}",
        encode(SCOPE),
        encode(client_id),
        encode(redirect_uri),
        encode(state),
        encode(nonce),
    )
}

/// Exchanges an authorization code for an `id_token`.
pub(crate) async fn exchange_code(
    http: &dyn HttpClient,
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
    code: &str,
) -> Result<String, SlackError> {
    let body = post_form(
        http,
        TOKEN_URL,
        code_form(client_id, client_secret, redirect_uri, code),
    )
    .await?;
    body.get("id_token")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .ok_or_else(SlackError::opaque)
}

/// The form both token endpoints take: the app's credentials, the code, and
/// the redirect URI it was issued against.
fn code_form(client_id: &str, client_secret: &str, redirect_uri: &str, code: &str) -> String {
    format!(
        "client_id={}&client_secret={}&code={}&redirect_uri={}",
        encode(client_id),
        encode(client_secret),
        encode(code),
        encode(redirect_uri),
    )
}

/// A form POST to one of Slack's token endpoints, returning only after
/// `ok: true`.
async fn post_form(
    http: &dyn HttpClient,
    uri: &str,
    form: String,
) -> Result<serde_json::Value, SlackError> {
    let request = http::Request::builder()
        .method(http::Method::POST)
        .uri(uri)
        .header(
            http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(Bytes::from(form))
        .map_err(|_| SlackError::opaque())?;
    let response = http.send(request).await.map_err(|_| SlackError::opaque())?;

    let body: serde_json::Value =
        serde_json::from_slice(response.body()).map_err(|_| SlackError::opaque())?;
    if body.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(SlackError::refused(
            body.get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default(),
        ));
    }
    Ok(body)
}

/// Where a workspace's owner sends a browser to install the app (OAuth v2).
const INSTALL_URL: &str = "https://slack.com/oauth/v2/authorize";
/// Where the install's authorization code becomes a bot token.
const INSTALL_TOKEN_URL: &str = "https://slack.com/api/oauth.v2.access";

/// What Slack's install endpoint says about the workspace that installed the
/// app, narrowed to what [`install_code`]'s caller stores.
///
/// The token is a `BotToken` and not a `String` so that it cannot reach a
/// log through a `Debug`: every error path here is a log line, and a
/// `Debug`-printed `String` in a struct is exactly how a token ends up in
/// one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlackInstall {
    /// The team that installed the app.
    pub team_id: String,
    /// The app's own id, which a re-install keeps.
    pub app_id: String,
    /// The bot user Slack created for this installation.
    pub bot_user_id: String,
    /// The `xoxb-` bot token, sealed by the caller and never printed.
    pub token: BotToken,
}

/// A Slack bot token. The `Debug` impl prints `[redacted]`, so the one
/// derived trait that reaches logs and error messages cannot print it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct BotToken(String);

impl BotToken {
    /// The token's own bytes, for the seal that stores it. Named so that a
    /// reader has to notice.
    #[must_use]
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for BotToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BotToken([redacted])")
    }
}

/// The URL a workspace's owner is sent to in order to install the app.
///
/// `scopes` is comma-joined: OAuth v2 scopes one kind of token at a time, and
/// asking for a user scope here would be asking Slack for a user token this
/// module has no use for.
pub(crate) fn install_url(
    client_id: &str,
    redirect_uri: &str,
    scopes: &[&str],
    state: &str,
) -> String {
    format!(
        "{INSTALL_URL}?client_id={}&scope={}&redirect_uri={}&state={}",
        encode(client_id),
        encode(&scopes.join(",")),
        encode(redirect_uri),
        encode(state),
    )
}

/// Exchanges an install code for the bot token, the team that installed the
/// app and the bot user Slack made for it.
///
/// The same form POST as [`exchange_code`], to a different endpoint: Slack's
/// install endpoint is OAuth v2 and answers `access_token` where the OpenID
/// one answers `id_token`.
pub(crate) async fn install_code(
    http: &dyn HttpClient,
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
    code: &str,
) -> Result<SlackInstall, SlackError> {
    let body = post_form(
        http,
        INSTALL_TOKEN_URL,
        code_form(client_id, client_secret, redirect_uri, code),
    )
    .await?;
    let token = body
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(SlackError::opaque)?;
    let team_id = body
        .pointer("/team/id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(SlackError::opaque)?;
    Ok(SlackInstall {
        team_id: team_id.to_owned(),
        app_id: text(body.get("app_id")),
        bot_user_id: text(body.get("bot_user_id")),
        token: BotToken(token.to_owned()),
    })
}

/// An optional string field, empty when absent.
fn text(value: Option<&serde_json::Value>) -> String {
    value
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Checks an `id_token` and reads the Slack claims out of it.
///
/// The signature is **not** verified, and that is deliberate: the token
/// came back from Slack's own token endpoint, over TLS, in answer to a
/// request this server authenticated with its client secret — OIDC Core
/// §3.1.3.7 item 6 calls that a sufficient alternative to checking the
/// signature, and verifying it would mean fetching and rotating Slack's
/// JWKS for a guarantee already held. Everything else *is* checked: the
/// issuer, the audience, the expiry, and the nonce this server minted.
pub(crate) fn verify_id_token(
    id_token: &str,
    client_id: &str,
    nonce: &str,
    now: i64,
) -> Result<SlackIdentity, SlackError> {
    let mut parts = id_token.split('.');
    let (Some(_header), Some(payload), Some(_signature)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return Err(SlackError::opaque());
    };
    if parts.next().is_some() {
        return Err(SlackError::opaque());
    }

    let raw = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| SlackError::opaque())?;
    let claims: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|_| SlackError::opaque())?;

    if claims.get("iss").and_then(serde_json::Value::as_str) != Some(ISSUER) {
        return Err(SlackError::opaque());
    }
    // OIDC allows `aud` to be a string or an array of strings; the token is
    // ours if our client id is one of them.
    if !audience(&claims, client_id) {
        return Err(SlackError::opaque());
    }
    let exp = claims
        .get("exp")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or_default();
    if exp <= now {
        return Err(SlackError::opaque());
    }
    if claims.get("nonce").and_then(serde_json::Value::as_str) != Some(nonce) {
        return Err(SlackError::opaque());
    }

    Ok(SlackIdentity {
        team_id: claim(&claims, CLAIM_TEAM_ID)?,
        user_id: claim(&claims, CLAIM_USER_ID)?,
        team_name: text_claim(&claims, CLAIM_TEAM_NAME),
        name: text_claim(&claims, "name"),
    })
}

/// Whether the token's `aud` names this client, as a string or as an array
/// of strings.
fn audience(claims: &serde_json::Value, client_id: &str) -> bool {
    match claims.get("aud") {
        Some(serde_json::Value::String(value)) => value == client_id,
        Some(serde_json::Value::Array(values)) => {
            values.iter().any(|value| value.as_str() == Some(client_id))
        }
        _ => false,
    }
}

/// A required string claim.
fn claim(claims: &serde_json::Value, name: &str) -> Result<String, SlackError> {
    claims
        .get(name)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(SlackError::opaque)
}

/// An optional string claim, empty when absent.
fn text_claim(claims: &serde_json::Value, name: &str) -> String {
    claims
        .get(name)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Percent-encodes everything outside RFC 3986's unreserved set, which is
/// what a query-string value and a form field both want.
fn encode(value: &str) -> String {
    utf8_percent_encode(value, UNRESERVED).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_encodes_every_parameter() {
        let url = authorize_url(
            "1234.5678",
            "https://brain.example/v1/workspaces/slack/callback",
            "state&x",
            "nonce",
        );
        assert!(url.contains("scope=openid%20profile%20email"));
        assert!(url.contains("client_id=1234.5678"));
        assert!(url.contains(
            "redirect_uri=https%3A%2F%2Fbrain.example%2Fv1%2Fworkspaces%2Fslack%2Fcallback"
        ));
        assert!(url.contains("state=state%26x"));
        assert!(url.ends_with("&nonce=nonce"));
    }

    #[test]
    fn audience_accepts_a_string_or_an_array() {
        let as_string = serde_json::json!({"aud": "1234.5678"});
        assert!(audience(&as_string, "1234.5678"));
        assert!(!audience(&as_string, "9999.9999"));

        let as_array = serde_json::json!({"aud": ["9.9", "1234.5678"]});
        assert!(audience(&as_array, "1234.5678"));
        assert!(!audience(&as_array, "9999.9999"));

        let absent = serde_json::json!({});
        assert!(!audience(&absent, "1234.5678"));
    }

    #[test]
    fn only_plain_snake_case_slack_codes_reach_a_problem_detail() {
        assert_eq!(
            SlackError::refused("invalid_code").0.as_deref(),
            Some("Slack error: invalid_code")
        );
        for hostile in ["", "Invalid Code", "<script>", "a/b", "code\nheader"] {
            assert_eq!(SlackError::refused(hostile), SlackError(None), "{hostile}");
        }
        assert_eq!(SlackError::opaque(), SlackError(None));
    }

    #[test]
    fn a_bot_token_never_prints_itself() {
        // Assembled from fragments so the fixture's bot token never appears
        // verbatim in the source tree, where scanners read it as a live one.
        let token = BotToken(concat!("xo", "xb-1111-2222-secret").to_owned());
        assert_eq!(format!("{token:?}"), "BotToken([redacted])");
        assert!(!format!("{token:?}").contains(concat!("xo", "xb")));
        assert_eq!(token.expose(), concat!("xo", "xb-1111-2222-secret"));
    }
}
