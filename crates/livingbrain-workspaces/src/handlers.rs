//! The module's routes, mounted at `/v1/workspaces`.
//!
//! Two are the Slack sign-in flow (`/slack/start` and `/slack/callback`),
//! two read the session back (`/me` and `/members`), one links a Slack team
//! to a workspace the caller is already in (`/connections/slack/start`),
//! three are the email magic link (`/email/start`, and `/email/verify`
//! as a form to render and a form to submit), and one ends the session
//! (`/signout`).
//!
//! No route takes a workspace id from the request and acts on it. The
//! workspace is whatever the verified session cookie names, what the sealed
//! flow cookie carries, or what a *spent* sign-in token resolves to — never
//! the caller's input. A workspace id the caller may name (the
//! `workspace_id` on `/email/start`) only decides which member may be signed
//! in; it never grants one.

use std::sync::Arc;

use cratefield_core::axum::extract::{Query, State};
use cratefield_core::axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use cratefield_core::axum::response::{Html, IntoResponse, Response};
use cratefield_core::axum::routing::{get, post};
use cratefield_core::axum::{self, Json};
use cratefield_core::{
    Clock, Database, DbError, IdGen, MailError, Message, ModuleContext, Problem, ProblemDef,
    SendOutcome, Signer, constant_time_eq, invalid_email_problem, normalize_email, origin_of,
};
use cratefield_kms::Kms;
use livingbrain_pages::Answers;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;

use crate::config::{self, Settings};
use crate::conversation::{LocalTurns, Turns};
use crate::events;
use crate::events::events as slack_events;
use crate::flow::{self, Flow, Session};
use crate::install;
use crate::slack;
use crate::store::{self, LinkOutcome, Member, Workspace};

/// A cookie-carrying write a browser reports as coming from another site.
/// The same refusal `livingbrain-tokens` answers its cookie writes with.
const CROSS_SITE_REQUEST: ProblemDef = ProblemDef {
    slug: "workspaces/cross-site-request",
    status: StatusCode::FORBIDDEN,
    title: "This request came from another site",
    description: "Signing out is only accepted from this site's own pages.",
};

/// The Slack app has not been configured. A 503, not a 500: nothing is
/// broken, the deployment has not been given the credentials, and the
/// detail names which.
const NOT_CONFIGURED: ProblemDef = ProblemDef {
    slug: "workspaces/not-configured",
    status: StatusCode::SERVICE_UNAVAILABLE,
    title: "Slack sign-in is not configured",
    description: "An operator sets WORKSPACES_SLACK_CLIENT_ID, \
                  WORKSPACES_SLACK_CLIENT_SECRET and WORKSPACES_REDIRECT_BASE.",
};

/// The callback could not be matched to a live attempt: no flow cookie, an
/// expired or tampered one, or a state that is not the one this browser
/// was sent away with.
const SIGN_IN_EXPIRED: ProblemDef = ProblemDef {
    slug: "workspaces/sign-in-expired",
    status: StatusCode::BAD_REQUEST,
    title: "The sign-in attempt expired",
    description: "Start again from /v1/workspaces/slack/start.",
};

/// Slack would not complete the exchange, or answered with an `id_token`
/// that fails a check (issuer, audience, expiry, nonce). A 502: the failure
/// is upstream, not in the request.
const SIGN_IN_FAILED: ProblemDef = ProblemDef {
    slug: "workspaces/sign-in-failed",
    status: StatusCode::BAD_GATEWAY,
    title: "Slack sign-in failed",
    description: "Start again from /v1/workspaces/slack/start.",
};

/// A read with no usable session: no cookie, one that does not verify or
/// has expired, or a verified session naming someone who is not a member of
/// that workspace.
const NO_SESSION: ProblemDef = ProblemDef {
    slug: "workspaces/no-session",
    status: StatusCode::UNAUTHORIZED,
    title: "Sign in with Slack",
    description: "Sign in first at /v1/workspaces/slack/start.",
};

/// A verified session that is not allowed to do this. Distinct from
/// [`NO_SESSION`] so a client can tell "sign in" from "sign in as somebody
/// else".
const NOT_ALLOWED: ProblemDef = ProblemDef {
    slug: "workspaces/not-allowed",
    status: StatusCode::FORBIDDEN,
    title: "Only the workspace owner can do this",
    description: "Ask the owner of the workspace to start it.",
};

/// A Slack team, Discord guild or email address that is already linked to
/// another workspace. The existing link is left exactly as it was; a
/// deployment resolves a clash by unlinking the other side deliberately,
/// never by this route overwriting it.
const LINK_CONFLICT: ProblemDef = ProblemDef {
    slug: "workspaces/link-conflict",
    status: StatusCode::CONFLICT,
    title: "That account is already linked to another workspace",
    description: "Unlink it there first, then start again.",
};

/// No `Mailer` port is wired, or the one that is has no provider key: a
/// deployment fault, not a bad request, and the same 503 Cratefield's other
/// mail-sending modules answer with.
const MAIL_NOT_CONFIGURED: ProblemDef = ProblemDef {
    slug: "workspaces/mail-not-configured",
    status: StatusCode::SERVICE_UNAVAILABLE,
    title: "Email sign-in is not configured",
    description: "An operator wires a Mailer port and sets WORKSPACES_MAIL_FROM.",
};

/// The mail provider refused or could not deliver the sign-in link. A 502:
/// the failure is upstream, and the caller still gets the same 202 body the
/// route always answers with, because whether a mail went out says nothing
/// about the address and everything to a spammer.
const MAIL_FAILED: ProblemDef = ProblemDef {
    slug: "workspaces/mail-failed",
    status: StatusCode::BAD_GATEWAY,
    title: "The sign-in mail could not be sent",
    description: "Try again in a minute.",
};

/// A sign-in link for an address that is not a member identity of the
/// workspace it was minted for. A 400, and deliberately the same shape as
/// an expired link: a caller who did not receive the mail learns nothing
/// about who has accounts here.
const NOT_A_MEMBER: ProblemDef = ProblemDef {
    slug: "workspaces/not-a-member",
    status: StatusCode::BAD_REQUEST,
    title: "That address cannot sign in to this workspace",
    description: "Ask somebody who is already a member to send a link.",
};

/// The key custodian is absent, so no bot token can be sealed or opened. A
/// 503 for the same reason the Slack credentials are one: the deployment is
/// missing what these routes need, and nothing about the request is wrong.
const NO_KEY_CUSTODIAN: ProblemDef = ProblemDef {
    slug: "workspaces/no-key-custodian",
    status: StatusCode::SERVICE_UNAVAILABLE,
    title: "Slack install is not configured",
    description: "An operator sets HARNESS_KEK_CURRENT and HARNESS_KEK_V1.",
};

/// The install callback could not be matched to an attempt this server
/// started. The same refusal as a sign-in whose flow cookie is gone: an
/// install is a CSRF-protected round trip exactly as a sign-in is.
const INSTALL_EXPIRED: ProblemDef = ProblemDef {
    slug: "workspaces/install-expired",
    status: StatusCode::BAD_REQUEST,
    title: "The Slack install attempt expired",
    description: "Start again from /v1/workspaces/slack/install.",
};

/// Slack would not complete the install, or the key ring would not seal the
/// token. A 502: the failure is upstream, not in the request.
const INSTALL_FAILED: ProblemDef = ProblemDef {
    slug: "workspaces/install-failed",
    status: StatusCode::BAD_GATEWAY,
    title: "The Slack install failed",
    description: "Start again from /v1/workspaces/slack/install.",
};

