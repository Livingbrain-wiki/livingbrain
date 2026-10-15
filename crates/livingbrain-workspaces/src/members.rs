//! Members, roles, invitations and the workspace switcher (ADR 0004, phase a).
//!
//! Every route here is a **session** route: the workspace and the acting
//! member come from the verified cookie through [`session_scope`], never
//! from the request, and a personal access token cannot reach them — a
//! credential that could add people to a workspace is not one a script
//! should hold. Every write takes the same-origin check the module's other
//! cookie writes take.
//!
//! ## Who may do what
//!
//! | Action | Who |
//! |---|---|
//! | list members | any member |
//! | invite, change a role, remove a member | an owner or admin |
//! | anything that makes, unmakes or removes an **owner** | an owner |
//! | remove yourself (leave) | anyone, unless you are the last owner |
//!
//! A workspace always keeps one owner: demoting or removing the last one is
//! a `409`, decided by one guarded statement ([`store::set_role`],
//! [`store::remove_member`]).
//!
//! ## Removal
//!
//! The member row is tombstoned and their sign-in identities deleted, which
//! ends every session and personal access token they hold on the next
//! request — both re-read the member row every time, and a removed row is no
//! row. A Slack sign-in by a removed person lands on the tombstone and is
//! refused. The composition's [`MemberRemoval`](crate::MemberRemoval) seam
//! then cleans up what this module cannot reach (token rows, agent accounts,
//! grants).
//!
//! ## Invitations
//!
//! An invitation mails a sign-in link for the workspace with the
//! invitation's lifetime (seven days). Spending it is the ordinary
//! `POST /email/verify`: an address that is not yet a member but holds an
//! open invitation becomes a member with the invited role. While the
//! invitation is open, a fresh magic link for the same workspace and address
//! accepts it too, so a lost mail is not a lost invitation.

use std::sync::Arc;

use cratefield_core::axum::Json;
use cratefield_core::axum::extract::{Path, State};
use cratefield_core::axum::http::{HeaderMap, StatusCode};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::{Problem, ProblemDef, SendOutcome, invalid_email_problem, normalize_email};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;

use crate::flow::{self, Session};
use crate::handlers::{
    ModuleState, database, mail_failed, port, require_same_origin, session_scope, sign_in_token,
    token_hash,
};
use crate::store::{self, Member, Role, RosterOutcome};
use crate::{Invitation, SignInMailInput};

/// How long an invitation, and the link it mails, stays usable.
pub(crate) const INVITE_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// An action the caller's role does not allow.
const FORBIDDEN: ProblemDef = ProblemDef {
    slug: "workspaces/forbidden",
    status: StatusCode::FORBIDDEN,
    title: "Your role does not allow this",
    description: "Ask an owner or admin of the workspace.",
};

/// A member or invitation id that is not in this workspace.
const NOT_FOUND: ProblemDef = ProblemDef {
    slug: "workspaces/not-found",
    status: StatusCode::NOT_FOUND,
    title: "Not found",
    description: "There is no such member or invitation in this workspace.",
};

/// The change would leave the workspace without an owner.
const LAST_OWNER: ProblemDef = ProblemDef {
    slug: "workspaces/last-owner",
    status: StatusCode::CONFLICT,
    title: "A workspace keeps at least one owner",
    description: "Make somebody else an owner first.",
};

/// The address already belongs to a member.
const ALREADY_A_MEMBER: ProblemDef = ProblemDef {
    slug: "workspaces/already-a-member",
    status: StatusCode::CONFLICT,
    title: "That address is already a member",
    description: "Change their role instead.",
};

/// A role this deployment does not accept.
const UNKNOWN_ROLE: ProblemDef = ProblemDef {
    slug: "workspaces/unknown-role",
    status: StatusCode::UNPROCESSABLE_ENTITY,
    title: "Unknown role",
    description: "A role is owner, admin or member. The read-only viewer role arrives \
                  with access grants (ADR 0004, phase c).",
};

/// The mailer is not wired.
const MAIL_NOT_CONFIGURED: ProblemDef = ProblemDef {
    slug: "workspaces/mail-not-configured",
    status: StatusCode::SERVICE_UNAVAILABLE,
    title: "Invitations need email",
    description: "An operator wires a Mailer port and sets WORKSPACES_MAIL_FROM.",
};

fn forbidden() -> Problem {
    Problem::new(&FORBIDDEN)
}

