//! The module's routes, mounted at `/v1/workspaces`.
//!
//! Two are the sign-in flow (`/slack/start` and `/slack/callback`), and
//! two read it back (`/me` and `/members`). None of the four takes a
//! workspace id from the request: the workspace is whatever the verified
//! session cookie names, and for the callback it is whatever Slack's
//! `id_token` names — never the caller's input.

use std::sync::Arc;

use cratefield_core::axum::extract::{Query, State};
use cratefield_core::axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::axum::routing::get;
use cratefield_core::axum::{self, Json};
use cratefield_core::{
    Clock, Database, DbError, IdGen, ModuleContext, Problem, ProblemDef, Signer, constant_time_eq,
};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;

use crate::config::Settings;
use crate::flow::{self, Flow, Session};
use crate::slack;
use crate::store::{self, Member, Workspace};

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

/// The router state: the module's ports, and the Slack settings as a
/// `Result` so a missing credential is a 503 per request, not a panic at
/// startup.
struct ModuleState {
    ctx: ModuleContext,
    settings: Result<Settings, String>,
}

impl ModuleState {
    fn settings(&self) -> Result<&Settings, Problem> {
        self.settings
            .as_ref()
            .map_err(|reason| Problem::new(&NOT_CONFIGURED).with_detail(reason.clone()))
    }
}

/// The module's routes.
pub(crate) fn router(ctx: ModuleContext) -> axum::Router {
    let settings = Settings::from_config(&*ctx.config).map_err(|err| err.to_string());
    let state = Arc::new(ModuleState { ctx, settings });
    axum::Router::new()
        .route("/slack/start", get(slack_start))
        .route("/slack/callback", get(slack_callback))
        .route("/me", get(me))
        .route("/members", get(members))
        .with_state(state)
}

/// A port the module declared and the runtime did not supply: a deployment
/// fault, answered with nothing more than a 500.
fn port<T: ?Sized>(slot: Option<Arc<T>>) -> Result<Arc<T>, Problem> {
    slot.ok_or_else(Problem::internal)
}

/// A failed statement. The adapter's message never reaches the body.
fn database(_error: DbError) -> Problem {
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

/// `GET /slack/start` — mint the CSRF state and OIDC nonce, seal them into
/// the flow cookie, and send the browser to Slack.
async fn slack_start(State(state): State<Arc<ModuleState>>) -> Result<Response, Problem> {
    let settings = state.settings()?;
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
        flow::FLOW_PURPOSE,
        &Flow {
            state: state_value.clone(),
            nonce: nonce.clone(),
            expires_at,
        },
    )
    .ok_or_else(Problem::internal)?;

    let url = slack::authorize_url(
        &settings.client_id,
        &settings.callback_url(),
        &state_value,
        &nonce,
    );
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
/// state, and the OIDC nonce.
fn secret(id_gen: &dyn IdGen) -> String {
    format!("{}{}", id_gen.ulid(), id_gen.ulid())
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
}

/// `GET /slack/callback` — verify the flow cookie, exchange the code,
/// check the `id_token`, then create the workspace if this is its first
/// signer, refresh the member row, and set the session cookie.
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
    // Insert-if-absent, owner this signer: the first person through here
    // owns the workspace, and no later one can change that.
    store::ensure_workspace(
        &*db,
        &identity.team_id,
        &identity.team_name,
        &identity.user_id,
        &now,
    )
    .await
    .map_err(database)?;
    // Name only: the `id_token` carries no timezone and no admin flag.
    store::upsert_member(
        &*db,
        &identity.team_id,
        &identity.user_id,
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
            team_id: identity.team_id,
            user_id: identity.user_id,
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

/// The one place the session's workspace field is read: a verified
/// [`Session`] resolved to its workspace and member, or refused exactly
/// like no session at all. Both the module's routes and [`caller`] go
/// through here.
async fn session_scope_of(
    db: &dyn Database,
    session: &Session,
) -> Result<(Workspace, Member, bool), Problem> {
    let Some(workspace) = store::workspace(db, &session.team_id)
        .await
        .map_err(database)?
    else {
        return Err(no_session());
    };
    let Some(member) = store::member(db, &session.team_id, &session.user_id)
        .await
        .map_err(database)?
    else {
        return Err(no_session());
    };
    let is_owner = workspace.owner_id == session.user_id;
    Ok((workspace, member, is_owner))
}

fn member_view(member: Member) -> MemberView {
    MemberView {
        user_id: member.user_id,
        name: member.name,
        timezone: member.timezone,
        is_admin: member.is_admin,
    }
}