/// The router state: the module's ports, and the Slack settings as a
/// `Result` so a missing credential is a 503 per request, not a panic at
/// startup. `pub(crate)` because [`crate::events`] serves the Events API out
/// of the same state — the ports and the deployment's Slack configuration are
/// one thing, and a second copy of it is how the two halves of a Slack app
/// end up reading different secrets.
pub(crate) struct ModuleState {
    pub(crate) ctx: ModuleContext,
    settings: Result<Settings, String>,
    /// Where a sign-in link points. Read apart from [`Settings`] so a
    /// deployment with no Slack app can still send one, and resolved once
    /// because it cannot change under a running router.
    public_base: Option<String>,
    /// The `From` of a sign-in mail.
    mail_from: String,
    /// The Slack app's Events API signing secret, read lazily: a deployment
    /// that signs people in with Slack OpenID and takes no events is
    /// correctly configured without it.
    signing_secret: Option<String>,
    /// The key custodian the bot token is sealed under; `None` is a
    /// deployment with no key ring, which still serves sign-in and email.
    pub(crate) kms: Option<Arc<dyn Kms>>,
    /// The seam the Slack agent answers through (issue #123). `None` is a
    /// composition that wired no agent: the events still arrive, are
    /// deduplicated and are answered by nobody.
    pub(crate) answers: Option<Arc<dyn Answers>>,
    /// The seam the agent takes its turn from (issue #7). Absent at build,
    /// the per-isolate coordinator is the answer — one isolate serialises
    /// itself, which is all a composition without the venture's Durable
    /// Object can promise.
    pub(crate) turns: Arc<dyn Turns>,
}

impl ModuleState {
    pub(crate) fn settings(&self) -> Result<&Settings, Problem> {
        self.settings
            .as_ref()
            .map_err(|reason| Problem::new(&NOT_CONFIGURED).with_detail(reason.clone()))
    }

    /// The origin a sign-in link is built from, or the 503 that says the
    /// deployment has not said where it lives. A relative link in a mail is
    /// a dead link, so this is refused rather than guessed.
    fn public_base(&self) -> Result<&str, Problem> {
        self.public_base.as_deref().ok_or_else(|| {
            Problem::new(&NOT_CONFIGURED).with_detail(
                "an operator sets WORKSPACES_REDIRECT_BASE to the public origin \
                 sign-in links point at",
            )
        })
    }

    /// The Events API signing secret, or [`None`] when this deployment takes
    /// no Slack events.
    pub(crate) fn signing_secret(&self) -> Option<String> {
        self.signing_secret.clone()
    }

    /// The key custodian, or the 503 that says a bot token cannot be sealed.
    fn kms(&self) -> Result<&Arc<dyn Kms>, Problem> {
        self.kms
            .as_ref()
            .ok_or_else(|| Problem::new(&NO_KEY_CUSTODIAN))
    }
}

/// The module's routes.
pub(crate) fn router(
    ctx: ModuleContext,
    answers: Option<Arc<dyn Answers>>,
    turns: Option<Arc<dyn Turns>>,
) -> axum::Router {
    let settings = Settings::from_config(&*ctx.config).map_err(|err| err.to_string());
    let public_base = config::public_base(&*ctx.config);
    let mail_from = config::mail_from(&*ctx.config);
    let signing_secret = config::signing_secret(&*ctx.config);
    let kms = install::key_custodian(&*ctx.config).ok();
    let state = Arc::new(ModuleState {
        ctx,
        settings,
        public_base,
        mail_from,
        signing_secret,
        kms,
        answers,
        turns: turns.unwrap_or_else(|| Arc::new(LocalTurns::default())),
    });
    axum::Router::new()
        .route("/slack/start", get(slack_start))
        .route("/slack/callback", get(slack_callback))
        .route("/connections/slack/start", get(slack_link_start))
        .route("/slack/install", get(slack_install))
        .route("/slack/install/callback", get(slack_install_callback))
        .route("/slack/manifest", get(slack_manifest))
        .route("/slack/events", post(slack_events))
        .route("/me", get(me))
        .route("/members", get(members))
        .route("/signout", post(signout))
        .route("/email/start", post(email_start))
        .route("/email/verify", get(email_verify_form).post(email_verify))
        .with_state(state)
}

/// A port the module declared and the runtime did not supply: a deployment
/// fault, answered with nothing more than a 500.
pub(crate) fn port<T: ?Sized>(slot: Option<Arc<T>>) -> Result<Arc<T>, Problem> {
    slot.ok_or_else(Problem::internal)
}

/// A failed statement. The adapter's message never reaches the body.
pub(crate) fn database(_error: DbError) -> Problem {
    Problem::internal()
}

/// A sign-in step that failed upstream. Only the Slack `error` code the
/// module kept survives into the detail — never transport text, and never
/// the token.
fn sign_in_failed(error: slack::SlackError) -> Problem {
    let problem = Problem::new(&SIGN_IN_FAILED);
    match error.0 {
        Some(detail) => problem.with_detail(detail),
        None => problem,
    }
}

fn no_session() -> Problem {
    Problem::new(&NO_SESSION)
}

fn expired() -> Problem {
    Problem::new(&SIGN_IN_EXPIRED)
}

fn link_conflict() -> Problem {
    Problem::new(&LINK_CONFLICT)
}

fn not_allowed() -> Problem {
    Problem::new(&NOT_ALLOWED)
}

/// A workspace id we mint: `ws_` plus an IdGen id, which is what ADR 0002
/// makes the id — opaque and ours, never a Slack team id.
fn new_workspace_id(id_gen: &dyn IdGen) -> String {
    format!("ws_{}", id_gen.ulid())
}

/// A member id for somebody first seen through their email address. A
/// member first seen through Slack keeps the Slack user id as their member
/// id, so `0001`'s rows and issue #6's `apply_user_change` seam are
/// unchanged.
fn new_user_id(id_gen: &dyn IdGen) -> String {
    format!("usr_{}", id_gen.ulid())
}

/// `GET /slack/start` — mint the CSRF state and OIDC nonce, seal them into
/// the flow cookie, and send the browser to Slack.
///
/// A sign in: the flow cookie carries no workspace, so the callback resolves
/// one from the `id_token`. The linking route mints the same cookie with a
/// workspace in it.
async fn slack_start(State(state): State<Arc<ModuleState>>) -> Result<Response, Problem> {
    let settings = state.settings()?;
    slack_redirect(&state, settings, SlackFlow::SignIn, None, None).await
}

/// `GET /connections/slack/start` — the same Slack OAuth flow, for a caller
/// who is already signed in and wants to *link* this Slack team to their
/// workspace.
///
/// The workspace and member go into the sealed flow cookie, never into the
/// authorize URL's parameters: a query parameter is a value the caller could
/// have edited, and this one decides which workspace a team is bound to.
/// The session is checked first, so an unauthenticated caller is a 401 and
/// never reaches Slack, and the owner rule next, so a member who is not the
/// owner is a 403 and never reaches Slack either: only the owner may link a
/// team that is not linked yet, and there is nothing here a member could do
/// with a redirect to Slack. The callback keeps the same check, because a
/// flow cookie minted before this rule lived is still a valid one.
///
/// What is *not* checked, here or in the callback, is the caller's
/// authority over the Slack team: Slack's OpenID Connect `id_token` cannot
/// say who is an admin of it, so there is nothing here to check against.
/// See the note on [`link_slack_workspace`].
async fn slack_link_start(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let (workspace, member, is_owner) = session_scope(&state, &headers).await?;
    if !is_owner {
        return Err(not_allowed());
    }
    let settings = state.settings()?;
    slack_redirect(
        &state,
        settings,
        SlackFlow::SignIn,
        Some(workspace.id),
        Some(member.user_id),
    )
    .await
}