/// The roles a request may name. `viewer` is refused until access grants
/// can make it mean read-only everywhere: a role that says "read-only" and
/// is not would be worse than no such role.
fn requested_role(value: &str) -> Result<Role, Problem> {
    match Role::parse(value.trim()) {
        Some(role @ (Role::Owner | Role::Admin | Role::Member)) => Ok(role),
        _ => Err(Problem::new(&UNKNOWN_ROLE)),
    }
}

fn now_text(state: &ModuleState) -> Result<(time::OffsetDateTime, String), Problem> {
    let clock = port(state.ctx.ports.clock.clone())?;
    let now = clock.now();
    Ok((now, now.format(&Rfc3339).unwrap_or_default()))
}

// ---------------------------------------------------------------------------
// Members

#[derive(Debug, Deserialize)]
pub(crate) struct RoleBody {
    role: String,
}

/// `PATCH /members/{user_id}` — change a member's role.
pub(crate) async fn change_role(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Json(body): Json<RoleBody>,
) -> Result<Response, Problem> {
    require_same_origin(&headers)?;
    let (workspace, actor, _) = session_scope(&state, &headers).await?;
    let role = requested_role(&body.role)?;
    let db = port(state.ctx.ports.db.clone())?;
    if !actor.may_administer() {
        return Err(forbidden());
    }
    let Some(target) = store::member(&*db, &workspace.id, &user_id)
        .await
        .map_err(database)?
    else {
        return Err(Problem::new(&NOT_FOUND));
    };
    // Making, unmaking or touching an owner is an owner's call.
    if (target.role == Role::Owner || role == Role::Owner) && actor.role != Role::Owner {
        return Err(forbidden());
    }
    let (_, now) = now_text(&state)?;
    match store::set_role(&*db, &workspace.id, &user_id, role, &now)
        .await
        .map_err(database)?
    {
        RosterOutcome::Done => {}
        RosterOutcome::NotFound => return Err(Problem::new(&NOT_FOUND)),
        RosterOutcome::LastOwner => return Err(Problem::new(&LAST_OWNER)),
    }
    let updated = store::member(&*db, &workspace.id, &user_id)
        .await
        .map_err(database)?
        .ok_or_else(|| Problem::new(&NOT_FOUND))?;
    Ok(Json(MemberView::from(updated)).into_response())
}