/// Which Slack round trip a flow cookie is for. They are the same shape — a
/// CSRF state in a sealed cookie and a redirect to Slack — and differ only in
/// what comes back and in the purpose that seals the cookie, so a cookie
/// minted for one cannot verify as the other.
#[derive(Debug, Clone, Copy)]
enum SlackFlow {
    /// An OpenID Connect sign-in.
    SignIn,
    /// An OAuth v2 bot install (issue #6).
    Install,
}

/// Mints the flow cookie and answers with Slack's authorize redirect.
/// `workspace`/`user` are set only for a link, and a link's callback
/// resolves a workspace from the cookie rather than creating one.
async fn slack_redirect(
    state: &ModuleState,
    settings: &Settings,
    flow: SlackFlow,
    workspace: Option<String>,
    user: Option<String>,
) -> Result<Response, Problem> {
    let signer = port(state.ctx.ports.signer.clone())?;
    let clock = port(state.ctx.ports.clock.clone())?;
    let id_gen = port(state.ctx.ports.id_gen.clone())?;

    // Two ULIDs each: a ULID carries 80 random bits, so one is thin for a
    // CSRF state, and this costs nothing.
    let state_value = secret(&*id_gen);
    let nonce = secret(&*id_gen);
    let expires_at = clock.now().unix_timestamp() + flow::FLOW_TTL_SECS;
    let sealed = flow::seal(
        &*signer,
        match flow {
            SlackFlow::SignIn => flow::FLOW_PURPOSE,
            SlackFlow::Install => flow::INSTALL_PURPOSE,
        },
        &Flow {
            state: state_value.clone(),
            nonce: nonce.clone(),
            expires_at,
            workspace_id: workspace,
            user_id: user,
        },
    )
    .ok_or_else(Problem::internal)?;

    let url = match flow {
        SlackFlow::SignIn => slack::authorize_url(
            &settings.client_id,
            &settings.callback_url(),
            &state_value,
            &nonce,
        ),
        SlackFlow::Install => slack::install_url(
            &settings.client_id,
            &events::install_callback_url(state.public_base()?),
            &events::bot_scopes(),
            &state_value,
        ),
    };
    Ok((
        StatusCode::FOUND,
        [
            (
                header::SET_COOKIE,
                flow::set_cookie(flow::FLOW_COOKIE, &sealed, flow::FLOW_TTL_SECS)
                    .ok_or_else(Problem::internal)?,
            ),
            (
                header::LOCATION,
                HeaderValue::from_str(&url).map_err(|_| Problem::internal())?,
            ),
        ],
    )
        .into_response())
}

/// 160 bits of randomness for a value an attacker must not guess: the CSRF
/// state, the OIDC nonce, and the sign-in token (which is this hashed down
/// to 32 bytes, so the same 160 bits as these two).
fn secret(id_gen: &dyn IdGen) -> String {
    format!("{}{}", id_gen.ulid(), id_gen.ulid())
}

/// 32 random bytes as 64 hex characters, hex encoded so the token is safe
/// in a URL and in a query string without escaping.
///
/// The `IdGen` port mints ULIDs and nothing else, so this is the 160 bits of
/// [`secret`] run through SHA-256: a 32-byte token with the same entropy as
/// the state and nonce, in the 64 characters ADR 0002 asks for. Only the
/// token's SHA-256 is ever stored — see [`token_hash`] — so nothing anybody
/// can read out of `sign_in_links` is a value they could present.
fn sign_in_token(id_gen: &dyn IdGen) -> String {
    hex(&Sha256::digest(secret(id_gen).as_bytes()))
}

/// The hex encoding of `bytes`, one `write!` per byte. A `hex` crate for
/// sixty-four characters would be a dependency to audit for no gain.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            // Writing into a `String` cannot fail.
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// The SHA-256 of a sign-in token, hex encoded: what `sign_in_links` stores
/// in place of the token. A read of that table therefore yields nothing
/// anybody can present.
fn token_hash(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
}

/// `GET /slack/callback` — verify the flow cookie, exchange the code and
/// check the `id_token`, then either *link* the Slack team to the
/// workspace the flow cookie named, or *sign in* by resolving that team
/// through `workspace_connections` and creating a workspace if the team has
/// none.
async fn slack_callback(
    State(state): State<Arc<ModuleState>>,
    Query(query): Query<CallbackQuery>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let settings = state.settings()?;
    let signer = port(state.ctx.ports.signer.clone())?;
    let clock = port(state.ctx.ports.clock.clone())?;
    let http = port(state.ctx.ports.http.clone())?;
    let db = port(state.ctx.ports.db.clone())?;
    let id_gen = port(state.ctx.ports.id_gen.clone())?;

    let Some(cookie) = flow::cookie_value(&headers, flow::FLOW_COOKIE) else {
        return Err(expired());
    };
    let Some(flow_cookie) = flow::open_flow(&*signer, &*clock, &cookie) else {
        return Err(expired());
    };
    let (Some(code), Some(returned_state)) = (query.code.as_deref(), query.state.as_deref()) else {
        return Err(expired());
    };
    // Constant time, so the state cannot be guessed a byte at a time.
    if !constant_time_eq(flow_cookie.state.as_bytes(), returned_state.as_bytes()) {
        return Err(expired());
    }

    let id_token = slack::exchange_code(
        &*http,
        &settings.client_id,
        &settings.client_secret,
        &settings.callback_url(),
        code,
    )
    .await
    .map_err(sign_in_failed)?;
    let identity = slack::verify_id_token(
        &id_token,
        &settings.client_id,
        &flow_cookie.nonce,
        clock.now().unix_timestamp(),
    )
    .map_err(sign_in_failed)?;

    let now = clock.now().format(&Rfc3339).unwrap_or_default();
    // The team id is read out of the `id_token` this server just verified,
    // never from the request, so a caller cannot name the team that gets
    // linked.
    let (workspace_id, member_id) = match (
        flow_cookie.workspace_id.as_deref(),
        flow_cookie.user_id.as_deref(),
    ) {
        (Some(workspace_id), Some(user_id)) => {
            link_slack_workspace(&*db, workspace_id, user_id, &identity, &now).await?
        }
        // A flow cookie with half a link is not one this module wrote; a
        // sign-in cookie with no workspace is the ordinary case.
        (None, None) => sign_in_slack_workspace(&*db, &*id_gen, &identity, &now).await?,
        _ => return Err(expired()),
    };

    // Name only: the `id_token` carries no timezone and no admin flag.
    store::upsert_member(
        &*db,
        &workspace_id,
        &member_id,
        store::MemberFields {
            name: Some(&identity.name),
            ..store::MemberFields::default()
        },
        &now,
    )
    .await
    .map_err(database)?;

    let session_value = flow::seal(
        &*signer,
        flow::SESSION_PURPOSE,
        &Session {
            workspace_id,
            user_id: member_id,
            exp: clock.now().unix_timestamp() + flow::SESSION_TTL_SECS,
        },
    )
    .ok_or_else(Problem::internal)?;

    // Back to the root, never to a caller-supplied URL: a redirect target
    // read from the request is an open redirect.
    let mut response = (StatusCode::FOUND, [(header::LOCATION, "/")]).into_response();
    for cookie in [
        flow::clear_cookie(flow::FLOW_COOKIE),
        flow::set_cookie(flow::SESSION_COOKIE, &session_value, flow::SESSION_TTL_SECS),
    ] {
        response
            .headers_mut()
            .append(header::SET_COOKIE, cookie.ok_or_else(Problem::internal)?);
    }
    Ok(response)
}