/// `DELETE /members/{user_id}` — remove a member, or leave.
pub(crate) async fn remove(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> Result<Response, Problem> {
    require_same_origin(&headers)?;
    let (workspace, actor, _) = session_scope(&state, &headers).await?;
    let db = port(state.ctx.ports.db.clone())?;
    let leaving = actor.user_id == user_id;
    if !leaving && !actor.may_administer() {
        return Err(forbidden());
    }
    let Some(target) = store::member(&*db, &workspace.id, &user_id)
        .await
        .map_err(database)?
    else {
        return Err(Problem::new(&NOT_FOUND));
    };
    if !leaving && target.role == Role::Owner && actor.role != Role::Owner {
        return Err(forbidden());
    }
    let (now, now_text) = now_text(&state)?;
    match store::remove_member(&*db, &workspace.id, &user_id, &now_text)
        .await
        .map_err(database)?
    {
        RosterOutcome::Done => {}
        RosterOutcome::NotFound => return Err(Problem::new(&NOT_FOUND)),
        RosterOutcome::LastOwner => return Err(Problem::new(&LAST_OWNER)),
    }
    if let Some(hook) = &state.removal {
        hook.removed(&*db, now, &workspace.id, &user_id).await;
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    // Leaving ends this browser's session too, not only every later one.
    if leaving {
        response.headers_mut().append(
            cratefield_core::axum::http::header::SET_COOKIE,
            flow::clear_cookie(flow::SESSION_COOKIE).ok_or_else(Problem::internal)?,
        );
    }
    Ok(response)
}

/// One member, as the roster lists them.
#[derive(Debug, Serialize)]
pub(crate) struct MemberView {
    pub user_id: String,
    pub name: String,
    pub timezone: Option<String>,
    /// Slack's admin flag.
    pub is_admin: bool,
    /// `owner`, `admin`, `member` or `viewer`.
    pub role: &'static str,
}

impl From<Member> for MemberView {
    fn from(member: Member) -> Self {
        Self {
            user_id: member.user_id,
            name: member.name,
            timezone: member.timezone,
            is_admin: member.is_admin,
            role: member.role.as_str(),
        }
    }
}

// ---------------------------------------------------------------------------
// Invitations

#[derive(Debug, Deserialize)]
pub(crate) struct InviteBody {
    email: String,
    #[serde(default)]
    role: Option<String>,
}

#[derive(Debug, Serialize)]
struct InviteView {
    id: String,
    email: String,
    role: &'static str,
    invited_by: String,
    created_at: String,
    expires_at: String,
}

impl From<store::Invite> for InviteView {
    fn from(invite: store::Invite) -> Self {
        Self {
            id: invite.id,
            email: invite.email,
            role: invite.role.as_str(),
            invited_by: invite.invited_by,
            created_at: invite.created_at,
            expires_at: invite.expires_at,
        }
    }
}

/// `POST /invites` — invite an address with a role, and mail the link.
pub(crate) async fn invite(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
    Json(body): Json<InviteBody>,
) -> Result<Response, Problem> {
    require_same_origin(&headers)?;
    let (workspace, actor, _) = session_scope(&state, &headers).await?;
    if !actor.may_administer() {
        return Err(forbidden());
    }
    let role = requested_role(body.role.as_deref().unwrap_or("member"))?;
    if role == Role::Owner && actor.role != Role::Owner {
        return Err(forbidden());
    }
    let email = normalize_email(&body.email);
    if let Some(problem) = cratefield_core::validation_error(&email).map(invalid_email_problem) {
        return Err(problem);
    }
    let db = port(state.ctx.ports.db.clone())?;
    if store::member_for_identity(&*db, &workspace.id, "email", &email)
        .await
        .map_err(database)?
        .is_some()
    {
        return Err(Problem::new(&ALREADY_A_MEMBER));
    }
    let mailer = state
        .ctx
        .ports
        .mailer
        .clone()
        .ok_or_else(|| Problem::new(&MAIL_NOT_CONFIGURED))?;
    let public_base = state.public_base()?.to_owned();
    let id_gen = port(state.ctx.ports.id_gen.clone())?;
    let (now, now_text) = now_text(&state)?;
    let expires_at = (now + time::Duration::seconds(INVITE_TTL_SECS))
        .format(&Rfc3339)
        .unwrap_or_default();

    let token = sign_in_token(&*id_gen);
    let link_hash = token_hash(&token);
    store::put_sign_in_link(
        &*db,
        &link_hash,
        &email,
        Some(&workspace.id),
        "",
        &expires_at,
        &now_text,
    )
    .await
    .map_err(database)?;
    let id = format!("inv_{}", id_gen.ulid());
    store::put_invite(
        &*db,
        &store::NewInvite {
            id: &id,
            workspace_id: &workspace.id,
            email: &email,
            role,
            invited_by: &actor.user_id,
            link_hash: &link_hash,
            expires_at: &expires_at,
        },
        &now_text,
    )
    .await
    .map_err(database)?;

    let link = format!("{public_base}/v1/workspaces/email/verify?token={token}");
    let message = state.message(
        &email,
        &link,
        SignInMailInput {
            link: &link,
            minutes: INVITE_TTL_SECS / 60,
            recipient: &email,
            invitation: Some(Invitation {
                workspace: &workspace.name,
                invited_by: &actor.name,
            }),
        },
    );
    match mailer.send(message).await {
        Ok(SendOutcome::Sent { .. }) => {}
        // Nothing was sent, so nothing may stay open: the invitation and its
        // link are withdrawn rather than left for a mail that never went.
        Ok(SendOutcome::NotConfigured) => {
            let _ = store::revoke_invite(&*db, &workspace.id, &id, &now_text).await;
            return Err(Problem::new(&MAIL_NOT_CONFIGURED));
        }
        Err(error) => {
            let _ = store::revoke_invite(&*db, &workspace.id, &id, &now_text).await;
            return Err(mail_failed(error));
        }
    }
    let created = store::invite(&*db, &workspace.id, &id)
        .await
        .map_err(database)?
        .ok_or_else(Problem::internal)?;
    Ok((StatusCode::CREATED, Json(InviteView::from(created))).into_response())
}

/// `GET /invites` — the open invitations. Owners and admins only: the list
/// names addresses.
pub(crate) async fn invites(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let (workspace, actor, _) = session_scope(&state, &headers).await?;
    if !actor.may_administer() {
        return Err(forbidden());
    }
    let db = port(state.ctx.ports.db.clone())?;
    let (_, now) = now_text(&state)?;
    let open = store::open_invites(&*db, &workspace.id, &now)
        .await
        .map_err(database)?;
    Ok(Json(open.into_iter().map(InviteView::from).collect::<Vec<_>>()).into_response())
}

/// `DELETE /invites/{id}` — withdraw an open invitation; its link stops
/// working.
pub(crate) async fn revoke_invite(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Problem> {
    require_same_origin(&headers)?;
    let (workspace, actor, _) = session_scope(&state, &headers).await?;
    if !actor.may_administer() {
        return Err(forbidden());
    }
    let db = port(state.ctx.ports.db.clone())?;
    let Some(found) = store::invite(&*db, &workspace.id, &id)
        .await
        .map_err(database)?
    else {
        return Err(Problem::new(&NOT_FOUND));
    };
    // An admin may not withdraw what only an owner could have sent.
    if found.role == Role::Owner && actor.role != Role::Owner {
        return Err(forbidden());
    }
    let (_, now) = now_text(&state)?;
    if !store::revoke_invite(&*db, &workspace.id, &id, &now)
        .await
        .map_err(database)?
    {
        return Err(Problem::new(&NOT_FOUND));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// More than one workspace

#[derive(Debug, Serialize)]
struct WorkspaceItem {
    id: String,
    name: String,
    role: &'static str,
    current: bool,
}

/// `GET /` — the workspaces the signed-in person can switch to: every
/// workspace where an email address they sign in with here is a live
/// member's identity, plus this one.
pub(crate) async fn list_workspaces(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let (workspace, member, _) = session_scope(&state, &headers).await?;
    let db = port(state.ctx.ports.db.clone())?;
    let addresses = store::email_identities(&*db, &workspace.id, &member.user_id)
        .await
        .map_err(database)?;
    let mut items: Vec<WorkspaceItem> = store::memberships_for(&*db, &addresses)
        .await
        .map_err(database)?
        .into_iter()
        .map(|found| WorkspaceItem {
            current: found.workspace_id == workspace.id,
            id: found.workspace_id,
            name: found.workspace_name,
            role: found.role.as_str(),
        })
        .collect();
    items.dedup_by(|a, b| a.id == b.id);
    if !items.iter().any(|item| item.current) {
        items.insert(
            0,
            WorkspaceItem {
                id: workspace.id,
                name: workspace.name,
                role: member.role.as_str(),
                current: true,
            },
        );
    }
    Ok(Json(items).into_response())
}

#[derive(Debug, Deserialize)]
pub(crate) struct SwitchBody {
    workspace_id: String,
}

/// `POST /switch` — seal a session for the member the signed-in person is
/// in another workspace. Refused unless an email address they sign in with
/// *here* is a live member's identity *there*: a workspace is never reached
/// by naming it.
pub(crate) async fn switch(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
    Json(body): Json<SwitchBody>,
) -> Result<Response, Problem> {
    require_same_origin(&headers)?;
    let (workspace, member, _) = session_scope(&state, &headers).await?;
    let db = port(state.ctx.ports.db.clone())?;
    let addresses = store::email_identities(&*db, &workspace.id, &member.user_id)
        .await
        .map_err(database)?;
    let target = store::memberships_for(&*db, &addresses)
        .await
        .map_err(database)?
        .into_iter()
        .find(|found| found.workspace_id == body.workspace_id.trim())
        .ok_or_else(|| Problem::new(&NOT_FOUND))?;
    let signer = port(state.ctx.ports.signer.clone())?;
    let clock = port(state.ctx.ports.clock.clone())?;
    let value = flow::seal(
        &*signer,
        flow::SESSION_PURPOSE,
        &Session {
            workspace_id: target.workspace_id.clone(),
            user_id: target.user_id,
            exp: clock.now().unix_timestamp() + flow::SESSION_TTL_SECS,
        },
    )
    .ok_or_else(Problem::internal)?;
    let mut response =
        Json(serde_json::json!({ "workspace_id": target.workspace_id })).into_response();
    response.headers_mut().append(
        cratefield_core::axum::http::header::SET_COOKIE,
        flow::set_cookie(flow::SESSION_COOKIE, &value, flow::SESSION_TTL_SECS)
            .ok_or_else(Problem::internal)?,
    );
    Ok(response)
}

/// Accepts an open invitation while a sign-in link is spent: called by
/// `POST /email/verify` for an address that is not yet a member of the
/// link's workspace. Answers the new member's id, or `None` when there is no
/// open invitation for the address.
pub(crate) async fn accept_on_sign_in(
    db: &dyn cratefield_core::Database,
    id_gen: &dyn cratefield_core::IdGen,
    workspace_id: &str,
    email: &str,
    now: &str,
) -> Result<Option<(String, Role)>, Problem> {
    let user_id = format!("usr_{}", id_gen.ulid());
    let Some(role) = store::accept_invite(db, workspace_id, email, &user_id, now)
        .await
        .map_err(database)?
    else {
        return Ok(None);
    };
    store::link_identity(db, workspace_id, "email", email, &user_id, now)
        .await
        .map_err(database)?;
    Ok(Some((user_id, role)))
}