/// The *link* half of the callback: the flow cookie already named a
/// workspace and a member, so the verified team id is bound to them rather
/// than to a workspace minted here.
///
/// A team that is already linked to this workspace is a no-op success —
/// re-linking what is linked is the state the caller asked for. A team
/// linked to a *different* workspace is a 409 and changes nothing. A team
/// linked to nothing may only be linked by the owner of this workspace
/// (ADR 0002): a member cannot attach a team to a workspace they do not
/// own, which would make any workspace claim any team its members are in.
/// `slack_link_start` refuses a member before the round trip, but a flow
/// cookie sealed before that check existed still arrives here, so the rule
/// is enforced again rather than assumed.
// SECURITY HOLE, NOT CLOSED — read before adding a second sign-in method.
///
/// The only authority this checks is the caller's authority over *their own*
/// workspace. **Nothing checks that the caller has any authority over the
/// Slack team being linked.** The attack: a person who owns a workspace here
/// but is only an ordinary member of a public Slack team nobody has linked
/// starts `/connections/slack/start`, approves OpenID as that ordinary
/// member, and the callback binds that team to their workspace. Every member
/// of the team who later signs in with Slack lands in the attacker's
/// workspace, and there is no unlink route, so it is permanent. This is what
/// an `is_team_admin` check on `slack::SlackIdentity`, gating this call
// except for the `AlreadyThisWorkspace` no-op, would close.
///
/// It is not closed because Slack's OpenID Connect cannot say who is an
/// admin, and a check that reads a claim Slack never sends is not a check:
///
/// - `https://slack.com/.well-known/openid-configuration` advertises
///   `"scopes_supported": ["openid","profile","email"]` and no admin scope,
///   so `slack::SCOPE` cannot be widened to one on this endpoint;
/// - the `id_token` claims Slack documents for `openid.connect.token` are
///   `iss`, `sub`, `aud`, `exp`, `iat`, `auth_time`, `nonce`, `at_hash`,
///   `email`, `email_verified`, `date_email_verified`, `locale`, `name`,
///   `given_name`, `family_name` and the `https://slack.com/` team, user
///   and image claims. There is no `admin` flag and no `entitlements`
///   claim, so `SlackIdentity` cannot surface one.
///
/// What closes it, when it is worth building: Slack's *legacy* OAuth app,
/// which can request the `admin` scope (granted only to a workspace's
/// admins and owners) and answer `admin.users.list` with the team's admins;
/// or a Slack app that is *installed by* a team admin, where
/// `oauth.v2.access`'s `team.id` plus the installer establishes the
/// authority. Either is a second Slack app and a second callback, not a
/// change to this one — so until then this route links a team on the
/// caller's word alone, and ADR 0002 should say so.
async fn link_slack_workspace(
    db: &dyn cratefield_core::Database,
    workspace_id: &str,
    user_id: &str,
    identity: &slack::SlackIdentity,
    now: &str,
) -> Result<(String, String), Problem> {
    let linked = store::workspace_for_platform(db, "slack", &identity.team_id)
        .await
        .map_err(database)?;
    if let Some(owner) = linked.as_deref() {
        if owner != workspace_id {
            return Err(link_conflict());
        }
        // Already this workspace's: fall through to the identity link, which
        // is a no-op when it is already there too.
    } else {
        let workspace = store::workspace(db, workspace_id)
            .await
            .map_err(database)?
            .ok_or_else(no_session)?;
        if workspace.owner_id != user_id {
            return Err(not_allowed());
        }
        match store::link_connection(db, workspace_id, "slack", &identity.team_id, now)
            .await
            .map_err(database)?
        {
            LinkOutcome::Conflict => return Err(link_conflict()),
            LinkOutcome::Linked | LinkOutcome::AlreadyThisWorkspace => {}
        }
    }
    // The caller's own Slack user id is bound to the member they were
    // signed in as, so signing in with Slack later lands on the same row.
    store::link_identity(db, workspace_id, "slack", &identity.user_id, user_id, now)
        .await
        .map_err(database)?;
    Ok((workspace_id.to_owned(), user_id.to_owned()))
}

/// The *sign-in* half of the callback: resolve the verified team id
/// through `workspace_connections`, and mint a workspace only when the team
/// has none — which, on a database created by `0002`, is a team that has
/// genuinely never been seen.
async fn sign_in_slack_workspace(
    db: &dyn cratefield_core::Database,
    id_gen: &dyn IdGen,
    identity: &slack::SlackIdentity,
    now: &str,
) -> Result<(String, String), Problem> {
    let workspace_id = match store::workspace_for_platform(db, "slack", &identity.team_id)
        .await
        .map_err(database)?
    {
        Some(workspace_id) => workspace_id,
        None => {
            let workspace_id = new_workspace_id(id_gen);
            // Insert-if-absent, owner this signer: the first person through
            // here owns the workspace, and no later one can change that. A
            // workspace minted here is one nobody has been in before, so
            // this person's Slack user id *is* their member id.
            store::ensure_workspace(
                db,
                &workspace_id,
                &identity.team_name,
                &identity.user_id,
                now,
            )
            .await
            .map_err(database)?;
            match store::link_connection(db, &workspace_id, "slack", &identity.team_id, now)
                .await
                .map_err(database)?
            {
                LinkOutcome::Conflict => return Err(link_conflict()),
                LinkOutcome::Linked | LinkOutcome::AlreadyThisWorkspace => {}
            }
            workspace_id
        }
    };

    // **Which row this person is.** A Slack user id is not always a member
    // id: a person who created the workspace with their email address has a
    // `usr_` member row, and `link_slack_workspace` wrote their Slack user
    // id into `member_identities` pointing at it — "so signing in with Slack
    // later lands on the same row". That promise is kept here, by asking
    // `member_identities` rather than assuming. Without it the same person
    // would get a second member row keyed on the Slack user id, be listed
    // twice by `/members`, see a `/me` whose `user_id` depends on which way
    // they signed in, and lose the `is_owner` their email sign-in reported.
    //
    // The fallback is the Slack-first path this module started as: a person
    // the workspace has never seen keeps the Slack user id as their member
    // id, so `0001`'s rows and issue #6's `apply_user_change` seam are
    // unchanged.
    let member_id = match store::member_for_identity(db, &workspace_id, "slack", &identity.user_id)
        .await
        .map_err(database)?
    {
        Some(member) => member.user_id,
        None => identity.user_id.clone(),
    };
    // Re-linking what is already there is a no-op, so signing in repeatedly
    // is safe, and the identity still names the row chosen above.
    store::link_identity(
        db,
        &workspace_id,
        "slack",
        &identity.user_id,
        &member_id,
        now,
    )
    .await
    .map_err(database)?;
    Ok((workspace_id, member_id))
}

// ---------------------------------------------------------------------------
// Installing the Slack app into a workspace (issue #6)

/// `GET /slack/install` — send a workspace's owner to Slack to approve the
/// app. The whole point is a bot token in `slack_installs`, so it needs the
/// app credentials *and* the public origin (the callback URL has to be
/// byte-identical to the one the token call carries, or Slack refuses the
/// exchange), and the key ring that token will be sealed under — refused
/// before the round trip, because an install that cannot seal what it is
/// about to receive is a 503, not a Slack screen followed by a failure.
async fn slack_install(State(state): State<Arc<ModuleState>>) -> Result<Response, Problem> {
    let settings = state.settings()?;
    state.kms()?;
    slack_redirect(&state, settings, SlackFlow::Install, None, None).await
}

/// `GET /slack/install/callback?code&state` — verify the state, exchange the
/// code, and seal the bot token.
///
/// The response is a small JSON body rather than a redirect, because the
/// browser that arrives here is Slack's approval screen following a link and
/// there is no session for it to land in: signing somebody in is a different
/// route (`/slack/callback`), and an install never signs anybody in. The
/// team is named in the body — the installer needs to see that *their*
/// workspace is the one that was installed.
async fn slack_install_callback(
    State(state): State<Arc<ModuleState>>,
    Query(query): Query<CallbackQuery>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let settings = state.settings()?;
    let kms = state.kms()?;
    let signer = port(state.ctx.ports.signer.clone())?;
    let clock = port(state.ctx.ports.clock.clone())?;
    let http = port(state.ctx.ports.http.clone())?;
    let db = port(state.ctx.ports.db.clone())?;

    let Some(cookie) = flow::cookie_value(&headers, flow::FLOW_COOKIE) else {
        return Err(install_expired());
    };
    let Some(flow_cookie) = flow::open_install(&*signer, &*clock, &cookie) else {
        return Err(install_expired());
    };
    let (Some(code), Some(returned_state)) = (query.code.as_deref(), query.state.as_deref()) else {
        return Err(install_expired());
    };
    // Constant time, like the sign-in callback: the state is what proves this
    // redirect belongs to an attempt this server started.
    if !constant_time_eq(flow_cookie.state.as_bytes(), returned_state.as_bytes()) {
        return Err(install_expired());
    }

    let installed = slack::install_code(
        &*http,
        &settings.client_id,
        &settings.client_secret,
        &events::install_callback_url(state.public_base()?),
        code,
    )
    .await
    .map_err(install_failed)?;

    // The KMS error text names the key ring, never the token: it is a
    // deployment fault and its own detail is already scrubbed.
    let sealed = install::seal(&**kms, &installed.team_id, installed.token.expose())
        .await
        .map_err(|err| install_failed(slack::SlackError::internal(err.to_string())))?;
    let now = clock.now().format(&Rfc3339).unwrap_or_default();
    install::put_install(
        &*db,
        &installed.team_id,
        &installed.app_id,
        &installed.bot_user_id,
        &sealed,
        &now,
    )
    .await
    .map_err(database)?;

    let mut response = Json(Installed {
        status: "installed",
        team_id: &installed.team_id,
        app_id: &installed.app_id,
    })
    .into_response();
    // The flow cookie is spent either way: one install attempt, one token.
    response.headers_mut().append(
        header::SET_COOKIE,
        flow::clear_cookie(flow::FLOW_COOKIE).ok_or_else(Problem::internal)?,
    );
    Ok(response)
}

/// `GET /slack/manifest` — this deployment's Slack app manifest. A 503 when
/// the public origin is unset, because a manifest built without it names URLs
/// nobody can reach, and an operator pasting that into Slack gets an app that
/// installs and then silently receives nothing.
async fn slack_manifest(State(state): State<Arc<ModuleState>>) -> Result<Response, Problem> {
    Ok(Json(events::manifest(state.public_base()?)).into_response())
}

/// The one body the install callback answers with. It names the team that was
/// installed and nothing else — in particular never the token, which is in the
/// database and nowhere else.
#[derive(Debug, Serialize)]
struct Installed<'a> {
    status: &'static str,
    team_id: &'a str,
    app_id: &'a str,
}

fn install_expired() -> Problem {
    Problem::new(&INSTALL_EXPIRED)
}

fn install_failed(error: slack::SlackError) -> Problem {
    let problem = Problem::new(&INSTALL_FAILED);
    match error.0 {
        Some(detail) => problem.with_detail(detail),
        None => problem,
    }
}

#[derive(Debug, Serialize)]
struct WorkspaceView {
    id: String,
    name: String,
    owner_id: String,
}

#[derive(Debug, Serialize)]
struct MemberView {
    user_id: String,
    name: String,
    timezone: Option<String>,
    is_admin: bool,
}

#[derive(Debug, Serialize)]
struct MeResponse {
    workspace: WorkspaceView,
    member: MemberView,
    is_owner: bool,
}

/// `GET /me` — the signed-in person's workspace, their member row, and
/// whether they own it.
async fn me(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let (workspace, member, is_owner) = session_scope(&state, &headers).await?;
    Ok(Json(MeResponse {
        workspace: WorkspaceView {
            id: workspace.id,
            name: workspace.name,
            owner_id: workspace.owner_id,
        },
        member: member_view(member),
        is_owner,
    })
    .into_response())
}

/// `GET /members` — every member of the session's workspace, and only that
/// workspace's.
async fn members(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let (workspace, _member, _is_owner) = session_scope(&state, &headers).await?;
    let db = port(state.ctx.ports.db.clone())?;
    let rows = store::members(&*db, &workspace.id)
        .await
        .map_err(database)?;
    Ok(Json(rows.into_iter().map(member_view).collect::<Vec<_>>()).into_response())
}

/// Resolves the signed-in person from the session cookie, and only from
/// it: the workspace id is the one the verified claims name, so no request
/// can point the query at another workspace. A verified session whose
/// member row is gone is refused exactly like no session at all.
async fn session_scope(
    state: &ModuleState,
    headers: &HeaderMap,
) -> Result<(Workspace, Member, bool), Problem> {
    // The cookie is checked before the ports so that a request carrying no
    // session at all is a plain 401 whatever the deployment's config looks
    // like: only a request that actually has something to verify needs the
    // signer the deployment has to have supplied.
    let Some(cookie) = flow::cookie_value(headers, flow::SESSION_COOKIE) else {
        return Err(no_session());
    };

    let signer = port(state.ctx.ports.signer.clone())?;
    let clock = port(state.ctx.ports.clock.clone())?;
    let db = port(state.ctx.ports.db.clone())?;
    let Some(session) = flow::open_session(&*signer, &*clock, &cookie) else {
        return Err(no_session());
    };
    session_scope_of(&*db, &session).await
}

/// Who is calling: the workspace the session names, the member within it,
/// and whether that member may administer the workspace.
///
/// The seam a sibling module reads when it needs tenancy without
/// reimplementing the cookie check — [`caller`] hands it over. The ids are
/// opaque: a caller may only carry them into a query scoped by both.
#[derive(Debug, Clone)]
pub struct Caller {
    /// The workspace the verified session names.
    pub workspace_id: String,
    /// The member within that workspace.
    pub user_id: String,
    /// The workspace's owner, or a member the sign-in method marks admin.
    pub is_admin: bool,
}

/// Resolves the caller from the session cookie, through the same
/// [`session_scope`] the module's own routes use, so there is one
/// implementation of "who is signed in" rather than two that can drift.
///
/// # Errors
///
/// The same [`Problem`] `session_scope` answers: no cookie, one that does
/// not verify or has expired, or a verified session whose workspace or
/// member row is gone.
pub async fn caller(
    signer: &dyn Signer,
    clock: &dyn Clock,
    db: &dyn Database,
    headers: &HeaderMap,
) -> Result<Caller, Problem> {
    let Some(cookie) = flow::cookie_value(headers, flow::SESSION_COOKIE) else {
        return Err(no_session());
    };
    let Some(session) = flow::open_session(signer, clock, &cookie) else {
        return Err(no_session());
    };
    let (workspace, member, is_owner) = session_scope_of(db, &session).await?;
    Ok(Caller {
        workspace_id: workspace.id,
        user_id: member.user_id,
        is_admin: is_owner || member.is_admin,
    })
}

/// Resolves a caller from a **verified pair of ids** rather than from a
/// cookie — a personal access token (issue #72) stores `workspace_id` and
/// `user_id` as columns. `Ok(None)` is the same refusal a missing session
/// gets, which is the property a token needs: one minted for somebody who
/// has since left the workspace stops working rather than outliving the
/// membership it was minted for. These are row reads against this module's
/// own tables, so a sibling module never has to know their shape.
///
/// # Errors
///
/// [`DbError`] when either lookup fails.
pub async fn caller_for(
    db: &dyn Database,
    workspace_id: &str,
    user_id: &str,
) -> Result<Option<Caller>, DbError> {
    let Some(workspace) = store::workspace(db, workspace_id).await? else {
        return Ok(None);
    };
    let Some(member) = store::member(db, workspace_id, user_id).await? else {
        return Ok(None);
    };
    Ok(Some(Caller {
        workspace_id: workspace.id,
        user_id: member.user_id,
        is_admin: workspace.owner_id == user_id || member.is_admin,
    }))
}

/// The one place the session's workspace field is read: a verified
/// [`Session`] resolved to its workspace and member, or refused exactly
/// like no session at all. Both the module's routes and [`caller`] go
/// through here.
async fn session_scope_of(
    db: &dyn Database,
    session: &Session,
) -> Result<(Workspace, Member, bool), Problem> {
    let Some(workspace) = store::workspace(db, &session.workspace_id)
        .await
        .map_err(database)?
    else {
        return Err(no_session());
    };
    let Some(member) = store::member(db, &session.workspace_id, &session.user_id)
        .await
        .map_err(database)?
    else {
        return Err(no_session());
    };
    let is_owner = workspace.owner_id == session.user_id;
    Ok((workspace, member, is_owner))
}

/// `POST /signout` — end the session in this browser.
///
/// Answers `204` and a `Set-Cookie` that drops `__Host-lb_session` with the
/// same attributes it was set with (`Path=/`, `Secure`, `HttpOnly`,
/// `SameSite=Lax`) and `Max-Age=0`. It answers `204` whether or not a session
/// was presented, so signing out twice — or in a second tab — is not an
/// error.
///
/// It is a cookie write, so it takes the same same-origin check the other
/// cookie writes in this venture take ([`require_same_origin`]): another site
/// cannot sign a person out by posting here. `SameSite=Lax` already keeps the
/// cookie off a cross-site `POST`, but a sibling subdomain is *same-site*, so
/// the check is what refuses one of those.
///
/// There is no session row to revoke: a session is a signed, self-expiring
/// cookie ([`flow::Session`]) and nothing is stored server-side, so a copy of
/// the cookie taken before this call still verifies until its `exp`. Closing
/// that needs a server-side session table or a per-member revocation stamp,
/// which is a migration and not this route.
async fn signout(headers: HeaderMap) -> Result<Response, Problem> {
    require_same_origin(&headers)?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        flow::clear_cookie(flow::SESSION_COOKIE).ok_or_else(Problem::internal)?,
    );
    Ok(response)
}

/// Refuses a write a browser reports as coming from another site.
///
/// The rule `livingbrain-tokens` applies to its cookie writes, restated here
/// because that crate depends on this one and not the other way round.
/// `sec-fetch-site` is checked first — `same-origin` and `none` pass and
/// everything else, including `same-site`, is refused — then `Origin`, when
/// present, must be this request's own origin, compared as an RFC 6454
/// origin rather than as a string. A request carrying neither header is not
/// a browser and is accepted.
fn require_same_origin(headers: &HeaderMap) -> Result<(), Problem> {
    let refused = |detail: String| Problem::new(&CROSS_SITE_REQUEST).with_detail(detail);
    if let Some(site) = headers.get("sec-fetch-site") {
        let site = site.to_str().unwrap_or_default().to_ascii_lowercase();
        if !matches!(site.as_str(), "same-origin" | "none") {
            return Err(refused(format!(
                "sec-fetch-site is {site}; only same-origin or none is accepted"
            )));
        }
    }
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Ok(());
    };
    let presented = origin
        .to_str()
        .map_err(|_| refused("origin was not valid UTF-8".to_owned()))?;
    let presented = origin_of(presented)
        .map_err(|err| refused(format!("origin {presented:?} is not an origin: {err}")))?;
    let own = own_origin(headers)
        .ok_or_else(|| refused("origin was present but the request named no host".to_owned()))?;
    if presented != own {
        return Err(refused(format!(
            "origin {presented} does not match the host this request reached ({own})"
        )));
    }
    Ok(())
}

/// The origin this request reached: its `Host`, under `x-forwarded-proto`
/// when a proxy names one and `https` otherwise — the resolution
/// `livingbrain-tokens` uses for the same check.
fn own_origin(headers: &HeaderMap) -> Option<String> {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .filter(|host| !host.is_empty())?;
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(|scheme| scheme.split(',').next().unwrap_or(scheme).trim())
        .filter(|scheme| matches!(*scheme, "http" | "https"))
        .unwrap_or("https");
    origin_of(&format!("{scheme}://{host}")).ok()
}

fn member_view(member: Member) -> MemberView {
    MemberView {
        user_id: member.user_id,
        name: member.name,
        timezone: member.timezone,
        is_admin: member.is_admin,
    }
}

// ---------------------------------------------------------------------------
// The email magic link

#[derive(Debug, Deserialize)]
struct EmailStartBody {
    email: String,
    workspace_id: Option<String>,
    workspace_name: Option<String>,
}

/// The one body this route ever answers with. It is a constant because the
/// response must not vary with whether the address belongs to anybody: a
/// different answer for a known address is an enumeration oracle, and this
/// route sends mail to any address that asks for it.
#[derive(Debug, Serialize)]
struct EmailAccepted {
    status: &'static str,
}

/// `POST /email/start` — mint a single-use sign-in link and mail it.
///
/// Always answers `202` with [`EmailAccepted`], whatever the address is and
/// whatever the mail provider did. An invalid address is the one thing that
/// changes the answer, and it is a 400 that says nothing about any other
/// address; a request that cannot be delivered at all is a 503 or a 502,
/// which are facts about the deployment, not about the person asking.
///
/// Works with no Slack credentials: nothing on this path reads
/// [`ModuleState::settings`].
async fn email_start(
    State(state): State<Arc<ModuleState>>,
    Json(body): Json<EmailStartBody>,
) -> Result<Response, Problem> {
    let email = normalize_email(&body.email);
    // A malformed address is a 400 rather than a quiet 202, which is the
    // one distinction this route may make: it is a property of the string
    // the caller sent, never of whether the address exists here. The
    // address is validated *before* the deployment is asked about, because
    // a 400 for a typo is more useful than a 503 from an operator who has
    // not wired a mailer yet. Reordering it above the `mailer` and
    // `public_base` lookups is what makes that true of the code rather
    // than only of the comment, and it leaks nothing: which of the two the
    // caller gets says nothing about whether the address belongs to
    // anybody, and both are properties of the request or the deployment.
    if let Some(problem) = cratefield_core::validation_error(&email).map(invalid_email_problem) {
        return Err(problem);
    }
    let mailer = state.mailer()?;
    let public_base = state.public_base()?;
    let clock = port(state.ctx.ports.clock.clone())?;
    let id_gen = port(state.ctx.ports.id_gen.clone())?;
    let db = port(state.ctx.ports.db.clone())?;

    let workspace_id = body
        .workspace_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let workspace_name = body
        .workspace_name
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();

    let token = sign_in_token(&*id_gen);
    let now = clock.now();
    let now_text = now.format(&Rfc3339).unwrap_or_default();
    let expires_at = (now + time::Duration::seconds(flow::SIGN_IN_TTL_SECS))
        .format(&Rfc3339)
        .unwrap_or_default();
    store::put_sign_in_link(
        &*db,
        &token_hash(&token),
        &email,
        workspace_id,
        workspace_name,
        &expires_at,
        &now_text,
    )
    .await
    .map_err(database)?;

    let link = format!("{public_base}/v1/workspaces/email/verify?token={token}");
    let message = sign_in_mail(&state.mail_from, &email, &link);
    match mailer.send(message).await {
        Ok(SendOutcome::Sent { .. }) => {}
        // A mailer that is wired but has no key sends nothing. That is a
        // deployment fault, and the 202 below would be a lie, so it is
        // refused here rather than swallowed.
        Ok(SendOutcome::NotConfigured) => {
            return Err(Problem::new(&MAIL_NOT_CONFIGURED));
        }
        Err(error) => return Err(mail_failed(error)),
    }
    email_ok()
}

/// The 202 every address gets, valid or not.
fn email_ok() -> Result<Response, Problem> {
    Ok((
        StatusCode::ACCEPTED,
        Json(EmailAccepted { status: "accepted" }),
    )
        .into_response())
}

impl ModuleState {
    /// The `Mailer` port, or the 503 that says no mail can be sent. It is
    /// optional rather than required, so a deployment that only ever signs
    /// people in with Slack composes without one.
    fn mailer(&self) -> Result<Arc<dyn cratefield_core::Mailer>, Problem> {
        self.ctx
            .ports
            .mailer
            .clone()
            .ok_or_else(|| Problem::new(&MAIL_NOT_CONFIGURED))
    }
}

/// A sign-in mail, rendered here rather than through a template crate: a
/// module may depend on `cratefield-core` and pure Rust, and this venture's
/// mail theme belongs to the module that owns it.
///
/// The link is the only thing the body has to say, so the text part is one
/// line and the HTML part is the same sentence in a link.
fn sign_in_mail(from: &str, to: &str, link: &str) -> Message {
    let minutes = flow::SIGN_IN_TTL_SECS / 60;
    let text = format!(
        "Your Living Brain sign-in link:\n\n{link}\n\nIt works once, and for the next \
         {minutes} minutes. If you did not ask for it, ignore this message — nothing has \
         happened yet."
    );
    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <title>Your Living Brain sign-in link</title>\n</head>\n<body>\n\
         <h1>Sign in to Living Brain</h1>\n\
         <p>This link works once, and for the next {minutes} minutes.</p>\n\
         <p><a href=\"{link}\">Sign in</a></p>\n\
         <p>If you did not ask for it, ignore this message — nothing has happened yet.</p>\n\
         </body>\n</html>\n"
    );
    Message::new(
        to.to_owned(),
        from.to_owned(),
        "Your Living Brain sign-in link",
        text,
        html,
    )
    // A provider that retried the same send collapses it instead of
    // mailing twice. The key is the SHA-256 of the address, not the
    // address: an `Idempotency-Key` header is sent to a third party.
    .idempotency_key(format!("workspaces-sign-in-{}", token_hash(to)))
}

/// A mail the provider refused. The error's `Display` is already scrubbed
/// of addresses, so it is safe as a detail — and the response body still
/// says nothing about whether the address is one anybody here has.
fn mail_failed(_error: MailError) -> Problem {
    Problem::new(&MAIL_FAILED)
}

#[derive(Debug, Deserialize)]
struct VerifyQuery {
    token: Option<String>,
}

/// `GET /email/verify?token=…` — render the form a person submits, and
/// nothing else.
///
/// **It does not spend the token.** Mail scanners and link-safety checkers
/// fetch every link in a message on arrival, before anybody has read it;
/// spending here would mean the real recipient arrives to a dead link. The
/// token is spent by the POST, which a scanner does not make.
///
/// Because the URL carries a live credential and the page repeats it into a
/// hidden field, the response is written by [`verify_page`]: no cache, no
/// referrer, no framing. It is used once, in the tab it was opened in.
async fn email_verify_form(
    State(state): State<Arc<ModuleState>>,
    Query(query): Query<VerifyQuery>,
) -> Result<Response, Problem> {
    let Some(token) = query
        .token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return Ok(verify_page(&verify_shell(
            "Sign in",
            "<p class=\"eyebrow\">Sign-in link</p>\n\
             <h1>This sign-in link is not usable</h1>\n\
             <div class=\"notice\" data-tone=\"warn\">Ask for a new one and open the newest \
             message.</div>\n\
             <p><a class=\"btn\" href=\"/index.html\">Back to sign-in</a></p>\n",
        )));
    };
    // The token is the caller's own, from their own URL, and it is rendered
    // into a hidden field of a form that posts it straight back. It is
    // still escaped, because a token is still a value from a request.
    let escaped = html_escape(token);
    let action = format!("{}/v1/workspaces/email/verify", state.public_base()?);
    Ok(verify_page(&verify_shell(
        "Sign in to Living Brain",
        &format!(
            "<p class=\"eyebrow\">Sign-in link</p>\n\
             <h1>Sign in to Living Brain</h1>\n\
             <p class=\"lede\">This link works once, and for the next {} minutes.</p>\n\
             <form method=\"post\" action=\"{action}\">\n\
             <input type=\"hidden\" name=\"token\" value=\"{escaped}\">\n\
             <button type=\"submit\" data-variant=\"primary\">Sign in</button>\n</form>\n\
             <p class=\"tiny\">If you did not ask for it, close this page and ignore the \
             message.</p>\n",
            flow::SIGN_IN_TTL_SECS / 60
        ),
    )))
}

/// The page around a verify body: the web app's stylesheet, mark and header,
/// so the one page the API renders itself looks like the rest of the app.
///
/// The app is served from the same host as static assets, so the stylesheet
/// and icon are root-relative. Both are same-origin and the page sends no
/// referrer, so loading them cannot carry the token in the URL anywhere; on a
/// host without the app's assets they 404 and the page still works unstyled.
/// No script is added: this page needs none, and the theme simply follows the
/// system here.
fn verify_shell(title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"color-scheme\" content=\"dark light\">\n\
         <title>{title}</title>\n\
         <link rel=\"icon\" href=\"/assets/favicon.svg\" type=\"image/svg+xml\">\n\
         <link rel=\"stylesheet\" href=\"/assets/app.css\">\n</head>\n<body>\n\
         <header class=\"hd\"><div class=\"wrap hd__in\">\
         <a class=\"brand\" href=\"/index.html\"><span class=\"mark\" aria-hidden=\"true\">\
         <span></span></span><span class=\"brand__word\">livingbrain<span>.wiki</span>\
         </span></a></div></header>\n\
         <main id=\"main\" class=\"wrap solo\">\n<div class=\"card\">\n{body}</div>\n</main>\n\
         </body>\n</html>\n"
    )
}

/// A page the verify route serves, with the headers a page carrying a live
/// token in its URL needs.
///
/// The URL holds the token, and it is also rendered into a hidden field, so:
///
/// - `no-store` keeps the response — token included — out of a shared
///   cache, a back-forward cache, and any disk the browser keeps;
/// - `no-referrer` keeps the URL out of the `Referer` of anything this page
///   loads or the person clicks, which is the other way a token in a query
///   string escapes;
/// - `frame-ancestors 'none'`, with `X-Frame-Options: DENY` for the user
///   agents that predate CSP, keeps another site from framing the form —
///   clickjacking somebody into spending their own link on a machine that
///   already holds somebody else's session.
///
/// Cratefield's harness already stamps the same four on every `/v1/*`
/// response (architecture section 6, issue #435), so at runtime these
/// agree with it — the layer `insert`s the two it shares and *appends* its
/// CSP, which a browser enforces alongside this one, so the answer is
/// unchanged. They are here because the safety of this page should be the
/// page's own property rather than a property of whatever happens to wrap
/// it: a route mounted outside `/v1`, a second router, or a future platform
/// that drops the layer would otherwise leave a token in a URL with no
/// cache, referrer or framing control at all. An integration test cannot
/// tell the difference — the layer answers first — so the unit test at the
/// bottom of this file is what holds this in place.
///
/// [`Html`] sends `content-type: text/html; charset=utf-8` itself, so the
/// escaping in [`html_escape`] is the only thing between a request and this
/// page, and a browser is told what it is reading.
fn verify_page(body: &str) -> Response {
    (
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'"),
            // `frame-ancestors` supersedes this; it is here for the user
            // agents that do not read CSP.
            (header::X_FRAME_OPTIONS, "DENY"),
        ],
        Html(body.to_owned()),
    )
        .into_response()
}

/// `POST /email/verify` — spend the token, resolve the member it names,
/// and seal the same session cookie the Slack path seals.
///
/// The row decides what happens. A row with a workspace id signs in the
/// member the address is already an identity of, and a row with none
/// creates a workspace with that address as its owner and first member. A
/// link for a workspace where the address is nobody's identity is refused:
/// ADR 0002 makes a magic link a way *into* a workspace you belong to, not
/// a way to claim one you do not.
async fn email_verify(
    State(state): State<Arc<ModuleState>>,
    Query(query): Query<VerifyQuery>,
    body: String,
) -> Result<Response, Problem> {
    let signer = port(state.ctx.ports.signer.clone())?;
    let clock = port(state.ctx.ports.clock.clone())?;
    let db = port(state.ctx.ports.db.clone())?;
    let id_gen = port(state.ctx.ports.id_gen.clone())?;

    // The form posts a urlencoded body; a client that prefers JSON may post
    // the same shape. Either way the token is the one the query carried or
    // the one the body carried, and it is hashed before it is looked up.
    let token = submitted_token(query.token.as_deref(), &body).ok_or_else(expired)?;

    let now = clock.now();
    let now_text = now.format(&Rfc3339).unwrap_or_default();
    // The UPDATE marks the row spent, so a link cannot be spent twice even
    // if two requests race it. An expired row is spent too: forwarding a
    // link must not keep it alive.
    let Some(row) = store::take_sign_in_link(&*db, &token_hash(&token), &now_text)
        .await
        .map_err(database)?
    else {
        return Err(expired());
    };
    // Every timestamp here is RFC 3339 in the same offset, so the ordering
    // is a string comparison; an expired link is one that sorts before now.
    if row.expires_at <= now_text {
        return Err(expired());
    }

    // `display_name` is what to write to a *new* member row, and `None`
    // means "write nothing": an address is not a display name, and a member
    // who signed in with Slack has a real one that an email sign-in must
    // not overwrite.
    let (workspace_id, member_id, display_name) = match row.workspace_id.as_deref() {
        Some(workspace_id) => {
            let Some(member) = store::member_for_identity(&*db, workspace_id, "email", &row.email)
                .await
                .map_err(database)?
            else {
                return Err(Problem::new(&NOT_A_MEMBER));
            };
            (workspace_id.to_owned(), member.user_id, None)
        }
        None => {
            let owner_id = new_user_id(&*id_gen);
            let workspace_id = new_workspace_id(&*id_gen);
            let name = workspace_name(&row);
            // Insert-if-absent, owner this signer — the same rule Slack
            // sign-in follows, and the only moment an owner is decided.
            store::ensure_workspace(&*db, &workspace_id, &name, &owner_id, &now_text)
                .await
                .map_err(database)?;
            // `('email', address)` is unique across every workspace, not
            // within one: the first workspace ever created from an address
            // owns that connection, and `UNIQUE (platform, external_id)`
            // refuses a second. So the two outcomes are not the same
            // thing, and the outcome is named rather than dropped:
            //
            // - `Linked` / `AlreadyThisWorkspace` — this workspace now has
            //   the address as a connection, which is the normal case.
            // - `Conflict` — this address is already the connection of
            //   *another* workspace, and that row is left exactly as it
            //   was. That is expected, not an error: a person may create a
            //   second workspace from the same address, and signing in is
            //   decided by `member_identities`, which is per workspace and
            //   is written just below, so this workspace is reachable by
            //   its owner's address either way. The row that holds the
            //   address is the first workspace's, and taking it from them
            //   would be the hijack `link_connection` refuses everywhere
            //   else.
            match store::link_connection(&*db, &workspace_id, "email", &row.email, &now_text)
                .await
                .map_err(database)?
            {
                LinkOutcome::Linked | LinkOutcome::AlreadyThisWorkspace => {}
                LinkOutcome::Conflict => {}
            }
            store::link_identity(
                &*db,
                &workspace_id,
                "email",
                &row.email,
                &owner_id,
                &now_text,
            )
            .await
            .map_err(database)?;
            // The local part is the closest thing to a name an address
            // carries, and it is what the person would have typed anyway.
            (workspace_id, owner_id, Some(name))
        }
    };

    store::upsert_member(
        &*db,
        &workspace_id,
        &member_id,
        store::MemberFields {
            name: display_name.as_deref(),
            ..store::MemberFields::default()
        },
        &now_text,
    )
    .await
    .map_err(database)?;

    let session_value = flow::seal(
        &*signer,
        flow::SESSION_PURPOSE,
        &Session {
            workspace_id: workspace_id.clone(),
            user_id: member_id,
            exp: clock.now().unix_timestamp() + flow::SESSION_TTL_SECS,
        },
    )
    .ok_or_else(Problem::internal)?;

    // The flow cookie is dropped on this path too: it belongs to a Slack
    // round trip and a magic link starts a session of its own.
    let mut response = (StatusCode::FOUND, [(header::LOCATION, "/")]).into_response();
    for cookie in [
        flow::clear_cookie(flow::FLOW_COOKIE),
        flow::set_cookie(flow::SESSION_COOKIE, &session_value, flow::SESSION_TTL_SECS),
    ] {
        response
            .headers_mut()
            .append(header::SET_COOKIE, cookie.ok_or_else(Problem::internal)?);
    }
    Ok(response)
}

/// The token a verify POST carries: the query parameter, or the `token`
/// field of a urlencoded form, or the `token` field of a JSON body.
///
/// The form is what [`email_verify_form`] renders, so urlencoded is the
/// path a browser takes; the other two are what a client sends.
fn submitted_token(query: Option<&str>, body: &str) -> Option<String> {
    let from_query = query
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_owned);
    if from_query.is_some() {
        return from_query;
    }
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(body) {
        return map
            .get("token")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(str::to_owned);
    }
    body.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name.trim() == "token")
            .then(|| percent_decode(value))
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty())
    })
}

/// `application/x-www-form-urlencoded`'s decoding of one value: `+` is a
/// space and `%XX` is a byte. Anything malformed is passed through
/// unchanged, so a token that is not percent-encoded still works.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or_default();
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| value.to_owned())
}

/// The five characters that can break out of HTML text or an attribute, and
/// nothing else. The token is hex, so in practice none of them appear —
/// which is exactly why this is here rather than trusted.
fn html_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// The name for a workspace a link is about to create: what the caller
/// asked for, or the local part of the address, which is what a person
/// typing their own email would have called it.
fn workspace_name(row: &store::SignInLinkRow) -> String {
    let asked_for = row.workspace_name.trim();
    if !asked_for.is_empty() {
        return asked_for.to_owned();
    }
    row.email
        .split_once('@')
        .map(|(local, _)| local)
        .filter(|local| !local.is_empty())
        .unwrap_or(row.email.as_str())
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The verify page carries a live token in its URL and in a hidden
    /// field, so the response that carries it must be uncacheable,
    /// unreferrable and unframable.
    ///
    /// This is a unit test on purpose. The harness stamps the same four on
    /// every `/v1/*` response (Cratefield architecture section 6), so an
    /// integration test would pass whether or not this page set them — and
    /// would pass if they were ever removed here.
    #[test]
    fn the_verify_page_is_never_cached_referred_or_framed() {
        let response = verify_page("<html></html>");
        let headers = response.headers();
        for (name, expected) in [
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'"),
            (header::X_FRAME_OPTIONS, "DENY"),
        ] {
            assert_eq!(
                headers.get(&name).and_then(|value| value.to_str().ok()),
                Some(expected),
                "{} on the verify page",
                name.as_str()
            );
        }
        assert_eq!(
            headers
                .get(&header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/html; charset=utf-8"),
            "and it is HTML, so the escaping is the only defence there is"
        );
    }
}
